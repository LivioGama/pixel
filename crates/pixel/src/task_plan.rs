//! Deterministic validation for a model-proposed task work plan.
//!
//! A work plan is deliberately smaller than a task specification. It carries
//! only the facts Pixel must be able to check before concurrent workers may be
//! launched: explicit ownership, selected symbols, dependencies, and the
//! requested candidate count. This module never resolves a symbol from prose
//! and never decides that overlapping lanes are safe.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Component, Path};

use serde::{Deserialize, Serialize};

pub(crate) const MAX_CANDIDATES_PER_LANE: usize = 3;
pub(crate) const MAX_RACING_LANES: usize = 8;

/// The only model-proposed input admitted before worker scheduling.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkPlan {
    pub(crate) lanes: Vec<WorkLane>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkLane {
    pub(crate) id: String,
    /// Exact repository-relative paths the lane may modify.
    pub(crate) owned_paths: Vec<String>,
    /// Exact symbols selected by the planner. Pixel does not fill this in.
    pub(crate) symbols: Vec<String>,
    #[serde(default)]
    pub(crate) depends_on: Vec<String>,
    pub(crate) candidate_count: usize,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PlanDisposition {
    /// Every deterministic scheduling precondition holds.
    ParallelEligible,
    /// The proposal is parseable, but Pixel must not fan it out or race it.
    SerialRequired,
    /// The plan is malformed and cannot be scheduled at all.
    Rejected,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct PlanValidation {
    pub(crate) disposition: PlanDisposition,
    pub(crate) reasons: Vec<String>,
}

impl PlanValidation {
    fn accepted() -> Self {
        Self {
            disposition: PlanDisposition::ParallelEligible,
            reasons: Vec::new(),
        }
    }

    fn serial_required(reasons: Vec<String>) -> Self {
        Self {
            disposition: PlanDisposition::SerialRequired,
            reasons,
        }
    }

    fn rejected(reasons: Vec<String>) -> Self {
        Self {
            disposition: PlanDisposition::Rejected,
            reasons,
        }
    }
}

/// Parse an untrusted JSON proposal. The caller can return parse failures to
/// the planner without treating them as an executable task plan.
pub(crate) fn parse(json: &str) -> Result<WorkPlan, String> {
    serde_json::from_str(json).map_err(|error| format!("invalid work plan JSON: {error}"))
}

/// Validate a plan without repository inference or model calls.
///
/// Invalid identities, path spelling, dependency graphs, and excessive
/// candidate counts are rejected. Missing symbols, unsafe ownership overlap,
/// and a plan too wide to race are retained as structured proposals but must
/// be scheduled serially.
pub(crate) fn validate(plan: &WorkPlan) -> PlanValidation {
    if plan.lanes.is_empty() {
        return PlanValidation::rejected(vec!["no_lanes".to_string()]);
    }

    let mut rejected = Vec::new();
    let mut ids = BTreeSet::new();
    for lane in &plan.lanes {
        if !valid_id(&lane.id) {
            rejected.push(format!("invalid_lane_id:{}", lane.id));
        } else if !ids.insert(lane.id.as_str()) {
            rejected.push(format!("duplicate_lane_id:{}", lane.id));
        }
        if lane.candidate_count == 0 {
            rejected.push(format!("zero_candidate_count:{}", lane.id));
        } else if lane.candidate_count > MAX_CANDIDATES_PER_LANE {
            rejected.push(format!("candidate_cap_exceeded:{}", lane.id));
        }
        let mut lane_paths = BTreeSet::new();
        for path in &lane.owned_paths {
            if !valid_repo_relative_path(path) {
                rejected.push(format!("invalid_owned_path:{}:{}", lane.id, path));
            } else if !lane_paths.insert(path.as_str()) {
                rejected.push(format!("duplicate_owned_path:{}:{}", lane.id, path));
            }
        }
        for dependency in &lane.depends_on {
            if !valid_id(dependency) {
                rejected.push(format!("invalid_dependency:{}:{}", lane.id, dependency));
            }
        }
    }
    if !rejected.is_empty() {
        rejected.sort();
        rejected.dedup();
        return PlanValidation::rejected(rejected);
    }

    let known_ids: BTreeSet<&str> = plan.lanes.iter().map(|lane| lane.id.as_str()).collect();
    for lane in &plan.lanes {
        for dependency in &lane.depends_on {
            if dependency == &lane.id {
                rejected.push(format!("self_dependency:{}", lane.id));
            } else if !known_ids.contains(dependency.as_str()) {
                rejected.push(format!("unknown_dependency:{}:{}", lane.id, dependency));
            }
        }
    }
    if !rejected.is_empty() {
        rejected.sort();
        rejected.dedup();
        return PlanValidation::rejected(rejected);
    }
    if has_dependency_cycle(plan) {
        return PlanValidation::rejected(vec!["dependency_cycle".to_string()]);
    }

    let mut serial = Vec::new();
    for lane in &plan.lanes {
        if lane.symbols.is_empty() || lane.symbols.iter().any(|symbol| !valid_symbol(symbol)) {
            serial.push(format!("missing_explicit_symbol:{}", lane.id));
        }
        if lane.owned_paths.is_empty() {
            serial.push(format!("missing_owned_paths:{}", lane.id));
        }
    }

    let racing_lanes = plan
        .lanes
        .iter()
        .filter(|lane| lane.candidate_count > 1)
        .count();
    if racing_lanes > MAX_RACING_LANES {
        serial.push("race_lane_cap_exceeded".to_string());
    }

    let mut owners: BTreeMap<&str, &str> = BTreeMap::new();
    for lane in &plan.lanes {
        for path in &lane.owned_paths {
            for (claimed_path, owner) in &owners {
                if paths_overlap(path, claimed_path) && *owner != lane.id {
                    serial.push(format!("owned_path_overlap:{}:{}:{}", owner, lane.id, path));
                }
            }
            owners.insert(path, lane.id.as_str());
        }
    }

    if serial.is_empty() {
        PlanValidation::accepted()
    } else {
        serial.sort();
        serial.dedup();
        PlanValidation::serial_required(serial)
    }
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 96
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

fn valid_symbol(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= 512 && !value.contains(['\n', '\r', '\0'])
}

fn valid_repo_relative_path(value: &str) -> bool {
    if value.is_empty() || value.len() > 1024 || value.contains('\\') {
        return false;
    }
    let path = Path::new(value);
    if path.is_absolute() {
        return false;
    }
    path.components()
        .all(|component| matches!(component, Component::Normal(_)))
}

fn paths_overlap(left: &str, right: &str) -> bool {
    left == right
        || left
            .strip_prefix(right)
            .is_some_and(|suffix| suffix.starts_with('/'))
        || right
            .strip_prefix(left)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn has_dependency_cycle(plan: &WorkPlan) -> bool {
    let mut incoming: BTreeMap<&str, usize> = plan
        .lanes
        .iter()
        .map(|lane| (lane.id.as_str(), lane.depends_on.len()))
        .collect();
    let mut outgoing: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for lane in &plan.lanes {
        for dependency in &lane.depends_on {
            outgoing
                .entry(dependency.as_str())
                .or_default()
                .push(lane.id.as_str());
        }
    }
    let mut ready: VecDeque<&str> = incoming
        .iter()
        .filter_map(|(id, count)| (*count == 0).then_some(*id))
        .collect();
    let mut visited = 0usize;
    while let Some(id) = ready.pop_front() {
        visited += 1;
        for next in outgoing.get(id).into_iter().flatten() {
            let count = incoming.get_mut(next).expect("known plan lane");
            *count -= 1;
            if *count == 0 {
                ready.push_back(next);
            }
        }
    }
    visited != plan.lanes.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lane(id: &str, path: &str) -> WorkLane {
        WorkLane {
            id: id.to_string(),
            owned_paths: vec![path.to_string()],
            symbols: vec![format!("crate::{id}")],
            depends_on: Vec::new(),
            candidate_count: 1,
        }
    }

    #[test]
    fn accepts_disjoint_explicit_lanes() {
        let mut second = lane("tests", "tests/runtime.rs");
        second.depends_on = vec!["runtime".to_string()];
        let plan = WorkPlan {
            lanes: vec![lane("runtime", "src/runtime.rs"), second],
        };

        assert_eq!(validate(&plan), PlanValidation::accepted());
    }

    #[test]
    fn overlap_requires_serial_execution() {
        let plan = WorkPlan {
            lanes: vec![
                lane("parent", "src/runtime"),
                lane("child", "src/runtime/ledger.rs"),
            ],
        };

        let result = validate(&plan);
        assert_eq!(result.disposition, PlanDisposition::SerialRequired);
        assert!(
            result
                .reasons
                .iter()
                .any(|reason| reason.starts_with("owned_path_overlap:parent:child:"))
        );
    }

    #[test]
    fn cycle_is_rejected() {
        let mut first = lane("first", "src/first.rs");
        first.depends_on.push("second".to_string());
        let mut second = lane("second", "src/second.rs");
        second.depends_on.push("first".to_string());

        assert_eq!(
            validate(&WorkPlan {
                lanes: vec![first, second]
            }),
            PlanValidation::rejected(vec!["dependency_cycle".to_string()])
        );
    }

    #[test]
    fn missing_symbol_requires_serial_execution_without_inference() {
        let mut lane = lane("runtime", "src/runtime.rs");
        lane.symbols.clear();

        let result = validate(&WorkPlan { lanes: vec![lane] });
        assert_eq!(result.disposition, PlanDisposition::SerialRequired);
        assert_eq!(result.reasons, vec!["missing_explicit_symbol:runtime"]);
    }

    #[test]
    fn candidate_cap_is_rejected_and_race_width_becomes_serial() {
        let mut too_many = lane("runtime", "src/runtime.rs");
        too_many.candidate_count = MAX_CANDIDATES_PER_LANE + 1;
        assert_eq!(
            validate(&WorkPlan {
                lanes: vec![too_many]
            }),
            PlanValidation::rejected(vec!["candidate_cap_exceeded:runtime".to_string()])
        );

        let lanes = (0..=MAX_RACING_LANES)
            .map(|index| {
                let mut lane = lane(&format!("lane-{index}"), &format!("src/{index}.rs"));
                lane.candidate_count = 2;
                lane
            })
            .collect();
        assert_eq!(
            validate(&WorkPlan { lanes }).disposition,
            PlanDisposition::SerialRequired
        );
    }

    #[test]
    fn parser_rejects_unknown_fields_and_unsafe_paths() {
        assert!(parse(r#"{"lanes":[],"unexpected":true}"#).is_err());
        let plan = WorkPlan {
            lanes: vec![lane("runtime", "../outside.rs")],
        };
        assert_eq!(validate(&plan).disposition, PlanDisposition::Rejected);
    }
}

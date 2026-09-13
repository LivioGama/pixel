//! Workflow estimator v1. These are policy assumptions, not measured averages:
//! 4 KiB per distinct returned evidence file and 1 KiB per native command or
//! returned relationship inspected. Known evidence bytes replace file assumptions.
//! No source, graph, or native comparison operation is performed here. Tokens are
//! always UTF-8 bytes / 4, including the live reporting line when emitted.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::ActionEvent;

pub const ESTIMATOR_VERSION: &str = "workflow-v1";
pub const TIME_ESTIMATOR_VERSION: &str = "sequential-v1";
pub const DEFAULT_ROUND_TRIP_MS: u64 = 2000;
static INVOCATION_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(crate) fn invocation_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "{}-{nanos:x}-{:x}",
        std::process::id(),
        INVOCATION_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

/// Counts refer only to evidence returned by this operation. Callers deduplicate
/// file paths before passing them. Never use repository size or unseen results.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowEvidence {
    pub distinct_files: u64,
    pub relationships: u64,
    pub native_commands: u64,
    pub known_file_bytes: Option<u64>,
    pub partial: bool,
}

impl WorkflowEvidence {
    pub fn estimated_bytes(&self) -> u64 {
        self.known_file_bytes
            .unwrap_or_else(|| self.distinct_files.saturating_mul(4096))
            .saturating_add(
                self.native_commands
                    .saturating_add(self.relationships)
                    .saturating_mul(1024),
            )
    }
}

/// Rough sequential workflow scenario, not a latency measurement. Each file
/// read and native command is one assumed round trip; relationships affect only
/// token volume. The common first round trip cancels. Native command execution
/// time is assumed zero; elapsed Pixel execution is the measured subtraction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowTimeEstimate {
    pub estimator_version: String,
    pub round_trip_ms: u64,
    pub native_command_ms: u64,
    pub sequential_steps: u64,
    pub saved_ms: f64,
}

impl WorkflowTimeEstimate {
    fn from_evidence(
        evidence: &WorkflowEvidence,
        duration_us: u64,
        round_trip_ms: u64,
    ) -> Option<Self> {
        let sequential_steps = evidence
            .native_commands
            .saturating_add(evidence.distinct_files);
        if sequential_steps == 0 {
            return None;
        }
        Some(Self {
            estimator_version: TIME_ESTIMATOR_VERSION.to_owned(),
            round_trip_ms,
            native_command_ms: 0,
            sequential_steps,
            saved_ms: sequential_steps.saturating_sub(1) as f64 * round_trip_ms as f64
                - duration_us as f64 / 1000.0,
        })
    }
}

/// Measured facts and versioned estimates remain separate in the serialized log.
/// `None` evidence means a meaningful native comparison is unavailable; explicit
/// zero evidence is a meaningful zero token baseline. Sequential time remains
/// unavailable when there are no command/file-read steps.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OperationMetrics {
    pub duration_us: u64,
    /// Describes which rendered streams `output_bytes` measures. Absence means
    /// unspecified (including records written before this field existed).
    /// `cli-rendered-streams` means CLI-owned rendered stdout and diagnostics;
    /// it excludes writes owned by lower-level libraries or subprocesses.
    /// Reporting is accounted separately in `reporting_bytes` in every scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_scope: Option<String>,
    pub output_bytes: u64,
    pub reporting_bytes: u64,
    pub estimator_version: String,
    pub native_workflow_bytes: Option<u64>,
    pub evidence: Option<WorkflowEvidence>,
    /// Missing in older records; never backfill an unrecorded time assumption.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_estimate: Option<WorkflowTimeEstimate>,
}

impl OperationMetrics {
    pub fn new(duration: Duration, output_bytes: u64, evidence: Option<WorkflowEvidence>) -> Self {
        let duration_us = duration.as_micros().min(u64::MAX as u128) as u64;
        Self {
            duration_us,
            output_scope: None,
            output_bytes,
            reporting_bytes: 0,
            estimator_version: ESTIMATOR_VERSION.to_owned(),
            native_workflow_bytes: evidence.as_ref().map(WorkflowEvidence::estimated_bytes),
            time_estimate: evidence.as_ref().and_then(|evidence| {
                WorkflowTimeEstimate::from_evidence(evidence, duration_us, DEFAULT_ROUND_TRIP_MS)
            }),
            evidence,
        }
    }

    /// Override only this invocation's sequential round-trip policy assumption.
    /// Zero is valid. Unavailable/zero-step evidence remains unavailable.
    pub fn with_round_trip_ms(mut self, round_trip_ms: u64) -> Self {
        self.time_estimate = self.evidence.as_ref().and_then(|evidence| {
            WorkflowTimeEstimate::from_evidence(evidence, self.duration_us, round_trip_ms)
        });
        self
    }

    pub fn saved_time_ms(&self) -> Option<f64> {
        self.time_estimate
            .as_ref()
            .map(|estimate| estimate.saved_ms)
    }

    pub fn output_tokens(&self) -> f64 {
        (self.output_bytes as f64 + self.reporting_bytes as f64) / 4.0
    }

    pub fn saved_tokens(&self) -> Option<f64> {
        Some(self.native_workflow_bytes? as f64 / 4.0 - self.output_tokens())
    }

    pub fn partial(&self) -> bool {
        self.evidence.as_ref().is_some_and(|e| e.partial)
    }
}

impl ActionEvent {
    pub fn with_metrics(mut self, metrics: OperationMetrics) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Call once before logging/emitting a live line. Repeated calls are
    /// idempotent. Disabled/stream-only reporting should not call this method.
    /// The returned string excludes its newline, which is still accounted for.
    pub fn finalize_metrics_line(&mut self) -> Option<String> {
        // Width can only change near digit boundaries. Taking the maximum
        // prevents two-value oscillation at a negative-savings boundary.
        for _ in 0..8 {
            let line = format_metrics_line(self)?;
            let bytes = line.len() as u64 + 2;
            let metrics = self.metrics.as_mut()?;
            if metrics.reporting_bytes == bytes {
                return Some(line);
            }
            metrics.reporting_bytes = metrics.reporting_bytes.max(bytes);
        }
        format_metrics_line(self)
    }
}

/// Reproduce the invocation's authoritative line from its finalized record.
/// Never consult a global latest record: native hosts correlate invocation IDs.
pub fn format_metrics_line(event: &ActionEvent) -> Option<String> {
    let metrics = event.metrics.as_ref()?;
    // Prevent command/control characters from entering the line.
    let command = event
        .command
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>();
    let duration_ms = metrics.duration_us as f64 / 1000.0;
    // Evaluate the user-facing savings claim from the command payload only.
    // Reporting bytes remain fully accounted for, but including them here
    // makes whether a savings row exists depend on its own emitted size.
    let payload_tok = metrics.output_bytes as f64 / 4.0;
    let id = event.invocation_id.as_deref().unwrap_or("legacy");
    let short_id: String = id.split('-').nth(1).map_or_else(
        || id.to_owned(),
        |s| {
            s.chars()
                .rev()
                .take(6)
                .collect::<String>()
                .chars()
                .rev()
                .collect()
        },
    );

    // Line 1: identity header.
    let header = format!("🟩 pixel {command} ❀ {duration_ms:.1}ms ❀ #{short_id}");

    // Keep the estimate readable in terminal UIs that render block characters
    // as low-contrast progress tracks. A negative estimate is overhead, not
    // negative savings, so never express it as a misleading percentage.
    let time_section = metrics.saved_time_ms().and_then(|saved_ms| {
            let native_time_ms = saved_ms + duration_ms;
            let partial_tag = if metrics.partial() { ", partial" } else { "" };
            if native_time_ms > 0.0 {
                if saved_ms > 0.0 {
                    let pct = (saved_ms / native_time_ms * 100.0).round() as i64;
                    Some(format!(
                        "{pct}% faster, {duration_ms:.1}ms against ~{native_time_ms:.0}ms estimated{partial_tag}"
                    ))
                } else if saved_ms < 0.0 {
                    Some(format!(
                        "+{:.1}ms overhead, {duration_ms:.1}ms against ~{native_time_ms:.0}ms estimated{partial_tag}",
                        -saved_ms
                    ))
                } else {
                    Some(format!(
                        "{duration_ms:.1}ms against ~{native_time_ms:.0}ms estimated · no estimated time saving{partial_tag}"
                    ))
                }
            } else {
                None
            }
        });

    let token_section = metrics.native_workflow_bytes.and_then(|native_bytes| {
        let native_tok = native_bytes as f64 / 4.0;
        let partial_tag = if metrics.partial() { ", partial" } else { "" };
        if native_tok > 0.0 {
            let saved_tok = native_tok - payload_tok;
            if saved_tok > 0.0 {
                let pct = (saved_tok / native_tok * 100.0).round() as i64;
                Some(format!(
                    "estimated LLM context saved: ~{saved_tok:.0} tok ({pct}%){partial_tag}"
                ))
            } else {
                None
            }
        } else {
            None
        }
    });

    let mut rows = Vec::new();
    if let Some(time_section) = time_section {
        rows.push(format!("  ├─ ⏱ {time_section}"));
    }
    if let Some(token_section) = token_section {
        rows.push(format!("  ├─ § {token_section}"));
    }
    let stem = "  │";
    let separator = "  └────────────────────────────────────────────────────────";

    let mut line = if rows.is_empty() {
        header
    } else {
        format!("{header}\n{stem}\n{}\n{stem}\n{separator}", rows.join("\n"))
    };
    // Padding resolves the rare digit-boundary fixed-point oscillation without
    // lying about emitted overhead. Bound it when replaying malformed records.
    let padding = metrics
        .reporting_bytes
        .saturating_sub(line.len() as u64 + 2)
        .min(64);
    line.extend(std::iter::repeat_n(' ', padding as usize));
    Some(line)
}

/// Summarize only finalized operation records. Duplicate invocation IDs count
/// once (first record wins); legacy records without metrics remain separate.
/// Each estimator version and coverage class has its own totals: partial and
/// unavailable baselines are never blended into complete workflow comparisons.
pub fn summarize_metrics(events: &[ActionEvent]) -> serde_json::Value {
    #[derive(Default)]
    struct Totals {
        operations: u64,
        duration_us: u64,
        output_bytes: u64,
        reporting_bytes: u64,
        native_workflow_bytes: u64,
        output_scopes: std::collections::BTreeSet<String>,
    }
    #[derive(Default)]
    struct TimeTotals {
        operations: u64,
        duration_us: u64,
        sequential_steps: u64,
        saved_ms: f64,
    }

    let mut seen = std::collections::HashSet::new();
    let mut duplicate_records = 0;
    let mut legacy_records = 0;
    let mut unavailable_records = 0;
    let mut time_unavailable_records = 0;
    // Token estimates keep their existing aggregation contract. Time estimates
    // have a separate grouping because round-trip assumptions may vary between
    // invocations even when the token estimator and coverage are identical.
    let mut time_groups: std::collections::BTreeMap<(&str, &str, u64, u64, &str), TimeTotals> =
        std::collections::BTreeMap::new();
    let mut versions: std::collections::BTreeMap<&str, std::collections::BTreeMap<&str, Totals>> =
        std::collections::BTreeMap::new();
    for event in events {
        if let Some(id) = event.invocation_id.as_deref()
            && !seen.insert(id)
        {
            duplicate_records += 1;
            continue;
        }
        let Some(metrics) = event.metrics.as_ref() else {
            legacy_records += 1;
            continue;
        };
        let coverage = if metrics.native_workflow_bytes.is_none() {
            unavailable_records += 1;
            "unavailable"
        } else if metrics.partial() {
            "partial"
        } else {
            "complete"
        };
        if let Some(time) = metrics.time_estimate.as_ref() {
            let totals = time_groups
                .entry((
                    metrics.estimator_version.as_str(),
                    time.estimator_version.as_str(),
                    time.round_trip_ms,
                    time.native_command_ms,
                    coverage,
                ))
                .or_default();
            totals.operations += 1;
            totals.duration_us = totals.duration_us.saturating_add(metrics.duration_us);
            totals.sequential_steps = totals
                .sequential_steps
                .saturating_add(time.sequential_steps);
            totals.saved_ms += time.saved_ms;
        } else {
            time_unavailable_records += 1;
        }
        let totals = versions
            .entry(&metrics.estimator_version)
            .or_default()
            .entry(coverage)
            .or_default();
        totals.operations += 1;
        totals.output_scopes.insert(
            metrics
                .output_scope
                .as_deref()
                .unwrap_or("unspecified")
                .to_owned(),
        );
        totals.duration_us = totals.duration_us.saturating_add(metrics.duration_us);
        totals.output_bytes = totals.output_bytes.saturating_add(metrics.output_bytes);
        totals.reporting_bytes = totals
            .reporting_bytes
            .saturating_add(metrics.reporting_bytes);
        totals.native_workflow_bytes = totals
            .native_workflow_bytes
            .saturating_add(metrics.native_workflow_bytes.unwrap_or(0));
    }
    let versions: serde_json::Map<String, serde_json::Value> = versions
        .into_iter()
        .map(|(version, groups)| {
            let groups: serde_json::Map<String, serde_json::Value> = groups
                .into_iter()
                .map(|(coverage, totals)| {
                    let output_tokens =
                        (totals.output_bytes as f64 + totals.reporting_bytes as f64) / 4.0;
                    let native_bytes =
                        (coverage != "unavailable").then_some(totals.native_workflow_bytes);
                    let native_tokens = native_bytes.map(|bytes| bytes as f64 / 4.0);
                    (
                        coverage.to_owned(),
                        serde_json::json!({
                            "operations": totals.operations,
                            "measured": {
                                "duration_us": totals.duration_us,
                                "output_bytes": totals.output_bytes,
                                "reporting_bytes": totals.reporting_bytes,
                                "output_scopes": totals.output_scopes,
                            },
                            "estimated": {
                                "native_workflow_bytes": native_bytes,
                                "output_tokens": output_tokens,
                                "native_workflow_tokens": native_tokens,
                                "saved_tokens": native_tokens.map(|tokens| tokens - output_tokens),
                            },
                        }),
                    )
                })
                .collect();
            (version.to_owned(), serde_json::Value::Object(groups))
        })
        .collect();
    let time_estimates: Vec<serde_json::Value> = time_groups.into_iter().map(|((token_version, time_version, round_trip_ms, native_command_ms, coverage), totals)| {
        serde_json::json!({
            "token_estimator_version": token_version,
            "time_estimator_version": time_version,
            "round_trip_ms": round_trip_ms,
            "native_command_ms": native_command_ms,
            "coverage": coverage,
            "operations": totals.operations,
            "measured": { "duration_us": totals.duration_us },
            "estimated": { "sequential_steps": totals.sequential_steps, "saved_ms": totals.saved_ms },
        })
    }).collect();
    serde_json::json!({
        "record_count": events.len(),
        "unique_records": events.len() - duplicate_records,
        "duplicate_records": duplicate_records,
        "legacy_records": legacy_records,
        "unavailable_records": unavailable_records,
        "time_unavailable_records": time_unavailable_records,
        "token_approximation": "UTF-8 bytes / 4 (including reporting overhead)",
        "versions": versions,
        "time_estimates": time_estimates,
        "time_approximation": "sequential one-round-trip-per-command-or-file-read scenario; common first round trip cancels; native command runtime assumed zero; batching or parallel calls may save less",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequential_time_cancels_first_round_trip_and_excludes_relationships() {
        let metrics = OperationMetrics::new(
            Duration::from_micros(12400),
            100,
            Some(WorkflowEvidence {
                native_commands: 1,
                distinct_files: 3,
                relationships: 900,
                partial: true,
                ..Default::default()
            }),
        );
        let time = metrics.time_estimate.as_ref().unwrap();
        assert_eq!(time.estimator_version, "sequential-v1");
        assert_eq!(time.round_trip_ms, 2000);
        assert_eq!(time.native_command_ms, 0);
        assert_eq!(time.sequential_steps, 4);
        assert_eq!(metrics.saved_time_ms(), Some(5987.6));
        assert_eq!(
            metrics.clone().with_round_trip_ms(500).saved_time_ms(),
            Some(1487.6)
        );
        assert_eq!(metrics.with_round_trip_ms(0).saved_time_ms(), Some(-12.4));
    }

    #[test]
    fn one_step_time_is_negative_or_zero_and_missing_zero_step_evidence_is_unavailable() {
        let one = WorkflowEvidence {
            native_commands: 1,
            ..Default::default()
        };
        assert_eq!(
            OperationMetrics::new(Duration::from_micros(12400), 0, Some(one.clone()))
                .saved_time_ms(),
            Some(-12.4)
        );
        assert_eq!(
            OperationMetrics::new(Duration::ZERO, 0, Some(one)).saved_time_ms(),
            Some(0.0)
        );
        assert!(
            OperationMetrics::new(Duration::ZERO, 0, Some(WorkflowEvidence::default()))
                .time_estimate
                .is_none()
        );
        assert!(
            OperationMetrics::new(Duration::ZERO, 0, None)
                .with_round_trip_ms(0)
                .time_estimate
                .is_none()
        );
    }

    #[test]
    fn older_metrics_never_invent_time_estimates_and_new_records_round_trip() {
        let old: OperationMetrics = serde_json::from_str(r#"{"duration_us":21,"output_bytes":120,"reporting_bytes":0,"estimator_version":"workflow-v1","native_workflow_bytes":5120,"evidence":{"distinct_files":1,"relationships":0,"native_commands":1,"known_file_bytes":null,"partial":false}}"#).unwrap();
        assert!(old.time_estimate.is_none());
        assert_eq!(old.saved_time_ms(), None);
        let new = OperationMetrics::new(Duration::from_millis(20), 120, old.evidence)
            .with_round_trip_ms(2500);
        let decoded: OperationMetrics =
            serde_json::from_value(serde_json::to_value(&new).unwrap()).unwrap();
        assert_eq!(decoded.time_estimate.unwrap().round_trip_ms, 2500);
        let mut event = ActionEvent::new("impact", "run").with_metrics(new);
        let line = event.finalize_metrics_line().unwrap();
        assert!(line.starts_with("🟩 pixel impact"));
        assert!(line.contains("saved"));
        assert!(line.contains("against ~2500ms estimated"));
        assert_eq!(
            event.metrics.unwrap().reporting_bytes,
            line.len() as u64 + 2
        );
    }

    #[test]
    fn time_summary_separates_versions_assumptions_coverage_and_missing_history() {
        let make = |round_trip_ms, partial| {
            ActionEvent::new("impact", "run").with_metrics(
                OperationMetrics::new(
                    Duration::from_millis(20),
                    40,
                    Some(WorkflowEvidence {
                        native_commands: 1,
                        distinct_files: 2,
                        partial,
                        ..Default::default()
                    }),
                )
                .with_round_trip_ms(round_trip_ms),
            )
        };
        let a = make(2000, false);
        let b = make(500, false);
        let partial = make(2000, true);
        let mut future_time = make(2000, false);
        future_time
            .metrics
            .as_mut()
            .unwrap()
            .time_estimate
            .as_mut()
            .unwrap()
            .estimator_version = "sequential-v2".to_owned();
        let mut future_tokens = make(2000, false);
        future_tokens.metrics.as_mut().unwrap().estimator_version = "workflow-v2".to_owned();
        let mut old_metrics = make(2000, false);
        old_metrics.metrics.as_mut().unwrap().time_estimate = None;
        let unavailable = ActionEvent::new("index", ".").with_metrics(OperationMetrics::new(
            Duration::from_millis(4),
            0,
            None,
        ));
        let legacy = ActionEvent::new("search", "a").with_savings(20, 8000);
        let summary = summarize_metrics(&[
            a.clone(),
            a,
            b,
            partial,
            future_time,
            future_tokens,
            old_metrics,
            unavailable,
            legacy,
        ]);
        assert_eq!(summary["duplicate_records"], 1);
        assert_eq!(summary["legacy_records"], 1);
        assert_eq!(summary["time_unavailable_records"], 2);
        let groups = summary["time_estimates"].as_array().unwrap();
        assert_eq!(groups.len(), 5);
        assert!(groups.iter().all(|group| group["operations"] == 1
            && group["measured"]["duration_us"] == 20000
            && group["estimated"]["sequential_steps"] == 3));
        let group = groups
            .iter()
            .find(|group| group["round_trip_ms"] == 500)
            .unwrap();
        assert_eq!(group["estimated"]["saved_ms"], 980.0);
        assert_eq!(
            groups
                .iter()
                .filter(|group| group["coverage"] == "partial")
                .count(),
            1
        );
        assert!(
            groups
                .iter()
                .any(|group| group["time_estimator_version"] == "sequential-v2")
        );
        assert!(
            groups
                .iter()
                .any(|group| group["token_estimator_version"] == "workflow-v2")
        );
    }

    #[test]
    fn time_summary_retains_negative_and_zero_estimates() {
        let make = |duration| {
            ActionEvent::new("search", "needle").with_metrics(OperationMetrics::new(
                duration,
                0,
                Some(WorkflowEvidence {
                    native_commands: 1,
                    ..Default::default()
                }),
            ))
        };
        let summary = summarize_metrics(&[make(Duration::from_millis(12)), make(Duration::ZERO)]);
        let group = &summary["time_estimates"][0];
        assert_eq!(group["operations"], 2);
        assert_eq!(group["measured"]["duration_us"], 12000);
        assert_eq!(group["estimated"]["saved_ms"], -12.0);
        assert_eq!(group["estimated"]["sequential_steps"], 2);
    }

    #[test]
    fn output_scope_defaults_unspecified_and_round_trips_explicit_cli_scope() {
        let old_json = r#"{"duration_us":21,"output_bytes":120,"reporting_bytes":0,"estimator_version":"workflow-v1","native_workflow_bytes":null,"evidence":null}"#;
        let mut metrics: OperationMetrics = serde_json::from_str(old_json).unwrap();
        assert_eq!(metrics.output_scope, None);
        assert!(
            serde_json::to_value(&metrics)
                .unwrap()
                .get("output_scope")
                .is_none()
        );
        metrics.output_scope = Some("cli-rendered-streams".to_owned());
        let reloaded: OperationMetrics =
            serde_json::from_value(serde_json::to_value(&metrics).unwrap()).unwrap();
        assert_eq!(
            reloaded.output_scope.as_deref(),
            Some("cli-rendered-streams")
        );
        let event = ActionEvent::new("index", ".").with_metrics(reloaded);
        let summary = summarize_metrics(&[event]);
        assert_eq!(
            summary["versions"]["workflow-v1"]["unavailable"]["measured"]["output_scopes"],
            serde_json::json!(["cli-rendered-streams"])
        );
    }

    #[test]
    fn negative_zero_partial_and_unavailable_are_distinct() {
        let evidence = WorkflowEvidence {
            distinct_files: 2,
            relationships: 3,
            native_commands: 1,
            partial: true,
            ..Default::default()
        };
        let metrics = OperationMetrics::new(Duration::from_micros(12400), 12896, Some(evidence));
        assert_eq!(metrics.native_workflow_bytes, Some(12288));
        assert_eq!(metrics.saved_tokens(), Some(-152.0));
        assert!(metrics.partial());
        let zero = OperationMetrics::new(Duration::ZERO, 0, Some(WorkflowEvidence::default()));
        assert_eq!(zero.saved_tokens(), Some(0.0));
        assert_eq!(
            OperationMetrics::new(Duration::ZERO, 0, None).saved_tokens(),
            None
        );
    }

    #[test]
    fn known_evidence_bytes_replace_file_assumptions() {
        let evidence = WorkflowEvidence {
            distinct_files: 5,
            known_file_bytes: Some(31),
            native_commands: 1,
            ..Default::default()
        };
        assert_eq!(evidence.estimated_bytes(), 1055);
    }

    #[test]
    fn reporting_is_accounted_and_finalizing_is_idempotent() {
        let mut event =
            ActionEvent::new("impact", "target=run").with_metrics(OperationMetrics::new(
                Duration::from_micros(12400),
                3280,
                Some(WorkflowEvidence {
                    distinct_files: 4,
                    partial: true,
                    ..Default::default()
                }),
            ));
        let line = event.finalize_metrics_line().unwrap();
        assert!(line.contains("12.4ms"));
        assert!(line.contains("partial"));
        assert_eq!(
            event.metrics.as_ref().unwrap().reporting_bytes,
            line.len() as u64 + 2
        );
        assert_eq!(
            event.finalize_metrics_line().as_deref(),
            Some(line.as_str())
        );
        assert_eq!(format_metrics_line(&event).unwrap(), line);
    }

    #[test]
    fn reporting_matches_actual_bytes_across_digit_and_sign_boundaries() {
        for output_bytes in 0..12000 {
            let mut event = ActionEvent::new("impact", "run").with_metrics(OperationMetrics::new(
                Duration::from_micros(12400),
                output_bytes,
                Some(WorkflowEvidence {
                    distinct_files: 1,
                    ..Default::default()
                }),
            ));
            let line = event.finalize_metrics_line().unwrap();
            let metrics = event.metrics.as_ref().unwrap();
            assert_eq!(metrics.reporting_bytes, line.len() as u64 + 2);
            assert_eq!(
                metrics.output_tokens(),
                (output_bytes + line.len() as u64 + 2) as f64 / 4.0
            );
            assert_eq!(
                metrics.saved_tokens(),
                Some(1024.0 - metrics.output_tokens())
            );
        }
    }

    #[test]
    fn no_reporting_has_zero_overhead_and_keeps_error_outcome() {
        let event = ActionEvent::new("search", "[")
            .with_result(
                &Err("invalid pattern".to_owned()),
                Duration::from_micros(900),
            )
            .with_metrics(OperationMetrics::new(Duration::from_micros(900), 41, None));
        assert_eq!(event.outcome, crate::Outcome::Error);
        assert_eq!(event.error.as_deref(), Some("invalid pattern"));
        let metrics = event.metrics.as_ref().unwrap();
        assert_eq!(metrics.reporting_bytes, 0);
        assert_eq!(metrics.output_tokens(), 10.25);
        assert_eq!(metrics.duration_us, 900);
    }

    #[test]
    fn legacy_json_has_no_invented_metrics_or_identity() {
        let event: ActionEvent = serde_json::from_str(r#"{"ts_ms":1,"pid":2,"command":"search","args":"abc","cwd":"/tmp","outcome":"ok","duration_ms":5,"snippet_cap_chars":5,"pool_chars":20}"#).unwrap();
        assert!(event.metrics.is_none());
        assert!(event.invocation_id.is_none());
        assert_eq!(event.savings_ratio(), Some(0.75));
    }

    #[test]
    fn concurrent_ids_are_unique_and_records_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("actions.jsonl");
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let path = &path;
                scope.spawn(move || {
                    let mut log = crate::ActionLog::spawn_at(path.clone());
                    for _ in 0..10 {
                        let event = ActionEvent::new("search", "needle").with_metrics(
                            OperationMetrics::new(Duration::from_micros(99), 43, None),
                        );
                        log.log(event);
                    }
                    log.finish();
                });
            }
        });
        let events = crate::tail(&path, 100).unwrap();
        assert_eq!(events.len(), 80);
        let ids = events
            .iter()
            .map(|e| e.invocation_id.as_ref().unwrap())
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(ids.len(), 80);
        assert!(
            events
                .iter()
                .all(|e| e.metrics.as_ref().unwrap().duration_us == 99)
        );
    }

    #[test]
    fn summary_deduplicates_and_separates_legacy_partial_unavailable_and_versions() {
        let complete = ActionEvent::new("impact", "run").with_metrics(OperationMetrics::new(
            Duration::from_micros(12),
            5000,
            Some(WorkflowEvidence {
                distinct_files: 1,
                ..Default::default()
            }),
        ));
        let partial = ActionEvent::new("targets", "task").with_metrics(OperationMetrics::new(
            Duration::from_micros(18),
            40,
            Some(WorkflowEvidence {
                native_commands: 1,
                partial: true,
                ..Default::default()
            }),
        ));
        let unavailable = ActionEvent::new("index", ".").with_metrics(OperationMetrics::new(
            Duration::from_micros(30),
            120,
            None,
        ));
        let mut legacy = ActionEvent::new("search", "a").with_savings(20, 8000);
        legacy.invocation_id = None;
        let mut future = ActionEvent::new("impact", "run").with_metrics(OperationMetrics::new(
            Duration::from_micros(3),
            0,
            Some(WorkflowEvidence::default()),
        ));
        future.metrics.as_mut().unwrap().estimator_version = "workflow-v2".to_owned();
        let summary = summarize_metrics(&[
            complete.clone(),
            complete,
            partial,
            unavailable,
            legacy.clone(),
            legacy,
            future,
        ]);
        assert_eq!(summary["record_count"], 7);
        assert_eq!(summary["unique_records"], 6);
        assert_eq!(summary["duplicate_records"], 1);
        assert_eq!(summary["legacy_records"], 2);
        assert_eq!(summary["unavailable_records"], 1);
        let v1 = &summary["versions"]["workflow-v1"];
        assert_eq!(v1["complete"]["operations"], 1);
        assert_eq!(v1["complete"]["measured"]["duration_us"], 12);
        assert_eq!(v1["complete"]["estimated"]["saved_tokens"], -226.0);
        assert_eq!(v1["partial"]["estimated"]["saved_tokens"], 246.0);
        assert!(v1["unavailable"]["estimated"]["saved_tokens"].is_null());
        assert_eq!(v1["unavailable"]["measured"]["output_bytes"], 120);
        assert_eq!(
            summary["versions"]["workflow-v2"]["complete"]["estimated"]["saved_tokens"],
            0.0
        );
    }

    #[test]
    fn summary_counts_live_reporting_bytes_once_and_empty_has_no_baseline() {
        let mut event = ActionEvent::new("search", "needle").with_metrics(OperationMetrics::new(
            Duration::from_micros(21),
            300,
            Some(WorkflowEvidence {
                distinct_files: 1,
                ..Default::default()
            }),
        ));
        let line = event.finalize_metrics_line().unwrap();
        let summary = summarize_metrics(&[event.clone(), event]);
        let group = &summary["versions"]["workflow-v1"]["complete"];
        assert_eq!(group["measured"]["reporting_bytes"], line.len() + 2);
        assert_eq!(
            group["estimated"]["output_tokens"],
            (300 + line.len() + 2) as f64 / 4.0
        );
        let empty = summarize_metrics(&[]);
        assert_eq!(empty["record_count"], 0);
        assert_eq!(empty["versions"], serde_json::json!({}));
    }
}

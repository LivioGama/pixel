//! Pure workflow gates shared by live adapters and deterministic replay.

use crate::model::{CheckOutcome, Decision, Gate, ObservationKind, Phase, Route, Task};

/// Evaluate a gate without invoking tools, classifiers, clocks or processes.
pub fn decide(task: &Task, gate: Gate, current_source_id: Option<&str>) -> Decision {
    let mut reasons = Vec::new();
    let mut routes = vec![Route::Investigate, Route::Recover];
    if task.phase == Phase::Cancelled {
        reasons.push("task cancelled".into());
        return Decision {
            allowed: false,
            reasons,
            eligible_routes: routes,
            phase: task.phase,
        };
    }
    if let Err(error) = task.contract.validate() {
        reasons.push(error.to_string());
        routes.push(Route::Configure);
    }
    if task.legacy_status.is_some() {
        reasons.push("legacy task needs an explicit completion contract".into());
        routes.push(Route::Configure);
    }
    if task.contract.checks.is_empty() {
        reasons.push("repository verification checks are not configured".into());
        if !routes.contains(&Route::Configure) {
            routes.push(Route::Configure);
        }
    }
    let current = current_source_id
        .zip(task.source.as_ref())
        .is_some_and(|(current, snapshot)| current == snapshot.content_id);
    if !current {
        reasons.push("source evidence missing or stale".into());
        routes.push(Route::Prepare);
    }
    if task.contract.require_preparation {
        for kind in [ObservationKind::Scope, ObservationKind::Impact] {
            if !task.observations.iter().any(|observation| {
                observation.kind == kind
                    && observation.complete
                    && Some(observation.source_id.as_str()) == current_source_id
            }) {
                reasons.push(format!("fresh complete {kind:?} evidence required"));
                if !routes.contains(&Route::Prepare) {
                    routes.push(Route::Prepare);
                }
            }
        }
    }
    if task.running.is_some() {
        reasons.push("verification is running or interrupted; recover before continuing".into());
    }
    if gate == Gate::Edit {
        if task.phase == Phase::Complete {
            reasons.push("completed task needs a new task binding before editing".into());
        }
        if reasons.is_empty() {
            routes.push(Route::Edit);
        }
        return Decision {
            allowed: reasons.is_empty(),
            reasons,
            eligible_routes: routes,
            phase: task.phase,
        };
    }
    let contract_id = task.contract.id().ok();
    let passed = |id: &str| {
        task.receipts
            .iter()
            .rev()
            .find(|receipt| {
                receipt.check_id == id
                    && Some(receipt.source_id.as_str()) == current_source_id
                    && Some(receipt.contract_id.as_str()) == contract_id.as_deref()
            })
            .is_some_and(|receipt| {
                receipt.outcome == CheckOutcome::Passed && receipt.exit_code == Some(0)
            })
    };
    if task.contract.criteria.is_empty() {
        reasons.push("task acceptance criteria are not configured".into());
        routes.push(Route::Configure);
    }
    for check in task.contract.checks.iter().filter(|check| check.required) {
        if !passed(&check.id) {
            reasons.push(format!(
                "required check {} has no current passing receipt",
                check.id
            ));
        }
    }
    for criterion in &task.contract.criteria {
        if criterion.checks.is_empty() || !criterion.checks.iter().all(|id| passed(id)) {
            reasons.push(format!("acceptance criterion {} is unmet", criterion.id));
        }
    }
    if reasons.iter().any(|reason| {
        reason.starts_with("required check") || reason.starts_with("acceptance criterion")
    }) {
        routes.push(Route::Verify);
    }
    if task.contract.require_review
        && !task.review.as_ref().is_some_and(|review| {
            review.passed
                && review.findings.is_empty()
                && Some(review.source_id.as_str()) == current_source_id
                && Some(review.contract_id.as_str()) == contract_id.as_deref()
        })
    {
        reasons.push("current change review required".into());
        routes.push(Route::Review);
    }
    if reasons.is_empty() {
        routes.push(Route::Finish);
    }
    Decision {
        allowed: reasons.is_empty(),
        reasons,
        eligible_routes: routes,
        phase: task.phase,
    }
}

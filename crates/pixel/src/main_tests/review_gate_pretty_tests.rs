//! The review-gate human render is the feature's face: a findings list of
//! severity, anchor, rule, witness and fix. These tests pin that shape.

use super::*;

fn report(findings: Value) -> Value {
    serde_json::json!({
        "findings": findings,
        "caps": [],
        "snapshot": {"branch": "feat/x", "head": "abc1234567890", "dirty_count": 1},
    })
}

#[test]
fn clean_report_renders_one_line_with_the_snapshot_anchor() {
    let out = pretty_review_gate(&report(serde_json::json!([]))).unwrap();
    assert_eq!(out, "clean — 0 findings (feat/x @ abc1234)\n");
}

#[test]
fn a_finding_renders_severity_anchor_rule_witness_and_fix() {
    let out = pretty_review_gate(&report(serde_json::json!([{
        "rule": "possible-secret",
        "severity": "CRITICAL",
        "file": "src/config.rs",
        "line": 12,
        "evidence": "added line matches the openai-key pattern",
        "fix_hint": "confirm this is not a live credential; rotate it if it is",
    }])))
    .unwrap();
    assert_eq!(
        out,
        "BLOCKER    src/config.rs:12  possible-secret\n\
         \x20         added line matches the openai-key pattern\n\
         \x20         fix: confirm this is not a live credential; rotate it if it is\n\
         \n1 finding(s) (feat/x @ abc1234)\n"
    );
}

#[test]
fn a_finding_without_a_file_renders_repo_wide() {
    let out = pretty_review_gate(&report(serde_json::json!([{
        "rule": "risk-climb",
        "severity": "HIGH",
        "file": null,
        "line": null,
        "evidence": "the change set reaches CRITICAL",
        "fix_hint": "split or cover the risky path",
    }])))
    .unwrap();
    assert!(out.starts_with("CONCERN    repo-wide  risk-climb\n"));
}

#[test]
fn caps_are_listed_after_the_findings() {
    let mut r = report(serde_json::json!([]));
    r["caps"] = serde_json::json!(["findings truncated at 64; lower-severity tail not listed"]);
    let out = pretty_review_gate(&r).unwrap();
    assert_eq!(
        out,
        "clean — 0 findings (feat/x @ abc1234)\n\
         cap: findings truncated at 64; lower-severity tail not listed\n"
    );
}

#[test]
fn fail_on_thresholds_rank_blocker_above_all() {
    assert_eq!(ReviewFailOn::Blocker.threshold(), 4);
    assert_eq!(ReviewFailOn::Concern.threshold(), 3);
    assert_eq!(ReviewFailOn::Suggestion.threshold(), 2);
    assert_eq!(ReviewFailOn::Nit.threshold(), 1);
    assert!(ReviewFailOn::Blocker.threshold() > ReviewFailOn::Concern.threshold());
    assert!(ReviewFailOn::Concern.threshold() > ReviewFailOn::Suggestion.threshold());
    assert!(ReviewFailOn::Suggestion.threshold() > ReviewFailOn::Nit.threshold());
}

#[test]
fn review_severity_rank_orders_critical_first() {
    assert_eq!(review_severity_rank("CRITICAL"), 4);
    assert_eq!(review_severity_rank("HIGH"), 3);
    assert_eq!(review_severity_rank("MEDIUM"), 2);
    assert_eq!(review_severity_rank("LOW"), 1);
    assert_eq!(review_severity_rank("anything-else"), 1);
}

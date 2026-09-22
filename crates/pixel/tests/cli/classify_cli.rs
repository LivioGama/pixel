//! `pixel classify` outer-routing errors exercised without opening the model.

use crate::support::pixel_command;

#[test]
fn classify_requires_labels_unless_jsonl() {
    let out = pixel_command()
        .args(["classify", "some state text"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--label") || stderr.contains("required"),
        "{stderr}"
    );
}

#[test]
fn classify_rejects_command_context_in_jsonl_mode_without_opening_model() {
    let out = pixel_command()
        .args(["classify", "--jsonl", "--context", "the rubric preamble"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        stderr.lines().next().unwrap(),
        "error: the argument '--jsonl' cannot be used with '--context <CONTEXT>'"
    );
}

#[test]
fn classify_rejects_bad_criterion_after_outer_dispatch_without_opening_model() {
    let out = pixel_command()
        .args([
            "classify",
            "the state",
            "--label",
            "yes,no",
            "--criterion",
            "missing-equals",
        ])
        .env("PIXEL_METRICS", "0")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).trim(),
        "pixel: --criterion needs label=description, got \"missing-equals\""
    );
}

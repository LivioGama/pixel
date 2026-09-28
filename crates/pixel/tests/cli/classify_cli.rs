//! `pixel classify` outer-routing errors exercised without opening the model.

use crate::support::{Scratch, pixel_command};

#[test]
fn classify_without_labels_needs_the_local_engine() {
    let home = Scratch::for_test("classify", "labels-enabled-home");
    std::fs::create_dir_all(home.join(".pixel")).unwrap();
    std::fs::write(
        home.join(".pixel/config.yaml"),
        "classify: {enabled: true}\n",
    )
    .unwrap();
    let out = pixel_command()
        .env("HOME", &*home)
        .args(["classify", "some state text", "--engine", "remote"])
        .env("PIXEL_METRICS", "0")
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--label") && stderr.contains("ollaya"),
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
    let home = Scratch::for_test("classify", "criterion-enabled-home");
    std::fs::create_dir_all(home.join(".pixel")).unwrap();
    std::fs::write(
        home.join(".pixel/config.yaml"),
        "classify: {enabled: true}\n",
    )
    .unwrap();
    let out = pixel_command()
        .env("HOME", &*home)
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

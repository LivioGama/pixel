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

/// A port nothing listens on: bound, read, released.
fn closed_base() -> String {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    format!("http://127.0.0.1:{}", probe.local_addr().unwrap().port())
}

#[test]
fn classify_if_warm_should_fail_fast_with_empty_stdout_when_no_local_engine_listens() {
    let home = Scratch::for_test("classify", "if-warm-home");
    let base = closed_base();
    std::fs::create_dir_all(home.join(".pixel")).unwrap();
    // A recorded launch that would be auto-started without --if-warm.
    std::fs::write(
        home.join(".pixel/config.yaml"),
        format!(
            "classify: {{enabled: true, engine: local, ollaya: {{base: \"{base}\", argv: [\"/usr/bin/false\"]}}}}\n"
        ),
    )
    .unwrap();
    let started = std::time::Instant::now();
    let out = pixel_command()
        .env("HOME", &*home)
        .args([
            "classify",
            "fix the login bug",
            "--task-intent",
            "--if-warm",
        ])
        .env("PIXEL_METRICS", "0")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "--if-warm never waits for a start: {:?}",
        started.elapsed()
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), "");
    assert_eq!(
        String::from_utf8_lossy(&out.stderr).trim(),
        format!(
            "pixel: not warm: no local classify engine is listening at {base}; --if-warm never starts it (`pixel classify` without --if-warm does)"
        )
    );
}

#[test]
fn classify_task_intent_should_refuse_explicit_labels() {
    let out = pixel_command()
        .args(["classify", "t", "--task-intent", "--label", "a,b"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.starts_with("error: the argument '--task-intent' cannot be used with"),
        "{stderr}"
    );
}

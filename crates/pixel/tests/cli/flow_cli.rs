//! Real CLI boundaries with a disposable home and a fake browser only.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output};

struct Fixture(PathBuf);

impl Fixture {
    fn new(tag: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("pixel-flow-cli-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(root.join("bin")).unwrap();
        let stub = root.join("bin/agent-browser");
        std::fs::write(&stub, "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$AUDIT_CALLS\"\nprintf 'https://example.test\\n'\nexit \"${AUDIT_EXIT:-0}\"\n").unwrap();
        std::fs::set_permissions(stub, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(
            root.join("steps.json"),
            r#"[{"action":"open","url":"https://example.test"}]"#,
        )
        .unwrap();
        let fixture = Self(root);
        let saved = fixture.run(
            &[
                "replay-flow",
                "save",
                "audit",
                "--title",
                "Audit",
                "--from-file",
                "steps.json",
                "--json",
            ],
            false,
        );
        assert!(saved.status.success(), "{saved:?}");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&saved.stdout).unwrap()["saved"],
            true
        );
        fixture
    }

    fn run(&self, args: &[&str], failing_browser: bool) -> Output {
        Command::new(env!("CARGO_BIN_EXE_pixel"))
            .args(args)
            .current_dir(&self.0)
            .env("HOME", &self.0)
            .env("PIXEL_FLOW_DIR", self.0.join("flows"))
            .env("PIXEL_METRICS", "0")
            .env("PIXEL_DAEMON_AUTO_START", "0")
            .env("PATH", self.0.join("bin"))
            .env("AUDIT_CALLS", self.0.join("calls"))
            .env("AUDIT_EXIT", if failing_browser { "7" } else { "0" })
            .output()
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            crate::support::assert_no_daemon(&self.0);
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn flow_dry_run_and_execute_are_rejected_before_browser_launch() {
    let fixture = Fixture::new("dry");
    let output = fixture.run(
        &[
            "replay-flow",
            "replay",
            "audit",
            "--execute",
            "--dry-run",
            "--json",
        ],
        false,
    );
    assert!(
        !output.status.success(),
        "conflicting execution modes must fail"
    );
    assert!(output.stdout.is_empty(), "{output:?}");
    assert!(
        !fixture.0.join("calls").exists(),
        "dry-run launched the browser"
    );
}

#[test]
fn flow_json_lifecycle_emits_documents_and_executes_only_when_requested() {
    let fixture = Fixture::new("json");
    for args in [
        vec!["replay-flow", "get", "audit", "--json"],
        vec!["replay-flow", "list", "--json"],
        vec!["replay-flow", "show", "audit", "--json"],
        vec![
            "replay-flow",
            "revise",
            "audit",
            "--title",
            "Revised audit",
            "--json",
        ],
        vec!["replay-flow", "replay", "audit", "--dry-run", "--json"],
    ] {
        let output = fixture.run(&args, false);
        assert!(output.status.success(), "{args:?}: {output:?}");
        let value: serde_json::Value = serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|e| panic!("{args:?}: {e}: {output:?}"));
        assert!(value.is_object() || value.is_array(), "{args:?}: {value}");
    }
    assert!(!fixture.0.join("calls").exists());
    let output = fixture.run(
        &["replay-flow", "replay", "audit", "--execute", "--json"],
        false,
    );
    assert!(output.status.success(), "{output:?}");
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["success"], true, "{value}");
    let calls = std::fs::read_to_string(fixture.0.join("calls")).unwrap();
    assert!(
        calls
            .lines()
            .next()
            .unwrap()
            .contains("open https://example.test")
    );
    let deleted = fixture.run(&["replay-flow", "delete", "audit", "--json"], false);
    assert!(deleted.status.success(), "{deleted:?}");
    assert!(serde_json::from_slice::<serde_json::Value>(&deleted.stdout).is_ok());
    assert!(!fixture.0.join("flows/audit.json").exists());
}

#[test]
fn flow_execute_failure_is_nonzero_and_never_a_success_document() {
    let fixture = Fixture::new("failure");
    let output = fixture.run(
        &["replay-flow", "replay", "audit", "--execute", "--json"],
        true,
    );
    assert!(
        !output.status.success(),
        "browser failure must fail CLI: {output:?}"
    );
    assert!(output.stdout.is_empty(), "{output:?}");
    assert!(!output.stderr.is_empty());
    assert!(fixture.0.join("calls").exists());
}

/// Without `--json`, `--execute` prints the browser log to stderr and one
/// summary line to stdout: the agent reads the verdict, not a document.
#[test]
fn flow_execute_prints_a_summary_line_and_the_log_on_stderr() {
    let fixture = Fixture::new("summary");
    let output = fixture.run(&["replay-flow", "replay", "audit", "--execute"], false);
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        stdout.trim_end(),
        "✓ Flow executed: 1 steps, 0 skipped",
        "{output:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("# Executing flow: audit"), "{stderr}");
    assert!(
        stderr.contains("agent-browser open \"https://example.test\""),
        "{stderr}"
    );
}

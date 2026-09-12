//! Differential checks against the actual native executables, not snapshots
//! of Pixel's own formatting. Hook payload tests never execute their input.
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

const PIXEL: &str = env!("CARGO_BIN_EXE_pixel");
static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture(PathBuf);

impl Fixture {
    fn new(content: &[u8]) -> Self {
        let root = std::env::temp_dir().join(format!(
            "pixel-search-compat-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(root.join(".pixel")).unwrap();
        std::fs::write(root.join("a file.rs"), content).unwrap();
        Self(root.canonicalize().unwrap())
    }

    fn command(&self, binary: &str) -> Command {
        let mut command = Command::new(binary);
        command
            .current_dir(&self.0)
            .env_remove("RIPGREP_CONFIG_PATH")
            .env_remove("GREP_OPTIONS")
            .env_remove("PIXEL_TARGETS_GUARD");
        command
    }

    fn compare(&self, tool: &str, args: &[&str], backend: &str) {
        let native = self.command(tool).args(args).output().unwrap();
        let routed = self
            .command(PIXEL)
            .args(["search-compat", tool, "--"])
            .args(args)
            .output()
            .unwrap();
        assert_output_eq(&native, &routed, &format!("{tool} {args:?}"));
        if backend == "pixel" {
            let log = std::fs::read_to_string(self.0.join(".pixel/actions.jsonl")).unwrap();
            assert!(
                log.contains("backend=pixel"),
                "Pixel backend not observed: {log}"
            );
        } else {
            assert!(
                !self.0.join(".pixel/actions.jsonl").exists(),
                "native fallback must not add to the search corpus"
            );
        }
    }

    fn guard(&self, provider: &str, command: &str, delegate: bool, path: Option<&Path>) -> Output {
        let payload = serde_json::json!({
            "hook_event_name": "PreToolUse",
            "tool_name": if provider == "devin" { "exec" } else { "Bash" },
            "cwd": self.0,
            "tool_input": {"command": command, "timeout_ms": 1234, "extra": {"keep": true}}
        });
        let mut cmd = self.command(PIXEL);
        cmd.args(["hook", "guard", "--provider", provider]);
        if delegate {
            cmd.arg("--delegate-rtk");
        }
        if let Some(path) = path {
            cmd.env("PATH", path);
        }
        run_hook(cmd, &payload)
    }
}

fn run_hook(mut command: Command, payload: &serde_json::Value) -> Output {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn assert_output_eq(native: &Output, routed: &Output, label: &str) {
    assert_eq!(
        routed.status.code(),
        native.status.code(),
        "status: {label}; {}",
        String::from_utf8_lossy(&routed.stderr)
    );
    assert_eq!(routed.stdout, native.stdout, "stdout: {label}");
    assert_eq!(routed.stderr, native.stderr, "stderr: {label}");
}

#[test]
fn literal_file_search_matches_native_bytes_and_status() {
    // `grep` is available on both macOS and GitHub's Ubuntu runners. The
    // routing contract is shared with `rg`, but the test must not require an
    // optional executable on the runner.
    for tool in ["grep"] {
        for flags in [
            vec![],
            vec!["-n"],
            vec!["-nH"],
            vec!["--line-number", "--no-filename"],
        ] {
            let fixture = Fixture::new(b"first needle\nplain\nlast needle");
            let mut args = flags;
            args.extend(["needle", "a file.rs"]);
            fixture.compare(tool, &args, "pixel");
        }
        Fixture::new(b"nothing here\n").compare(tool, &["-n", "needle", "a file.rs"], "pixel");
        Fixture::new(b"foo.bar\nfooXbar\n").compare(
            tool,
            &["-nF", "foo.bar", "a file.rs"],
            "pixel",
        );
    }
    Fixture::new(b"needle\n").compare("grep", &["-nHh", "needle", "a file.rs"], "pixel");
}

#[test]
fn more_than_enriched_search_default_is_not_truncated() {
    let content = "needle\n".repeat(150);
    Fixture::new(content.as_bytes()).compare("grep", &["-n", "needle", "a file.rs"], "pixel");
}

#[test]
fn unsupported_arguments_and_bytes_execute_original_native_search() {
    let cases: &[(&str, &[&str], &[u8])] = &[
        (
            "grep",
            &["-A", "2", "needle", "a file.rs"],
            b"needle\na\nb\n",
        ),
        ("grep", &["-i", "NEEDLE", "a file.rs"], b"needle\n"),
        ("grep", &["-n", "needle", "a file.rs"], b"needle\r\n"),
        (
            "grep",
            &["-n", "needle", "a file.rs"],
            b"needle\x00binary\n",
        ),
        (
            "grep",
            &["-n", "needle", "a file.rs"],
            "needle café\n".as_bytes(),
        ),
        ("grep", &["-n", "needle", "missing.rs"], b"needle\n"),
    ];
    for (tool, args, bytes) in cases {
        Fixture::new(bytes).compare(tool, args, "native");
    }
    let content = format!("needle {}\n", "x".repeat(70_000));
    Fixture::new(content.as_bytes()).compare("grep", &["needle", "a file.rs"], "native");
}

#[test]
fn modified_file_is_refreshed_before_compatibility_search() {
    let fixture = Fixture::new(b"old needle\n");
    fixture.compare("grep", &["needle", "a file.rs"], "pixel");
    std::fs::write(fixture.0.join("a file.rs"), b"new needle\n").unwrap();
    fixture.compare("grep", &["needle", "a file.rs"], "pixel");
}

#[test]
fn repeated_search_keeps_executing_and_reports_changed_file() {
    let fixture = Fixture::new(b"before needle\n");
    for attempt in 0..5 {
        if attempt == 4 {
            std::fs::write(fixture.0.join("a file.rs"), b"after needle\n").unwrap();
        }
        let output = fixture
            .command(PIXEL)
            .env_remove("PIXEL_TEST")
            .args(["search", "needle", "a file.rs", "--json", "--no-daemon"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "attempt {attempt}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let row: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            row["text"],
            if attempt == 4 {
                "after needle"
            } else {
                "before needle"
            }
        );
        if attempt >= 2 {
            assert!(String::from_utf8_lossy(&output.stderr).contains("Continuing retrieval."));
        }
    }
}

#[test]
fn provider_rewrites_preserve_metadata_and_authorize_only_codex() {
    for provider in ["claude", "codex", "devin"] {
        let fixture = Fixture::new(b"needle\n");
        let out = fixture.guard(provider, "grep -n needle 'a file.rs'", false, None);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let response: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        let output = &response["hookSpecificOutput"];
        assert!(
            output["updatedInput"]["command"]
                .as_str()
                .unwrap()
                .starts_with("pixel search-compat grep --")
        );
        assert_eq!(output["updatedInput"]["timeout_ms"], 1234);
        assert_eq!(output["updatedInput"]["extra"]["keep"], true);
        if provider == "codex" {
            assert_eq!(output["permissionDecision"], "allow");
        } else {
            assert!(output.get("permissionDecision").is_none());
        }
    }
}

#[test]
fn codex_argv_shell_events_rewrite_only_the_script_token() {
    let fixture = Fixture::new(b"needle\n");
    let payload = serde_json::json!({
        "hook_event_name": "PreToolUse",
        "tool_name": "shell",
        "cwd": fixture.0,
        "tool_input": {
            "command": ["bash", "-lc", "grep -n needle 'a file.rs'"],
            "timeout_ms": 1234,
            "extra": {"keep": true}
        }
    });
    let mut cmd = fixture.command(PIXEL);
    cmd.args(["hook", "guard", "--provider", "codex"]);
    let out = run_hook(cmd, &payload);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let response: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let output = &response["hookSpecificOutput"];
    assert_eq!(output["permissionDecision"], "allow");
    assert_eq!(output["updatedInput"]["command"][0], "bash");
    assert_eq!(output["updatedInput"]["command"][1], "-lc");
    assert!(
        output["updatedInput"]["command"][2]
            .as_str()
            .unwrap()
            .starts_with("pixel search-compat grep --")
    );
    assert_eq!(output["updatedInput"]["timeout_ms"], 1234);
    assert_eq!(output["updatedInput"]["extra"]["keep"], true);
}

#[test]
fn unsupported_provider_commands_never_get_authorized_or_rewritten() {
    let fixture = Fixture::new(b"needle\n");
    std::fs::write(fixture.0.join("#file"), b"needle\n").unwrap();
    for provider in ["claude", "codex", "devin"] {
        for command in [
            "grep -rln needle . | wc -l",
            "grep -A20 needle 'a file.rs'",
            "git reset --hard HEAD",
            "env LC_ALL=C grep needle 'a file.rs'",
            "rtk grep needle 'a file.rs'",
            "grep -F needle #file",
        ] {
            let out = fixture.guard(provider, command, false, None);
            assert!(out.status.success());
            assert!(out.stdout.is_empty(), "{provider}: {command}");
            assert!(out.stderr.is_empty(), "{provider}: {command}");
        }
        let quoted = fixture.guard(provider, "grep -F needle '#file'", false, None);
        let response: serde_json::Value = serde_json::from_slice(&quoted.stdout).unwrap();
        assert!(response["hookSpecificOutput"].get("updatedInput").is_some());
    }
}

#[test]
fn credential_shaped_paths_keep_native_permission_boundaries() {
    let fixture = Fixture::new(b"needle\n");
    std::fs::create_dir(fixture.0.join("secrets")).unwrap();
    for path in [
        ".env",
        ".env.local",
        "credentials.json",
        "test.pem",
        "id_ed25519",
        "serviceAccountKey.json",
        "secrets/fake.rs",
    ] {
        // Synthetic, nonsensitive fixture bytes only. The guard examines
        // path metadata, never the contents of credential-shaped files.
        std::fs::write(fixture.0.join(path), b"fake fixture\n").unwrap();
        for provider in ["claude", "codex", "devin"] {
            let out = fixture.guard(provider, &format!("grep needle '{path}'"), false, None);
            assert!(out.status.success(), "{provider}: {path}");
            assert!(out.stdout.is_empty(), "{provider}: {path}");
        }
    }
}

#[test]
fn native_configuration_and_environment_overrides_never_get_autoauthorized() {
    let fixture = Fixture::new(b"needle\n");
    for provider in ["claude", "codex", "devin"] {
        for key in ["RIPGREP_CONFIG_PATH", "GREP_OPTIONS", "env", "environment"] {
            let mut payload = serde_json::json!({
                "hook_event_name": "PreToolUse",
                "tool_name": if provider == "devin" { "exec" } else { "Bash" },
                "cwd": fixture.0,
                "tool_input": {"command": "rg needle 'a file.rs'"}
            });
            let mut command = fixture.command(PIXEL);
            command.args(["hook", "guard", "--provider", provider]);
            if matches!(key, "env" | "environment") {
                payload["tool_input"][key] =
                    serde_json::json!({"RIPGREP_CONFIG_PATH": "fake-native-config"});
            } else {
                command.env(key, "fake-native-config");
            }
            let output = run_hook(command, &payload);
            assert!(output.status.success(), "{provider}: {key}");
            assert!(output.stdout.is_empty(), "{provider}: {key}");
            assert!(output.stderr.is_empty(), "{provider}: {key}");
        }
    }
}

#[test]
fn claude_coordinator_delegates_rtk_exactly_once_only_on_fallback() {
    let fixture = Fixture::new(b"needle\n");
    let bin = fixture.0.join("bin");
    std::fs::create_dir(&bin).unwrap();
    let script = bin.join("rtk");
    std::fs::write(&script, "#!/bin/sh\n/bin/cat > rtk-input.json\nprintf 'rtk-response'\nprintf 'rtk-diagnostic' >&2\nexit 7\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let supported = fixture.guard("claude", "grep -n needle 'a file.rs'", true, Some(&bin));
    assert!(supported.status.success());
    assert!(!fixture.0.join("rtk-input.json").exists());
    let fallback = fixture.guard("claude", "printf hello", true, Some(&bin));
    assert_eq!(fallback.status.code(), Some(7));
    assert_eq!(fallback.stdout, b"rtk-response");
    assert_eq!(fallback.stderr, b"rtk-diagnostic");
    let delegated: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fixture.0.join("rtk-input.json")).unwrap()).unwrap();
    assert_eq!(delegated["tool_input"]["command"], "printf hello");
    assert_eq!(delegated["tool_input"]["timeout_ms"], 1234);
}

//! Real CLI metrics boundaries: invocation-local accounting, never stream decoration.
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};

const PIXEL: &str = env!("CARGO_BIN_EXE_pixel");
static NEXT: AtomicU64 = AtomicU64::new(0);

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "pixel-metrics-cli-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/login.rs"),
            "pub fn login_user(name: &str) -> bool {\n    !name.is_empty()\n}\n",
        )
        .unwrap();
        fs::write(
            root.join("src/caller.rs"),
            "use crate::login::login_user;\npub fn go() { login_user(\"someone\"); }\n",
        )
        .unwrap();
        fs::write(root.join(".gitignore"), ".pixel/\n").unwrap();
        // Native Git only prepares the isolated test repository; Pixel has no init operation.
        for args in [
            vec!["init", "-q"],
            vec!["add", "."],
            vec!["commit", "-qm", "fixture"],
        ] {
            let result = Command::new("git")
                .args([
                    "-c",
                    "user.name=Fixture",
                    "-c",
                    "user.email=fixture@example.invalid",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .current_dir(&root)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .output()
                .unwrap();
            assert!(result.status.success(), "{result:?}");
        }
        Self(root.canonicalize().unwrap())
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(PIXEL);
        cmd.current_dir(&self.0)
            .env("PIXEL_DAEMON_AUTO_START", "0")
            .env("PIXEL_METRICS", "1")
            .env_remove("PIXEL_METRICS_ROUND_TRIP_MS")
            .env_remove("PIXEL_TARGETS_GUARD")
            .env_remove("PIXEL_TARGETS_MANIFEST");
        cmd
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }

    fn events(&self, command: &str) -> Vec<Value> {
        fs::read_to_string(self.0.join(".pixel/actions.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).expect("complete JSONL record"))
            .filter(|event| event["command"] == command)
            .collect()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            crate::support::assert_no_daemon(&self.0);
        }
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn metric_lines(output: &Output) -> Vec<String> {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let lines: Vec<_> = stderr.lines().collect();
    let mut blocks = Vec::new();
    let mut index = 0;

    while index < lines.len() {
        if !lines[index].starts_with("🟩 pixel ") {
            index += 1;
            continue;
        }

        let start = index;
        index += 1;
        if lines.get(index) == Some(&"  │") {
            while index < lines.len() {
                let is_separator = lines[index].starts_with("  └");
                index += 1;
                if is_separator {
                    break;
                }
            }
        }
        blocks.push(lines[start..index].join("\n"));
    }

    blocks
}

fn short_invocation_id(event: &Value) -> String {
    let id = event["invocation_id"].as_str().unwrap();
    id.split('-').nth(1).map_or_else(
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
    )
}

fn assert_metric_identity(block: &str, event: &Value) {
    let header = block.lines().next().unwrap();
    assert!(
        header.starts_with(&format!(
            "🟩 pixel {} ❀ ",
            event["command"].as_str().unwrap()
        )),
        "unexpected metric header: {header}"
    );
    assert!(
        header.ends_with(&format!(" ❀ #{}", short_invocation_id(event))),
        "unexpected metric header: {header}"
    );
}

fn assert_success(output: &Output) {
    assert!(output.status.success(), "{output:?}");
}

#[test]
fn dual_savings_use_recorded_round_trip_policy_without_changing_search_json() {
    let fixture = Fixture::new();
    let args = ["search-content", "login_user", ".", "--json", "--no-daemon"];
    let baseline = fixture.run(&args);
    assert_success(&baseline);
    let default_event = fixture.events("search-content").pop().unwrap();
    assert_eq!(
        default_event["metrics"]["time_estimate"]["round_trip_ms"],
        2000
    );

    for (input, expected_ms) in [
        ("3500", 3500),
        ("0", 0),
        ("-1", 2000),
        ("NaN", 2000),
        ("1.5", 2000),
        ("18446744073709551616", 2000),
    ] {
        let output = fixture
            .command()
            .args(args)
            .env("PIXEL_METRICS_ROUND_TRIP_MS", input)
            .output()
            .unwrap();
        assert_success(&output);
        assert_eq!(output.stdout, baseline.stdout);
        let lines = metric_lines(&output);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("estimated LLM context saved:"));
        if expected_ms > 0 {
            assert!(lines[0].contains("against ~"));
        }
        let event = fixture.events("search-content").pop().unwrap();
        let metrics = &event["metrics"];
        let time = &metrics["time_estimate"];
        assert_eq!(time["estimator_version"], "sequential-v1");
        assert_eq!(time["round_trip_ms"], expected_ms);
        assert_eq!(time["native_command_ms"], 0);
        let steps = metrics["evidence"]["native_commands"].as_u64().unwrap()
            + metrics["evidence"]["distinct_files"].as_u64().unwrap();
        assert_eq!(time["sequential_steps"], steps);
        let expected_saved_ms = steps.saturating_sub(1) as f64 * expected_ms as f64
            - metrics["duration_us"].as_u64().unwrap() as f64 / 1000.0;
        assert!((time["saved_ms"].as_f64().unwrap() - expected_saved_ms).abs() < 0.000_001);
        if expected_ms == 0 {
            assert!(time["saved_ms"].as_f64().unwrap() < 0.0);
        }
        assert_eq!(metrics["reporting_bytes"], lines[0].len() + 2);
        assert_metric_identity(&lines[0], &event);
    }
    let disabled = fixture
        .command()
        .args(args)
        .arg("--metrics=off")
        .env("PIXEL_METRICS_ROUND_TRIP_MS", "7000")
        .output()
        .unwrap();
    assert_success(&disabled);
    assert_eq!(disabled.stdout, baseline.stdout);
    assert!(metric_lines(&disabled).is_empty());
    let event = fixture.events("search-content").pop().unwrap();
    assert_eq!(event["metrics"]["time_estimate"]["round_trip_ms"], 7000);
    assert_eq!(event["metrics"]["reporting_bytes"], 0);
}

#[test]
fn concurrent_time_policies_stay_with_their_own_invocations() {
    let fixture = Fixture::new();
    let children: Vec<_> = [0_u64, 1000, 3500, 9000]
        .into_iter()
        .map(|policy| {
            let child = fixture
                .command()
                .args(["repo-state", ".", "--json"])
                .env("PIXEL_METRICS_ROUND_TRIP_MS", policy.to_string())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            (policy, child)
        })
        .collect();
    let outputs: Vec<_> = children
        .into_iter()
        .map(|(policy, child)| (policy, child.wait_with_output().unwrap()))
        .collect();
    let events = fixture.events("repo-state");
    assert_eq!(events.len(), 4);
    let mut ids = HashSet::new();
    for (policy, output) in outputs {
        assert_success(&output);
        serde_json::from_slice::<Value>(&output.stdout).unwrap();
        let lines = metric_lines(&output);
        assert_eq!(lines.len(), 1);
        let header = lines[0].lines().next().unwrap();
        assert!(
            events
                .iter()
                .any(|event| { header.ends_with(&format!("#{}", short_invocation_id(event))) })
        );
        let event = events
            .iter()
            .find(|event| event["metrics"]["time_estimate"]["round_trip_ms"] == policy)
            .unwrap();
        assert!(ids.insert(event["invocation_id"].as_str().unwrap()));
        assert_eq!(event["metrics"]["time_estimate"]["sequential_steps"], 3);
        assert!(
            events
                .iter()
                .any(|event| event["metrics"]["reporting_bytes"] == lines[0].len() + 2),
            "each complete emitted block must be fully accounted for"
        );
    }
}

#[test]
fn time_history_preserves_assumptions_legacy_unavailability_and_exact_lines() {
    use std::io::Write;
    let fixture = Fixture::new();
    let mut historical_lines = Vec::new();
    for ms in [1000, 3500] {
        let output = fixture
            .command()
            .args(["repo-state", ".", "--json"])
            .env("PIXEL_METRICS_ROUND_TRIP_MS", ms.to_string())
            .output()
            .unwrap();
        assert_success(&output);
        historical_lines.extend(metric_lines(&output));
    }
    let originals = fixture.events("repo-state");
    let mut old_metrics = originals[0].clone();
    old_metrics["invocation_id"] = json!("old-token-only-record");
    old_metrics["metrics"]
        .as_object_mut()
        .unwrap()
        .remove("time_estimate");
    let legacy = json!({"ts_ms":1,"pid":1,"command":"legacy","args":"fixture",
        "cwd":fixture.0,"outcome":"ok","duration_ms":3});
    let mut log = fs::OpenOptions::new()
        .append(true)
        .open(fixture.0.join(".pixel/actions.jsonl"))
        .unwrap();
    for event in [&originals[0], &old_metrics, &legacy] {
        writeln!(log, "{event}").unwrap();
    }
    drop(log);
    let report = fixture.run(&["token-savings", ".", "--json", "--metrics=off"]);
    assert_success(&report);
    let data: Value = serde_json::from_slice(&report.stdout).unwrap();
    let summary = &data["workflow_metrics"];
    assert_eq!(summary["duplicate_records"], 1);
    assert_eq!(summary["legacy_records"], 1);
    assert_eq!(summary["time_unavailable_records"], 1);
    let groups = summary["time_estimates"].as_array().unwrap();
    assert_eq!(groups.len(), 2);
    for event in &originals {
        let time = &event["metrics"]["time_estimate"];
        let group = groups
            .iter()
            .find(|g| g["round_trip_ms"] == time["round_trip_ms"])
            .unwrap();
        assert_eq!(group["operations"], 1);
        assert_eq!(group["estimated"]["saved_ms"], time["saved_ms"]);
        assert_eq!(
            group["measured"]["duration_us"],
            event["metrics"]["duration_us"]
        );
    }
    // A later invocation's assumption cannot reinterpret saved records.
    let replay = fixture
        .command()
        .args(["action-log", ".", "--metrics=off"])
        .env("PIXEL_METRICS_ROUND_TRIP_MS", "99999")
        .output()
        .unwrap();
    assert_success(&replay);
    let replay = String::from_utf8(replay.stdout).unwrap();
    for line in historical_lines {
        let header = line.lines().next().unwrap();
        assert!(
            replay
                .lines()
                .any(|saved| saved.strip_prefix("  ") == Some(header)),
            "{replay}"
        );
    }
}

#[test]
fn search_json_is_identical_with_reporting_on_off_and_env_off() {
    let fixture = Fixture::new();
    let args = ["search-content", "login_user", ".", "--json", "--no-daemon"];
    let on = fixture.run(&args);
    assert_success(&on);
    let lines = metric_lines(&on);
    assert_eq!(lines.len(), 1, "{on:?}");
    assert!(String::from_utf8_lossy(&on.stdout).contains("login_user"));
    for line in String::from_utf8_lossy(&on.stdout).lines() {
        serde_json::from_str::<Value>(line).unwrap();
    }
    let events = fixture.events("search-content");
    assert_eq!(events.len(), 1, "search must finalize only one record");
    let event = &events[0];
    assert_eq!(event["outcome"], "ok");
    assert_eq!(
        event["metrics"]["output_bytes"],
        on.stdout.len() + on.stderr.len() - lines[0].len() - 2
    );
    assert_eq!(event["metrics"]["reporting_bytes"], lines[0].len() + 2);
    assert_metric_identity(&lines[0], event);
    assert!(
        event["metrics"]["evidence"]["distinct_files"]
            .as_u64()
            .unwrap()
            >= 2
    );

    let mut disabled_bytes = Vec::new();
    for before in [true, false] {
        let mut cmd = fixture.command();
        if before {
            cmd.arg("--metrics=off");
        }
        cmd.args(args);
        if !before {
            cmd.arg("--metrics=off");
        }
        let off = cmd.output().unwrap();
        assert_success(&off);
        assert_eq!(off.stdout, on.stdout);
        assert!(metric_lines(&off).is_empty());
        disabled_bytes.push(off.stdout.len() + off.stderr.len());
    }
    let env_off = fixture
        .command()
        .args(args)
        .env("PIXEL_METRICS", "0")
        .output()
        .unwrap();
    assert_success(&env_off);
    assert_eq!(env_off.stdout, on.stdout);
    assert!(metric_lines(&env_off).is_empty());
    disabled_bytes.push(env_off.stdout.len() + env_off.stderr.len());
    let events = fixture.events("search-content");
    assert_eq!(events.len(), 4, "disabled reporting retains accounting");
    for (event, bytes) in events[1..].iter().zip(disabled_bytes) {
        assert_eq!(event["metrics"]["reporting_bytes"], 0);
        assert_eq!(event["metrics"]["output_bytes"], bytes);
    }
}

#[test]
fn capped_search_marks_only_returned_evidence_partial() {
    let fixture = Fixture::new();
    let output = fixture.run(&[
        "search-content",
        "login_user",
        ".",
        "--limit",
        "1",
        "--json",
        "--no-daemon",
    ]);
    assert_success(&output);
    let lines = metric_lines(&output);
    assert_eq!(lines.len(), 1);
    assert!(lines[0].contains("partial"), "{}", lines[0]);
    let events = fixture.events("search-content");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["metrics"]["evidence"]["partial"], true);
    assert_eq!(events[0]["metrics"]["evidence"]["distinct_files"], 1);
    assert_eq!(
        events[0]["metrics"]["output_bytes"],
        output.stdout.len() + output.stderr.len() - lines[0].len() - 2
    );
}

#[test]
fn operation_error_precedes_metrics_and_preserves_failure() {
    let fixture = Fixture::new();
    let output = fixture.run(&["search-content", "(", ".", "--json", "--no-daemon"]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr.clone()).unwrap();
    let lines = metric_lines(&output);
    assert_eq!(lines.len(), 1);
    assert!(stderr.starts_with("pixel:"), "{stderr}");
    assert!(stderr.ends_with(&format!("\n{}\n", lines[0])));
    let diagnostics = stderr.strip_suffix(&format!("\n{}\n", lines[0])).unwrap();
    assert!(diagnostics.contains("regex") || diagnostics.contains("pattern"));
    let events = fixture.events("search-content");
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["outcome"], "error");
    assert_eq!(events[0]["metrics"]["output_bytes"], diagnostics.len());
    assert!(events[0]["metrics"]["native_workflow_bytes"].is_null());
    assert_metric_identity(&lines[0], &events[0]);

    let disabled = fixture.run(&[
        "--metrics=off",
        "search-content",
        "(",
        ".",
        "--json",
        "--no-daemon",
    ]);
    assert_eq!(output.status.code(), disabled.status.code());
    assert_eq!(disabled.stdout, output.stdout);
    assert_eq!(disabled.stderr, diagnostics.as_bytes());
}

#[test]
fn concurrent_invocations_keep_unique_complete_records_and_lines() {
    let fixture = Fixture::new();
    let children: Vec<_> = (0..4)
        .map(|_| {
            fixture
                .command()
                .args(["repo-state", ".", "--json"])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    let outputs: Vec<_> = children
        .into_iter()
        .map(|child| child.wait_with_output().unwrap())
        .collect();
    let events = fixture.events("repo-state");
    assert_eq!(events.len(), 4);
    let ids: HashSet<_> = events
        .iter()
        .map(|event| event["invocation_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), 4);
    for output in outputs {
        assert_success(&output);
        let document: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(document["head"].as_str().unwrap().len() >= 40);
        let lines = metric_lines(&output);
        assert_eq!(lines.len(), 1);
        let event = events
            .iter()
            .find(|event| {
                lines[0].lines().next().is_some_and(|header| {
                    header.ends_with(&format!("#{}", short_invocation_id(event)))
                })
            })
            .expect("each live metrics block must identify its own action record");
        assert_metric_identity(&lines[0], event);
        assert_eq!(event["metrics"]["output_bytes"], output.stdout.len());
        assert_eq!(event["metrics"]["reporting_bytes"], lines[0].len() + 2);
    }
}

#[test]
fn action_log_failure_does_not_change_success_or_reporting() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.0.join(".pixel/actions.jsonl")).unwrap();
    let output = fixture.run(&["repo-state", ".", "--json"]);
    assert_success(&output);
    let document: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(document["dirty_count"], 0);
    assert_eq!(metric_lines(&output).len(), 1);
    assert!(fixture.0.join(".pixel/actions.jsonl").is_dir());
}

#[test]
fn protected_native_hook_and_statusline_streams_have_no_metrics_line() {
    let fixture = Fixture::new();
    let native = Command::new("grep")
        .args(["-n", "login_user", "src/login.rs"])
        .current_dir(&fixture.0)
        .env_remove("GREP_OPTIONS")
        .output()
        .unwrap();
    let routed = fixture
        .command()
        .args([
            "search-like-rg",
            "grep",
            "--",
            "-n",
            "login_user",
            "src/login.rs",
        ])
        .env_remove("GREP_OPTIONS")
        .output()
        .unwrap();
    assert_eq!(routed.status.code(), native.status.code());
    assert_eq!(routed.stdout, native.stdout);
    assert_eq!(routed.stderr, native.stderr);
    assert!(String::from_utf8_lossy(&routed.stdout).contains("pub fn login_user"));

    for args in [
        vec!["status", ".", "--statusline"],
        vec!["run-hook", "session-start", "."],
    ] {
        let output = fixture.run(&args);
        assert_success(&output);
        assert!(
            !output.stdout.is_empty(),
            "protected operation must exercise real output"
        );
        assert!(metric_lines(&output).is_empty());
        assert!(!String::from_utf8_lossy(&output.stdout).contains("🟩 pixel "));
        let events = fixture.events(args[0]);
        assert!(!events.is_empty());
        assert!(
            events.last().unwrap()["metrics"].is_null(),
            "unsupported volume capture is unavailable"
        );
    }
}

#[test]
fn explicit_repository_impact_logs_at_target_and_preserves_json() {
    let fixture = Fixture::new();
    // First graph construction intentionally adds timing/freshness metadata;
    // compare equivalent warm results rather than cold-vs-warm output.
    assert_success(&fixture.run(&["repo-map", ".", "--json", "--metrics=off"]));
    let output = fixture
        .command()
        .current_dir(std::env::temp_dir())
        .args(["impact", "login_user"])
        .arg(&fixture.0)
        .arg("--json")
        .output()
        .unwrap();
    assert_success(&output);
    let data: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(data["affected_files"].as_u64().unwrap() >= 1, "{data}");
    let lines = metric_lines(&output);
    assert_eq!(lines.len(), 1);
    let events = fixture.events("impact");
    assert_eq!(events.len(), 1);
    assert!(
        events[0]["metrics"]["evidence"]["relationships"]
            .as_u64()
            .unwrap()
            >= 1
    );
    let off = fixture.run(&["impact", "login_user", ".", "--json", "--metrics=off"]);
    assert_success(&off);
    assert_eq!(output.stdout, off.stdout);
}

#[test]
fn legacy_savings_and_error_details_survive_with_workflow_summary() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.0.join(".pixel")).unwrap();
    let legacy = json!({
        "ts_ms": 1, "pid": 1, "command": "search", "args": "legacy needle",
        "cwd": fixture.0, "outcome": "ok", "duration_ms": 3,
        "snippet_cap_chars": 20, "pool_chars": 80
    });
    fs::write(
        fixture.0.join(".pixel/actions.jsonl"),
        format!("{legacy}\n"),
    )
    .unwrap();
    assert_success(&fixture.run(&["repo-state", ".", "--json"]));
    let failed = fixture.run(&["search-content", "(", ".", "--json", "--no-daemon"]);
    assert!(!failed.status.success());
    let errors = fixture.run(&["action-log", ".", "--errors-only", "--metrics=off"]);
    assert_success(&errors);
    let text = String::from_utf8(errors.stdout).unwrap();
    assert!(text.contains("search-content ("), "arguments lost: {text}");
    assert!(
        text.contains("regex") || text.contains("pattern"),
        "error lost: {text}"
    );
    let report = fixture.run(&["token-savings", ".", "--json", "--metrics=off"]);
    assert_success(&report);
    let data: Value = serde_json::from_slice(&report.stdout).unwrap();
    assert_eq!(data["total_pool_chars"], 80);
    assert_eq!(data["total_snippet_chars"], 20);
    assert_eq!(data["overall_savings"], 0.75);
    assert_eq!(data["workflow_metrics"]["legacy_records"], 1);
    assert!(
        data["workflow_metrics"]["versions"]["workflow-v1"]["complete"]["operations"]
            .as_u64()
            .unwrap()
            >= 1
    );
    assert!(
        data["workflow_metrics"]["versions"]["workflow-v1"]["unavailable"]["operations"]
            .as_u64()
            .unwrap()
            >= 1
    );
}

#[test]
fn graph_text_and_json_account_for_same_returned_files() {
    let fixture = Fixture::new();
    for args in [vec!["find-code", "login_user", "."], vec!["repo-map", "."]] {
        let mut text_args = args.clone();
        if args[0] == "repo-map" {
            text_args.push("--markdown");
        }
        let text_output = fixture.run(&text_args);
        assert_success(&text_output);
        assert!(String::from_utf8_lossy(&text_output.stdout).contains("login_user"));
        let mut json_args = args.clone();
        json_args.push("--json");
        let json_output = fixture.run(&json_args);
        assert_success(&json_output);
        serde_json::from_slice::<Value>(&json_output.stdout).unwrap();
        let events = fixture.events(args[0]);
        assert_eq!(events.len(), 2);
        let text_files = events[0]["metrics"]["evidence"]["distinct_files"]
            .as_u64()
            .unwrap();
        assert!(text_files > 0);
        assert_eq!(
            text_files,
            events[1]["metrics"]["evidence"]["distinct_files"]
                .as_u64()
                .unwrap()
        );
        assert_ne!(
            events[0]["metrics"]["output_bytes"],
            events[1]["metrics"]["output_bytes"]
        );
        assert_eq!(metric_lines(&text_output).len(), 1);
        assert_eq!(metric_lines(&json_output).len(), 1);
    }
}

#[test]
fn negative_workflow_savings_are_not_clamped() {
    let fixture = Fixture::new();
    for i in 0..40 {
        fs::write(
            fixture.0.join(format!(
                "src/{}_{}.rs",
                "long_fixture_filename".repeat(6),
                i
            )),
            "pub fn item() {}\n",
        )
        .unwrap();
    }
    let output = fixture.run(&["repo-state", ".", "--json"]);
    assert_success(&output);
    let data: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(data["dirty_count"], 40);
    let lines = metric_lines(&output);
    assert_eq!(lines.len(), 1);
    assert!(
        !lines[0].contains("estimated LLM context saved:"),
        "negative token savings must not be presented as a saving: {}",
        lines[0]
    );
    let event = &fixture.events("repo-state")[0];
    let metrics = &event["metrics"];
    assert!(
        metrics["native_workflow_bytes"].as_u64().unwrap()
            < metrics["output_bytes"].as_u64().unwrap()
                + metrics["reporting_bytes"].as_u64().unwrap()
    );
}

#[test]
fn metrics_preserve_safe_publication_replay_and_head_guard() {
    let fixture = Fixture::new();
    for (key, value) in [
        ("user.name", "Fixture"),
        ("user.email", "fixture@example.invalid"),
        ("commit.gpgsign", "false"),
    ] {
        assert!(
            Command::new("git")
                .args(["config", key, value])
                .current_dir(&fixture.0)
                .status()
                .unwrap()
                .success()
        );
    }
    let before: Value =
        serde_json::from_slice(&fixture.run(&["repo-state", "--json"]).stdout).unwrap();
    let head = before["head"].as_str().unwrap();
    fs::write(
        fixture.0.join("src/login.rs"),
        "pub fn login_user() -> bool { true }\n",
    )
    .unwrap();
    let args = [
        "commit",
        "--message",
        "test: greeting fixture",
        "--request-id",
        "metrics-publication",
        "--expected-head",
        head,
        "--files",
        "src/login.rs",
        "--json",
    ];
    let first = fixture.run(&args);
    assert_success(&first);
    let published: Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(published["published"], true);
    assert_ne!(published["head"], before["head"]);
    let replay = fixture.run(&args);
    assert_success(&replay);
    assert_eq!(
        serde_json::from_slice::<Value>(&replay.stdout).unwrap()["head"],
        published["head"]
    );
    fs::write(
        fixture.0.join("src/login.rs"),
        "pub fn login_user() -> bool { false }\n",
    )
    .unwrap();
    let refused = fixture.run(&[
        "commit",
        "--message",
        "test: stale guard",
        "--request-id",
        "stale-metrics-publication",
        "--expected-head",
        head,
        "--files",
        "src/login.rs",
        "--json",
    ]);
    assert_eq!(refused.status.code(), Some(1));
    assert_eq!(metric_lines(&refused).len(), 1);
    let after: Value =
        serde_json::from_slice(&fixture.run(&["repo-state", "--json"]).stdout).unwrap();
    assert_eq!(after["head"], published["head"]);
    let events = fixture.events("commit");
    assert_eq!(events.len(), 3);
    assert_eq!(events[2]["outcome"], "error");
    assert_eq!(events[2]["metrics"]["output_scope"], "cli-rendered-streams");
}

#[test]
fn daemon_reindex_reports_actual_nested_index_counts() {
    let fixture = Fixture::new();
    assert_success(&fixture.run(&["build-index"]));
    assert_success(&fixture.run(&["daemon", "start"]));
    let reindexed = fixture.run(&["build-index"]);
    let status = fixture.run(&["status", "--json"]);
    // Stop before assertions so a failed count assertion never leaves a daemon.
    assert_success(&fixture.run(&["daemon", "stop"]));
    assert_success(&reindexed);
    assert_success(&status);
    let value: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert!(value["index"]["base_files"].as_u64().unwrap() > 0);
    let expected = format!(
        "indexed via daemon: base_files={} delta_files={} overlay_files={}",
        value["index"]["base_files"],
        value["index"]["delta_files"],
        value["index"]["overlay_files"]
    );
    assert!(String::from_utf8_lossy(&reindexed.stderr).contains(&expected));
    assert_eq!(metric_lines(&reindexed).len(), 1);
}

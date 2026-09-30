//! `pixel ai-cli-readify` at the command line: the contract a caller sees.
//!
//! The command's job is to say what is true, so the tests here are about
//! what it refuses to claim as much as what it reports. Every run in this
//! file is pointed at an empty HOME with no provider keys, which is the one
//! state that needs no network: three classified failures and four agents
//! that could not reach a model. A test that reached a real provider would
//! be a test about the machine, not about the command.

use crate::support::{neutral_home, pixel_command};

/// Run the command with a HOME of its own and every provider key removed, so
/// nothing it does depends on the machine it runs on.
fn readify(args: &[&str]) -> std::process::Output {
    let mut command = pixel_command();
    command.args(args);
    command.env("HOME", neutral_home());
    for key in [
        "OLLAMA_API_KEY",
        "GROQ_API_KEY",
        "CEREBRAS_API_KEY",
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_CUSTOM_HEADERS",
        "ANTHROPIC_BASE_URL",
    ] {
        command.env_remove(key);
    }
    command.output().expect("pixel ai-cli-readify runs")
}

/// The JSON report as a value, asserting the run produced one at all.
fn report(args: &[&str]) -> serde_json::Value {
    let out = readify(args);
    assert!(
        out.status.success(),
        "a run that establishes nothing is still a successful run: {out:?}"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("stdout is not JSON ({e}): {stdout}"))
}

#[test]
fn the_json_report_names_all_three_providers_in_priority_order() {
    let report = report(&["ai-cli-readify", "--json", "--timeout", "1"]);
    let providers: Vec<&str> = report["providers"]
        .as_array()
        .expect("a providers array")
        .iter()
        .map(|row| row["provider"].as_str().unwrap())
        .collect();
    // The order is the priority order, not the completion order: the three
    // probes run concurrently and the report must not reflect which socket
    // answered first.
    assert_eq!(providers, ["ollama", "groq", "cerebras"]);
}

#[test]
fn a_provider_with_no_key_is_reported_as_a_missing_key() {
    let report = report(&["ai-cli-readify", "--json", "--timeout", "1"]);
    for row in report["providers"].as_array().unwrap() {
        assert_eq!(row["ready"], false, "{row}");
        assert_eq!(
            row["failure"], "no key",
            "an absent key is its own condition, not an unreachable provider: {row}"
        );
    }
}

#[test]
fn no_provider_answering_selects_none_and_rewrites_nothing() {
    let report = report(&["ai-cli-readify", "--json", "--timeout", "1"]);
    assert_eq!(report["selected"], serde_json::Value::Null);
    assert!(
        report["applied"].as_array().unwrap().is_empty(),
        "nothing may be rewritten when no provider answered: {report}"
    );
}

#[test]
fn every_agent_is_reported_when_no_agent_flag_was_given() {
    let report = report(&["ai-cli-readify", "--json", "--timeout", "1"]);
    let agents: Vec<&str> = report["agents"]
        .as_array()
        .expect("an agents array")
        .iter()
        .map(|row| row["agent"].as_str().unwrap())
        .collect();
    assert_eq!(agents, ["codex", "claude", "antigravity", "devin"]);
}

#[test]
fn the_agent_flag_narrows_the_report_to_the_agents_named() {
    let report = report(&[
        "ai-cli-readify",
        "--json",
        "--timeout",
        "1",
        "--agent",
        "devin",
    ]);
    let agents: Vec<&str> = report["agents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["agent"].as_str().unwrap())
        .collect();
    assert_eq!(agents, ["devin"]);
}

#[test]
fn an_agent_that_cannot_reach_a_provider_is_not_reported_ready() {
    let report = report(&["ai-cli-readify", "--json", "--timeout", "1"]);
    for row in report["agents"].as_array().unwrap() {
        assert_eq!(row["ready"], false, "{row}");
        // The reason has to be in the row: "not ready" alone is the
        // narrowing this command exists to refuse.
        assert!(
            !row["detail"].as_str().unwrap().trim().is_empty(),
            "a not-ready agent must say why: {row}"
        );
    }
}

#[test]
fn devin_is_verified_and_never_rewritten() {
    let report = report(&["ai-cli-readify", "--json", "--timeout", "1"]);
    let verified: Vec<String> = report["verified_only"]
        .as_array()
        .unwrap()
        .iter()
        .map(|line| line.as_str().unwrap().to_string())
        .collect();
    assert!(
        verified.iter().any(|line| line.starts_with("devin:")),
        "Devin's absence from the rewrite list must read as a decision: {report}"
    );
}

#[test]
fn the_non_json_report_states_the_overall_verdict() {
    let out = readify(&["ai-cli-readify", "--timeout", "1"]);
    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("providers"), "{stdout}");
    assert!(stdout.contains("agents"), "{stdout}");
    assert!(
        stdout.contains("overall: not ready"),
        "with no key and no provider the verdict must not be ready: {stdout}"
    );
    assert!(
        !stdout.contains("overall: ready"),
        "an unready run must not print the ready verdict anywhere: {stdout}"
    );
}

#[test]
fn apply_writes_nothing_when_no_provider_answered() {
    // `--apply` is the one flag that touches the user's files, so the case
    // where it must do nothing is asserted against a HOME it could have
    // written into, and the files are read back afterwards rather than
    // trusted. `neutral_home` is shared, so the check is that the run added
    // nothing rather than that the directory is empty.
    let home = neutral_home();
    let before: Vec<std::path::PathBuf> = ["codex/config.toml", "claude/settings.json"]
        .iter()
        .map(|rel| home.join(rel))
        .collect();
    for path in &before {
        assert!(
            !path.exists(),
            "this test needs a HOME without agent configs, found {}",
            path.display()
        );
    }
    let out = readify(&["ai-cli-readify", "--apply", "--timeout", "1"]);
    assert!(out.status.success(), "{out:?}");
    for path in &before {
        assert!(
            !path.exists(),
            "--apply must not create {} when no provider was chosen for it",
            path.display()
        );
    }
}

#[test]
fn approvals_are_not_attempted_without_the_flag_and_the_report_says_so() {
    let report = report(&["ai-cli-readify", "--json", "--timeout", "1"]);
    assert_eq!(
        report["approvals"],
        serde_json::Value::Null,
        "a run that never asked must not report an empty approval list, which is what a run that asked and found nothing reports: {report}"
    );
    let out = readify(&["ai-cli-readify", "--timeout", "1"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("approvals: not attempted"),
        "the absence of the section must read as a decision: {stdout}"
    );
}

#[test]
fn the_approve_flag_clears_nothing_when_the_agent_binaries_are_absent() {
    // `--approve` is the one flag that writes a trust decision, so the case
    // where it must write nothing is run with a PATH that cannot reach
    // `codex`, against a HOME it could have written into. Nothing here is
    // asserted about *why* each agent failed beyond the one path this
    // command owns: the point is that a missing binary is not an approval.
    let mut command = pixel_command();
    command
        .args(["ai-cli-readify", "--json", "--approve", "--timeout", "1"])
        .env("HOME", neutral_home())
        .env("PATH", empty_path());
    for key in [
        "OLLAMA_API_KEY",
        "GROQ_API_KEY",
        "CEREBRAS_API_KEY",
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
    ] {
        command.env_remove(key);
    }
    let out = command.output().expect("pixel ai-cli-readify runs");
    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let report: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stdout is not JSON ({e}): {stdout}"));

    let rows = report["approvals"]
        .as_array()
        .expect("--approve reports one row per agent");
    assert_eq!(rows.len(), 4, "{report}");
    for row in rows {
        assert_eq!(row["approved"], false, "{row}");
        assert!(
            !row["detail"].as_str().unwrap().trim().is_empty(),
            "a gate that was not cleared must say why: {row}"
        );
    }
    for path in ["codex/config.toml", ".claude.json"] {
        assert!(
            !neutral_home().join(path).exists(),
            "--approve must not create {path} when it cleared nothing"
        );
    }
}

#[test]
fn the_agents_with_no_approval_path_say_so_rather_than_reporting_a_failure() {
    let mut command = pixel_command();
    command
        .args(["ai-cli-readify", "--json", "--approve", "--timeout", "1"])
        .env("HOME", neutral_home())
        .env("PATH", empty_path());
    let out = command.output().expect("pixel ai-cli-readify runs");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let report: serde_json::Value = serde_json::from_str(&stdout).expect("a JSON report");
    for agent in ["antigravity", "devin"] {
        let row = report["approvals"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["agent"] == agent)
            .unwrap_or_else(|| panic!("no approval row for {agent}: {report}"));
        assert!(
            row["detail"].as_str().unwrap().contains("no approval path"),
            "{agent} has no writer in the reference, and that must read as a decision: {row}"
        );
    }
}

/// A directory with nothing in it, for a run that must not find an agent
/// binary: `PATH` pointing there is how a test proves that a missing
/// executable is reported rather than spawned.
fn empty_path() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("pixel-cli-empty-path-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create the empty PATH directory");
        dir
    })
}

#[test]
fn the_timeout_flag_is_honoured_and_the_run_is_bounded() {
    // Written to fail on a hang rather than on a slow machine: the run with
    // no key never opens a socket, so a second is generous, and a run that
    // ignores its budget would still be waiting here long after it.
    let started = std::time::Instant::now();
    let out = readify(&["ai-cli-readify", "--timeout", "1"]);
    assert!(out.status.success(), "{out:?}");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(30),
        "the run took {:?} with no key to reach any provider",
        started.elapsed()
    );
}

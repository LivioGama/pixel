//! `pixel doctor`'s exit contract: 0 when no check reaches `--fail-on`, 1
//! when one does, 2 when the checks could not run. Agents and CI gate on the
//! code alone, so a red report that exits 0 reads as healthy.

use std::path::Path;
use std::process::Output;

use crate::support::{Scratch, pixel_command};

fn doctor(home: &Path, repo: &Path, args: &[&str]) -> Output {
    pixel_command()
        .arg("doctor")
        .arg(repo)
        .args(["--shell", "zsh"])
        .args(args)
        .env("HOME", home)
        .env("CODEX_HOME", home.join(".codex"))
        .env("PIXEL_METRICS", "0")
        .output()
        .unwrap()
}

fn fixture(tag: &str) -> (Scratch, Scratch) {
    let home = Scratch::for_test("doctor-cli-home", tag);
    let repo = Scratch::for_test("doctor-cli-repo", tag);
    (home, repo)
}

#[test]
fn doctor_should_exit_1_with_the_fix_when_a_check_is_red() {
    let (home, repo) = fixture("red");
    let out = doctor(&home, &repo, &[]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.starts_with("pixel doctor: ran "), "{text}");
    assert!(
        text.contains("  [red] install.agent-prompt: agent-prompt.md not deployed"),
        "{text}"
    );
    assert!(text.contains("    fix: pixel install\n"), "{text}");
    assert!(
        !text.contains("[green]"),
        "green checks stay in the tally: {text}"
    );
}

/// `--json` changes the format, not the verdict: the report is still whole
/// and parsable, and the exit code still says it is unhealthy.
#[test]
fn doctor_json_should_keep_the_exit_code_and_carry_the_fix() {
    let (home, repo) = fixture("json");
    let out = doctor(&home, &repo, &["--json", "--skip", "install.pi-prompt"]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["ok"], false);
    assert_eq!(report["summary"]["skipped"], 1, "{report}");
    let checks = report["checks"].as_array().unwrap();
    let find = |id: &str| checks.iter().find(|c| c["id"] == id);
    assert!(find("install.pi-prompt").is_none(), "--skip left it out");
    assert_eq!(
        find("install.agent-prompt").unwrap()["fix"],
        "pixel install"
    );
    // The repo checks judge the path given on the command line, and their
    // fix names it.
    let index = find("index.freshness").expect("repo checks ran");
    assert_eq!(
        index["fix"],
        format!("pixel prepare-repo '{}'", repo.display()),
        "{index}"
    );
    // The installed rule text is dry-run against this binary's own parser.
    assert!(find("rule.parity").is_some(), "{report}");
    // `--shell zsh` decides which profile the wrapper check reads first.
    let profiles = &find("install.legacy-wrappers").unwrap()["detail"]["profiles_checked"];
    assert!(
        profiles[0].as_str().unwrap().ends_with(".zshrc"),
        "{profiles}"
    );
}

#[test]
fn doctor_should_exit_0_when_the_selected_checks_are_green() {
    let (home, repo) = fixture("only");
    let out = doctor(&home, &repo, &["--only", "binary.path"]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        text.starts_with("pixel doctor: ran 1 check(s), skipped "),
        "{text}"
    );
}

/// A yellow finding passes the default gate and fails `--fail-on yellow`,
/// the posture for "confirm everything is green".
#[test]
fn doctor_fail_on_yellow_should_turn_a_yellow_check_into_exit_1() {
    let (home, repo) = fixture("yellow");
    // In a bare home no rule text is installed: rule.scenarios is yellow.
    let only = ["--only", "rule.scenarios"];
    let default = doctor(&home, &repo, &only);
    assert_eq!(default.status.code(), Some(0), "{default:?}");
    assert!(
        String::from_utf8_lossy(&default.stdout).contains("[yellow] rule.scenarios"),
        "{default:?}"
    );
    let strict = doctor(&home, &repo, &[only[0], only[1], "--fail-on", "yellow"]);
    assert_eq!(strict.status.code(), Some(1), "{strict:?}");
}

#[test]
fn doctor_should_exit_2_on_an_unknown_check_id() {
    let (home, repo) = fixture("unknown");
    let out = doctor(&home, &repo, &["--only", "install.shell-wrappers"]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(out.stdout.is_empty(), "no partial report: {out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr)
            .contains("unknown doctor check `install.shell-wrappers`"),
        "{out:?}"
    );
}

#[test]
fn doctor_list_should_name_every_check_with_its_fix() {
    let (home, repo) = fixture("list");
    let out = doctor(&home, &repo, &["--list"]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        text.lines().count(),
        pixel_install::doctor::CHECKS.len(),
        "{text}"
    );
    assert!(
        text.lines().any(|l| l.starts_with("facts.freshness")
            && l.ends_with("pixel build-index --history {root}")),
        "{text}"
    );

    let json = doctor(&home, &repo, &["--list", "--json"]);
    let catalogue: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(catalogue[0]["id"], "binary.path", "{catalogue}");
    assert_eq!(catalogue[0]["fix"], serde_json::Value::Null);
}

/// After an upgrade the prompts `pixel install` deployed are the old
/// release's, and agents keep reading them: every ordinary command names
/// them in one stderr line, while `doctor`, which reports them itself, and a
/// home where nothing was ever deployed stay quiet.
#[test]
fn a_stale_deployed_prompt_is_named_by_ordinary_commands_but_not_by_doctor() {
    let (home, repo) = fixture("stale-prompt");
    let ordinary = || {
        pixel_command()
            .args(["action-log", "--limit", "1"])
            .env("HOME", &*home)
            .output()
            .unwrap()
    };
    let never_installed = ordinary();
    assert!(never_installed.status.success(), "{never_installed:?}");
    let stderr = String::from_utf8_lossy(&never_installed.stderr).into_owned();
    assert!(!stderr.contains("pixel install"), "{stderr}");

    let deployed = home.join(".local/share/pixel");
    std::fs::create_dir_all(&deployed).unwrap();
    std::fs::write(
        deployed.join("agent-prompt.md"),
        "# an older release's prompt\n",
    )
    .unwrap();
    let warned = ordinary();
    assert!(warned.status.success(), "{warned:?}");
    let stderr = String::from_utf8_lossy(&warned.stderr).into_owned();
    let notes: Vec<&str> = stderr
        .lines()
        .filter(|line| line.starts_with("note: agent-prompt.md"))
        .collect();
    assert_eq!(notes.len(), 1, "{stderr}");
    assert!(
        notes[0].contains("does not match pixel")
            && notes[0].ends_with("run `pixel install` to update"),
        "{}",
        notes[0]
    );

    let report = doctor(&home, &repo, &["--only", "install.agent-prompt"]);
    let stderr = String::from_utf8_lossy(&report.stderr).into_owned();
    assert!(!stderr.contains("note: agent-prompt.md"), "{stderr}");
    let text = String::from_utf8_lossy(&report.stdout).into_owned();
    assert!(text.contains("agent-prompt.md is stale"), "{text}");
}

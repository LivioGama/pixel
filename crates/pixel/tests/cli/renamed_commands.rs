//! The pre-rename command names stay accepted until 1.0.
//!
//! The clean-break rename (08268b0) broke every consumer that tracked
//! `develop`: a CI job calling `pixel ready` got "unrecognized subcommand".
//! These tests hold the compatibility contract end to end, through the built
//! binary: an old name parses to the same command as its new name, answers
//! byte-for-byte the same, and announces the new name on stderr only where
//! live reporting is allowed.

use std::path::Path;
use std::process::{Command, Output, Stdio};

use pixel_proto::commands::RENAMED_COMMANDS;

use crate::support::{Scratch, pixel_command};

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

fn fixture(tag: &str) -> Scratch {
    let dir = Scratch::for_test("pixel-renamed", tag);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src/login.rs"),
        "pub fn login_user(name: &str) -> bool {\n    !name.is_empty()\n}\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src/caller.rs"),
        "use crate::login::login_user;\npub fn go() { login_user(\"a\"); }\n",
    )
    .unwrap();
    std::fs::write(dir.join(".gitignore"), ".pixel/\n").unwrap();
    git(&dir, &["init", "-q"]);
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "fixture"]);
    dir
}

/// Run in `dir` with the metrics environment cleared, so the caller decides
/// whether live reporting is on.
fn pixel(dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut command = pixel_command();
    command
        .args(args)
        .current_dir(dir)
        .env_remove("PIXEL_METRICS")
        .stdin(Stdio::null());
    for (key, value) in envs {
        command.env(key, value);
    }
    command.output().unwrap()
}

fn stderr_notes(out: &Output) -> Vec<String> {
    String::from_utf8_lossy(&out.stderr)
        .lines()
        .filter(|line| line.starts_with("note: '"))
        .map(str::to_string)
        .collect()
}

#[test]
fn every_old_name_prints_the_help_and_exit_code_of_its_new_name() {
    let dir = Scratch::for_test("pixel-renamed", "help");
    for (old, new) in RENAMED_COMMANDS {
        let from_old = pixel(&dir, &[old, "--help"], &[]);
        let from_new = pixel(&dir, &[new, "--help"], &[]);
        assert_eq!(
            from_old.status.code(),
            from_new.status.code(),
            "`{old} --help` and `{new} --help` must exit alike"
        );
        assert_eq!(
            from_new.status.code(),
            Some(0),
            "`{new} --help`: {from_new:?}"
        );
        // clap renders the usage under the command's own name whichever
        // spelling selected it; normalising the usage line alone covers a
        // clap that would echo the alias there, without rewriting help text
        // that quotes a command name on purpose.
        let old_text = String::from_utf8_lossy(&from_old.stdout).replace(
            &format!("Usage: pixel {old} "),
            &format!("Usage: pixel {new} "),
        );
        let new_text = String::from_utf8_lossy(&from_new.stdout);
        assert!(
            new_text.contains(&format!("Usage: pixel {new}")),
            "`{new} --help` has no usage line: {new_text}"
        );
        assert_eq!(
            old_text, new_text,
            "`{old} --help` differs from `{new} --help`"
        );
        assert!(
            from_old.stderr.is_empty(),
            "`{old} --help` must not print the rename note: {}",
            String::from_utf8_lossy(&from_old.stderr)
        );
    }
}

#[test]
fn ready_answers_with_the_json_of_prepare_repo() {
    // The exact line the yespark-rails CI job runs.
    const FLAGS: [&str; 4] = ["--no-daemon", "--metrics", "off", "--json"];
    let dir = fixture("ready-json");
    let run = |name: &str| {
        let mut args = vec![name];
        args.extend(FLAGS);
        let out = pixel(&dir, &args, &[]);
        assert!(out.status.success(), "{name}: {out:?}");
        assert!(
            out.stderr.is_empty(),
            "--metrics off silences both the metrics line and the rename note: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let mut doc: serde_json::Value = serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|e| panic!("{name} stdout is one JSON document ({e}): {out:?}"));
        // Wall-clock time of the graph pass is the only field that varies
        // between two runs on the same tree.
        doc["graph"]
            .as_object_mut()
            .expect("graph block")
            .remove("elapsed_ms");
        doc
    };
    // The first run builds the index and the graph; compare two warm runs.
    let _ = run("prepare-repo");
    let from_old = run("ready");
    let from_new = run("prepare-repo");
    assert_eq!(from_old, from_new);
    assert_eq!(from_old["graph"]["symbols"], 2, "{from_old}");
}

#[test]
fn an_old_name_announces_its_new_name_once_on_stderr_only_when_live() {
    let dir = fixture("note");
    let warm = pixel(
        &dir,
        &["prepare-repo", "--no-daemon", "--metrics", "off"],
        &[],
    );
    assert!(warm.status.success(), "{warm:?}");

    let live = pixel(&dir, &["ready", "--no-daemon", "--json"], &[]);
    assert!(live.status.success(), "{live:?}");
    assert_eq!(
        stderr_notes(&live),
        vec!["note: 'ready' is now 'prepare-repo'; the old name stays accepted until 1.0"]
    );
    let stdout: serde_json::Value = serde_json::from_slice(&live.stdout)
        .unwrap_or_else(|e| panic!("the note must never reach stdout ({e}): {live:?}"));
    assert!(stdout.get("graph").is_some(), "{stdout}");

    let current = pixel(&dir, &["prepare-repo", "--no-daemon", "--json"], &[]);
    assert!(stderr_notes(&current).is_empty(), "{current:?}");

    let env_off = pixel(
        &dir,
        &["ready", "--no-daemon", "--json"],
        &[("PIXEL_METRICS", "0")],
    );
    assert!(env_off.status.success(), "{env_off:?}");
    assert!(
        stderr_notes(&env_off).is_empty(),
        "PIXEL_METRICS=0: {env_off:?}"
    );

    let flag_off = pixel(&dir, &["--metrics=off", "ready", "--no-daemon"], &[]);
    assert!(flag_off.status.success(), "{flag_off:?}");
    assert!(
        stderr_notes(&flag_off).is_empty(),
        "--metrics=off: {flag_off:?}"
    );

    // A hook response is a protected stream: the old `pixel hook …` entries
    // every 0.2.x install wrote must keep answering without extra output.
    let hook = pixel(&dir, &["hook", "session-start", "."], &[]);
    assert!(hook.status.success(), "{hook:?}");
    assert!(stderr_notes(&hook).is_empty(), "hook: {hook:?}");
}

#[test]
fn migrate_is_a_hidden_no_op_that_exits_zero() {
    let dir = fixture("migrate");
    std::fs::create_dir_all(dir.join(".gitpixel")).unwrap();
    for args in [&["migrate"][..], &["migrate", ".", "--json"][..]] {
        let out = pixel(&dir, args, &[("PIXEL_METRICS", "0")]);
        assert_eq!(out.status.code(), Some(0), "{args:?}: {out:?}");
        assert!(out.stdout.is_empty(), "{args:?} prints no result: {out:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("'migrate' was removed and does nothing"),
            "{args:?}: {out:?}"
        );
    }
    assert!(
        dir.join(".gitpixel").is_dir(),
        "a no-op must not delete the legacy directory it used to remove"
    );
    let help = pixel(&dir, &["--help"], &[]);
    assert!(
        !String::from_utf8_lossy(&help.stdout).contains("migrate"),
        "migrate stays out of --help"
    );
}

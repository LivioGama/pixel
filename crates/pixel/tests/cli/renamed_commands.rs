//! After the clean-break rename, old command names and old protocol op tags are
//! not accepted. These tests verify that the CLI rejects the pre-rename
//! vocabulary and that `migrate` remains a hidden no-op.

use std::path::Path;
use std::process::{Command, Output, Stdio};

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

#[test]
fn old_command_names_are_rejected() {
    let dir = fixture("rejected");
    for old in [
        "ready", "search", "targets", "symbol", "resolve", "publish", "ship", "inspect", "changes",
        "context",
    ] {
        let out = pixel(&dir, &[old, "--help"], &[]);
        assert!(
            !out.status.success(),
            "`{old}` must not be accepted: {out:?}"
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("unrecognized subcommand"),
            "`{old}` error must mention unrecognized subcommand: {stderr}"
        );
    }
}

#[test]
fn current_command_names_work() {
    let dir = fixture("current");
    for name in [
        "prepare-repo",
        "search-content",
        "scope-task",
        "find-symbol",
        "repo-state",
    ] {
        let out = pixel(&dir, &[name, "--help"], &[]);
        assert!(out.status.success(), "`{name}` must work: {out:?}");
    }
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

//! CLI output contract for `--json`.
//!
//! Agents parse `pixel … --json` stdout with a JSON parser. The contract
//! this file pins down is the one they rely on:
//!
//! - stdout is JSON and nothing else: one document per line (a single
//!   document for most commands, NDJSON for `search`), no prose, no notes;
//! - human-facing notes (graph build announcements, caveats) go to stderr;
//! - a failing command exits non-zero, writes the reason to stderr, and
//!   leaves stdout empty, so a parser never sees a half answer.
//!
//! Each command runs against the in-process service (`PIXEL_DAEMON_AUTO_START=0`)
//! so the test does not depend on, or leave behind, a background daemon.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

fn pixel(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_pixel"))
        .args(args)
        .current_dir(dir)
        .env("PIXEL_DAEMON_AUTO_START", "0")
        .output()
        .unwrap()
}

fn fixture(tag: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("pixel-json-contract-{tag}-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
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

/// Every non-empty stdout line must parse as a JSON value on its own.
/// Returns the parsed documents so callers can assert on content.
fn parse_stdout_lines(out: &Output, what: &str) -> Vec<serde_json::Value> {
    let stdout = String::from_utf8(out.stdout.clone())
        .unwrap_or_else(|e| panic!("{what}: stdout is not UTF-8: {e}"));
    stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line).unwrap_or_else(|e| {
                panic!(
                    "{what}: stdout line is not JSON ({e}):\n{line}\n--- stderr:\n{}",
                    String::from_utf8_lossy(&out.stderr)
                )
            })
        })
        .collect()
}

#[test]
fn json_commands_emit_only_json_on_stdout() {
    let dir = fixture("ok");

    // (argv, expected top-level keys on the single document)
    let single_doc: &[(&[&str], &[&str])] = &[
        (&["status", ".", "--json"], &[]),
        (&["symbol", "login_user", ".", "--json"], &[]),
        (&["impact", "login_user", ".", "--json"], &["epistemics"]),
        (
            &["targets", "fix login_user", ".", "--json", "--no-manifest"],
            &["targets", "epistemics"],
        ),
        (&["inspect", ".", "--json"], &["head", "branch"]),
    ];
    for (argv, keys) in single_doc {
        let out = pixel(&dir, argv);
        let what = argv.join(" ");
        assert!(
            out.status.success(),
            "{what} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let docs = parse_stdout_lines(&out, &what);
        assert_eq!(
            docs.len(),
            1,
            "{what}: expected exactly one JSON document, got {docs:?}"
        );
        assert!(docs[0].is_object(), "{what}: top-level must be an object");
        for k in *keys {
            assert!(
                docs[0].get(k).is_some(),
                "{what}: missing key {k:?} in {}",
                docs[0]
            );
        }
    }

    // `search --json` is NDJSON: one match object per line, every line JSON.
    let out = pixel(&dir, &["search", "login_user", ".", "--json"]);
    assert!(
        out.status.success(),
        "search: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let docs = parse_stdout_lines(&out, "search --json");
    assert!(!docs.is_empty(), "search must find the fixture symbol");
    for d in &docs {
        assert!(
            d.get("path").is_some() || d.get("epistemics").is_some(),
            "unexpected line: {d}"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// A failure must be unambiguous to a parser: nothing on stdout, a reason
/// on stderr, non-zero exit. A `null` or partial document on stdout would
/// be read as an answer.
#[test]
fn failing_json_command_leaves_stdout_empty() {
    let dir = fixture("fail");
    let out = pixel(&dir, &["symbol", "no_such_symbol_anywhere", ".", "--json"]);
    // `symbol` on an unknown name may answer with an empty candidate set or
    // fail; either way stdout must be parseable and stderr must carry any
    // failure. Force a definite failure with a malformed regex on search.
    let _ = parse_stdout_lines(&out, "symbol unknown");

    let out = pixel(&dir, &["search", "(", ".", "--json"]);
    assert!(!out.status.success(), "malformed regex must fail");
    assert!(
        out.stdout.is_empty(),
        "stdout must be empty on failure, got: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.starts_with("pixel: "),
        "stderr must carry the reason: {stderr}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

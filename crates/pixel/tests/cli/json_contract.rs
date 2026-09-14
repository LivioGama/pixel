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

use std::path::Path;
use std::process::{Command, Output};

use crate::support::{Scratch, pixel_command};

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
    pixel_command()
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap()
}

fn fixture(tag: &str) -> Scratch {
    let dir = Scratch::for_test("pixel-json-contract", tag);
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
        (&["find-symbol", "login_user", ".", "--json"], &[]),
        (&["impact", "login_user", ".", "--json"], &["epistemics"]),
        (
            &[
                "scope-task",
                "fix login_user",
                ".",
                "--json",
                "--no-manifest",
            ],
            &["targets", "epistemics"],
        ),
        (&["repo-state", ".", "--json"], &["head", "branch"]),
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
    let out = pixel(&dir, &["search-content", "login_user", ".", "--json"]);
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
    let out = pixel(
        &dir,
        &["find-symbol", "no_such_symbol_anywhere", ".", "--json"],
    );
    // `symbol` on an unknown name may answer with an empty candidate set or
    // fail; either way stdout must be parseable and stderr must carry any
    // failure. Force a definite failure with a malformed regex on search.
    let _ = parse_stdout_lines(&out, "symbol unknown");

    let out = pixel(&dir, &["search-content", "(", ".", "--json"]);
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

/// A repo with a big untracked tree (a `vendor/bundle`) is the everyday
/// case that used to break the contract: `status`/`ready` embedded the full
/// dirty list, blew the 256 KB cap, and the whole answer degraded to a
/// `{partial: "..."}` wrapper — `jq .index` stopped working on a command
/// whose job is only to say "index and graph are ready". The freshness
/// answers must stay small (a count, never the list), and the commands that
/// legitimately return the list must be cut structurally: valid JSON, every
/// scalar field intact, the list shortened and the cut named.
#[test]
fn big_untracked_tree_keeps_json_answers_structured() {
    let dir = fixture("bigdirty");
    for i in 0..3000 {
        let d = dir.join(format!("vendor/bundle/gems/g{}", i / 100));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join(format!("f{i}.txt")), "x").unwrap();
    }

    for args in [
        &["status", ".", "--json"][..],
        &["prepare-repo", ".", "--json", "--no-daemon"][..],
    ] {
        let out = pixel(&dir, args);
        assert!(out.status.success(), "{args:?}: {out:?}");
        let docs = parse_stdout_lines(&out, &format!("{args:?}"));
        assert_eq!(docs.len(), 1);
        let doc = &docs[0];
        assert!(
            out.stdout.len() < 4096,
            "{args:?}: freshness answer must not scale with the dirty tree ({} bytes)",
            out.stdout.len()
        );
        assert!(doc.get("truncated").is_none(), "{args:?}: {doc}");
        assert!(doc["index"].is_object(), "{args:?}: {doc}");
        let dirty_count = if args[0] == "status" {
            assert!(doc["snapshot"].get("dirty").is_none(), "{args:?}: {doc}");
            doc["snapshot"]["dirty_count"].as_u64()
        } else {
            assert!(
                doc.get("status").is_none(),
                "prepare-repo must not embed status"
            );
            doc["dirty_count"].as_u64()
        };
        assert_eq!(dirty_count, Some(3000), "{args:?}: {doc}");
    }

    // Graph/retrieval answers are computed AGAINST a tree state, they do
    // not report it: the snapshot names HEAD/branch and counts the dirty
    // paths. Before this, `symbol`/`resolve` on a CI checkout with an
    // untracked `vendor/bundle` weighed 238 KB each, all of it path list.
    for args in [
        &["find-symbol", "login_user", ".", "--json"][..],
        &["find-code", "login user", ".", "--json"][..],
        &["impact", "login_user", ".", "--json"][..],
        &[
            "who-calls",
            "login_user",
            ".",
            "--role",
            "callers",
            "--json",
        ][..],
        &["what-changed", ".", "--json"][..],
    ] {
        let out = pixel(&dir, args);
        assert!(out.status.success(), "{args:?}: {out:?}");
        let docs = parse_stdout_lines(&out, &format!("{args:?}"));
        assert_eq!(docs.len(), 1, "{args:?}");
        let doc = &docs[0];
        assert!(
            out.stdout.len() < 4096,
            "{args:?}: graph answer must not scale with the dirty tree ({} bytes)",
            out.stdout.len()
        );
        // `truncated` here would be the output-cap wrapper (`uses` carries
        // its own pagination `truncated: false`, which is fine).
        assert_ne!(doc["truncated"], true, "{args:?}: {doc}");
        assert!(doc.get("cap_bytes").is_none(), "{args:?}: {doc}");
        assert!(
            doc["snapshot"].get("dirty").is_none(),
            "{args:?}: snapshot must not enumerate dirty paths: {doc}"
        );
        assert_eq!(
            doc["snapshot"]["dirty_count"].as_u64(),
            Some(3000),
            "{args:?}: {doc}"
        );
        assert!(doc["snapshot"]["head"].is_string(), "{args:?}: {doc}");
    }

    // `inspect` owns the list: under a small cap it is shortened, not
    // replaced by a textual wrapper.
    let out = pixel_command()
        .args(["repo-state", ".", "--json"])
        .current_dir(&dir)
        .env("PIXEL_OUTPUT_CAP_BYTES", "4096")
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert!(out.stdout.len() <= 4097, "{}", out.stdout.len());
    let doc = &parse_stdout_lines(&out, "inspect capped")[0];
    assert_eq!(doc["truncated"], true);
    assert_eq!(doc["cap_bytes"], 4096);
    assert!(
        doc.get("partial").is_none(),
        "structural cut, not wrapper: {doc}"
    );
    assert_eq!(doc["branch"].as_str().map(str::is_empty), Some(false));
    let cut = &doc["truncated_arrays"][0];
    assert_eq!(cut["path"], "dirty");
    assert_eq!(cut["total"], 3000);
    assert_eq!(
        cut["kept"].as_u64(),
        doc["dirty"].as_array().map(|a| a.len() as u64)
    );
    assert!(cut["kept"].as_u64().unwrap() > 0);

    // `0` lifts the cap: the full list comes back and nothing is flagged.
    let out = pixel_command()
        .args(["repo-state", ".", "--json"])
        .current_dir(&dir)
        .env("PIXEL_OUTPUT_CAP_BYTES", "0")
        .output()
        .unwrap();
    let doc = &parse_stdout_lines(&out, "inspect uncapped")[0];
    assert!(doc.get("truncated").is_none(), "{doc}");
    assert_eq!(doc["dirty"].as_array().map(Vec::len), Some(3000));

    std::fs::remove_dir_all(&dir).ok();
}

/// The statusline shows `indexed/total` commits only when the history index
/// knows about commits: a repository without any must not print `0/0`.
#[test]
fn statusline_reports_the_commit_fraction_only_when_there_are_commits() {
    let dir = fixture("statusline-commits");
    let indexed = pixel(&dir, &["build-index", "--history", "."]);
    assert!(indexed.status.success(), "{indexed:?}");
    let out = pixel(&dir, &["status", ".", "--statusline"]);
    assert!(out.status.success(), "{out:?}");
    let line = String::from_utf8_lossy(&out.stdout);
    assert!(line.contains(" 1/1"), "one commit indexed of one: {line}");

    let empty = Scratch::for_test("pixel-json-contract", "statusline-empty");
    std::fs::write(empty.join(".gitignore"), ".pixel/\n").unwrap();
    git(&empty, &["init", "-q"]);
    let _ = pixel(&empty, &["build-index", "--history", "."]);
    let out = pixel(&empty, &["status", ".", "--statusline"]);
    assert!(out.status.success(), "{out:?}");
    let line = String::from_utf8_lossy(&out.stdout);
    assert!(!line.contains("0/0"), "{line}");
}

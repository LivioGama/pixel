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

/// The standard fixture plus a file with enough matches that `--context 20`
/// makes the enriched page outgrow a small `PIXEL_OUTPUT_CAP_BYTES` long
/// before the daemon's own 64 KiB byte cap.
fn fixture_with_many_matches(tag: &str) -> Scratch {
    let dir = fixture(tag);
    let terms: Vec<String> = (1..=200)
        .map(|n| format!("// the line {n} mentions the search term"))
        .collect();
    std::fs::write(dir.join("src/terms.rs"), format!("{}\n", terms.join("\n"))).unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "terms"]);
    dir
}

/// `pixel search-content the . --limit <limit> --json` against `dir` under a
/// stdout cap, optionally with `--context <context>`.
fn capped_search(dir: &Path, cap: usize, limit: usize, context: Option<usize>) -> Output {
    let cap = cap.to_string();
    let limit = limit.to_string();
    let mut cmd = pixel_command();
    cmd.args([
        "search-content",
        "the",
        ".",
        "--limit",
        limit.as_str(),
        "--json",
    ])
    .current_dir(dir)
    .env("PIXEL_OUTPUT_CAP_BYTES", cap.as_str());
    if let Some(context) = context {
        cmd.args(["--context", &context.to_string()]);
    }
    cmd.output().unwrap()
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

    // `search --json` is NDJSON: one match object per line, then the final
    // page-metadata line, which is the only one without a `path`.
    let out = pixel(&dir, &["search-content", "login_user", ".", "--json"]);
    assert!(
        out.status.success(),
        "search: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let docs = parse_stdout_lines(&out, "search --json");
    assert!(!docs.is_empty(), "search must find the fixture symbol");
    let (meta, matches) = docs.split_last().unwrap();
    assert!(
        meta.get("truncated").is_some(),
        "the last line is the page metadata: {meta}"
    );
    assert!(meta.get("path").is_none(), "{meta}");
    for d in matches {
        assert!(d.get("path").is_some(), "unexpected line: {d}");
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

/// A capped `search --json` page must describe itself: the last NDJSON line
/// carries the page state (`truncated`, `next_offset`) and the envelope's
/// `epistemics`/`warnings`, because a page cut short is otherwise
/// byte-for-byte a complete answer. The cap counts the `--context` text the
/// CLI adds *after* the daemon's own byte cap — the 154 KB page measured
/// under an 8 KB cap.
#[test]
fn search_json_page_ends_with_the_state_of_the_page_it_printed() {
    let dir = fixture_with_many_matches("search-page-meta");
    let cap = 8192;

    // `--context 20` makes each match ~2 KB, so an 8 KB cap cuts the page long
    // before the daemon's 64 KB byte cap: only the trailer can say so.
    let out = capped_search(&dir, cap, 300, Some(20));
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stdout.len() <= cap + 1,
        "stdout must respect PIXEL_OUTPUT_CAP_BYTES ({cap}): {} bytes",
        out.stdout.len()
    );
    let docs = parse_stdout_lines(&out, "capped search --json --context 20");
    let (meta, matches) = docs.split_last().expect("the metadata line at least");
    assert!(
        meta.get("path").is_none(),
        "the last line is metadata: {meta}"
    );
    assert!(!matches.is_empty(), "the cap leaves room for matches");
    assert_eq!(meta["truncated"], true, "{meta}");
    assert_eq!(
        meta["next_offset"].as_u64(),
        Some(matches.len() as u64),
        "the page resumes at the first match it did not print: {meta}"
    );
    assert!(meta["epistemics"].is_object(), "{meta}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("stdout cap (PIXEL_OUTPUT_CAP_BYTES)"),
        "the cut is named on stderr: {stderr}"
    );

    // Without `--context` the rows are small enough to fill the cap tightly:
    // the reserved metadata line stays inside the cap too.
    let tight = capped_search(&dir, cap, 300, None);
    assert!(tight.status.success(), "{tight:?}");
    assert!(
        tight.stdout.len() <= cap + 1,
        "the reserved trailer must stay inside the cap: {} bytes",
        tight.stdout.len()
    );
    let docs = parse_stdout_lines(&tight, "capped search --json");
    let (meta, matches) = docs.split_last().unwrap();
    assert_eq!(meta["truncated"], true, "{meta}");
    assert_eq!(
        meta["next_offset"].as_u64(),
        Some(matches.len() as u64),
        "{meta}"
    );

    // The same page without a cap is complete, and says so: the control that
    // the daemon was not the one truncating the pages above.
    let full = capped_search(&dir, 0, 300, Some(20));
    assert!(full.status.success(), "{full:?}");
    assert!(full.stdout.len() > cap, "the cap really cut something");
    let docs = parse_stdout_lines(&full, "uncapped search --json --context 20");
    let (meta, matches) = docs.split_last().unwrap();
    assert_eq!(matches.len(), 200, "every match is in the page");
    assert_eq!(meta["truncated"], false, "{meta}");
    assert!(meta["next_offset"].is_null(), "{meta}");

    // A page the daemon itself capped keeps the daemon's resume offset, and
    // the stderr line names the daemon's row cap rather than the stdout cap.
    let limited = capped_search(&dir, cap, 5, None);
    assert!(limited.status.success(), "{limited:?}");
    let docs = parse_stdout_lines(&limited, "row-capped search --json");
    let (meta, matches) = docs.split_last().unwrap();
    assert_eq!(matches.len(), 5, "{docs:?}");
    assert_eq!(meta["truncated"], true, "{meta}");
    assert_eq!(meta["next_offset"].as_u64(), Some(5), "{meta}");
    let stderr = String::from_utf8_lossy(&limited.stderr);
    assert!(
        stderr.contains("row limit 5"),
        "the daemon's cap is named on stderr: {stderr}"
    );
    assert!(
        !stderr.contains("stdout cap"),
        "the stdout cap did not fire: {stderr}"
    );

    std::fs::remove_dir_all(&dir).ok();
}

/// Human `search` output keeps its rendering and its stderr warning: the
/// stdout cap cuts between rows, and the note names both how many rows were
/// written and the offset that resumes the page.
#[test]
fn capped_search_human_output_names_the_stdout_cap() {
    let dir = fixture_with_many_matches("search-page-human");
    let cap = 8192usize;
    let cap_arg = cap.to_string();
    let out = pixel_command()
        .args([
            "search-content",
            "the",
            ".",
            "--limit",
            "300",
            "--context",
            "20",
        ])
        .current_dir(&dir)
        .env("PIXEL_OUTPUT_CAP_BYTES", cap_arg.as_str())
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert!(
        out.stdout.len() <= cap + 1,
        "human output respects the cap too: {} bytes",
        out.stdout.len()
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.starts_with("--- "),
        "the context block rendering is unchanged: {}",
        text.chars().take(80).collect::<String>()
    );
    assert!(!text.contains("\"path\":"), "human mode stays human");
    let blocks = text.matches("--- ").count();
    assert!(blocks > 0, "some matches fit under the cap: {text}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(&format!("wrote {blocks} of ")),
        "the note names how many rows were written ({blocks}): {stderr}"
    );
    assert!(
        stderr.contains("stdout cap (PIXEL_OUTPUT_CAP_BYTES)"),
        "and which cap cut the page: {stderr}"
    );

    std::fs::remove_dir_all(&dir).ok();
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

/// The session-start capability block carries the history index's phase
/// and freshness (`repo.facts_phase`, `repo.facts_fresh`), read through
/// the CLI's own facts probe: an agent starting a session sees whether
/// `dig-history`/`plan-rollback` have a fresh index without a second command.
#[test]
fn session_start_block_reports_the_history_index_phase_and_freshness() {
    let dir = fixture("session-start-facts");
    let indexed = pixel(&dir, &["build-index", "--history", "."]);
    assert!(indexed.status.success(), "{indexed:?}");
    let out = pixel(&dir, &["run-hook", "session-start", "."]);
    assert!(out.status.success(), "{out:?}");
    let block: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    // The agent types what the block advertises: current command names,
    // never a wire op tag that names another command (`update`, `sync`).
    let capabilities: Vec<&str> = block["pixel"]["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap())
        .collect();
    for command in ["scope-task", "fast-forward", "fetch", "plan", "impact"] {
        assert!(
            capabilities.contains(&command),
            "{command}: {capabilities:?}"
        );
    }
    for (old, _) in pixel_proto::commands::RENAMED_COMMANDS {
        assert!(!capabilities.contains(old), "{old}: {capabilities:?}");
    }
    assert!(
        !capabilities.contains(&"migrate"),
        "hidden commands stay hidden"
    );
    let repo = &block["pixel"]["repo"];
    assert!(
        repo["facts_phase"].is_string(),
        "phase is the ingest phase name: {block}"
    );
    assert_eq!(
        repo["facts_fresh"], true,
        "one commit, fully ingested: {block}"
    );
}

/// `dig-history --show` is the follow-up every `dig-history` answer names:
/// it prints the file at the commit, and without `--file` it refuses under
/// the command's current name, so the agent can correct the call it typed.
#[test]
fn dig_history_show_prints_the_file_and_requires_file() {
    let dir = fixture("dig-history-show");
    let shown = pixel(
        &dir,
        &[
            "dig-history",
            "--show",
            "HEAD",
            "--file",
            "src/login.rs",
            "--json",
            ".",
        ],
    );
    assert!(shown.status.success(), "{shown:?}");
    let doc: serde_json::Value = serde_json::from_slice(&shown.stdout).unwrap();
    assert_eq!(
        doc["content"],
        "pub fn login_user(name: &str) -> bool {\n    !name.is_empty()\n}\n"
    );
    assert_eq!(doc["parent_fallback"], false);

    let missing = pixel(&dir, &["dig-history", "--show", "HEAD", "."]);
    assert!(!missing.status.success(), "{missing:?}");
    assert!(missing.stdout.is_empty(), "{missing:?}");
    let stderr = String::from_utf8_lossy(&missing.stderr);
    assert!(
        stderr.contains("dig-history --show requires --file"),
        "{stderr}"
    );
}

/// `pixel plan` renders the daemon's findings: JSON carries each finding
/// and the verify flag, compact prints `file:line label [SEVERITY]`, and an
/// unknown query fails instead of printing an empty plan.
#[test]
fn plan_renders_daemon_findings_as_json_and_compact() {
    let dir = fixture("plan");
    let out = pixel(&dir, &["plan", "--query", "dead-code", "--json", "."]);
    assert!(out.status.success(), "{out:?}");
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["verify"], true, "{doc}");
    let findings = doc["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 1, "{doc}");
    assert_eq!(findings[0]["file"], "src/caller.rs");
    assert_eq!(findings[0]["severity"], "low");
    assert!(
        findings[0]["label"]
            .as_str()
            .unwrap()
            .starts_with("No callers found for function `go`"),
        "{doc}"
    );

    let compact = pixel(
        &dir,
        &[
            "plan",
            "--query",
            "dead-code",
            "--format",
            "compact",
            "--no-verify",
            ".",
        ],
    );
    assert!(compact.status.success(), "{compact:?}");
    let text = String::from_utf8(compact.stdout).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 1, "{text}");
    assert!(
        lines[0].starts_with("src/caller.rs:2 No callers found"),
        "{text}"
    );
    assert!(lines[0].ends_with("[LOW]"), "{text}");

    let unknown = pixel(&dir, &["plan", "--query", "everything", "."]);
    assert!(!unknown.status.success(), "{unknown:?}");
    assert!(unknown.stdout.is_empty(), "{unknown:?}");
    assert!(
        String::from_utf8_lossy(&unknown.stderr).contains("unknown query 'everything'"),
        "{unknown:?}"
    );
}

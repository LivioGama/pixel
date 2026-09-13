//! `pixel recall` end to end on a scratch corpus: one Claude transcript
//! under a throwaway `HOME`, indexed by the binary, then read back through
//! `search`, `show`, `status` and `context`. These are the commands the
//! agent prompt's recall method documents; each assertion is on the stdout
//! contract an agent parses, so a command that silently prints nothing
//! (or the wrong session) fails here.

use std::path::Path;
use std::process::Command;

use serde_json::Value;

use crate::support::{Scratch, pixel_command};

const SESSION_ID: &str = "0123abcd-0000-4000-8000-000000000001";
/// The human turn: the needle, then enough text that its full-turn block
/// cannot fit a 100-token context budget while the header line can.
const NEEDLE: &str = "please fix the streamed needle";
const TAIL: &str = " it fails on the second retry and the log shows nothing useful";

/// A scratch HOME and recall dir holding one two-turn Claude session: a
/// human prompt with the needle and a harness-injected reminder turn.
struct Corpus {
    home: Scratch,
}

impl Corpus {
    fn new(tag: &str) -> Self {
        let home = Scratch::for_test("recall-cli", tag);
        let slug = home.join(".claude/projects/-work-pixel");
        std::fs::create_dir_all(&slug).unwrap();
        let lines = [
            serde_json::json!({
                "type": "user", "cwd": "/work/pixel", "gitBranch": "develop",
                "timestamp": "2025-10-09T08:53:20.000Z",
                "message": {"content": [{"type": "text", "text": format!("{NEEDLE}{}", TAIL.repeat(8))}]},
            })
            .to_string(),
            serde_json::json!({
                "type": "user", "cwd": "/work/pixel",
                "timestamp": "2025-10-09T08:54:20.000Z",
                "message": {"content": [{"type": "text", "text": "<system-reminder>injected context</system-reminder>"}]},
            })
            .to_string(),
        ];
        std::fs::write(
            slug.join(format!("{SESSION_ID}.jsonl")),
            format!("{}\n", lines.join("\n")),
        )
        .unwrap();
        let corpus = Self { home };
        let out = corpus.run(&["recall", "index", "--source", "claude"]);
        assert!(out.status.success(), "index: {out:?}");
        corpus
    }

    fn command(&self) -> Command {
        let mut cmd = pixel_command();
        cmd.env("HOME", self.home.as_ref() as &Path)
            .env("PIXEL_RECALL_DIR", self.home.join("recall"))
            .env_remove("PIXEL_RECALL_MODEL")
            .current_dir(self.home.as_ref() as &Path);
        cmd
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        self.command().args(args).output().unwrap()
    }

    fn stdout(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(out.status.success(), "pixel {args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap()
    }

    fn json(&self, args: &[&str]) -> Value {
        serde_json::from_str(&self.stdout(args)).unwrap_or_else(|e| panic!("{args:?}: {e}"))
    }
}

#[test]
fn search_returns_the_matching_turn_with_its_session_reference() {
    let corpus = Corpus::new("search");
    let out = corpus.json(&["recall", "search", "streamed needle", "--json"]);
    let hits = out["hits"].as_array().expect("hits array");
    assert_eq!(hits.len(), 1, "{out}");
    assert_eq!(hits[0]["agent"], "claude");
    assert_eq!(hits[0]["source_session_id"], SESSION_ID);
    assert_eq!(hits[0]["role"], "user");
    assert_eq!(hits[0]["cwd"], "/work/pixel");
    assert!(
        hits[0]["snippet"]
            .as_str()
            .unwrap()
            .contains("streamed needle"),
        "{out}"
    );
    assert_eq!(out["truncated"], false);
    assert_eq!(out["turns_considered"].as_u64().map(|n| n >= 1), Some(true));

    let text = corpus.stdout(&["recall", "search", "streamed needle"]);
    assert!(
        text.starts_with("claude:0123abcd #"),
        "text line names the session ref: {text}"
    );
    assert!(
        text.contains("user \"please fix the streamed needle"),
        "{text}"
    );

    let miss = corpus.stdout(&["recall", "search", "no-such-token-xyzzy"]);
    assert!(miss.starts_with("no matches ("), "{miss}");
}

#[test]
fn search_role_filter_is_validated_and_applied() {
    let corpus = Corpus::new("search-role");
    let bad = corpus.run(&["recall", "search", "needle", "--role", "bogus"]);
    assert!(!bad.status.success());
    assert!(
        String::from_utf8_lossy(&bad.stderr).contains("--role must be user, assistant, or tool"),
        "{bad:?}"
    );
    let user = corpus.json(&["recall", "search", "needle", "--role", "user", "--json"]);
    assert_eq!(user["hits"].as_array().map(Vec::len), Some(1), "{user}");
    let tool = corpus.json(&["recall", "search", "needle", "--role", "tool", "--json"]);
    assert_eq!(tool["hits"].as_array().map(Vec::len), Some(0), "{tool}");
}

#[test]
fn show_prints_the_turns_and_marks_harness_injected_ones() {
    let corpus = Corpus::new("show");
    let out = corpus.json(&["recall", "show", "claude:0123abcd", "--json"]);
    assert_eq!(out["session"]["source_session_id"], SESSION_ID);
    let turns = out["turns"].as_array().expect("turns");
    assert_eq!(turns.len(), 2, "{out}");
    assert_eq!(turns[0]["seq"], 0);
    assert!(
        turns[0]["text"].as_str().unwrap().starts_with(NEEDLE),
        "{out}"
    );
    assert_eq!(turns[0]["intent_source"], "human");
    assert_eq!(turns[1]["intent_source"], "orchestrator");
    assert_eq!(turns[1]["truncated"], false);

    let text = corpus.stdout(&["recall", "show", "claude:0123abcd"]);
    let mut lines = text.lines();
    assert!(
        lines.next().unwrap().starts_with("claude:0123abcd #"),
        "{text}"
    );
    assert_eq!(lines.next(), Some("branch: develop"));
    assert!(
        text.contains("--- #0 user 2025-10-09 08:53 ---\nplease fix the streamed needle"),
        "{text}"
    );
    assert!(
        text.contains("--- #1 user 2025-10-09 08:54 (orchestrator) ---\n"),
        "the injected turn is labelled: {text}"
    );

    let ranged = corpus.stdout(&["recall", "show", "claude:0123abcd", "--turn", "1..1"]);
    assert!(ranged.contains("--- #1 user"), "{ranged}");
    assert!(!ranged.contains("--- #0 user"), "{ranged}");
    let empty = corpus.stdout(&["recall", "show", "claude:0123abcd", "--turn", "5..9"]);
    assert!(empty.trim_end().ends_with("(no turns in range)"), "{empty}");
}

#[test]
fn status_reports_the_corpus_counts_and_location() {
    let corpus = Corpus::new("status");
    let out = corpus.json(&["recall", "status", "--json"]);
    assert_eq!(out["total_turns"], 2, "{out}");
    assert_eq!(
        out["unsegmented_turns"], 0,
        "index segments what it ingests"
    );
    assert_eq!(out["lexical_segments"], 1);
    assert_eq!(
        out["location"],
        Value::String(corpus.home.join("recall").display().to_string())
    );
    let agents = out["agents"].as_array().expect("agents");
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0]["agent"], "claude");
    assert_eq!(agents[0]["sessions"], 1);
    assert_eq!(agents[0]["turns"], 2);

    let text = corpus.stdout(&["recall", "status"]);
    assert!(text.starts_with("corpus: "), "{text}");
    assert!(text.contains("claude"), "{text}");
}

#[test]
fn context_packs_the_matching_session_within_the_budget() {
    let corpus = Corpus::new("context");
    let text = corpus.stdout(&[
        "recall",
        "context",
        "streamed needle",
        "--lexical-only",
        "--budget",
        "500",
    ]);
    assert!(
        text.starts_with("recall context for: streamed needle\n"),
        "{text}"
    );
    assert!(text.contains("- [claude:0123abcd #"), "L0 header: {text}");
    assert!(
        text.contains(&format!("--- session #1 turn 0 (user) ---\n{NEEDLE}")),
        "L2 full turn: {text}"
    );
    assert!(text.contains("\nfitted: budget=500 used="), "{text}");
    assert!(text.trim_end().ends_with("dropped_blocks=0"), "{text}");

    // Every filter narrows the pack: the wrong agent, another repo or a
    // window after the session each leave no session group.
    for extra in [
        ["--agent", "codex"],
        ["--repo", "/elsewhere"],
        ["--since", "1h"],
        ["--until", "2020-01-01"],
    ] {
        let mut args = vec!["recall", "context", "streamed needle", "--lexical-only"];
        args.extend(extra);
        let filtered = corpus.stdout(&args);
        assert!(
            !filtered.contains("- [claude:"),
            "{extra:?} must exclude the session: {filtered}"
        );
    }

    // A budget that fits the header line but not the full turn drops the
    // turn and says so; `used` stays within the budget.
    let tight = corpus.stdout(&[
        "recall",
        "context",
        "streamed needle",
        "--lexical-only",
        "--budget",
        "100",
    ]);
    assert!(
        tight.contains("- [claude:0123abcd #"),
        "header kept: {tight}"
    );
    assert!(
        !tight.contains("--- session #"),
        "the full turn block is dropped: {tight}"
    );
    let footer = tight.lines().last().unwrap();
    assert!(footer.ends_with("dropped_blocks=1"), "{footer}");
    let used: usize = footer
        .split("used=")
        .nth(1)
        .and_then(|rest| rest.split(' ').next())
        .and_then(|n| n.parse().ok())
        .unwrap();
    assert!(used <= 100, "{footer}");

    let err = corpus.run(&["recall", "context", "x", "--budget", "10"]);
    assert!(!err.status.success());
    assert!(
        String::from_utf8_lossy(&err.stderr).contains("--budget must be between"),
        "{err:?}"
    );
}

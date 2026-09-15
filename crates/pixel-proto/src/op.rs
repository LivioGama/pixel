//! `Op`: a type-level mirror of `pixel_daemon::api::Request`
//! (`crates/pixel-daemon/src/api.rs`), reproduced here so the shared
//! contract crate carries the wire-format definition rather than the daemon
//! crate.
//!
//! This is **not yet wired into `pixel-daemon`** — `Request` there remains
//! the live type the daemon dispatches on. Swapping the daemon over to this
//! `Op` (and re-deriving CLI args / MCP tool schemas from it, per `PLAN.md`
//! A2) is a separate future step. Until then, this enum's only job is to
//! exist, compile, and round-trip identically to `Request`'s current wire
//! format so it is ready to be swapped in without a contract change.
//!
//! Variants, field shapes, and the `#[serde(tag = "op", rename_all =
//! "snake_case")]` wire convention are copied verbatim from `Request`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    Ping,
    /// Transcript-corpus operation, served only by a recall daemon (a repo
    /// daemon answers it with an "unsupported" error). `action` selects the
    /// recall op ("search-content" | "search-meaning"); `params` is its argument object.
    Recall {
        action: String,
        #[serde(default)]
        params: Value,
    },
    #[serde(rename = "search-content")]
    SearchContent {
        pattern: String,
        #[serde(default)]
        json: bool,
        #[serde(default)]
        limit: Option<usize>,
        #[serde(default)]
        offset: Option<usize>,
        /// Repo-relative path prefixes to restrict the search to (rg-style
        /// multi-path invocations). None/empty = whole repo.
        #[serde(default)]
        paths: Option<Vec<String>>,
        /// `"code"` enables ranked output: matches are reranked by file-level
        /// signals (filename match, symbol-name match, content density) via
        /// pixel-rank's RRF, without changing the hit set. Default (None or
        /// any other value) preserves the existing path/line order.
        #[serde(default)]
        scope: Option<String>,
    },
    /// Sniper target list: task description in, closed prioritized file
    /// list (P0/P1/P2) out.
    #[serde(rename = "scope-task")]
    ScopeTask {
        task: String,
        #[serde(default)]
        limit: Option<usize>,
        #[serde(default)]
        max_tier: Option<String>,
        #[serde(default)]
        precision: bool,
    },
    #[serde(rename = "find-symbol")]
    FindSymbol {
        name: String,
    },
    /// All signatures in a file — the "list-signatures" view at ~10% of Read cost.
    #[serde(rename = "list-signatures")]
    ListSignatures {
        file: String,
    },
    #[serde(rename = "pack-context")]
    PackContext {
        uid: String,
        #[serde(default)]
        budget_tokens: Option<usize>,
    },
    Impact {
        uid_or_name: String,
        direction: String,
        #[serde(default)]
        depth: Option<u32>,
    },
    #[serde(rename = "who-calls")]
    WhoCalls {
        uid_or_name: String,
        /// "callers" | "callees"
        role: String,
        #[serde(default)]
        offset: Option<usize>,
    },
    #[serde(rename = "call-path")]
    CallPath {
        from: String,
        to: String,
    },
    #[serde(rename = "list-flows")]
    ListFlows {
        #[serde(default)]
        offset: Option<usize>,
    },
    #[serde(rename = "list-areas")]
    ListAreas {
        #[serde(default)]
        offset: Option<usize>,
    },
    #[serde(rename = "what-changed")]
    WhatChanged {
        #[serde(default)]
        base: Option<String>,
        #[serde(default)]
        offset: Option<usize>,
        /// Map affected symbols to the test files that exercise them
        /// (upstream caller walk). Default false; serde default keeps
        /// existing wire calls unaffected.
        #[serde(default)]
        include_tests: bool,
    },
    #[serde(rename = "rebuild-graph")]
    RebuildGraph {},
    Status {},
    /// Force a rebuild of the text index shard. Returns BuildStats.
    /// When sent to the daemon, the daemon's already-open Service does
    /// the rebuild (singleton — no concurrent build races).
    Reindex {},
    /// Engine 1: concept-index resolution. `resolve "<phrase>"` returns a
    /// cascade-ranked match list (T0 exact-unique → T1 kind-directed → T2
    /// word intersection → T3 trigram), each tier short-circuiting, with
    /// explicit confidence.
    #[serde(rename = "find-code")]
    FindCode {
        phrase: String,
        #[serde(default)]
        limit: Option<usize>,
    },
    /// M3 / Engine 2: history-wide fact + diff search. `scope` selects
    /// "message" | "path" | "diff" | "all" (default "all").
    #[serde(rename = "search-history")]
    SearchHistory {
        query: String,
        #[serde(default)]
        facet: Option<String>,
        #[serde(default)]
        limit: Option<usize>,
    },
    /// Engine 2: lifecycle of a path or token — first-seen, last-changed,
    /// removed-in, present-at-HEAD.
    #[serde(rename = "file-history")]
    FileHistory {
        #[serde(default)]
        path: Option<String>,
        #[serde(default)]
        token: Option<String>,
    },
    /// Engine 2: history-wide discovery ("dig-history"). `phrase` may be empty
    /// to list the ingest checkpoint/state only.
    #[serde(rename = "dig-history")]
    DigHistory {
        #[serde(default)]
        phrase: Option<String>,
        #[serde(default)]
        path: Option<String>,
        #[serde(default)]
        from: Option<String>,
        #[serde(default)]
        to: Option<String>,
        #[serde(default)]
        limit: Option<usize>,
    },
    /// Engine 4: one-call deterministic branch sync. `strategy` is
    /// "report" (default) or "rebase-if-clean" (explicit opt-in).
    #[serde(rename = "sync-branch")]
    SyncBranch {
        #[serde(default)]
        strategy: Option<String>,
        #[serde(default)]
        push: Option<String>,
        /// Integration target: rebase current branch onto origin/<target>,
        /// then fast-forward the local <target> branch (never merge).
        #[serde(default)]
        into: Option<String>,
        /// Idempotency / recovery key threaded to the ops journal.
        #[serde(default)]
        request_id: Option<String>,
    },
    /// M5: journal a session event into the session db (fire-and-forget).
    #[serde(rename = "record-event")]
    RecordEvent {
        kind: String,
        #[serde(default)]
        path: Option<String>,
        #[serde(default)]
        detail: Option<String>,
    },
    // -- M2: git ops -----------------------------------------------------
    /// Repo state snapshot: HEAD, branch, dirty files, fingerprints.
    #[serde(rename = "repo-state")]
    RepoState {
        #[serde(default)]
        files: Option<Vec<String>>,
    },
    /// Show working-tree changes as structured items (staged, unstaged,
    /// untracked, conflicted).
    #[serde(rename = "review-changes")]
    ReviewChanges {
        #[serde(default)]
        cursor: Option<String>,
        #[serde(default)]
        byte_cap: Option<usize>,
    },
    /// Structured diff between two refs or working tree.
    Diff {
        from: String,
        #[serde(default)]
        to: Option<String>,
        #[serde(default)]
        paths: Option<Vec<String>>,
        #[serde(default)]
        byte_cap: Option<usize>,
    },
    /// Commit history (git log) with detail levels and byte caps. Named
    /// `HistoryOp` to avoid clashing with the M3 `History` (history-wide
    /// fact + diff search) variant.
    #[serde(rename = "commit-history")]
    CommitHistory {
        #[serde(default)]
        ref_name: Option<String>,
        #[serde(default)]
        limit: Option<usize>,
        #[serde(default)]
        detail: Option<String>,
        #[serde(default)]
        cursor: Option<String>,
        #[serde(default)]
        byte_cap: Option<usize>,
    },
    /// Stage files, commit, and optionally push. Crash-safe via journal.
    #[serde(rename = "commit")]
    Commit {
        message: String,
        files: Vec<String>,
        #[serde(default)]
        expected_head: Option<String>,
        #[serde(default)]
        push: Option<bool>,
        #[serde(default)]
        amend: Option<bool>,
        request_id: String,
    },
    /// Leased push with crash-safe journaling.
    Push {
        remote: String,
        refspec: String,
        #[serde(default)]
        force_with_lease: Option<bool>,
        request_id: String,
    },
    /// Publish + push in one op (convenience wrapper).
    #[serde(rename = "commit-and-push")]
    CommitAndPush {
        message: String,
        files: Vec<String>,
        remote: String,
        refspec: String,
        request_id: String,
    },
    /// Create a new branch from HEAD or a base ref. Named `BranchOp` to
    /// avoid clashing with any future `Branch` variant.
    #[serde(rename = "new-branch")]
    NewBranch {
        name: String,
        #[serde(default)]
        from: Option<String>,
        request_id: String,
    },
    /// Fast-forward merge with expectedHead + targetOid.
    #[serde(rename = "fast-forward")]
    FastForward {
        expected_head: String,
        target_oid: String,
        request_id: String,
    },
    /// Explicit-refspec fetch (idempotent).
    #[serde(rename = "fetch")]
    Fetch {
        remote: String,
        #[serde(default)]
        refspec: Option<String>,
    },
    /// P2·2: human notes — durable annotations keyed by `file` + `target`
    /// (a symbol `name` or concept `norm`) that survive rebuilds and are
    /// merged into `resolve`/`targets` results. `action` is one of
    /// "set" | "get" | "rm" | "list"; `set` requires `note`,
    /// `get`/`rm` require `file` + `target`, `list` takes an optional
    /// `file` filter.
    Note {
        action: String,
        #[serde(default)]
        file: Option<String>,
        #[serde(default)]
        target: Option<String>,
        #[serde(default)]
        note: Option<String>,
    },
    /// P2·2: structural repo map — every indexed file with its symbols.
    /// `markdown` emits the exportable document form (per-file headings +
    /// symbol bullets); otherwise a compact per-file outline.
    #[serde(rename = "repo-map")]
    RepoMap {
        #[serde(default)]
        markdown: bool,
    },
    /// Deterministic todo list: graph and git-history findings for either an
    /// explicit `query` (dead-interactive | dead-code | hotspots |
    /// recent-changes | by-concept) or the queries `prompt` classifies to.
    /// Served by the daemon so the graph it reads is the daemon's, kept
    /// fresh incrementally and never rebuilt under a concurrent reader.
    Plan {
        #[serde(default)]
        prompt: Option<String>,
        #[serde(default)]
        query: Option<String>,
        #[serde(default)]
        tag: Option<String>,
        #[serde(default)]
        limit: Option<usize>,
    },
    Shutdown,
}

impl Op {
    /// The wire tag name for this variant — the value serde emits under the
    /// `"op"` field (`"ping"`, `"search-content"`, `"scope-task"`, …). Used to populate
    /// the response envelope's `op` field so every response self-describes
    /// which op it answers.
    pub fn op_name(&self) -> &'static str {
        match self {
            Op::Ping => "ping",
            Op::Recall { .. } => "recall",
            Op::SearchContent { .. } => "search-content",
            Op::ScopeTask { .. } => "scope-task",
            Op::FindSymbol { .. } => "find-symbol",
            Op::ListSignatures { .. } => "list-signatures",
            Op::PackContext { .. } => "pack-context",
            Op::Impact { .. } => "impact",
            Op::WhoCalls { .. } => "who-calls",
            Op::CallPath { .. } => "call-path",
            Op::ListFlows { .. } => "list-flows",
            Op::ListAreas { .. } => "list-areas",
            Op::WhatChanged { .. } => "what-changed",
            Op::RebuildGraph {} => "rebuild-graph",
            Op::Status {} => "status",
            Op::FindCode { .. } => "find-code",
            Op::SearchHistory { .. } => "search-history",
            Op::FileHistory { .. } => "file-history",
            Op::DigHistory { .. } => "dig-history",
            Op::SyncBranch { .. } => "sync-branch",
            Op::RecordEvent { .. } => "record-event",
            Op::RepoState { .. } => "repo-state",
            Op::ReviewChanges { .. } => "review-changes",
            Op::Diff { .. } => "diff",
            Op::CommitHistory { .. } => "commit-history",
            Op::Commit { .. } => "commit",
            Op::Push { .. } => "push",
            Op::CommitAndPush { .. } => "commit-and-push",
            Op::NewBranch { .. } => "new-branch",
            Op::FastForward { .. } => "fast-forward",
            Op::Fetch { .. } => "fetch",
            Op::Note { .. } => "note",
            Op::RepoMap { .. } => "repo-map",
            Op::Plan { .. } => "plan",
            Op::Shutdown => "shutdown",
            Op::Reindex { .. } => "reindex",
        }
    }
}

/// The daemon ops a client may send — every real variant's [`Op::op_name`]
/// except `shutdown` (an internal admin op). These are wire tags, not CLI
/// commands: the SessionStart hook advertises the parser's subcommands
/// instead, because several tags name another command or none (`update` is
/// `fast-forward`, `sync` is `fetch`, `history_op` is `commit-history`). Kept in this file, beside the enum, so adding a
/// variant is a one-line addition here too; `session_capabilities_track_every_real_op`
/// below fails loudly if this list and the enum ever drift apart, which is
/// the specific failure this const exists to make structurally impossible
/// (a prior hand-maintained copy of this list, kept in the CLI crate with
/// no link back to `Op`, silently went stale and undermined the exact
/// anti-false-context guarantee the SessionStart hook is supposed to give).
pub const SESSION_CAPABILITIES: &[&str] = &[
    "ping",
    "recall",
    "search-content",
    "scope-task",
    "find-symbol",
    "list-signatures",
    "pack-context",
    "impact",
    "who-calls",
    "call-path",
    "list-flows",
    "list-areas",
    "what-changed",
    "rebuild-graph",
    "status",
    "find-code",
    "search-history",
    "file-history",
    "dig-history",
    "sync-branch",
    "record-event",
    "repo-state",
    "review-changes",
    "diff",
    "commit-history",
    "commit",
    "push",
    "commit-and-push",
    "new-branch",
    "fast-forward",
    "fetch",
    "note",
    "repo-map",
    "plan",
    "flow",
];

/// The one-paragraph usage doctrine the SessionStart hook injects into every
/// agent session. Lives beside `Op`/[`SESSION_CAPABILITIES`] so the doctrine
/// string and the op registry travel together and `pixel doctor`'s
/// scenario-consistency check can compare the installed rule text against
/// exactly what the binary injects.
///
/// Must name every mandatory scenario: scope-task (mandatory first call,
/// advisory fence — the guard warns on out-of-scope files rather than
/// silently allowing drift), find-code, plan-rollback/dig-history, sync-branch, and
/// impact/what-changed (blast radius before edits).
pub const SESSION_USAGE: &str = "pixel is the unified retrieval + git engine. Use `pixel <verb>` for search-content, find-code, scope-task, search-history, and safe git ops. Five mandatory scenarios: (1) `pixel scope-task \"<task>\"` — mandatory first call before the first file read (advisory fence: the guard warns on out-of-list files); (2) `pixel find-code \"<phrase>\"` before any free-text search; (3) `pixel plan-rollback`/`pixel dig-history` the moment code was working before; (4) `pixel sync-branch` for any branch sync; (5) `pixel impact <symbol>` before editing any symbol and `pixel what-changed` before any edit batch — measure the blast radius before edits.";

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ping_serializes_as_bare_tag() {
        let value = serde_json::to_value(Op::Ping).unwrap();
        assert_eq!(value, json!({"op": "ping"}));
    }

    #[test]
    fn search_serializes_with_snake_case_tag_and_fields() {
        let op = Op::SearchContent {
            pattern: "fn main".into(),
            json: true,
            limit: Some(50),
            offset: None,
            paths: Some(vec!["src".into()]),
            scope: None,
        };
        let value = serde_json::to_value(&op).unwrap();
        assert_eq!(
            value,
            json!({
                "op": "search-content",
                "pattern": "fn main",
                "json": true,
                "limit": 50,
                "offset": null,
                "paths": ["src"],
                "scope": null,
            })
        );
    }

    #[test]
    fn targets_omits_defaulted_limit_on_deserialize() {
        let op: Op =
            serde_json::from_value(json!({"op": "scope-task", "task": "fix the bug"})).unwrap();
        assert_eq!(
            op,
            Op::ScopeTask {
                task: "fix the bug".into(),
                limit: None,
                max_tier: None,
                precision: false,
            }
        );
    }

    #[test]
    fn impact_round_trips() {
        let op = Op::Impact {
            uid_or_name: "foo#1".into(),
            direction: "upstream".into(),
            depth: Some(3),
        };
        let text = serde_json::to_string(&op).unwrap();
        let parsed: Op = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, op);
    }

    #[test]
    fn graph_and_status_serialize_as_empty_object_variants() {
        assert_eq!(
            serde_json::to_value(Op::RebuildGraph {}).unwrap(),
            json!({"op": "rebuild-graph"})
        );
        assert_eq!(
            serde_json::to_value(Op::Status {}).unwrap(),
            json!({"op": "status"})
        );
    }

    #[test]
    fn shutdown_serializes_as_bare_tag() {
        let value = serde_json::to_value(Op::Shutdown).unwrap();
        assert_eq!(value, json!({"op": "shutdown"}));
    }

    #[test]
    fn resolve_round_trips() {
        let op = Op::FindCode {
            phrase: "the form".into(),
            limit: Some(5),
        };
        let value = serde_json::to_value(&op).unwrap();
        assert_eq!(value["op"], "find-code");
        assert_eq!(value["phrase"], "the form");
        assert_eq!(value["limit"], 5);
        let back: Op = serde_json::from_value(value).unwrap();
        assert_eq!(back, op);
    }

    #[test]
    fn reconcile_round_trips_with_defaults() {
        let op: Op = serde_json::from_value(json!({"op": "sync-branch"})).unwrap();
        assert_eq!(
            op,
            Op::SyncBranch {
                strategy: None,
                push: None,
                into: None,
                request_id: None
            }
        );
    }

    #[test]
    fn inspect_round_trips_with_defaults() {
        let op: Op = serde_json::from_value(json!({"op": "repo-state"})).unwrap();
        assert_eq!(op, Op::RepoState { files: None });
    }

    #[test]
    fn publish_round_trips() {
        let op = Op::Commit {
            message: "fix bug".into(),
            files: vec!["src/a.rs".into(), "src/b.rs".into()],
            expected_head: Some("abc123".into()),
            push: Some(true),
            amend: Some(false),
            request_id: "req-1".into(),
        };
        let value = serde_json::to_value(&op).unwrap();
        assert_eq!(value["op"], "commit");
        assert_eq!(value["message"], "fix bug");
        assert_eq!(value["files"], json!(["src/a.rs", "src/b.rs"]));
        assert_eq!(value["expected_head"], "abc123");
        assert_eq!(value["push"], true);
        assert_eq!(value["amend"], false);
        assert_eq!(value["request_id"], "req-1");
        let back: Op = serde_json::from_value(value).unwrap();
        assert_eq!(back, op);
    }

    #[test]
    fn update_round_trips() {
        let op = Op::FastForward {
            expected_head: "abc123".into(),
            target_oid: "def456".into(),
            request_id: "req-2".into(),
        };
        let value = serde_json::to_value(&op).unwrap();
        assert_eq!(value["op"], "fast-forward");
        assert_eq!(value["expected_head"], "abc123");
        assert_eq!(value["target_oid"], "def456");
        assert_eq!(value["request_id"], "req-2");
        let back: Op = serde_json::from_value(value).unwrap();
        assert_eq!(back, op);
    }

    #[test]
    fn op_name_matches_serde_tag() {
        // Every variant's op_name() must equal the "op" field serde emits,
        // so the response envelope's op field is always consistent with the
        // request that triggered it.
        let cases: &[(Op, &str)] = &[
            (Op::Ping, "ping"),
            (
                Op::Recall {
                    action: "x".into(),
                    params: json!(null),
                },
                "recall",
            ),
            (
                Op::SearchContent {
                    pattern: "".into(),
                    json: false,
                    limit: None,
                    offset: None,
                    paths: None,
                    scope: None,
                },
                "search-content",
            ),
            (
                Op::ScopeTask {
                    task: "".into(),
                    limit: None,
                    max_tier: None,
                    precision: false,
                },
                "scope-task",
            ),
            (Op::FindSymbol { name: "".into() }, "find-symbol"),
            (Op::ListSignatures { file: "".into() }, "list-signatures"),
            (
                Op::PackContext {
                    uid: "".into(),
                    budget_tokens: None,
                },
                "pack-context",
            ),
            (
                Op::Impact {
                    uid_or_name: "".into(),
                    direction: "".into(),
                    depth: None,
                },
                "impact",
            ),
            (
                Op::WhoCalls {
                    uid_or_name: "".into(),
                    role: "".into(),
                    offset: None,
                },
                "who-calls",
            ),
            (
                Op::CallPath {
                    from: "".into(),
                    to: "".into(),
                },
                "call-path",
            ),
            (Op::ListFlows { offset: None }, "list-flows"),
            (Op::ListAreas { offset: None }, "list-areas"),
            (
                Op::WhatChanged {
                    base: None,
                    offset: None,
                    include_tests: false,
                },
                "what-changed",
            ),
            (Op::RebuildGraph {}, "rebuild-graph"),
            (Op::Status {}, "status"),
            (
                Op::FindCode {
                    phrase: "".into(),
                    limit: None,
                },
                "find-code",
            ),
            (
                Op::SearchHistory {
                    query: "".into(),
                    facet: None,
                    limit: None,
                },
                "search-history",
            ),
            (
                Op::FileHistory {
                    path: None,
                    token: None,
                },
                "file-history",
            ),
            (
                Op::DigHistory {
                    phrase: None,
                    path: None,
                    from: None,
                    to: None,
                    limit: None,
                },
                "dig-history",
            ),
            (
                Op::SyncBranch {
                    strategy: None,
                    push: None,
                    into: None,
                    request_id: None,
                },
                "sync-branch",
            ),
            (
                Op::RecordEvent {
                    kind: "".into(),
                    path: None,
                    detail: None,
                },
                "record-event",
            ),
            (Op::RepoState { files: None }, "repo-state"),
            (
                Op::ReviewChanges {
                    cursor: None,
                    byte_cap: None,
                },
                "review-changes",
            ),
            (
                Op::Diff {
                    from: "".into(),
                    to: None,
                    paths: None,
                    byte_cap: None,
                },
                "diff",
            ),
            (
                Op::CommitHistory {
                    ref_name: None,
                    limit: None,
                    detail: None,
                    cursor: None,
                    byte_cap: None,
                },
                "commit-history",
            ),
            (
                Op::Commit {
                    message: "".into(),
                    files: vec![],
                    expected_head: None,
                    push: None,
                    amend: None,
                    request_id: "".into(),
                },
                "commit",
            ),
            (
                Op::Push {
                    remote: "".into(),
                    refspec: "".into(),
                    force_with_lease: None,
                    request_id: "".into(),
                },
                "push",
            ),
            (
                Op::CommitAndPush {
                    message: "".into(),
                    files: vec![],
                    remote: "".into(),
                    refspec: "".into(),
                    request_id: "".into(),
                },
                "commit-and-push",
            ),
            (
                Op::NewBranch {
                    name: "".into(),
                    from: None,
                    request_id: "".into(),
                },
                "new-branch",
            ),
            (
                Op::FastForward {
                    expected_head: "".into(),
                    target_oid: "".into(),
                    request_id: "".into(),
                },
                "fast-forward",
            ),
            (
                Op::Fetch {
                    remote: "".into(),
                    refspec: None,
                },
                "fetch",
            ),
            (
                Op::Plan {
                    prompt: None,
                    query: None,
                    tag: None,
                    limit: None,
                },
                "plan",
            ),
            (Op::Shutdown, "shutdown"),
        ];
        for (op, expected) in cases {
            assert_eq!(op.op_name(), *expected);
            let serialized = serde_json::to_value(op).unwrap();
            assert_eq!(serialized["op"].as_str(), Some(*expected));
        }
    }

    #[test]
    fn session_usage_names_all_five_mandatory_scenarios() {
        for scenario in [
            "scope-task",
            "find-code",
            "plan-rollback",
            "sync-branch",
            "impact",
            "what-changed",
        ] {
            assert!(
                SESSION_USAGE.contains(scenario),
                "SESSION_USAGE must name the mandatory scenario '{scenario}' — \
                 an injected session that never hears about a scenario will never use it"
            );
        }
    }

    #[test]
    fn session_capabilities_track_every_real_op() {
        // The exhaustive real variant set, independent of SESSION_CAPABILITIES
        // itself — this must be updated by hand whenever a variant is added,
        // same as op_name_matches_serde_tag's `cases` above, so the two lists
        // can't silently drift in the same direction and still agree.
        let all_real_ops: &[&str] = &[
            "ping",
            "recall",
            "search-content",
            "scope-task",
            "find-symbol",
            "list-signatures",
            "pack-context",
            "impact",
            "who-calls",
            "call-path",
            "list-flows",
            "list-areas",
            "what-changed",
            "rebuild-graph",
            "status",
            "find-code",
            "search-history",
            "file-history",
            "dig-history",
            "sync-branch",
            "record-event",
            "repo-state",
            "review-changes",
            "diff",
            "commit-history",
            "commit",
            "push",
            "commit-and-push",
            "new-branch",
            "fast-forward",
            "fetch",
            "note",
            "repo-map",
            "plan",
            "flow",
            "shutdown",
        ];
        // Every advertised capability must be a real op.
        for cap in SESSION_CAPABILITIES {
            assert!(
                all_real_ops.contains(cap),
                "SESSION_CAPABILITIES advertises '{cap}', which is not a real Op variant — \
                 this is exactly the false-context bug this list exists to prevent"
            );
        }
        // Every real, user-facing op (everything except the internal `shutdown`)
        // must be advertised — an op silently missing from the capability
        // block is a quieter version of the same failure.
        for op in all_real_ops {
            if *op == "shutdown" {
                continue;
            }
            assert!(
                SESSION_CAPABILITIES.contains(op),
                "'{op}' is a real Op variant but missing from SESSION_CAPABILITIES — \
                 an agent reading the SessionStart block won't know it exists"
            );
        }
    }
}

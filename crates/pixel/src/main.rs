//! pixel CLI — index/search plus the graph command surface, speaking to a
//! per-root daemon over its Unix socket when one is up, else in-process.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::{ArgMatches, CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum};

// Count rendered writes without changing descriptors, native streams, or TTY state.
macro_rules! print {
    ($($arg:tt)*) => { crate::operation_metrics::print(format_args!($($arg)*)) };
}
macro_rules! println {
    () => { crate::operation_metrics::print(format_args!("\n")) };
    ($($arg:tt)*) => { crate::operation_metrics::print(format_args!("{}\n", format_args!($($arg)*))) };
}
macro_rules! eprintln {
    () => { crate::operation_metrics::print_error(format_args!("\n")) };
    ($($arg:tt)*) => { crate::operation_metrics::print_error(format_args!("{}\n", format_args!($($arg)*))) };
}
mod call_guard;
mod claude_controller;
mod coverage_cmd;
mod evaluate_cmd;
mod guard;
mod index_cmd;
mod mcp_cmd;
mod operation_metrics;
mod plan_cmd;
mod plan_state;
mod post_compaction;
mod prompt_submit;
mod recall_cmd;
mod rescue_cmd;
mod search_compat;
mod sniper_cmd;
mod task_plan;
mod task_runtime;
mod task_sandbox;
mod task_scheduler;
mod web_search;
mod workspace_cmd;
use pixel_daemon::api::{PROTOCOL_VERSION, Request, Response, Service, failure_response};
use pixel_daemon::daemon;
use pixel_index::index::{build, shard_path};
use pixel_index::shard::Shard;
use pixel_index::{Crc32Weigher, GramExtractor, SparseGramExtractor, TrigramExtractor};
use pixel_proto::{QueryKind, QueryStatus, compile_query};
use serde_json::{Value, json};

/// `pixel --version` (long form): the crate version plus where the binary
/// came from, all captured by `build.rs` at compile time (`unknown` when a
/// value could not be determined, e.g. a tarball build without `.git`).
/// `pixel -V` keeps the one-line `pixel x.y.z`.
const LONG_VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    "\ncommit: ",
    env!("PIXEL_BUILD_COMMIT"),
    "\ntarget: ",
    env!("PIXEL_BUILD_TARGET"),
    "\nrustc: ",
    env!("PIXEL_RUSTC_VERSION"),
    "\nbuilt: ",
    env!("PIXEL_BUILD_DATE"),
);

#[derive(Parser)]
#[command(
    name = "pixel",
    version,
    long_version = LONG_VERSION,
    about = "Fast, fresh code retrieval for agents"
)]
struct Cli {
    /// Disable live metrics (accounting remains available in the local action log).
    #[arg(long, global = true, default_value = "on", value_parser = ["on", "off"])]
    metrics: String,
    #[command(subcommand)]
    command: Command,
}

#[derive(Copy, Clone, ValueEnum)]
enum ExtractorKind {
    Sparse,
    Trigram,
}

#[derive(Copy, Clone, ValueEnum)]
enum DirectionArg {
    Upstream,
    Downstream,
}

#[derive(Copy, Clone, ValueEnum)]
enum RoleArg {
    Callers,
    Callees,
}

#[derive(Subcommand)]
enum Command {
    /// Build (or rebuild) the text index for a directory tree.
    #[command(alias = "index")]
    BuildIndex {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long, value_enum, default_value = "trigram")]
        extractor: ExtractorKind,
        /// Maximum sparse gram length (ignored for trigram).
        #[arg(long, default_value_t = pixel_index::gram::DEFAULT_MAX_GRAM)]
        max_gram: usize,
        /// Also ingest the facts/history db (commit metadata + diff text).
        #[arg(long)]
        history: bool,
    },
    /// Search the indexed tree with a regex pattern. Accepts any number of
    /// paths (repo roots, subdirectories, or files) — ripgrep-style; the repo
    /// root is discovered automatically for each.
    #[command(alias = "search")]
    SearchContent {
        pattern: String,
        /// Paths to search: repo roots, subdirectories, or files (any mix).
        #[arg(default_value = ".")]
        paths: Vec<PathBuf>,
        /// Emit ndjson matches instead of text lines.
        #[arg(long)]
        json: bool,
        /// Print candidate/timing stats to stderr.
        #[arg(long)]
        stats: bool,
        /// Maximum matching lines to return (hard-capped at 10,000).
        #[arg(long)]
        limit: Option<usize>,
        /// Skip this many matching lines for page-wise retrieval.
        #[arg(long, default_value_t = 0)]
        offset: usize,
        /// Skip the daemon even if one is running.
        #[arg(long)]
        no_daemon: bool,
        /// Ranking scope. `code` reranks matches by file-level signals
        /// (filename match, symbol match, content density) via pixel-rank's
        /// RRF without changing the hit set. `hybrid` adds a semantic
        /// embedding channel (static code embeddings, potion-code-16M-v2)
        /// fused as a 6th RRF signal — better recall on paraphrase/synonym
        /// queries at ~ms cost when the model is cached; degrades to `code`
        /// if the model is unavailable. Any other value is an error. Omit
        /// `scope` for unranked (path/line) order.
        #[arg(long)]
        scope: Option<String>,
        /// Lines of context to include around each match (reads the file
        /// on-demand). Eliminates the need for a follow-up Read call.
        #[arg(long, default_value_t = 0)]
        context: usize,
        /// Case-insensitive search (ripgrep-compatible shorthand).
        #[arg(short = 'i', long = "ignore-case")]
        ignore_case: bool,
    },
    /// Native-output literal file search for automatic routing; unsupported
    /// inputs execute the original rg/grep command without modification.
    #[command(alias = "search-compat")]
    SearchLikeRg {
        #[arg(value_enum)]
        tool: search_compat::SearchTool,
        #[arg(last = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Compile and execute one bounded deterministic retrieval recipe.
    #[command(alias = "query")]
    RunRecipe {
        intent: String,
        #[arg(long, default_value = ".")]
        path: PathBuf,
        #[arg(long, default_value = "auto")]
        kind: String,
        #[arg(long, default_value_t = 800)]
        budget: usize,
        #[arg(long)]
        json: bool,
        #[arg(long)]
        no_daemon: bool,
    },
    /// Semantic code search: embed a natural-language question ("how is
    /// authentication handled?") and rank files by semantic/lexical rank fusion.
    /// Complements `search` (regex) and `resolve` (deterministic phrase→code);
    /// the answer is a ranked list, not a resolved certainty. First use
    /// downloads the embedding model into the shared recall model cache
    /// (once; subsequent calls are offline).
    #[command(alias = "ask")]
    SearchMeaning {
        /// The natural-language question.
        question: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Number of ranked hits to return.
        #[arg(long, default_value_t = 8)]
        limit: usize,
        /// Cap on how many source files are scanned.
        #[arg(long, default_value_t = 2000)]
        max_files: usize,
        #[arg(long)]
        json: bool,
    },
    /// Sniper target list: task description in, closed prioritized file list
    /// out (P0 = start here, P1 = likely, P2 = droppable). Writes the
    /// enforcement manifest .pixel/targets.json unless --no-manifest.
    #[command(alias = "targets")]
    ScopeTask {
        /// Task/feature description (omit with --clear).
        task: Option<String>,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
        /// Maximum files in the closed list (default 20, max 100).
        #[arg(long)]
        limit: Option<usize>,
        /// Skip writing the enforcement manifest.
        #[arg(long)]
        no_manifest: bool,
        /// Deactivate scoping: delete .pixel/targets.json and exit.
        #[arg(long)]
        clear: bool,
        /// Drop files above this tier: "P0" = P0 only, "P1" = P0+P1, "P2" = all.
        #[arg(long)]
        max_tier: Option<String>,
        /// Precision mode: drop low-score P1/P2 files when there's a sharp
        /// score gap after P0. Improves precision on simple tasks.
        #[arg(long)]
        precision: bool,
    },
    /// Surgical revert planner: locate the files a problem points at, list
    /// recent versions with the likely-breaking commit flagged, recommend a
    /// last-known-good candidate. Plan only — nothing is written without
    /// --apply. Never resets; never touches the index or HEAD.
    #[command(alias = "rescue")]
    PlanRollback {
        /// Problem description ("login was working before ...").
        problem: Option<String>,
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Explicit target file(s), repo-relative; skips target discovery.
        #[arg(long = "file")]
        files: Vec<String>,
        /// Commits of per-file history to inspect.
        #[arg(long, default_value_t = 10)]
        depth: usize,
        /// Restore the --file targets to this commit (gated action).
        #[arg(long)]
        apply: Option<String>,
        /// With --apply on dirty files: deterministic 3-way merge that keeps
        /// in-progress edits (may leave conflict markers).
        #[arg(long)]
        merge: bool,
        /// With --apply: `git stash push` the dirty planned files first.
        #[arg(long)]
        stash_first: bool,
        /// With --apply: overwrite dirty files (loses in-progress work).
        #[arg(long)]
        allow_dirty: bool,
        #[arg(long)]
        json: bool,
    },
    /// Look up symbols by name in the code graph.
    #[command(alias = "symbol")]
    FindSymbol {
        name: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// All signatures in a file — the skeleton view at ~10% of Read cost.
    #[command(alias = "skeleton")]
    ListSignatures {
        file: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Human notes on the map: durable annotations keyed by file + symbol
    /// name (or concept norm). Survive rebuilds; merged into `resolve` and
    /// `targets` results. `pixel note set <file> <target> <note>`,
    /// `get`/`rm <file> <target>`, `list [file]`.
    Note {
        /// set | get | rm | list
        action: String,
        /// File the note is attached to (repo-relative or absolute).
        file: Option<String>,
        /// Symbol name or concept norm the note targets.
        target: Option<String>,
        /// Note text (required for `set`).
        note: Option<String>,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Structural repo map: every indexed file with its symbols. `--markdown`
    /// emits the exportable document form — the human-editable projection of
    /// the graph that `note` annotations key onto.
    #[command(alias = "map")]
    RepoMap {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Emit the exportable markdown document.
        #[arg(long)]
        markdown: bool,
        #[arg(long)]
        json: bool,
    },
    /// Budget-fitted context for a symbol uid.
    #[command(alias = "context")]
    PackContext {
        uid: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        budget: Option<usize>,
        #[arg(long)]
        json: bool,
    },
    /// Blast radius of a symbol (callers upstream / callees downstream).
    Impact {
        uid_or_name: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long, value_enum, default_value = "upstream")]
        direction: DirectionArg,
        #[arg(long)]
        depth: Option<u32>,
        /// Answer from every repo in .pixel/workspace.json, merged per repo.
        #[arg(long)]
        workspace: bool,
        #[arg(long)]
        json: bool,
    },
    /// Direct callers or callees of a symbol.
    #[command(alias = "uses")]
    WhoCalls {
        uid_or_name: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long, value_enum, default_value = "callers")]
        role: RoleArg,
        /// Skip this many relationships for page-wise retrieval.
        #[arg(long, default_value_t = 0)]
        offset: usize,
        /// Answer from every repo in .pixel/workspace.json, merged per repo.
        #[arg(long)]
        workspace: bool,
        #[arg(long)]
        json: bool,
    },
    /// Rename a symbol like an IDE refactor — graph-resolved call/reference
    /// sites and import bindings, each verified against a fresh tree-sitter
    /// parse before its bytes are touched. Unresolved same-name sites are
    /// reported, never guessed.
    Rename {
        /// Symbol name to rename.
        name: String,
        /// New identifier.
        new_name: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Disambiguate to the declaration in this file.
        #[arg(long)]
        file: Option<String>,
        /// Disambiguate to this symbol uid (from `find-symbol`).
        #[arg(long)]
        uid: Option<String>,
        /// Compute and print the verified edit set without writing.
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        json: bool,
    },
    /// Call path between two symbols.
    #[command(alias = "trace")]
    CallPath {
        from: String,
        to: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Evaluate a bounded predicate about the indexed call graph and
    /// return the witness that established it.
    ///
    /// Unlike `call-path`, a negative distinguishes "no path in the stored
    /// relation, traversal exhaustive" from "the traversal was cut": the
    /// first is an answer, the second is `unknown` with the budget to
    /// raise. The verdict always carries the snapshot it is about.
    Evaluate {
        #[command(subcommand)]
        cmd: EvaluateCmd,
    },
    /// Discovered execution flows.
    #[command(alias = "processes")]
    ListFlows {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long, default_value_t = 0)]
        offset: usize,
        #[arg(long)]
        json: bool,
    },
    /// Functional-area clusters.
    #[command(alias = "clusters")]
    ListAreas {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long, default_value_t = 0)]
        offset: usize,
        #[arg(long)]
        json: bool,
    },
    /// Symbols/flows affected by working-tree changes.
    #[command(alias = "changes")]
    WhatChanged {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        base: Option<String>,
        #[arg(long, default_value_t = 0)]
        offset: usize,
        /// Also map affected symbols to the test files that exercise them.
        #[arg(long)]
        tests: bool,
        #[arg(long)]
        json: bool,
    },
    /// Force (re)build of the code graph db.
    #[command(alias = "graph")]
    RebuildGraph {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Manage the multi-repo workspace (.pixel/workspace.json) that
    /// `impact --workspace` and `who-calls --workspace` fan out across.
    Workspace {
        #[command(subcommand)]
        cmd: workspace_cmd::WorkspaceCmd,
    },
    /// Freeze this repo's index into a single shareable `.pxpack` bundle —
    /// the file CI builds once and teammates install instead of re-indexing.
    IndexPack {
        /// Output file (e.g. index.pxpack).
        #[arg(long)]
        out: PathBuf,
        /// Also pack history.db (the on-demand facts index).
        #[arg(long)]
        include_history: bool,
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Install a packed index into this repo's `.pixel/` — from a path or
    /// an https:// URL.
    IndexUnpack {
        /// Pack file path or URL.
        source: String,
        /// Replace the index while a daemon is running.
        #[arg(long)]
        force: bool,
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Serve this repo's index over MCP stdio — the single integration for
    /// every MCP-capable agent (search, resolve, impact, callers/callees,
    /// evaluate, context, status).
    Mcp {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Index + graph freshness status.
    Status {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
        /// Compact one-line summary for shell prompts / statuslines.
        #[arg(long)]
        statusline: bool,
    },
    /// Per-language coverage: files the index policy sees on disk vs files
    /// the graph actually indexed, with symbol counts per language.
    Coverage {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Make a repository ready for agent work: index, graph, and warm daemon.
    #[command(alias = "ready")]
    PrepareRepo {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Build indexes only; do not start or use the background daemon.
        #[arg(long)]
        no_daemon: bool,
        #[arg(long)]
        json: bool,
    },
    /// Show raw shard metadata (legacy).
    #[command(alias = "stats")]
    IndexStats {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Manage the per-root background daemon.
    Daemon {
        #[command(subcommand)]
        cmd: DaemonCmd,
    },
    /// Search and browse LLM CLI transcripts (machine-wide corpus).
    Recall {
        #[command(subcommand)]
        cmd: recall_cmd::RecallCmd,
    },
    /// One-look error capture: query the sniper error sink.
    #[command(alias = "sniper")]
    ListErrors {
        #[command(subcommand)]
        cmd: sniper_cmd::SniperCmd,
    },
    /// Deterministic web lookup for terms the index cannot know — the
    /// refine step of a gated `pixel plan`. No LLM, no daemon.
    WebSearch {
        /// The term or question to resolve.
        query: String,
        #[arg(long, default_value_t = web_search::DEFAULT_LIMIT)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    // -----------------------------------------------------------------
    // M2 — safe git mutation ops (pixel-ops)
    // -----------------------------------------------------------------
    /// Show repo state: HEAD, branch, dirty files, fingerprints.
    ///
    /// The answer is compact: the exact `dirty_count`/`clean_count` and the
    /// dirty files. The tracked-clean file list (200 paths, the bulk of the
    /// answer on a clean tree) needs `--include-clean`.
    #[command(alias = "inspect")]
    RepoState {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Restrict the snapshot to these repo-relative paths.
        #[arg(long = "files")]
        files: Vec<String>,
        /// Include the capped tracked-clean file list (default: count only).
        #[arg(long)]
        include_clean: bool,
        #[arg(long)]
        json: bool,
    },
    /// Review working-tree changes (staged, unstaged, untracked, conflicted).
    #[command(alias = "review")]
    ReviewChanges {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Pagination cursor (opaque).
        #[arg(long)]
        cursor: Option<String>,
        /// Cap output bytes.
        #[arg(long)]
        byte_cap: Option<usize>,
        #[arg(long)]
        json: bool,
    },
    /// Commit history with detail levels and byte caps.
    #[command(alias = "history")]
    CommitHistory {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Ref to log (default HEAD).
        #[arg(long = "ref")]
        ref_: Option<String>,
        /// Max commits (capped at 100).
        #[arg(long)]
        limit: Option<usize>,
        /// compact (oid+subject) or full (oid+author+date+subject+body).
        #[arg(long, default_value = "compact")]
        detail: String,
        /// Pagination cursor (skip N commits).
        #[arg(long)]
        cursor: Option<String>,
        /// Cap output bytes.
        #[arg(long)]
        byte_cap: Option<usize>,
        #[arg(long)]
        json: bool,
    },
    /// Structured diff between two refs (or ref → working tree).
    Diff {
        from: String,
        /// Optional target ref; if omitted, diff to working tree.
        to: Option<String>,
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Restrict diff to these paths.
        #[arg(long)]
        paths: Vec<String>,
        /// Cap diff text bytes.
        #[arg(long)]
        byte_cap: Option<usize>,
        #[arg(long)]
        json: bool,
    },
    /// Stage files, commit, and optionally push (crash-safe, idempotent).
    #[command(alias = "publish")]
    Commit {
        /// Commit message. Use `--message-file` for a multi-paragraph body.
        #[arg(
            short = 'm',
            long = "message",
            required_unless_present = "message_file"
        )]
        message: Option<String>,
        /// Read the commit message from this file (`-` for stdin), verbatim
        /// except for trailing whitespace. Mutually exclusive with `-m`.
        #[arg(short = 'F', long = "message-file", conflicts_with = "message")]
        message_file: Option<PathBuf>,
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Files to stage (repo-relative). Repeat the flag once per file
        /// (`--files a --files b`) — a single `--files a b` does NOT work:
        /// `b` silently becomes the trailing PATH argument instead of a
        /// second file, or errors if a PATH was already given.
        #[arg(long = "files")]
        files: Vec<String>,
        /// Also push after committing.
        #[arg(long)]
        push: bool,
        /// Amend the current commit instead of creating a new one.
        #[arg(long)]
        amend: bool,
        /// Reject if HEAD does not match this OID.
        #[arg(long)]
        expected_head: Option<String>,
        /// Idempotency / recovery key.
        #[arg(long)]
        request_id: String,
        #[arg(long)]
        json: bool,
    },
    /// Leased push to a remote (crash-safe, idempotent).
    Push {
        remote: String,
        refspec: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        force_with_lease: bool,
        /// Idempotency / recovery key.
        #[arg(long)]
        request_id: String,
        #[arg(long)]
        json: bool,
    },
    /// Publish + push in one op (commit then leased push).
    #[command(alias = "ship")]
    CommitAndPush {
        /// Commit message. Use `--message-file` for a multi-paragraph body.
        #[arg(
            short = 'm',
            long = "message",
            required_unless_present = "message_file"
        )]
        message: Option<String>,
        /// Read the commit message from this file (`-` for stdin), verbatim
        /// except for trailing whitespace. Mutually exclusive with `-m`.
        #[arg(short = 'F', long = "message-file", conflicts_with = "message")]
        message_file: Option<PathBuf>,
        // Positional order matches Push: required remote + refspec first,
        // then the defaulted path. (A defaulted positional BEFORE required
        // ones trips clap's debug assertions — every debug-build parse
        // panicked before this reorder.)
        remote: String,
        refspec: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Files to stage (repo-relative). Repeat the flag once per file
        /// (`--files a --files b`) — a single `--files a b` does NOT work:
        /// `b` silently becomes the trailing PATH argument instead of a
        /// second file, or errors if a PATH was already given.
        #[arg(long = "files")]
        files: Vec<String>,
        /// Leased force-push (`--force-with-lease`) for the push phase.
        #[arg(long)]
        force_with_lease: bool,
        /// Idempotency / recovery key.
        #[arg(long)]
        request_id: String,
        #[arg(long)]
        json: bool,
    },
    /// Create a new branch from HEAD (or --from <ref>).
    #[command(alias = "branch")]
    NewBranch {
        name: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Base ref (default HEAD).
        #[arg(long)]
        from: Option<String>,
        /// Idempotency / recovery key.
        #[arg(long)]
        request_id: String,
        #[arg(long)]
        json: bool,
    },
    /// Fast-forward merge to a target OID (refuses non-ff + dirty intersection).
    #[command(alias = "update")]
    FastForward {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Reject if HEAD does not match this OID.
        #[arg(long)]
        expected_head: String,
        /// Fast-forward target OID.
        #[arg(long)]
        target_oid: String,
        /// Idempotency / recovery key.
        #[arg(long)]
        request_id: String,
        #[arg(long)]
        json: bool,
    },
    /// Fetch from a remote (idempotent).
    #[command(alias = "sync")]
    Fetch {
        remote: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Optional refspec.
        #[arg(long)]
        refspec: Option<String>,
        #[arg(long)]
        json: bool,
    },
    // -----------------------------------------------------------------
    // M3/M4 — engines (resolve, history, lifecycle, excavate, reconcile)
    // -----------------------------------------------------------------
    /// Engine 1: resolve a phrase to code via the concept index.
    #[command(alias = "resolve")]
    FindCode {
        phrase: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        limit: Option<usize>,
        #[arg(long)]
        json: bool,
    },
    /// M3: history-wide fact + diff search.
    #[command(alias = "history-search")]
    SearchHistory {
        query: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        /// message | path | diff | all
        #[arg(long, default_value = "all")]
        facet: String,
        #[arg(long)]
        limit: Option<usize>,
        #[arg(long)]
        json: bool,
    },
    /// Engine 2: lifecycle of a path or token.
    #[command(alias = "lifecycle")]
    FileHistory {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Repo-relative path to inspect.
        #[arg(long)]
        file: Option<String>,
        /// Token to inspect.
        #[arg(long)]
        token: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Engine 2: history-wide discovery (rescue v2).
    #[command(alias = "excavate")]
    DigHistory {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Phrase to search for in diff text.
        #[arg(long)]
        phrase: Option<String>,
        /// Restrict to a repo-relative path (with --show: the path to read).
        #[arg(long)]
        file: Option<String>,
        #[arg(long)]
        from: Option<String>,
        #[arg(long)]
        to: Option<String>,
        #[arg(long)]
        limit: Option<usize>,
        /// Print the FULL content of --file at this commit (safe
        /// `git show <oid>:<path>` equivalent). When the file does not
        /// exist at <oid> but does at <oid>^ — i.e. <oid> is the deletion
        /// commit — the parent's pre-deletion content is returned and
        /// flagged. One pixel call replaces the `git show` follow-ups.
        #[arg(long, value_name = "OID")]
        show: Option<String>,
        /// With --show: read from the commit's first parent (<oid>^)
        /// directly, skipping the read at <oid> itself.
        #[arg(long)]
        parent: bool,
        #[arg(long)]
        json: bool,
    },
    /// Engine 4: one-call deterministic branch sync.
    #[command(alias = "reconcile")]
    SyncBranch {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// report (default) | rebase-if-clean
        #[arg(long, default_value = "report")]
        strategy: String,
        /// auto (default) | none
        #[arg(long, default_value = "auto")]
        push: String,
        /// Integrate: rebase current branch onto origin/<TARGET>, then
        /// fast-forward local <TARGET> to the rebased head (never merge).
        /// `--strategy` is ignored in this mode.
        #[arg(long, value_name = "TARGET")]
        into: Option<String>,
        /// Idempotency / recovery key.
        #[arg(long)]
        request_id: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// M5: journal a session event (fire-and-forget).
    #[command(alias = "journal")]
    RecordEvent {
        kind: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Repo-relative path the event concerns.
        #[arg(long)]
        file: Option<String>,
        #[arg(long)]
        detail: Option<String>,
        #[arg(long)]
        json: bool,
    },
    // -----------------------------------------------------------------
    // M5/M6 — install / doctor / migrate / hook
    // -----------------------------------------------------------------
    /// Idempotently deploy the agent prompt, the Claude shell wrapper and the
    /// Codex developer_instructions config key.
    Install {
        #[arg(long)]
        json: bool,
        /// Shell to install the `claude` wrapper block for
        /// (default: $SHELL). Pass e.g. `fish` when the invoking process
        /// does not run under your login shell.
        #[arg(long)]
        shell: Option<String>,
    },
    /// Remove everything `pixel install` wrote: managed blocks from
    /// agent-config files, hook entries from all settings files, hook
    /// scripts, the pi guard extension, the rule source file, and the
    /// pixel binary itself. Idempotent: safe to re-run.
    Uninstall {
        #[arg(long)]
        json: bool,
        /// Preview what would be removed without making any changes.
        #[arg(long)]
        dry_run: bool,
        /// Path to the pixel binary to remove (default: ~/.local/bin/pixel).
        #[arg(long)]
        binary_path: Option<PathBuf>,
        /// Shell whose wrapper block should be removed (default: the
        /// account's login shell, then $SHELL).
        #[arg(long)]
        shell: Option<String>,
        /// Remove only that shell's wrapper block and keep everything else
        /// installed: the fix for a block `pixel doctor` reports in a
        /// profile the login shell never loads.
        #[arg(long)]
        wrappers_only: bool,
    },
    /// Check that a release tag is consistent with the tree before anything
    /// is built or published: crates/pixel/Cargo.toml carries the version,
    /// Cargo.lock is fresh for every workspace member, CHANGELOG.md has the
    /// `## [x.y.z]` heading and an empty Unreleased section. Exit 1 on any
    /// failed check.
    #[command(alias = "release-check")]
    CheckRelease {
        /// The version or tag: `1.2.3`, `v1.2.3` or `refs/tags/v1.2.3`.
        version: String,
        /// Repository root (default: current directory).
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        /// Emit the report as one JSON document.
        #[arg(long)]
        json: bool,
    },
    /// Rebuild the binary, stop the daemon, copy the new binary to the
    /// install path, and optionally restart the daemon. Solves the
    /// "Text file busy" error when the daemon holds the binary open.
    #[command(alias = "upgrade")]
    SelfUpdate {
        /// Cargo build command to run (default: `cargo build --release -p pixel-cli`).
        /// The built binary is read from `target/<profile>/pixel`, with the
        /// profile taken from this command's `--profile`/`--release` flags.
        #[arg(long, default_value = "cargo build --release -p pixel-cli")]
        build: String,
        /// Install path. Default: the binary running this command (unless
        /// it lives in a cargo `target/` dir), else the first `pixel` on
        /// PATH (shim directories skipped), else ~/.local/bin/pixel. A
        /// default that lands in a mise install dir or a Homebrew Cellar is
        /// refused; passing this flag writes there anyway.
        #[arg(long)]
        install_path: Option<PathBuf>,
        /// Install to ~/.local/bin/pixel-dev instead: a side build to call
        /// as `pixel-dev`, which never shadows or replaces `pixel`.
        #[arg(long, conflicts_with = "install_path")]
        dev: bool,
        /// Restart the daemon after upgrade.
        #[arg(long)]
        restart_daemon: bool,
        /// Repo path for daemon restart.
        #[arg(long)]
        repo: Option<PathBuf>,
        /// Resolve and print the install path, then exit without building
        /// or installing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Health check: install state, daemon, index/graph/facts freshness.
    Doctor {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
        /// Shell whose wrapper block should be checked (default: $SHELL).
        #[arg(long)]
        shell: Option<String>,
    },
    /// Removed: the legacy `.gitpixel/` migration. Hidden and kept only so a
    /// script that still calls it exits 0 with a note instead of failing
    /// with "unrecognized subcommand".
    #[command(hide = true)]
    Migrate {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Hook entrypoints (guard, session-start) invoked by Claude hooks.
    #[command(alias = "hook")]
    RunHook {
        #[command(subcommand)]
        cmd: HookCmd,
    },
    /// Inspect or reset Claude Code's local Pixel task-runtime packet.
    #[command(alias = "task")]
    TaskState {
        #[command(subcommand)]
        cmd: TaskCmd,
    },
    /// Self-assessment: pixel's own action log (what ran, what went wrong).
    /// Reads <path>/.pixel/actions.jsonl, written asynchronously by every
    /// pixel invocation.
    #[command(alias = "log")]
    ActionLog {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Most recent entries to show.
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Only show entries that ended in an error.
        #[arg(long)]
        errors_only: bool,
        #[arg(long)]
        json: bool,
        /// Delete the action log for this root and exit.
        #[arg(long)]
        clear: bool,
    },
    /// Token-savings report: for retrieval-shaped commands (search/query/
    /// context/resolve) that recorded snippet-vs-pool volumes, aggregate the
    /// fraction of the candidate pool the agent did NOT have to read. A
    /// measured counter to semble's '99% fewer tokens' claim — own numbers,
    /// same format.
    #[command(alias = "savings")]
    TokenSavings {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
        /// Only consider events from the last N hours.
        #[arg(long)]
        since_hours: Option<u64>,
    },
    /// Squash every commit on the current branch since its base into ONE
    /// commit (crash-safe, backup-ref'd), optionally force-pushing with lease.
    #[command(alias = "rewrite")]
    SquashBranch {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Explicit base ref (squash <onto>..HEAD). Default: merge-base with
        /// the branch upstream, else with the remote default branch.
        #[arg(long)]
        onto: Option<String>,
        /// Squash commit message (default: auto-generated subject list).
        #[arg(short = 'm', long = "message")]
        message: Option<String>,
        /// Push the rewritten branch with --force-with-lease afterwards.
        #[arg(long)]
        push: bool,
        /// Remote name.
        #[arg(long, default_value = "origin")]
        remote: String,
        /// Reject if HEAD does not match this OID.
        #[arg(long)]
        expected_head: Option<String>,
        /// Allow rewriting the default branch and published mainline commits.
        /// Overrides both default-branch and published-mainline protection.
        #[arg(long)]
        allow_default_branch: bool,
        /// Idempotency / recovery key.
        #[arg(long)]
        request_id: String,
        #[arg(long)]
        json: bool,
    },
    /// Per-region blame attribution: who introduced/owns each region of a file.
    #[command(alias = "provenance")]
    WhoWrote {
        /// Repo-relative file to attribute.
        file: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Restrict to lines a,b (1-based inclusive), e.g. --lines 10,40.
        #[arg(long, value_parser = parse_line_range)]
        lines: Option<(u32, u32)>,
        /// Author query (case-insensitive substring on name or email) —
        /// adds a did-they-touch-this verdict.
        #[arg(long)]
        author: Option<String>,
        /// Max regions emitted (default 200); truncation sets lower_bound.
        #[arg(long, default_value_t = 200)]
        limit_regions: usize,
        #[arg(long)]
        json: bool,
    },
    /// One-call read-only branch inventory: ahead/behind, merged, stale,
    /// unpushed — the deterministic "did you push everything?" answer.
    #[command(alias = "branches")]
    ListBranches {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Run `git fetch --prune <remote>` first for a live view.
        #[arg(long)]
        fetch: bool,
        /// Remote name.
        #[arg(long, default_value = "origin")]
        remote: String,
        /// Days after which a branch counts as stale.
        #[arg(long, default_value_t = 30)]
        stale_days: u64,
        #[arg(long)]
        json: bool,
    },
    /// Additive-only, key-level .env mutations with snapshots and restore.
    /// Values are NEVER printed in any output.
    #[command(alias = "env")]
    EditEnv {
        #[command(subcommand)]
        cmd: EnvCmd,
    },
    /// Deterministic todo list generation from code analysis.
    Plan {
        /// Natural-language prompt (optional if --query is used).
        prompt: Option<String>,
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Explicit query: dead-interactive | dead-code | hotspots | recent-changes | by-concept.
        #[arg(long)]
        query: Option<String>,
        /// Tag filter for dead-interactive (e.g. button, a, Link).
        #[arg(long)]
        tag: Option<String>,
        /// Limit for hotspots/recent-changes.
        #[arg(long)]
        limit: Option<usize>,
        /// Output format: markdown | json | compact.
        #[arg(long, default_value = "markdown")]
        format: String,
        /// Omit the trailing verification todo.
        #[arg(long)]
        no_verify: bool,
        /// Cap the number of findings returned.
        #[arg(long)]
        max_todos: Option<usize>,
        /// Print the tracked checklist (.pixel/plan.json) without planning.
        #[arg(long, conflicts_with_all = ["prompt", "query", "tag", "limit", "format", "no_verify", "max_todos"])]
        status: bool,
        /// Mark tracked item N done (numbering from --status). Repeatable.
        #[arg(long, value_name = "N", conflicts_with_all = ["prompt", "query", "tag", "limit", "format", "no_verify", "max_todos"])]
        done: Vec<usize>,
        /// Mark tracked item N not done. Repeatable.
        #[arg(long, value_name = "N", conflicts_with_all = ["prompt", "query", "tag", "limit", "format", "no_verify", "max_todos"])]
        undone: Vec<usize>,
        /// Drop findings the latest plan no longer reports.
        #[arg(long, conflicts_with_all = ["prompt", "query", "tag", "limit", "format", "no_verify", "max_todos"])]
        prune: bool,
        #[arg(long)]
        json: bool,
    },
    /// Save, retrieve, list, revise, and replay proven agent-browser paths
    /// (auth flows, config flows) so the agent follows a deterministic
    /// shortcut instead of re-discovering the UI from scratch every time.
    #[command(alias = "flow")]
    ReplayFlow {
        #[command(subcommand)]
        cmd: FlowCmd,
    },
}

#[derive(Subcommand)]
enum EvaluateCmd {
    /// Does a path exist from `--from` to `--to` in the indexed call graph?
    ///
    /// `--tiers` selects which stored edges form the relation, never a
    /// confidence threshold: nothing widens automatically when the narrow
    /// relation finds nothing.
    Path {
        /// Source symbol: a uid (`path#qualified#kind`) or a name that
        /// resolves to exactly one symbol.
        #[arg(long)]
        from: String,
        /// Target symbol: a uid or an unambiguous name.
        #[arg(long)]
        to: String,
        /// Walk outgoing edges (`callees`) or incoming ones (`callers`).
        #[arg(long, default_value = "callees")]
        traversal: String,
        /// Edge tiers forming the relation: `exact` or `exact,probable`.
        #[arg(long, default_value = "exact")]
        tiers: String,
        /// Maximum traversal depth.
        #[arg(long)]
        max_depth: Option<u32>,
        /// Wall-clock budget for the traversal itself, in milliseconds.
        #[arg(long)]
        time_budget_ms: Option<u64>,
        /// Resolve names only under this repo-relative path prefix.
        #[arg(long = "in")]
        scope: Option<String>,
        /// Answer about the stored snapshot: skip the after-check.
        #[arg(long)]
        at_snapshot: bool,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum EnvCmd {
    /// List .env files under root — key NAMES only, never values.
    Inventory {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Set one key (snapshot-first; every other line byte-preserved).
    Set {
        #[arg(long)]
        file: PathBuf,
        #[arg(long)]
        key: String,
        #[arg(long)]
        value: String,
        /// Create the file if it does not exist.
        #[arg(long)]
        create_file: bool,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Restore from a snapshot (latest if --snapshot omitted; undoable).
    Restore {
        #[arg(long)]
        file: PathBuf,
        #[arg(long)]
        snapshot: Option<String>,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// List snapshots recorded for a file.
    Snapshots {
        #[arg(long)]
        file: PathBuf,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Verify required keys exist (names only).
    Check {
        #[arg(long)]
        file: PathBuf,
        /// Required key name; repeat the flag once per key.
        #[arg(long = "require")]
        require: Vec<String>,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum FlowCmd {
    /// Create a new flow. Refuses to overwrite — use `revise` to update.
    Save {
        /// Flow name (kebab-case recommended, e.g. "github-auth-device-flow").
        name: String,
        #[arg(long)]
        title: String,
        #[arg(long, default_value = "")]
        description: String,
        /// Tag; repeat the flag once per tag (`--tag auth --tag github`).
        #[arg(long = "tag")]
        tags: Vec<String>,
        #[arg(long)]
        url: Option<String>,
        /// Path to a JSON file containing the steps array.
        #[arg(long)]
        from_file: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Retrieve a flow by name (for the agent to follow deterministically).
    Get {
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// List all saved flows, optionally filtered by tag.
    List {
        #[arg(long)]
        tag: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Update an existing flow's metadata and/or steps. Bumps revision.
    Revise {
        name: String,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        description: Option<String>,
        /// Path to a JSON file containing the new steps array.
        #[arg(long)]
        from_file: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Emit ready-to-run agent-browser commands with variable substitution.
    /// Pixel does NOT run agent-browser — it outputs the deterministic
    /// command sequence for the agent to execute.
    ///
    /// Use `--execute` to actually run the commands via agent-browser.
    Replay {
        name: String,
        /// Variable substitution: `--var key=value`. Repeat per var.
        #[arg(long = "var")]
        vars: Vec<String>,
        /// Shortcut for `--var google_account=<value>` (or `openai_account`
        /// depending on the flow). Picks which account to use.
        /// Accepts a full email address (e.g. user@example.com).
        #[arg(long)]
        account: Option<String>,
        /// Actually execute the flow by running agent-browser commands.
        /// Without this flag, replay only prints the command sequence.
        #[arg(long, conflicts_with = "dry_run")]
        execute: bool,
        /// Print commands without marking as executed (default is still
        /// print-only — pixel never runs agent-browser).
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        json: bool,
    },
    /// Delete a flow by name.
    Delete {
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// Pretty-print the full flow document (human-readable).
    Show {
        name: String,
        #[arg(long)]
        json: bool,
    },
}

/// Parse a 1-based inclusive line range "a,b" for `provenance --lines`.
fn parse_line_range(s: &str) -> Result<(u32, u32), String> {
    let (a, b) = s
        .split_once(',')
        .ok_or_else(|| format!("expected 'start,end', got '{s}'"))?;
    let a: u32 = a
        .trim()
        .parse()
        .map_err(|e| format!("bad start line: {e}"))?;
    let b: u32 = b.trim().parse().map_err(|e| format!("bad end line: {e}"))?;
    if a == 0 || b < a {
        return Err(format!("invalid range {a},{b}: need 1 <= start <= end"));
    }
    Ok((a, b))
}

#[derive(Subcommand)]
enum HookCmd {
    /// `pixel hook guard "$@"` — targets enforcement guard.
    Guard {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Use the provider's verified hook response contract.
        #[arg(long, value_enum)]
        provider: Option<guard::Provider>,
        /// Claude only: delegate unchanged calls to the adopted RTK hook.
        #[arg(long)]
        delegate_rtk: bool,
    },
    /// `pixel hook composed-guard --provider codex --backup <path>` — run a
    /// sealed install-time foreign-hook snapshot before Pixel's Codex rewrite.
    ComposedGuard {
        /// Currently only Codex has a composable PreToolUse contract.
        #[arg(long, value_enum, default_value = "codex")]
        provider: guard::Provider,
        /// 0600 JSON snapshot of pre-existing foreign PreToolUse groups.
        #[arg(long)]
        backup: PathBuf,
    },
    /// `pixel hook session-start` — emit capability block from op registry.
    SessionStart {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// `pixel hook prompt-submit "$@"` — task boundary detector.
    /// Reads the UserPromptSubmit payload from stdin, embeds the prompt
    /// and recent context, and emits a `[PIXEL:TASK_BOUNDARY]` advisory
    /// when a task boundary is detected.
    PromptSubmit {
        /// Select the provider-specific task-runtime contract. Omit to
        /// preserve the legacy provider-neutral prompt context behavior.
        #[arg(long, value_enum)]
        provider: Option<guard::Provider>,
    },
    /// `pixel hook post-compaction` — re-inject targets manifest after
    /// context compaction. Reads the PostCompaction payload from stdin,
    /// finds the active `.pixel/targets.json`, and emits it as
    /// `additionalContext` so the agent resumes with its retrieval state.
    PostCompaction {
        /// Select the provider-specific task-runtime contract. Omit to
        /// preserve the legacy provider-neutral compact restoration behavior.
        #[arg(long, value_enum)]
        provider: Option<guard::Provider>,
    },
    /// `pixel hook post-tool-use` — P0·3 blast-radius: after an edit, emit
    /// the dependants of what was just changed (unsolicited). Unlike \[`run`\]
    /// which infers the event from the payload, this *forces* `PostToolUse` —
    /// PostToolUse hook files are per-event,so `hook_event_name` is often absent.
    PostToolUse {
        /// Provider whose hook-response contract to emit under.
        #[arg(long, value_enum)]
        provider: Option<guard::Provider>,
    },
}

#[derive(Subcommand)]
enum TaskCmd {
    /// Begin a provider-neutral durable Pixel task ledger.
    Begin {
        /// Observable objective. Stored as task specification, not model output.
        objective: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Provider which will consume the task evidence.
        #[arg(long, default_value = "claude")]
        provider: String,
        /// Optional provider session mapped to this task.
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Atomically accept a task for Pixel-owned worker execution.
    Accept {
        /// Observable objective retained in the durable task specification.
        objective: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long, default_value = "claude")]
        provider: String,
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Refresh a task's factual repository snapshot.
    Prepare {
        task_id: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Print a durable task record. Corrupt or unavailable state is absent.
    Status {
        task_id: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Replay bounded, factual task-ledger events.
    Events {
        task_id: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Create or reopen an isolated candidate worktree at current HEAD.
    SandboxCreate {
        task_id: String,
        candidate_id: String,
        /// Repo-relative path the candidate exclusively owns. Repeat per path.
        #[arg(long = "owned-path", required = true)]
        owned_paths: Vec<String>,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Inspect a candidate without mutating either worktree.
    SandboxInspect {
        task_id: String,
        candidate_id: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Compare-and-apply an eligible candidate; never performs a merge.
    SandboxPromote {
        task_id: String,
        candidate_id: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Discard the recorded candidate worktree. Safe to repeat.
    SandboxCleanup {
        task_id: String,
        candidate_id: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Alias for sandbox cleanup when cancelling a candidate.
    SandboxCancel {
        task_id: String,
        candidate_id: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Start one Claude worker inside an existing Pixel-owned sandbox.
    WorkerStart {
        task_id: String,
        candidate_id: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Claude executable used for this isolated worker.
        #[arg(long, default_value = "claude")]
        executable: PathBuf,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        max_turns: Option<u32>,
        #[arg(long)]
        max_budget_usd: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Report the recorded process truth for one Pixel worker.
    WorkerStatus {
        task_id: String,
        candidate_id: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Stop one Pixel-owned Claude worker and its process group.
    WorkerStop {
        task_id: String,
        candidate_id: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Start a bounded race across pre-registered, isolated candidates.
    RaceStart {
        task_id: String,
        /// Candidate IDs for the same accepted task. Pixel caps the group at three.
        #[arg(required = true, num_args = 1..=3)]
        candidate_ids: Vec<String>,
        #[arg(long, default_value = ".")]
        path: PathBuf,
        #[arg(long, default_value = "claude")]
        executable: PathBuf,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        max_turns: Option<u32>,
        #[arg(long)]
        max_budget_usd: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Inspect a race and promote the first Pixel-eligible candidate, if any.
    RacePoll {
        task_id: String,
        #[arg(required = true, num_args = 1..=3)]
        candidate_ids: Vec<String>,
        #[arg(long, default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Deterministically validate a model-proposed work plan before fanout.
    PlanValidate {
        /// Durable task which owns this proposed plan.
        task_id: String,
        /// JSON file containing only explicit lanes, ownership, symbols, and dependencies.
        #[arg(long)]
        file: PathBuf,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Print the current Claude session task packet, if it is still valid.
    Show {
        /// Claude Code hook session ID.
        #[arg(long)]
        session: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Remove the current Claude session task packet. Safe when absent.
    Reset {
        /// Claude Code hook session ID.
        #[arg(long)]
        session: String,
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum DaemonCmd {
    /// Start the daemon (background unless --foreground).
    Start {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        foreground: bool,
    },
    /// Stop a running daemon.
    Stop {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Check whether a daemon is running.
    Status {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
}

// ---------------------------------------------------------------------------
// daemon client / execution
// ---------------------------------------------------------------------------

/// One NDJSON round trip on an open stream.
fn roundtrip(stream: &mut UnixStream, req: &Request) -> Option<Response> {
    let mut line = serde_json::to_string(req).ok()?;
    line.push('\n');
    stream.write_all(line.as_bytes()).ok()?;
    stream.flush().ok()?;
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut buf = String::new();
    reader.read_line(&mut buf).ok()?;
    serde_json::from_str(&buf).ok()
}

/// Daemon path: only if the socket answers Ping within ~100ms.
fn try_daemon(root: &Path, req: &Request) -> Option<Response> {
    try_daemon_inner(root, req).or_else(|| {
        // Auto-start: socket connection failed. Spawn the daemon in the
        // background and retry once. This makes the fast path transparent —
        // no need for the user to run `pixel daemon start` manually.
        // `PIXEL_DAEMON_AUTO_START=0` disables auto-start.
        if env_flag_off("PIXEL_DAEMON_AUTO_START") {
            return None;
        }
        let exe = std::env::current_exe().ok()?;
        let abs = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let mut command = std::process::Command::new(exe);
        command
            .arg("daemon")
            .arg("start")
            .arg(&abs)
            .arg("--foreground")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0);
        command.spawn().ok()?;
        // Wait up to 5s for the socket to come up.
        for _ in 0..50 {
            if let Some(resp) = try_daemon_inner(root, req) {
                return Some(resp);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        None
    })
}

fn try_daemon_inner(root: &Path, req: &Request) -> Option<Response> {
    let sock = daemon::socket_path(root);
    let mut stream = UnixStream::connect(&sock).ok()?;
    // The daemon drains its debounced watcher batch before serving a
    // connection; on a cold or loaded host that drain can outlast a short
    // probe timeout. 5s covers the drain without masking a dead daemon.
    stream
        .set_read_timeout(Some(Duration::from_millis(5000)))
        .ok()?;
    stream
        .set_write_timeout(Some(Duration::from_millis(5000)))
        .ok()?;
    let ping = roundtrip(&mut stream, &Request::Ping)?;
    if !ping.ok
        || ping.data().get("protocol_version").and_then(Value::as_u64) != Some(PROTOCOL_VERSION)
    {
        // Old daemons must not serve stale schemas to a newer CLI. They all
        // understand Shutdown; close them and use the current in-process
        // service for this command. A later explicit start launches current.
        let _ = roundtrip(&mut stream, &Request::Shutdown);
        return None;
    }
    // Real request may legitimately take a while (lazy graph build).
    stream
        .set_read_timeout(Some(Duration::from_secs(600)))
        .ok()?;
    stream
        .set_write_timeout(Some(Duration::from_secs(30)))
        .ok()?;
    roundtrip(&mut stream, req)
}

/// Check if an env var is explicitly set to "0"/"false"/"off".
fn env_flag_off(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| matches!(v.as_str(), "0" | "false" | "off"))
}

/// Prefer the daemon; fall back to an in-process Service. The given path may
/// be anywhere inside a repo — the root is discovered automatically, so
/// pointing any command at a subdirectory or file just works.
fn execute(path: &Path, req: Request, no_daemon: bool) -> Result<Value, String> {
    let root = discover_root(path)?;
    if !no_daemon && let Some(resp) = try_daemon(&root, &req) {
        return unwrap_response(resp);
    }
    let mut svc = Service::open(&root).map_err(|e| e.to_string())?;
    unwrap_response(svc.handle(req))
}

fn unwrap_response(resp: Response) -> Result<Value, String> {
    if !resp.ok {
        return Err(resp.error_message());
    }
    // Display plumbing for the Envelope v2 honesty fields: the daemon
    // attaches `epistemics`/`snapshot`/`warnings` at the ENVELOPE level, but
    // the CLI historically prints only the result payload — which would
    // silently strip the completeness contract. Fold them into the printed
    // object (never clobbering a same-named key an op itself emitted) so
    // every cap surfaces in what the caller actually sees.
    let Response {
        snapshot,
        epistemics,
        warnings,
        result,
        ..
    } = resp;
    let mut data = result.unwrap_or(Value::Null);
    if let Some(obj) = data.as_object_mut() {
        if let Some(e) = epistemics
            && !obj.contains_key("epistemics")
        {
            obj.insert(
                "epistemics".into(),
                serde_json::to_value(e).unwrap_or(Value::Null),
            );
        }
        if let Some(s) = snapshot
            && !obj.contains_key("snapshot")
        {
            obj.insert(
                "snapshot".into(),
                serde_json::to_value(s).unwrap_or(Value::Null),
            );
        }
        if !warnings.is_empty() && !obj.contains_key("warnings") {
            obj.insert(
                "warnings".into(),
                serde_json::to_value(warnings).unwrap_or(Value::Null),
            );
        }
    }
    Ok(data)
}

fn announce_graph_build(data: &Value) {
    if let Some(info) = data.get("graph_build") {
        eprintln!("pixel: {}", graph_build_notice(info));
    }
}

/// One stderr line per graph build/update, naming which path ran: an agent
/// that edits then checks `impact` must be able to tell a 1-second
/// incremental update from a 100-second rebuild of the whole tree.
fn graph_build_notice(info: &Value) -> String {
    let ms = info.get("build_ms").and_then(Value::as_u64).unwrap_or(0);
    if info.get("incremental").and_then(Value::as_bool) == Some(true) {
        let changed = info
            .get("changed_files")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let removed = info
            .get("removed_files")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let removed = if removed > 0 {
            format!(", {removed} removed")
        } else {
            String::new()
        };
        return format!("updated graph.db for {changed} changed file(s){removed} ({ms} ms)");
    }
    match info.get("reason").and_then(Value::as_str) {
        Some("threshold") => {
            format!("rebuilt graph.db: drift above PIXEL_GRAPH_INCREMENTAL_MAX_PCT ({ms} ms)")
        }
        Some("incremental_failed") => format!(
            "rebuilt graph.db: incremental update failed ({}) ({ms} ms)",
            info.get("incremental_error")
                .and_then(Value::as_str)
                .unwrap_or("unknown error")
        ),
        Some("no_signature") | Some("unreadable") => {
            format!("rebuilt graph.db: no trusted freshness signature ({ms} ms)")
        }
        _ => format!("built graph.db on first use ({ms} ms)"),
    }
}

fn write_stdout(text: &str) -> Result<(), String> {
    match operation_metrics::Stdout(std::io::stdout().lock()).write_all(text.as_bytes()) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        Err(error) => Err(format!("write stdout: {error}")),
    }
}

/// Whether the parsed invocation asked for JSON on stdout. Read from the
/// parsed matches, never from argv: `--json` is a flag only where a command
/// declares it. The walk reaches the deepest subcommand so a nested command
/// (`list-errors last --json`) answers for its own flag, and a command with no
/// `json` argument reads as `false`.
fn json_requested(matches: &ArgMatches) -> bool {
    let mut deepest = matches;
    while let Some((_, sub)) = deepest.subcommand() {
        deepest = sub;
    }
    deepest
        .try_get_one::<bool>("json")
        .ok()
        .flatten()
        .copied()
        .unwrap_or(false)
}

/// Write the `--json` failure envelope (`ok: false` + `error.code`) to stdout:
/// the parsable counterpart of the `pixel: <reason>` line on stderr. `op`
/// names the command the caller ran (the CLI's own label), not the daemon's
/// wire op. Built through the daemon's own `failure_response`, so a CLI-side
/// failure (a bad path, a refused flag combination) and a failure the daemon
/// already classified answer with the same code, from the same classifier.
///
/// Best-effort by design: a closed or full stdout must never change the exit
/// status. It bypasses `print_data`'s rendering cap on purpose — a truncated
/// failure envelope would be invalid JSON, worse than no cap at all.
fn write_failure_envelope(op: &str, message: &str) {
    let envelope: Response = failure_response(op, message);
    if let Ok(line) = serde_json::to_string(&envelope) {
        let _ = write_stdout(&format!("{line}\n"));
    }
}

/// Whether a failing command must answer with the failure envelope on stdout:
/// `--json` was asked for, the command does not own stdout, and it has not
/// answered yet. A command that owns stdout (`search-like-rg`'s passthrough,
/// the provider hook contracts, the statusline, a foreground daemon) keeps it,
/// and so does one that already wrote a document: `check-release --json`
/// reports its own failures as a document and exits 1, where a second JSON
/// line would break a reader that parses the stream as one answer.
fn failure_envelope_wanted(matches: &ArgMatches, protected: bool) -> bool {
    if protected || operation_metrics::stdout_bytes() > 0 {
        return false;
    }
    json_requested(matches)
}

/// Global stdout byte cap for `print_data` — the last-chance safety net
/// before pixel output hits the agent's context window. Individual commands
/// have their own (smaller) caps, but any command that routes through
/// `print_data` without its own cap would dump unlimited bytes to stdout.
/// At ~4 chars/token, 256KB ≈ 64K tokens ≈ $0.20 on Claude Sonnet. The
/// agent sees a `truncated` flag in the output and can page or narrow.
/// `PIXEL_OUTPUT_CAP_BYTES` overrides it (see [`stdout_byte_cap`]).
const STDOUT_BYTE_CAP: usize = 256 * 1024;

/// Effective stdout cap: `PIXEL_OUTPUT_CAP_BYTES=<bytes>` overrides the
/// default and `0` lifts the cap entirely (the same convention as
/// `PIXEL_INDEX_BUDGET_MS=0`). Unset, empty or unparsable values keep the
/// default so a typo can never silently disable the safety net.
fn stdout_byte_cap() -> usize {
    parse_output_cap(std::env::var("PIXEL_OUTPUT_CAP_BYTES").ok().as_deref())
}

fn parse_output_cap(raw: Option<&str>) -> usize {
    match raw.map(|v| v.trim().parse::<usize>()) {
        Some(Ok(0)) => usize::MAX,
        Some(Ok(n)) => n,
        _ => STDOUT_BYTE_CAP,
    }
}

fn print_data(data: &Value, raw_json: bool) -> Result<(), String> {
    let rendered = render_data(data, raw_json, stdout_byte_cap());
    if rendered.truncated {
        // Never count evidence hidden behind the final rendering cap.
        operation_metrics::unavailable();
    } else {
        operation_metrics::observe(data);
    }
    write_stdout(&rendered.text)
}

/// Output of [`render_data`]: the bytes for stdout plus whether the cap
/// fired (so metrics can refuse to count evidence the caller never saw).
struct Rendered {
    text: String,
    truncated: bool,
}

/// One array shortened by [`truncate_structurally`]: dotted path from the
/// document root (`snapshot.dirty`, `matches[3].lines`), elements kept, and
/// the original length.
#[derive(Debug)]
struct TruncatedArray {
    path: String,
    kept: usize,
    total: usize,
}

/// Bytes reserved for the metadata [`truncate_structurally`] splices into the
/// top-level object (`truncated`, `cap_bytes`, `truncated_arrays`).
const TRUNCATION_META_RESERVE: usize = 256;

/// Most structural-truncation rounds before giving up: each round shortens
/// the (then) largest array, so a handful of rounds covers every realistic
/// response shape without letting a pathological document spin.
const TRUNCATION_MAX_ROUNDS: usize = 8;

fn serialized_len(v: &Value) -> usize {
    serde_json::to_vec(v).map_or(0, |b| b.len())
}

/// Locate the non-empty array under `v` whose tail is worth cutting most:
/// ranked by REMOVABLE bytes, `bytes - bytes / len` (everything past a
/// mean-sized first element), not raw size. Raw size would always pick an
/// enclosing array over the long list nested inside it (the parent is never
/// smaller than its child) and drop whole sibling records instead of
/// trimming the list. Returns `(path, len, bytes)`.
fn largest_array(v: &Value, path: &str) -> Option<(String, usize, usize)> {
    let mut best: Option<(String, usize, usize)> = None;
    let removable = |c: &(String, usize, usize)| c.2 - c.2 / c.1;
    let mut consider = |cand: Option<(String, usize, usize)>| {
        if let Some(c) = cand
            && best.as_ref().is_none_or(|b| removable(&c) > removable(b))
        {
            best = Some(c);
        }
    };
    match v {
        Value::Array(items) => {
            if !items.is_empty() {
                consider(Some((path.to_string(), items.len(), serialized_len(v))));
            }
            for (i, item) in items.iter().enumerate() {
                consider(largest_array(item, &format!("{path}[{i}]")));
            }
        }
        Value::Object(map) => {
            for (k, item) in map {
                let child = if path.is_empty() {
                    k.clone()
                } else {
                    format!("{path}.{k}")
                };
                consider(largest_array(item, &child));
            }
        }
        _ => {}
    }
    best
}

/// Walk `v` to the array at the dotted `path` produced by [`largest_array`].
fn array_at_mut<'a>(v: &'a mut Value, path: &str) -> Option<&'a mut Vec<Value>> {
    let mut cur = v;
    if !path.is_empty() {
        for seg in path.split('.') {
            let (key, indexes) = match seg.find('[') {
                Some(i) => (&seg[..i], &seg[i..]),
                None => (seg, ""),
            };
            if !key.is_empty() {
                cur = cur.get_mut(key)?;
            }
            for idx in indexes
                .trim_start_matches('[')
                .trim_end_matches(']')
                .split("][")
                .filter(|s| !s.is_empty())
            {
                cur = cur.get_mut(idx.parse::<usize>().ok()?)?;
            }
        }
    }
    cur.as_array_mut()
}

/// Shorten the largest arrays in `data` until its compact serialization fits
/// `cap`, keeping every other field and the document structure intact, then
/// stamp the top-level object with `truncated: true`, `cap_bytes` and a
/// `truncated_arrays` list naming each cut (path, kept, total).
///
/// Returns `None` when `data` is not an object or no amount of array
/// trimming gets it under the cap (for example when the bulk is one huge
/// string), in which case the caller falls back to the textual wrapper.
fn truncate_structurally(data: &Value, cap: usize) -> Option<Value> {
    if !data.is_object() {
        return None;
    }
    let mut doc = data.clone();
    let mut cuts: Vec<TruncatedArray> = Vec::new();
    let budget = cap.saturating_sub(TRUNCATION_META_RESERVE);
    for _ in 0..TRUNCATION_MAX_ROUNDS {
        let size = serialized_len(&doc);
        if size <= budget {
            break;
        }
        let (path, len, bytes) = largest_array(&doc, "")?;
        // Elements that fit: the budget left after everything outside this
        // array, divided by the mean element size — always strictly fewer
        // than `len` so every round makes progress.
        let outside = size.saturating_sub(bytes);
        let per_elem = (bytes / len).max(1);
        let keep = budget
            .saturating_sub(outside)
            .checked_div(per_elem)
            .unwrap_or(0)
            .min(len - 1);
        let arr = array_at_mut(&mut doc, &path)?;
        arr.truncate(keep);
        match cuts.iter_mut().find(|c| c.path == path) {
            Some(c) => c.kept = keep,
            None => cuts.push(TruncatedArray {
                path,
                kept: keep,
                total: len,
            }),
        }
    }
    if serialized_len(&doc) > budget {
        return None;
    }
    let meta: Vec<Value> = cuts
        .iter()
        .map(|c| json!({"path": c.path, "kept": c.kept, "total": c.total}))
        .collect();
    let obj = doc.as_object_mut()?;
    obj.insert("truncated".into(), json!(true));
    obj.insert("cap_bytes".into(), json!(cap));
    obj.insert("truncated_arrays".into(), Value::Array(meta));
    (serialized_len(&doc) <= cap).then_some(doc)
}

/// Serialize `data` for stdout under a byte cap.
///
/// Human mode (`raw_json == false`) pretty-prints and, when over the cap,
/// cuts the text on a char boundary and appends a visible truncation note.
///
/// JSON mode (`raw_json == true`) must never emit anything that is not one
/// JSON document: a caller doing `serde_json::from_slice(stdout)` cannot
/// recover from a cut-off object followed by prose. When the compact
/// serialization exceeds the cap, the document is first truncated
/// STRUCTURALLY ([`truncate_structurally`]): the largest arrays are
/// shortened, every other field survives, and the object gains
/// `truncated: true`, `cap_bytes` and `truncated_arrays`, so `jq '.index'`
/// keeps working on a capped response. Only when no array trimming can fit
/// the cap does the output fall back to the small wrapper object
/// `{truncated: true, cap_bytes, note, partial}` where `partial` is the
/// leading bytes of the original serialization as a string. The wrapper
/// itself can exceed the cap by the size of the note and JSON escaping;
/// that is bounded and preferable to invalid output.
fn render_data(data: &Value, raw_json: bool, cap: usize) -> Rendered {
    let mut output = if raw_json {
        serde_json::to_string(data).unwrap_or_default()
    } else {
        serde_json::to_string_pretty(data).unwrap_or_default()
    };
    let truncated = output.len() > cap;
    if truncated
        && raw_json
        && let Some(doc) = truncate_structurally(data, cap)
    {
        output = serde_json::to_string(&doc).unwrap_or_default();
    } else if truncated {
        let mut end = cap;
        while end > 0 && !output.is_char_boundary(end) {
            end -= 1;
        }
        let note = format!(
            "OUTPUT TRUNCATED AT {cap} BYTES (global safety cap). The full \
             response was larger — re-run with a narrower scope, --limit, or \
             --offset to page through results. Remove .pixel/calls.json if \
             the circuit breaker fires."
        );
        output.truncate(end);
        if raw_json {
            output = serde_json::to_string(&json!({
                "truncated": true,
                "cap_bytes": cap,
                "note": note,
                "partial": output,
            }))
            .unwrap_or_default();
        } else {
            output.push_str("\n\n⚠ ");
            output.push_str(&note);
        }
    }
    output.push('\n');
    Rendered {
        text: output,
        truncated,
    }
}

#[cfg(test)]
mod render_data_tests {
    use super::*;

    fn big() -> Value {
        json!({"matches": (0..200).map(|i| json!({"path": format!("src/file_{i}.rs"), "line": i, "text": "é".repeat(20)})).collect::<Vec<_>>()})
    }

    #[test]
    fn under_cap_is_untouched() {
        let d = json!({"a": 1});
        let r = render_data(&d, true, 1024);
        assert_eq!(r.text, "{\"a\":1}\n");
        assert!(!r.truncated);
        assert_eq!(
            serde_json::from_str::<Value>(&render_data(&d, false, 1024).text).unwrap(),
            d
        );
    }

    /// The reason this matters: agents call `pixel … --json` and parse
    /// stdout, then `jq '.index'` / `.graph` on the result. A response that
    /// is over the cap only because ONE list is long (a `snapshot.dirty`
    /// full of untracked `vendor/bundle` paths) must keep its shape: the
    /// scalar fields stay addressable, only the list is shortened, and the
    /// document says which list was cut and how much survived.
    #[test]
    fn json_mode_truncation_shortens_the_largest_array_and_keeps_structure() {
        let d = json!({
            "index": {"base_files": 238, "commit_oid": "abc"},
            "snapshot": {"head": "abc", "branch": "main",
                          "dirty": (0..500).map(|i| format!("vendor/bundle/gems/g{i}/lib/x.rb")).collect::<Vec<_>>()},
        });
        let full = serde_json::to_string(&d).unwrap().len();
        let cap = full / 3;
        let r = render_data(&d, true, cap);
        assert!(r.truncated);
        assert!(r.text.len() <= cap + 1, "{} > cap {cap}", r.text.len());
        let v: Value = serde_json::from_str(&r.text).expect("stdout must remain one JSON document");
        assert_eq!(v["truncated"], true);
        assert_eq!(v["cap_bytes"], cap);
        assert_eq!(
            v["index"]["base_files"], 238,
            "untouched fields must survive"
        );
        assert_eq!(v["snapshot"]["branch"], "main");
        let dirty = v["snapshot"]["dirty"].as_array().unwrap();
        assert!(
            !dirty.is_empty() && dirty.len() < 500,
            "kept {}",
            dirty.len()
        );
        assert_eq!(
            dirty[0], "vendor/bundle/gems/g0/lib/x.rb",
            "prefix, not a sample"
        );
        let cuts = v["truncated_arrays"].as_array().unwrap();
        assert_eq!(cuts.len(), 1);
        assert_eq!(cuts[0]["path"], "snapshot.dirty");
        assert_eq!(cuts[0]["total"], 500);
        assert_eq!(cuts[0]["kept"], dirty.len());
        assert!(
            v.get("partial").is_none(),
            "no textual wrapper when structure fits"
        );
        assert_eq!(r.text.matches('\n').count(), 1, "single NDJSON-safe line");
    }

    /// Nested arrays: the cut lands on the array that actually carries the
    /// bytes, not blindly on the top-level one, and the path names it.
    #[test]
    fn structural_truncation_targets_nested_array_by_size() {
        let d = json!({"groups": [
            {"name": "small", "items": ["a", "b"]},
            {"name": "huge", "items": (0..2000).map(|i| format!("item-{i:05}")).collect::<Vec<_>>()},
        ]});
        let r = render_data(&d, true, 2000);
        let v: Value = serde_json::from_str(&r.text).unwrap();
        assert_eq!(
            v["groups"].as_array().unwrap().len(),
            2,
            "outer array intact"
        );
        assert_eq!(v["groups"][0]["items"].as_array().unwrap().len(), 2);
        assert!(v["groups"][1]["items"].as_array().unwrap().len() < 2000);
        assert_eq!(v["truncated_arrays"][0]["path"], "groups[1].items");
        assert!(r.text.len() <= 2001);
    }

    /// When the bulk is not an array (one huge string) structural trimming
    /// cannot help; the textual wrapper must still be one valid document
    /// that says it was cut.
    #[test]
    fn json_mode_falls_back_to_wrapper_when_no_array_can_be_cut() {
        let d = json!({"blob": "x".repeat(5000)});
        let r = render_data(&d, true, 500);
        assert!(r.truncated);
        let v: Value = serde_json::from_str(&r.text).expect("stdout must remain one JSON document");
        assert_eq!(v["truncated"], true);
        assert_eq!(v["cap_bytes"], 500);
        let partial = v["partial"].as_str().unwrap();
        assert!(partial.len() <= 500);
        assert!(partial.starts_with("{\"blob\":\""));
        assert!(v["note"].as_str().unwrap().contains("TRUNCATED"));
        assert_eq!(r.text.matches('\n').count(), 1, "single NDJSON-safe line");
    }

    #[test]
    fn json_mode_array_of_objects_is_cut_structurally() {
        let r = render_data(&big(), true, 500);
        let v: Value = serde_json::from_str(&r.text).unwrap();
        assert_eq!(v["truncated"], true);
        assert!(v["matches"].is_array());
        assert_eq!(v["truncated_arrays"][0]["path"], "matches");
        assert_eq!(v["truncated_arrays"][0]["total"], 200);
    }

    #[test]
    fn human_mode_truncation_keeps_visible_note() {
        let r = render_data(&big(), false, 500);
        assert!(r.truncated);
        assert!(r.text.contains("⚠ OUTPUT TRUNCATED AT 500 BYTES"));
        assert!(serde_json::from_str::<Value>(&r.text).is_err());
    }

    /// Multi-byte text near the cap: the cut must land on a char boundary
    /// so the partial string is valid UTF-8 and serializable.
    #[test]
    fn truncation_respects_char_boundaries() {
        let d = json!({"t": "é".repeat(1000)});
        for cap in 100..140 {
            let out = render_data(&d, true, cap).text;
            let v: Value = serde_json::from_str(&out).unwrap();
            assert!(v["partial"].as_str().unwrap().len() <= cap);
        }
    }

    /// `array_at_mut` must round-trip every path shape `largest_array`
    /// emits, otherwise a cut silently targets nothing.
    #[test]
    fn array_path_round_trip() {
        let mut d = json!({"a": {"b": [[1, 2, 3], {"c": [4, 5]}]}, "d": [6]});
        let (path, len, _) = largest_array(&d, "").unwrap();
        assert_eq!(
            path, "a.b",
            "21 bytes / 2 elems: 11 removable, beats a.b[0]'s 5"
        );
        assert_eq!(len, 2);
        assert_eq!(array_at_mut(&mut d, "a.b[0]").unwrap().len(), 3);
        assert_eq!(array_at_mut(&mut d, "a.b[1].c").unwrap().len(), 2);
        assert_eq!(array_at_mut(&mut d, "d").unwrap().len(), 1);
        assert!(array_at_mut(&mut d, "a.b[5]").is_none());
    }

    /// `PIXEL_OUTPUT_CAP_BYTES=0` is the documented escape hatch for a
    /// consumer that wants the whole document; anything unparsable must
    /// keep the safety net rather than silently disabling it.
    #[test]
    fn output_cap_env_parsing() {
        assert_eq!(parse_output_cap(None), STDOUT_BYTE_CAP);
        assert_eq!(parse_output_cap(Some("0")), usize::MAX);
        assert_eq!(parse_output_cap(Some(" 4096 ")), 4096);
        assert_eq!(parse_output_cap(Some("lots")), STDOUT_BYTE_CAP);
        assert_eq!(parse_output_cap(Some("")), STDOUT_BYTE_CAP);
    }

    /// `status`/`ready` are freshness answers: the dirty LIST is what let an
    /// untracked vendor tree blow the cap, the COUNT is all they need.
    /// The stderr line is how an agent tells a 1 s incremental update from
    /// a 100 s rebuild of the whole tree; the two must not read the same.
    #[test]
    fn graph_build_notice_distinguishes_incremental_from_full() {
        let incremental =
            json!({"incremental": true, "changed_files": 2, "removed_files": 0, "build_ms": 1200});
        assert_eq!(
            graph_build_notice(&incremental),
            "updated graph.db for 2 changed file(s) (1200 ms)"
        );
        let with_removed =
            json!({"incremental": true, "changed_files": 1, "removed_files": 1, "build_ms": 40});
        assert_eq!(
            graph_build_notice(&with_removed),
            "updated graph.db for 1 changed file(s), 1 removed (40 ms)"
        );
        let first = json!({"incremental": false, "reason": "missing", "build_ms": 36000});
        assert_eq!(
            graph_build_notice(&first),
            "built graph.db on first use (36000 ms)"
        );
        let threshold = json!({"incremental": false, "reason": "threshold", "build_ms": 5});
        assert!(graph_build_notice(&threshold).contains("PIXEL_GRAPH_INCREMENTAL_MAX_PCT"));
        // Older daemon without the field: still the first-use wording.
        assert_eq!(
            graph_build_notice(&json!({"build_ms": 7})),
            "built graph.db on first use (7 ms)"
        );
    }

    #[test]
    fn compact_snapshot_replaces_dirty_list_with_count() {
        let mut d = json!({"index": {"base_files": 1},
            "snapshot": {"head": "abc", "branch": "main", "dirty": ["a", "b", "c"]}});
        compact_snapshot(&mut d);
        assert_eq!(d["snapshot"]["dirty_count"], 3);
        assert!(d["snapshot"].get("dirty").is_none());
        assert_eq!(d["snapshot"]["head"], "abc");
        assert_eq!(d["index"]["base_files"], 1);
        // No snapshot (older daemon / in-process service without one): no-op.
        let mut bare = json!({"index": {}});
        compact_snapshot(&mut bare);
        assert_eq!(bare, json!({"index": {}}));
    }

    #[test]
    fn compact_repo_state_drops_the_clean_list_and_keeps_the_count() {
        let mut d = json!({"root": "/repo", "head": "abc", "branch": "main",
            "dirty": [], "dirty_count": 0,
            "clean": ["a", "b"], "clean_count": 2,
            "clean_list_truncated": false, "clean_list_cap": 200});
        compact_repo_state(&mut d);
        assert!(d.get("clean").is_none(), "{d}");
        assert!(d.get("clean_list_truncated").is_none(), "{d}");
        assert!(d.get("clean_list_cap").is_none(), "{d}");
        assert_eq!(d["clean_count"], 2);
        assert_eq!(d["head"], "abc");
        // A non-object (never produced today): no-op, no panic.
        let mut bare = json!([]);
        compact_repo_state(&mut bare);
        assert_eq!(bare, json!([]));
    }
}

/// Shared graph-command epilogue: candidates protocol + build announcement.
fn finish_graph_cmd(
    data: Value,
    raw_json: bool,
    pretty: impl Fn(&Value) -> Option<String>,
) -> Result<(), String> {
    announce_graph_build(&data);
    if raw_json {
        return print_data(&data, true);
    }
    if let Some(cands) = data.get("candidates").and_then(Value::as_array) {
        operation_metrics::observe(&json!({"candidates": cands}));
        eprintln!("ambiguous name — re-run with one of these uids:");
        let mut output = String::new();
        for c in cands {
            output.push_str(&format!(
                "  {}  ({} {}:{})\n",
                c.get("uid").and_then(Value::as_str).unwrap_or("?"),
                c.get("kind").and_then(Value::as_str).unwrap_or("?"),
                c.get("path").and_then(Value::as_str).unwrap_or("?"),
                c.get("start_line").and_then(Value::as_u64).unwrap_or(0),
            ));
        }
        return write_stdout(&output);
    }
    if let Some(output) = pretty(&data) {
        operation_metrics::observe(&data);
        write_stdout(&output)
    } else {
        print_data(&data, false)
    }
}

fn symbol_line(s: &Value) -> String {
    format!(
        "{:<9} {}  {}:{}-{}  {}",
        s.get("kind").and_then(Value::as_str).unwrap_or("?"),
        s.get("name").and_then(Value::as_str).unwrap_or("?"),
        s.get("path").and_then(Value::as_str).unwrap_or("?"),
        s.get("start_line").and_then(Value::as_u64).unwrap_or(0),
        s.get("end_line").and_then(Value::as_u64).unwrap_or(0),
        s.get("uid").and_then(Value::as_str).unwrap_or("?"),
    )
}

/// Tiered pretty rendering for `targets`.
fn pretty_targets(d: &Value) -> Option<String> {
    let targets = d.get("targets")?.as_array()?;
    let mut output = String::new();
    for (tier, title) in [
        ("P0", "P0 — primary (start here)"),
        ("P1", "P1 — likely needed"),
        ("P2", "P2 — peripheral (droppable)"),
    ] {
        let group: Vec<&Value> = targets.iter().filter(|t| t["tier"] == tier).collect();
        if group.is_empty() {
            continue;
        }
        output.push_str(title);
        output.push('\n');
        for t in group {
            output.push_str(&format!(
                "  {:<50} {:.6}\n",
                t.get("path").and_then(Value::as_str).unwrap_or("?"),
                t.get("score").and_then(Value::as_f64).unwrap_or(0.0),
            ));
            if let Some(notes) = t.get("notes").and_then(Value::as_array) {
                for n in notes {
                    output.push_str(&format!(
                        "      note [{}]: {}\n",
                        n.get("target").and_then(Value::as_str).unwrap_or("?"),
                        n.get("note").and_then(Value::as_str).unwrap_or(""),
                    ));
                }
            }
            if let Some(reasons) = t.get("reasons").and_then(Value::as_array) {
                for r in reasons {
                    output.push_str(&format!("      {}\n", r.as_str().unwrap_or("")));
                }
            }
        }
    }
    let limit = d
        .get("stats")
        .and_then(|s| s.get("limit"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    output.push_str(&format!(
        "closed list: {} files (limit {limit})\n",
        targets.len()
    ));
    if let Some(cw) = d.get("closed_world").and_then(Value::as_str) {
        output.push_str(cw);
        output.push('\n');
    }
    envelope_note(d);
    Some(output)
}

/// TTL for a scoped task inside the targets manifest — matches the guard's
/// `MANIFEST_MAX_AGE_SECS` (crates/pixel/src/guard.rs).
const TARGETS_TTL_SECS: u64 = 24 * 3600;

/// Maximum concurrent tasks in the manifest. Without a cap, an agent that
/// runs `pixel targets` repeatedly without `--clear` stacks tasks
/// indefinitely, growing the manifest file and confusing the edit guard
/// (which may use the wrong task's scoping). The cap evicts the OLDEST
/// tasks first — newest wins because the agent's current task is the one
/// it just ran.
const MAX_MANIFEST_TASKS: usize = 8;

/// Stable short id for a task string (FNV-1a 64, hex). Deliberately NOT
/// `DefaultHasher` — the id must survive across pixel builds so a re-run of
/// the same task replaces its own entry instead of appending a duplicate.
fn targets_task_id(task: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in task.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")[..12].to_string()
}

/// Merge a new task entry into an existing manifest (v2 multi-task, legacy
/// single-task, or absent/corrupt), producing the v2 shape:
/// `{version: 2, tasks: [{id, task, created_unix, targets: [...]}]}`.
/// Tasks older than the 24h TTL are dropped; a task with the same id as the
/// new one is replaced. Pure function — file I/O stays in the caller.
fn merge_targets_manifest(existing: Option<&str>, new_task: Value, now: u64) -> Value {
    let new_id = new_task
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let mut tasks: Vec<Value> = Vec::new();
    if let Some(text) = existing
        && let Ok(v) = serde_json::from_str::<Value>(text)
    {
        if v.get("version").and_then(Value::as_u64) == Some(2) {
            tasks = v
                .get("tasks")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
        } else if let Some(old_task) = v.get("task").and_then(Value::as_str) {
            // Legacy single-task shape — wrap it as one v2 task so a
            // concurrent agent's active scope survives this write.
            tasks = vec![serde_json::json!({
                "id": targets_task_id(old_task),
                "task": old_task,
                "created_unix": v.get("created_unix").cloned().unwrap_or(Value::Null),
                "head_oid": v.get("head_oid").cloned().unwrap_or(Value::Null),
                "limit": v.get("limit").cloned().unwrap_or(Value::Null),
                "targets": v.get("files").cloned().unwrap_or_else(|| Value::Array(vec![])),
            })];
        }
    }
    tasks.retain(|t| {
        let created = t.get("created_unix").and_then(Value::as_u64).unwrap_or(0);
        let id = t.get("id").and_then(Value::as_str).unwrap_or("");
        now.saturating_sub(created) <= TARGETS_TTL_SECS && id != new_id
    });
    tasks.push(new_task);
    // Enforce the concurrent-task cap. Tasks are sorted by created_unix
    // ascending (oldest first in the vec after the retain+push). If we're
    // over the cap, drop the oldest entries until we're back under it.
    // This prevents unbounded manifest growth from agents that run
    // `pixel targets` repeatedly without `--clear`.
    if tasks.len() > MAX_MANIFEST_TASKS {
        tasks.sort_by_key(|t| t.get("created_unix").and_then(Value::as_u64).unwrap_or(0));
        let overflow = tasks.len() - MAX_MANIFEST_TASKS;
        tasks.drain(0..overflow);
    }
    serde_json::json!({ "version": 2, "tasks": tasks })
}

/// Write the enforcement manifest atomically (tmp + rename), merging into
/// any manifest already on disk so concurrent agents' tasks coexist instead
/// of clobbering each other. Returns the number of active tasks.
fn write_targets_manifest(manifest_path: &Path, task: &str, data: &Value) -> Result<usize, String> {
    let files: Vec<Value> = data
        .get("targets")
        .and_then(Value::as_array)
        .map(|ts| {
            ts.iter()
                .map(|t| {
                    serde_json::json!({
                        "path": t.get("path").cloned().unwrap_or(Value::Null),
                        "tier": t.get("tier").cloned().unwrap_or(Value::Null),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let created_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let new_task = serde_json::json!({
        "id": targets_task_id(task),
        "task": task,
        "created_unix": created_unix,
        "head_oid": data
            .get("stats")
            .and_then(|s| s.get("commit_oid"))
            .cloned()
            .unwrap_or(Value::Null),
        "limit": data
            .get("stats")
            .and_then(|s| s.get("limit"))
            .cloned()
            .unwrap_or(Value::Null),
        "targets": files,
    });
    let existing = std::fs::read_to_string(manifest_path).ok();
    let manifest = merge_targets_manifest(existing.as_deref(), new_task, created_unix);
    let active = manifest
        .get("tasks")
        .and_then(Value::as_array)
        .map_or(1, Vec::len);
    if let Some(parent) = manifest_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    let tmp = manifest_path.with_extension("json.tmp");
    std::fs::write(
        &tmp,
        serde_json::to_vec_pretty(&manifest).unwrap_or_default(),
    )
    .map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, manifest_path)
        .map_err(|e| format!("publish {}: {e}", manifest_path.display()))?;
    Ok(active)
}

#[cfg(test)]
mod targets_manifest_tests {
    use super::*;

    fn task_entry(id_src: &str, created: u64, path: &str) -> Value {
        serde_json::json!({
            "id": targets_task_id(id_src),
            "task": id_src,
            "created_unix": created,
            "targets": [{"path": path, "tier": "P0"}],
        })
    }

    #[test]
    fn merge_two_tasks_coexist() {
        let now = 1_000_000;
        let v = merge_targets_manifest(None, task_entry("task A", now, "src/a.rs"), now);
        let text = v.to_string();
        let v2 = merge_targets_manifest(Some(&text), task_entry("task B", now, "src/b.rs"), now);
        let tasks = v2["tasks"].as_array().unwrap();
        assert_eq!(v2["version"], 2);
        assert_eq!(tasks.len(), 2, "concurrent tasks must both survive");
        let names: Vec<&str> = tasks.iter().map(|t| t["task"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["task A", "task B"]);
    }

    #[test]
    fn merge_replaces_same_task_id() {
        let now = 1_000_000;
        let v = merge_targets_manifest(None, task_entry("task A", now - 100, "src/old.rs"), now);
        let text = v.to_string();
        let v2 = merge_targets_manifest(Some(&text), task_entry("task A", now, "src/new.rs"), now);
        let tasks = v2["tasks"].as_array().unwrap();
        assert_eq!(tasks.len(), 1, "same task id must replace, not append");
        assert_eq!(tasks[0]["targets"][0]["path"], "src/new.rs");
    }

    #[test]
    fn merge_drops_expired_tasks() {
        let now = 1_000_000_000;
        let old = merge_targets_manifest(
            None,
            task_entry("stale task", now - TARGETS_TTL_SECS - 1, "src/stale.rs"),
            now - TARGETS_TTL_SECS - 1,
        );
        let text = old.to_string();
        let v2 = merge_targets_manifest(
            Some(&text),
            task_entry("fresh task", now, "src/fresh.rs"),
            now,
        );
        let tasks = v2["tasks"].as_array().unwrap();
        assert_eq!(tasks.len(), 1, "expired task must be dropped on merge");
        assert_eq!(tasks[0]["task"], "fresh task");
    }

    #[test]
    fn merge_wraps_legacy_singleton() {
        let now = 1_000_000;
        let legacy = serde_json::json!({
            "version": 1,
            "task": "legacy task",
            "created_unix": now - 50,
            "head_oid": "abc",
            "limit": 20,
            "files": [{"path": "src/legacy.rs", "tier": "P0"}],
        })
        .to_string();
        let v2 = merge_targets_manifest(
            Some(&legacy),
            task_entry("new task", now, "src/new.rs"),
            now,
        );
        let tasks = v2["tasks"].as_array().unwrap();
        assert_eq!(
            tasks.len(),
            2,
            "legacy singleton must be preserved as a v2 task"
        );
        assert_eq!(tasks[0]["task"], "legacy task");
        assert_eq!(tasks[0]["targets"][0]["path"], "src/legacy.rs");
        assert_eq!(tasks[1]["task"], "new task");
    }

    #[test]
    fn merge_survives_corrupt_existing() {
        let now = 1_000_000;
        let v2 = merge_targets_manifest(
            Some("{not json"),
            task_entry("task A", now, "src/a.rs"),
            now,
        );
        assert_eq!(v2["tasks"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn task_id_stable_and_short() {
        assert_eq!(targets_task_id("x"), targets_task_id("x"));
        assert_ne!(targets_task_id("x"), targets_task_id("y"));
        assert_eq!(targets_task_id("anything").len(), 12);
    }

    #[test]
    fn merge_caps_at_max_manifest_tasks() {
        let mut now = 1_000_000u64;
        let mut text =
            merge_targets_manifest(None, task_entry("task 0", now, "src/a0.rs"), now).to_string();

        // Add MAX_MANIFEST_TASKS more tasks (total = MAX+1, should cap).
        for i in 1..=MAX_MANIFEST_TASKS {
            now += 10;
            text = merge_targets_manifest(
                Some(&text),
                task_entry(&format!("task {i}"), now, &format!("src/a{i}.rs")),
                now,
            )
            .to_string();
        }
        let v: Value = serde_json::from_str(&text).unwrap();
        let tasks = v["tasks"].as_array().unwrap();
        assert_eq!(
            tasks.len(),
            MAX_MANIFEST_TASKS,
            "manifest must be capped at MAX_MANIFEST_TASKS"
        );
        // Oldest task ("task 0") must be evicted; newest ("task {MAX}") kept.
        let names: Vec<&str> = tasks.iter().map(|t| t["task"].as_str().unwrap()).collect();
        assert!(!names.contains(&"task 0"), "oldest task must be evicted");
        assert!(
            names.contains(&format!("task {MAX_MANIFEST_TASKS}").as_str()),
            "newest task must survive"
        );
    }
}

/// Circuit-breaker guard for retrieval commands. Call at the top of
/// each guarded command handler. If the breaker fires, prints the
/// guidance message to stderr and returns `true` (caller should return
/// early with an error). If the call is allowed, returns `false`.
fn call_guard_check(command: &str, args: &str) -> bool {
    // Test mode: skip the circuit breaker entirely so integration tests
    // that call pixel search/resolve multiple times don't hit it.
    if std::env::var("PIXEL_TEST").is_ok() {
        return false;
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    match call_guard::check_and_record(command, args, &cwd) {
        call_guard::CallGuardResult::Allow => false,
        call_guard::CallGuardResult::Warn(msg) => {
            eprintln!("{msg}");
            false
        }
    }
}

fn envelope_note(data: &Value) {
    if let Some(env) = data.get("envelope")
        && env
            .get("lower_bound")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    {
        let n = env
            .get("unresolved_same_name")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        eprintln!("note: lower bound — {n} same-name call site(s) unresolved");
    }
}

// ---------------------------------------------------------------------------
// repo-root discovery
// ---------------------------------------------------------------------------

/// Walk up from `path` (file or directory) to the nearest ancestor holding a
/// `.pixel` index or a `.git` dir/file (worktrees). Falls back to the
/// starting directory. This lets every command accept a subdirectory or file
/// where an LLM would naturally point it, instead of requiring the repo root.
/// Hard deadline for the SessionStart per-repo freshness probe. The
/// capability block must reach the agent even when the probe cannot
/// complete, so the probe is bounded rather than trusted.
const SESSION_STATUS_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

pub(crate) fn discover_root(path: &Path) -> Result<PathBuf, String> {
    let abs = path
        .canonicalize()
        .map_err(|e| format!("bad path {}: {e}", path.display()))?;
    let start = if abs.is_file() {
        abs.parent().map_or_else(|| abs.clone(), Path::to_path_buf)
    } else {
        abs.clone()
    };
    // The nearest `.git` defines the repo boundary and always wins — a
    // nested `.pixel` left behind by indexing a subdirectory must never
    // shadow the real repo root. `.pixel` anchors a non-git tree only when
    // it actually holds a shard: a journal-only `.pixel` (actions/history —
    // e.g. the global `$HOME/.pixel` state dir) must never anchor, or every
    // gitless invocation below it silently re-roots to that ancestor and
    // plain-walk-indexes the entire home directory.
    let mut nearest_index: Option<PathBuf> = None;
    let mut cur = start.clone();
    loop {
        if cur.join(".git").exists() {
            return Ok(cur);
        }
        if nearest_index.is_none()
            && cur
                .join(pixel_index::index::SHARD_DIR)
                .join(pixel_index::index::SHARD_FILE)
                .is_file()
        {
            nearest_index = Some(cur.clone());
        }
        match cur.parent() {
            Some(p) => cur = p.to_path_buf(),
            None => return Ok(nearest_index.unwrap_or(start)),
        }
    }
}

// ---------------------------------------------------------------------------
// legacy index/search helpers (kept behavior)
// ---------------------------------------------------------------------------

fn make_extractor(kind: ExtractorKind, max_gram: usize) -> Box<dyn GramExtractor> {
    match kind {
        ExtractorKind::Sparse => Box::new(SparseGramExtractor::with_lengths(
            Crc32Weigher,
            pixel_index::gram::DEFAULT_MIN_GRAM,
            max_gram,
        )),
        ExtractorKind::Trigram => Box::new(TrigramExtractor),
    }
}

fn extractor_for_shard(shard: &Shard) -> Result<Box<dyn GramExtractor>, String> {
    let id = shard.extractor_id();
    if id == "trigram" {
        return Ok(Box::new(TrigramExtractor));
    }
    if let Some(rest) = id.strip_prefix("sparse-crc32-")
        && let Some((min, max)) = rest.split_once('-')
        && let (Ok(min), Ok(max)) = (min.parse::<usize>(), max.parse::<usize>())
    {
        return Ok(Box::new(SparseGramExtractor::with_lengths(
            Crc32Weigher,
            min,
            max,
        )));
    }
    Err(format!(
        "index built with unsupported extractor {id:?}; re-run `pixel build-index`"
    ))
}

/// One stdout line for a `search` match: the compact object in `--json`
/// mode (with `context` when the CLI enriched the match), else the
/// `path:line:text` row — or the `--- path:line ---` block once the match
/// carries surrounding lines.
fn search_match_line(m: &Value, json: bool) -> String {
    let path = m.get("path").and_then(Value::as_str).unwrap_or("");
    let line = m.get("line").and_then(Value::as_u64).unwrap_or(0);
    let text = m.get("text").and_then(Value::as_str).unwrap_or("");
    let context = m.get("context").and_then(Value::as_str);
    if json {
        let mut entry = serde_json::json!({"path": path, "line": line, "text": text});
        if let Some(ctx) = context {
            entry["context"] = Value::String(ctx.to_string());
        }
        format!("{entry}\n")
    } else if let Some(ctx) = context {
        format!("--- {path}:{line} ---\n{ctx}\n")
    } else {
        format!("{path}:{line}:{text}\n")
    }
}

/// The NDJSON line a `--json` search page ends with: the page state no match
/// line carries, because a page cut by a cap is otherwise byte-for-byte a
/// complete answer. `epistemics`/`warnings` are the envelope honesty fields
/// [`unwrap_response`] folded into the response. Every key is always present
/// (`null` for the absent ones), so the trailer's size is bounded by its
/// values alone.
fn search_meta(data: &Value, truncated: bool, next_offset: Option<u64>) -> Value {
    json!({
        "truncated": truncated,
        "next_offset": next_offset,
        "epistemics": data.get("epistemics").cloned().unwrap_or(Value::Null),
        "warnings": data.get("warnings").cloned().unwrap_or(Value::Null),
    })
}

/// Whether one more `line` fits: what is already assembled plus the line plus
/// the room reserved for the metadata line stays within `cap`. Equality fits
/// — the cap is a ceiling, not a limit one byte under it.
fn fits_before_cap(assembled: usize, line: usize, reserve: usize, cap: usize) -> bool {
    assembled + line + reserve <= cap
}

/// What one `search` page wrote: `printed` match lines out of the daemon's
/// page, whether the page is partial (`truncated`, whether the daemon's own
/// caps or the stdout cap cut it), and whether the stdout cap — not the
/// daemon — is what cut it, so the caller names the right cap on stderr.
struct SearchPage {
    printed: usize,
    truncated: bool,
    cap_fired: bool,
}

/// Print a `search` page and report what reached stdout.
///
/// `--json` output is NDJSON: one match per line, then the [`search_meta`]
/// line, so no reader has to guess whether a short page is the whole answer.
/// The global stdout cap is enforced here, during assembly: `--context` text
/// is added by the CLI, after the daemon's own byte cap, so this is the last
/// place that can hold one.
fn print_search_matches(data: &Value, matches: &[Value], json: bool) -> Result<SearchPage, String> {
    let cap = stdout_byte_cap();
    let offset = data.get("offset").and_then(Value::as_u64).unwrap_or(0);
    let daemon_truncated = data
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    // Reserve the metadata line before the matches: it is what tells the
    // caller the page is partial, so it must not be the part that falls off
    // the cap. Reserve the longer of the two reachable trailers (partial and
    // resumable, or complete) with the largest `next_offset` this page can
    // end on, so the trailer stays inside the cap. A `cap` smaller than the
    // trailer itself is the one case it exceeds — bounded, and preferable to
    // hiding the page state.
    let reserve = if json {
        let resumable = search_meta(
            data,
            true,
            Some(offset.saturating_add(matches.len() as u64)),
        );
        let complete = search_meta(data, false, None);
        serialized_len(&resumable).max(serialized_len(&complete))
    } else {
        0
    };
    let mut output = String::with_capacity(matches.len() * 80);
    let mut printed = 0;
    let mut cap_fired = false;
    for m in matches {
        let line = search_match_line(m, json);
        if !fits_before_cap(output.len(), line.len(), reserve, cap) {
            cap_fired = true;
            break;
        }
        output.push_str(&line);
        printed += 1;
    }
    let truncated = cap_fired || daemon_truncated;
    if json {
        let next_offset = truncated.then_some(offset.saturating_add(printed as u64));
        output.push_str(&format!("{}\n", search_meta(data, truncated, next_offset)));
    }
    write_stdout(&output)?;
    Ok(SearchPage {
        printed,
        truncated,
        cap_fired,
    })
}

/// Read surrounding lines from the file and attach as a `context` field.
/// Eliminates the need for a follow-up Read call — the agent gets the full
/// definition in one pixel search response. A per-run file-content cache is
/// threaded through so each file is read from disk at most once even when
/// many matches land in the same file.
fn enrich_with_context(
    m: &Value,
    root: &Path,
    context: usize,
    qualify: bool,
    cache: &mut HashMap<PathBuf, Option<String>>,
) -> Value {
    let rel = m.get("path").and_then(Value::as_str).unwrap_or("");
    let line_no = m.get("line").and_then(Value::as_u64).unwrap_or(0) as usize;
    let abs = root.join(rel);
    let content = match cache.entry(abs.clone()) {
        std::collections::hash_map::Entry::Occupied(e) => e.get().clone(),
        std::collections::hash_map::Entry::Vacant(e) => {
            e.insert(std::fs::read_to_string(&abs).ok()).clone()
        }
    };
    let Some(content) = content else {
        return m.clone();
    };
    let lines: Vec<&str> = content.lines().collect();
    let start = line_no.saturating_sub(context + 1).min(lines.len());
    let end = (line_no + context).min(lines.len());
    let mut ctx_lines = Vec::with_capacity(end - start);
    for (i, l) in lines[start..end].iter().enumerate() {
        let ln = start + i + 1;
        let marker = if ln == line_no { ">>" } else { "  " };
        ctx_lines.push(format!("{marker} {ln:>5}: {l}"));
    }
    let mut enriched = m.clone();
    enriched["context"] = Value::String(ctx_lines.join("\n"));
    if qualify {
        enriched["path"] = Value::String(abs.display().to_string());
    }
    enriched
}

/// Attach inline source context to every `resolve` match, same rationale as
/// `enrich_with_context` for `search`: without this, a `resolve` response
/// gives only a location, forcing a mandatory follow-up Read on every call —
/// measured as a real cost on trivial lookups (docs/bench/agent-ab-2026-08-30
/// clean-postfix.txt, s1-locate). Spans `[start_line - MARGIN, end_line +
/// MARGIN]` (not a fixed radius around one line) so a multi-line symbol's
/// full body is included, not just its first line.
fn enrich_resolve_matches_with_context(data: &mut Value, root: &Path) {
    const MARGIN: usize = 2;
    let Some(matches) = data.get_mut("matches").and_then(Value::as_array_mut) else {
        return;
    };
    let mut cache: HashMap<PathBuf, Option<String>> = HashMap::new();
    for m in matches.iter_mut() {
        let rel = m
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if rel.is_empty() {
            continue;
        }
        let start_line = m.get("start_line").and_then(Value::as_u64).unwrap_or(0) as usize;
        let end_line = m
            .get("end_line")
            .and_then(Value::as_u64)
            .map_or(start_line, |v| v as usize)
            .max(start_line);
        if start_line == 0 {
            continue;
        }
        let abs = root.join(&rel);
        let content = match cache.entry(abs.clone()) {
            std::collections::hash_map::Entry::Occupied(e) => e.get().clone(),
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(std::fs::read_to_string(&abs).ok()).clone()
            }
        };
        let Some(content) = content else { continue };
        let lines: Vec<&str> = content.lines().collect();
        let from = start_line.saturating_sub(MARGIN + 1).min(lines.len());
        let to = (end_line + MARGIN).min(lines.len());
        let mut ctx_lines = Vec::with_capacity(to - from);
        for (i, l) in lines[from..to].iter().enumerate() {
            let ln = from + i + 1;
            let marker = if ln >= start_line && ln <= end_line {
                ">>"
            } else {
                "  "
            };
            ctx_lines.push(format!("{marker} {ln:>5}: {l}"));
        }
        m["context"] = Value::String(ctx_lines.join("\n"));
    }
}

fn print_resolve_human(data: &Value) -> Result<(), String> {
    operation_metrics::observe(data);
    let Some(matches) = data.get("matches").and_then(Value::as_array) else {
        return print_data(data, false);
    };
    if matches.is_empty() {
        println!("No matches found.");
        return Ok(());
    }
    let mut output = String::new();
    for m in matches {
        let path = m.get("path").and_then(Value::as_str).unwrap_or("?");
        let start_line = m.get("start_line").and_then(Value::as_u64).unwrap_or(0);
        let kind = m.get("kind").and_then(Value::as_str).unwrap_or("");
        let score = m.get("score").and_then(Value::as_f64).unwrap_or(0.0);
        let raw = m
            .get("raw")
            .or_else(|| m.get("norm"))
            .and_then(Value::as_str)
            .unwrap_or("");

        output.push_str(&format!(
            "{path}:{start_line} ({kind}, score: {score:.2}) {raw}\n"
        ));
        if let Some(notes) = m.get("notes").and_then(Value::as_array) {
            for n in notes {
                output.push_str(&format!(
                    "  note [{}]: {}\n",
                    n.get("target").and_then(Value::as_str).unwrap_or("?"),
                    n.get("note").and_then(Value::as_str).unwrap_or("")
                ));
            }
        }
        if let Some(ctx) = m.get("context").and_then(Value::as_str) {
            output.push_str(ctx);
            output.push('\n');
        }
        output.push('\n');
    }
    let confidence = data
        .get("confidence")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let basis = data.get("basis").and_then(Value::as_str).unwrap_or("");
    output.push_str(&format!("Confidence: {confidence} ({basis})\n"));
    write_stdout(&output)
}

/// Group user-supplied paths by their discovered repo root, mapping each to a
/// repo-relative prefix ("" = whole repo).
fn group_by_root(paths: &[PathBuf]) -> Result<Vec<(PathBuf, Vec<String>)>, String> {
    let mut groups: Vec<(PathBuf, Vec<String>)> = Vec::new();
    for p in paths {
        let abs = p
            .canonicalize()
            .map_err(|e| format!("bad path {}: {e}", p.display()))?;
        let root = discover_root(&abs)?;
        let rel = abs
            .strip_prefix(&root)
            .map(|r| r.to_string_lossy().into_owned())
            .unwrap_or_default();
        match groups.iter_mut().find(|(r, _)| *r == root) {
            Some((_, rels)) => {
                if rel.is_empty() {
                    rels.clear();
                    rels.push(String::new());
                } else if !rels.iter().any(String::is_empty) {
                    rels.push(rel);
                }
            }
            None => groups.push((root, vec![rel])),
        }
    }
    Ok(groups)
}

#[allow(clippy::too_many_arguments)]
fn run_search(
    pattern: String,
    paths: Vec<PathBuf>,
    json: bool,
    stats: bool,
    limit: Option<usize>,
    offset: usize,
    no_daemon: bool,
    scope: Option<String>,
    context: usize,
    ignore_case: bool,
    logger: &pixel_actionlog::ActionLog,
) -> Result<(), String> {
    let effective_pattern = if ignore_case && !pattern.starts_with("(?i)") {
        format!("(?i){pattern}")
    } else {
        pattern
    };
    let groups = group_by_root(&paths)?;
    let multi_root = groups.len() > 1;
    for (root, rels) in groups {
        let whole_repo = rels.iter().any(String::is_empty);
        let req_paths = if whole_repo { None } else { Some(rels) };
        run_search_one(
            &effective_pattern,
            &root,
            req_paths,
            multi_root,
            json,
            stats,
            limit,
            offset,
            no_daemon,
            scope.clone(),
            context,
            logger,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_search_one(
    pattern: &str,
    root: &Path,
    req_paths: Option<Vec<String>>,
    qualify: bool,
    json: bool,
    stats: bool,
    limit: Option<usize>,
    offset: usize,
    no_daemon: bool,
    scope: Option<String>,
    context: usize,
    _logger: &pixel_actionlog::ActionLog,
) -> Result<(), String> {
    let data = execute(
        root,
        Request::Search {
            pattern: pattern.to_string(),
            json,
            limit,
            offset: Some(offset),
            paths: req_paths,
            scope,
        },
        no_daemon,
    )?;
    let empty = Vec::new();
    let matches = data
        .get("matches")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    // Enrich matches with surrounding context lines if requested.
    let mut cache: HashMap<PathBuf, Option<String>> = HashMap::new();
    let enriched: Vec<Value> = if context > 0 {
        matches
            .iter()
            .map(|m| enrich_with_context(m, root, context, qualify, &mut cache))
            .collect()
    } else if qualify {
        matches
            .iter()
            .map(|m| {
                let mut m = m.clone();
                if let Some(rel) = m.get("path").and_then(Value::as_str) {
                    let full = root.join(rel).display().to_string();
                    m["path"] = Value::String(full);
                }
                m
            })
            .collect()
    } else {
        matches.to_vec()
    };
    let page = print_search_matches(&data, &enriched, json)?;
    // Warn the user when results were truncated so the default row cap
    // is never a surprise.
    let match_count = data.get("match_count").and_then(Value::as_u64).unwrap_or(0);
    let limit = data.get("limit").and_then(Value::as_u64).unwrap_or(0);
    if page.cap_fired {
        // The `--context` text is added on this side of the daemon's byte
        // cap, so the stdout cap can be what cut this page: name that cap and
        // the offset that resumes the page instead of the daemon's row cap.
        eprintln!(
            "⚠ results truncated at the {}-byte stdout cap (PIXEL_OUTPUT_CAP_BYTES): wrote {} of {match_count} matches. \
             Narrow --context/--limit, or continue with --offset {}.",
            stdout_byte_cap(),
            page.printed,
            offset.saturating_add(page.printed),
        );
    } else if page.truncated {
        eprintln!(
            "⚠ results truncated: returned {match_count}; more matches exist (row limit {limit}, byte cap {} bytes). \
             Continue with --offset {} or pass --limit to raise the row cap (maximum 10000).",
            data.get("byte_cap").and_then(Value::as_u64).unwrap_or(0),
            data.get("next_offset").and_then(Value::as_u64).unwrap_or(0),
        );
    }
    // Epistemics surfacing for search's line-oriented output (which prints
    // matches, not the whole response object): when the answer is a bounded
    // partial, say so on stderr with the named caps.
    if let Some(e) = data.get("epistemics")
        && e.get("lower_bound").and_then(Value::as_bool) == Some(true)
        && let Some(basis) = e.get("basis").and_then(Value::as_str)
    {
        eprintln!("note: bounded result — {basis}");
    }
    if stats && let Some(s) = data.get("stats") {
        eprintln!(
            "candidates={}{} matches={} elapsed_us={}",
            s.get("candidates").and_then(Value::as_u64).unwrap_or(0),
            if s.get("scanned_all")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                " (full scan)"
            } else {
                ""
            },
            s.get("matches").and_then(Value::as_u64).unwrap_or(0),
            s.get("elapsed_us").and_then(Value::as_u64).unwrap_or(0),
        );
    }
    // One invocation record owns both duration and savings. Count distinct
    // returned evidence only; no metadata sweeps or duplicate search events.
    operation_metrics::observe(&json!({
        "matches": enriched,
        "truncated": page.truncated || offset > 0,
        "epistemics": data.get("epistemics"),
    }));
    Ok(())
}

// ---------------------------------------------------------------------------
// daemon management
// ---------------------------------------------------------------------------

/// Probe a daemon without starting one. Deliberately not `try_daemon`: that
/// one auto-starts a repository `Service` for any root it is given, which
/// turns a `status` or a `daemon start` check (the recall daemon's included)
/// into a spurious service — with `.pixel/` artifacts — on that root.
fn daemon_ping(root: &Path) -> bool {
    pixel_daemon::daemon::ping_only(root)
}

#[cfg(test)]
mod daemon_ping_tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::time::Instant;

    fn scratch_root(tag: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("pixel-daemon-ping-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root.canonicalize().unwrap()
    }

    /// Nothing listens: the probe is false and opens nothing — the repository
    /// `Service` an auto-starting probe would open is what left `.pixel/`
    /// inside `~/.local/share/pixel/recall`.
    #[test]
    fn daemon_ping_is_false_without_a_daemon() {
        let root = scratch_root("idle");
        assert!(!daemon_ping(&root));
        assert!(
            !root.join(pixel_index::index::SHARD_DIR).exists(),
            "the probe must not open a Service on {}",
            root.display()
        );
    }

    /// A daemon answering `Ping` is what the probe reports: without this half,
    /// a probe that always returned false would pass the idle test.
    #[test]
    fn daemon_ping_is_true_when_a_daemon_answers() {
        let root = scratch_root("live");
        let listener = UnixListener::bind(daemon::socket_path(&root)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let server = std::thread::spawn(move || {
            // Poll with a deadline: a probe that never connects must fail the
            // assertion, not hang the suite.
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        // A non-blocking listener hands back a non-blocking
                        // socket on BSD: reset it, or the read races the
                        // client's write instead of waiting for it.
                        let _ = stream.set_nonblocking(false);
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                        let mut line = String::new();
                        let Ok(n) = BufReader::new(&stream).read_line(&mut line) else {
                            return;
                        };
                        if n == 0 {
                            return;
                        }
                        assert_eq!(
                            serde_json::from_str::<Request>(&line).unwrap(),
                            Request::Ping,
                            "the probe asks with a Ping"
                        );
                        let reply = Response::success("ping", json!({"pong": true}));
                        writeln!(stream, "{}", serde_json::to_string(&reply).unwrap()).unwrap();
                        return;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Err(_) => return,
                }
            }
        });
        assert!(daemon_ping(&root));
        server.join().unwrap();
        let _ = std::fs::remove_file(daemon::socket_path(&root));
    }
}

fn daemon_start(path: PathBuf, foreground: bool, quiet: bool) -> Result<(), String> {
    let report = |message: &str| {
        if quiet {
            eprint!("{message}");
            Ok(())
        } else {
            write_stdout(message)
        }
    };
    if foreground {
        return daemon::run(&path).map_err(|e| e.to_string());
    }
    if daemon_ping(&path) {
        report(&format!(
            "daemon already running ({})\n",
            daemon::socket_path(&path).display()
        ))?;
        return Ok(());
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let abs = path
        .canonicalize()
        .map_err(|e| format!("bad path {}: {e}", path.display()))?;
    let mut command = std::process::Command::new(exe);
    command
        .arg("daemon")
        .arg("start")
        .arg(&abs)
        .arg("--foreground")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // Detach from the caller's process group so terminal/agent supervisors
    // do not tear down the daemon when the short-lived start command exits.
    command.process_group(0);
    command.spawn().map_err(|e| format!("spawn daemon: {e}"))?;
    // Wait for the socket to come up (index build can take a moment).
    for _ in 0..100 {
        if daemon_ping(&abs) {
            report(&format!(
                "daemon started ({})\n",
                daemon::socket_path(&abs).display()
            ))?;
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(format!(
        "daemon spawned but did not become ready ({})",
        daemon::socket_path(&abs).display()
    ))
}

fn daemon_stop(path: PathBuf) -> Result<(), String> {
    match try_daemon(&path, &Request::Shutdown) {
        Some(r) if r.ok => {
            write_stdout("daemon stopped\n")?;
            Ok(())
        }
        _ => {
            write_stdout(&format!("no daemon running for {}\n", path.display()))?;
            Ok(())
        }
    }
}

/// Where `pixel upgrade` writes the new binary, and why that path won.
struct UpgradeTarget {
    path: PathBuf,
    source: &'static str,
    /// The user named this path (`--install-path`): no ownership check.
    explicit: bool,
}

/// The package manager that owns an install, and how to update it there.
#[derive(Clone, Copy)]
enum ManagedBy {
    Mise,
    Homebrew,
}

impl ManagedBy {
    /// The manager's name, as the refusal message spells it.
    fn name(self) -> &'static str {
        match self {
            ManagedBy::Mise => "mise",
            ManagedBy::Homebrew => "Homebrew",
        }
    }

    /// The command that updates an install this manager owns. `brew update`
    /// comes first: with auto-update disabled, `brew upgrade` alone would
    /// install the tap's stale formula.
    fn upgrade_command(self) -> &'static str {
        match self {
            ManagedBy::Mise => "mise upgrade pixel",
            ManagedBy::Homebrew => "brew update && brew upgrade LivioGama/tap/pixel",
        }
    }
}

/// A directory whose files a package manager installed and checksummed.
struct ManagedRoot {
    root: PathBuf,
    manager: ManagedBy,
}

/// The install trees `pixel upgrade` must not write into on its own:
/// mise's `installs/` (`~/.local/share/mise`, or `$MISE_DATA_DIR`) and the
/// Homebrew Cellars (`/opt/homebrew`, `/usr/local`, or `$HOMEBREW_CELLAR`
/// that `brew shellenv` exports). Roots are canonicalized when they exist
/// so they compare against a resolved binary path (`/tmp` is
/// `/private/tmp` on macOS).
fn package_manager_roots(
    home: &Path,
    mise_data_dir: Option<&std::ffi::OsStr>,
    homebrew_cellar: Option<&std::ffi::OsStr>,
) -> Vec<ManagedRoot> {
    let mut roots = vec![ManagedRoot {
        root: home.join(".local/share/mise/installs"),
        manager: ManagedBy::Mise,
    }];
    if let Some(dir) = mise_data_dir.filter(|d| !d.is_empty()) {
        roots.push(ManagedRoot {
            root: Path::new(dir).join("installs"),
            manager: ManagedBy::Mise,
        });
    }
    for cellar in ["/opt/homebrew/Cellar", "/usr/local/Cellar"] {
        roots.push(ManagedRoot {
            root: PathBuf::from(cellar),
            manager: ManagedBy::Homebrew,
        });
    }
    if let Some(cellar) = homebrew_cellar.filter(|c| !c.is_empty()) {
        roots.push(ManagedRoot {
            root: PathBuf::from(cellar),
            manager: ManagedBy::Homebrew,
        });
    }
    for managed in &mut roots {
        if let Ok(canonical) = managed.root.canonicalize() {
            managed.root = canonical;
        }
    }
    roots
}

/// Why `pixel upgrade` refuses `target`, or `None` when it may write there.
///
/// Overwriting a package manager's binary is silent corruption: on
/// 2026-09-14 a bare `pixel upgrade` replaced the mise-installed 0.2.4 with
/// a dirty `target/dev-release` build while mise still listed 0.2.4 (mise
/// checks the checksum at install time only), and nothing said so. The
/// path is resolved first, so a symlink into a Cellar (`/opt/homebrew/bin/
/// pixel`) is refused like the Cellar file itself. The refusal names the
/// manager's own upgrade command, and `--install-path` is the user's
/// decision and is never refused.
fn upgrade_target_refusal(target: &UpgradeTarget, roots: &[ManagedRoot]) -> Option<String> {
    if target.explicit {
        return None;
    }
    let resolved = target
        .path
        .canonicalize()
        .unwrap_or_else(|_| target.path.clone());
    let owner = roots.iter().find(|r| resolved.starts_with(&r.root))?;
    Some(format!(
        "refusing to install over {resolved} ({source}): it lies under {root}, which {manager} \
         installed; overwriting it would leave {manager} listing a version that is no longer \
         there. Update it with `{command}` instead. Run `pixel self-update --dry-run` to see \
         where an upgrade lands, pass `--install-path {resolved}` to write there anyway, or \
         `--dev` to install a side build at ~/.local/bin/pixel-dev.",
        resolved = resolved.display(),
        source = target.source,
        root = owner.root.display(),
        manager = owner.manager.name(),
        command = owner.manager.upgrade_command(),
    ))
}

/// `pixel upgrade --dev` destination: a distinct name, so a local build
/// can be exercised as `pixel-dev` while `pixel` stays the managed one.
fn dev_install_path(home: &Path) -> PathBuf {
    home.join(".local/bin/pixel-dev")
}

/// True when `path` has a `target` directory component: a cargo build
/// output, never an install location.
fn is_cargo_target_path(path: &Path) -> bool {
    path.components()
        .any(|c| c.as_os_str() == std::ffi::OsStr::new("target"))
}

/// Every `pixel` executable on `path_var`, in PATH order, symlinks
/// resolved, deduplicated. Directories named `shims` (mise, asdf) are
/// skipped: a shim is a launcher that `exec`s the managed install, so it is
/// neither a place to write a binary nor a competing copy.
fn pixel_binaries_on_path(path_var: Option<&std::ffi::OsStr>) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = Vec::new();
    let Some(path_var) = path_var else {
        return found;
    };
    for dir in std::env::split_paths(path_var) {
        if dir.file_name().and_then(|n| n.to_str()) == Some("shims") {
            continue;
        }
        let candidate = dir.join("pixel");
        if !candidate.is_file() {
            continue;
        }
        let resolved = candidate.canonicalize().unwrap_or(candidate);
        if !found.contains(&resolved) {
            found.push(resolved);
        }
    }
    found
}

/// Cargo profile directory a build command writes to, from its own flags:
/// `--profile <name>` / `--profile=<name>` wins, then `--release`
/// (`release`), else cargo's default `debug`. `pixel upgrade` used to
/// hardcode `target/release`, so a `--build` with another profile installed
/// whatever stale binary sat there. A command that does not invoke cargo
/// (a wrapper script, `/usr/bin/true` in tests) keeps the historical
/// `release`: cargo's flag semantics do not apply to it.
fn cargo_profile_dir(build: &str) -> String {
    let mut args = build.split_whitespace().peekable();
    let mut release = false;
    let mut invokes_cargo = false;
    while let Some(arg) = args.next() {
        if arg == "cargo" || arg.ends_with("/cargo") {
            invokes_cargo = true;
        } else if arg == "--profile" {
            if let Some(name) = args.next() {
                return profile_dir_name(name);
            }
        } else if let Some(name) = arg.strip_prefix("--profile=") {
            return profile_dir_name(name);
        } else if arg == "--release" || arg == "-r" {
            release = true;
        }
    }
    if release || !invokes_cargo {
        "release".into()
    } else {
        "debug".into()
    }
}

/// Cargo maps the built-in `dev`/`test` profiles to `target/debug` and
/// `bench` to `target/release`; custom profiles use their own name.
fn profile_dir_name(name: &str) -> String {
    match name {
        "dev" | "test" => "debug".into(),
        "bench" => "release".into(),
        other => other.into(),
    }
}

/// Decide where `pixel upgrade` installs.
///
/// The historical fixed `~/.local/bin/pixel` was wrong on any machine whose
/// `pixel` is managed elsewhere (a mise/asdf install dir behind a shim, a
/// Homebrew cellar, `~/.cargo/bin`): it dropped a second copy that either
/// shadowed the managed one or never reached PATH. Order:
///
/// 1. `--install-path`, verbatim.
/// 2. The binary running this command: after a shim `exec`s the managed
///    install, `current_exe` IS the managed install. Skipped when it sits
///    in a cargo `target/` dir (`target/release/pixel upgrade`).
/// 3. The first `pixel` on PATH outside a `shims` dir or a `target/` dir.
/// 4. `~/.local/bin/pixel`, the legacy default.
///
/// Steps 2 to 4 only find a path; `upgrade_target_refusal` then refuses one
/// a package manager owns.
fn resolve_upgrade_target(
    explicit: Option<PathBuf>,
    current_exe: Option<PathBuf>,
    path_var: Option<&std::ffi::OsStr>,
    home: &Path,
) -> UpgradeTarget {
    if let Some(path) = explicit {
        return UpgradeTarget {
            path,
            source: "--install-path",
            explicit: true,
        };
    }
    if let Some(exe) = current_exe {
        let exe = exe.canonicalize().unwrap_or(exe);
        if exe.is_file() && !is_cargo_target_path(&exe) {
            return UpgradeTarget {
                path: exe,
                source: "running binary",
                explicit: false,
            };
        }
    }
    if let Some(path) = pixel_binaries_on_path(path_var)
        .into_iter()
        .find(|p| !is_cargo_target_path(p))
    {
        return UpgradeTarget {
            path,
            source: "first pixel on PATH",
            explicit: false,
        };
    }
    UpgradeTarget {
        path: home.join(".local").join("bin").join("pixel"),
        source: "default",
        explicit: false,
    }
}

/// The `pixel` that a shell would run INSTEAD of `installed`, if any: the
/// first PATH hit that is a different file. This is how a stale copy in
/// `~/.local/bin` silently kept serving an old version after an upgrade
/// landed in a mise install dir that came later on PATH. A binary installed
/// under another name (`pixel-dev`) is not what `pixel` runs, so no other
/// `pixel` can shadow it.
fn upgrade_shadowed_by(installed: &Path, path_var: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    if installed.file_name() != Some(std::ffi::OsStr::new("pixel")) {
        return None;
    }
    let installed = installed.canonicalize().ok()?;
    pixel_binaries_on_path(path_var)
        .into_iter()
        .next()
        .filter(|first| *first != installed)
}

#[cfg(test)]
mod upgrade_target_tests {
    use super::cargo_profile_dir;

    /// The install step must read the binary the build step wrote: a
    /// `--build` on another profile (the `dev-release` iteration loop) used
    /// to install the stale `target/release/pixel` without any error.
    #[test]
    fn profile_dir_follows_the_build_command() {
        assert_eq!(
            cargo_profile_dir("cargo build --release -p pixel-cli"),
            "release"
        );
        assert_eq!(cargo_profile_dir("cargo build -r -p pixel-cli"), "release");
        assert_eq!(cargo_profile_dir("cargo build -p pixel-cli"), "debug");
        assert_eq!(
            cargo_profile_dir("cargo build --profile dev-release -p pixel-cli"),
            "dev-release"
        );
        assert_eq!(
            cargo_profile_dir("cargo build --profile=dev-release"),
            "dev-release"
        );
        // `--profile` beats `--release` whichever comes first, as in cargo.
        assert_eq!(
            cargo_profile_dir("cargo build --release --profile dev-release"),
            "dev-release"
        );
        assert_eq!(cargo_profile_dir("cargo build --profile dev"), "debug");
        assert_eq!(cargo_profile_dir("cargo build --profile bench"), "release");
        assert_eq!(
            cargo_profile_dir("~/.cargo/bin/cargo build -p pixel-cli"),
            "debug"
        );
        // Not a cargo invocation: no flag semantics, historical `release`
        // (the upgrade CLI tests fake the build with `/usr/bin/true`).
        assert_eq!(cargo_profile_dir("/usr/bin/true"), "release");
        assert_eq!(cargo_profile_dir("./scripts/build.sh"), "release");
        assert_eq!(
            cargo_profile_dir("./scripts/build.sh --profile fast"),
            "fast"
        );
    }
    use super::*;

    fn sandbox(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("pixel-upgrade-target-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn touch(path: &Path) -> PathBuf {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"x").unwrap();
        path.canonicalize().unwrap()
    }

    #[test]
    fn explicit_flag_wins_verbatim() {
        let t = resolve_upgrade_target(
            Some(PathBuf::from("/opt/x/pixel")),
            Some(PathBuf::from("/nope")),
            None,
            Path::new("/home/u"),
        );
        assert_eq!(t.path, PathBuf::from("/opt/x/pixel"));
        assert_eq!(t.source, "--install-path");
        assert!(t.explicit);
    }

    /// The point of the change: on a machine where `pixel` is a managed
    /// install behind a shim, the running binary is that install, and the
    /// upgrade must land there, not in a `~/.local/bin` that shadows or
    /// misses PATH.
    #[test]
    fn running_binary_is_the_install_location() {
        let d = sandbox("running");
        let managed = touch(&d.join("mise/installs/pixel/rev-abc/bin/pixel"));
        let t = resolve_upgrade_target(None, Some(managed.clone()), None, &d);
        assert_eq!(t.path, managed);
        assert_eq!(t.source, "running binary");
        assert!(!t.explicit, "a resolved default is subject to the refusal");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// `target/release/pixel upgrade` (or a test binary) must never make
    /// the build output the install location.
    #[test]
    fn cargo_target_binary_falls_through_to_path_then_default() {
        let d = sandbox("target");
        let built = touch(&d.join("repo/target/release/pixel"));
        let on_path = touch(&d.join("cellar/bin/pixel"));
        let shim = touch(&d.join("mise/shims/pixel"));
        let path_var = std::env::join_paths([
            shim.parent().unwrap().to_path_buf(),
            d.join("repo/target/release"),
            on_path.parent().unwrap().to_path_buf(),
        ])
        .unwrap();
        let t = resolve_upgrade_target(None, Some(built.clone()), Some(&path_var), &d);
        assert_eq!(t.path, on_path, "shim dir and target dir skipped");
        assert_eq!(t.source, "first pixel on PATH");
        assert!(!t.explicit);

        let t = resolve_upgrade_target(None, Some(built), None, &d);
        assert_eq!(t.path, d.join(".local/bin/pixel"));
        assert_eq!(t.source, "default");
        assert!(!t.explicit);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn shadow_is_reported_only_when_a_different_pixel_comes_first() {
        let d = sandbox("shadow");
        let stale = touch(&d.join("local/bin/pixel"));
        let managed = touch(&d.join("mise/installs/pixel/bin/pixel"));
        let link_dir = d.join("linkdir");
        std::fs::create_dir_all(&link_dir).unwrap();
        std::os::unix::fs::symlink(&managed, link_dir.join("pixel")).unwrap();

        let stale_first = std::env::join_paths([
            stale.parent().unwrap().to_path_buf(),
            managed.parent().unwrap().to_path_buf(),
        ])
        .unwrap();
        assert_eq!(
            upgrade_shadowed_by(&managed, Some(&stale_first)),
            Some(stale.clone())
        );

        let managed_first = std::env::join_paths([
            managed.parent().unwrap().to_path_buf(),
            stale.parent().unwrap().to_path_buf(),
        ])
        .unwrap();
        assert_eq!(upgrade_shadowed_by(&managed, Some(&managed_first)), None);

        // A symlink to the installed binary is the same file, not a shadow.
        let link_first =
            std::env::join_paths([link_dir, stale.parent().unwrap().to_path_buf()]).unwrap();
        assert_eq!(upgrade_shadowed_by(&managed, Some(&link_first)), None);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// `pixel upgrade --dev` exists to never touch `pixel`: a `pixel` earlier
    /// on PATH is not shadowing `pixel-dev`, so warning about it would be a
    /// false alarm on every dev install.
    #[test]
    fn a_binary_under_another_name_is_never_shadowed() {
        let d = sandbox("devshadow");
        let managed = touch(&d.join("mise/installs/pixel/bin/pixel"));
        let dev = touch(&d.join("local/bin/pixel-dev"));
        let path_var = std::env::join_paths([managed.parent().unwrap()]).unwrap();
        assert_eq!(upgrade_shadowed_by(&dev, Some(&path_var)), None);
        assert_eq!(
            dev_install_path(Path::new("/home/u")),
            PathBuf::from("/home/u/.local/bin/pixel-dev")
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    fn roots_of(roots: &[ManagedRoot]) -> Vec<(PathBuf, &'static str)> {
        roots
            .iter()
            .map(|r| (r.root.clone(), r.manager.name()))
            .collect()
    }

    /// The trees a bare upgrade must not write into: mise's default and
    /// relocated `installs/`, both Homebrew Cellars, and the Cellar
    /// `brew shellenv` exports. An unset or empty variable adds nothing
    /// (an empty `HOMEBREW_CELLAR` would otherwise make `""` a root).
    #[test]
    fn package_manager_roots_cover_mise_and_homebrew() {
        let home = Path::new("/nonexistent-home");
        let base = roots_of(&package_manager_roots(home, None, None));
        assert_eq!(
            base,
            vec![
                (home.join(".local/share/mise/installs"), "mise"),
                (PathBuf::from("/opt/homebrew/Cellar"), "Homebrew"),
                (PathBuf::from("/usr/local/Cellar"), "Homebrew"),
            ]
        );
        let empty = std::ffi::OsStr::new("");
        assert_eq!(
            roots_of(&package_manager_roots(home, Some(empty), Some(empty))),
            base
        );
        let with_env = roots_of(&package_manager_roots(
            home,
            Some(std::ffi::OsStr::new("/nonexistent-mise")),
            Some(std::ffi::OsStr::new("/nonexistent-cellar")),
        ));
        assert!(with_env.contains(&(PathBuf::from("/nonexistent-mise/installs"), "mise")));
        assert!(with_env.contains(&(PathBuf::from("/nonexistent-cellar"), "Homebrew")));
        assert_eq!(with_env.len(), 5);
    }

    fn target(path: PathBuf, explicit: bool) -> UpgradeTarget {
        UpgradeTarget {
            path,
            source: "running binary",
            explicit,
        }
    }

    /// The refusal is the guard against clobbering a managed install: a
    /// resolved path under a root is refused (directly or through a
    /// symlink), a sibling that merely shares a name prefix is not, and an
    /// explicit `--install-path` is always the user's call.
    #[test]
    fn refusal_follows_symlinks_into_managed_roots_and_spares_explicit_paths() {
        let d = sandbox("refusal");
        let cellar = d.join("Cellar");
        let keg = touch(&cellar.join("pixel/1.0/bin/pixel"));
        let roots = vec![ManagedRoot {
            root: cellar.canonicalize().unwrap(),
            manager: ManagedBy::Homebrew,
        }];

        let reason = upgrade_target_refusal(&target(keg.clone(), false), &roots).unwrap();
        assert!(reason.contains(&keg.display().to_string()), "{reason}");
        assert!(reason.contains("(running binary)"), "{reason}");
        assert!(reason.contains("Homebrew"), "{reason}");
        assert!(
            reason.contains("brew update && brew upgrade LivioGama/tap/pixel"),
            "{reason}"
        );

        let link = d.join("bin/pixel");
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&keg, &link).unwrap();
        let reason = upgrade_target_refusal(&target(link, false), &roots).unwrap();
        assert!(
            reason.contains(&keg.display().to_string()),
            "resolved: {reason}"
        );

        assert!(upgrade_target_refusal(&target(keg, true), &roots).is_none());
        let sibling = touch(&d.join("Cellarx/pixel"));
        assert!(upgrade_target_refusal(&target(sibling, false), &roots).is_none());
        // A path that does not exist yet (the `~/.local/bin/pixel` default)
        // is compared as given.
        let missing = cellar.canonicalize().unwrap().join("new/pixel");
        assert!(upgrade_target_refusal(&target(missing, false), &roots).is_some());
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Each manager's refusal names its own update command: a mise or
    /// Homebrew install is updated through its package manager, not by
    /// `pixel self-update`, and that command is the actionable half of the
    /// refusal.
    #[test]
    fn refusal_names_the_owning_managers_update_command() {
        let d = sandbox("manager-command");
        let mise_root = d.join("Mise");
        let mise_keg = touch(&mise_root.join("pixel/1.0/bin/pixel"));
        let cellar = d.join("Cellar");
        let brew_keg = touch(&cellar.join("pixel/1.0/bin/pixel"));
        let roots = vec![
            ManagedRoot {
                root: mise_root.canonicalize().unwrap(),
                manager: ManagedBy::Mise,
            },
            ManagedRoot {
                root: cellar.canonicalize().unwrap(),
                manager: ManagedBy::Homebrew,
            },
        ];

        let reason = upgrade_target_refusal(&target(mise_keg, false), &roots).unwrap();
        assert!(reason.contains("mise upgrade pixel"), "{reason}");

        let reason = upgrade_target_refusal(&target(brew_keg, false), &roots).unwrap();
        assert!(
            reason.contains("brew update && brew upgrade LivioGama/tap/pixel"),
            "{reason}"
        );
        let _ = std::fs::remove_dir_all(&d);
    }
}

/// Upgrade control messages must never inherit the retrieval client's 600s timeout.
/// Only a missing/refused socket means absent; a stalled or malformed reply is an error.
fn upgrade_daemon_request(root: &Path, req: &Request) -> Result<Option<Response>, String> {
    let mut stream = match UnixStream::connect(daemon::socket_path(root)) {
        Ok(stream) => stream,
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Ok(None);
        }
        Err(e) => return Err(format!("upgrade daemon connection: {e}")),
    };
    stream
        .set_read_timeout(Some(Duration::from_millis(1500)))
        .and_then(|_| stream.set_write_timeout(Some(Duration::from_millis(1500))))
        .map_err(|e| format!("upgrade daemon timeout: {e}"))?;
    roundtrip(&mut stream, req).map(Some).ok_or_else(|| {
        "installed binary, but daemon control timed out or returned an invalid response".into()
    })
}

/// A daemon that accepted shutdown is stopped only after it unlinks its own
/// repository socket. Pinging during that teardown can be accepted by the
/// listener after the serving loop has exited, then time out without a reply.
fn upgrade_daemon_socket_stopped(root: &Path) -> Result<bool, String> {
    let socket = daemon::socket_path(root);
    match std::fs::symlink_metadata(&socket) {
        Ok(_) => Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(format!(
            "installed binary, but could not inspect daemon socket {}: {error}",
            socket.display()
        )),
    }
}

fn daemon_status(path: PathBuf) -> Result<(), String> {
    if daemon_ping(&path) {
        write_stdout(&format!(
            "daemon running ({})\n",
            daemon::socket_path(&path).display()
        ))?;
    } else {
        write_stdout(&format!("daemon not running for {}\n", path.display()))?;
    }
    Ok(())
}

/// Replace `snapshot.dirty` (the full dirty path list) with `dirty_count`.
///
/// Freshness/readiness answers (`status`, `ready`) only need to say HOW
/// dirty the tree is: the list itself belongs to `inspect`/`review`, and
/// carrying it here let one untracked `vendor/bundle` push a 200-byte
/// answer past the global output cap. The daemon now ships the compact
/// form itself for every op but `inspect`/`review`
/// (`SnapshotInfo::compact`); this stays as the client-side guard when
/// talking to an older daemon that still sends the list.
fn compact_snapshot(data: &mut Value) {
    if let Some(snap) = data.get_mut("snapshot").and_then(Value::as_object_mut)
        && let Some(dirty) = snap.remove("dirty")
    {
        let n = dirty.as_array().map_or(0, Vec::len);
        snap.insert("dirty_count".into(), json!(n));
    }
}

/// Drop the tracked-clean file list from a `repo-state` answer.
///
/// On a clean tree the list is the bulk of the answer (200 of 351 paths,
/// ~7 KB measured on this repository) and no consumer reads it: `repo-state`
/// answers "HEAD + branch + dirty" and `clean_count` is what says how clean
/// the rest of the tree is. `--include-clean` asks for the full, capped list.
fn compact_repo_state(data: &mut Value) {
    if let Some(obj) = data.as_object_mut() {
        obj.remove("clean");
        obj.remove("clean_list_cap");
        obj.remove("clean_list_truncated");
    }
}

/// Prepare every local GitPixel prerequisite in one deterministic operation.
///
/// The JSON answer is deliberately compact — index counters, graph counters,
/// daemon state, `dirty_count` — and never embeds the status snapshot: the
/// dirty file list is irrelevant to "is the index ready" and is what blew
/// past the output cap on repos with untracked vendor trees.
fn ready(path: PathBuf, no_daemon: bool, json: bool) -> Result<(), String> {
    let root = discover_root(&path)?;
    let status = execute(&root, Request::Status {}, no_daemon)?;
    let graph = execute(&root, Request::Graph {}, no_daemon)?;
    if !no_daemon {
        daemon_start(root.clone(), false, json)?;
    }
    let snapshot = status.get("snapshot");
    let dirty_count = snapshot
        .and_then(|s| s.get("dirty_count"))
        .and_then(Value::as_u64)
        .unwrap_or_else(|| {
            // Older daemon: the status snapshot still carries the list.
            snapshot
                .and_then(|s| s.get("dirty"))
                .and_then(Value::as_array)
                .map_or(0, Vec::len) as u64
        });
    let data = serde_json::json!({
        "root": root,
        "index": status.get("index").cloned().unwrap_or(Value::Null),
        "graph": graph,
        "daemon": if no_daemon { "skipped" } else { "running" },
        "dirty_count": dirty_count,
    });
    if json {
        print_data(&data, true)
    } else {
        write_stdout(&format!(
            "ready: {}\nindex: ready\ngraph: ready\ndaemon: {}\n",
            data.get("root").and_then(Value::as_str).unwrap_or("?"),
            data.get("daemon").and_then(Value::as_str).unwrap_or("?")
        ))
    }
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

/// Facts/history visibility block for `pixel status`: phase, commits indexed
/// vs the git rev-list count, diff coverage, freshness, and schema version.
fn facts_status(root: &Path) -> Option<Value> {
    let store = pixel_facts::FactsStore::open(root).ok()?;
    let state = store.index_state();
    Some(json!({
        "phase": state.phase,
        "commits_indexed": state.commits_indexed,
        "total_commits": pixel_git::GitRunner::new(root)
            .rev_list_count_all()
            .unwrap_or(state.total_commits),
        "diff_indexed_pct": state.diff_indexed_pct,
        "fresh": state.fresh,
        "schema_version": state.schema_version,
    }))
}

/// Parse argv, dispatch, and record the invocation to the per-repo action
/// log (`<root>/.pixel/actions.jsonl`) so a session can be self-assessed
/// later. Logging is best-effort and asynchronous — it can never fail or
/// slow down the command it observes: `discover_root` failures fall back to
/// a no-op logger, and `ActionLog::finish` bounds the writer's shutdown
/// window instead of blocking on a slow disk.
fn operation_path(matches: &clap::ArgMatches) -> Option<PathBuf> {
    if let Some((_, nested)) = matches.subcommand()
        && let Some(path) = operation_path(nested)
    {
        return Some(path);
    }
    for key in ["path", "repo"] {
        if let Ok(Some(path)) = matches.try_get_one::<PathBuf>(key) {
            return Some(path.clone());
        }
    }
    matches
        .try_get_many::<PathBuf>("paths")
        .ok()
        .flatten()
        .and_then(|mut paths| paths.next().cloned())
}

/// What `pixel migrate` prints now that the command does nothing.
const MIGRATE_REMOVED_NOTE: &str = "note: 'migrate' was removed and does nothing; pixel no longer \
     reads legacy .gitpixel/ state, so delete that directory by hand if it is still there";

/// The pre-rename subcommand this invocation was spelled with, and its
/// current name. Only the first word that is not an option names the
/// command (`pixel prepare-repo ready` runs `prepare-repo` on a path called
/// `ready`); `--metrics` is the one option taking a separate value before it.
fn renamed_invocation(argv: &[String]) -> Option<(&str, &'static str)> {
    let mut words = argv.iter().skip(1);
    while let Some(word) = words.next() {
        if word == "--metrics" {
            words.next();
        } else if !word.starts_with('-') {
            return pixel_proto::commands::renamed_to(word).map(|new| (word.as_str(), new));
        }
    }
    None
}

/// The one stderr line announcing that an old command name was used, or
/// `None` when the name is current or live reporting is off. `live` is the
/// metrics gate: `--metrics off`, `PIXEL_METRICS=0` and the protected
/// streams (hooks, `search-like-rg`, the statusline) silence both alike, so
/// the note never lands in a hook response or a byte-compatible rg output.
fn rename_note(argv: &[String], live: bool) -> Option<String> {
    if !live {
        return None;
    }
    let (old, new) = renamed_invocation(argv)?;
    Some(format!(
        "note: '{old}' is now '{new}'; the old name stays accepted until {}\n",
        pixel_proto::commands::ALIAS_REMOVAL_VERSION
    ))
}

fn run() -> Result<(), String> {
    let started = std::time::Instant::now();
    let argv: Vec<String> = std::env::args().collect();
    let matches = Cli::command().get_matches();
    let command_label = matches.subcommand_name().unwrap_or("unknown").to_string();
    let path = operation_path(&matches).unwrap_or_else(|| PathBuf::from("."));
    let cli = Cli::from_arg_matches(&matches).unwrap_or_else(|error| error.exit());
    let protected = matches!(
        &cli.command,
        Command::SearchLikeRg { .. }
            | Command::RunHook { .. }
            | Command::Status {
                statusline: true,
                ..
            }
            | Command::Daemon {
                cmd: DaemonCmd::Start {
                    foreground: true,
                    ..
                }
            }
            | Command::Mcp { .. }
            | Command::ListErrors {
                cmd: sniper_cmd::SniperCmd::Mcp { .. } | sniper_cmd::SniperCmd::Run { .. }
            }
    );
    let live = !protected
        && cli.metrics != "off"
        && std::env::var_os("PIXEL_METRICS").is_none_or(|v| v != "0");
    if let Some(note) = rename_note(&argv, live) {
        eprint!("{note}");
    }
    let root = discover_root(&path).or_else(|_| discover_root(Path::new(".")));
    operation_metrics::begin(root.as_deref().unwrap_or(Path::new(".")));
    // Compatibility fallback must exec the original before any logging changes
    // its search corpus; its successful Pixel branch retains existing logging.
    let mut logger = match &root {
        _ if matches!(&cli.command, Command::SearchLikeRg { .. }) => {
            pixel_actionlog::ActionLog::noop()
        }
        Ok(root) => pixel_actionlog::ActionLog::spawn_for_root(root),
        Err(_) => pixel_actionlog::ActionLog::noop(),
    };
    // An exit code a command owns end to end. `pixel evaluate` answers on
    // a three-way contract (0 evaluated / 2 usage / 3 technical) that
    // `main`'s two-way `Result` cannot carry, and it prints its own
    // envelope, so it must not return `Err` — that would add a second
    // diagnostic to stderr — nor exit before the action log is written.
    let owned_exit: std::cell::Cell<Option<i32>> = std::cell::Cell::new(None);
    let result = run_command(cli.command, &logger, &owned_exit);
    let owned_exit = owned_exit.get();
    if let Err(error) = &result {
        // The stdout contract under `--json`: a failing command answers with a
        // parsable failure envelope carrying `error.code`, so an agent can
        // tell NOT_FOUND (widen the query) from INVALID_INPUT (fix the call)
        // without reading prose. Human mode is unchanged — stdout stays empty,
        // the reason goes to stderr, the exit status stays 1 — and a command
        // that owns stdout or already wrote part of an answer keeps it.
        if failure_envelope_wanted(&matches, protected) {
            write_failure_envelope(&command_label, error);
        }
        // The diagnostic precedes the authoritative metrics line. Its failure
        // is best-effort and must never change the operation's result.
        let _ = operation_metrics::Counted(std::io::stderr().lock())
            .write_all(format!("pixel: {error}\n").as_bytes());
    }
    let _ = std::io::stdout().flush();
    let elapsed = started.elapsed();
    // A command that owns its exit code still reports its outcome to the
    // journal: a non-zero code is a failure there, even though it never
    // travelled as an `Err`.
    let logged_result = match owned_exit {
        Some(code) if code != 0 => Err(format!("{command_label} exited {code}")),
        _ => result.clone(),
    };
    let mut event = pixel_actionlog::ActionEvent::new(&command_label, argv[1..].join(" "))
        .with_result(&logged_result, elapsed);
    if !protected {
        let output_bytes = operation_metrics::output_bytes();
        let mut metrics = match operation_metrics::evidence(&command_label, result.is_ok()) {
            Ok(evidence) => {
                pixel_actionlog::OperationMetrics::new(elapsed, output_bytes, Some(evidence))
            }
            Err(gap) => pixel_actionlog::OperationMetrics::new(elapsed, output_bytes, None)
                .with_comparison_gap(gap),
        };
        // Optional policy input, never a measured LLM latency. Invalid or
        // non-Unicode values retain the versioned default without affecting
        // command success, diagnostics, or protected streams.
        if let Some(round_trip_ms) = std::env::var("PIXEL_METRICS_ROUND_TRIP_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
        {
            metrics = metrics.with_round_trip_ms(round_trip_ms);
        }
        metrics.output_scope = Some("cli-rendered-streams".to_owned());
        event = event.with_metrics(metrics);
    }
    if live && let Some(line) = event.finalize_metrics_line() {
        // A blank record creates a distinct terminal block so transcript UIs
        // cannot visually attach the metrics matrix to command output.
        // Reporting bytes include this separator and the trailing newline.
        let _ = writeln!(std::io::stderr().lock(), "\n{line}");
    }
    logger.log(event);
    logger.finish();
    if let Some(code) = owned_exit {
        std::process::exit(code);
    }
    result
}

fn run_command(
    command: Command,
    logger: &pixel_actionlog::ActionLog,
    owned_exit: &std::cell::Cell<Option<i32>>,
) -> Result<(), String> {
    match command {
        Command::SearchLikeRg { tool, args } => search_compat::run(tool, args),
        Command::BuildIndex {
            path,
            extractor,
            max_gram,
            history,
        } => {
            let path = discover_root(&path)?;
            // Route through the daemon when available (singleton build —
            // no concurrent build races). Fall back to in-process build.
            if let Some(resp) = try_daemon(&path, &Request::Reindex {}) {
                let v = unwrap_response(resp)?;
                eprintln!(
                    "indexed via daemon: base_files={} delta_files={} overlay_files={}",
                    v.pointer("/index/base_files")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0),
                    v.pointer("/index/delta_files")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0),
                    v.pointer("/index/overlay_files")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0),
                );
                if history {
                    let mut store =
                        pixel_facts::FactsStore::open(&path).map_err(|e| e.to_string())?;
                    let opts = pixel_facts::ingest::IngestOptions::default();
                    let report = pixel_facts::ingest::ingest_until_fresh(&mut store, &opts)
                        .map_err(|e| e.to_string())?;
                    eprintln!(
                        "facts: phase={} commits={} diff_coverage={:.0}% fresh={}",
                        report.phase,
                        report.commits_indexed,
                        report.diff_indexed_pct * 100.0,
                        report.fresh
                    );
                }
                return Ok(());
            }
            // In-process fallback (with build lock to prevent concurrent
            // build races when multiple CLI invocations hit the same root).
            let _lock =
                pixel_index::BuildLock::acquire(&path).map_err(|e| format!("build lock: {e}"))?;
            let ex = make_extractor(extractor, max_gram);
            let stats = build(&path, ex.as_ref()).map_err(|e| e.to_string())?;
            eprintln!(
                "indexed {} files ({} bytes) -> {} grams, shard {} bytes, {} ms",
                stats.files, stats.bytes, stats.grams, stats.shard_bytes, stats.elapsed_ms
            );
            if history {
                let mut store = pixel_facts::FactsStore::open(&path).map_err(|e| e.to_string())?;
                let opts = pixel_facts::ingest::IngestOptions::default();
                let report = pixel_facts::ingest::ingest_until_fresh(&mut store, &opts)
                    .map_err(|e| e.to_string())?;
                eprintln!(
                    "facts: phase={} commits={} diff_coverage={:.0}% fresh={}",
                    report.phase,
                    report.commits_indexed,
                    report.diff_indexed_pct * 100.0,
                    report.fresh
                );
            }
            Ok(())
        }
        Command::SearchContent {
            pattern,
            paths,
            json,
            stats,
            limit,
            offset,
            no_daemon,
            scope,
            context,
            ignore_case,
        } => {
            if call_guard_check("search-content", &format!("{pattern} {paths:?}")) {
                return Err("circuit breaker: repeated search calls".to_string());
            }
            run_search(
                pattern,
                paths,
                json,
                stats,
                limit,
                offset,
                no_daemon,
                scope,
                context,
                ignore_case,
                logger,
            )
        }
        Command::RunRecipe {
            intent,
            path,
            kind,
            budget,
            json,
            no_daemon,
        } => run_query(intent, path, &kind, budget, json, no_daemon, logger),
        Command::SearchMeaning {
            question,
            path,
            limit,
            max_files,
            json,
        } => run_ask(question, path, limit, max_files, json),
        Command::ScopeTask {
            task,
            path,
            json,
            limit,
            no_manifest,
            clear,
            max_tier,
            precision,
        } => {
            if clear {
                // With --clear the sole positional (if any) is a path, not a
                // task: `gitpixel targets --clear .` must just work.
                let clear_path = match task {
                    Some(t) => {
                        let p = PathBuf::from(&t);
                        if p.exists() {
                            p
                        } else {
                            return Err("--clear takes no task argument".to_string());
                        }
                    }
                    None => path,
                };
                let root = discover_root(&clear_path)?;
                let manifest_path = root
                    .join(pixel_index::index::SHARD_DIR)
                    .join("targets.json");
                if manifest_path.exists() {
                    std::fs::remove_file(&manifest_path)
                        .map_err(|e| format!("remove {}: {e}", manifest_path.display()))?;
                    println!("targets manifest cleared");
                } else {
                    println!("no active targets manifest");
                }
                return Ok(());
            }
            let task =
                task.ok_or_else(|| "missing task description (or pass --clear)".to_string())?;
            let root = discover_root(&path)?;
            let manifest_path = root
                .join(pixel_index::index::SHARD_DIR)
                .join("targets.json");
            let data = execute(
                &path,
                Request::Targets {
                    task: task.clone(),
                    limit,
                    max_tier: max_tier.clone(),
                    precision,
                },
                false,
            )?;
            let active_tasks = if no_manifest {
                None
            } else {
                Some(write_targets_manifest(&manifest_path, &task, &data)?)
            };
            finish_graph_cmd(data, json, pretty_targets)?;
            if let Some(active) = active_tasks {
                eprintln!(
                    "targets manifest active: {} ({active} task(s)) — scoping enforced; run `pixel scope-task --clear` when the task ends",
                    manifest_path.display()
                );
            }
            Ok(())
        }
        Command::PlanRollback {
            problem,
            path,
            files,
            depth,
            apply,
            merge,
            stash_first,
            allow_dirty,
            json,
        } => {
            let root = discover_root(&path)?;
            if let Some(oid) = apply {
                let result = rescue_cmd::apply(
                    &root,
                    &oid,
                    &files,
                    &rescue_cmd::ApplyOptions {
                        merge,
                        stash_first,
                        allow_dirty,
                    },
                )?;
                if json {
                    return print_data(&result, true);
                }
                if let Some(applied) = result["files"].as_array() {
                    for f in applied {
                        println!(
                            "{}: {}{}",
                            f["path"].as_str().unwrap_or("?"),
                            f["action"].as_str().unwrap_or("?"),
                            f["conflicts"]
                                .as_i64()
                                .filter(|c| *c > 0)
                                .map(|c| format!(" ({c} conflict hunk(s) — resolve the markers)"))
                                .unwrap_or_default(),
                        );
                    }
                }
                println!("{}", result["note"].as_str().unwrap_or(""));
                return Ok(());
            }
            let problem = problem.ok_or_else(|| "missing problem description".to_string())?;
            // Locate targets: explicit --file hints win; otherwise the sniper
            // target engine points the problem at files (P0 slice).
            let (target_paths, keywords) = if files.is_empty() {
                let data = execute(
                    &path,
                    Request::Targets {
                        task: problem.clone(),
                        limit: Some(10),
                        max_tier: None,
                        precision: false,
                    },
                    false,
                )?;
                let all = data["targets"].as_array().cloned().unwrap_or_default();
                let mut paths: Vec<String> = all
                    .iter()
                    .filter(|t| t["tier"] == "P0")
                    .filter_map(|t| t["path"].as_str().map(str::to_string))
                    .take(5)
                    .collect();
                if paths.is_empty() {
                    paths = all
                        .iter()
                        .filter_map(|t| t["path"].as_str().map(str::to_string))
                        .take(5)
                        .collect();
                }
                let kws: Vec<String> = data["keywords"]
                    .as_array()
                    .map(|ks| {
                        ks.iter()
                            .filter_map(|k| k.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                (paths, kws)
            } else {
                let q = pixel_rank::tokenize_task(&problem).unwrap_or_default();
                (files.clone(), q.keywords)
            };
            if target_paths.is_empty() {
                return Err(
                    "could not locate target files for this problem — pass --file <path>"
                        .to_string(),
                );
            }
            let plan = rescue_cmd::plan(&root, &problem, &target_paths, &keywords, depth)?;
            if json {
                return print_data(&plan, true);
            }
            for t in plan["targets"].as_array().cloned().unwrap_or_default() {
                println!(
                    "{}{}",
                    t["path"].as_str().unwrap_or("?"),
                    if t["dirty"].as_bool().unwrap_or(false) {
                        "  [DIRTY — has uncommitted changes]"
                    } else {
                        ""
                    }
                );
                for v in t["versions"].as_array().cloned().unwrap_or_default() {
                    println!(
                        "  {}  {}{}",
                        v["short"].as_str().unwrap_or("?"),
                        v["subject"].as_str().unwrap_or(""),
                        if v["suspect"].as_bool().unwrap_or(false) {
                            "  [SUSPECT]"
                        } else {
                            ""
                        }
                    );
                }
                if let Some(rec) = t["recommended"].as_object() {
                    println!(
                        "  → recommended: {} ({})",
                        rec.get("oid").and_then(Value::as_str).unwrap_or("?"),
                        rec.get("reason").and_then(Value::as_str).unwrap_or(""),
                    );
                }
                println!();
            }
            for c in plan["decision"]["caveats"]
                .as_array()
                .cloned()
                .unwrap_or_default()
            {
                eprintln!("⚠ {}", c.as_str().unwrap_or(""));
            }
            if let Some(cmd) = plan["decision"]["options"][0]["command"].as_str() {
                println!("revert: {cmd}");
            }
            println!("fix forward: keep current code and fix the bug in place");
            Ok(())
        }
        Command::FindSymbol { name, path, json } => {
            let data = execute(&path, Request::Symbol { name }, false)?;
            finish_graph_cmd(data, json, |d| {
                let syms = d.get("symbols")?.as_array()?;
                let mut output = String::new();
                if syms.is_empty() {
                    output.push_str("no symbols found\n");
                } else {
                    for s in syms {
                        output.push_str(&symbol_line(s));
                        output.push('\n');
                    }
                }
                envelope_note(d);
                Some(output)
            })?;
            Ok(())
        }
        Command::ListSignatures { file, path, json } => {
            let data = execute(&path, Request::Skeleton { file }, false)?;
            finish_graph_cmd(data, json, |d| {
                let syms = d.get("symbols")?.as_array()?;
                let fname = d.get("file")?.as_str().unwrap_or("");
                let lang = d.get("lang")?.as_str().unwrap_or("");
                let mut output = format!("// {fname} [{lang}]\n");
                if syms.is_empty() {
                    output.push_str("// (no indexed symbols — run `pixel build-index .` first)\n");
                } else {
                    for s in syms {
                        let kind = s.get("kind")?.as_str().unwrap_or("");
                        let sig = s.get("sig")?.as_str().unwrap_or("");
                        let line = s.get("start_line")?.as_u64().unwrap_or(0);
                        if !sig.is_empty() {
                            output.push_str(&format!("  L{line:>5}  {kind}  {sig}\n"));
                        }
                    }
                }
                Some(output)
            })?;
            Ok(())
        }
        Command::Note {
            action,
            file,
            target,
            note,
            path,
            json,
        } => {
            let data = execute(
                &path,
                Request::Note {
                    action: action.clone(),
                    file,
                    target,
                    note,
                },
                false,
            )?;
            finish_graph_cmd(data, json, |d| {
                if let Some(notes) = d.get("notes").and_then(Value::as_array) {
                    // `note list` output: grouped by file.
                    if notes.is_empty() {
                        return Some("no notes\n".to_string());
                    }
                    let mut output = String::new();
                    for n in notes {
                        output.push_str(&format!(
                            "{}  {}  {}\n",
                            n.get("file_path").and_then(Value::as_str).unwrap_or("?"),
                            n.get("target").and_then(Value::as_str).unwrap_or("?"),
                            n.get("note").and_then(Value::as_str).unwrap_or("?"),
                        ));
                    }
                    return Some(output);
                }
                if let Some(n) = d.get("note") {
                    // `note get` / `note set` echo.
                    return Some(match n.as_str() {
                        Some(text) => format!(
                            "{}  {}  {}\n",
                            d.get("file").and_then(Value::as_str).unwrap_or("?"),
                            d.get("target").and_then(Value::as_str).unwrap_or("?"),
                            text
                        ),
                        None => "no note\n".to_string(),
                    });
                }
                if d.get("removed").is_some() {
                    return Some(format!(
                        "removed {}\n",
                        d.get("removed").and_then(Value::as_bool).unwrap_or(false)
                    ));
                }
                None
            })?;
            Ok(())
        }
        Command::RepoMap {
            path,
            markdown,
            json,
        } => {
            let data = execute(&path, Request::Map { markdown }, false)?;
            operation_metrics::observe(&data);
            if json {
                return print_data(&data, true);
            }
            announce_graph_build(&data);
            if markdown && let Some(md) = data.get("markdown").and_then(Value::as_str) {
                return write_stdout(md);
            }
            // Compact outline: one line per file, indented symbol names.
            let files = data.get("files").and_then(Value::as_array);
            let Some(files) = files else {
                return print_data(&data, false);
            };
            let mut output = String::new();
            for f in files {
                let path = f.get("path").and_then(Value::as_str).unwrap_or("?");
                output.push_str(path);
                output.push('\n');
                if let Some(syms) = f.get("symbols").and_then(Value::as_array) {
                    for s in syms {
                        output.push_str(&format!(
                            "  {} {}\n",
                            s.get("kind").and_then(Value::as_str).unwrap_or("?"),
                            s.get("name").and_then(Value::as_str).unwrap_or("?"),
                        ));
                    }
                }
            }
            write_stdout(&output)
        }
        Command::PackContext {
            uid,
            path,
            budget,
            json,
        } => {
            if call_guard_check("pack-context", &format!("{uid} {}", path.display())) {
                return Err("circuit breaker: repeated context calls".to_string());
            }
            let data = execute(
                &path,
                Request::Context {
                    uid,
                    budget_tokens: budget,
                },
                false,
            )?;
            finish_graph_cmd(data, json, |d| {
                let mut output = String::new();
                if let Some(s) = d.get("symbol") {
                    output.push_str(&symbol_line(s));
                    output.push('\n');
                }
                let text = d.get("text").and_then(Value::as_str).unwrap_or("");
                if !text.is_empty() {
                    output.push('\n');
                    output.push_str(text);
                    output.push('\n');
                } else {
                    output.push_str(&format!(
                        "\nincoming: {}\n",
                        serde_json::to_string_pretty(d.get("incoming").unwrap_or(&Value::Null))
                            .unwrap_or_default()
                    ));
                    output.push_str(&format!(
                        "outgoing: {}\n",
                        serde_json::to_string_pretty(d.get("outgoing").unwrap_or(&Value::Null))
                            .unwrap_or_default()
                    ));
                }
                envelope_note(d);
                Some(output)
            })?;
            Ok(())
        }
        Command::Impact {
            uid_or_name,
            path,
            direction,
            depth,
            workspace,
            json,
        } => {
            if call_guard_check("impact", &format!("{uid_or_name} {}", path.display())) {
                return Err("circuit breaker: repeated impact calls".to_string());
            }
            let dir = match direction {
                DirectionArg::Upstream => "upstream",
                DirectionArg::Downstream => "downstream",
            };
            if workspace {
                let results = workspace_cmd::fan_out(&path, &|| Request::Impact {
                    uid_or_name: uid_or_name.clone(),
                    direction: dir.to_string(),
                    depth,
                })?;
                return workspace_cmd::print_fan_out(&results, json);
            }
            let data = execute(
                &path,
                Request::Impact {
                    uid_or_name,
                    direction: dir.to_string(),
                    depth,
                },
                false,
            )?;
            finish_graph_cmd(data, json, |_| None)?;
            Ok(())
        }
        Command::WhoCalls {
            uid_or_name,
            path,
            role,
            offset,
            workspace,
            json,
        } => {
            let role_s = match role {
                RoleArg::Callers => "callers",
                RoleArg::Callees => "callees",
            };
            if workspace {
                let results = workspace_cmd::fan_out(&path, &|| Request::Uses {
                    uid_or_name: uid_or_name.clone(),
                    role: role_s.to_string(),
                    offset: Some(offset),
                })?;
                return workspace_cmd::print_fan_out(&results, json);
            }
            let data = execute(
                &path,
                Request::Uses {
                    uid_or_name,
                    role: role_s.to_string(),
                    offset: Some(offset),
                },
                false,
            )?;
            finish_graph_cmd(data, json, |d| {
                let edges = d.get("edges")?.as_array()?;
                let role = d.get("role").and_then(Value::as_str).unwrap_or("?");
                let mut output = String::new();
                if let Some(s) = d.get("symbol") {
                    output.push_str(&symbol_line(s));
                    output.push('\n');
                }
                output.push_str(&format!(
                    "{role}: {}/{} (offset {})\n",
                    edges.len(),
                    d.get("total_edges").and_then(Value::as_u64).unwrap_or(0),
                    d.get("offset").and_then(Value::as_u64).unwrap_or(0),
                ));
                for e in edges {
                    let tier = e.get("tier").and_then(Value::as_str).unwrap_or("?");
                    let line = e.get("site_line").and_then(Value::as_u64).unwrap_or(0);
                    match e.get("symbol").filter(|s| !s.is_null()) {
                        Some(s) => output
                            .push_str(&format!("  [{tier}] line {line}  {}\n", symbol_line(s))),
                        None => {
                            output.push_str(&format!("  [{tier}] line {line}  <unknown symbol>\n"))
                        }
                    }
                }
                envelope_note(d);
                Some(output)
            })?;
            Ok(())
        }
        Command::Rename {
            name,
            new_name,
            path,
            file,
            uid,
            dry_run,
            json,
        } => {
            let data = execute(
                &path,
                Request::Rename {
                    name,
                    new_name,
                    file,
                    uid,
                    dry_run,
                },
                false,
            )?;
            finish_graph_cmd(data, json, |d| {
                let mut output = String::new();
                let old = d.get("old_name").and_then(Value::as_str).unwrap_or("?");
                let new = d.get("new_name").and_then(Value::as_str).unwrap_or("?");
                let count = d.get("edit_count").and_then(Value::as_u64).unwrap_or(0);
                let verb = if d.get("dry_run").and_then(Value::as_bool) == Some(true) {
                    "would rename"
                } else {
                    "renamed"
                };
                output.push_str(&format!("{verb} {old} → {new} ({count} sites)\n"));
                for f in d.get("edits")?.as_array()? {
                    let path = f.get("path").and_then(Value::as_str).unwrap_or("?");
                    let kinds: Vec<String> = f
                        .get("edits")
                        .and_then(Value::as_array)
                        .map(|es| {
                            es.iter()
                                .map(|e| {
                                    format!(
                                        "L{}:{}",
                                        e.get("line").and_then(Value::as_u64).unwrap_or(0),
                                        e.get("kind").and_then(Value::as_str).unwrap_or("?"),
                                    )
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    output.push_str(&format!("  {path}  ({})\n", kinds.join(", ")));
                }
                for s in d.get("skipped")?.as_array()? {
                    output.push_str(&format!(
                        "  skipped {}:{} — {}\n",
                        s.get("path").and_then(Value::as_str).unwrap_or("?"),
                        s.get("line").and_then(Value::as_u64).unwrap_or(0),
                        s.get("reason").and_then(Value::as_str).unwrap_or("?"),
                    ));
                }
                if let Some(obj) = d.get("unclaimed_text").and_then(Value::as_object)
                    && !obj.is_empty()
                {
                    let details: Vec<String> = obj
                        .iter()
                        .map(|(p, n)| format!("{p} ({})", n.as_u64().unwrap_or(0)))
                        .collect();
                    output.push_str(&format!(
                        "  note: unclaimed occurrences of the old name remain: {}\n",
                        details.join(", ")
                    ));
                }
                Some(output)
            })?;
            Ok(())
        }
        Command::CallPath {
            from,
            to,
            path,
            json,
        } => {
            let data = execute(&path, Request::Trace { from, to }, false)?;
            finish_graph_cmd(data, json, |_| None)?;
            Ok(())
        }
        Command::Evaluate {
            cmd:
                EvaluateCmd::Path {
                    from,
                    to,
                    traversal,
                    tiers,
                    max_depth,
                    time_budget_ms,
                    scope,
                    at_snapshot,
                    path,
                    json,
                },
        } => {
            owned_exit.set(Some(evaluate_cmd::run(evaluate_cmd::EvaluateOptions {
                from,
                to,
                traversal,
                tiers,
                max_depth,
                time_budget_ms,
                scope,
                at_snapshot,
                path,
                json,
            })));
            Ok(())
        }
        Command::ListFlows { path, offset, json } => {
            let data = execute(
                &path,
                Request::Processes {
                    offset: Some(offset),
                },
                false,
            )?;
            finish_graph_cmd(data, json, |_| None)?;
            Ok(())
        }
        Command::ListAreas { path, offset, json } => {
            let data = execute(
                &path,
                Request::Clusters {
                    offset: Some(offset),
                },
                false,
            )?;
            finish_graph_cmd(data, json, |_| None)?;
            Ok(())
        }
        Command::WhatChanged {
            path,
            base,
            offset,
            tests,
            json,
        } => {
            if call_guard_check("what-changed", &format!("{} {:?}", path.display(), base)) {
                return Err("circuit breaker: repeated changes calls".to_string());
            }
            let data = execute(
                &path,
                Request::Changes {
                    base,
                    offset: Some(offset),
                    include_tests: tests,
                },
                false,
            )?;
            finish_graph_cmd(data, json, |_| None)?;
            Ok(())
        }
        Command::RebuildGraph { path, json } => {
            let v = execute(&path, Request::Graph {}, false)?;
            eprintln!(
                "graph built in {} ms -> {}",
                v.get("elapsed_ms").and_then(Value::as_u64).unwrap_or(0),
                path.join(pixel_index::index::SHARD_DIR)
                    .join("graph.db")
                    .display()
            );
            print_data(&v, json)?;
            Ok(())
        }
        Command::Coverage { path, json } => {
            coverage_cmd::run(coverage_cmd::CoverageOptions { path, json })
        }
        Command::Workspace { cmd } => workspace_cmd::run(cmd),
        Command::IndexPack {
            out,
            include_history,
            path,
        } => index_cmd::run(index_cmd::IndexCmd::Pack {
            out,
            include_history,
            path,
        }),
        Command::IndexUnpack {
            source,
            force,
            path,
        } => index_cmd::run(index_cmd::IndexCmd::Unpack {
            source,
            force,
            path,
        }),
        Command::Mcp { path } => mcp_cmd::run(path),
        Command::Status {
            path,
            json,
            statusline,
        } => {
            let mut data = execute(&path, Request::Status {}, false)?;
            // `status` is the compact freshness answer: keep the snapshot's
            // head/branch but collapse the dirty list to a count.
            compact_snapshot(&mut data);
            // The daemon/service now attaches a rich `facts` block itself
            // (schema version, phase-A state, hunk/gram counts). Only fill in
            // the client-side fallback when talking to an older daemon that
            // doesn't send one.
            if data.get("facts").is_none_or(serde_json::Value::is_null)
                && let Some(facts) = facts_status(&path)
            {
                data["facts"] = facts;
            }
            if statusline {
                // Compact one-liner — size + freshness + enrichment, so a
                // shell prompt/statusline shows staleness & coverage without
                // ever needing an explicit `pixel doctor`:
                //   `pixel: 285f 12k sym 45k edges 180/200 commits 90%diff fresh`
                let files = data
                    .get("index")
                    .and_then(|i| i.get("base_files").and_then(Value::as_u64))
                    .unwrap_or(0);
                let (sym, edges) = match data.get("graph") {
                    Some(g) if g.get("present").and_then(Value::as_bool).unwrap_or(false) => (
                        g.get("symbols").and_then(Value::as_u64).unwrap_or(0),
                        g.get("edges").and_then(Value::as_u64).unwrap_or(0),
                    ),
                    _ => (0, 0),
                };
                let fmt = |n: u64| -> String {
                    if n >= 1000 {
                        format!("{}k", n / 1000)
                    } else {
                        n.to_string()
                    }
                };
                let mut line = format!(
                    "pixel: {}f {} sym {} edges",
                    fmt(files),
                    fmt(sym),
                    fmt(edges)
                );
                // Enrichment coverage (only when the facts db exists and has
                // enough history to report a meaningful fraction): commits
                // indexed + diff-text coverage %, plus the fresh/stale state.
                if let Some(f) = data.get("facts") {
                    if let (Some(ci), Some(tc)) = (
                        f.get("commits_indexed").and_then(Value::as_u64),
                        f.get("total_commits").and_then(Value::as_u64),
                    ) {
                        if tc > 0 {
                            line.push_str(&format!(" {ci}/{tc}"));
                        }
                        let covered = ci == tc;
                        if let Some(pct) = f.get("diff_indexed_pct").and_then(Value::as_f64) {
                            line.push_str(&format!(
                                " {:.0}%diff",
                                if covered {
                                    // Full commit coverage implies the diff
                                    // text is indexed too; report a clean 100%.
                                    if pct >= 99.0 { 100.0 } else { pct }
                                } else {
                                    pct
                                }
                            ));
                        }
                    }
                    let state = if f.get("fresh").and_then(Value::as_bool).unwrap_or(false) {
                        "fresh"
                    } else {
                        "stale"
                    };
                    line.push_str(&format!(" {state}"));
                } else {
                    line.push_str(" ?facts");
                }
                write_stdout(&format!("{line}\n"))?;
                return Ok(());
            }
            if json {
                print_data(&data, true)?;
            } else {
                let mut output = format!(
                    "root: {}\n",
                    data.get("root").and_then(Value::as_str).unwrap_or("?")
                );
                if let Some(i) = data.get("index") {
                    output.push_str(&format!(
                        "index: commit={} base_files={} delta_files={} overlay_files={} tombstones={}\n",
                        i.get("commit_oid").and_then(Value::as_str).unwrap_or("-"),
                        i.get("base_files").and_then(Value::as_u64).unwrap_or(0),
                        i.get("delta_files").and_then(Value::as_u64).unwrap_or(0),
                        i.get("overlay_files").and_then(Value::as_u64).unwrap_or(0),
                        i.get("tombstones").and_then(Value::as_u64).unwrap_or(0),
                    ));
                }
                match data.get("graph") {
                    Some(g) if g.get("present").and_then(Value::as_bool).unwrap_or(false) => {
                        output.push_str(&format!(
                            "graph: files={} symbols={} edges={} unresolved_calls={}\n",
                            g.get("files").and_then(Value::as_u64).unwrap_or(0),
                            g.get("symbols").and_then(Value::as_u64).unwrap_or(0),
                            g.get("edges").and_then(Value::as_u64).unwrap_or(0),
                            g.get("unresolved_calls")
                                .and_then(Value::as_u64)
                                .unwrap_or(0),
                        ));
                    }
                    _ => output.push_str("graph: not built (runs on first graph command)\n"),
                }
                if let Some(f) = data.get("facts") {
                    output.push_str(&format!(
                        "facts: phase={} commits={}/{} diff_coverage={:.0}% fresh={} schema_version={}\n",
                        f.get("phase").and_then(Value::as_str).unwrap_or("?"),
                        f.get("commits_indexed").and_then(Value::as_u64).unwrap_or(0),
                        f.get("total_commits").and_then(Value::as_u64).unwrap_or(0),
                        f.get("diff_indexed_pct").and_then(Value::as_f64).unwrap_or(0.0) * 100.0,
                        f.get("fresh").and_then(Value::as_bool).unwrap_or(false),
                        f.get("schema_version").and_then(Value::as_i64).unwrap_or(0),
                    ));
                    // Only the daemon-side block carries the poisoning
                    // counters; print them when present.
                    if let (Some(h), Some(g)) = (
                        f.get("hunks_with_text").and_then(Value::as_u64),
                        f.get("diff_grams").and_then(Value::as_u64),
                    ) {
                        output
                            .push_str(&format!("facts-text: hunks_with_text={h} diff_grams={g}\n"));
                    }
                }
                output.push_str(&format!(
                    "daemon: {}\n",
                    if daemon_ping(&path) {
                        "running"
                    } else {
                        "not running"
                    }
                ));
                write_stdout(&output)?;
            }
            Ok(())
        }
        Command::PrepareRepo {
            path,
            no_daemon,
            json,
        } => ready(path, no_daemon, json),
        Command::IndexStats { path } => {
            let path = discover_root(&path)?;
            let shard = Shard::open(&shard_path(&path)).map_err(|e| e.to_string())?;
            let _ = extractor_for_shard(&shard); // validates extractor id
            write_stdout(&format!(
                "files={} grams={} extractor={} commit={}\n",
                shard.file_count(),
                shard.gram_count(),
                shard.extractor_id(),
                shard.commit_oid().unwrap_or("-")
            ))?;
            Ok(())
        }
        Command::Daemon { cmd } => match cmd {
            DaemonCmd::Start { path, foreground } => {
                daemon_start(discover_root(&path)?, foreground, false)
            }
            DaemonCmd::Stop { path } => daemon_stop(discover_root(&path)?),
            DaemonCmd::Status { path } => daemon_status(discover_root(&path)?),
        },
        Command::Recall { cmd } => recall_cmd::run_recall(cmd),
        Command::ListErrors { cmd } => sniper_cmd::run_sniper(cmd),
        Command::WebSearch { query, limit, json } => {
            web_search::run(web_search::WebSearchOptions { query, limit, json })
        }
        // -------------------------------------------------------------
        // M2 — safe git mutation ops (pixel-ops)
        // -------------------------------------------------------------
        Command::RepoState {
            path,
            files,
            include_clean,
            json,
        } => {
            let root = discover_root(&path)?;
            let mut data = pixel_ops::inspect::inspect(&root)?;
            if !files.is_empty() {
                // Filter the dirty/clean lists to the requested paths.
                if let Some(dirty) = data.get_mut("dirty").and_then(Value::as_array_mut) {
                    dirty.retain(|d| {
                        d.get("path")
                            .and_then(Value::as_str)
                            .is_some_and(|p| files.iter().any(|f| f == p))
                    });
                }
                if let Some(clean) = data.get_mut("clean").and_then(Value::as_array_mut) {
                    clean.retain(|c| c.as_str().is_some_and(|p| files.iter().any(|f| f == p)));
                }
                data["dirty_count"] = json!(
                    data.get("dirty")
                        .and_then(Value::as_array)
                        .map_or(0, Vec::len)
                );
                data["clean_count"] = json!(
                    data.get("clean")
                        .and_then(Value::as_array)
                        .map_or(0, Vec::len)
                );
            }
            if !include_clean {
                compact_repo_state(&mut data);
            }
            print_data(&data, json)
        }
        Command::ReviewChanges {
            path,
            cursor,
            byte_cap,
            json,
        } => {
            let root = discover_root(&path)?;
            let data = pixel_ops::review::review(&root, cursor.as_deref(), byte_cap)?;
            print_data(&data, json)
        }
        Command::CommitHistory {
            path,
            ref_,
            limit,
            detail,
            cursor,
            byte_cap,
            json,
        } => {
            let root = discover_root(&path)?;
            let data = pixel_ops::history::history(
                &root,
                ref_.as_deref(),
                limit,
                &detail,
                cursor.as_deref(),
                byte_cap,
            )?;
            print_data(&data, json)
        }
        Command::Diff {
            from,
            to,
            path,
            paths,
            byte_cap,
            json,
        } => {
            let root = discover_root(&path)?;
            let paths_opt = if paths.is_empty() {
                None
            } else {
                Some(paths.as_slice())
            };
            let data = pixel_ops::diff::diff(&root, &from, to.as_deref(), paths_opt, byte_cap)?;
            print_data(&data, json)
        }
        Command::Commit {
            message,
            message_file,
            path,
            files,
            push,
            amend,
            expected_head,
            request_id,
            json,
        } => {
            let root = discover_root(&path)?;
            let message = commit_message(message, message_file.as_deref())?;
            let opts = pixel_ops::publish::PublishOptions {
                message,
                files,
                expected_head,
                expected_fingerprints: std::collections::BTreeMap::new(),
                push,
                amend,
                request_id,
            };
            let data = pixel_ops::publish::publish(&root, &opts, None)?;
            print_data(&data, json)
        }
        Command::Push {
            remote,
            refspec,
            path,
            force_with_lease,
            request_id,
            json,
        } => {
            let root = discover_root(&path)?;
            let opts = pixel_ops::push::PushOptions {
                remote,
                refspec,
                request_id,
                force_with_lease,
            };
            let data = pixel_ops::push::push(&root, &opts, None)?;
            print_data(&data, json)
        }
        Command::CommitAndPush {
            message,
            message_file,
            path,
            files,
            remote,
            refspec,
            force_with_lease,
            request_id,
            json,
        } => {
            let root = discover_root(&path)?;
            let message = commit_message(message, message_file.as_deref())?;
            let data = pixel_ops::ship::ship_with_lease(
                &root,
                &message,
                &files,
                &remote,
                &refspec,
                &request_id,
                force_with_lease,
            )?;
            print_data(&data, json)
        }
        Command::NewBranch {
            name,
            path,
            from,
            request_id,
            json,
        } => {
            let root = discover_root(&path)?;
            let opts = pixel_ops::branch::BranchOptions {
                name,
                from,
                request_id,
            };
            let data = pixel_ops::branch::branch(&root, &opts)?;
            print_data(&data, json)
        }
        Command::FastForward {
            path,
            expected_head,
            target_oid,
            request_id,
            json,
        } => {
            let root = discover_root(&path)?;
            let opts = pixel_ops::update::UpdateOptions {
                expected_head,
                target_oid,
                request_id,
            };
            let data = pixel_ops::update::update(&root, &opts)?;
            print_data(&data, json)
        }
        Command::Fetch {
            remote,
            path,
            refspec,
            json,
        } => {
            let root = discover_root(&path)?;
            let data = pixel_ops::sync::sync(&root, &remote, refspec.as_deref())?;
            print_data(&data, json)
        }
        // -------------------------------------------------------------
        // M3/M4 — engines
        // -------------------------------------------------------------
        Command::FindCode {
            phrase,
            path,
            limit,
            json,
        } => {
            if call_guard_check("find-code", &format!("{phrase} {}", path.display())) {
                return Err("circuit breaker: repeated resolve calls".to_string());
            }
            let mut data = execute(&path, Request::Resolve { phrase, limit }, false)?;
            if let Ok(root) = discover_root(&path) {
                enrich_resolve_matches_with_context(&mut data, &root);
            }
            if json {
                print_data(&data, true)
            } else {
                print_resolve_human(&data)
            }
        }
        Command::SearchHistory {
            query,
            path,
            facet,
            limit,
            json,
        } => {
            let data = execute(
                &path,
                Request::History {
                    query,
                    facet: Some(facet),
                    limit,
                },
                false,
            )?;
            print_data(&data, json)
        }
        Command::FileHistory {
            path,
            file,
            token,
            json,
        } => {
            let data = execute(&path, Request::Lifecycle { path: file, token }, false)?;
            print_data(&data, json)
        }
        Command::DigHistory {
            path,
            phrase,
            file,
            from,
            to,
            limit,
            show,
            parent,
            json,
        } => {
            if let Some(oid) = show {
                return excavate_show(&path, &oid, file.as_deref(), parent, json);
            }
            let data = execute(
                &path,
                Request::Excavate {
                    phrase,
                    path: file,
                    from,
                    to,
                    limit,
                },
                false,
            )?;
            print_data(&data, json)
        }
        Command::SyncBranch {
            path,
            strategy,
            push,
            into,
            request_id,
            json,
        } => {
            let data = execute(
                &path,
                Request::Reconcile {
                    strategy: Some(strategy),
                    push: Some(push),
                    into,
                    request_id,
                },
                false,
            )?;
            print_data(&data, json)
        }
        Command::RecordEvent {
            kind,
            path,
            file,
            detail,
            json,
        } => {
            let data = execute(
                &path,
                Request::Journal {
                    kind,
                    path: file,
                    detail,
                },
                false,
            )?;
            print_data(&data, json)
        }
        // -------------------------------------------------------------
        // M5/M6 — install / doctor / migrate / hook
        // -------------------------------------------------------------
        Command::Install { json, shell } => {
            let report = pixel_install::install::install(&pixel_install::install::InstallOptions {
                shell,
                ..Default::default()
            })
            .map_err(|e| e.to_string())?;
            print_data(
                &serde_json::to_value(&report).map_err(|e| e.to_string())?,
                json,
            )
        }
        Command::Uninstall {
            json,
            dry_run,
            binary_path,
            shell,
            wrappers_only,
        } => {
            let report =
                pixel_install::uninstall::uninstall(&pixel_install::uninstall::UninstallOptions {
                    binary_path,
                    dry_run,
                    shell,
                    wrappers_only,
                    ..Default::default()
                })
                .map_err(|e| e.to_string())?;
            print_data(
                &serde_json::to_value(&report).map_err(|e| e.to_string())?,
                json,
            )
        }
        Command::CheckRelease {
            version,
            repo,
            json,
        } => {
            let report = pixel_release::run(&repo, &version)?;
            if json {
                print_data(&report.to_json(), true)?;
            } else {
                write_stdout(&report.render())?;
            }
            if report.ok() {
                Ok(())
            } else {
                Err("release-check failed".to_string())
            }
        }
        Command::SelfUpdate {
            build,
            install_path,
            dev,
            restart_daemon,
            repo,
            dry_run,
        } => {
            let home = std::env::var("HOME").map_err(|_| "HOME not set".to_string())?;
            let path_var = std::env::var_os("PATH");
            let target = if dev {
                UpgradeTarget {
                    path: dev_install_path(Path::new(&home)),
                    source: "--dev",
                    explicit: false,
                }
            } else {
                resolve_upgrade_target(
                    install_path,
                    std::env::current_exe().ok(),
                    path_var.as_deref(),
                    Path::new(&home),
                )
            };
            let refusal = upgrade_target_refusal(
                &target,
                &package_manager_roots(
                    Path::new(&home),
                    std::env::var_os("MISE_DATA_DIR").as_deref(),
                    std::env::var_os("HOMEBREW_CELLAR").as_deref(),
                ),
            );
            let dest = target.path;
            eprintln!("Install path: {} ({})", dest.display(), target.source);
            if dry_run {
                if let Some(other) = upgrade_shadowed_by(&dest, path_var.as_deref()) {
                    eprintln!("warning: {} precedes that path on PATH", other.display());
                }
                write_stdout(&format!("{}\n", dest.display()))?;
            }
            // A dry run still fails on a refused path, so
            // `pixel self-update --dry-run && pixel self-update` never writes.
            if let Some(reason) = refusal {
                return Err(reason);
            }
            if dry_run {
                return Ok(());
            }
            // 1. Build.
            eprintln!("Building: {build}");
            let status = std::process::Command::new("sh")
                .arg("-c")
                .arg(&build)
                .status()
                .map_err(|e| format!("build failed: {e}"))?;
            if !status.success() {
                return Err(format!("build exited with status {status}"));
            }
            // 2. Find the built binary (target/<profile>/pixel relative to
            //    cwd, profile taken from the build command's own flags).
            let src = std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join("target")
                .join(cargo_profile_dir(&build))
                .join("pixel");
            if !src.is_file() {
                return Err(format!("built binary not found at {}", src.display()));
            }
            // 3. Address only this repository; never signal unrelated processes.
            // Use the non-starting client: stopping must not launch a daemon.
            let repo_path = repo
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
                .canonicalize()
                .map_err(|e| format!("upgrade repository path: {e}"))?;
            // 4. Atomic install: copy to temp, then rename. In-place cp
            //    overwrites a mapped Mach-O on macOS, invalidating the
            //    ad-hoc code signature and causing SIGKILL on next run.
            eprintln!("Installing to {}", dest.display());
            // `$$` is a shell idiom and is NOT expanded by Rust, so a literal
            // name would collide between concurrent upgrades. Use the real pid,
            // and keep the temp file in `dest`'s directory so the rename stays
            // atomic (same filesystem). Mirrors scripts/install.sh's `.pixel.tmp.$$`.
            let tmp = dest.with_file_name(format!(
                ".{}.tmp.{}",
                dest.file_name().and_then(|n| n.to_str()).unwrap_or("pixel"),
                std::process::id()
            ));
            if let Some(parent) = dest.parent().filter(|p| !p.as_os_str().is_empty()) {
                std::fs::create_dir_all(parent).map_err(|e| format!("install directory: {e}"))?;
            }
            std::fs::copy(&src, &tmp).map_err(|e| {
                let _ = std::fs::remove_file(&tmp);
                format!("copy failed: {e}")
            })?;
            std::fs::rename(&tmp, &dest).map_err(|e| {
                let _ = std::fs::remove_file(&tmp);
                format!("rename failed: {e}")
            })?;
            eprintln!("Installed binary: {} -> {}", src.display(), dest.display());
            if let Some(other) = upgrade_shadowed_by(&dest, path_var.as_deref()) {
                eprintln!(
                    "warning: {} precedes the upgraded binary on PATH; `pixel` will keep \
                     running that copy until it is removed or PATH is reordered",
                    other.display()
                );
            }
            // 5. Stop only the selected repository after installation succeeds.
            if let Some(response) = upgrade_daemon_request(&repo_path, &Request::Shutdown)? {
                if !response.ok {
                    return Err("installed binary, but repository daemon refused shutdown".into());
                }
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                loop {
                    if upgrade_daemon_socket_stopped(&repo_path)? {
                        break;
                    }
                    if std::time::Instant::now() >= deadline {
                        return Err("installed binary, but repository daemon did not stop".into());
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
            if restart_daemon {
                eprintln!("Starting daemon in {}...", repo_path.display());
                let status = std::process::Command::new(&dest)
                    .arg("daemon")
                    .arg("start")
                    .arg(&repo_path)
                    .status()
                    .map_err(|e| format!("installed binary, but restart failed: {e}"))?;
                if !status.success() {
                    return Err(format!(
                        "installed binary, but restart exited with {status}"
                    ));
                }
            }
            eprintln!("Upgrade complete: {}", dest.display());
            Ok(())
        }
        Command::Doctor { path, json, shell } => {
            let root = discover_root(&path)?;
            let report = pixel_install::doctor::doctor(&pixel_install::doctor::DoctorOptions {
                repo_root: Some(root),
                shell,
                // Hand the doctor this binary's REAL clap parser so the
                // rule-vs-binary parity check dry-runs every `pixel …` line
                // documented in the installed rule text against the actual
                // CLI definition — documented-but-rejected syntax goes red.
                syntax_validator: Some(validate_cli_syntax),
                ..Default::default()
            })
            .map_err(|e| e.to_string())?;
            print_data(
                &serde_json::to_value(&report).map_err(|e| e.to_string())?,
                json,
            )
        }
        Command::Migrate { .. } => {
            eprintln!("{MIGRATE_REMOVED_NOTE}");
            Ok(())
        }
        Command::RunHook { cmd } => match cmd {
            HookCmd::Guard {
                path: _,
                provider,
                delegate_rtk,
            } => {
                guard::run(provider, delegate_rtk);
            }
            HookCmd::ComposedGuard { provider, backup } => {
                if provider == guard::Provider::Codex {
                    guard::run_composed_codex(&backup);
                }
                std::process::exit(0);
            }
            HookCmd::SessionStart { path } => {
                let root = discover_root(&path)?;
                // Advertise the commands the agent types, read from the
                // parser itself so the block cannot name one that does not
                // exist. The daemon's wire op tags (`targets`, `update`,
                // `sync`) are not commands: `pixel update` is fast-forward,
                // `pixel sync` is fetch.
                let ops = session_commands();
                // The usage doctrine is a shared constant beside the op
                // registry (pixel-proto), so the injected text, the doctor's
                // scenario-consistency check, and the rule file can never
                // silently disagree on the five mandatory scenarios.
                let mut pixel = serde_json::json!({
                    "capabilities": ops,
                    "protocol_version": PROTOCOL_VERSION,
                    "usage": pixel_proto::op::SESSION_USAGE,
                });
                // Per-repo freshness: index commit, graph presence, facts
                // phase/fresh. Best-effort — if status can't be read (not a
                // git repo, index not built), the capability block still
                // stands and the repo field is simply omitted.
                //
                // The probe is hard-bounded by a deadline. "Best-effort"
                // has to mean it, because `Status` on a root that is not a
                // git repo and has no shards walks the entire tree: a
                // session started in a plain directory (a home directory,
                // `/tmp`) would otherwise hang the hook forever and the
                // agent would receive no capability block at all — the exact
                // failure this hook exists to prevent. A presence check on
                // `.git`/`.pixel` is not enough of a guard: a bare
                // `.pixel/history.db` left in a home directory by any
                // history op makes that directory look indexed.
                //
                // On timeout the block is emitted without `repo` and the
                // still-running probe dies with the process.
                let probe = {
                    let root = root.clone();
                    let (tx, rx) = std::sync::mpsc::channel();
                    std::thread::spawn(move || {
                        let _ = tx.send(execute(&root, Request::Status {}, true));
                    });
                    rx.recv_timeout(SESSION_STATUS_PROBE_TIMEOUT).ok()
                };
                if let Some(Ok(data)) = probe {
                    let mut repo = serde_json::Map::new();
                    if let Some(i) = data.get("index") {
                        repo.insert(
                            "index_commit".into(),
                            i.get("commit_oid").cloned().unwrap_or(Value::Null),
                        );
                    }
                    let graph_present = data
                        .get("graph")
                        .and_then(|g| g.get("present"))
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    repo.insert("graph_present".into(), Value::Bool(graph_present));
                    if let Some(f) = facts_status(&root) {
                        repo.insert(
                            "facts_phase".into(),
                            f.get("phase").cloned().unwrap_or(Value::Null),
                        );
                        repo.insert(
                            "facts_fresh".into(),
                            f.get("fresh").cloned().unwrap_or(Value::Bool(false)),
                        );
                    }
                    pixel["repo"] = Value::Object(repo);
                }
                let block = serde_json::json!({ "pixel": pixel });
                write_stdout(&serde_json::to_string_pretty(&block).map_err(|e| e.to_string())?)?;
                Ok(())
            }
            HookCmd::PromptSubmit { provider } => {
                // Task boundary detector — reads UserPromptSubmit payload
                // from stdin, embeds prompt + context, emits advisory if a
                // boundary is detected. Never returns (exits 0 or via the
                // emit function).
                prompt_submit::run(provider);
            }
            HookCmd::PostCompaction { provider } => {
                // Post-compaction re-injection — reads PostCompaction
                // payload from stdin, finds the active targets manifest,
                // and emits it as additionalContext. Never returns.
                post_compaction::run(provider);
            }
            HookCmd::PostToolUse { provider } => {
                // P0·3 blast-radius — reads stdin, forces the `PostToolUse`
                // event, and emits a NON-BLOCKING advisory listing what was just
                // changed's dependants. Never returns.
                guard::run_post_tool_use(provider);
            }
        },
        Command::TaskState { cmd } => match cmd {
            TaskCmd::Begin {
                objective,
                path,
                provider,
                session,
                json,
            } => {
                let root = discover_root(&path)?;
                let record = task_runtime::begin(&root, &objective, &provider, session.as_deref())?;
                print_data(
                    &serde_json::to_value(record).map_err(|e| e.to_string())?,
                    json,
                )
            }
            TaskCmd::Accept {
                objective,
                path,
                provider,
                session,
                json,
            } => {
                let root = discover_root(&path)?;
                let record =
                    task_runtime::accept_task(&root, &objective, &provider, session.as_deref())?;
                print_data(
                    &serde_json::to_value(record).map_err(|e| e.to_string())?,
                    json,
                )
            }
            TaskCmd::Prepare {
                task_id,
                path,
                json,
            } => {
                let root = discover_root(&path)?;
                let data = task_runtime::prepare(&root, &task_id)?
                    .map(|record| serde_json::to_value(record).map_err(|e| e.to_string()))
                    .transpose()?
                    .unwrap_or_else(|| json!({"task_id": task_id, "status": "absent"}));
                print_data(&data, json)
            }
            TaskCmd::Status {
                task_id,
                path,
                json,
            } => {
                let root = discover_root(&path)?;
                let data = task_runtime::status(&root, &task_id)
                    .map(|record| serde_json::to_value(record).map_err(|e| e.to_string()))
                    .transpose()?
                    .unwrap_or_else(|| json!({"task_id": task_id, "status": "absent"}));
                print_data(&data, json)
            }
            TaskCmd::Events {
                task_id,
                path,
                json,
            } => {
                let root = discover_root(&path)?;
                let events = task_runtime::events(&root, &task_id);
                print_data(&json!({"task_id": task_id, "events": events}), json)
            }
            TaskCmd::SandboxCreate {
                task_id,
                candidate_id,
                owned_paths,
                path,
                json,
            } => {
                let root = discover_root(&path)?;
                let candidate = task_sandbox::create(&root, &task_id, &candidate_id, owned_paths)?;
                print_data(
                    &serde_json::to_value(candidate).map_err(|e| e.to_string())?,
                    json,
                )
            }
            TaskCmd::SandboxInspect {
                task_id,
                candidate_id,
                path,
                json,
            } => {
                let root = discover_root(&path)?;
                let data = match task_sandbox::load(&root, &task_id, &candidate_id)? {
                    Some(candidate) => serde_json::to_value(task_sandbox::inspect(&candidate)?)
                        .map_err(|e| e.to_string())?,
                    None => {
                        json!({"task_id": task_id, "candidate_id": candidate_id, "status": "absent"})
                    }
                };
                print_data(&data, json)
            }
            TaskCmd::SandboxPromote {
                task_id,
                candidate_id,
                path,
                json,
            } => {
                let root = discover_root(&path)?;
                let data = match task_sandbox::load(&root, &task_id, &candidate_id)? {
                    Some(candidate) => serde_json::to_value(task_sandbox::promote(&candidate)?)
                        .map_err(|e| e.to_string())?,
                    None => {
                        json!({"task_id": task_id, "candidate_id": candidate_id, "status": "absent"})
                    }
                };
                print_data(&data, json)
            }
            TaskCmd::SandboxCleanup {
                task_id,
                candidate_id,
                path,
                json,
            }
            | TaskCmd::SandboxCancel {
                task_id,
                candidate_id,
                path,
                json,
            } => {
                let root = discover_root(&path)?;
                let removed = task_sandbox::cleanup(&root, &task_id, &candidate_id)?;
                print_data(
                    &json!({"task_id": task_id, "candidate_id": candidate_id, "removed": removed}),
                    json,
                )
            }
            TaskCmd::WorkerStart {
                task_id,
                candidate_id,
                path,
                executable,
                model,
                max_turns,
                max_budget_usd,
                json,
            } => {
                let root = discover_root(&path)?;
                let config = task_scheduler::WorkerConfig {
                    executable,
                    model,
                    max_turns,
                    max_budget_usd,
                    system_prompt_file: None,
                    subagent_prompt_file: None,
                };
                let record = task_scheduler::start(&root, &task_id, &candidate_id, &config)?;
                print_data(
                    &serde_json::to_value(record).map_err(|e| e.to_string())?,
                    json,
                )
            }
            TaskCmd::WorkerStatus {
                task_id,
                candidate_id,
                path,
                json,
            } => {
                let root = discover_root(&path)?;
                let data = task_scheduler::status(&root, &task_id, &candidate_id)?
                    .map(|status| serde_json::to_value(status).map_err(|e| e.to_string()))
                    .transpose()?
                    .unwrap_or_else(|| json!({"task_id": task_id, "candidate_id": candidate_id, "status": "absent"}));
                print_data(&data, json)
            }
            TaskCmd::WorkerStop {
                task_id,
                candidate_id,
                path,
                json,
            } => {
                let root = discover_root(&path)?;
                let data = task_scheduler::stop(&root, &task_id, &candidate_id)?
                    .map(|record| serde_json::to_value(record).map_err(|e| e.to_string()))
                    .transpose()?
                    .unwrap_or_else(|| json!({"task_id": task_id, "candidate_id": candidate_id, "status": "absent"}));
                print_data(&data, json)
            }
            TaskCmd::RaceStart {
                task_id,
                candidate_ids,
                path,
                executable,
                model,
                max_turns,
                max_budget_usd,
                json,
            } => {
                let root = discover_root(&path)?;
                let config = task_scheduler::WorkerConfig {
                    executable,
                    model,
                    max_turns,
                    max_budget_usd,
                    system_prompt_file: None,
                    subagent_prompt_file: None,
                };
                let records = task_scheduler::start_race(&root, &task_id, &candidate_ids, &config)?;
                print_data(
                    &serde_json::to_value(records).map_err(|e| e.to_string())?,
                    json,
                )
            }
            TaskCmd::RacePoll {
                task_id,
                candidate_ids,
                path,
                json,
            } => {
                let root = discover_root(&path)?;
                let result = task_scheduler::poll_race(&root, &task_id, &candidate_ids)?;
                print_data(
                    &serde_json::to_value(result).map_err(|e| e.to_string())?,
                    json,
                )
            }
            TaskCmd::PlanValidate {
                task_id,
                file,
                path,
                json,
            } => {
                let root = discover_root(&path)?;
                task_runtime::status(&root, &task_id)
                    .ok_or_else(|| "task is absent or corrupt".to_string())?;
                let proposal = std::fs::read_to_string(&file)
                    .map_err(|error| format!("read work plan {}: {error}", file.display()))?;
                let plan = task_plan::parse(&proposal)?;
                let validation = task_plan::validate(&plan);
                print_data(&json!({"task_id": task_id, "validation": validation}), json)
            }
            TaskCmd::Show {
                session,
                path,
                json,
            } => {
                let root = discover_root(&path)?;
                let data = task_runtime::show(&root, &session)?.unwrap_or_else(
                    || json!({"session_id": session, "task_id": Value::Null, "status": "absent"}),
                );
                print_data(&data, json)
            }
            TaskCmd::Reset {
                session,
                path,
                json,
            } => {
                let root = discover_root(&path)?;
                let removed = task_runtime::reset(&root, &session)?;
                print_data(&json!({"session_id": session, "removed": removed}), json)
            }
        },
        Command::ActionLog {
            path,
            limit,
            errors_only,
            json,
            clear,
        } => run_log(&path, limit, errors_only, json, clear),
        Command::TokenSavings {
            path,
            json,
            since_hours,
        } => run_savings(&path, json, since_hours),
        Command::SquashBranch {
            path,
            onto,
            message,
            push,
            remote,
            expected_head,
            allow_default_branch,
            request_id,
            json,
        } => {
            let root = discover_root(&path)?;
            let opts = pixel_ops::rewrite::RewriteOptions {
                onto,
                message,
                push,
                remote,
                request_id,
                expected_head,
                allow_default_branch,
            };
            let data = pixel_ops::rewrite::rewrite(&root, &opts)?;
            print_data(&data, json)
        }
        Command::WhoWrote {
            file,
            path,
            lines,
            author,
            limit_regions,
            json,
        } => {
            let root = discover_root(&path)?;
            let opts = pixel_ops::provenance::ProvenanceOptions {
                file,
                lines,
                author,
                limit_regions,
            };
            let data = pixel_ops::provenance::provenance(&root, &opts)?;
            print_data(&data, json)
        }
        Command::ListBranches {
            path,
            fetch,
            remote,
            stale_days,
            json,
        } => {
            let root = discover_root(&path)?;
            let opts = pixel_ops::branches::BranchesOptions {
                fetch,
                remote,
                stale_days,
            };
            let data = pixel_ops::branches::branches(&root, &opts)?;
            print_data(&data, json)
        }
        Command::EditEnv { cmd } => {
            use pixel_ops::envfile::EnvAction;
            let (path, json, action) = match cmd {
                EnvCmd::Inventory { path, json } => (path, json, EnvAction::Inventory),
                EnvCmd::Set {
                    file,
                    key,
                    value,
                    create_file,
                    path,
                    json,
                } => (
                    path,
                    json,
                    EnvAction::Set {
                        file,
                        key,
                        value,
                        create_file,
                    },
                ),
                EnvCmd::Restore {
                    file,
                    snapshot,
                    path,
                    json,
                } => (path, json, EnvAction::Restore { file, snapshot }),
                EnvCmd::Snapshots { file, path, json } => {
                    (path, json, EnvAction::Snapshots { file })
                }
                EnvCmd::Check {
                    file,
                    require,
                    path,
                    json,
                } => (path, json, EnvAction::Check { file, require }),
            };
            let root = discover_root(&path)?;
            let data = pixel_ops::envfile::envfile(&root, &action)?;
            print_data(&data, json)
        }
        Command::Plan {
            prompt,
            path,
            query,
            tag,
            limit,
            format,
            no_verify,
            max_todos,
            status,
            done,
            undone,
            prune,
            json,
        } => {
            let format = if json { "json".to_string() } else { format };
            plan_cmd::run(plan_cmd::PlanOptions {
                prompt,
                path,
                query,
                tag,
                limit,
                format,
                no_verify,
                max_todos,
                status,
                done,
                undone,
                prune,
                json,
            })
        }
        Command::ReplayFlow { cmd } => {
            use pixel_flow::FlowAction;
            let json = match &cmd {
                FlowCmd::Save { json, .. }
                | FlowCmd::Get { json, .. }
                | FlowCmd::List { json, .. }
                | FlowCmd::Revise { json, .. }
                | FlowCmd::Replay { json, .. }
                | FlowCmd::Delete { json, .. }
                | FlowCmd::Show { json, .. } => *json,
            };
            let action = match cmd {
                FlowCmd::Save {
                    name,
                    title,
                    description,
                    tags,
                    url,
                    from_file,
                    json: _,
                } => FlowAction::Save {
                    name,
                    title,
                    description,
                    tags,
                    url,
                    from_file: Some(from_file),
                },
                FlowCmd::Get { name, json: _ } => FlowAction::Get { name },
                FlowCmd::List { tag, json: _ } => FlowAction::List { tag },
                FlowCmd::Revise {
                    name,
                    title,
                    description,
                    from_file,
                    json: _,
                } => FlowAction::Revise {
                    name,
                    title,
                    description,
                    from_file,
                },
                FlowCmd::Replay {
                    name,
                    vars,
                    account,
                    execute,
                    dry_run,
                    json: _,
                } => {
                    let mut var_map = std::collections::HashMap::new();
                    for v in &vars {
                        let (k, val) = v
                            .split_once('=')
                            .ok_or_else(|| format!("--var expects key=value, got '{v}'"))?;
                        var_map.insert(k.to_string(), val.to_string());
                    }
                    // --account shortcut: resolve alias to full email and
                    // inject into the flow's account var. Try openai_account
                    // first (Codex), then google_account (Claude/others).
                    if let Some(acct) = account {
                        let resolved = acct.clone();
                        // Check which var the flow expects by loading it.
                        let var_name = pixel_flow::load(&name)
                            .ok()
                            .and_then(|f| {
                                f.vars.iter().find_map(|v| {
                                    if v.name == "openai_account" {
                                        Some("openai_account")
                                    } else if v.name == "google_account" {
                                        Some("google_account")
                                    } else {
                                        None
                                    }
                                })
                            })
                            .unwrap_or("google_account");
                        var_map.insert(var_name.to_string(), resolved);
                    }
                    if execute {
                        FlowAction::Execute {
                            name,
                            vars: var_map,
                        }
                    } else {
                        FlowAction::Replay {
                            name,
                            vars: var_map,
                            dry_run,
                        }
                    }
                }
                FlowCmd::Delete { name, json: _ } => FlowAction::Delete { name },
                FlowCmd::Show { name, json: _ } => FlowAction::Show { name },
            };
            let data = pixel_flow::flow(&action)?;
            if matches!(action, FlowAction::Execute { .. })
                && data.get("success").and_then(serde_json::Value::as_bool) != Some(true)
            {
                return Err(format!(
                    "flow execution failed: {}",
                    data.get("error")
                        .and_then(|v| v.as_str())
                        .unwrap_or("flow did not complete successfully")
                ));
            }
            if json {
                return print_data(&data, true);
            }
            // For replay and show, the output field contains human-readable
            // text — print it directly to stdout. For everything else, use
            // the standard print_data path (JSON or pretty).
            match &action {
                FlowAction::Replay { .. } | FlowAction::Show { .. } => {
                    if let Some(output) = data.get("output").and_then(|v| v.as_str()) {
                        println!("{output}");
                        Ok(())
                    } else {
                        print_data(&data, true)
                    }
                }
                FlowAction::Execute { .. } => {
                    // Print the execution log to stderr, result summary to stdout.
                    if let Some(log) = data.get("log").and_then(|v| v.as_str()) {
                        eprintln!("{log}");
                    }
                    let success = data
                        .get("success")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false);
                    let steps = data
                        .get("steps_executed")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0);
                    let skipped = data
                        .get("steps_skipped")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0);
                    if success {
                        println!("✓ Flow executed: {} steps, {} skipped", steps, skipped);
                    } else if let Some(err) = data.get("error").and_then(|v| v.as_str()) {
                        println!("✗ Flow failed after {} steps: {}", steps, err);
                    } else {
                        println!(
                            "~ Flow completed with warnings: {} steps, {} skipped",
                            steps, skipped
                        );
                    }
                    Ok(())
                }
                _ => print_data(&data, true),
            }
        }
    }
}

/// The subcommands `pixel --help` lists (hidden aliases and commands
/// excluded), in declaration order: what the session-start block advertises.
fn session_commands() -> Vec<String> {
    Cli::command()
        .get_subcommands()
        .filter(|c| !c.is_hide_set())
        .map(|c| c.get_name().to_string())
        .collect()
}

fn run_query(
    intent: String,
    path: PathBuf,
    kind: &str,
    budget: usize,
    json_output: bool,
    no_daemon: bool,
    _logger: &pixel_actionlog::ActionLog,
) -> Result<(), String> {
    let kind = match kind {
        "auto" => QueryKind::Auto,
        "locate" => QueryKind::Locate,
        "scope" => QueryKind::Scope,
        "impact" => QueryKind::Impact,
        "history-recovery" => QueryKind::HistoryRecovery,
        "status" => QueryKind::Status,
        _ => return Err(format!("unsupported query kind '{kind}'")),
    };
    let mut result = compile_query(&intent, kind);
    if result.status == QueryStatus::Ranked {
        let output = serde_json::to_value(&result).map_err(|error| error.to_string())?;
        return print_data(&output, true);
    }
    let operation = result.plan[0].operations[0].as_str();
    let evidence = match operation {
        "resolve" => {
            let phrase = intent
                .trim()
                .trim_start_matches("where is `")
                .trim_end_matches('`');
            execute(
                &path,
                Request::Resolve {
                    phrase: phrase.into(),
                    limit: None,
                },
                no_daemon,
            )?
        }
        "targets" => execute(
            &path,
            Request::Targets {
                task: intent.clone(),
                limit: None,
                max_tier: None,
                precision: false,
            },
            no_daemon,
        )?,
        "impact" => {
            let target = intent.trim().trim_start_matches("show impact of ");
            execute(
                &path,
                Request::Impact {
                    uid_or_name: target.into(),
                    direction: "upstream".into(),
                    depth: Some(3),
                },
                no_daemon,
            )?
        }
        "excavate" => execute(
            &path,
            Request::Excavate {
                phrase: Some(intent.clone()),
                path: None,
                from: None,
                to: None,
                limit: None,
            },
            no_daemon,
        )?,
        "inspect" => execute(&path, Request::Inspect { files: None }, no_daemon)?,
        _ => return Err(format!("unsupported query operation '{operation}'")),
    };
    result.evidence.push(evidence);
    let output = json!({
        "op": "query",
        "result": result,
        "metrics": {"budget_tokens": budget, "operations": 1},
        "epistemics": {"closed_world": false, "lower_bound": true, "basis": "compiled bounded recipe"}
    });
    print_data(&output, json_output)
}

/// `pixel log` — the self-assessment surface over the async action log every
/// pixel invocation writes to `<root>/.pixel/actions.jsonl`.
fn run_log(
    path: &Path,
    limit: usize,
    errors_only: bool,
    json: bool,
    clear: bool,
) -> Result<(), String> {
    let root = discover_root(path)?;
    let log_path = pixel_actionlog::ActionLog::path_for_root(&root);
    if clear {
        return match std::fs::remove_file(&log_path) {
            Ok(()) => {
                println!("action log cleared: {}", log_path.display());
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                println!("no action log at {}", log_path.display());
                Ok(())
            }
            Err(e) => Err(format!("remove {}: {e}", log_path.display())),
        };
    }
    // Over-fetch when filtering to errors so `limit` still means "the last
    // N errors", not "the last N entries, some of which happen to be errors".
    let fetch = if errors_only {
        limit.max(1) * 20
    } else {
        limit.max(1)
    };
    let mut events = pixel_actionlog::tail(&log_path, fetch)
        .map_err(|e| format!("read {}: {e}", log_path.display()))?;
    if errors_only {
        events.retain(|e| e.outcome == pixel_actionlog::Outcome::Error);
    }
    if events.len() > limit {
        let start = events.len() - limit;
        events.drain(0..start);
    }
    if json {
        for e in &events {
            println!("{}", serde_json::to_string(e).map_err(|e| e.to_string())?);
        }
        return Ok(());
    }
    if events.is_empty() {
        println!("no recorded actions at {}", log_path.display());
        return Ok(());
    }
    let now = pixel_actionlog::now_ms();
    for e in &events {
        let when = relative_time(e.ts_ms, now);
        match e.outcome {
            pixel_actionlog::Outcome::Ok => {
                println!(
                    "{when:>8}  ok     {:<10} {} ({} ms)",
                    e.command, e.args, e.duration_ms
                );
            }
            pixel_actionlog::Outcome::Error => {
                println!(
                    "{when:>8}  ERROR  {:<10} {} ({} ms) — {}",
                    e.command,
                    e.args,
                    e.duration_ms,
                    e.error.as_deref().unwrap_or("?")
                );
            }
        }
        if let Some(line) = pixel_actionlog::format_metrics_line(e) {
            println!("  {line}");
        }
    }
    Ok(())
}

/// Preserve legacy snippet/pool reports, separately aggregate versioned
/// invocation metrics. Old measurements are never silently reclassified as v1.
fn run_savings(path: &Path, json: bool, since_hours: Option<u64>) -> Result<(), String> {
    use std::collections::BTreeMap;
    /// Per-command aggregate: invocations, pool chars, snippet chars.
    #[derive(Default)]
    struct Agg {
        count: u64,
        pool: u64,
        snippet: u64,
    }
    let root = discover_root(path)?;
    let log_path = pixel_actionlog::ActionLog::path_for_root(&root);
    // Over-fetch; savings is a lightweight aggregate read.
    let events = pixel_actionlog::tail(&log_path, 1_000_000)
        .map_err(|e| format!("read {}: {e}", log_path.display()))?;
    let cutoff_ms = since_hours.map(|h| {
        pixel_actionlog::now_ms().saturating_sub(
            i64::try_from(h)
                .unwrap_or(i64::MAX)
                .saturating_mul(3_600_000),
        )
    });
    let filtered: Vec<_> = events
        .iter()
        .filter(|e| cutoff_ms.is_none_or(|c| e.ts_ms >= c))
        .cloned()
        .collect();
    let workflow_metrics = pixel_actionlog::summarize_metrics(&filtered);
    // Aggregate per command: pool chars, snippet chars, count.
    let mut by_cmd: BTreeMap<String, Agg> = BTreeMap::new();
    for e in &events {
        if let Some(c) = cutoff_ms
            && e.ts_ms < c
        {
            continue;
        }
        let (Some(snippet), Some(pool)) = (e.snippet_cap_chars, e.pool_chars) else {
            continue; // not retrieval-shaped (or volumes not recorded)
        };
        let agg = by_cmd.entry(e.command.clone()).or_default();
        agg.count += 1;
        agg.pool = agg.pool.saturating_add(pool);
        agg.snippet = agg.snippet.saturating_add(snippet);
    }

    let tot_pool: u64 = by_cmd.values().map(|a| a.pool).sum();
    let tot_snippet: u64 = by_cmd.values().map(|a| a.snippet).sum();
    let overall = if tot_pool > 0 {
        1.0 - (tot_snippet as f64 / tot_pool as f64)
    } else {
        0.0
    };

    if json {
        let rows: Vec<serde_json::Value> = by_cmd
            .iter()
            .map(|(cmd, a)| {
                let ratio = if a.pool > 0 {
                    1.0 - (a.snippet as f64 / a.pool as f64)
                } else {
                    0.0
                };
                serde_json::json!({
                    "command": cmd,
                    "calls": a.count,
                    "pool_chars": a.pool,
                    "snippet_chars": a.snippet,
                    "savings": ratio,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::json!({
                "overall_savings": overall,
                "total_pool_chars": tot_pool,
                "total_snippet_chars": tot_snippet,
                "by_command": rows,
                "legacy_basis": "legacy snippet/pool byte comparison; not workflow-v1",
                "workflow_metrics": workflow_metrics,
            })
        );
        return Ok(());
    }

    println!("workflow metrics (measured bytes/duration; versioned byte-based token estimates)");
    println!(
        "{}",
        serde_json::to_string_pretty(&workflow_metrics).map_err(|e| e.to_string())?
    );
    println!("legacy savings (snippet vs candidate-pool chars; not workflow-v1)");
    println!(
        "{:<14} {:>5}  {:>12}  {:>14}  {:>7}",
        "command", "calls", "pool_chars", "snippet_chars", "savings"
    );
    for (cmd, a) in &by_cmd {
        let ratio = if a.pool > 0 {
            1.0 - (a.snippet as f64 / a.pool as f64)
        } else {
            0.0
        };
        println!(
            "{:<14} {:>5}  {:>12}  {:>14}  {:>6.1}%",
            cmd,
            a.count,
            a.pool,
            a.snippet,
            ratio * 100.0
        );
    }
    println!(
        "{:<14} {:>5}  {:>12}  {:>14}  {:>6.1}%",
        "TOTAL",
        by_cmd.values().map(|a| a.count).sum::<u64>(),
        tot_pool,
        tot_snippet,
        overall * 100.0
    );
    Ok(())
}

/// `pixel ask "<question>"` — semantic code search via static embeddings.
/// Ranked answer, not resolved certainty. On model-embedding failure, reports
/// the reason and defers rather than crashing.
fn run_ask(
    question: String,
    path: PathBuf,
    limit: usize,
    max_files: usize,
    json: bool,
) -> Result<(), String> {
    let root = discover_root(&path)?;
    let result = pixel_recall::code_search::ask_with_metadata(&root, &question, limit, max_files)
        .map_err(|e| format!("ask: {e}"))?;
    let hits = &result.hits;
    if json {
        let rows: Vec<serde_json::Value> = hits
            .iter()
            .map(|h| {
                serde_json::json!({
                    "path": h.path,
                    "score": h.score,
                    "semantic_score": h.semantic_score,
                    "ranking_score": h.ranking_score,
                    "lexical_matches": h.lexical_matches,
                    "query_terms": h.query_terms,
                    "snippet": h.snippet,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::json!({
                "question": question,
                "hits": rows,
                "coverage": result.coverage,
                "note": "hybrid semantic/lexical ranking — score is cosine; ranking_score determines order; verify before acting",
            })
        );
        return Ok(());
    }
    if hits.is_empty() {
        println!("no matches found for \"{question}\" in {}", root.display());
        return Ok(());
    }
    println!("hybrid matches for \"{question}\" (RRF ranking score; cosine semantic score):");
    for (i, h) in hits.iter().enumerate() {
        println!(
            "  {}. RRF {:.6}  cosine {:.3}  {} : \"{}\"",
            i + 1,
            h.ranking_score,
            h.semantic_score,
            h.path,
            h.snippet
        );
    }
    Ok(())
}

fn relative_time(ts_ms: i64, now_ms: i64) -> String {
    let delta_ms = (now_ms - ts_ms).max(0);
    let secs = delta_ms / 1000;
    if secs < 5 {
        "just now".to_string()
    } else if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}

/// Dry-run parse one `pixel …` argv (including the leading "pixel") against
/// this binary's real clap definition — nothing is executed. Used by
/// `pixel doctor`'s rule-vs-binary parity check so the installed rule text
/// can never document syntax the parser would reject.
/// The commit message of `publish`/`ship`: `-m <text>`, or the content of
/// `--message-file <path>` (`-` reads stdin). Clap guarantees exactly one
/// of the two is present.
fn commit_message(inline: Option<String>, file: Option<&Path>) -> Result<String, String> {
    let raw = match (inline, file) {
        (Some(text), _) => text,
        (None, Some(path)) if path == Path::new("-") => {
            let mut text = String::new();
            std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)
                .map_err(|e| format!("cannot read commit message from stdin: {e}"))?;
            text
        }
        (None, Some(path)) => std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read commit message file {}: {e}", path.display()))?,
        (None, None) => return Err("a commit message is required (-m or --message-file)".into()),
    };
    normalize_commit_message(&raw)
}

/// Trailing whitespace goes (an editor's final newline would otherwise
/// become a blank trailer line); a blank message is refused before git
/// sees it, so no journal entry is written for a commit git would reject.
fn normalize_commit_message(raw: &str) -> Result<String, String> {
    let text = raw.trim_end();
    if text.trim().is_empty() {
        return Err("commit message is empty".into());
    }
    Ok(text.to_string())
}

#[cfg(test)]
mod commit_message_tests {
    use super::{commit_message, normalize_commit_message};
    use std::path::Path;

    #[test]
    fn normalize_should_keep_paragraphs_and_drop_trailing_whitespace() {
        let raw = "subject\n\nbody line one\n\n- bullet\n\n";
        assert_eq!(
            normalize_commit_message(raw).unwrap(),
            "subject\n\nbody line one\n\n- bullet"
        );
    }

    #[test]
    fn normalize_should_refuse_a_blank_message() {
        assert_eq!(
            normalize_commit_message(" \n\t\n").unwrap_err(),
            "commit message is empty"
        );
    }

    #[test]
    fn commit_message_should_prefer_inline_text_and_name_a_missing_file() {
        assert_eq!(
            commit_message(Some("fix: x".into()), None).unwrap(),
            "fix: x"
        );
        let missing = Path::new("/nonexistent/pixel-msg.txt");
        let err = commit_message(None, Some(missing)).unwrap_err();
        assert!(
            err.starts_with("cannot read commit message file /nonexistent/pixel-msg.txt:"),
            "{err}"
        );
        assert_eq!(
            commit_message(None, None).unwrap_err(),
            "a commit message is required (-m or --message-file)"
        );
    }

    #[test]
    fn commit_message_should_read_the_file_verbatim() {
        let dir = std::env::temp_dir().join(format!("pixel-msg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("msg.txt");
        std::fs::write(&file, "feat: a\n\nSecond paragraph.\n").unwrap();
        assert_eq!(
            commit_message(None, Some(&file)).unwrap(),
            "feat: a\n\nSecond paragraph."
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}

fn validate_cli_syntax(args: &[String]) -> Result<(), String> {
    let args = args.to_vec();
    std::thread::Builder::new()
        .stack_size(4 * 1024 * 1024)
        .spawn(move || match Cli::try_parse_from(&args) {
            Ok(_) => Ok(()),
            Err(e) => Err(e
                .to_string()
                .lines()
                .next()
                .unwrap_or("parse error")
                .to_string()),
        })
        .map_err(|e| e.to_string())?
        .join()
        .map_err(|_| "parse thread panicked".to_string())?
}

/// `pixel excavate --show <oid> --file <path>`: full historical file content
/// in ONE call — the follow-up to an excavate candidate list that previously
/// forced agents into raw `git show`/`git log` rounds. Reads `<oid>:<path>`
/// through the safe `pixel_git::GitRunner` (ref-validated, output-capped);
/// when the file does not exist at `<oid>` (e.g. `<oid>` is the deletion
/// commit itself) it falls back to `<oid>^:<path>` — the pre-deletion
/// content — and says so. `--parent` skips straight to the parent read.
/// Implemented CLI-side (no daemon/proto round-trip): the content lives in
/// the object store, not the facts db, so a direct git read is exact.
fn excavate_show(
    path: &Path,
    oid: &str,
    file: Option<&str>,
    parent: bool,
    json: bool,
) -> Result<(), String> {
    let Some(file) = file else {
        return Err("dig-history --show requires --file <repo-relative path>".to_string());
    };
    let root = discover_root(path)?;
    let runner = pixel_git::GitRunner::new(&root);
    let (content, source, parent_fallback) = if parent {
        let c = runner
            .show_blob_string_at_parent(oid, file)
            .map_err(|e| format!("cannot read {oid}^:{file}: {e}"))?;
        (c, format!("{oid}^:{file}"), false)
    } else {
        match runner.show_blob_string(oid, file) {
            Ok(c) => (c, format!("{oid}:{file}"), false),
            Err(at_oid_err) => match runner.show_blob_string_at_parent(oid, file) {
                Ok(c) => (c, format!("{oid}^:{file}"), true),
                Err(_) => {
                    return Err(format!(
                        "{file} exists neither at {oid} nor at {oid}^: {at_oid_err}"
                    ));
                }
            },
        }
    };
    if json {
        let data = serde_json::json!({
            "oid": oid,
            "file": file,
            "source": source,
            "parent_fallback": parent_fallback,
            "content": content,
        });
        print_data(&data, true)
    } else {
        if parent_fallback {
            eprintln!(
                "pixel: {file} does not exist at {oid}; showing the parent's \
                 pre-deletion content ({source})"
            );
        } else {
            eprintln!("pixel: {source}");
        }
        write_stdout(&content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// The `--json` failure envelope is decided from the parsed flag, not
    /// from argv: `--json` counts only where the command declares it, and it
    /// counts through a nested subcommand (`list-errors last --json`), whose
    /// flag the top-level matches cannot see. Parsed on a thread with a 4 MiB
    /// stack — the derived command tree overflows the 2 MiB a test thread
    /// gets, the same reason `validate_cli_syntax` spawns one.
    #[test]
    fn json_requested_reads_the_deepest_subcommands_own_flag() {
        fn parsed(argv: &[&str]) -> bool {
            let argv: Vec<String> = argv.iter().map(ToString::to_string).collect();
            std::thread::Builder::new()
                .stack_size(4 * 1024 * 1024)
                .spawn(move || json_requested(&Cli::command().get_matches_from(&argv)))
                .unwrap()
                .join()
                .unwrap()
        }

        assert!(parsed(&["pixel", "status", ".", "--json"]));
        assert!(parsed(&["pixel", "list-errors", "last", "--json"]));
        assert!(!parsed(&["pixel", "status", "."]));
        // A nested command with no `json` argument of its own: false, not a
        // flag inherited from somewhere in argv.
        assert!(!parsed(&["pixel", "daemon", "status", "."]));
    }

    /// A `.pixel` holding only the global journal (no `base.shard`) — e.g.
    /// the `$HOME/.pixel` state dir — must NOT anchor root discovery, or
    /// every gitless invocation below it re-roots to that ancestor and
    /// plain-walk-indexes the whole home directory. Regression for the
    /// `pixel resolve`/`search` in `~/.zcode` hang (9+ min, 1.7GB RSS via a
    /// journal-only `~/.pixel`).
    #[test]
    fn discover_root_ignores_journal_only_pixel_dir() {
        let base = std::env::temp_dir().join(format!(
            "pixel-discover-journal-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::remove_dir_all(&base).ok();
        std::fs::create_dir_all(base.join("home/.pixel")).unwrap();
        std::fs::create_dir_all(base.join("home/work")).unwrap();
        let work = std::fs::canonicalize(base.join("home/work")).unwrap();

        // Journal-only `.pixel` (actions/history, no shard): no anchor.
        assert_eq!(discover_root(&work).unwrap(), work);

        // With a shard present, the `.pixel` ancestor anchors as before.
        std::fs::write(
            base.join("home/.pixel")
                .join(pixel_index::index::SHARD_FILE),
            b"shard",
        )
        .unwrap();
        assert_eq!(
            discover_root(&work).unwrap(),
            std::fs::canonicalize(base.join("home")).unwrap()
        );
        std::fs::remove_dir_all(&base).ok();
    }

    /// End-to-end rule-vs-binary parity of the doctor's normalizer against
    /// THIS binary's real clap definition: every canonical rule command
    /// shape must normalize and dry-run parse. This is the compile-time-side
    /// twin of the runtime `rule.parity` doctor check.
    #[test]
    fn canonical_rule_command_lines_parse_against_the_real_cli() {
        let canonical = [
            // NOTE: the historical rule text wrote `[--path <path>]` here —
            // the real flag is `--file`. That drift is exactly what the
            // runtime `rule.parity` doctor check flags.
            r#"pixel dig-history --phrase "<what you're looking for>" [--file <path>] [--json]"#,
            r#"pixel plan-rollback "<what broke, in the user's words>" /path/to/repo [--json]"#,
            r#"pixel plan-rollback --apply <oid> --file <path> /path/to/repo [--merge|--stash-first|--allow-dirty]"#,
            r#"pixel find-code "<phrase>" /path/to/repo [--json] [--limit N]"#,
            r#"pixel search-content "<pattern>" /path/to/repo --context 5 [--json] [--limit N]"#,
            r#"pixel sync-branch /path/to/repo [--strategy report|rebase-if-clean] [--push auto|never]"#,
            r#"pixel scope-task "<one-line task description>" /path/to/repo [--json] [--limit N]"#,
            r#"pixel scope-task --clear /path/to/repo"#,
            r#"pixel impact <symbol_name_or_uid> /path/to/repo [--direction upstream|downstream] [--depth N] [--json]"#,
            r#"pixel what-changed /path/to/repo [--base <ref>] [--json]"#,
            r#"pixel repo-state /path/to/repo [--json]"#,
            r#"pixel review-changes /path/to/repo [--json]"#,
            r#"pixel commit-history /path/to/repo [--ref <ref>] [--limit N] [--json]"#,
            r#"pixel diff <from> /path/to/repo [--paths <p>...] [--json]"#,
            r#"pixel commit --files <f>... --message "<msg>" --request-id <id> /path/to/repo"#,
            r#"pixel push <remote> <refspec> /path/to/repo --request-id <id>"#,
            r#"pixel commit-and-push --files <f>... --message "<msg>" <remote> <refspec> /path/to/repo --request-id <id>"#,
            r#"pixel new-branch <name> /path/to/repo --request-id <id>"#,
            r#"pixel fetch <remote> /path/to/repo [--json]"#,
            r#"pixel fast-forward /path/to/repo --expected-head <oid> --target-oid <oid> --request-id <id>"#,
            r#"pixel status /path/to/repo"#,
            r#"pixel build-index --history ."#,
            r#"pixel install"#,
            r#"pixel doctor"#,
        ];
        let mut failures = Vec::new();
        for line in canonical {
            match pixel_install::doctor::normalize_rule_command(line) {
                None => failures.push(format!("`{line}` did not normalize")),
                Some(argv) => {
                    if let Err(e) = validate_cli_syntax(&argv) {
                        failures.push(format!("`{line}` → argv {argv:?} → {e}"));
                    }
                }
            }
        }
        assert!(
            failures.is_empty(),
            "canonical rule command lines must parse against the real CLI:\n{}",
            failures.join("\n")
        );
    }

    /// A knowingly-wrong documented command must be REJECTED — this is what
    /// makes the parity check able to go red at all.
    #[test]
    fn known_bad_rule_command_lines_are_rejected() {
        for bad in [
            vec![
                "pixel".to_string(),
                "search-content".into(),
                "--no-such-flag".into(),
            ],
            vec!["pixel".to_string(), "frobnicate".into()],
            vec![
                "pixel".to_string(),
                "plan-rollback".into(),
                "--limit".into(),
                "3".into(),
            ],
        ] {
            assert!(
                validate_cli_syntax(&bad).is_err(),
                "argv {bad:?} should be rejected by the CLI parser"
            );
        }
    }

    #[test]
    fn claude_task_runtime_commands_parse() {
        for argv in [
            vec![
                "pixel".to_string(),
                "task-state".into(),
                "begin".into(),
                "add durable task state".into(),
                "--provider".into(),
                "claude".into(),
                "--session".into(),
                "session-123".into(),
                "/repo".into(),
                "--json".into(),
            ],
            vec![
                "pixel".to_string(),
                "task-state".into(),
                "accept".into(),
                "change greeting behavior".into(),
                "--provider".into(),
                "claude".into(),
                "/repo".into(),
            ],
            vec![
                "pixel".to_string(),
                "task-state".into(),
                "prepare".into(),
                "task-100-1".into(),
                "/repo".into(),
            ],
            vec![
                "pixel".to_string(),
                "task-state".into(),
                "status".into(),
                "task-100-1".into(),
                "/repo".into(),
                "--json".into(),
            ],
            vec![
                "pixel".to_string(),
                "task-state".into(),
                "events".into(),
                "task-100-1".into(),
                "/repo".into(),
            ],
            vec![
                "pixel".to_string(),
                "task-state".into(),
                "sandbox-create".into(),
                "task-100-1".into(),
                "candidate-1".into(),
                "--owned-path".into(),
                "src/a.rs".into(),
                "--owned-path".into(),
                "src/b.rs".into(),
                "/repo".into(),
            ],
            vec![
                "pixel".to_string(),
                "task-state".into(),
                "sandbox-inspect".into(),
                "task-100-1".into(),
                "candidate-1".into(),
                "/repo".into(),
            ],
            vec![
                "pixel".to_string(),
                "task-state".into(),
                "sandbox-promote".into(),
                "task-100-1".into(),
                "candidate-1".into(),
                "/repo".into(),
            ],
            vec![
                "pixel".to_string(),
                "task-state".into(),
                "sandbox-cancel".into(),
                "task-100-1".into(),
                "candidate-1".into(),
                "/repo".into(),
            ],
            vec![
                "pixel".to_string(),
                "task-state".into(),
                "worker-start".into(),
                "task-100-1".into(),
                "candidate-1".into(),
                "--model".into(),
                "sonnet".into(),
                "--max-turns".into(),
                "12".into(),
                "/repo".into(),
            ],
            vec![
                "pixel".to_string(),
                "task-state".into(),
                "worker-status".into(),
                "task-100-1".into(),
                "candidate-1".into(),
                "/repo".into(),
            ],
            vec![
                "pixel".to_string(),
                "task-state".into(),
                "worker-stop".into(),
                "task-100-1".into(),
                "candidate-1".into(),
                "/repo".into(),
            ],
            vec![
                "pixel".to_string(),
                "task-state".into(),
                "race-start".into(),
                "task-100-1".into(),
                "candidate-1".into(),
                "candidate-2".into(),
                "--path".into(),
                "/repo".into(),
            ],
            vec![
                "pixel".to_string(),
                "task-state".into(),
                "race-poll".into(),
                "task-100-1".into(),
                "candidate-1".into(),
                "candidate-2".into(),
                "--path".into(),
                "/repo".into(),
            ],
            vec![
                "pixel".to_string(),
                "task-state".into(),
                "plan-validate".into(),
                "task-100-1".into(),
                "--file".into(),
                "plan.json".into(),
                "/repo".into(),
                "--json".into(),
            ],
            vec![
                "pixel".to_string(),
                "task-state".into(),
                "show".into(),
                "--session".into(),
                "session-123".into(),
                "/repo".into(),
                "--json".into(),
            ],
            vec![
                "pixel".to_string(),
                "task-state".into(),
                "reset".into(),
                "--session".into(),
                "session-123".into(),
                "/repo".into(),
            ],
            vec![
                "pixel".to_string(),
                "run-hook".into(),
                "prompt-submit".into(),
                "--provider".into(),
                "claude".into(),
            ],
            vec![
                "pixel".to_string(),
                "run-hook".into(),
                "post-compaction".into(),
                "--provider".into(),
                "claude".into(),
            ],
        ] {
            assert!(validate_cli_syntax(&argv).is_ok(), "argv {argv:?}");
        }
    }

    #[test]
    fn enrich_with_context_returns_surrounding_lines() {
        // Create a temp file with known content
        let dir = std::env::temp_dir();
        let path = dir.join("pixel_ctx_test.rs");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "line 1").unwrap();
        writeln!(f, "line 2").unwrap();
        writeln!(f, "line 3").unwrap();
        writeln!(f, "pub const FOO: &str =").unwrap();
        writeln!(f, "    \"bar\";").unwrap();
        writeln!(f, "line 6").unwrap();
        writeln!(f, "line 7").unwrap();
        drop(f);

        let root = dir;
        let match_val = serde_json::json!({
            "path": "pixel_ctx_test.rs",
            "line": 4,
            "text": "pub const FOO: &str ="
        });

        let enriched = enrich_with_context(&match_val, &root, 2, false, &mut HashMap::new());
        let ctx = enriched
            .get("context")
            .and_then(Value::as_str)
            .unwrap_or("");

        // Should contain lines 2-6 (context=2 around line 4)
        assert!(
            ctx.contains(">>     4: pub const FOO"),
            "match line should be marked with >>"
        );
        assert!(
            ctx.contains("      2: line 2"),
            "should include 2 lines before"
        );
        assert!(
            ctx.contains("      6: line 6"),
            "should include 2 lines after"
        );
        assert!(
            !ctx.contains("line 1"),
            "should not include lines outside context window"
        );
        assert!(
            !ctx.contains("line 7"),
            "should not include lines outside context window"
        );

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn enrich_with_context_zero_context_returns_original() {
        let match_val = serde_json::json!({"path": "nonexistent.rs", "line": 1, "text": "foo"});
        let enriched =
            enrich_with_context(&match_val, Path::new("/tmp"), 0, false, &mut HashMap::new());
        // context=0 means no enrichment — original returned
        assert!(
            enriched.get("context").is_none(),
            "context=0 should not add context field"
        );
    }

    #[test]
    fn enrich_with_context_missing_file_returns_original() {
        let match_val =
            serde_json::json!({"path": "does_not_exist_xyz.rs", "line": 1, "text": "foo"});
        let enriched =
            enrich_with_context(&match_val, Path::new("/tmp"), 5, false, &mut HashMap::new());
        // File doesn't exist — should return original without context
        assert!(
            enriched.get("context").is_none(),
            "missing file should not add context"
        );
    }

    #[test]
    fn enrich_with_context_clamps_at_file_boundaries() {
        let dir = std::env::temp_dir();
        let path = dir.join("pixel_ctx_short.rs");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "only line").unwrap();
        drop(f);

        let root = dir;
        let match_val = serde_json::json!({
            "path": "pixel_ctx_short.rs",
            "line": 1,
            "text": "only line"
        });

        // Request 10 lines of context but file only has 1
        let enriched = enrich_with_context(&match_val, &root, 10, false, &mut HashMap::new());
        let ctx = enriched
            .get("context")
            .and_then(Value::as_str)
            .unwrap_or("");
        assert!(
            ctx.contains(">>     1: only line"),
            "should contain the match line"
        );
        assert!(!ctx.contains("line 0"), "should not go before line 1");

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn enrich_with_context_caches_file_content() {
        let dir = std::env::temp_dir();
        let path = dir.join("pixel_ctx_cache.rs");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "line 1").unwrap();
        writeln!(f, "line 2").unwrap();
        writeln!(f, "line 3").unwrap();
        drop(f);

        let root = dir;
        let mut cache: HashMap<PathBuf, Option<String>> = HashMap::new();
        let m1 = serde_json::json!({"path": "pixel_ctx_cache.rs", "line": 1, "text": "line 1"});
        let m2 = serde_json::json!({"path": "pixel_ctx_cache.rs", "line": 2, "text": "line 2"});
        let e1 = enrich_with_context(&m1, &root, 1, false, &mut cache);
        let e2 = enrich_with_context(&m2, &root, 1, false, &mut cache);
        assert!(e1.get("context").and_then(Value::as_str).is_some());
        assert!(e2.get("context").and_then(Value::as_str).is_some());
        // The cache holds the file content so the second call did not re-read.
        let key = root.join("pixel_ctx_cache.rs");
        assert!(cache.contains_key(&key));
        assert!(cache.get(&key).unwrap().is_some());

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn resolve_context_covers_full_multiline_span_not_just_start_line() {
        let dir = std::env::temp_dir();
        let path = dir.join("pixel_resolve_ctx_test.rs");
        let mut f = std::fs::File::create(&path).unwrap();
        for i in 1..=10 {
            writeln!(f, "line {i}").unwrap();
        }
        drop(f);

        let root = dir;
        let mut data = serde_json::json!({
            "matches": [
                {"path": "pixel_resolve_ctx_test.rs", "start_line": 4, "end_line": 7}
            ]
        });
        enrich_resolve_matches_with_context(&mut data, &root);
        let ctx = data["matches"][0]
            .get("context")
            .and_then(Value::as_str)
            .unwrap_or("");
        // Full span [4,7] must be marked, not just the start line.
        assert!(ctx.contains(">>     4: line 4"));
        assert!(ctx.contains(">>     5: line 5"));
        assert!(ctx.contains(">>     6: line 6"));
        assert!(ctx.contains(">>     7: line 7"));
        // Margin lines present but unmarked.
        assert!(ctx.contains("  2: line 2") || ctx.contains(" 2: line 2"));
        assert!(!ctx.contains(">>     2: line 2"));
        assert!(!ctx.contains(">>     9: line 9"));

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn resolve_context_skips_match_with_no_start_line() {
        let mut data = serde_json::json!({
            "matches": [{"path": "whatever.rs"}]
        });
        enrich_resolve_matches_with_context(&mut data, Path::new("/tmp"));
        assert!(data["matches"][0].get("context").is_none());
    }

    /// The `--json` page trailer: the page state a match row cannot carry,
    /// plus the envelope honesty fields, with every key always present so a
    /// strict NDJSON reader can tell a partial page from a complete one.
    #[test]
    fn search_meta_carries_the_page_state_and_the_envelope_honesty() {
        let data = serde_json::json!({
            "offset": 0,
            "epistemics": {"basis": "text index", "lower_bound": false},
            "warnings": [{"code": "cap", "message": "row cap"}],
        });
        let resumable = search_meta(&data, true, Some(3));
        assert_eq!(resumable["truncated"], true);
        assert_eq!(resumable["next_offset"], 3);
        assert_eq!(resumable["epistemics"]["basis"], "text index");
        assert_eq!(resumable["warnings"][0]["code"], "cap");
        let complete = search_meta(&data, false, None);
        assert_eq!(complete["truncated"], false);
        assert!(complete["next_offset"].is_null());
        let bare = search_meta(&serde_json::json!({}), true, None);
        assert!(bare["epistemics"].is_null());
        assert!(bare["warnings"].is_null());
    }

    /// The stdout cap counts the reserved metadata line, and a line that ends
    /// exactly on the cap still prints: the cut lands between lines, so a
    /// reader never gets a half-written JSON row.
    #[test]
    fn a_match_line_that_ends_exactly_on_the_cap_still_fits() {
        assert!(fits_before_cap(0, 10, 0, 10));
        assert!(!fits_before_cap(0, 11, 0, 10));
        assert!(fits_before_cap(4, 4, 2, 10));
        assert!(!fits_before_cap(4, 5, 2, 10), "the reserved trailer counts");
    }

    /// Both renderings of one match: the JSON object agents parse, and the
    /// human row or `--- path:line ---` context block.
    #[test]
    fn search_match_lines_render_the_json_and_human_forms() {
        let plain = serde_json::json!({"path": "a.rs", "line": 3, "text": "x"});
        assert_eq!(
            search_match_line(&plain, true),
            "{\"line\":3,\"path\":\"a.rs\",\"text\":\"x\"}\n"
        );
        assert_eq!(search_match_line(&plain, false), "a.rs:3:x\n");
        let enriched =
            serde_json::json!({"path": "a.rs", "line": 3, "text": "x", "context": ">> 3: x"});
        assert_eq!(
            search_match_line(&enriched, true),
            "{\"context\":\">> 3: x\",\"line\":3,\"path\":\"a.rs\",\"text\":\"x\"}\n"
        );
        assert_eq!(
            search_match_line(&enriched, false),
            "--- a.rs:3 ---\n>> 3: x\n"
        );
    }
}

/// Every `pixel …` line in the fenced blocks of the two bundled prompt
/// assets must parse against this binary's clap definition. `pixel doctor`'s
/// `rule.parity` only covers the rule text installed on a machine; the
/// assets themselves are what every wrapped `claude`, every Codex session and every
/// print-mode sub-agent reads, so their drift has to fail the build.
#[cfg(test)]
mod prompt_asset_parity {
    use pixel_install::doctor::{extract_rule_commands, normalize_rule_command};

    const ASSETS: [(&str, &str); 2] = [
        (
            "pixel-agent-prompt.md",
            include_str!("../../pixel-install/assets/pixel-agent-prompt.md"),
        ),
        (
            "pixel-subagent-prompt.md",
            include_str!("../../pixel-install/assets/pixel-subagent-prompt.md"),
        ),
    ];

    #[test]
    fn every_documented_command_line_parses_against_the_cli() {
        let mut checked = 0;
        let mut failures = Vec::new();
        for (name, text) in ASSETS {
            let commands = extract_rule_commands(text);
            assert!(
                !commands.is_empty(),
                "{name}: no fenced `pixel …` line found — the extractor or the asset changed shape"
            );
            for line in commands {
                let Some(argv) = normalize_rule_command(&line) else {
                    continue;
                };
                checked += 1;
                if let Err(error) = super::validate_cli_syntax(&argv) {
                    failures.push(format!("{name}: `{line}` → {error}"));
                }
            }
        }
        assert!(checked > 0, "nothing was checked");
        assert!(
            failures.is_empty(),
            "documented command lines the CLI rejects:\n{}",
            failures.join("\n")
        );
    }

    /// The kill switches (`PIXEL_DAEMON_AUTO_START=0`, `PIXEL_GUARD_*=off`)
    /// fire only on an explicit off value: unset and any other value keep
    /// the feature on.
    #[test]
    fn env_flag_off_fires_only_on_an_explicit_off_value() {
        let name = format!("PIXEL_TEST_FLAG_{}_{}", std::process::id(), line!());
        assert!(!crate::env_flag_off(&name), "unset");
        for (value, expected) in [
            ("0", true),
            ("false", true),
            ("off", true),
            ("1", false),
            ("", false),
            ("no", false),
        ] {
            // SAFETY: the variable name is unique to this test (pid + line),
            // so no other thread in the process reads or writes it.
            unsafe {
                std::env::set_var(&name, value);
            }
            assert_eq!(crate::env_flag_off(&name), expected, "{value:?}");
        }
        // SAFETY: as above.
        unsafe {
            std::env::remove_var(&name);
        }
    }
}

#[cfg(test)]
mod renamed_command_tests {
    use super::{Cli, rename_note, renamed_invocation};
    use clap::CommandFactory;
    use std::collections::BTreeSet;

    /// The full `Cli` definition overflows a 2 MiB test thread in debug
    /// builds (the reason `validate_cli_syntax` runs on 4 MiB); build and
    /// parse it on a thread sized the same way.
    fn on_big_stack<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(f)
            .unwrap()
            .join()
            .unwrap()
    }

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn clap_aliases_are_exactly_the_rename_table() {
        // Every hidden alias the parser accepts must be a documented rename,
        // and every documented rename must parse: a variant renamed again
        // without updating the table fails here, not in a user's script.
        let registered = on_big_stack(|| {
            let cli = Cli::command();
            let mut registered = BTreeSet::new();
            for sub in cli.get_subcommands() {
                for alias in sub.get_all_aliases() {
                    registered.insert((alias.to_string(), sub.get_name().to_string()));
                }
            }
            registered
        });
        let table: BTreeSet<(String, String)> = pixel_proto::commands::RENAMED_COMMANDS
            .iter()
            .map(|(old, new)| ((*old).to_string(), (*new).to_string()))
            .collect();
        assert_eq!(registered, table);
    }

    #[test]
    fn an_old_name_parses_to_the_current_subcommand() {
        let name = |words: &'static [&'static str]| {
            on_big_stack(move || {
                Cli::command()
                    .try_get_matches_from(words)
                    .unwrap()
                    .subcommand_name()
                    .map(str::to_string)
            })
        };
        // The action log and the metrics evidence key on this name, so an
        // alias invocation is recorded exactly like the current spelling.
        assert_eq!(
            name(&["pixel", "ready", "--no-daemon", "--json"]).as_deref(),
            Some("prepare-repo")
        );
        assert_eq!(
            name(&["pixel", "hook", "session-start"]).as_deref(),
            Some("run-hook")
        );
    }

    #[test]
    fn renamed_invocation_reads_the_command_word_only() {
        assert_eq!(
            renamed_invocation(&argv(&["pixel", "ready"])),
            Some(("ready", "prepare-repo"))
        );
        assert_eq!(
            renamed_invocation(&argv(&["pixel", "--metrics", "off", "changes", "."])),
            Some(("changes", "what-changed")),
            "the value of --metrics is not the command"
        );
        assert_eq!(
            renamed_invocation(&argv(&["pixel", "--metrics=off", "symbol", "x"])),
            Some(("symbol", "find-symbol"))
        );
        assert_eq!(
            renamed_invocation(&argv(&["pixel", "prepare-repo", "ready"])),
            None,
            "a path named like an old command is not a renamed invocation"
        );
        assert_eq!(renamed_invocation(&argv(&["pixel", "impact", "x"])), None);
        assert_eq!(renamed_invocation(&argv(&["pixel", "--help"])), None);
        assert_eq!(renamed_invocation(&argv(&["pixel"])), None);
    }

    #[test]
    fn rename_note_is_one_line_and_follows_the_metrics_gate() {
        assert_eq!(
            rename_note(&argv(&["pixel", "ready", "--json"]), true).as_deref(),
            Some("note: 'ready' is now 'prepare-repo'; the old name stays accepted until 1.0\n")
        );
        assert_eq!(rename_note(&argv(&["pixel", "ready"]), false), None);
        assert_eq!(rename_note(&argv(&["pixel", "prepare-repo"]), true), None);
    }
}

//! Transport-agnostic service: one `Request` in, one `Response` out.
//!
//! All cross-crate contract calls (graph analyses, context rendering) are
//! centralized in the `bridge` module at the bottom so integration drift is
//! a one-line fix per call site.

use std::collections::BTreeMap;
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Instant;

use serde::Serialize;
use serde_json::{Value, json};

use pixel_context::estimate_tokens;
use pixel_facts::FactsStore;
use pixel_graph::{EdgeKind, EdgeRow, FileRow, GraphStore, SymbolKind, SymbolRow};
use pixel_index::TrigramExtractor;
use pixel_index::index::{MAX_FILE_BYTES, open_regular_bounded};
use pixel_index::indexset::{IndexSet, IndexSetError, RefreshOutcome};
use pixel_proto::{Envelope, Epistemics, ErrorCode, PixelError, SnapshotInfo, Warning};
use pixel_recall::embed::{EmbedKind, Embedder, open_default_embedder};

pub const GRAPH_DB_FILE: &str = "graph.v2.db";
/// Increment whenever the daemon request/response contract changes in a way
/// that an older process cannot safely serve to a newer CLI. Bumped from 6
/// to 7 with the Envelope v2 migration: the wire shape changed from
/// `{ok, error, data}` to the full `Envelope` (`ok, op, protocol, requestId,
/// snapshot, epistemics, budget, result, error, warnings`), gated by
/// `pixel_proto::ENVELOPE_PROTOCOL_VERSION`. 10: the `plan` op, which a
/// daemon of an older build rejects as an unknown variant.
pub const PROTOCOL_VERSION: u64 = 11;

/// The potion model the daemon warms; the `potion.ok` marker carries the
/// repo name so a stale v1 marker cannot pass for v2.
const POTION_V2_REPO: &str = "minishlab/potion-code-64M-v2";

// `targets` (S3 probes, graph expansion and evidence): the caps keep the op
// ms-scale; every cap that fires is named in the epistemics envelope.
const CONTENT_PROBE_LIMIT: usize = 1000;
const MAX_SEED_FILES: usize = 8;
const MAX_SEED_SYMBOLS: usize = 24;
/// P0 is the only tier the doctrine mandates checking before the first
/// edit, so it is the only tier worth spending tokens to pre-justify.
const EVIDENCE_MAX_LINES_PER_TARGET: usize = 2;

/// `context` and `uses`: edges returned per direction before elision.
const EDGE_LIMIT: usize = 20;
// `context` budget shape: item count and source bytes, per target and per
// neighbour snippet.
const MAX_CONTEXT_ITEMS: usize = 41;
const MAX_CONTEXT_SOURCE_BYTES: usize = 262_144; // 256 KiB
const MAX_TARGET_SNIPPET_BYTES: usize = 32_768; // 32 KiB
const MAX_NEIGHBOR_SNIPPET_BYTES: usize = 4_096; // 4 KiB

/// Hard cap so `map` on a pathological repo stays bounded; the flag
/// surfaces in the output so a truncated map is never passed off as
/// complete.
const MAP_FILE_CAP: usize = 2000;

// ---------------------------------------------------------------------------
// errors
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum ServeError {
    Index(IndexSetError),
    Io(std::io::Error),
    Msg(String),
}

impl std::fmt::Display for ServeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ServeError::Index(e) => write!(f, "{e}"),
            ServeError::Io(e) => write!(f, "io error: {e}"),
            ServeError::Msg(m) => write!(f, "{m}"),
        }
    }
}
impl std::error::Error for ServeError {}

impl From<IndexSetError> for ServeError {
    fn from(e: IndexSetError) -> Self {
        ServeError::Index(e)
    }
}
impl From<std::io::Error> for ServeError {
    fn from(e: std::io::Error) -> Self {
        ServeError::Io(e)
    }
}

// ---------------------------------------------------------------------------
// wire types — now derived from pixel-proto (the single contract crate)
// ---------------------------------------------------------------------------

/// The daemon request type. Re-exported from `pixel_proto::Op` so the daemon,
/// CLI, and MCP surfaces all share one enum — per PLAN.md A1, this kills the
/// N-touchpoint op-registration problem (adding an op is one variant here,
/// not edits across 4+ crates).
pub use pixel_proto::Op as Request;

use crate::evaluate;
use pixel_proto::evaluate as wire;

#[cfg(test)]
#[path = "evaluate_tests.rs"]
mod evaluate_tests;

/// The daemon response type: a `pixel_proto::Envelope<serde_json::Value>`.
/// Success → `Envelope::success(op_name, result)`; failure →
/// `Envelope::failure(op_name, error)`. The old ad-hoc `{ok, error, data}`
/// struct is gone; `resp.data()` reads the envelope's `result` field.
pub type Response = Envelope<Value>;

/// Classify an operation error string into the best-fit `ErrorCode`.
///
/// The message is always preserved verbatim in the envelope's `error.message`;
/// the code is for programmatic handling, so it is read from what the message
/// already carries instead of being guessed from prose:
///
/// - `pixel-ops` prefixes the messages it types itself with the code it means
///   (`NON_FAST_FORWARD: merge-base … is not an ancestor of …`,
///   `STALE_STATE: expected head …`, `REF_EXISTS: branch … already exists`,
///   `GIT_FAILED: …`, `NETWORK_AMBIGUITY: …`, `UNSUPPORTED_STATE: …`), so a
///   leading token that names an [`ErrorCode`] IS that code. The markers it
///   writes that name no code (`REFUSED:`, `STALE_REMOTE:`,
///   `FILE_NOT_TRACKED:`, `PROVENANCE_BAD_ARGS:`, …) stay unclassified.
/// - the repository lock reports `repository is busy…`, and the lookups in
///   this file report `no symbol named …` / `no symbol with uid …`.
///
/// Everything else — a malformed regex, a bad parameter, an opaque message
/// forwarded from another crate — stays `InvalidInput`, the correct default
/// for "the request itself was not satisfiable". The codes with no producer
/// anywhere in the tree (listed on [`ErrorCode`]) are not pretended to be
/// reachable: `IndexBuilding`/`NotIndexed` never surface because
/// `ensure_graph` and `IndexSet::open_or_build` build lazily instead of
/// failing, and the one ambiguous case (`resolve_symbol`'s multi-candidate
/// result) is returned as `Ok(candidates_value(...))`, never an error.
fn classify_error(msg: &str) -> ErrorCode {
    if let Some(code) = code_named_in_prefix(msg) {
        return code;
    }
    let lower = msg.to_lowercase();
    if lower.starts_with("no symbol named") || lower.starts_with("no symbol with uid") {
        ErrorCode::NotFound
    } else if lower.starts_with("repository is busy") {
        ErrorCode::BusyRepository
    } else {
        ErrorCode::InvalidInput
    }
}

/// The code a message names in its leading `"<CODE>: …"` token, when that
/// token is one of [`ErrorCode`]'s wire names.
fn code_named_in_prefix(msg: &str) -> Option<ErrorCode> {
    let (head, _) = msg.split_once(':')?;
    serde_json::from_str(&format!("\"{}\"", head.trim())).ok()
}

/// Build a failure envelope from a plain error string (the shape `dispatch`
/// returns). Used by `handle` and the daemon transport for pre-parse errors.
pub fn failure_response(op: &str, msg: impl Into<String>) -> Response {
    let msg = msg.into();
    let code = classify_error(&msg);
    Envelope::failure(op, PixelError::new(code, msg))
}

// ---------------------------------------------------------------------------
// service
// ---------------------------------------------------------------------------

pub struct Service {
    root: PathBuf,
    index: Arc<RwLock<IndexSet>>,
    pub(crate) publication: Arc<RwLock<Publication>>,
    read_only: bool,
    reader_generation: u64,
    graph: Option<GraphStore>,
    /// Watcher-driven graph updates that failed (a locked db, an unreadable
    /// row). Counted so `status` shows a daemon that is serving an index it
    /// could not keep in sync.
    graph_failures: FailureLog,
    /// `notify` backend errors reported by the transport loop: a watcher
    /// that stopped seeing changes leaves the same stale answers.
    watcher_failures: FailureLog,
    /// Lazily opened on first `scope: "hybrid"` search; kept warm for the
    /// daemon's lifetime. `None` = not yet loaded (not tried, download in
    /// progress, or load failed transiently).
    embedder: Option<Box<dyn Embedder>>,
    /// Permanent: the model IS cached but the load itself errors (corrupt
    /// download, incompatible build). Never set for "not cached yet" —
    /// that's transient and retried on every call until the background
    /// download writes the marker.
    embedder_unavailable: bool,
    /// A background download has been spawned — prevents duplicate threads.
    embedder_download_started: bool,
    /// The facts/history warm loop is demand-driven: spawned on the first
    /// facts-consuming request, not at daemon start — users who never touch
    /// history commands never pay for the index.
    facts_warmer_started: AtomicBool,
}

/// Version of a coherently published index/graph pair, not a filesystem snapshot.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Publication {
    pub generation: u64,
    pub healthy: bool,
}

/// Counts watcher-side failures and decides which ones are logged: the
/// first, then every doubling (1, 2, 4, 8 …). A permanently broken graph.db
/// or watch stays visible in `status` without one stderr line per filesystem
/// event.
#[derive(Debug, Default)]
struct FailureLog {
    failures: u64,
}

impl FailureLog {
    /// Count one failure; `true` when this one is due a log line.
    fn record(&mut self) -> bool {
        self.failures += 1;
        self.failures.is_power_of_two()
    }

    fn count(&self) -> u64 {
        self.failures
    }
}

/// Log one failure line. stderr diagnostics only, so it is skipped by the
/// mutation gate: `FailureLog::record` holds the rate-limit decision and its
/// unit test pins it.
#[cfg_attr(test, mutants::skip)]
fn note_failure(log: &mut FailureLog, what: &str) {
    if log.record() {
        eprintln!("pixel daemon: {what} ({} failure(s) this run)", log.count());
    }
}

impl Service {
    /// Open (building layers if needed) the text index; graph db is lazy.
    pub fn open(root: &Path) -> Result<Self, ServeError> {
        let root = root
            .canonicalize()
            .map_err(|e| ServeError::Msg(format!("bad root {}: {e}", root.display())))?;
        let index = IndexSet::open_or_build(&root, Box::new(TrigramExtractor))?;
        Ok(Service {
            root,
            index: Arc::new(RwLock::new(index)),
            publication: Arc::new(RwLock::new(Publication {
                generation: 1,
                healthy: true,
            })),
            read_only: false,
            reader_generation: 0,
            graph: None,
            graph_failures: FailureLog::default(),
            watcher_failures: FailureLog::default(),
            embedder: None,
            embedder_unavailable: false,
            embedder_download_started: false,
            facts_warmer_started: AtomicBool::new(false),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// A persistent reader shares the warm text index, but never a SQLite connection.
    pub(crate) fn read_replica(&self) -> Self {
        Self {
            root: self.root.clone(),
            index: Arc::clone(&self.index),
            publication: Arc::clone(&self.publication),
            read_only: true,
            reader_generation: 0,
            graph: None,
            graph_failures: FailureLog::default(),
            watcher_failures: FailureLog::default(),
            embedder: None,
            embedder_unavailable: true,
            embedder_download_started: false,
            facts_warmer_started: AtomicBool::new(true),
        }
    }

    /// Read-plane allowlist: no writes, model downloads, history ingest, or repo snapshot subprocesses.
    pub(crate) fn read_evidence(
        &mut self,
        kind: &str,
        query: &str,
        limit: usize,
    ) -> (Publication, Result<Value, String>) {
        let publication = Arc::clone(&self.publication);
        let state = publication.read().expect("publication lock poisoned");
        if !state.healthy {
            return (
                *state,
                Err("index/graph publication unhealthy; retry after refresh".into()),
            );
        }
        if self.reader_generation != state.generation {
            self.graph = None;
            self.reader_generation = state.generation;
        }
        if self.graph.is_none() {
            self.graph = GraphStore::open_read_only(&self.graph_db_path()).ok();
        }
        if let Some(graph) = &self.graph
            && let Err(error) = graph.conn().execute_batch("BEGIN")
        {
            return (*state, Err(error.to_string()));
        }
        let result = match kind {
            "execution_brief" => self.op_targets(query, Some(limit), Some("P1"), false),
            "search" => self.op_search(query, Some(limit), None, None, None),
            "resolve" => self.op_resolve(query, Some(limit)),
            "impact" => self.op_impact(query, "upstream", Some(2)),
            _ => Err(format!("unsupported evidence query kind: {kind}")),
        };
        if let Some(graph) = &self.graph {
            let _ = graph.conn().execute_batch("ROLLBACK");
        }
        (*state, result)
    }

    pub(crate) fn admitted_paths(&self) -> Vec<String> {
        self.index.read().expect("index lock poisoned").paths()
    }

    pub fn graph_db_path(&self) -> PathBuf {
        self.root
            .join(pixel_index::index::SHARD_DIR)
            .join(GRAPH_DB_FILE)
    }

    /// Watcher hook: refresh one file in index + graph.
    pub fn refresh_file(&mut self, rel: &str) {
        self.refresh_files(&[(rel, false)]);
    }

    /// Watcher hook: file deleted.
    pub fn remove_file(&mut self, rel: &str) {
        self.refresh_files(&[(rel, true)]);
    }

    /// Watcher hook: refresh a batch of files in index + graph.
    pub fn refresh_files(&mut self, files: &[(&str, bool)]) {
        if files.is_empty() {
            return;
        }
        let publication = Arc::clone(&self.publication);
        let mut state = publication.write().expect("publication lock poisoned");
        state.healthy = false;
        let graph_changes = self
            .index
            .write()
            .expect("index lock poisoned")
            .refresh_files(files);
        let graph_files: Vec<(&str, bool)> = graph_changes
            .iter()
            .map(|(path, outcome)| (path.as_str(), *outcome == RefreshOutcome::Excluded))
            .collect();
        let db = self.graph_db_path();
        if db.exists() {
            if let Err(error) = bridge::update_files(&self.root, &db, &graph_files) {
                self.note_graph_update_failure(
                    &format!("batch of {} file(s) from {}", files.len(), files[0].0),
                    &error,
                );
                self.graph = None;
                return;
            }
            self.graph = None;
        }
        state.generation += 1;
        state.healthy = true;
    }

    /// Count a watcher-driven graph update that failed. The cached handle is
    /// dropped either way, so the next graph op walks the tree and repairs
    /// the drift — but the failure itself must not be invisible.
    fn note_graph_update_failure(&mut self, rel: &str, error: &str) {
        note_failure(
            &mut self.graph_failures,
            &format!("graph update failed for {rel}: {error}"),
        );
    }

    /// Count a `notify` backend error reported by the transport loop.
    pub(crate) fn note_watcher_error(&mut self, error: &str) {
        note_failure(
            &mut self.watcher_failures,
            &format!("watcher error: {error}"),
        );
    }

    /// Make sure `self.graph` is populated. Builds graph.db on first use;
    /// when the working tree has drifted from the built state (detected via
    /// the build-time freshness signature) it re-extracts only the files
    /// whose content hash changed and drops the removed ones, falling back
    /// to a full rebuild when the db carries no signature or the drift
    /// exceeds `PIXEL_GRAPH_INCREMENTAL_MAX_PCT` (default 20 %) of the
    /// indexed files. Returns build info (stats, timing, `incremental`)
    /// when a build/update happened.
    ///
    /// Without a git anchor (no `.git`) the same path applies: the walk
    /// (`policy_walk`, which respects .gitignore in gitless trees) is capped
    /// by `PIXEL_GRAPH_MAX_FILES` (default 50000) and the signature is
    /// file-hash-based, so drift detection works without git.
    fn ensure_graph(&mut self) -> Result<Option<Value>, String> {
        if self.graph.is_some() {
            return Ok(None);
        }
        if self.read_only {
            return Err(
                "graph unavailable in read plane; build it through the maintenance lane".into(),
            );
        }
        let publication = Arc::clone(&self.publication);
        let mut state = publication.write().expect("publication lock poisoned");
        state.healthy = false;
        let result = self.ensure_graph_inner();
        if result.is_ok() {
            state.generation += 1;
            state.healthy = true;
        }
        result
    }

    fn ensure_graph_inner(&mut self) -> Result<Option<Value>, String> {
        let db = self.graph_db_path();
        let gitless = pixel_index::gitsync::rev_parse_head(&self.root).is_none();
        let mut built = None;
        if !db.exists() {
            built = Some(self.full_rebuild_info("missing")?);
        } else {
            // One walk answers both "fresh?" and "which files drifted?".
            match bridge::tree_delta(&self.root, &db) {
                Ok(Some(delta)) if delta.fresh => {}
                Ok(Some(delta)) => {
                    let pct = incremental_max_pct();
                    if incremental_allowed(delta.changed_count(), delta.indexed_files, pct) {
                        built = Some(self.incremental_update_info(&delta)?);
                    } else {
                        built = Some(self.full_rebuild_info("threshold")?);
                    }
                }
                Ok(None) => built = Some(self.full_rebuild_info("no_signature")?),
                Err(_) => built = Some(self.full_rebuild_info("unreadable")?),
            }
        }
        if self.graph.is_none() {
            self.graph = Some(GraphStore::open(&db).map_err(|e| e.to_string())?);
        }
        if gitless && let Some(obj) = built.as_mut().and_then(Value::as_object_mut) {
            obj.insert("gitless".into(), json!(true));
        }
        Ok(built)
    }

    /// Full rebuild plus its `graph_build` record. If the build fails (e.g.
    /// file-count cap hit on a huge gitless directory), the error is
    /// returned so callers can degrade from it — same pattern as
    /// `op_targets`.
    fn full_rebuild_info(&mut self, reason: &str) -> Result<Value, String> {
        let (stats, build_ms) = self.rebuild_graph()?;
        Ok(json!({
            "graph_built": true,
            "incremental": false,
            "reason": reason,
            "build_ms": build_ms,
            "stats": stats,
        }))
    }

    /// Apply a tree delta in place. An incremental update that fails
    /// (concurrent edit, unreadable row, sqlite busy) degrades to a full
    /// rebuild rather than surfacing an error: the full path is always
    /// available and the caller only asked for a fresh graph.
    fn incremental_update_info(
        &mut self,
        delta: &pixel_graph::build::TreeDelta,
    ) -> Result<Value, String> {
        let db = self.graph_db_path();
        self.graph.take();
        let started = Instant::now();
        if let Err(error) = bridge::apply_tree_delta(&self.root, &db, delta) {
            let mut info = self.full_rebuild_info("incremental_failed")?;
            info["incremental_error"] = json!(error);
            return Ok(info);
        }
        let build_ms = started.elapsed().as_millis() as u64;
        let store = GraphStore::open(&db).map_err(|e| e.to_string())?;
        let (files, symbols, edges, unresolved) = store.counts().map_err(|e| e.to_string())?;
        self.graph = Some(store);
        Ok(json!({
            "graph_built": true,
            "incremental": true,
            "changed_files": delta.changed.len(),
            "removed_files": delta.removed.len(),
            "build_ms": build_ms,
            "stats": {
                "files": files,
                "symbols": symbols,
                "edges": edges,
                "unresolved": unresolved,
                "elapsed_ms": build_ms,
            },
        }))
    }

    fn rebuild_graph(&mut self) -> Result<(Value, u64), String> {
        let db = self.graph_db_path();
        let tmp = db.with_file_name(format!(".graph-rebuild-{}.db", std::process::id()));
        self.graph.take();
        remove_sqlite_files(&tmp)?;
        let started = Instant::now();
        let stats = bridge::build_graph(&self.root, &tmp)?;
        {
            let checkpoint = GraphStore::open(&tmp).map_err(|error| error.to_string())?;
            checkpoint
                .conn()
                .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
                .map_err(|error| error.to_string())?;
        }
        remove_sqlite_sidecars(&db)?;
        std::fs::rename(&tmp, &db).map_err(|error| {
            format!(
                "publish graph {} -> {}: {error}",
                tmp.display(),
                db.display()
            )
        })?;
        remove_sqlite_sidecars(&tmp)?;
        self.graph = Some(GraphStore::open(&db).map_err(|error| error.to_string())?);
        Ok((stats, started.elapsed().as_millis() as u64))
    }

    /// Open a fresh, on-disk `GraphStore` handle purely for the search
    /// ranking symbol signal (Bug 2 fix). Deliberately independent of
    /// `self.graph`: that field is populated only as a side effect of some
    /// OTHER op (`targets`, `symbol`, `impact`, ...) having called
    /// `ensure_graph()` earlier in this same daemon process, so reading it
    /// here would make ranking depend on which unrelated ops happened to
    /// run first — the exact non-determinism this fixes (and it is always
    /// `None` for `--no-daemon`/in-process CLI runs, since a fresh
    /// `Service::open` never populates it). Basing the decision solely on
    /// "does graph.db exist on disk" makes the same search call produce the
    /// same ranking regardless of prior daemon activity or transport.
    ///
    /// This never builds or rebuilds the graph — mirrors `op_status`'s
    /// existing pattern of opening the db directly without going through
    /// `ensure_graph()`. A search must stay within its latency budget and
    /// can never pay graph-build cost (which can take minutes on a large or
    /// dirty repo); when no graph.db exists yet, ranking simply proceeds
    /// without the symbol signal, same as today.
    fn open_graph_for_ranking(&self) -> Option<GraphStore> {
        let db = self.graph_db_path();
        if db.exists() {
            GraphStore::open_read_only(&db).ok()
        } else {
            None
        }
    }

    /// Lazy-load the code embedding model. Three-tier graceful degradation:
    ///
    /// 1. **Cached** (marker file exists for v2): load now with
    ///    `download=false` — fast (~100ms for a static model). On load
    ///    failure (corrupt download, incompatible build), set
    ///    `embedder_unavailable` permanently — re-downloading won't fix a
    ///    corrupt file; the user must clear the model cache.
    ///
    /// 2. **Not cached, no download started**: spawn a detached background
    ///    thread that downloads + caches the model (writes the marker on
    ///    success). Degrade THIS call to `code` ranking (semantic channel
    ///    returns `None`). The next call finds the marker and loads from
    ///    cache. Never blocks the search latency budget on a network
    ///    download.
    ///
    /// 3. **Not cached, download already in progress**: degrade silently.
    ///    The background thread will write the marker when done; the next
    ///    call picks it up.
    ///
    /// `embedder_unavailable` is NEVER set for "not cached" — only for a
    /// confirmed load failure on a cached model. This means `pixel search-meaning`
    /// (which uses `download=true`) can cache the model at any time, and
    /// the next `--scope hybrid` search will pick it up without a restart.
    // Loads or downloads the potion model: unit tests never have it, so no
    // test can observe the difference (`pixel search-meaning` covers it end to end).
    #[cfg_attr(test, mutants::skip)]
    fn ensure_embedder(&mut self) {
        if self.embedder.is_some() || self.embedder_unavailable {
            return;
        }

        // Check if the v2 model is cached. The marker file contains the
        // repo name, so a stale v1 marker won't cause a false positive.
        let marker = pixel_recall::models_dir().join("potion.ok");
        let is_cached =
            std::fs::read_to_string(&marker).is_ok_and(|content| content.trim() == POTION_V2_REPO);

        if is_cached {
            // Cached — load now (fast, no network).
            // SAFETY: set_var is process-global. This runs on the request thread
            // before the embedder is opened, and nothing else reads
            // PIXEL_RECALL_MODEL_REPO concurrently.
            unsafe {
                std::env::set_var("PIXEL_RECALL_MODEL_REPO", POTION_V2_REPO);
            }
            match open_default_embedder(false) {
                Ok(e) => self.embedder = Some(e),
                Err(_) => self.embedder_unavailable = true,
            }
            return;
        }

        // Not cached — trigger a one-shot background download, degrade this
        // call. The thread is detached; it writes the marker on success.
        //
        // In daemon mode (long-running), the thread completes and the next
        // search picks up the cached model. In `--no-daemon` mode (one-shot
        // CLI), the process exits before the thread finishes — the user
        // sees the hint below and runs `pixel search-meaning` once to cache the model.
        if !self.embedder_download_started {
            self.embedder_download_started = true;
            eprintln!(
                "pixel: semantic channel unavailable — model not cached yet. \
                 Run `pixel search-meaning \"test\" .` once to download it (~1s), then retry \
                 `--scope hybrid`. Degrading to `code` ranking for this call."
            );
            std::thread::spawn(move || {
                // SAFETY: same variable and value as the cached path above; the only
                // reader is the embedder opened on this thread right after.
                unsafe {
                    std::env::set_var("PIXEL_RECALL_MODEL_REPO", POTION_V2_REPO);
                }
                // download=true: fetches + caches the model, writes marker.
                // Result is intentionally ignored — best-effort background
                // download; the next search call will recheck the marker.
                let _ = open_default_embedder(true);
            });
        }
        // Degrade: semantic_rank_for_search returns None → caller skips
        // the semantic channel, fusion is identical to `scope: "code"`.
    }

    /// Compute a semantic file ranking over the matched pool: embed the
    /// pattern and each file's matched lines (concatenated), then rank files
    /// by max cosine similarity. Returns `None` when the model is unavailable
    /// — the caller skips the semantic channel (graceful degradation, same
    /// pattern as graph-unavailable).
    ///
    /// Latency: static embeddings process the pool (~100 files × a few lines
    /// each) in single-digit milliseconds on CPU when the model is warm.
    fn semantic_rank_for_search(
        &mut self,
        matches: &[pixel_index::verify::MatchLine],
        pattern: &str,
    ) -> Option<Vec<String>> {
        self.ensure_embedder();
        let embedder = self.embedder.as_deref_mut()?;
        // Group matched lines by file.
        let mut by_file: HashMap<String, String> = HashMap::new();
        for m in matches {
            by_file.entry(m.path.clone()).or_default().push_str(&m.line);
            by_file.entry(m.path.clone()).or_default().push('\n');
        }
        if by_file.is_empty() {
            return Some(Vec::new());
        }
        let files: Vec<String> = by_file.keys().cloned().collect();
        let texts: Vec<String> = files.iter().map(|f| by_file[f].clone()).collect();
        // Embed the pattern (query) and the per-file matched-line blobs.
        let qvec = embedder
            .embed_batch(&[pattern], EmbedKind::Query)
            .ok()?
            .into_iter()
            .next()?;
        let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        let cvecs = embedder.embed_batch(&refs, EmbedKind::Passage).ok()?;
        if cvecs.len() != files.len() {
            return Some(Vec::new());
        }
        // Rank files by cosine similarity to the query.
        let mut scored: Vec<(f32, String)> = files
            .iter()
            .zip(&cvecs)
            .map(|(f, v)| (cosine_sim(&qvec, v), f.clone()))
            .collect();
        scored.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.1.cmp(&b.1))
        });
        Some(scored.into_iter().map(|(_, f)| f).collect())
    }

    pub fn handle(&mut self, req: Request) -> Response {
        let resp = self.handle_inner(req);
        // Envelope choke point: every response leaves through here, so a
        // structurally broken envelope (success without result, failure
        // without error, wrong protocol) cannot reach the wire unnoticed in
        // debug builds. Release builds skip the check; the contract tests
        // cover them.
        debug_assert!(
            resp.validate().is_ok(),
            "invalid envelope for op {}: {}",
            resp.op,
            resp.validate().unwrap_err()
        );
        resp
    }

    fn handle_inner(&mut self, req: Request) -> Response {
        let op_name = req.op_name();
        // Ops that return repo state attach a `snapshot` envelope field so
        // callers can correlate the answer with the exact working-tree state
        // it was computed against (HEAD, branch, dirty file list). This
        // covers the git-state ops AND every retrieval-class op: a retrieval
        // answer is only meaningful relative to the repo state it was
        // computed against.
        let attach_snapshot = matches!(
            op_name,
            "inspect" | "review" | "diff" | "status" | "changes"
        ) || is_retrieval_op(op_name);
        // Only the ops whose job is to report the working tree carry the
        // dirty path list. Everything else gets `dirty_count`: a retrieval
        // answer needs to say WHICH tree state it was computed against, not
        // enumerate 15 000 untracked `vendor/bundle` paths on every call.
        let full_dirty_list = matches!(op_name, "inspect" | "review");
        match self.dispatch(req) {
            Ok(v) => {
                let mut env = Envelope::success(op_name, v);
                if attach_snapshot {
                    let snapshot = self.repo_snapshot();
                    env = env.with_snapshot(if full_dirty_list {
                        snapshot
                    } else {
                        snapshot.compact()
                    });
                }
                // Epistemics choke point: EVERY successful retrieval-class
                // response carries an `epistemics` object — this is the ONLY
                // place retrieval envelopes are built, so an op cannot ship
                // without one. Ops that fired caps have them named in
                // `basis` and mirrored as envelope warnings; ops that
                // attested nothing get a conservative not-closed-world
                // default rather than an implied claim of completeness.
                if is_retrieval_op(op_name) {
                    let (epistemics, cap_warnings) =
                        derive_epistemics(op_name, env.result.as_ref().unwrap_or(&Value::Null));
                    env = env.with_epistemics(epistemics);
                    if !cap_warnings.is_empty() {
                        env = env.with_warnings(cap_warnings);
                    }
                }
                env
            }
            Err(msg) => failure_response(op_name, msg),
        }
    }

    /// Build an Envelope v2 `SnapshotInfo` from the current working-tree
    /// state: HEAD oid, branch name, and the list of dirty (modified /
    /// staged / untracked) repo-relative paths. `token` is left `None`
    /// here — pixel-ops computes the validated snapshot token separately.
    fn repo_snapshot(&self) -> SnapshotInfo {
        let head = pixel_index::gitsync::rev_parse_head(&self.root);
        let branch = pixel_index::gitsync::current_branch(&self.root);
        let dirty: Vec<String> = pixel_index::gitsync::status_porcelain(&self.root)
            .into_iter()
            .map(|(_xy, path)| path)
            .collect();
        SnapshotInfo {
            token: None,
            head,
            branch,
            dirty,
            dirty_count: None,
        }
    }

    fn dispatch(&mut self, req: Request) -> Result<Value, String> {
        match req {
            Request::Ping => Ok(json!({
                "pong": true,
                "root": self.root.display().to_string(),
                "protocol_version": PROTOCOL_VERSION,
            })),
            Request::Shutdown => Ok(json!({"shutting_down": true})),
            Request::Recall { .. } => Err(
                "recall ops are served by the recall daemon (`pixel recall daemon start`), not a repository daemon"
                    .to_string(),
            ),
            Request::Search {
                pattern,
                json: _,
                limit,
                offset,
                paths,
                scope,
            } => self.op_search(&pattern, limit, offset, paths.as_deref(), scope.as_deref()),
            Request::Targets { task, limit, max_tier, precision } => self.op_targets(&task, limit, max_tier.as_deref(), precision),
            Request::Symbol { name } => self.op_symbol(&name),
            Request::Skeleton { file } => self.op_skeleton(&file),
            Request::Context { uid, budget_tokens } => self.op_context(&uid, budget_tokens),
            Request::Impact {
                uid_or_name,
                direction,
                depth,
            } => self.op_impact(&uid_or_name, &direction, depth),
            Request::Uses {
                uid_or_name,
                role,
                offset,
            } => self.op_uses(&uid_or_name, &role, offset),
            Request::Trace { from, to } => self.op_trace(&from, &to),
            Request::Processes { offset } => self.op_processes(offset),
            Request::Clusters { offset } => self.op_clusters(offset),
            Request::Changes {
                base,
                offset,
                include_tests,
            } => self.op_changes(base.as_deref(), offset, include_tests),
            Request::Graph {} => self.op_graph(),
            Request::Status {} => self.op_status(),
            Request::Reindex {} => self.op_reindex(),
            Request::Resolve { phrase, limit } => self.op_resolve(&phrase, limit),
            Request::History { query, facet, limit } => {
                self.op_history(&query, facet.as_deref(), limit)
            }
            Request::Lifecycle { path, token } => self.op_lifecycle(path.as_deref(), token.as_deref()),
            Request::Excavate { phrase, path, from, to, limit } => {
                self.op_excavate(phrase.as_deref(), path.as_deref(), from.as_deref(), to.as_deref(), limit)
            }
            Request::Reconcile { strategy, push, into, request_id } => {
                self.op_reconcile(strategy.as_deref(), push.as_deref(), into.as_deref(), request_id.as_deref())
            }
            Request::Journal { kind, path, detail } => {
                self.op_journal(&kind, path.as_deref(), detail.as_deref())
            }
            Request::Inspect { .. } => {
                pixel_ops::inspect::inspect(&self.root)
            }
            Request::Review { cursor, byte_cap } => {
                pixel_ops::review::review(&self.root, cursor.as_deref(), byte_cap)
            }
            Request::Diff { from, to, paths, byte_cap } => {
                pixel_ops::diff::diff(&self.root, &from, to.as_deref(), paths.as_deref(), byte_cap)
            }
            Request::HistoryOp { ref_name, limit, detail, cursor, byte_cap } => {
                pixel_ops::history::history(
                    &self.root,
                    ref_name.as_deref(),
                    limit,
                    detail.as_deref().unwrap_or("compact"),
                    cursor.as_deref(),
                    byte_cap,
                )
            }
            Request::Publish {
                message,
                files,
                expected_head,
                push,
                amend,
                request_id,
            } => {
                let opts = pixel_ops::publish::PublishOptions {
                    message,
                    files,
                    expected_head,
                    expected_fingerprints: std::collections::BTreeMap::new(),
                    push: push.unwrap_or(false),
                    amend: amend.unwrap_or(false),
                    request_id,
                };
                pixel_ops::publish::publish(&self.root, &opts, None)
            }
            Request::Push {
                remote,
                refspec,
                force_with_lease,
                request_id,
            } => {
                let opts = pixel_ops::push::PushOptions {
                    remote,
                    refspec,
                    request_id,
                    force_with_lease: force_with_lease.unwrap_or(false),
                };
                pixel_ops::push::push(&self.root, &opts, None)
            }
            Request::Ship {
                message,
                files,
                remote,
                refspec,
                request_id,
            } => {
                pixel_ops::ship::ship(&self.root, &message, &files, &remote, &refspec, &request_id)
            }
            Request::BranchOp { name, from, request_id } => {
                let opts = pixel_ops::branch::BranchOptions {
                    name,
                    from,
                    request_id,
                };
                pixel_ops::branch::branch(&self.root, &opts)
            }
            Request::Update {
                expected_head,
                target_oid,
                request_id,
            } => {
                let opts = pixel_ops::update::UpdateOptions {
                    expected_head,
                    target_oid,
                    request_id,
                };
                pixel_ops::update::update(&self.root, &opts)
            }
            Request::Sync { remote, refspec } => {
                pixel_ops::sync::sync(&self.root, &remote, refspec.as_deref())
            }
            Request::Note { action, file, target, note } => {
                self.op_note(&action, file.as_deref(), target.as_deref(), note.as_deref())
            }
            Request::Map { markdown } => self.op_map(markdown),
            Request::Rename {
                name,
                new_name,
                file,
                uid,
                dry_run,
            } => self.op_rename(&name, &new_name, file.as_deref(), uid.as_deref(), dry_run),
            Request::Plan {
                prompt,
                query,
                tag,
                limit,
            } => self.op_plan(prompt.as_deref(), query.as_deref(), tag.as_deref(), limit),
            Request::Evaluate {
                from,
                to,
                traversal,
                tiers,
                max_depth,
                time_budget_ms,
                scope,
                at_snapshot,
            } => self.op_evaluate(EvaluateRequest {
                from,
                to,
                traversal,
                tiers,
                max_depth,
                time_budget_ms,
                scope,
                at_snapshot,
            }),
        }
    }

    // -- ops ---------------------------------------------------------------

    fn op_search(
        &mut self,
        pattern: &str,
        limit: Option<usize>,
        offset: Option<usize>,
        paths: Option<&[String]>,
        scope: Option<&str>,
    ) -> Result<Value, String> {
        // Default row limit and byte cap protect against broad patterns
        // (`.*`, short literals that hit every file) returning unbounded
        // output. A caller-provided limit overrides the row cap; the byte cap
        // always applies as a safety valve.
        const DEFAULT_LIMIT: usize = 100;
        const MAX_LIMIT: usize = 10_000;
        const BYTE_CAP: usize = 64 * 1024;
        /// Per-match text cap: a single match line is truncated to this many
        /// bytes so one oversized line cannot bypass the byte cap.
        const PER_MATCH_TEXT_CAP: usize = 4096;
        let row_limit = limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let offset = offset.unwrap_or(0);

        // `scope` selects match ORDER, not a different data source. `None`/
        // `""` is unranked path/line order; `"code"` (case-insensitive) is
        // ranked via pixel-rank-family RRF (M1 gate per PLAN.md). Any other
        // value used to fall through silently to unranked search (Bug 5) —
        // a typo'd or unimplemented scope gave the caller zero signal their
        // request wasn't honored.
        let scope_normalized = scope.map(str::to_lowercase);
        let (ranked, hybrid) = match scope_normalized.as_deref() {
            None | Some("") => (false, false),
            Some("code") => (true, false),
            Some("hybrid") => (true, true),
            Some(other) => {
                return Err(format!(
                    "unsupported search scope {other:?}; supported values are \"code\" \
                     (rank matches by file-level signals), \"hybrid\" \
                     (code + semantic embedding channel), or omitting `scope` for unranked \
                     path/line order"
                ));
            }
        };

        // Epistemics: the ranked branch's candidate pool is itself capped —
        // when it fires, ranking never even saw the overflow candidates.
        let mut ranked_pool_capped = false;
        let (matches, stats) = if ranked {
            // Ranked search cannot simply rerank the (offset, limit)-sliced
            // page the unranked branch fetches below: that page is sliced
            // in PATH order BEFORE ranking exists, so (a) the single
            // best-ranked match is invisible unless it happens to land
            // inside that slice (e.g. an exact filename match sorting
            // alphabetically last is never even fetched at `--limit 5`),
            // and (b) `next_offset` walks pre-rank order while the emitted
            // rows are in post-rank order — a page boundary and a rank
            // reordering disagreeing means paging duplicates or drops rows
            // (confirmed: 31 true matches paged via limit=40/offset=40
            // previously yielded 31 rows but only 30 distinct).
            //
            // Fix: fetch one bounded candidate POOL (bounded by
            // RANK_CANDIDATE_CAP — the same style of safety cap `row_limit`
            // already enforces everywhere else in this function; never
            // "fetch everything"), rank that whole pool ONCE, then serve
            // `offset`/`row_limit` as a plain slice over the resulting
            // stable array. Because the pool and its rank order are
            // recomputed identically on every call against the same repo
            // state (see `open_graph_for_ranking` for the accompanying
            // determinism fix), `offset` now indexes one coherent sequence:
            // paging through it can neither skip nor repeat a row.
            const RANK_CANDIDATE_CAP: usize = MAX_LIMIT;
            let (pool, pool_stats) = self
                .index
                .read()
                .expect("index lock poisoned")
                .search_page_in(pattern, 0, Some(RANK_CANDIDATE_CAP), paths)
                .map_err(|e| e.to_string())?;
            if pool_stats.truncated {
                ranked_pool_capped = true;
            }
            let ranking_graph = self.open_graph_for_ranking();
            // Precision 1: semantic channel. When `scope: "hybrid"` and the
            // embedding model is warm, compute a cosine-similarity file ranking
            // over the matched pool and fuse it as the 6th RRF channel. Degrades
            // gracefully: if the model is unavailable, `semantic_rank` is `None`
            // and the fusion is identical to `scope: "code"` (no regression).
            let semantic_rank: Option<Vec<String>> = if hybrid {
                self.semantic_rank_for_search(&pool, pattern)
            } else {
                None
            };
            let ranked_pool =
                rank_search_matches(&pool, pattern, &ranking_graph, semantic_rank.as_deref());

            let start = offset.min(ranked_pool.len());
            // More ranked rows remain beyond this page (independent of the
            // byte cap, which is applied below). Folded into `stats.truncated`
            // — the same meaning the unranked branch's `stats.truncated`
            // already carries: "the candidate/index layer says this page
            // isn't everything."
            let more_beyond_page = start.saturating_add(row_limit) < ranked_pool.len();
            let mut stats = pool_stats;
            stats.truncated = stats.truncated || more_beyond_page;
            (ranked_pool[start..].to_vec(), stats)
        } else {
            self.index
                .read()
                .expect("index lock poisoned")
                .search_page_in(pattern, offset, Some(row_limit), paths)
                .map_err(|e| e.to_string())?
        };

        // Render matches until either the row limit or the byte cap is hit.
        let mut arr: Vec<Value> = Vec::with_capacity(matches.len().min(row_limit));
        let mut bytes = 0usize;
        let mut byte_capped = false;
        for m in &matches {
            // Cap per-match text so a single oversized line cannot dominate.
            let text = if m.line.len() > PER_MATCH_TEXT_CAP {
                let mut end = PER_MATCH_TEXT_CAP;
                while !m.line.is_char_boundary(end) {
                    end -= 1;
                }
                format!("{}…[truncated]", &m.line[..end])
            } else {
                m.line.clone()
            };
            let entry = json!({"path": m.path, "line": m.line_number, "text": text});
            let entry_bytes = serde_json::to_vec(&entry)
                .map_err(|error| error.to_string())?
                .len();
            if bytes.saturating_add(entry_bytes) > BYTE_CAP {
                byte_capped = true;
                break;
            }
            bytes += entry_bytes;
            arr.push(entry);
            if arr.len() >= row_limit {
                break;
            }
        }
        // `truncated` is authoritative from the index layer (which knows if
        // more candidates exist) OR from the byte cap. Do NOT use
        // `arr.len() < matches.len()` alone — it can be true when exactly
        // `limit` matches were found but the byte cap reduced the output.
        let truncated = stats.truncated || byte_capped;
        let next_offset = truncated.then_some(offset.saturating_add(arr.len()));
        // Named caps for the envelope epistemics: every bound that actually
        // fired on THIS response, so a partial answer is explicitly bounded
        // instead of silently truncated.
        let mut caps: Vec<String> = Vec::new();
        if byte_capped {
            caps.push(format!(
                "output truncated by the {BYTE_CAP}-byte response cap; continue via next_offset"
            ));
        }
        if stats.truncated {
            caps.push(format!(
                "match list truncated at row limit {row_limit}; more matches exist — continue \
                 via next_offset"
            ));
        }
        if ranked_pool_capped {
            caps.push(format!(
                "ranked candidate pool capped at {MAX_LIMIT} matches; ranking never saw \
                 candidates beyond the cap"
            ));
        }
        Ok(json!({
            "matches": arr,
            "caps": caps,
            "truncated": truncated,
            "offset": offset,
            "next_offset": next_offset,
            "limit": row_limit,
            "byte_cap": BYTE_CAP,
            "match_count": arr.len(),
            "ranked": ranked,
            "stats": {
                "candidates": stats.candidates,
                "scanned_all": stats.scanned_all,
                "matches": stats.matches,
                "elapsed_us": stats.elapsed_us as u64,
                "truncated": stats.truncated,
            }
        }))
    }

    /// Sniper target list: tokenize the task, gather lexical + graph signals,
    /// fuse, tier. Graph failure degrades to lexical-only (envelope says so)
    /// instead of erroring — a scoping request must never die on a broken
    /// graph build.
    fn op_targets(
        &mut self,
        task: &str,
        limit: Option<usize>,
        max_tier: Option<&str>,
        precision: bool,
    ) -> Result<Value, String> {
        use pixel_graph::targets as graph_targets;
        use pixel_rank as engine;

        let started = Instant::now();
        let query = engine::tokenize_task(task)?;

        let ensured = self.ensure_graph();
        let graph_available = ensured.is_ok();
        let build_info = ensured.ok().flatten();

        let all_paths = self.admitted_paths();
        // S6: explicit file paths named in the task, matched against the
        // live tree before any lexical probe runs.
        let path_hits = engine::path_rank(&all_paths, &query.path_tokens);

        // S3: per-keyword content match counts (capped probes keep this ms-scale).
        let mut content_hits: BTreeMap<String, Vec<(String, u32)>> = BTreeMap::new();
        // Epistemics: every probe cap that fires is NAMED here and forces
        // lower_bound on the report envelope — a truncated probe must never
        // feed an "exhaustive" claim.
        let mut probe_caps: Vec<String> = Vec::new();
        // Phase 3 item 1 (targets evidence): keep the first ~2 match lines per
        // (file, keyword) so the caller can verify a target's content match
        // without re-searching. Near-zero cost — the lines are already fetched.
        let mut evidence: BTreeMap<String, Vec<Value>> = BTreeMap::new();

        let mut probe_keywords = query.keywords.clone();
        for exp in engine::expand_keywords(&query.keywords, query.language) {
            if !probe_keywords.contains(&exp) && probe_keywords.len() < 6 {
                probe_keywords.push(exp);
            }
        }

        for kw in &probe_keywords {
            // Word-bounded so "auth" cannot count every "author" as signal.
            // Keywords are [a-z0-9_]+ by construction (tokenize_task), but
            // escape defensively anyway.
            let pattern = format!(r"(?i)\b{}\b", regex_escape_keyword(kw));
            if let Ok((matches, probe_stats)) = self
                .index
                .read()
                .expect("index lock poisoned")
                .search_page_in(&pattern, 0, Some(CONTENT_PROBE_LIMIT), None)
            {
                if probe_stats.truncated {
                    probe_caps.push(format!(
                        "content probe truncated at {CONTENT_PROBE_LIMIT} matches for keyword \
                         '{kw}'; files beyond the cap carry no content signal"
                    ));
                }
                let mut counts: BTreeMap<String, u32> = BTreeMap::new();
                let mut kept_per_file: HashMap<String, usize> = HashMap::new();
                for m in matches {
                    *counts.entry(m.path.clone()).or_default() += 1;
                    let kept = kept_per_file.entry(m.path.clone()).or_insert(0);
                    if *kept < 2 {
                        *kept += 1;
                        evidence.entry(m.path.clone()).or_default().push(json!({
                            "line": m.line_number,
                            "text": m.line,
                            "keyword": kw,
                        }));
                    }
                }
                if !counts.is_empty() {
                    content_hits.insert(kw.clone(), counts.into_iter().collect());
                }
            }
        }

        let mut symbol_hits = Vec::new();
        let mut graph_neighbors: Vec<(String, String)> = Vec::new();
        let mut cluster_neighbors: Vec<(String, String)> = Vec::new();
        let mut envelope = None;
        if graph_available {
            let store = self.graph.as_ref().unwrap();
            symbol_hits = graph_targets::symbol_hits(store, &probe_keywords, &query.exact_tokens)
                .map_err(|e| e.to_string())?;

            // Graph expansion is seeded from the lexical pre-fuse so every
            // P1/P2 neighbor traces back to a lexical anchor.
            let seed_paths: Vec<String> = engine::lexical_rank(
                &all_paths,
                &probe_keywords,
                query.language,
                &symbol_hits,
                &content_hits,
            )
            .into_iter()
            .take(MAX_SEED_FILES)
            .collect();
            let seed_set: HashSet<&str> = seed_paths.iter().map(String::as_str).collect();
            let mut seed_symbol_ids: Vec<i64> = Vec::new();
            for hit in &symbol_hits {
                if seed_set.contains(hit.path.as_str()) {
                    for (sym, _) in &hit.symbols {
                        if seed_symbol_ids.len() < MAX_SEED_SYMBOLS {
                            seed_symbol_ids.push(sym.id);
                        }
                    }
                }
            }

            let mut seen: HashSet<String> = HashSet::new();
            for (path, reason) in graph_targets::neighbor_files(store, &seed_symbol_ids)
                .map_err(|e| e.to_string())?
                .into_iter()
                .chain(
                    graph_targets::import_adjacent_files(store, &seed_paths)
                        .map_err(|e| e.to_string())?,
                )
            {
                if seen.insert(path.clone()) {
                    graph_neighbors.push((path, reason));
                }
            }
            cluster_neighbors =
                graph_targets::cluster_co_files(store, &seed_symbol_ids, &probe_keywords)
                    .map_err(|e| e.to_string())?;

            let mut names: Vec<&str> = query.exact_tokens.iter().map(String::as_str).collect();
            for hit in &symbol_hits {
                for (sym, _) in &hit.symbols {
                    names.push(sym.name.as_str());
                }
            }
            envelope =
                Some(graph_targets::envelope_for_names(store, &names).map_err(|e| e.to_string())?);
        }

        let opts = engine::TargetsOptions {
            limit: limit.unwrap_or(engine::DEFAULT_LIMIT),
            max_tier: max_tier.map(String::from),
            precision_mode: precision,
        };
        let mut report = engine::compute_targets(
            task,
            &query,
            engine::SignalInputs {
                all_paths,
                content_hits,
                symbol_hits,
                path_hits,
                graph_neighbors,
                cluster_neighbors,
                graph_available,
                envelope,
                caps: probe_caps,
            },
            &opts,
        );
        // Phase 1c: rerank within tiers via the Engine-3 reranker (first
        // production call site). v1 = activity-only signals (the session and
        // error-sink channels are not wired here). The per-path test penalty
        // demotes test files only when the task does NOT mention tests/specs
        // (a test file is a worse target for a non-test task).
        let target_paths: Vec<String> = report.targets.iter().map(|t| t.path.clone()).collect();
        let signals = self.engine_signals(&target_paths);
        // Per-path test penalty: demote a test file only when the task does
        // NOT mention tests/specs (a test file is a *worse* target for a
        // non-test task, but a *better* one for a test task).
        let mentions_tests = task.split(|c: char| !c.is_ascii_alphanumeric()).any(|t| {
            matches!(
                t.to_ascii_lowercase().as_str(),
                "test" | "tests" | "spec" | "specs"
            )
        });
        let penalty = |path: &str| -> f64 {
            if pixel_rank::signals::is_test_path(path) && !mentions_tests {
                0.7
            } else {
                1.0
            }
        };
        // The formula reads the tunable coefficients instead of its own
        // literals: the table `engine_signals` scored this bundle with
        // (`SignalOptions::default()`; neither call site tunes the weights yet).
        let weights =
            engine::rerank::RerankWeights::from(&engine::signals::SignalOptions::default());
        report.targets =
            engine::rerank::rerank_targets(report.targets, &signals, &weights, penalty);
        // Cross-lingual semantic fallback: when lexical targeting returns 0
        // P0/P1 files (e.g. a French task against English code), embed the
        // query with the multilingual potion-code model and inject top-k
        // files as a "P1 (semantic)" tier. English queries that produce P0/P1
        // targets lexically never hit this path.
        let has_p0_p1 = report
            .targets
            .iter()
            .any(|t| matches!(t.tier.as_str(), "P0" | "P1"));
        if semantic_fallback_wanted(has_p0_p1, max_tier) {
            let eff_limit = limit.unwrap_or(engine::DEFAULT_LIMIT);
            let fallback =
                pixel_recall::code_search::semantic_fallback(&self.root, task, eff_limit);
            apply_semantic_leads(&mut report, &fallback, eff_limit);
        }
        let mut out = serde_json::to_value(&report).map_err(|e| e.to_string())?;
        // Phase 3 item 1: attach per-file content evidence to each target so
        // the caller can trust a content match without re-searching (S2).
        //
        // Capped to P0 targets, 2 lines total per target (not per keyword):
        // measured 2026-08-30 on a 6-keyword, 20-target `targets` response,
        // uncapped evidence was 7.6KB of a 18.3KB response (42%) — ~1900
        // extra tokens injected into the conversation on every scoping call,
        // most of it justifying P1/P2 files the rule text already calls
        // "peripheral and droppable". P0 is the only tier the doctrine
        // mandates checking before the first edit, so it's the only tier
        // worth spending the token budget to pre-justify.
        if let Some(targets) = out.get_mut("targets").and_then(Value::as_array_mut) {
            for t in targets {
                let is_p0 = t.get("tier").and_then(Value::as_str) == Some("P0");
                if !is_p0 {
                    continue;
                }
                if let Some(path) = t.get("path").and_then(Value::as_str)
                    && let Some(ev) = evidence.get(path)
                {
                    let trimmed: Vec<&Value> =
                        ev.iter().take(EVIDENCE_MAX_LINES_PER_TARGET).collect();
                    t["evidence"] = json!(trimmed);
                }
            }
        }
        // P2·2: merge durable human notes per target file — a human who
        // corrected the map sees the correction land in the closed list.
        // Notes are human-authored and rare, so no tier gate here (unlike
        // evidence): a note on a P2 file is still worth its few tokens.
        if let Some(store) = self.graph.as_ref()
            && let Some(targets) = out.get_mut("targets").and_then(Value::as_array_mut)
        {
            for t in targets.iter_mut() {
                let Some(path) = t.get("path").and_then(Value::as_str) else {
                    continue;
                };
                if let Ok(notes) = store.annotations_for_file(path)
                    && !notes.is_empty()
                {
                    t["notes"] = json!(
                        notes
                            .iter()
                            .map(|a| json!({"target": a.target, "note": a.note}))
                            .collect::<Vec<_>>()
                    );
                }
            }
        }
        if let Some(stats) = out.get_mut("stats") {
            stats["elapsed_ms"] = json!(started.elapsed().as_millis() as u64);
            stats["commit_oid"] = json!(
                self.index
                    .read()
                    .expect("index lock poisoned")
                    .status()
                    .commit_oid
            );
        }
        merge_build_info(&mut out, build_info);
        Ok(out)
    }

    fn op_symbol(&mut self, name: &str) -> Result<Value, String> {
        let built = self.ensure_graph()?;
        let store = self.graph.as_ref().unwrap();
        let files = file_map(store)?;
        let syms = store
            .symbols_by_name(name, None, 50)
            .map_err(|e| e.to_string())?;
        let envelope = store.envelope_for_name(name).map_err(|e| e.to_string())?;
        let mut out = json!({
            "symbols": syms.iter().map(|s| symbol_json(s, &files)).collect::<Vec<_>>(),
            "envelope": envelope,
        });
        merge_build_info(&mut out, built);
        Ok(out)
    }

    /// `pixel list-signatures <file>` — all signatures in a file at ~10% of Read cost.
    /// The user-supplied path is converted to repo-relative before lookup.
    fn op_skeleton(&mut self, file: &str) -> Result<Value, String> {
        let built = self.ensure_graph()?;
        let store = self.graph.as_ref().unwrap();
        let files = file_map(store)?;
        // Normalize the user path to repo-relative (strip root, forward slashes).
        let rel = normalize_file_arg(&self.root, file);
        let file_row = store
            .file_by_path(&rel)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("no indexed file matching '{file}' (looked for '{rel}')"))?;
        let syms = store
            .symbols_in_file(file_row.id)
            .map_err(|e| e.to_string())?;
        let mut out = json!({
            "file": file_row.path,
            "lang": file_row.lang,
            "symbols": syms.iter().map(|s| symbol_json(s, &files)).collect::<Vec<_>>(),
        });
        merge_build_info(&mut out, built);
        Ok(out)
    }

    fn op_context(&mut self, uid: &str, budget_tokens: Option<usize>) -> Result<Value, String> {
        let built = self.ensure_graph()?;
        let store = self.graph.as_ref().unwrap();
        let files = file_map(store)?;
        let sym = store
            .symbol_by_uid(uid)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("no symbol with uid {uid:?}"))?;
        let envelope = store
            .envelope_for_name(&sym.name)
            .map_err(|e| e.to_string())?;
        let sym_json = symbol_json(&sym, &files);

        let budget = budget_tokens.unwrap_or(2000);
        let value_tokens =
            |value: &Value| estimate_tokens(&serde_json::to_string(value).unwrap_or_default());
        // `budget_basis` declares the approximation behind the token cap:
        // `estimate_tokens` is a bytes/4 heuristic, not a real tokenizer, so
        // the fit is approximate and the response says so instead of
        // presenting the heuristic as an exact token count.
        let minimum_response = json!({
            "budget_tokens": budget,
            "budget_basis": pixel_context::BUDGET_BASIS,
            "rendered_tokens": 0,
            "budgeted": true,
            "truncated": false,
            "text": "",
        });
        let minimum = value_tokens(&minimum_response);
        if budget < minimum {
            return Err(format!(
                "context budget {budget} is below the minimum response size of {minimum} tokens"
            ));
        }

        // Verify only files whose source will be returned, once per request.
        // Reuse these exact bytes for excerpts; never pair fresh file contents
        // with stale graph spans or stored Crux. No repository freshness sweep.
        let mut sources = HashMap::new();
        let stale_response = |mut response: Value| -> Result<Value, String> {
            response["truncated"] = json!(true);
            response["caps"] = json!([
                "context truncated: source differs from graph snapshot or is unavailable; stale excerpts and Crux omitted"
            ]);
            if value_tokens(&response) > budget {
                return Err("context source is stale or unavailable; budget cannot fit the freshness warning".to_owned());
            }
            Ok(response)
        };
        if validated_context_source(&self.root, store, sym.file_id, &files, &mut sources).is_none()
        {
            return stale_response(minimum_response);
        }

        let mut incoming = store.edges_to(sym.id, None).map_err(|e| e.to_string())?;
        let mut outgoing = store.edges_from(sym.id, None).map_err(|e| e.to_string())?;
        let edge_order = |a: &EdgeRow, b: &EdgeRow| {
            a.kind
                .as_str()
                .cmp(b.kind.as_str())
                .then(a.tier.as_str().cmp(b.tier.as_str()))
                .then(a.site_line.cmp(&b.site_line))
                .then(a.src_id.cmp(&b.src_id))
                .then(a.dst_id.cmp(&b.dst_id))
        };
        incoming.sort_by(edge_order);
        outgoing.sort_by(edge_order);
        incoming.dedup_by(|a, b| {
            a.src_id == b.src_id
                && a.dst_id == b.dst_id
                && a.kind == b.kind
                && a.tier == b.tier
                && a.site_line == b.site_line
        });
        outgoing.dedup_by(|a, b| {
            a.src_id == b.src_id
                && a.dst_id == b.dst_id
                && a.kind == b.kind
                && a.tier == b.tier
                && a.site_line == b.site_line
        });

        // Bound source retained before rendering. The target gets priority;
        // neighbors share only the remaining aggregate allowance.
        let mut source_remaining = budget.saturating_mul(4).min(MAX_CONTEXT_SOURCE_BYTES);
        let mut items = Vec::new();
        let target_cap = source_remaining.min(MAX_TARGET_SNIPPET_BYTES);
        let target = context_item(
            sources
                .get(&sym.file_id)
                .and_then(Option::as_deref)
                .unwrap(),
            &sym,
            &files,
            target_cap,
            &store
                .symbol_crux_by_id(sym.id)
                .map_err(|e| e.to_string())?
                .iter()
                .map(|c| (c.line, c.text.clone()))
                .collect::<Vec<_>>(),
        );
        source_remaining = source_remaining.saturating_sub(target.snippet.len());
        items.push(target);
        let mut seen_context = std::collections::HashSet::from([sym.id]);
        let neighbors = incoming
            .iter()
            .map(|edge| edge.src_id)
            .chain(outgoing.iter().map(|edge| edge.dst_id));
        let mut source_elided_items = 0usize;
        for symbol_id in neighbors {
            if !seen_context.insert(symbol_id) {
                continue;
            }
            if items.len() >= MAX_CONTEXT_ITEMS || source_remaining == 0 {
                source_elided_items += 1;
                continue;
            }
            if let Some(other) = symbol_by_id(store, symbol_id) {
                if validated_context_source(&self.root, store, other.file_id, &files, &mut sources)
                    .is_none()
                {
                    return stale_response(minimum_response);
                }
                let cap = source_remaining.min(MAX_NEIGHBOR_SNIPPET_BYTES);
                let item = context_item(
                    sources
                        .get(&other.file_id)
                        .and_then(Option::as_deref)
                        .unwrap(),
                    &other,
                    &files,
                    cap,
                    &store
                        .symbol_crux_by_id(symbol_id)
                        .map_err(|e| e.to_string())?
                        .iter()
                        .map(|c| (c.line, c.text.clone()))
                        .collect::<Vec<_>>(),
                );
                source_remaining = source_remaining.saturating_sub(item.snippet.len());
                items.push(item);
            }
        }

        let incoming_total = incoming.len();
        let outgoing_total = outgoing.len();
        let incoming_compact = compact_edges(store, &incoming, &files, false, EDGE_LIMIT)?;
        let outgoing_compact = compact_edges(store, &outgoing, &files, true, EDGE_LIMIT)?;
        let mut response = minimum_response;
        response["truncated"] = json!(
            incoming_total > EDGE_LIMIT || outgoing_total > EDGE_LIMIT || source_elided_items > 0
        );
        for (key, value) in [
            ("symbol", sym_json),
            (
                "envelope",
                serde_json::to_value(envelope).unwrap_or(Value::Null),
            ),
            ("incoming", incoming_compact),
            ("outgoing", outgoing_compact),
            ("incoming_total", json!(incoming_total)),
            ("outgoing_total", json!(outgoing_total)),
            ("context_items_total", json!(seen_context.len())),
            ("context_items_loaded", json!(items.len())),
        ] {
            let mut candidate = response.clone();
            candidate[key] = value;
            if value_tokens(&candidate) <= budget {
                response = candidate;
            } else {
                response["truncated"] = json!(true);
            }
        }
        if let Some(build_info) = built {
            let mut candidate = response.clone();
            candidate["graph_build"] = build_info;
            if value_tokens(&candidate) <= budget {
                response = candidate;
            } else {
                response["truncated"] = json!(true);
            }
        }

        let overhead = value_tokens(&response);
        let (mut text, layer, fit_elided_items) =
            bridge::render_context(&items, budget.saturating_sub(overhead));
        for (key, value) in [
            ("context_layer", json!(layer)),
            (
                "elided_items",
                json!(source_elided_items.saturating_add(fit_elided_items)),
            ),
        ] {
            let mut candidate = response.clone();
            candidate[key] = value;
            if value_tokens(&candidate) <= budget {
                response = candidate;
            } else {
                response["truncated"] = json!(true);
            }
        }
        if layer != "L2" || fit_elided_items > 0 {
            response["truncated"] = json!(true);
        }
        loop {
            response["text"] = json!(text);
            response["rendered_tokens"] = json!(estimate_tokens(
                response["text"].as_str().unwrap_or_default()
            ));
            if value_tokens(&response) <= budget {
                break;
            }
            let chars = response["text"]
                .as_str()
                .unwrap_or_default()
                .chars()
                .count();
            if chars == 0 {
                break;
            }
            response["truncated"] = json!(true);
            text = text.chars().take(chars.saturating_sub(1)).collect();
        }
        Ok(response)
    }

    fn op_impact(
        &mut self,
        uid_or_name: &str,
        direction: &str,
        depth: Option<u32>,
    ) -> Result<Value, String> {
        let built = self.ensure_graph()?;
        let store = self.graph.as_ref().unwrap();
        let sym = match resolve_symbol(store, uid_or_name)? {
            Resolved::One(s) => s,
            Resolved::Many(v) => return candidates_value(store, &v),
        };
        let mut out = bridge::impact(store, &sym.uid, direction, depth.unwrap_or(3))?;
        merge_build_info(&mut out, built);
        Ok(out)
    }

    fn op_uses(
        &mut self,
        uid_or_name: &str,
        role: &str,
        offset: Option<usize>,
    ) -> Result<Value, String> {
        let built = self.ensure_graph()?;
        let store = self.graph.as_ref().unwrap();
        let sym = match resolve_symbol(store, uid_or_name)? {
            Resolved::One(s) => s,
            Resolved::Many(v) => return candidates_value(store, &v),
        };
        let files = file_map(store)?;
        let (edges, other_is_src) = match role {
            "callees" => (store.edges_from(sym.id, Some(EdgeKind::Calls)), false),
            _ => (store.edges_to(sym.id, Some(EdgeKind::Calls)), true),
        };
        let mut edges = edges.map_err(|e| e.to_string())?;
        edges.sort_by(|a, b| {
            a.site_line
                .cmp(&b.site_line)
                .then(a.src_id.cmp(&b.src_id))
                .then(a.dst_id.cmp(&b.dst_id))
                .then(a.tier.as_str().cmp(b.tier.as_str()))
        });
        let total_edges = edges.len();
        let offset = offset.unwrap_or(0).min(total_edges);
        let mut arr = Vec::with_capacity(total_edges.min(EDGE_LIMIT));
        for e in edges.iter().skip(offset).take(EDGE_LIMIT) {
            let other_id = if other_is_src { e.src_id } else { e.dst_id };
            let other = symbol_by_id(store, other_id);
            arr.push(json!({
                "symbol": other.as_ref().map(|s| symbol_json(s, &files)),
                "tier": e.tier.as_str(),
                "site_line": e.site_line,
            }));
        }
        let mut envelope = json!(
            store
                .envelope_for_name(&sym.name)
                .map_err(|e| e.to_string())?
        );
        if role == "callees" {
            // Name-based uncertainty describes incoming calls. Outgoing queries
            // must also account for unresolved calls enclosed by this symbol.
            // Only calls: an unresolved callback argument is a reference the
            // symbol passes on, not a callee it invokes.
            let unresolved: u64 = store
                .conn()
                .query_row(
                    "SELECT COUNT(*) FROM unresolved_calls WHERE enclosing_symbol_id = ?1 AND kind = 'calls'",
                    [sym.id],
                    |row| row.get::<_, i64>(0).map(|count| count as u64),
                )
                .map_err(|e| e.to_string())?;
            envelope["unresolved_outgoing"] = json!(unresolved);
            if unresolved > 0 {
                envelope["lower_bound"] = json!(true);
                envelope["caps"] = json!([format!(
                    "graph lower bound: {unresolved} unresolved outgoing call site(s) — \
                     callees beyond this answer may exist"
                )]);
            }
        }
        let returned_edges = arr.len();
        let has_more = offset.saturating_add(returned_edges) < total_edges;
        let mut out = json!({
            "symbol": symbol_json(&sym, &files),
            "role": if role == "callees" { "callees" } else { "callers" },
            "edges": arr,
            "total_edges": total_edges,
            "returned_edges": returned_edges,
            "edge_limit": EDGE_LIMIT,
            "offset": offset,
            "next_offset": has_more.then_some(offset.saturating_add(returned_edges)),
            "truncated": has_more,
            "envelope": envelope,
        });
        merge_build_info(&mut out, built);
        Ok(out)
    }

    fn op_trace(&mut self, from: &str, to: &str) -> Result<Value, String> {
        let built = self.ensure_graph()?;
        let store = self.graph.as_ref().unwrap();
        let from_sym = match resolve_symbol(store, from)? {
            Resolved::One(s) => s,
            Resolved::Many(v) => return candidates_value(store, &v),
        };
        let to_sym = match resolve_symbol(store, to)? {
            Resolved::One(s) => s,
            Resolved::Many(v) => return candidates_value(store, &v),
        };
        let mut out = bridge::trace(store, &from_sym.uid, &to_sym.uid)?;
        merge_build_info(&mut out, built);
        Ok(out)
    }

    fn op_processes(&mut self, offset: Option<usize>) -> Result<Value, String> {
        let built = self.ensure_graph()?;
        let store = self.graph.as_mut().unwrap();
        let mut out = bridge::processes(store, offset.unwrap_or(0))?;
        merge_build_info(&mut out, built);
        Ok(out)
    }

    /// `pixel plan`: the findings of the requested plan queries over the
    /// daemon's graph, brought up to date first like every graph op.
    fn op_plan(
        &mut self,
        prompt: Option<&str>,
        query: Option<&str>,
        tag: Option<&str>,
        limit: Option<usize>,
    ) -> Result<Value, String> {
        let queries = pixel_graph::plan::plan_queries(prompt, query, tag, limit)?;
        let built = self.ensure_graph()?;
        let runner = pixel_git::GitRunner::new(&self.root);
        let store = self.graph.as_ref().unwrap();
        let findings = pixel_graph::plan::run_plan_queries(store, &self.root, &runner, &queries)
            .map_err(|e| format!("plan: {e}"))?;
        let names: Vec<&str> = queries
            .iter()
            .map(pixel_graph::plan::PlanQuery::name)
            .collect();
        let mut out = json!({ "queries": names, "findings": findings });
        merge_build_info(&mut out, built);
        Ok(out)
    }

    /// `evaluate`: does a path exist from `from` to `to`, and what proves it.
    ///
    /// The answer is attributed to a graph generation, so the op does more
    /// than run the traversal: it refuses to answer from a graph it cannot
    /// name (no database, drift past the incremental threshold, a withheld
    /// signature), and it brackets the traversal with a whole-tree check so
    /// a negative cannot come from a tree that had already grown the edge.
    fn op_evaluate(&mut self, req: EvaluateRequest) -> Result<Value, String> {
        self.op_evaluate_probed(req, &mut || {})
    }

    /// [`Self::op_evaluate`] with a seam for tests: `probe` runs after the
    /// before-check and before the after-check, which is exactly the window
    /// a concurrent edit has to slip through.
    fn op_evaluate_probed(
        &mut self,
        req: EvaluateRequest,
        probe: &mut dyn FnMut(),
    ) -> Result<Value, String> {
        let args = req.parse()?;
        let epistemics =
            |caps: Vec<String>| derive_epistemics("evaluate", &json!({ "caps": caps })).0;

        // Nothing to attribute an answer to: report it, never build one.
        // A rebuild is a decision the caller makes with a command of its own.
        if let Err(reason) = self.evaluate_gate()? {
            let halted = evaluate::halted(reason, None, &args, false, epistemics(Vec::new()));
            return serde_json::to_value(wire::Output::Evaluation(Box::new(halted)))
                .map_err(|e| e.to_string());
        }

        let db = self.graph_db_path();
        if self.graph.is_none() {
            self.graph = Some(GraphStore::open(&db).map_err(|e| e.to_string())?);
        }
        let root = self.root.clone();
        let store = self.graph.as_ref().expect("opened above");

        // Identity and rows come from one handle, so they are one
        // generation: the incremental writers commit rows and signature in
        // a single transaction, so no interleaving can show one without the
        // other.
        let identity = match evaluate::identity(store) {
            Ok(identity) => identity,
            // A store that could not be read is a technical failure, not an
            // absence: it leaves as `Err` and exits 3 rather than being
            // published as a reason the caller would read as an answer.
            Err(evaluate::Failure::Store(error)) => return Err(format!("evaluate: {error}")),
            Err(evaluate::Failure::Halt(evaluate::Halt(reason))) => {
                let halted = evaluate::halted(reason, None, &args, false, epistemics(Vec::new()));
                return serde_json::to_value(wire::Output::Evaluation(Box::new(halted)))
                    .map_err(|e| e.to_string());
            }
        };

        let resolved =
            evaluate::resolve_argument(store, "--from", &args.from, args.scope.as_deref())
                .and_then(|from| {
                    evaluate::resolve_argument(store, "--to", &args.to, args.scope.as_deref())
                        .map(|to| (from, to))
                });
        let (from, to) = match resolved {
            Ok(pair) => pair,
            Err(evaluate::Failure::Store(error)) => return Err(format!("evaluate: {error}")),
            Err(evaluate::Failure::Halt(evaluate::Halt(reason))) => {
                let halted =
                    evaluate::halted(reason, Some(&identity), &args, true, epistemics(Vec::new()));
                return serde_json::to_value(wire::Output::Evaluation(Box::new(halted)))
                    .map_err(|e| e.to_string());
            }
        };

        probe();

        let (traversal, tiers) = evaluate::request_shape(&args);
        let mut clock = pixel_graph::predicate::WallClock::start();
        let evaluation = pixel_graph::predicate::evaluate(
            store,
            pixel_graph::predicate::Request {
                sources: &[from.id],
                targets: &[to.id],
                traversal,
                tiers,
                budget: pixel_graph::predicate::Budget {
                    max_depth: args.max_depth,
                    time_budget: std::time::Duration::from_millis(args.time_budget_ms),
                },
            },
            &mut clock,
        )
        .map_err(|e| format!("evaluate: {e}"))?;

        // The after-check. Skipped only under `--at-snapshot`, where the
        // envelope says so and the answer is explicitly about the stored
        // snapshot rather than the tree on disk.
        if !args.at_snapshot && !evaluate::tree_matches(&root, &identity.signature) {
            let halted = evaluate::halted(
                wire::Reason::SnapshotChanged,
                Some(&identity),
                &args,
                false,
                epistemics(Vec::new()),
            );
            return serde_json::to_value(wire::Output::Evaluation(Box::new(halted)))
                .map_err(|e| e.to_string());
        }

        let caps = evaluate_caps(&evaluation);
        // The cap the loaded graph was built under, not the one this
        // process happens to have in its environment: a daemon restarted
        // with a different `PIXEL_GRAPH_MAX_FILES` must not describe an
        // older graph with a ceiling that never applied to it.
        let built_cap = {
            let store = self.graph.as_ref().expect("opened above");
            pixel_graph::build::stored_graph_file_cap(store)
                .map_err(|error| format!("evaluate: {error}"))?
        };
        let file_cap_hit = self.graph_file_cap_hit(built_cap);
        let store = self.graph.as_ref().expect("opened above");
        let envelope = evaluate::envelope(
            store,
            &evaluation,
            &identity,
            &args,
            epistemics(caps),
            file_cap_hit,
        );
        serde_json::to_value(wire::Output::Evaluation(Box::new(envelope)))
            .map_err(|e| e.to_string())
    }

    /// Bring the graph to a generation an answer can name, or say why not.
    ///
    /// Drift under the incremental threshold is applied, as every graph op
    /// does. Above it, and for a graph with no usable signature, the answer
    /// is `graph_stale`: a full rebuild can take minutes and is never a side
    /// effect of a question.
    fn evaluate_gate(&mut self) -> Result<Result<(), wire::Reason>, String> {
        let db = self.graph_db_path();
        if !db.exists() {
            return Ok(Err(wire::Reason::GraphUnavailable));
        }
        let delta = match bridge::tree_delta(&self.root, &db) {
            Ok(delta) => delta,
            Err(error) => return Err(format!("evaluate: {error}")),
        };
        match gate_action(delta.as_ref(), incremental_max_pct()) {
            GateAction::Ready => Ok(Ok(())),
            GateAction::Incremental => {
                // `Incremental` is only ever returned for a `Some` delta.
                if let Some(delta) = delta {
                    self.incremental_update_info(&delta)?;
                }
                Ok(Ok(()))
            }
            GateAction::Stale => Ok(Err(wire::Reason::GraphStale)),
        }
    }

    /// Whether the graph holds at least as many files as the build cap
    /// admits, which is when the walk stopped at the cap rather than at the
    /// end of the tree. Conservative: it can only over-report, and an
    /// over-report widens the stated limits of an absence rather than
    /// narrowing them.
    ///
    /// `cap` is passed in rather than read from the environment here so a
    /// test can reach all three cases — no cap, under it, at or over it —
    /// without setting `PIXEL_GRAPH_MAX_FILES`, which is process-global and
    /// would cap the walk of every graph another test builds concurrently
    /// in the same binary.
    fn graph_file_cap_hit(&self, cap: Option<usize>) -> bool {
        let Some(cap) = cap else {
            return false;
        };
        self.graph
            .as_ref()
            .and_then(|store| store.counts().ok())
            .is_some_and(|(files, _, _, _)| files >= cap as u64)
    }

    fn op_graph(&mut self) -> Result<Value, String> {
        let publication = Arc::clone(&self.publication);
        let mut state = publication.write().expect("publication lock poisoned");
        state.healthy = false;
        let (stats, build_ms) = self.rebuild_graph()?;
        state.generation += 1;
        state.healthy = true;
        Ok(json!({
            "files": stats.get("files").cloned().unwrap_or(Value::Null),
            "symbols": stats.get("symbols").cloned().unwrap_or(Value::Null),
            "edges": stats.get("edges").cloned().unwrap_or(Value::Null),
            "unresolved": stats.get("unresolved").cloned().unwrap_or(Value::Null),
            "elapsed_ms": build_ms,
        }))
    }

    fn op_clusters(&mut self, offset: Option<usize>) -> Result<Value, String> {
        let built = self.ensure_graph()?;
        let store = self.graph.as_mut().unwrap();
        let mut out = bridge::clusters(store, offset.unwrap_or(0))?;
        merge_build_info(&mut out, built);
        Ok(out)
    }

    fn op_changes(
        &mut self,
        base: Option<&str>,
        offset: Option<usize>,
        include_tests: bool,
    ) -> Result<Value, String> {
        const SYMBOL_LIMIT: usize = 20;
        const PROCESS_LIMIT: usize = 20;
        const PROCESSES_PER_SYMBOL_LIMIT: usize = 10;
        let offset = offset.unwrap_or(0);
        let built = self.ensure_graph()?;
        let root = self.root.clone();
        let store = self.graph.as_ref().unwrap();
        let mut out = bridge::changes(store, &root, base, include_tests)?;
        let mut nested_processes_truncated = false;
        let symbols_total = out["symbols"].as_array().map_or(0, Vec::len);
        if let Some(symbols) = out["symbols"].as_array_mut() {
            symbols.sort_by(|a, b| {
                a["path"]
                    .as_str()
                    .cmp(&b["path"].as_str())
                    .then(a["uid"].as_str().cmp(&b["uid"].as_str()))
            });
            let start = offset.min(symbols.len());
            let end = start.saturating_add(SYMBOL_LIMIT).min(symbols.len());
            *symbols = symbols.drain(start..end).collect();
            for symbol in symbols {
                let process_total = symbol["processes"].as_array().map_or(0, Vec::len);
                if let Some(processes) = symbol["processes"].as_array_mut() {
                    processes.truncate(PROCESSES_PER_SYMBOL_LIMIT);
                }
                if let Some(object) = symbol.as_object_mut() {
                    object.insert("processes_total".into(), json!(process_total));
                    object.insert(
                        "processes_truncated".into(),
                        json!(process_total > PROCESSES_PER_SYMBOL_LIMIT),
                    );
                }
                nested_processes_truncated |= process_total > PROCESSES_PER_SYMBOL_LIMIT;
            }
        }
        let affected_processes_total = out["affected_processes"].as_array().map_or(0, Vec::len);
        if let Some(processes) = out["affected_processes"].as_array_mut() {
            let start = offset.min(processes.len());
            let end = start.saturating_add(PROCESS_LIMIT).min(processes.len());
            *processes = processes.drain(start..end).collect();
        }
        let returned_symbols = out["symbols"].as_array().map_or(0, Vec::len);
        let returned_processes = out["affected_processes"].as_array().map_or(0, Vec::len);
        let has_more = offset.saturating_add(returned_symbols) < symbols_total
            || offset.saturating_add(returned_processes) < affected_processes_total;
        if let Some(object) = out.as_object_mut() {
            object.insert("symbols_total".into(), json!(symbols_total));
            object.insert("returned_symbols".into(), json!(returned_symbols));
            object.insert("symbol_limit".into(), json!(SYMBOL_LIMIT));
            object.insert("offset".into(), json!(offset));
            object.insert(
                "next_offset".into(),
                json!(
                    has_more
                        .then_some(offset.saturating_add(returned_symbols.max(returned_processes)))
                ),
            );
            object.insert(
                "affected_processes_total".into(),
                json!(affected_processes_total),
            );
            object.insert("process_limit".into(), json!(PROCESS_LIMIT));
            object.insert(
                "returned_affected_processes".into(),
                json!(returned_processes),
            );
            object.insert(
                "truncated".into(),
                json!(has_more || nested_processes_truncated),
            );
        }
        merge_build_info(&mut out, built);
        Ok(out)
    }

    fn op_status(&mut self) -> Result<Value, String> {
        let s = self.index.read().expect("index lock poisoned").status();
        let db = self.graph_db_path();
        let graph = if db.exists() {
            match GraphStore::open(&db) {
                Ok(store) => {
                    let (files, symbols, edges, unresolved) =
                        store.counts().map_err(|e| e.to_string())?;
                    json!({
                        "present": true,
                        "files": files,
                        "symbols": symbols,
                        "edges": edges,
                        "unresolved_calls": unresolved,
                    })
                }
                Err(e) => json!({"present": true, "error": e.to_string()}),
            }
        } else {
            json!({"present": false})
        };
        Ok(json!({
            "root": self.root.display().to_string(),
            "index": {
                "commit_oid": s.commit_oid,
                "base_files": s.base_files,
                "delta_files": s.delta_files,
                "overlay_files": s.overlay_files,
                "tombstones": s.tombstones,
            },
            "graph": graph,
            "watcher": {
                // Failures that used to be swallowed: a non-zero count here
                // means answers may have been served from a stale index.
                "graph_update_failures": self.graph_failures.count(),
                "notify_errors": self.watcher_failures.count(),
            },
            "facts": self.facts_visibility(),
        }))
    }

    /// Force-rebuild the text index shard via the daemon (singleton path:
    /// no concurrent build races because the daemon serializes requests).
    fn op_reindex(&mut self) -> Result<Value, String> {
        let publication = Arc::clone(&self.publication);
        let mut state = publication.write().expect("publication lock poisoned");
        state.healthy = false;
        // Remove the existing shard so open_or_build is forced to rebuild.
        let gpx = self.root.join(pixel_index::index::SHARD_DIR);
        let base = gpx.join(pixel_index::index::SHARD_FILE);
        std::fs::remove_file(&base).ok();
        std::fs::remove_file(pixel_index::delta::delta_shard_path(&gpx)).ok();
        std::fs::remove_file(pixel_index::delta::state_path(&gpx)).ok();

        // Re-open the index (the build lock ensures no race even if a
        // concurrent CLI also tries to build).
        let extractor: Box<dyn pixel_index::GramExtractor> =
            Box::new(pixel_index::TrigramExtractor);
        let new_index =
            pixel_index::indexset::IndexSet::open_or_build_bypass_cache(&self.root, extractor)
                .map_err(|e| e.to_string())?;
        *self.index.write().expect("index lock poisoned") = new_index;
        let s = self.index.read().expect("index lock poisoned").status();
        state.generation += 1;
        state.healthy = true;
        Ok(json!({
            "root": self.root.display().to_string(),
            "index": {
                "commit_oid": s.commit_oid,
                "base_files": s.base_files,
                "delta_files": s.delta_files,
                "overlay_files": s.overlay_files,
                "tombstones": s.tombstones,
            },
        }))
    }

    /// `pixel rename` — graph-driven, tree-sitter-verified identifier rename.
    /// `uid` or `file` disambiguate a shared name; `dry_run` returns the same
    /// verified edit set without touching files.
    fn op_rename(
        &mut self,
        name: &str,
        new_name: &str,
        file: Option<&str>,
        uid: Option<&str>,
        dry_run: bool,
    ) -> Result<Value, String> {
        let built = self.ensure_graph()?;
        let store = self.graph.as_ref().unwrap();
        let files = file_map(store)?;
        if !new_name
            .chars()
            .next()
            .is_some_and(|c| c.is_alphabetic() || c == '_')
            || !new_name.chars().all(|c| c.is_alphanumeric() || c == '_')
        {
            return Err(format!(
                "rename: {new_name:?} is not an identifier (letters, digits, `_`, non-digit first)"
            ));
        }

        let sym = if let Some(uid) = uid {
            store
                .symbol_by_uid(uid)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("no symbol with uid {uid:?}"))?
        } else {
            let mut syms = store
                .symbols_by_name(name, None, 50)
                .map_err(|e| e.to_string())?;
            if let Some(file) = file {
                let rel = normalize_file_arg(&self.root, file);
                let file_row = store
                    .file_by_path(&rel)
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| format!("no indexed file matching '{file}'"))?;
                syms.retain(|s| s.file_id == file_row.id);
            }
            match syms.len() {
                0 => {
                    return Err(format!(
                        "no symbol named {name:?}{}",
                        file.map(|f| format!(" in {f}")).unwrap_or_default()
                    ));
                }
                1 => syms.into_iter().next().unwrap(),
                _ => {
                    let mut out = candidates_value(store, &syms)?;
                    out["hint"] =
                        json!("ambiguous name; re-call with --file <path> or --uid <uid>");
                    return Ok(out);
                }
            }
        };

        let plan = pixel_graph::rename::plan(store, &self.root, &sym, new_name)?;
        let mut out = json!({
            "symbol": symbol_json(&sym, &files),
            "old_name": sym.name,
            "new_name": new_name,
            "dry_run": dry_run,
            "edits": plan
                .files
                .iter()
                .map(|(path, edits)| json!({
                    "path": path,
                    "edits": edits
                        .iter()
                        .map(|e| json!({
                            "line": e.line,
                            "kind": e.kind.as_str(),
                        }))
                        .collect::<Vec<_>>(),
                }))
                .collect::<Vec<_>>(),
            "edit_count": plan.files.values().map(Vec::len).sum::<usize>(),
            "skipped": plan.skipped,
            "unclaimed_text": plan.unclaimed_text,
        });
        if !dry_run {
            let written = pixel_graph::rename::apply(&self.root, &plan, &sym.name, new_name)?;
            out["applied"] = json!(written);
            // The store is now stale: the renamed files' rows no longer
            // match disk. Drop the handle so the next op re-syncs via the
            // tree delta instead of serving pre-rename spans.
            self.graph = None;
        }
        merge_build_info(&mut out, built);
        Ok(out)
    }

    /// Facts/history visibility for `op_status`: enough counters to tell a
    /// healthy db from a dead or poisoned one at a glance. Read-only — never
    /// triggers ingest (status must stay cheap).
    fn facts_visibility(&self) -> Value {
        let facts = match FactsStore::open(&self.root) {
            Ok(f) => f,
            Err(e) => return json!({"present": false, "error": e.to_string()}),
        };
        let state = facts.index_state();
        let count =
            |sql: &str| -> i64 { facts.conn().query_row(sql, [], |r| r.get(0)).unwrap_or(0) };
        let phase_a_done: bool = facts
            .conn()
            .query_row(
                "SELECT status FROM ingest_jobs WHERE phase = 'A'",
                [],
                |r| r.get::<_, String>(0),
            )
            .is_ok_and(|s| s == "done");
        // Full repo commit count via rev-list so a frozen enumeration is
        // visible as commits_indexed < total_commits. The facts universe also
        // covers stash/reflog-only commits that `--all` doesn't count, so take
        // the max — indexed exceeding rev-list is healthy, not suspicious.
        let total_commits = pixel_git::GitRunner::new(&self.root)
            .rev_list_count_all()
            .unwrap_or(0)
            .max(state.total_commits);
        json!({
            "present": true,
            "schema_version": state.schema_version,
            "phase": state.phase,
            "phase_a_done": phase_a_done,
            "commits_indexed": state.commits_indexed,
            "total_commits": total_commits,
            "diff_indexed_pct": state.diff_indexed_pct,
            "hunks_with_text": count(
                "SELECT count(*) FROM hunks WHERE length(added) > 0 OR length(removed) > 0"
            ),
            "diff_grams": count("SELECT count(*) FROM diff_grams"),
            "fresh": state.fresh,
        })
    }

    // -- Engine 1 / M3 / M4 / M5 ops --------------------------------------

    /// Engine 1: concept-index resolution cascade.
    fn op_resolve(&mut self, phrase: &str, limit: Option<usize>) -> Result<Value, String> {
        let ensured = self.ensure_graph();
        let build_info = ensured.ok().flatten();
        let store = match self.graph.as_ref() {
            Some(s) => s,
            None => {
                // Graph unavailable (e.g. non-git dir where build was refused
                // or capped). The semantic fallback needs no graph — try it
                // before returning the empty unresolved outcome.
                let semantic = pixel_recall::code_search::semantic_fallback(
                    &self.root,
                    phrase,
                    limit.unwrap_or(8),
                );
                let matches: Vec<Value> = semantic
                    .hits
                    .iter()
                    .map(|(path, score)| {
                        json!({
                            "path": path,
                            "start_line": 0,
                            "end_line": 0,
                            "kind": null,
                            "raw": phrase,
                            "norm": phrase,
                            "owner": null,
                            "symbol_kind": null,
                            "score": score,
                            "tier": "semantic",
                            "reasons": ["semantic lead (no graph, unverified)"],
                        })
                    })
                    .collect();
                let confidence = if matches.is_empty() {
                    "unresolved"
                } else {
                    "ranked"
                };
                let tier: Option<&str> = if matches.is_empty() {
                    None
                } else {
                    Some("semantic")
                };
                return Ok(serde_json::json!({
                    "confidence": confidence,
                    "tier": tier,
                    "matches": matches,
                    "tiers_attempted": [],
                    "scan_capped": false,
                    "basis": if matches.is_empty() {
                        "graph unavailable — no concept index to resolve against. Use `pixel search-content` for text matching."
                    } else {
                        "graph unavailable; semantic fallback via multilingual embeddings"
                    },
                    "index_state": {
                        "concepts": 0,
                        "concepts_version": null,
                        "fresh": false,
                    },
                    "caps": semantic.caps(),
                    "envelope": {
                        "graph": "unavailable",
                        "lower_bound": true,
                    },
                }));
            }
        };
        // Phase 1c: feed activity-only rerank signals (git churn) over the
        // candidate universe; session/error channels land in Phase 3.
        let all_paths = self.admitted_paths();
        let signals = self.engine_signals(&all_paths);
        let opts = pixel_graph::concept_resolve::ResolveOptions {
            limit: limit.unwrap_or(8),
            // Phase 1c: wire the real Engine-3 reranker (per-path test
            // penalty) instead of the default LexicalReranker.
            reranker: Some(Box::new(EngineReranker::new(phrase))),
            signals: pixel_graph::concept_resolve::SignalBundle {
                activity: signals.activity,
                session: signals.session,
                session_reasons: signals.session_reasons,
                error_reasons: signals.error_reasons,
            },
        };
        let outcome = pixel_graph::concept_resolve::resolve(store, phrase, &opts)
            .map_err(|e| e.to_string())?;
        let mut out = serde_json::to_value(&outcome).map_err(|e| e.to_string())?;
        // Cross-lingual semantic fallback: when lexical matching returns 0
        // matches (e.g. a French task against English code), embed the query
        // with the multilingual potion-code model and match against code
        // files. This is a fallback, not a replacement — English queries
        // that resolve lexically never hit this path.
        let matches_empty = out
            .get("matches")
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty);
        if matches_empty {
            // A zero limit asks for no match: nothing to embed.
            if let Some(limit) = std::num::NonZeroUsize::new(limit.unwrap_or(8)) {
                let fallback =
                    pixel_recall::code_search::semantic_fallback(&self.root, phrase, limit.get());
                let hits = &fallback.hits;
                if !hits.is_empty() {
                    // Emit the full ConceptMatch shape so downstream consumers
                    // (enrich_resolve_matches_with_context, notes merge) can
                    // process semantic hits the same way as lexical ones.
                    let semantic_matches: Vec<Value> = hits
                        .iter()
                        .map(|(path, score)| {
                            json!({
                                "path": path,
                                "start_line": 0,
                                "end_line": 0,
                                "kind": null,
                                "raw": phrase,
                                "norm": phrase,
                                "owner": null,
                                "symbol_kind": null,
                                "score": score,
                                "tier": "semantic",
                                "reasons": ["semantic lead (unverified)"],
                            })
                        })
                        .collect();
                    out["matches"] = json!(semantic_matches);
                    out["confidence"] = json!("ranked");
                    out["tier"] = json!("semantic");
                    // Record the fallback in tiers_attempted for the audit
                    // trail — a consumer reconciling which tiers ran sees it.
                    if let Some(tiers) =
                        out.get_mut("tiers_attempted").and_then(Value::as_array_mut)
                    {
                        tiers.push(json!("semantic"));
                    }
                    out["basis"] = json!(
                        "lexical matching returned 0 results; semantic fallback via multilingual embeddings"
                    );
                    out["caps"] = json!(fallback.caps());
                }
            }
        }
        // P2·2: merge durable human notes onto matches — keyed by concept
        // norm or owner symbol name inside the match's file, so a human
        // correction surfaces exactly where the agent lands.
        if let Some(matches) = out.get_mut("matches").and_then(Value::as_array_mut) {
            for m in matches.iter_mut() {
                let path = m.get("path").and_then(Value::as_str).unwrap_or("");
                if path.is_empty() {
                    continue;
                }
                let mut keys: Vec<&str> = Vec::new();
                if let Some(n) = m.get("norm").and_then(Value::as_str) {
                    keys.push(n);
                }
                if let Some(o) = m.get("owner").and_then(Value::as_str) {
                    keys.push(o);
                }
                if keys.is_empty() {
                    continue;
                }
                if let Ok(notes) = store.annotations_for_symbols(path, &keys)
                    && !notes.is_empty()
                {
                    m["notes"] = json!(
                        notes
                            .iter()
                            .map(|a| json!({"target": a.target, "note": a.note}))
                            .collect::<Vec<_>>()
                    );
                }
            }
        }
        merge_build_info(&mut out, build_info);
        Ok(out)
    }

    /// Lazy ingest on the query path: when the facts index is not fresh
    /// (never built, poisoned-and-rebuilt, or refs moved), run bounded ingest
    /// ticks before serving so a CLI with no daemon still gets real answers.
    /// Budget: `PIXEL_FACTS_QUERY_BUDGET_MS` (default 3000ms) — a query is
    /// never blocked longer than that; the attached `index_state` tells the
    /// caller whether coverage is complete.
    fn facts_open_and_catch_up(&self) -> Result<FactsStore, String> {
        let mut facts = FactsStore::open(&self.root).map_err(|e| e.to_string())?;
        if !facts.index_state().fresh {
            pixel_facts::ingest::lazy_ingest(&mut facts)
                .map_err(|e| format!("facts lazy ingest failed: {e}"))?;
        }
        // First facts use: start the keep-fresh warm loop. `lazy_ingest` above
        // serves this request within its budget; the warmer finishes the job
        // in the background and keeps the index fresh for later queries.
        if self.facts_warmer_needed() {
            crate::daemon::spawn_facts_ingest(&self.root);
        }
        Ok(facts)
    }

    /// True exactly once per Service: the first facts-consuming request.
    /// Claims the flag atomically so a second caller never double-spawns.
    fn facts_warmer_needed(&self) -> bool {
        !self.facts_warmer_started.swap(true, Ordering::SeqCst)
    }

    /// M3 / Engine 2: history-wide fact + diff search.
    fn op_history(
        &mut self,
        query: &str,
        facet: Option<&str>,
        limit: Option<usize>,
    ) -> Result<Value, String> {
        let facts = self.facts_open_and_catch_up()?;
        let result = pixel_facts::search::search(
            &facts,
            query,
            facet.unwrap_or("all").into(),
            limit.unwrap_or(200),
        )
        .map_err(|e| e.to_string())?;
        let mut value = serde_json::to_value(&result).map_err(|e| e.to_string())?;
        value["index_state"] =
            serde_json::to_value(facts.index_state()).map_err(|e| e.to_string())?;
        Ok(value)
    }

    /// Engine 2: lifecycle of a path or token.
    fn op_lifecycle(&mut self, path: Option<&str>, token: Option<&str>) -> Result<Value, String> {
        let facts = self.facts_open_and_catch_up()?;
        let result = match (path, token) {
            (Some(p), _) => facts.path_lifecycle(p).map_err(|e| e.to_string())?,
            (None, Some(t)) => facts.token_lifecycle(t).map_err(|e| e.to_string())?,
            (None, None) => return Err("lifecycle requires a path or token".to_string()),
        };
        let mut value = serde_json::to_value(&result).map_err(|e| e.to_string())?;
        value["index_state"] =
            serde_json::to_value(facts.index_state()).map_err(|e| e.to_string())?;
        Ok(value)
    }

    /// Engine 2: history-wide discovery (rescue v2).
    fn op_excavate(
        &mut self,
        phrase: Option<&str>,
        path: Option<&str>,
        from: Option<&str>,
        to: Option<&str>,
        limit: Option<usize>,
    ) -> Result<Value, String> {
        let facts = self.facts_open_and_catch_up()?;
        // Default cut from 200 to 15: only the top SNIPPET_TOP_N=5 candidates
        // ever carry a code snippet, so the other ~195 were pure metadata
        // rows a caller almost never needs — measured 2026-08-30, a 31-hit
        // query returned ~8,200 tokens of JSON where the useful signal
        // (5 ranked, snippet-bearing candidates) was under 3,500. `--limit`
        // still overrides for a caller that genuinely wants the long tail.
        let result = facts
            .excavate(phrase, path, from, to, limit.unwrap_or(15))
            .map_err(|e| e.to_string())?;
        serde_json::to_value(&result).map_err(|e| e.to_string())
    }

    /// Engine 4: one-call deterministic branch sync (delegates to pixel-ops).
    fn op_reconcile(
        &mut self,
        strategy: Option<&str>,
        push: Option<&str>,
        into: Option<&str>,
        request_id: Option<&str>,
    ) -> Result<Value, String> {
        let opts = pixel_ops::reconcile::ReconcileOptions {
            strategy: strategy.unwrap_or("report").to_string(),
            push: push.unwrap_or("auto").to_string(),
            request_id: request_id.unwrap_or("").to_string(),
            into_target: into.map(str::to_string),
        };
        pixel_ops::reconcile::reconcile(&self.root, &opts)
    }

    /// M5: journal a session event into the session db (fire-and-forget).
    fn op_journal(
        &mut self,
        kind: &str,
        path: Option<&str>,
        detail: Option<&str>,
    ) -> Result<Value, String> {
        let store = pixel_session::store::Store::open(&self.root).map_err(|e| e.to_string())?;
        let data = match (path, detail) {
            (Some(p), Some(d)) => Some(json!({"path": p, "detail": d})),
            (Some(p), None) => Some(json!({"path": p})),
            (None, Some(d)) => Some(json!({"detail": d})),
            (None, None) => None,
        };
        let id = store
            .record_event_raw(kind, data.as_ref(), None)
            .map_err(|e| e.to_string())?;
        Ok(json!({"recorded": true, "id": id, "kind": kind}))
    }

    /// P2·2: human notes — set/get/rm/list durable annotations keyed by
    /// `file` + `target` (a symbol `name` or concept `norm`). Notes are
    /// human-owned: they survive rebuilds and are merged into
    /// `resolve`/`targets` results.
    ///
    /// Opens `graph.db` directly (schema-only when absent) — a note must be
    /// writable before any graph build. Deliberately does NOT cache the
    /// opened store in `self.graph`: an annotations-only store would poison
    /// `ensure_graph`'s `is_some()` short-circuit and graph ops would read
    /// an empty graph instead of building it.
    fn op_note(
        &mut self,
        action: &str,
        file: Option<&str>,
        target: Option<&str>,
        note: Option<&str>,
    ) -> Result<Value, String> {
        // Normalize to the repo-relative convention used by every other op
        // (and by the resolve/targets merge) — an absolute path from the
        // CLI must still key the same row.
        let norm_file = file.map(|f| {
            let p = Path::new(f);
            if p.is_absolute() {
                p.strip_prefix(&self.root)
                    .map_or_else(|_| f.to_string(), |r| r.to_string_lossy().into_owned())
            } else {
                f.trim_start_matches("./").to_string()
            }
        });
        // Fresh handle when the graph isn't already open; never cached (see
        // doc comment — caching an annotations-only store would starve the
        // graph build path).
        let owned = match self.graph.as_ref() {
            Some(_) => None,
            None => Some(GraphStore::open(&self.graph_db_path()).map_err(|e| e.to_string())?),
        };
        let store = owned.as_ref().or(self.graph.as_ref()).unwrap();
        match action {
            "set" => {
                let (file, target, note) = match (norm_file.as_deref(), target, note) {
                    (Some(f), Some(t), Some(n))
                        if !f.is_empty() && !t.is_empty() && !n.is_empty() =>
                    {
                        (f, t, n)
                    }
                    _ => return Err("note set requires <file> <target> <note>".to_string()),
                };
                store
                    .set_annotation(file, target, note)
                    .map_err(|e| e.to_string())?;
                Ok(json!({"ok": true, "file": file, "target": target, "note": note}))
            }
            "get" => {
                let (file, target) = match (norm_file.as_deref(), target) {
                    (Some(f), Some(t)) if !f.is_empty() && !t.is_empty() => (f, t),
                    _ => return Err("note get requires <file> <target>".to_string()),
                };
                let note = store
                    .get_annotation(file, target)
                    .map_err(|e| e.to_string())?;
                Ok(json!({"file": file, "target": target, "note": note}))
            }
            "rm" | "delete" => {
                let (file, target) = match (norm_file.as_deref(), target) {
                    (Some(f), Some(t)) if !f.is_empty() && !t.is_empty() => (f, t),
                    _ => return Err("note rm requires <file> <target>".to_string()),
                };
                let existed = store
                    .get_annotation(file, target)
                    .map_err(|e| e.to_string())?
                    .is_some();
                store
                    .delete_annotation(file, target)
                    .map_err(|e| e.to_string())?;
                Ok(json!({"ok": true, "removed": existed, "file": file, "target": target}))
            }
            "list" => {
                const NOTE_LIST_CAP: u32 = 500;
                let rows = match norm_file.as_deref() {
                    Some(f) => store.annotations_for_file(f).map_err(|e| e.to_string())?,
                    None => store
                        .all_annotations(NOTE_LIST_CAP)
                        .map_err(|e| e.to_string())?,
                };
                let capped = norm_file.is_none() && rows.len() as u32 >= NOTE_LIST_CAP;
                Ok(json!({
                    "notes": rows,
                    "total": rows.len(),
                    "capped": capped,
                }))
            }
            other => Err(format!(
                "unknown note action '{other}' — expected set|get|rm|list"
            )),
        }
    }

    /// P2·2: structural repo map — every indexed file with its symbols,
    /// sorted by path. `markdown` additionally emits the exportable document
    /// (`markdown` field): directory sections, per-file headings, one bullet
    /// per symbol. This is the human-editable projection of the graph — the
    /// notes written via `note set` key on the same `file`/`target` pairs
    /// the map prints, so a human can correct the map and the correction
    /// merges back into `resolve`/`targets`.
    fn op_map(&mut self, markdown: bool) -> Result<Value, String> {
        let ensured = self.ensure_graph();
        let build_info = ensured.ok().flatten();
        let Some(store) = self.graph.as_ref() else {
            return Err(
                "graph unavailable — no index to map. Run `pixel rebuild-graph .` first"
                    .to_string(),
            );
        };
        let files = store.files().map_err(|e| e.to_string())?;
        let truncated = files.len() > MAP_FILE_CAP;
        let mut grouped: Vec<(&FileRow, Vec<SymbolRow>)> = Vec::new();
        let mut symbol_total = 0usize;
        for f in files.iter().take(MAP_FILE_CAP) {
            let mut syms = store.symbols_in_file(f.id).map_err(|e| e.to_string())?;
            syms.sort_by_key(|s| s.start_line);
            symbol_total += syms.len();
            grouped.push((f, syms));
        }
        grouped.sort_by(|a, b| a.0.path.cmp(&b.0.path));

        let mut md = String::new();
        if markdown {
            use std::fmt::Write;
            let root_name = self.root.file_name().map_or_else(
                || self.root.display().to_string(),
                |n| n.to_string_lossy().into_owned(),
            );
            let _ = writeln!(
                md,
                "# pixel repo-map — {root_name}\n\n{} files · {} symbols",
                grouped.len(),
                symbol_total
            );
            let mut cur_dir = String::new();
            for (f, syms) in &grouped {
                let dir = f.path.rsplit_once('/').map_or("", |(d, _)| d).to_string();
                if dir != cur_dir {
                    let _ = writeln!(md, "\n## {}", if dir.is_empty() { "." } else { &dir });
                    cur_dir = dir;
                }
                let _ = writeln!(md, "\n### `{}`", f.path);
                for s in syms {
                    let sig = s.sig.trim();
                    if sig.is_empty() {
                        let _ = writeln!(
                            md,
                            "- `{}` **{}** (L{}–{})",
                            s.kind.as_str(),
                            s.name,
                            s.start_line,
                            s.end_line
                        );
                    } else {
                        let _ = writeln!(
                            md,
                            "- `{}` **{}** (L{}–{}) — `{}`",
                            s.kind.as_str(),
                            s.name,
                            s.start_line,
                            s.end_line,
                            sig
                        );
                    }
                }
            }
            if truncated {
                let _ = writeln!(
                    md,
                    "\n> truncated at {MAP_FILE_CAP} files — narrow with `pixel list-signatures <file>`"
                );
            }
        }

        let mut out = json!({
            "files": grouped.iter().map(|(f, syms)| json!({
                "path": f.path,
                "lang": f.lang,
                "symbols": syms.iter().map(|s| json!({
                    "name": s.name,
                    "kind": s.kind.as_str(),
                    "start_line": s.start_line,
                    "end_line": s.end_line,
                    "sig": s.sig,
                })).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
            "file_count": grouped.len(),
            "symbol_count": symbol_total,
            "truncated": truncated,
        });
        if markdown {
            out["markdown"] = json!(md);
        }
        merge_build_info(&mut out, build_info);
        Ok(out)
    }

    /// Engine-3 rerank signals shared by `op_resolve` and `op_targets`.
    /// Activity-only: git churn over the last 90 days (via the one-shot
    /// `git log --name-only` fallback) plus the current dirty set. The session
    /// and error-sink channels are NOT wired here (`session_store: None`, no
    /// events), so `session` and the error reasons stay empty for every
    /// production caller. Deterministic for a fixed repo state; a failed or
    /// capped activity scan degrades to an empty `activity` map whose reason
    /// is named in `SignalBundle::activity_unavailable` (the reranker then
    /// applies only the per-path test penalty).
    fn engine_signals(&self, candidates: &[String]) -> pixel_rank::signals::SignalBundle {
        if self.read_only {
            return pixel_rank::signals::SignalBundle {
                activity_unavailable: Some(
                    "history enrichment excluded from interactive evidence reads".into(),
                ),
                ..Default::default()
            };
        }
        use pixel_rank::signals::{SignalOptions, compute_signals};
        let runner = pixel_git::GitRunner::new(&self.root);
        let dirty: Vec<String> = pixel_index::gitsync::status_porcelain(&self.root)
            .into_iter()
            .map(|(p, _)| p)
            .collect();
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as i64);
        // Fan-in (in-degree): count incoming `calls` edges per candidate
        // file via the graph.db escape hatch (`GraphStore::conn`). The graph
        // is lazily built by `ensure_graph` earlier in the request; if it is
        // unavailable, fan-in degrades to empty (the reranker then applies
        // only activity + session + test penalty).
        let fan_in_raw: std::collections::HashMap<String, u64> = self
            .graph
            .as_ref()
            .map(|store| fan_in_counts(store.conn(), candidates))
            .unwrap_or_default();
        let opts = SignalOptions {
            now_ms,
            ..Default::default()
        };
        compute_signals(
            &runner,
            None,
            &[],
            None,
            &dirty,
            &fan_in_raw,
            candidates,
            &opts,
        )
        .unwrap_or_default()
    }
}

/// Count incoming `calls` edges per candidate file from the graph.db.
/// `conn` is the `GraphStore::conn()` escape hatch. Returns a path → count
/// map; paths with no incoming calls edges are absent (treated as 0 by the
/// scorer). Any SQL failure degrades to an empty map (the reranker then
/// ignores fan-in).
fn fan_in_counts(
    conn: &rusqlite::Connection,
    candidates: &[String],
) -> std::collections::HashMap<String, u64> {
    use std::collections::HashMap;
    let mut out: HashMap<String, u64> = HashMap::new();
    if candidates.is_empty() {
        return out;
    }
    // Build a positional IN-list of placeholders matching the candidate
    // count; rusqlite's `params!` macro handles the binding.
    let placeholders: Vec<&str> = (0..candidates.len()).map(|_| "?").collect();
    let sql = format!(
        "SELECT f.path, COUNT(*) AS fan_in \
         FROM edges e \
         JOIN symbols s ON e.dst_id = s.id \
         JOIN files f ON s.file_id = f.id \
         WHERE e.kind = 'calls' AND f.path IN ({}) \
         GROUP BY f.path",
        placeholders.join(", ")
    );
    let params: Vec<&dyn rusqlite::ToSql> = candidates
        .iter()
        .map(|c| c as &dyn rusqlite::ToSql)
        .collect();
    let Ok(mut rows) = conn.prepare(&sql) else {
        return out;
    };
    let queried = rows
        .query_map(params.as_slice(), |r| {
            // COUNT(*) is an integer; rusqlite has no `FromSql` for `u64`,
            // so read it as `i64` and cast (counts are non-negative).
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64))
        })
        .ok();
    if let Some(iter) = queried {
        for row in iter.flatten() {
            out.insert(row.0, row.1);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// epistemics — the honesty layer every retrieval-class response carries
// ---------------------------------------------------------------------------

/// The retrieval-class ops: answers computed FROM repo state (index/graph/
/// working tree) whose completeness can silently degrade under caps. Every
/// one of these MUST ship an `epistemics` object (enforced in
/// `Service::handle` + the `every_retrieval_op_response_carries_epistemics`
/// test). Mutation and admin ops (publish/push/ping/…) are not listed: they
/// report what they DID, not what exists, so completeness honesty does not
/// apply the same way.
/// Whether `targets` may add semantic leads: only when the lexical pass found
/// nothing in P0/P1, and only when the caller accepts P2 (the tier the leads
/// land in).
fn semantic_fallback_wanted(has_p0_p1: bool, max_tier: Option<&str>) -> bool {
    !has_p0_p1 && max_tier.is_none_or(|m| m == "P2")
}

/// Append `fallback`'s new leads (see [`semantic_leads`]) after `report`'s
/// targets, capped at `limit`, and record the fallback: `stats.fallback`,
/// `envelope.lower_bound` and the caps. A fallback that adds no lead leaves
/// the report as the lexical pass wrote it.
fn apply_semantic_leads(
    report: &mut pixel_rank::TargetsReport,
    fallback: &pixel_recall::code_search::SemanticFallback,
    limit: usize,
) {
    let leads = semantic_leads(fallback, &report.targets);
    if leads.is_empty() {
        return;
    }
    let hits_count = leads.len();
    report.targets.extend(leads);
    report.targets.truncate(limit);
    if let Some(stats) = report.stats.as_object_mut() {
        stats.insert(
            "fallback".to_string(),
            json!({
                "channel": "semantic",
                "reason": "no_p0_p1",
                "hits": hits_count,
                "searched_files": fallback.searched_files,
                "file_limit_reached": fallback.file_limit_reached,
            }),
        );
    }
    if let Some(envelope) = report.envelope.as_object_mut() {
        envelope.insert("lower_bound".to_string(), json!(true));
        if let Some(caps) = envelope
            .entry("caps")
            .or_insert_with(|| json!([]))
            .as_array_mut()
        {
            caps.extend(fallback.caps().into_iter().map(Value::from));
        }
    }
}

/// The semantic hits not already targeted, as P2 targets appended after the
/// lexical ones: their similarity cannot tell a related file from an
/// unrelated one, so they are leads to check, never a likely (P1) target.
fn semantic_leads(
    fallback: &pixel_recall::code_search::SemanticFallback,
    targets: &[pixel_rank::TargetFile],
) -> Vec<pixel_rank::TargetFile> {
    let existing: HashSet<&str> = targets.iter().map(|t| t.path.as_str()).collect();
    fallback
        .hits
        .iter()
        .filter(|(path, _)| !existing.contains(path.as_str()))
        .map(|(path, score)| pixel_rank::TargetFile {
            path: path.clone(),
            tier: "P2".to_string(),
            score: *score,
            reasons: vec![format!("semantic lead (similarity {score:.2}, unverified)")],
            symbols: Vec::new(),
        })
        .collect()
}

pub const RETRIEVAL_OPS: &[&str] = &[
    "search",
    "resolve",
    "targets",
    "impact",
    "uses",
    "trace",
    "changes",
    "context",
    "symbol",
    "processes",
    "clusters",
    "plan",
];

fn is_retrieval_op(op_name: &str) -> bool {
    RETRIEVAL_OPS.contains(&op_name)
}

/// Backslash-escape regex metacharacters in a probe keyword. Keywords from
/// `tokenize_task` are `[a-z0-9_]+` by construction, so this is a no-op in
/// practice, but the probe must stay literal even if that invariant ever
/// changes upstream. Equivalent to `regex::escape` for the ASCII range
/// without pulling the `regex` crate into this crate's dependency set.
fn regex_escape_keyword(kw: &str) -> String {
    let mut out = String::with_capacity(kw.len());
    for c in kw.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            out.push(c);
        } else if c.is_ascii() {
            out.push('\\');
            out.push(c);
        } else {
            out.push(c);
        }
    }
    out
}

/// Derive the envelope [`Epistemics`] (and mirrored cap warnings) for a
/// retrieval-class response from the completeness markers the op's result
/// carries.
///
/// Why derivation-at-the-choke-point instead of a compile-time typed builder
/// per op: `dispatch` funnels 30+ ops through `Result<Value, String>`, and
/// several result shapes are produced by crates other agents own
/// (pixel-ops) — a full typed-response refactor ripples across ownership
/// boundaries. This function plus the response-walk test gives the same
/// guarantee mechanically: a retrieval response cannot ship without
/// epistemics, and an op that attests nothing is published as
/// `closed_world: false` (conservative) rather than silently complete.
///
/// Markers consumed (ops embed these in their result JSON):
/// - `caps: [string]` — named caps the op fired (search, targets via
///   `envelope.caps`).
/// - `truncated: bool` + `next_offset` — pagination/byte caps.
/// - `envelope.lower_bound` / `envelope.unresolved_same_name` — the graph
///   honesty envelope (impact/uses/symbol/context/targets).
/// - `confidence` (top-level or `envelope.confidence`) — epistemic label such
///   as `resolved`/`ranked`/`unresolved` from resolve and ranked results.
/// - `scan_capped` + `basis` — resolve's bounded fallback scans.
/// - `graph_build.build_ms` presence — the graph was rebuilt for THIS answer,
///   so staleness is 0ms (the one cheap staleness signal available).
fn derive_epistemics(op_name: &str, v: &Value) -> (Epistemics, Vec<Warning>) {
    let mut caps: Vec<String> = Vec::new();

    // Op-declared named caps (top-level and inside the targets envelope).
    for path in ["/caps", "/envelope/caps"] {
        if let Some(arr) = v.pointer(path).and_then(Value::as_array) {
            caps.extend(arr.iter().filter_map(Value::as_str).map(String::from));
        }
    }

    // Generic pagination / byte-cap truncation.
    if v.get("truncated").and_then(Value::as_bool) == Some(true) {
        // Search already names its caps in `caps`; avoid a duplicate
        // generic entry when specific ones exist for this marker.
        let already_named = caps.iter().any(|c| c.contains("truncated"));
        if !already_named {
            if v.get("next_offset").is_some_and(|n| !n.is_null()) {
                caps.push(
                    "results truncated by row/byte cap; more exist — continue via next_offset"
                        .to_string(),
                );
            } else {
                caps.push("results truncated by an output cap".to_string());
            }
        }
    }

    // Graph honesty envelope (impact/uses/symbol/context; targets folds its
    // graph state into envelope.caps + note instead).
    let graph_lower = v.pointer("/envelope/lower_bound").and_then(Value::as_bool) == Some(true);
    if graph_lower {
        let unresolved = v
            .pointer("/envelope/unresolved_same_name")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if unresolved > 0 {
            caps.push(format!(
                "graph lower bound: {unresolved} unresolved same-name call site(s) — \
                 edges beyond this answer may exist"
            ));
        } else if caps.is_empty() {
            caps.push("graph lower bound: resolver could not close the world".to_string());
        }
    }

    // Resolve's bounded fallback scans.
    if v.get("scan_capped").and_then(Value::as_bool) == Some(true) {
        caps.push(
            "fallback table scan hit its row cap; unscanned rows were never considered".to_string(),
        );
    }

    let source = match op_name {
        "search" => "text index",
        "targets" | "resolve" => "text index + code graph",
        "changes" => "code graph + working-tree diff",
        _ => "code graph",
    };
    let mut basis = String::from(source);
    if let Some(tier_basis) = v.get("basis").and_then(Value::as_str) {
        // resolve: which tier produced the answer.
        basis.push_str("; ");
        basis.push_str(tier_basis);
    }
    if !caps.is_empty() {
        basis.push_str("; caps: ");
        basis.push_str(&caps.join("; "));
    }

    // Static analysis (tree-sitter) cannot guarantee it found every call
    // site: callbacks passed as arguments, dynamic dispatch, macro-generated
    // calls, and eval are all invisible to it. This is an *extraction* limit
    // — distinct from the *resolution* uncertainty (`lower_bound`) the
    // envelope already tracks (same-name unresolved calls). Because
    // extraction limits always apply, `closed_world` is never true: a
    // "0 callers" answer means "no callers found", not "this symbol has no
    // callers". This is Pixel's "I say when I don't know" value prop.
    let extraction_limits = vec![
        "callbacks passed as arguments (e.g. schema.plugin(fn), emitter.on('event', fn))"
            .to_string(),
        "dynamic dispatch (e.g. obj[methodName]())".to_string(),
        "macro-generated calls".to_string(),
        "eval / new Function".to_string(),
    ];
    if caps.is_empty() {
        basis.push_str(
            "; static analysis cannot guarantee completeness: tree-sitter may miss callbacks, \
             dynamic dispatch, and macro-generated calls",
        );
    }

    // The one cheap staleness signal: a graph rebuilt for this very answer
    // is 0ms stale. Anything else is left unmeasured (None), never guessed.
    let staleness_ms = v
        .get("graph_build")
        .and_then(|b| b.get("build_ms"))
        .and_then(Value::as_u64)
        .map(|_| 0u64);

    // Epistemic confidence label: resolve places it at the top level;
    // targets and other ranked results place it inside `envelope`.
    let confidence = v
        .get("confidence")
        .and_then(Value::as_str)
        .or_else(|| v.pointer("/envelope/confidence").and_then(Value::as_str))
        .map(String::from);

    let epistemics = Epistemics {
        // extraction_limits is never empty, so closed_world is always false:
        // static analysis is never complete.
        closed_world: caps.is_empty() && extraction_limits.is_empty(),
        lower_bound: !caps.is_empty(),
        basis,
        staleness_ms,
        confidence,
        extraction_limits,
    };
    let warnings = caps
        .into_iter()
        .map(|message| Warning {
            code: "RESULT_CAPPED".to_string(),
            message,
        })
        .collect();
    (epistemics, warnings)
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Adapter wiring pixel-rank's Engine-3 reranker into pixel-graph's pluggable
/// `Reranker` trait. pixel-graph cannot depend on pixel-rank (circular), so
/// the daemon adapts `pixel_rank::rerank::rerank` into the trait here.
///
/// v1 = activity-only signals: the bundle is passed through as-is, and only
/// the activity map of it is populated (`engine_signals` fills it from the
/// git log; the session and error-sink channels are not wired).
#[derive(Clone)]
struct EngineReranker {
    /// Whether the resolve phrase mentions tests/specs — gates the per-path
    /// test penalty (a test file is demoted only when the task is NOT about
    /// tests).
    mentions_tests: bool,
    /// The test-penalty multiplier (0.7).
    test_penalty: f64,
}

impl EngineReranker {
    fn new(task: &str) -> Self {
        EngineReranker {
            mentions_tests: task.split(|c: char| !c.is_ascii_alphanumeric()).any(|t| {
                matches!(
                    t.to_ascii_lowercase().as_str(),
                    "test" | "tests" | "spec" | "specs"
                )
            }),
            test_penalty: 0.7,
        }
    }
}

impl pixel_graph::concept_resolve::Reranker for EngineReranker {
    fn rerank(
        &self,
        candidates: Vec<pixel_graph::concept_resolve::RankedCandidate>,
        signals: &pixel_graph::concept_resolve::SignalBundle,
    ) -> Vec<pixel_graph::concept_resolve::RankedCandidate> {
        use pixel_rank::rerank::RankedCandidate as PrCandidate;

        let pr_candidates: Vec<PrCandidate> = candidates
            .iter()
            .map(|c| PrCandidate {
                id: c.id,
                path: c.path.clone(),
                rrf_score: c.rrf_score,
                tier: c.tier.clone(),
            })
            .collect();
        let pr_signals = pixel_rank::signals::SignalBundle {
            activity: signals.activity.clone(),
            session: signals.session.clone(),
            fan_in: std::collections::HashMap::new(),
            session_reasons: signals.session_reasons.clone(),
            error_reasons: signals.error_reasons.clone(),
            // `op_resolve` mirrors the activity maps out of the bundle
            // `engine_signals` returned; pixel-graph's own bundle has no field
            // for the git-log scan's availability, so it is not carried here.
            activity_unavailable: None,
        };
        // Per-candidate test penalty: demote a test/spec file only when the
        // phrase itself is NOT about tests (per-path, via `is_test_path`).
        let penalty = |path: &str| -> f64 {
            if pixel_rank::signals::is_test_path(path) && !self.mentions_tests {
                self.test_penalty
            } else {
                1.0
            }
        };
        let reordered = pixel_rank::rerank::rerank(
            pr_candidates,
            &pr_signals,
            &pixel_rank::rerank::RerankWeights::from(&pixel_rank::signals::SignalOptions::default()),
            penalty,
        );

        // Restore the pixel-graph candidate shape (incl. `id`) by id — the
        // reranker only reorders, it never adds/removes candidates. Keying by
        // id (not path) keeps same-file concepts distinct (concept_resolve.rs
        // requires the adapter to preserve `id` through the round-trip).
        let by_id: HashMap<u64, &pixel_graph::concept_resolve::RankedCandidate> =
            candidates.iter().map(|c| (c.id, c)).collect();
        reordered
            .into_iter()
            .map(|c| {
                let orig = by_id[&c.id];
                pixel_graph::concept_resolve::RankedCandidate {
                    id: orig.id,
                    path: orig.path.clone(),
                    rrf_score: c.rrf_score,
                    tier: orig.tier.clone(),
                }
            })
            .collect()
    }

    fn clone_box(&self) -> Box<dyn pixel_graph::concept_resolve::Reranker> {
        Box::new(self.clone())
    }
}

/// The `evaluate` op's arguments as they arrive on the wire, before the
/// strings are parsed into the types the evaluation runs on.
pub(crate) struct EvaluateRequest {
    pub from: String,
    pub to: String,
    pub traversal: Option<String>,
    pub tiers: Option<String>,
    pub max_depth: Option<u32>,
    pub time_budget_ms: Option<u64>,
    pub scope: Option<String>,
    pub at_snapshot: bool,
}

/// Default traversal depth, matching `call-path`'s long-standing cap: deep
/// enough for the call chains people ask about, shallow enough that an
/// answer arrives.
pub(crate) const DEFAULT_EVALUATE_MAX_DEPTH: u32 = 8;
/// Default wall-clock budget for the traversal itself, excluding the
/// whole-tree checks that bracket it.
pub(crate) const DEFAULT_EVALUATE_TIME_BUDGET_MS: u64 = 250;

impl EvaluateRequest {
    /// Parse the wire strings. An unknown `traversal` or `tiers` is a usage
    /// error, never a silent fallback to a different relation: answering a
    /// question the caller did not ask is the failure this whole command
    /// exists to avoid.
    fn parse(self) -> Result<evaluate::Args, String> {
        let traversal = match self.traversal.as_deref() {
            None | Some("callees") => wire::Traversal::Callees,
            Some("callers") => wire::Traversal::Callers,
            Some(other) => {
                return Err(format!(
                    "evaluate: unknown --traversal {other:?} (callees | callers)"
                ));
            }
        };
        let tiers = match self.tiers.as_deref() {
            None => evaluate::TierSelection::Exact,
            Some(value) => evaluate::TierSelection::parse(value).ok_or_else(|| {
                format!("evaluate: unknown --tiers {value:?} (exact | exact,probable)")
            })?,
        };
        Ok(evaluate::Args {
            from: self.from,
            to: self.to,
            traversal,
            tiers,
            max_depth: self.max_depth.unwrap_or(DEFAULT_EVALUATE_MAX_DEPTH),
            time_budget_ms: self
                .time_budget_ms
                .unwrap_or(DEFAULT_EVALUATE_TIME_BUDGET_MS),
            scope: self.scope,
            at_snapshot: self.at_snapshot,
        })
    }
}

/// The caps an evaluation hit, in the words `derive_epistemics` turns into
/// `lower_bound`. A traversal that stopped early is a bounded answer and
/// the envelope must say so twice: once in `coverage`, once here.
fn evaluate_caps(evaluation: &pixel_graph::predicate::Evaluation) -> Vec<String> {
    let mut caps = Vec::new();
    if evaluation.coverage.depth_cap_dropped_frontier {
        let depth = evaluation.coverage.depth_cap;
        caps.push(format!(
            "traversal depth cap {depth} dropped a frontier node; paths beyond it were never walked"
        ));
    }
    if evaluation.coverage.time_budget_hit {
        let ms = evaluation.coverage.time_budget_ms;
        caps.push(format!(
            "traversal time budget {ms}ms expired with nodes still queued"
        ));
    }
    caps
}

enum Resolved {
    One(SymbolRow),
    Many(Vec<SymbolRow>),
}

/// `uid_or_name` protocol: '#' means uid; otherwise a name, with the
/// disambiguation protocol (`{candidates: [...], hint}`) on ambiguity.
fn resolve_symbol(store: &GraphStore, uid_or_name: &str) -> Result<Resolved, String> {
    if uid_or_name.contains('#') {
        return store
            .symbol_by_uid(uid_or_name)
            .map_err(|e| e.to_string())?
            .map(Resolved::One)
            .ok_or_else(|| format!("no symbol with uid {uid_or_name:?}"));
    }
    let syms = store
        .symbols_by_name(uid_or_name, None, 50)
        .map_err(|e| e.to_string())?;
    match syms.len() {
        0 => Err(format!("no symbol named {uid_or_name:?}")),
        1 => Ok(Resolved::One(syms.into_iter().next().unwrap())),
        _ => Ok(Resolved::Many(syms)),
    }
}

fn candidates_value(store: &GraphStore, syms: &[SymbolRow]) -> Result<Value, String> {
    let files = file_map(store)?;
    Ok(json!({
        "candidates": syms.iter().map(|s| symbol_json(s, &files)).collect::<Vec<_>>(),
        "hint": "ambiguous name; re-call with uid",
    }))
}

fn file_map(store: &GraphStore) -> Result<HashMap<i64, String>, String> {
    Ok(store
        .files()
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|f| (f.id, f.path))
        .collect())
}

/// Normalize a user-supplied file path to repo-relative form (strip root,
/// forward slashes, no leading slash) — matching how `build.rs::rel_path`
/// stores paths in the `files` table.
fn normalize_file_arg(root: &Path, file: &str) -> String {
    let p = Path::new(file);
    match p.strip_prefix(root) {
        Ok(rel) => rel.to_string_lossy().replace('\\', "/"),
        Err(_) => {
            // Not rooted — treat as already relative.
            file.replace('\\', "/").trim_start_matches('/').to_string()
        }
    }
}

fn symbol_json(s: &SymbolRow, files: &HashMap<i64, String>) -> Value {
    json!({
        "uid": s.uid,
        "name": s.name,
        "qualified": s.qualified,
        "kind": s.kind.as_str(),
        "path": files.get(&s.file_id).cloned().unwrap_or_default(),
        "start_line": s.start_line,
        "end_line": s.end_line,
        "sig": s.sig,
    })
}

/// Public-API `GraphStore` has uid/name lookups only; edge rows carry raw
/// ids, so resolve them through the sanctioned `conn()` escape hatch.
fn symbol_by_id(store: &GraphStore, id: i64) -> Option<SymbolRow> {
    store
        .conn()
        .query_row(
            "SELECT id, uid, file_id, name, qualified, kind, start_line, end_line, sig
             FROM symbols WHERE id = ?1",
            rusqlite::params![id],
            |r| {
                Ok(SymbolRow {
                    id: r.get(0)?,
                    uid: r.get(1)?,
                    file_id: r.get(2)?,
                    name: r.get(3)?,
                    qualified: r.get(4)?,
                    kind: SymbolKind::parse(&r.get::<_, String>(5)?),
                    start_line: r.get(6)?,
                    end_line: r.get(7)?,
                    sig: r.get(8)?,
                })
            },
        )
        .ok()
}

#[expect(
    dead_code,
    reason = "retained for the full-detail graph response shape while context uses compact edges"
)]
#[cfg_attr(test, mutants::skip)] // no caller: a mutation here is unobservable by design
fn edges_by_kind(
    store: &GraphStore,
    edges: &[EdgeRow],
    files: &HashMap<i64, String>,
    other_is_dst: bool,
) -> Result<Value, String> {
    let mut grouped: BTreeMap<&'static str, Vec<Value>> = BTreeMap::new();
    for e in edges {
        let other_id = if other_is_dst { e.dst_id } else { e.src_id };
        let other = symbol_by_id(store, other_id);
        grouped.entry(e.kind.as_str()).or_default().push(json!({
            "symbol": other.as_ref().map(|s| symbol_json(s, files)),
            "tier": e.tier.as_str(),
            "site_line": e.site_line,
        }));
    }
    Ok(serde_json::to_value(grouped).unwrap_or(Value::Null))
}

/// Compact edge representation for budgeted responses: just name, path, and
/// tier per edge, grouped by kind. Preserves relationship metadata without
/// the full symbol JSON (no sig, no uid, no line range).
fn compact_edges(
    store: &GraphStore,
    edges: &[EdgeRow],
    files: &HashMap<i64, String>,
    other_is_dst: bool,
    limit: usize,
) -> Result<Value, String> {
    let mut grouped: BTreeMap<&'static str, Vec<Value>> = BTreeMap::new();
    let mut seen = HashSet::new();
    for e in edges {
        if seen.len() >= limit {
            break;
        }
        let other_id = if other_is_dst { e.dst_id } else { e.src_id };
        let other = symbol_by_id(store, other_id);
        let entry = if let Some(s) = &other {
            let path = files.get(&s.file_id).cloned().unwrap_or_default();
            if !seen.insert((
                e.kind.as_str(),
                s.name.clone(),
                path.clone(),
                e.tier.as_str(),
            )) {
                continue;
            }
            json!({
                "name": s.name,
                "path": path,
                "tier": e.tier.as_str(),
            })
        } else {
            if !seen.insert((
                e.kind.as_str(),
                String::new(),
                String::new(),
                e.tier.as_str(),
            )) {
                continue;
            }
            json!({ "tier": e.tier.as_str() })
        };
        grouped.entry(e.kind.as_str()).or_default().push(entry);
    }
    Ok(serde_json::to_value(grouped).unwrap_or(Value::Null))
}

/// One bounded read and stored-hash comparison per distinct returned file.
/// Failed/unavailable files are cached too, so neighbors never retry a bad read.
fn validated_context_source<'a>(
    root: &Path,
    store: &GraphStore,
    file_id: i64,
    files: &HashMap<i64, String>,
    sources: &'a mut HashMap<i64, Option<String>>,
) -> Option<&'a str> {
    sources
        .entry(file_id)
        .or_insert_with(|| {
            let path = files.get(&file_id)?;
            let row = store.file_by_path(path).ok()??;
            let file = open_regular_bounded(&root.join(path), MAX_FILE_BYTES).ok()?;
            let mut bytes = Vec::new();
            file.take(MAX_FILE_BYTES.saturating_add(1))
                .read_to_end(&mut bytes)
                .ok()?;
            if bytes.len() as u64 > MAX_FILE_BYTES
                || format!("{:016x}", xxhash_rust::xxh3::xxh3_64(&bytes)) != row.blob_oid
            {
                return None;
            }
            String::from_utf8(bytes).ok()
        })
        .as_deref()
}

fn context_item(
    source: &str,
    s: &SymbolRow,
    files: &HashMap<i64, String>,
    max_snippet_bytes: usize,
    crux: &[(u32, String)],
) -> bridge::Item {
    let path = files.get(&s.file_id).cloned().unwrap_or_default();
    let snippet = read_snippet(source, s.start_line, s.end_line, 60, max_snippet_bytes);
    bridge::Item {
        name: s.name.clone(),
        kind: s.kind.as_str().to_string(),
        path,
        start_line: s.start_line,
        end_line: s.end_line,
        sig: s.sig.clone(),
        snippet,
        // Storage keeps body-relative lines for stable fingerprints. Rendering
        // uses file coordinates, just like the symbol header and source excerpt.
        // Reject invalid/overflowing offsets rather than inventing a location.
        crux: crux
            .iter()
            .filter_map(|(relative, text)| {
                let offset = relative.checked_sub(1)?;
                let line = s.start_line.checked_add(offset)?;
                (s.start_line > 0 && line <= s.end_line).then(|| (line, text.clone()))
            })
            .collect(),
    }
}

#[cfg(test)]
mod context_crux_coordinate_tests {
    use super::*;

    fn symbol(start_line: u32, end_line: u32) -> SymbolRow {
        SymbolRow {
            id: 1,
            uid: "sample.rs#check#function".to_owned(),
            file_id: 1,
            name: "check".to_owned(),
            qualified: "check".to_owned(),
            kind: pixel_graph::SymbolKind::Function,
            start_line,
            end_line,
            sig: "pub fn check(flag: bool) -> i32 {".to_owned(),
        }
    }

    #[test]
    fn context_crux_coordinates_match_real_source_after_shift() {
        let dir = std::env::temp_dir().join(format!(
            "pixel-crux-coordinates-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let body = "pub fn check(flag: bool) -> i32 {\n    if flag {\n        return 1;\n    }\n    0\n}\n";
        let crux = vec![(2, "if flag {".to_owned()), (3, "return 1;".to_owned())];
        let files = HashMap::from([(1, "sample.rs".to_owned())]);
        for padding in [10, 20] {
            let source = format!("{}{body}", "// padding\n".repeat(padding));
            std::fs::write(dir.join("sample.rs"), &source).unwrap();
            let item = context_item(
                &source,
                &symbol(padding as u32 + 1, padding as u32 + 6),
                &files,
                4096,
                &crux,
            );
            for (line, text) in &item.crux {
                assert_eq!(source.lines().nth(*line as usize - 1).unwrap().trim(), text);
            }
            let (rendered, _, _) = bridge::render_context(&[item], 2000);
            assert!(rendered.contains(&format!("crux:{} if flag {{", padding + 2)));
            assert!(rendered.contains(&format!("crux:{} return 1;", padding + 3)));
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn context_crux_rejects_zero_out_of_span_and_overflow_coordinates() {
        let files = HashMap::new();
        let crux = vec![
            (0, "invalid".to_owned()),
            (2, "valid".to_owned()),
            (9, "outside".to_owned()),
        ];
        let item = context_item("", &symbol(11, 16), &files, 0, &crux);
        assert_eq!(item.crux, vec![(12, "valid".to_owned())]);
        assert!(
            context_item("", &symbol(0, 6), &files, 0, &crux)
                .crux
                .is_empty()
        );
        assert!(
            context_item("", &symbol(u32::MAX, u32::MAX), &files, 0, &crux)
                .crux
                .is_empty()
        );
    }

    #[test]
    fn context_warm_graph_omits_stale_source_and_crux_with_truthful_warning() {
        let root = std::env::temp_dir().join(format!(
            "pixel-crux-freshness-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q"])
                .current_dir(&root)
                .status()
                .unwrap()
                .success()
        );
        let body = "pub fn check(flag: bool) -> i32 {\n    if flag {\n        return 1;\n    }\n    0\n}\n";
        let original = format!("{}{body}", "// padding\n".repeat(10));
        std::fs::write(root.join("sample.rs"), &original).unwrap();
        let request = || Request::Context {
            uid: "sample.rs#check#function".to_owned(),
            budget_tokens: Some(2000),
        };
        let mut service = Service::open(&root).unwrap();
        let before = service.handle(request());
        assert!(before.ok, "{before:?}");
        assert!(
            before.data()["text"]
                .as_str()
                .unwrap()
                .contains("crux:13 return 1;")
        );

        std::fs::write(
            root.join("sample.rs"),
            format!(
                "{}{}",
                "// shifted\n".repeat(10),
                original.replace("return 1;", "return 2;")
            ),
        )
        .unwrap();
        let stale = service.handle(request());
        assert!(stale.ok, "{stale:?}");
        assert_eq!(stale.data()["text"], "");
        assert!(stale.data().get("symbol").is_none());
        let wire = serde_json::to_value(&stale).unwrap();
        assert_eq!(wire["epistemics"]["lower_bound"], true);
        assert_eq!(wire["epistemics"]["closed_world"], false);
        assert!(wire["warnings"].as_array().unwrap().iter().any(|warning| {
            warning["message"]
                .as_str()
                .unwrap()
                .contains("source differs")
        }));

        // A fresh graph snapshot recovers normal excerpts and absolute lines.
        let mut refreshed = Service::open(&root).unwrap();
        let after = refreshed.handle(request());
        assert!(after.ok, "{after:?}");
        assert_eq!(after.data()["symbol"]["start_line"], 21);
        assert!(
            after.data()["text"]
                .as_str()
                .unwrap()
                .contains("crux:23 return 2;")
        );
        assert!(!after.data()["text"].as_str().unwrap().contains("return 1;"));
        std::fs::remove_dir_all(root).unwrap();
    }
}

fn read_snippet(
    source: &str,
    start_line: u32,
    end_line: u32,
    max_lines: usize,
    max_bytes: usize,
) -> String {
    if max_bytes == 0 {
        return String::new();
    }
    let start = start_line.saturating_sub(1) as usize;
    let mut snippet = String::new();
    for line in source.lines().skip(start).take(
        ((end_line as usize).saturating_sub(start))
            .min(max_lines)
            .max(1),
    ) {
        let separator = usize::from(!snippet.is_empty());
        let remaining = max_bytes.saturating_sub(snippet.len() + separator);
        if remaining == 0 {
            break;
        }
        if separator == 1 {
            snippet.push('\n');
        }
        if line.len() <= remaining {
            snippet.push_str(line);
            continue;
        }
        let mut end = remaining;
        while end > 0 && !line.is_char_boundary(end) {
            end -= 1;
        }
        snippet.push_str(&line[..end]);
        break;
    }
    snippet
}

/// Default share of indexed files above which a drifted graph is rebuilt
/// from scratch instead of updated file by file. Re-extracting one file is
/// cheap; re-resolving calls after a large drift is not much cheaper than a
/// fresh parallel build, and a rebuild is the path with no state to trust.
pub const DEFAULT_GRAPH_INCREMENTAL_MAX_PCT: u64 = 20;

/// `PIXEL_GRAPH_INCREMENTAL_MAX_PCT`: percentage of indexed files (0-100)
/// up to which drift is applied incrementally. `0` disables the incremental
/// path (always rebuild); `100` never rebuilds for drift alone. Unset,
/// empty, non-numeric or out-of-range values fall back to the default.
fn incremental_max_pct() -> u64 {
    std::env::var("PIXEL_GRAPH_INCREMENTAL_MAX_PCT")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|pct| *pct <= 100)
        .unwrap_or(DEFAULT_GRAPH_INCREMENTAL_MAX_PCT)
}

/// What [`Service::evaluate_gate`] must do with the delta it measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GateAction {
    /// The stored graph already describes the tree: answer from it.
    Ready,
    /// Drift small enough to apply before answering.
    Incremental,
    /// Too much drift, or no signature to compare against: refuse. A full
    /// rebuild can take minutes and is never a side effect of a question.
    Stale,
}

/// The gate's decision, separated from the I/O around it so that each
/// branch is reachable from a test.
///
/// "Is the graph fresh" is not the same question as "is the drift small
/// enough to apply". A fresh graph must be answered from directly: routing
/// it through the incremental path would make a question rewrite the graph
/// it is asking about, and `apply_tree_delta` re-signs the store even when
/// it has no rows to change. A drifted graph must never be treated as
/// fresh, which would answer from rows the tree no longer matches.
fn gate_action(delta: Option<&pixel_graph::build::TreeDelta>, max_pct: u64) -> GateAction {
    match delta {
        Some(delta) if delta.fresh => GateAction::Ready,
        Some(delta) if incremental_allowed(delta.changed_count(), delta.indexed_files, max_pct) => {
            GateAction::Incremental
        }
        // Drift past the threshold, or no usable signature at all: built
        // before signatures existed, or written by an update that could
        // not sign what it committed.
        Some(_) | None => GateAction::Stale,
    }
}

/// Whether `changed` drifted files out of `indexed` may be applied
/// incrementally under a `pct` threshold. A graph that indexed nothing has
/// no incremental state to reuse; a threshold of 0 means "never".
fn incremental_allowed(changed: usize, indexed: usize, pct: u64) -> bool {
    if pct == 0 || indexed == 0 {
        return false;
    }
    (changed as u128) * 100 <= (indexed as u128) * (pct as u128)
}

fn merge_build_info(out: &mut Value, built: Option<Value>) {
    if let (Some(info), Some(obj)) = (built, out.as_object_mut()) {
        obj.insert("graph_build".into(), info);
    }
}

fn sqlite_sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

fn remove_sqlite_sidecars(path: &Path) -> Result<(), String> {
    for sidecar in [sqlite_sidecar(path, "-wal"), sqlite_sidecar(path, "-shm")] {
        match std::fs::remove_file(&sidecar) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("remove {}: {error}", sidecar.display())),
        }
    }
    Ok(())
}

fn remove_sqlite_files(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("remove {}: {error}", path.display())),
    }
    remove_sqlite_sidecars(path)
}

// ---------------------------------------------------------------------------
// bridge — the ONLY place that calls concurrently-developed crate APIs.
// Each fn is one call deep so contract drift is a one-line fix.
// ---------------------------------------------------------------------------

mod bridge {
    use super::{Value, es, to_val};
    use pixel_graph::GraphStore;
    use std::path::Path;

    /// Neutral mirror of `pixel_context::ContextItem`.
    pub struct Item {
        pub name: String,
        pub kind: String,
        pub path: String,
        pub start_line: u32,
        pub end_line: u32,
        pub sig: String,
        pub snippet: String,
        /// Content-anchored crux lines, each `(line_number, trimmed_text)`
        /// (P2·3). Empty when none were extracted.
        pub crux: Vec<(u32, String)>,
    }

    pub fn build_graph(root: &Path, db: &Path) -> Result<Value, String> {
        let s = pixel_graph::build::build_graph(root, db).map_err(es)?;
        Ok(serde_json::json!({
            "files": s.files,
            "symbols": s.symbols,
            "edges": s.edges,
            "unresolved": s.unresolved,
            "elapsed_ms": s.elapsed_ms as u64,
        }))
    }

    /// One walk of `root`: freshness verdict plus the files that drifted
    /// from the on-disk graph. `None` when the db has no signature to trust.
    pub fn tree_delta(
        root: &Path,
        db: &Path,
    ) -> Result<Option<pixel_graph::build::TreeDelta>, String> {
        pixel_graph::build::tree_delta(root, db).map_err(es)
    }

    /// Re-extract the drifted files only and publish the delta's signature.
    pub fn apply_tree_delta(
        root: &Path,
        db: &Path,
        delta: &pixel_graph::build::TreeDelta,
    ) -> Result<(), String> {
        pixel_graph::build::apply_tree_delta(root, db, delta).map_err(es)
    }

    /// The same for a debounced batch of watcher events.
    pub fn update_files(root: &Path, db: &Path, files: &[(&str, bool)]) -> Result<(), String> {
        pixel_graph::build::update_files(root, db, files)
            .map(|_| ())
            .map_err(es)
    }

    pub fn impact(
        store: &GraphStore,
        uid: &str,
        direction: &str,
        depth: u32,
    ) -> Result<Value, String> {
        use pixel_graph::impact::{Direction, impact};
        let dir = if direction == "downstream" {
            Direction::Downstream
        } else {
            Direction::Upstream
        };
        // limit_per_depth=20: 3 depths × 20 = 60 max items in the report.
        // Each ImpactItem carries uid/name/path/tier/processes — at ~300
        // bytes each, 60 items ≈ 18KB. The previous 50-per-depth (150
        // total) could hit ~75KB+ with process lists, which is more than
        // an agent needs from an impact scan. The counts are always exact
        // (they count ALL edges, not just the listed ones); only the
        // detailed item list is capped.
        impact(store, uid, dir, depth, 20).map(to_val).map_err(es)
    }

    pub fn trace(store: &GraphStore, from_uid: &str, to_uid: &str) -> Result<Value, String> {
        pixel_graph::trace::trace(store, from_uid, to_uid, 8)
            .map(to_val)
            .map_err(es)
    }

    pub fn processes(store: &mut GraphStore, offset: usize) -> Result<Value, String> {
        use pixel_graph::process;
        const PROCESS_LIMIT: usize = 5;
        const STEP_LIMIT: usize = 10;
        let (listed, persisted_total) =
            process::list(store, PROCESS_LIMIT, STEP_LIMIT, offset).map_err(es)?;
        let (mut v, total_processes) = if persisted_total == 0 {
            const DISCOVERY_LIMIT: usize = 100;
            let discovered = process::discover(store, 6, 3, 3, DISCOVERY_LIMIT).map_err(es)?;
            let total = discovered.len();
            (
                discovered
                    .into_iter()
                    .skip(offset)
                    .take(PROCESS_LIMIT)
                    .collect(),
                total,
            )
        } else {
            (listed, persisted_total)
        };
        v.truncate(PROCESS_LIMIT);
        let mut steps_truncated = v
            .iter()
            .any(|summary| summary.step_count as usize > STEP_LIMIT);
        for summary in &mut v {
            if summary.steps.len() > STEP_LIMIT {
                summary.steps.truncate(STEP_LIMIT);
                steps_truncated = true;
            }
        }
        let returned_processes = v.len();
        let has_more = offset.saturating_add(returned_processes) < total_processes;
        Ok(serde_json::json!({
            "list-flows": to_val(v),
            "total_processes": total_processes,
            "returned_processes": returned_processes,
            "process_limit": PROCESS_LIMIT,
            "step_limit": STEP_LIMIT,
            "offset": offset,
            "next_offset": has_more.then_some(offset.saturating_add(returned_processes)),
            "truncated": has_more || steps_truncated,
        }))
    }

    pub fn clusters(store: &mut GraphStore, offset: usize) -> Result<Value, String> {
        use pixel_graph::cluster;
        const CLUSTER_LIMIT: usize = 50;
        let (listed, persisted_total) = cluster::list(store, CLUSTER_LIMIT, offset).map_err(es)?;
        let (mut v, total_clusters) = if persisted_total == 0 {
            let computed = cluster::compute(store).map_err(es)?;
            let total = computed.len();
            (
                computed
                    .into_iter()
                    .skip(offset)
                    .take(CLUSTER_LIMIT)
                    .collect(),
                total,
            )
        } else {
            (listed, persisted_total)
        };
        v.truncate(CLUSTER_LIMIT);
        let returned_clusters = v.len();
        let has_more = offset.saturating_add(returned_clusters) < total_clusters;
        Ok(serde_json::json!({
            "list-areas": to_val(v),
            "total_clusters": total_clusters,
            "returned_clusters": returned_clusters,
            "cluster_limit": CLUSTER_LIMIT,
            "offset": offset,
            "next_offset": has_more.then_some(offset.saturating_add(returned_clusters)),
            "truncated": has_more,
        }))
    }

    pub fn changes(
        store: &GraphStore,
        root: &Path,
        base: Option<&str>,
        include_tests: bool,
    ) -> Result<Value, String> {
        pixel_graph::changes::detect(store, root, base, include_tests)
            .map(to_val)
            .map_err(es)
    }

    /// Budget-fitted text rendering via pixel-context; empty on any miss.
    pub fn render_context(items: &[Item], budget_tokens: usize) -> (String, &'static str, usize) {
        use pixel_context::{ContextItem, Layer, fit_to_budget_detailed, render};
        let mapped: Vec<ContextItem> = items
            .iter()
            .map(|i| ContextItem {
                name: i.name.clone(),
                kind: i.kind.clone(),
                path: i.path.clone(),
                start_line: i.start_line,
                end_line: i.end_line,
                sig: i.sig.clone(),
                snippet: i.snippet.clone(),
                crux: i.crux.clone(),
            })
            .collect();
        if let Some((target, neighbors)) = mapped.split_first() {
            let target_text = render(std::slice::from_ref(target), Layer::L2);
            let target_tokens = pixel_context::estimate_tokens(&target_text);
            if target_tokens <= budget_tokens {
                let neighbors_fit =
                    fit_to_budget_detailed(neighbors, budget_tokens - target_tokens, Layer::L1);
                let layer = if neighbors_fit.text.is_empty() {
                    "L2"
                } else if neighbors_fit.layer == Layer::L1 {
                    "L2+L1"
                } else {
                    "L2+L0"
                };
                return (
                    format!("{target_text}{}", neighbors_fit.text),
                    layer,
                    neighbors_fit.elided_items,
                );
            }
            // P2·3 distilled-body fallback: when the target's L2 body exceeds
            // the budget, its crux lines (guards, mutations, early bails —
            // content-anchored, so still right when the file moved) carry the
            // logic at a fraction of the span's cost. Try sig + crux before
            // degrading to signatures-only.
            if !target.crux.is_empty() {
                let mut distilled = render(std::slice::from_ref(target), Layer::L1);
                let crux: Vec<(u32, &str)> = target
                    .crux
                    .iter()
                    .map(|(line, text)| (*line, text.as_str()))
                    .collect();
                pixel_context::render_crux(&mut distilled, &crux);
                let distilled_tokens = pixel_context::estimate_tokens(&distilled);
                if distilled_tokens <= budget_tokens {
                    let neighbors_fit = fit_to_budget_detailed(
                        neighbors,
                        budget_tokens - distilled_tokens,
                        Layer::L1,
                    );
                    let layer = if neighbors_fit.text.is_empty() {
                        "L1+crux"
                    } else if neighbors_fit.layer == Layer::L1 {
                        "L1+crux+L1"
                    } else {
                        "L1+crux+L0"
                    };
                    return (
                        format!("{distilled}{}", neighbors_fit.text),
                        layer,
                        neighbors_fit.elided_items,
                    );
                }
            }
        }
        let fitted = fit_to_budget_detailed(&mapped, budget_tokens, Layer::L2);
        (fitted.text, fitted.layer.as_str(), fitted.elided_items)
    }
}

fn to_val<T: Serialize>(t: T) -> Value {
    serde_json::to_value(t).unwrap_or(Value::Null)
}

// ---------------------------------------------------------------------------
// ranked search — `--scope code` reranking via pixel-rank's RRF
// ---------------------------------------------------------------------------

/// Rerank search matches by file-level signals WITHOUT changing the hit set.
///
/// The hit set (set of (path, line) pairs from the index) is preserved
/// exactly — only the order changes. Per PLAN.md's M1 gate: "identical hit
/// sets; order may differ deliberately due to ranking."
///
/// Signals (same RRF family as `targets`, K=60):
/// - **Filename**: a word of the search pattern appears in the file's
///   basename (per-word via `split_ident_words`, so "gain ledger" matches
///   `ledger.ts`). Weight 3.0 (matches `targets`'s filename signal).
/// - **Symbol**: the search pattern matches a symbol name in that file
///   (via the graph, if available). Weight 2.5.
/// - **Content density**: files with more matches rank higher. Weight 1.5.
/// - **Graph adjacency** (S4): matched files graph-adjacent to sibling
///   symbol-match files rank higher (only reorders existing matches). 1.0.
/// - **Cluster co-membership** (S5): matched files sharing a functional
///   cluster with a symbol-match file rank higher (reorders only). 0.5.
///
/// Files are ranked by fused RRF score; within a file, matches keep their
/// original line-number order (stable, deterministic). Graph failure
/// degrades to filename + content density only (same graceful-degradation
/// pattern as `op_targets`).
/// Tokenize free text into comparable words for the BM25 content channel.
///
/// `split_ident_words` is an IDENTIFIER splitter: it breaks on `_ - . : #`
/// and camelCase humps only. Applied to a raw regex pattern or a line of
/// source it yields one giant token — `"index|disambiguation"` stays whole,
/// and `"let index = compute(index);"` becomes a single word — so every term
/// frequency comes out zero. BM25 needs real text tokens, so split on any
/// non-alphanumeric boundary FIRST, then hand each run to the identifier
/// splitter so `snake_case` and `camelCase` still separate.
///
/// Used for both BM25 query terms and matched-line text.
fn tokenize_words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|run| !run.is_empty())
        .flat_map(pixel_graph::split_ident_words)
        .map(|w| w.to_lowercase())
        .filter(|w| !w.is_empty())
        .collect()
}

/// Split conversational words and alternatives without inventing identifier
/// terms from regex escape letters (for example the `b` in `\bimports\b`).
/// Complex regex syntax stays attached, as with the original identifier splitter.
fn search_signal_words(pattern: &str) -> Vec<String> {
    pattern
        .split(|c: char| c.is_whitespace() || c == '|')
        .flat_map(pixel_graph::split_ident_words)
        .map(|word| word.to_lowercase())
        .filter(|word| !word.is_empty())
        .collect()
}

fn rank_search_matches(
    matches: &[pixel_index::verify::MatchLine],
    pattern: &str,
    graph: &Option<GraphStore>,
    semantic_rank: Option<&[String]>,
) -> Vec<pixel_index::verify::MatchLine> {
    use std::collections::BTreeMap;

    // Group matches by file, preserving within-file line order.
    let mut by_file: BTreeMap<String, Vec<pixel_index::verify::MatchLine>> = BTreeMap::new();
    for m in matches {
        by_file.entry(m.path.clone()).or_default().push(m.clone());
    }
    let files: Vec<String> = by_file.keys().cloned().collect();
    if files.is_empty() {
        return Vec::new();
    }

    // --- Signal 1: per-word filename match ---
    // Split the pattern into identifier words; a file whose basename contains
    // any of those words ranks by how many distinct words match. This fixes
    // S1: "gain ledger" now matches `ledger.ts` (the word "ledger" is a
    // basename component), whereas whole-pattern basename containment
    // (`basename.contains("gain ledger")`) matched nothing. Within a tier, a
    // shorter/more-specific basename outranks a longer one (Bug 4: previously
    // sorted by length DESCENDING, so the longest matching filename won).
    let words = search_signal_words(pattern);
    let mut filename_rank: Vec<(String, usize)> = files
        .iter()
        .filter_map(|f| {
            let basename = std::path::Path::new(f)
                .file_name()
                .map(|s| s.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            let matched = words
                .iter()
                .filter(|w| !w.is_empty() && basename.contains(w.as_str()))
                .count();
            if matched > 0 {
                Some((f.clone(), matched))
            } else {
                None
            }
        })
        .collect();
    filename_rank.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| {
                std::path::Path::new(&a.0)
                    .file_name()
                    .map_or(0, |s| s.to_string_lossy().len())
                    .cmp(
                        &std::path::Path::new(&b.0)
                            .file_name()
                            .map_or(0, |s| s.to_string_lossy().len()),
                    )
            })
            .then_with(|| a.0.cmp(&b.0))
    });

    // --- Signal 2: symbol match (graph, if available) ---
    let pat_lower = pattern.to_lowercase();
    let symbol_rank: Vec<String> = if let Some(store) = graph {
        // For each file, check if any symbol name contains the pattern.
        let mut hits: Vec<(String, usize)> = files
            .iter()
            .filter_map(|f| {
                let file = store.file_by_path(f).ok().flatten()?;
                let syms = store.symbols_in_file(file.id).ok()?;
                let count = syms
                    .iter()
                    .filter(|s| {
                        let name_lc = s.name.to_lowercase();
                        name_lc.contains(&pat_lower)
                    })
                    .count();
                if count > 0 {
                    Some((f.clone(), count))
                } else {
                    None
                }
            })
            .collect();
        hits.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        hits.into_iter().map(|(f, _)| f).collect()
    } else {
        Vec::new()
    };

    // --- Signal 4 & 5 (graph): graph-adjacency and cluster co-membership ---
    // Same RRF family as `targets`'s S4/S5, but restricted to the matched
    // set: search is a LITERAL contract, so graph evidence may only REORDER
    // files that already have content matches — never introduce new files.
    // Seeds are the matched files' symbol-name hits, so a file that both
    // contains a match and is graph-adjacent to a sibling match file (or
    // shares a functional cluster with it) ranks higher.
    let (graph_rank, cluster_rank): (Vec<String>, Vec<String>) = if let Some(store) = graph {
        use pixel_graph::targets as graph_targets_th;
        let matched: HashSet<&str> = files.iter().map(String::as_str).collect();
        let kw = search_signal_words(pattern);
        let matched_sym_ids: Vec<i64> = graph_targets_th::symbol_hits(store, &kw, &[])
            .ok()
            .into_iter()
            .flatten()
            .filter(|h| matched.contains(h.path.as_str()))
            .flat_map(|h| h.symbols.into_iter().map(|(s, _)| s.id))
            .collect();
        if matched_sym_ids.is_empty() {
            (Vec::new(), Vec::new())
        } else {
            let neighbor: Vec<String> = graph_targets_th::neighbor_files(store, &matched_sym_ids)
                .unwrap_or_default()
                .into_iter()
                .map(|(p, _)| p)
                .filter(|p| matched.contains(p.as_str()))
                .collect();
            let cluster: Vec<String> =
                graph_targets_th::cluster_co_files(store, &matched_sym_ids, &kw)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(p, _)| p)
                    .filter(|p| matched.contains(p.as_str()))
                    .collect();
            (neighbor, cluster)
        }
    } else {
        (Vec::new(), Vec::new())
    };

    // --- Signal 3: content relevance (BM25 over the matched pool) ---
    // Was: raw match count per file. Raw counts have none of BM25's three
    // properties — a long file matching a common term 200 times buried a
    // short file matching the rare term 3 times. BM25 adds IDF, TF
    // saturation and length normalization over the SAME matched pool, so
    // this still only reorders candidates that already matched.
    //
    // tf/len are measured over each file's MATCHED LINES (the evidence we
    // actually retrieved), not whole file contents — reading full files at
    // query time would break the latency contract. Tokenization reuses
    // `split_ident_words`, the same splitter applied to the query, so
    // camelCase/snake_case segments line up on both sides.
    let mut density_rank: Vec<(String, usize)> = files
        .iter()
        .map(|f| (f.clone(), by_file.get(f).map_or(0, Vec::len)))
        .collect();
    density_rank.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    let bm25_terms: Vec<String> = {
        let mut seen: HashSet<String> = HashSet::new();
        tokenize_words(pattern)
            .into_iter()
            .filter(|w| seen.insert(w.clone()))
            .collect()
    };
    let bm25_docs: Vec<pixel_rank::Bm25Doc> = files
        .iter()
        .map(|f| {
            let mut term_freqs = vec![0u32; bm25_terms.len()];
            let mut len: u32 = 0;
            for m in by_file.get(f).map_or(&[][..], Vec::as_slice) {
                for tok in tokenize_words(&m.line) {
                    len = len.saturating_add(1);
                    if let Some(j) = bm25_terms.iter().position(|t| *t == tok) {
                        term_freqs[j] = term_freqs[j].saturating_add(1);
                    }
                }
            }
            pixel_rank::Bm25Doc {
                path: f.clone(),
                term_freqs,
                len,
            }
        })
        .collect();
    // Fall back to raw density when BM25 has no usable signal — e.g. a regex
    // pattern with no identifier words, or one whose words never appear
    // literally in the matched lines. Never emit an arbitrary path order.
    let content_rank: Vec<String> = pixel_rank::bm25_rank(&bm25_terms, &bm25_docs)
        .unwrap_or_else(|| density_rank.iter().map(|(f, _)| f.clone()).collect());

    // --- RRF fusion (K=60, same as targets) ---
    // Phase 1c: use the shared `pixel_rank::rrf_fuse` primitive instead of a
    // third independent reimplementation of weighted RRF. The weights are
    // pixel-rank's own pub constants, so a drift here can no longer silently
    // desync search ranking from `targets` ranking.
    //
    // Precision 1: the semantic channel (S6) is fused only when the caller
    // passed a semantic ranking (`scope: "hybrid"` and the model was warm).
    // It uses `W_SEMANTIC` — stronger than content density, weaker than
    // filename/symbol. When absent, the fusion is identical to the 5-channel
    // path (no latency regression, no behavior change for `scope: "code"`).
    let s1: Vec<String> = filename_rank.iter().map(|(f, _)| f.clone()).collect();
    let s2: Vec<String> = symbol_rank;
    let s3: Vec<String> = content_rank;

    let mut lists: Vec<(&[String], f64)> = vec![
        (&s1, pixel_rank::W_FILENAME),
        (&s2, pixel_rank::W_SYMBOL),
        (&s3, pixel_rank::W_CONTENT),
        (&graph_rank, pixel_rank::W_GRAPH),
        (&cluster_rank, pixel_rank::W_CLUSTER),
    ];
    if let Some(sem) = semantic_rank {
        lists.push((sem, pixel_rank::W_SEMANTIC));
    }

    let mut file_order = pixel_rank::rrf_fuse(&lists, pixel_rank::RRF_K);

    // Preserve the full hit set: files with no signal still appear (score
    // 0.0), sorted by path — matching the pre-fusion behavior where every
    // file in the pool was emitted.
    let fused_set: HashSet<&str> = file_order.iter().map(|(p, _)| p.as_str()).collect();
    let mut rest: Vec<String> = files
        .iter()
        .filter(|f| !fused_set.contains(f.as_str()))
        .cloned()
        .collect();
    rest.sort();
    for f in rest {
        file_order.push((f, 0.0));
    }

    // Emit matches in file order, preserving within-file line order.
    let mut out: Vec<pixel_index::verify::MatchLine> = Vec::with_capacity(matches.len());
    for (f, _) in file_order {
        if let Some(file_matches) = by_file.remove(&f) {
            out.extend(file_matches);
        }
    }
    out
}

fn es<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

/// Cosine similarity between two embedding vectors (defensive normalize).
fn cosine_sim(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    let denom = (na * nb).sqrt();
    if denom > 0.0 { dot / denom } else { 0.0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pixel_graph::Tier;
    use pixel_graph::concept_resolve::{RankedCandidate, Reranker, SignalBundle};
    use std::path::PathBuf;

    #[test]
    fn read_plane_shares_index_and_observes_published_refresh() {
        let root = tmpdir("read-plane-shared");
        std::fs::write(root.join("app.rs"), "fn initial_name() {}\n").unwrap();
        let mut writer = Service::open(&root).unwrap();
        let mut first = writer.read_replica();
        let mut second = writer.read_replica();
        assert!(Arc::ptr_eq(&writer.index, &first.index));
        assert!(Arc::ptr_eq(&first.index, &second.index));
        let (before, result) = first.read_evidence("search", "initial_name", 8);
        assert!(result.unwrap().to_string().contains("initial_name"));
        std::fs::write(root.join("app.rs"), "fn updated_name() {}\n").unwrap();
        writer.refresh_file("app.rs");
        let (after, result) = second.read_evidence("search", "updated_name", 8);
        assert!(after.generation > before.generation);
        assert!(result.unwrap().to_string().contains("updated_name"));
        writer.publication.write().unwrap().healthy = false;
        assert!(first.read_evidence("search", "updated_name", 8).1.is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "pixel-daemon-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
                % 1_000_000
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
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

    /// `Service::open` must stay cold: the history index is demand-driven,
    /// so opening the daemon service cannot create `.pixel/history.db`.
    #[test]
    fn service_open_does_not_create_the_history_index() {
        let root = tmpdir("lazy-facts-open");
        git(&root, &["init", "-q"]);
        std::fs::write(root.join("a.rs"), "fn a() {}\n").unwrap();
        let service = Service::open(&root).unwrap();
        drop(service);
        assert!(!root.join(".pixel/history.db").exists());
    }

    /// The facts warm loop is claimed exactly once: the first
    /// facts-consuming request spawns it, later ones must not.
    #[test]
    fn facts_warmer_is_claimed_once() {
        let root = tmpdir("lazy-facts-warmer");
        git(&root, &["init", "-q"]);
        let service = Service::open(&root).unwrap();
        assert!(service.facts_warmer_needed());
        assert!(!service.facts_warmer_needed());
        assert!(!service.facts_warmer_needed());
    }

    /// The codes an agent acts on (`BUSY_REPOSITORY` → retry later,
    /// `NON_FAST_FORWARD` → fetch and reconcile, `NOT_FOUND` → widen the
    /// query) must be the ones the message actually names. Each entry below
    /// is a message a real call site writes; the two unclassified markers
    /// pin that a prefix naming no `ErrorCode` is not force-fitted to one.
    #[test]
    fn classify_error_reads_the_code_the_message_names() {
        let cases: &[(&str, ErrorCode)] = &[
            // pixel-ops prefixes the code it means.
            (
                "NON_FAST_FORWARD: merge-base is aaa, expected bbb to be an ancestor of ccc",
                ErrorCode::NonFastForward,
            ),
            (
                "STALE_STATE: expected head aaa, got Some(\"bbb\")",
                ErrorCode::StaleState,
            ),
            (
                "UNSUPPORTED_STATE: detached HEAD; --into requires a checked-out feature branch",
                ErrorCode::UnsupportedState,
            ),
            (
                "REF_EXISTS: branch 'fix/x' already exists",
                ErrorCode::RefExists,
            ),
            (
                "GIT_FAILED: crash detected at index_staged; cannot safely determine whether the commit ran to completion",
                ErrorCode::GitFailed,
            ),
            (
                "NETWORK_AMBIGUITY: push may have started, cannot safely retry",
                ErrorCode::NetworkAmbiguity,
            ),
            // The repository lock and the lookups in this file.
            (
                "repository is busy (locked by another process)",
                ErrorCode::BusyRepository,
            ),
            ("repository is busy", ErrorCode::BusyRepository),
            ("no symbol named \"nope\"", ErrorCode::NotFound),
            ("no symbol with uid \"#42\"", ErrorCode::NotFound),
            // Markers that name no code, and messages that carry no code at all.
            (
                "REFUSED: main is the repository default branch; rewriting it is forbidden",
                ErrorCode::InvalidInput,
            ),
            (
                "STALE_REMOTE: leased push rejected — the remote no longer matches the pre-rewrite OID",
                ErrorCode::InvalidInput,
            ),
            (
                "unsupported search scope \"banana\"; supported values are \"code\"",
                ErrorCode::InvalidInput,
            ),
            (
                "bad path /nope: No such file or directory",
                ErrorCode::InvalidInput,
            ),
        ];
        for &(message, expected) in cases {
            assert_eq!(classify_error(message), expected, "{message}");
        }
    }

    /// A failure envelope must carry the classified code AND the message
    /// verbatim — the message is what the user reads on stderr, the code is
    /// what an agent switches on.
    #[test]
    fn failure_response_carries_the_code_and_the_message_verbatim() {
        let message = "repository is busy (locked by another process)";
        let resp = failure_response("publish", message);

        assert!(!resp.ok);
        assert_eq!(resp.op, "publish");
        let error = resp.error.as_ref().unwrap();
        assert_eq!(error.code, ErrorCode::BusyRepository);
        assert_eq!(error.message, message);
        assert_eq!(resp.validate(), Ok(()));
    }

    /// Every envelope the daemon emits must satisfy `Envelope::validate`,
    /// for success and failure alike, across the whole read-only op
    /// surface. Without this, an op can return `ok: true` with a null
    /// result (the CLI would print `null` as an answer) or `ok: false`
    /// with no message (the agent would retry blind).
    #[test]
    fn every_read_op_emits_a_valid_envelope() {
        let root = tmpdir("envelope-contract");
        std::fs::write(
            root.join("login.rs"),
            "pub fn login(user: &str) -> bool { !user.is_empty() }\n",
        )
        .unwrap();
        std::fs::write(
            root.join("caller.rs"),
            "use crate::login::login;\npub fn go() { login(\"a\"); }\n",
        )
        .unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "init"]);

        let mut svc = Service::open(&root).unwrap();
        let reqs: Vec<Request> = vec![
            Request::Ping,
            Request::Status {},
            Request::Graph {},
            serde_json::from_value(json!({"op":"search","pattern":"login"})).unwrap(),
            serde_json::from_value(json!({"op":"search","pattern":"("})).unwrap(),
            serde_json::from_value(json!({"op":"symbol","name":"login"})).unwrap(),
            serde_json::from_value(json!({"op":"symbol","name":"does_not_exist"})).unwrap(),
            serde_json::from_value(
                json!({"op":"impact","uid_or_name":"login","direction":"upstream"}),
            )
            .unwrap(),
            serde_json::from_value(json!({"op":"context","uid":"nope"})).unwrap(),
            serde_json::from_value(json!({"op":"skeleton","file":"login.rs"})).unwrap(),
            serde_json::from_value(json!({"op":"targets","task":"fix login"})).unwrap(),
            serde_json::from_value(json!({"op":"processes"})).unwrap(),
            serde_json::from_value(json!({"op":"clusters"})).unwrap(),
            serde_json::from_value(json!({"op":"inspect"})).unwrap(),
            serde_json::from_value(json!({"op":"recall","action":"search"})).unwrap(),
        ];
        let mut saw_failure = false;
        for req in reqs {
            let op = req.op_name();
            let resp = svc.handle(req);
            assert_eq!(resp.op, op, "envelope op must echo the request op");
            assert_eq!(resp.validate(), Ok(()), "op {op}: {resp:?}");
            saw_failure |= !resp.ok;
            // Round trip through the NDJSON wire: one line, parses back to
            // a still-valid envelope with the same verdict. (Payloads are
            // not compared: f32 scores widen to f64 on the way through
            // `Value`, which is a float detail, not a contract break.)
            let line = serde_json::to_string(&resp).unwrap();
            assert!(
                !line.contains('\n'),
                "op {op}: wire line must be single-line"
            );
            let back: Response = serde_json::from_str(&line).unwrap();
            assert_eq!(back.validate(), Ok(()), "op {op}: wire line re-validates");
            assert_eq!(
                (back.ok, back.op, back.error),
                (resp.ok, resp.op, resp.error)
            );
        }
        assert!(saw_failure, "battery must include at least one failing op");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `pixel list-signatures <file>` must render every symbol's signature — kind +
    /// sig — ordered by start_line, without pulling the file bodies in
    /// (that's what makes it ~10% of a Read). The lang is surfaced so the
    /// renderer can keep the `//` comment prefix mute per language.
    #[test]
    fn skeleton_renders_signatures_without_bodies() {
        let root = tmpdir("skeleton-render");
        std::fs::write(
            root.join("mod.rs"),
            concat!(
                "//! module doc\n",
                "pub struct Config { pub port: u16 }\n",
                "pub fn bootstrap(seed: u64) -> u64 {\n",
                "    seed + 1\n",
                "}\n",
                "mod inner { pub fn helper() -> i32 {\n",
                "    42\n",
                "} }\n",
                "pub trait Render { fn draw(&self); }\n",
            ),
        )
        .unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "init"]);

        let mut svc = Service::open(&root).unwrap();
        let resp = svc.handle(Request::Skeleton {
            file: "mod.rs".into(),
        });
        assert!(resp.ok, "{resp:?}");
        assert_eq!(resp.validate(), Ok(()));

        let data = resp.data();
        // Path is echoed back repo-relative and lang is detected.
        assert_eq!(data["file"], "mod.rs");
        assert_eq!(
            data["lang"].as_str(),
            Some("rust"),
            "skeleton must surface the detected lang: {data:?}"
        );
        let syms = data["symbols"].as_array().cloned().unwrap_or_default();
        assert!(!syms.is_empty(), "skeleton must find the indexed symbols");

        // Ordered by start_line: writers appear before the trait.
        let mut prev_line: i64 = -1;
        for s in &syms {
            let start = s["start_line"]
                .as_i64()
                .expect("every symbol has a start_line");
            assert!(
                start >= prev_line,
                "skeleton symbols must be ordered by start_line: {s:?}"
            );
            prev_line = start;
            // The skeleton contract: kind + sig (no body, no source text).
            let kind = s["kind"].as_str().unwrap_or("");
            let sig = s["sig"].as_str().map_or("", str::trim);
            assert!(!kind.is_empty(), "skeleton symbol missing kind: {s:?}");
            assert!(!sig.is_empty(), "skeleton symbol missing sig: {s:?}");
            // No `body`/source-text field survives on any skeleton symbol.
            assert!(
                !s.as_object().unwrap().contains_key("body"),
                "skeleton must not emit bodies: {s:?}"
            );
            // The stable id and line span survive so the skeleton can be `context`
            // drawn on via a follow-up op — that's the ~10%-cost value.
            assert!(
                !s["uid"].as_str().unwrap_or("").is_empty(),
                "skeleton symbol missing stable uid: {s:?}"
            );
            assert!(
                s["end_line"].as_i64().is_some(),
                "skeleton symbol missing end_line span: {s:?}"
            );
        }
        // Spot-check: the struct and fn are both signatures, not bodies.
        let names: std::collections::HashSet<&str> =
            syms.iter().filter_map(|s| s["name"].as_str()).collect();
        assert!(
            names.contains("bootstrap"),
            "skeleton must contain bootstrap: {names:?}"
        );
        assert!(
            names.contains("Config"),
            "skeleton must contain Config: {names:?}"
        );
        let bootstrap = syms
            .iter()
            .find(|s| s["name"] == "bootstrap")
            .expect("bootstrap");
        let sig = bootstrap["sig"].as_str().unwrap_or("");
        assert!(
            sig.contains("seed"),
            "skeleton sig must be the signature, not the body: {sig:?}"
        );
        assert!(
            !sig.contains("seed + 1"),
            "skeleton sig must NOT leak the body expression: {sig:?}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// `plan` answers from the daemon's own graph: an explicit query or the
    /// prompt's classification picks the queries, the answer names them, and
    /// an unknown query is an error rather than an empty plan.
    #[test]
    fn plan_runs_the_requested_queries_over_the_daemon_graph() {
        let root = tmpdir("plan-op");
        std::fs::write(
            root.join("app.ts"),
            "export function used() { return 1; }\n\
             export function orphan() { return 2; }\n\
             export function main() { return used(); }\n",
        )
        .unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "plan fixture"]);

        let mut svc = Service::open(&root).unwrap();
        let plan = |svc: &mut Service, prompt: Option<&str>, query: Option<&str>| {
            svc.handle(Request::Plan {
                prompt: prompt.map(str::to_string),
                query: query.map(str::to_string),
                tag: None,
                limit: None,
            })
        };
        let dead = plan(&mut svc, None, Some("dead-code"));
        assert!(dead.ok, "{:?}", dead.error);
        assert_eq!(dead.data()["queries"], json!(["dead-code"]));
        let findings = dead.data()["findings"].as_array().unwrap();
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0]["file"], "app.ts");
        assert_eq!(findings[0]["line"], 2);
        assert!(
            findings[0]["label"].as_str().unwrap().contains("`orphan`"),
            "{findings:?}"
        );
        assert_eq!(dead.op, "plan");
        assert!(dead.epistemics.is_some() && dead.snapshot.is_some());

        let classified = plan(&mut svc, Some("remove unused code"), None);
        assert!(classified.ok, "{:?}", classified.error);
        assert_eq!(classified.data()["queries"], json!(["dead-code"]));
        assert_eq!(classified.data()["findings"], dead.data()["findings"]);

        let unknown = plan(&mut svc, None, Some("everything"));
        assert!(!unknown.ok);
        assert!(
            unknown
                .error_message()
                .contains("unknown query 'everything'"),
            "{:?}",
            unknown.error
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn ping_reports_daemon_protocol_version() {
        let root = tmpdir("protocol-version");
        let mut svc = Service::open(&root).unwrap();
        let resp = svc.handle(Request::Ping);
        assert!(resp.ok);
        assert_eq!(
            resp.data().get("protocol_version").and_then(Value::as_u64),
            Some(super::PROTOCOL_VERSION)
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn uses_callees_reports_outgoing_uncertainty_without_changing_callers() {
        let root = tmpdir("uses-outgoing-uncertainty");
        std::fs::write(
            root.join("calls.ts"),
            "export function known() { return 1; }\n\
             export function entry() { known(); missing(); missing(); }\n\
             export function relay() { known(handler); }\n",
        )
        .unwrap();
        // Two files define `handler` and calls.ts imports neither: the
        // argument stays an unresolved reference.
        std::fs::write(root.join("a.ts"), "export function handler() {}\n").unwrap();
        std::fs::write(root.join("b.ts"), "export function handler() {}\n").unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "outgoing fixture"]);

        let mut svc = Service::open(&root).unwrap();
        let outgoing = svc.handle(Request::Uses {
            uid_or_name: "entry".into(),
            role: "callees".into(),
            offset: None,
        });
        assert!(outgoing.ok);
        assert_eq!(outgoing.data()["total_edges"], 1);
        assert_eq!(outgoing.data()["envelope"]["unresolved_outgoing"], 2);
        assert_eq!(outgoing.data()["envelope"]["lower_bound"], true);
        let epistemics = outgoing.epistemics.as_ref().unwrap();
        assert!(epistemics.lower_bound);
        assert!(!epistemics.closed_world);
        assert!(
            epistemics
                .basis
                .contains("2 unresolved outgoing call site(s)")
        );

        // `relay` passes an unresolved callback on but invokes only `known`:
        // the reference is not an outgoing call site the answer may miss.
        let relay = svc.handle(Request::Uses {
            uid_or_name: "relay".into(),
            role: "callees".into(),
            offset: None,
        });
        assert!(relay.ok);
        assert_eq!(relay.data()["total_edges"], 1, "{:?}", relay.data());
        assert_eq!(relay.data()["envelope"]["unresolved_outgoing"], 0);
        assert!(!relay.epistemics.as_ref().unwrap().lower_bound);

        for (symbol, role, count) in [
            ("entry", "callers", 0),
            ("known", "callees", 0),
            ("known", "callers", 2),
        ] {
            let response = svc.handle(Request::Uses {
                uid_or_name: symbol.into(),
                role: role.into(),
                offset: None,
            });
            assert!(response.ok);
            assert_eq!(response.data()["total_edges"], count);
            assert!(!response.epistemics.as_ref().unwrap().lower_bound);
            // closed_world is always false now: static analysis (tree-sitter)
            // cannot guarantee completeness — callbacks, dynamic dispatch,
            // and macro-generated calls are invisible to it.
            assert!(!response.epistemics.as_ref().unwrap().closed_world);
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn uses_pages_cover_all_edges_without_overlap() {
        let root = tmpdir("uses-pages");
        let mut source = String::from("export function target(): number { return 1 }\n");
        for index in 0..25 {
            source.push_str(&format!(
                "export function caller{index:02}(): number {{ return target() }}\n"
            ));
        }
        std::fs::write(root.join("calls.ts"), source).unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "calls"]);

        let mut svc = Service::open(&root).unwrap();
        let first = svc.handle(Request::Uses {
            uid_or_name: "target".into(),
            role: "callers".into(),
            offset: Some(0),
        });
        let second = svc.handle(Request::Uses {
            uid_or_name: "target".into(),
            role: "callers".into(),
            offset: Some(20),
        });
        assert!(first.ok && second.ok);
        let edge_uids = |response: &Response| {
            response.data()["edges"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|edge| edge["symbol"]["uid"].as_str().map(str::to_string))
                .collect::<std::collections::HashSet<_>>()
        };
        let first_uids = edge_uids(&first);
        let second_uids = edge_uids(&second);
        assert_eq!(first_uids.len(), 20);
        assert_eq!(second_uids.len(), 5);
        assert!(first_uids.is_disjoint(&second_uids));
        assert_eq!(first.data()["next_offset"].as_u64(), Some(20));
        assert!(second.data()["next_offset"].is_null());
        assert_eq!(second.data()["total_edges"].as_u64(), Some(25));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn changes_pages_cover_all_symbols_without_overlap() {
        let root = tmpdir("changes-pages");
        let make_source = |increment: usize| {
            (0..25)
                .map(|index| {
                    format!(
                        "export function changed{index:02}(x: number): number {{ return x + {increment} }}\n"
                    )
                })
                .collect::<String>()
        };
        std::fs::write(root.join("changed.ts"), make_source(1)).unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "baseline"]);
        std::fs::write(root.join("changed.ts"), make_source(2)).unwrap();

        let mut svc = Service::open(&root).unwrap();
        let first = svc.handle(Request::Changes {
            base: None,
            offset: Some(0),
            include_tests: false,
        });
        let second = svc.handle(Request::Changes {
            base: None,
            offset: Some(20),
            include_tests: false,
        });
        assert!(first.ok && second.ok, "first={first:?} second={second:?}");
        let symbol_uids = |response: &Response| {
            response.data()["symbols"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|symbol| symbol["uid"].as_str().map(str::to_string))
                .collect::<std::collections::HashSet<_>>()
        };
        let first_uids = symbol_uids(&first);
        let second_uids = symbol_uids(&second);
        assert_eq!(first_uids.len(), 20);
        assert_eq!(second_uids.len(), 5);
        assert!(first_uids.is_disjoint(&second_uids));
        assert_eq!(first.data()["next_offset"].as_u64(), Some(20));
        assert!(second.data()["next_offset"].is_null());
        assert_eq!(second.data()["symbols_total"].as_u64(), Some(25));

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Regression: a `Context` request with a small token budget must produce
    /// a response whose total serialized size is bounded by the budget (in
    /// tokens), not just the inner `text` field. Previously `--budget 50`
    /// could emit thousands of bytes because the structured incoming/outgoing
    /// sections were not counted.
    #[test]
    fn context_budget_covers_whole_response() {
        let root = tmpdir("ctx-budget");
        std::fs::write(
            root.join("a.ts"),
            "export function alpha(x: number): number { return x + 1 }\n\
             export function beta(x: number): number { return alpha(x) }\n",
        )
        .unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "init"]);

        let mut svc = Service::open(&root).unwrap();
        // Find the uid for `alpha`.
        let sym = svc.handle(Request::Symbol {
            name: "alpha".into(),
        });
        assert!(sym.ok, "symbol lookup: {sym:?}");
        let uid = sym
            .data()
            .get("symbols")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(|v| v.get("uid"))
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("no uid in {sym:?}"))
            .to_string();

        // Request a tiny budget. The whole response must be bounded.
        let resp = svc.handle(Request::Context {
            uid: uid.clone(),
            budget_tokens: Some(50),
        });
        assert!(resp.ok, "context: {resp:?}");
        let serialized = serde_json::to_string(resp.data()).unwrap();
        let tokens = pixel_context::estimate_tokens(&serialized);
        assert!(
            tokens <= 50,
            "whole-response budget exceeded: {tokens} tokens for budget 50 ({} bytes)",
            serialized.len()
        );
        // Text must be empty or very small when budget < overhead.
        let text = resp
            .data()
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("");
        assert!(
            text.is_empty() || pixel_context::estimate_tokens(text) <= 50,
            "text should be empty or tiny when budget is 50, got {} tokens",
            pixel_context::estimate_tokens(text)
        );
        // budgeted flag must be set so callers know the cap applied.
        assert_eq!(
            resp.data().get("budgeted").and_then(Value::as_bool),
            Some(true),
            "budgeted flag must be true when a budget is set"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Regression: a `Context` request with a moderate budget must produce a
    /// response whose total serialized size (including metadata + text) does
    /// not exceed the budget by more than a small rounding factor.
    #[test]
    fn context_moderate_budget_covers_whole_response() {
        let root = tmpdir("ctx-budget-moderate");
        std::fs::write(
            root.join("a.ts"),
            "export function alpha(x: number): number { return x + 1 }\n\
             export function beta(x: number): number { return alpha(x) }\n",
        )
        .unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "init"]);

        let mut svc = Service::open(&root).unwrap();
        let sym = svc.handle(Request::Symbol {
            name: "alpha".into(),
        });
        let uid = sym
            .data()
            .get("symbols")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .and_then(|v| v.get("uid"))
            .and_then(Value::as_str)
            .unwrap()
            .to_string();

        // Budget 500: metadata and text together must fit the hard limit.
        let resp = svc.handle(Request::Context {
            uid: uid.clone(),
            budget_tokens: Some(500),
        });
        assert!(resp.ok, "context: {resp:?}");
        let serialized = serde_json::to_string(resp.data()).unwrap();
        let tokens = pixel_context::estimate_tokens(&serialized);
        assert!(
            tokens <= 500,
            "whole-response budget exceeded: {tokens} tokens for budget 500 ({} bytes)",
            serialized.len()
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Regression: a broad search (`.*` or a short literal hitting every file)
    /// must be bounded by a default row limit and a byte cap, not return
    /// unbounded output.
    #[test]
    fn search_broad_pattern_is_bounded() {
        let root = tmpdir("search-bound");
        git(&root, &["init", "-q"]);
        // 120 files, each with a common needle, so the default 100-row page
        // is exercised rather than merely reported.
        for i in 0..120 {
            std::fs::write(
                root.join(format!("f{i:03}.rs")),
                format!("fn commonBroadNeedle{i:03}() {{}}\n"),
            )
            .unwrap();
        }
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "many"]);

        let mut svc = Service::open(&root).unwrap();
        // Broad pattern with no explicit limit: default limit applies.
        let resp = svc.handle(Request::Search {
            paths: None,
            pattern: "commonBroadNeedle".into(),
            json: true,
            limit: None,
            offset: None,
            scope: None,
        });
        assert!(resp.ok, "search: {resp:?}");
        let matches = resp
            .data()
            .get("matches")
            .and_then(Value::as_array)
            .unwrap();
        // Default limit is 100; 120 matching files must return one full page
        // with an exact continuation offset.
        assert_eq!(
            resp.data().get("limit").and_then(Value::as_u64),
            Some(100),
            "default limit must be reported"
        );
        assert_eq!(matches.len(), 100, "default limit must cap matches");
        assert_eq!(
            resp.data().get("next_offset").and_then(Value::as_u64),
            Some(100)
        );
        // Now request a tiny limit: must truncate.
        let resp = svc.handle(Request::Search {
            paths: None,
            pattern: "commonBroadNeedle".into(),
            json: true,
            limit: Some(5),
            offset: None,
            scope: None,
        });
        assert!(resp.ok);
        let matches = resp
            .data()
            .get("matches")
            .and_then(Value::as_array)
            .unwrap();
        let truncated = resp
            .data()
            .get("truncated")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        assert!(matches.len() <= 5, "explicit limit must cap matches");
        assert!(truncated, "truncated must be true when more matches exist");
        let first_page = matches.clone();

        let resp = svc.handle(Request::Search {
            paths: None,
            pattern: "commonBroadNeedle".into(),
            json: true,
            limit: Some(5),
            offset: Some(5),
            scope: None,
        });
        assert!(resp.ok);
        let second_page = resp
            .data()
            .get("matches")
            .and_then(Value::as_array)
            .unwrap();
        assert_eq!(second_page.len(), 5);
        assert!(
            first_page.iter().all(|item| !second_page.contains(item)),
            "offset page must not repeat prior matches"
        );
        assert_eq!(resp.data().get("offset").and_then(Value::as_u64), Some(5));
        assert_eq!(
            resp.data().get("next_offset").and_then(Value::as_u64),
            Some(10)
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// `search --scope code` must preserve the hit set (same (path, line)
    /// pairs as unranked) while reranking by file-level signals. A file
    /// whose basename matches the pattern should rank ahead of a file with
    /// the same match count but no filename/symbol signal.
    #[test]
    fn search_scope_code_preserves_hit_set_and_reranks() {
        let root = tmpdir("search-scope-code");
        // login.rs: basename matches "login", defines `login` symbol.
        std::fs::write(
            root.join("login.rs"),
            "pub fn login(user: &str) -> bool { !user.is_empty() }\n",
        )
        .unwrap();
        // caller.rs: contains "login" in content but not in filename/symbol.
        std::fs::write(
            root.join("caller.rs"),
            "use crate::login::login;\npub fn go() { login(\"a\"); }\n",
        )
        .unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "init"]);

        let mut svc = Service::open(&root).unwrap();

        // Unranked: path/line order → caller.rs before login.rs (alphabetical).
        let unranked = svc.handle(Request::Search {
            paths: None,
            pattern: "login".into(),
            json: true,
            limit: Some(50),
            offset: None,
            scope: None,
        });
        assert!(unranked.ok, "unranked: {:?}", unranked.error);
        assert_eq!(
            unranked.data().get("ranked").and_then(Value::as_bool),
            Some(false)
        );
        let unranked_matches = unranked
            .data()
            .get("matches")
            .and_then(Value::as_array)
            .unwrap();
        let unranked_set: std::collections::HashSet<(String, u64)> = unranked_matches
            .iter()
            .map(|m| {
                (
                    m["path"].as_str().unwrap().to_string(),
                    m["line"].as_u64().unwrap(),
                )
            })
            .collect();

        // Ranked: same hit set, but login.rs should rank first (filename +
        // symbol signal), ahead of caller.rs.
        let ranked = svc.handle(Request::Search {
            paths: None,
            pattern: "login".into(),
            json: true,
            limit: Some(50),
            offset: None,
            scope: Some("code".into()),
        });
        assert!(ranked.ok, "ranked: {:?}", ranked.error);
        assert_eq!(
            ranked.data().get("ranked").and_then(Value::as_bool),
            Some(true)
        );
        let ranked_matches = ranked
            .data()
            .get("matches")
            .and_then(Value::as_array)
            .unwrap();
        let ranked_set: std::collections::HashSet<(String, u64)> = ranked_matches
            .iter()
            .map(|m| {
                (
                    m["path"].as_str().unwrap().to_string(),
                    m["line"].as_u64().unwrap(),
                )
            })
            .collect();

        // Hit set must be identical (M1 parity gate).
        assert_eq!(
            unranked_set, ranked_set,
            "ranked search must preserve the hit set exactly"
        );

        // login.rs must rank first (filename + symbol signal beats content-only).
        let first_path = ranked_matches
            .first()
            .and_then(|m| m["path"].as_str())
            .unwrap_or("");
        assert_eq!(
            first_path, "login.rs",
            "filename+symbol signal must rank login.rs first, got {first_path}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Pins the tokenizer bug that made the BM25 channel silently inert:
    /// `split_ident_words` alone does not split on `|`, spaces or punctuation,
    /// so a regex-alternation pattern stayed one token and every term
    /// frequency came out zero.
    #[test]
    fn semantic_fallback_only_when_lexical_pass_is_empty_and_p1_is_allowed() {
        assert!(semantic_fallback_wanted(false, None));
        assert!(
            !semantic_fallback_wanted(false, Some("P1")),
            "semantic leads are P2: a P1 cap excludes them"
        );
        assert!(semantic_fallback_wanted(false, Some("P2")));
        assert!(
            !semantic_fallback_wanted(false, Some("P0")),
            "a P0-only caller must not receive semantic leads"
        );
        assert!(
            !semantic_fallback_wanted(true, None),
            "lexical P0/P1 hits make the fallback redundant"
        );
        assert!(!semantic_fallback_wanted(true, Some("P0")));
    }

    /// Semantic hits join `targets` as unverified P2 leads after the lexical
    /// targets, never duplicating a file already targeted.
    #[test]
    fn semantic_leads_are_unverified_p2_targets_not_already_present() {
        let fallback = pixel_recall::code_search::SemanticFallback {
            hits: vec![
                ("src/a.rs".to_string(), 0.391),
                ("src/b.rs".to_string(), 0.2),
            ],
            searched_files: 2,
            file_limit_reached: false,
        };
        let present = pixel_rank::TargetFile {
            path: "src/b.rs".to_string(),
            tier: "P2".to_string(),
            score: 0.1,
            reasons: Vec::new(),
            symbols: Vec::new(),
        };
        let leads = semantic_leads(&fallback, std::slice::from_ref(&present));
        assert_eq!(leads.len(), 1);
        assert_eq!(leads[0].path, "src/a.rs");
        assert_eq!(leads[0].tier, "P2");
        assert!((leads[0].score - 0.391).abs() < f64::EPSILON);
        assert_eq!(
            leads[0].reasons,
            ["semantic lead (similarity 0.39, unverified)"]
        );
        assert_eq!(semantic_leads(&fallback, &[]).len(), 2);
    }

    fn report_with(
        targets: Vec<pixel_rank::TargetFile>,
        caps: Option<Value>,
    ) -> pixel_rank::TargetsReport {
        let mut envelope = json!({ "lower_bound": false });
        if let Some(caps) = caps {
            envelope["caps"] = caps;
        }
        pixel_rank::TargetsReport {
            task: "t".to_string(),
            keywords: Vec::new(),
            exact_tokens: Vec::new(),
            path_tokens: Vec::new(),
            targets,
            envelope,
            closed_world: "false".to_string(),
            stats: json!({}),
        }
    }

    fn p2(path: &str) -> pixel_rank::TargetFile {
        pixel_rank::TargetFile {
            path: path.to_string(),
            tier: "P2".to_string(),
            score: 0.1,
            reasons: Vec::new(),
            symbols: Vec::new(),
        }
    }

    /// The fallback leaves a report untouched unless it adds a lead; a lead
    /// is appended within the limit and makes the answer a lower bound that
    /// names its caveats.
    #[test]
    fn apply_semantic_leads_records_the_fallback_only_when_it_adds_a_lead() {
        use pixel_recall::code_search::SemanticFallback;
        let mut report = report_with(vec![p2("src/a.rs")], None);
        apply_semantic_leads(&mut report, &SemanticFallback::default(), 5);
        let only_known = SemanticFallback {
            hits: vec![("src/a.rs".to_string(), 0.3)],
            ..SemanticFallback::default()
        };
        apply_semantic_leads(&mut report, &only_known, 5);
        assert_eq!(report.targets.len(), 1);
        assert!(report.stats.get("fallback").is_none(), "{}", report.stats);
        assert_eq!(report.envelope, json!({ "lower_bound": false }));

        let fallback = SemanticFallback {
            hits: vec![
                ("src/b.rs".to_string(), 0.4),
                ("src/c.rs".to_string(), 0.3),
                ("src/d.rs".to_string(), 0.2),
            ],
            searched_files: 2000,
            file_limit_reached: true,
        };
        let mut report = report_with(vec![p2("src/a.rs")], Some(json!(["probe capped"])));
        apply_semantic_leads(&mut report, &fallback, 3);
        let paths: Vec<&str> = report.targets.iter().map(|t| t.path.as_str()).collect();
        assert_eq!(
            paths,
            ["src/a.rs", "src/b.rs", "src/c.rs"],
            "lexical first, capped at 3"
        );
        assert_eq!(
            report.stats["fallback"],
            json!({"channel": "semantic", "reason": "no_p0_p1", "hits": 3, "searched_files": 2000, "file_limit_reached": true})
        );
        assert_eq!(report.envelope["lower_bound"], true);
        let caps: Vec<&str> = report.envelope["caps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c.as_str().unwrap())
            .collect();
        assert_eq!(caps.len(), 3, "{caps:?}");
        assert_eq!(caps[0], "probe capped");
        assert!(caps[1].starts_with("semantic leads are unverified"));
        assert!(caps[2].contains("first 2000"));

        let mut no_caps = report_with(Vec::new(), None);
        apply_semantic_leads(&mut no_caps, &fallback, 10);
        assert_eq!(no_caps.envelope["caps"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn tokenize_words_splits_on_punctuation_and_case() {
        assert_eq!(
            tokenize_words("index|disambiguation"),
            vec!["index", "disambiguation"],
            "regex alternation must split into terms"
        );
        assert_eq!(
            tokenize_words("let index = compute(index, index);"),
            vec!["let", "index", "compute", "index", "index"],
            "a source line must tokenize into words, not stay one blob"
        );
        assert_eq!(
            tokenize_words("resolveConceptPhrase snake_case"),
            vec!["resolve", "concept", "phrase", "snake", "case"],
            "camelCase and snake_case must still split"
        );
        assert!(
            tokenize_words("   ;;;   ").is_empty(),
            "punctuation-only yields no terms"
        );
    }

    /// Content channel is BM25, not raw match count. `index` appears in BOTH
    /// files (df = 2, so low IDF) and `alpha.rs` repeats it 90 times; `beta.rs`
    /// carries the rare `disambiguation` (df = 1, high IDF) 3 times in a far
    /// shorter document. Raw density
    /// ranks `alpha.rs` first on volume, and so does the path tie-break — so
    /// `beta.rs` landing first can only be BM25's IDF + length normalization.
    /// Neither basename contains a query word and there is no graph, so S1/S2
    /// are empty and S3 alone decides the fused order.
    #[test]
    fn rank_search_matches_content_channel_is_bm25_not_raw_count() {
        use pixel_index::verify::MatchLine;
        let mut matches: Vec<MatchLine> = (0..30)
            .map(|i| MatchLine {
                path: "alpha.rs".into(),
                line_number: i + 1,
                line: "let index = compute(index, index);".into(),
            })
            .collect();
        for i in 0..3 {
            matches.push(MatchLine {
                path: "beta.rs".into(),
                line_number: i + 1,
                line: "index disambiguation".into(),
            });
        }
        // Sanity: raw density really does favour alpha.rs here.
        let alpha_hits = matches.iter().filter(|m| m.path == "alpha.rs").count();
        let beta_hits = matches.iter().filter(|m| m.path == "beta.rs").count();
        assert!(
            alpha_hits > beta_hits,
            "precondition: alpha has more raw matches"
        );

        let ranked = rank_search_matches(&matches, "index|disambiguation", &None, None);
        assert_eq!(
            ranked[0].path, "beta.rs",
            "rare-term short doc must outrank common-term volume — BM25 content channel is not wired in"
        );
    }

    #[test]
    fn rank_search_matches_splits_filename_query_terms() {
        use pixel_index::verify::MatchLine;
        let matches = vec![
            MatchLine {
                path: "src/a_noise.rs".into(),
                line_number: 1,
                line: "imports edge".into(),
            },
            MatchLine {
                path: "src/z_imports.rs".into(),
                line_number: 1,
                line: "imports edge".into(),
            },
        ];
        for query in ["imports edge", "imports|edge"] {
            let ranked = rank_search_matches(&matches, query, &None, None);
            assert_eq!(
                ranked[0].path, "src/z_imports.rs",
                "filename terms must contribute for {query}"
            );
        }
    }

    #[test]
    fn rank_search_matches_does_not_promote_regex_escape_letters() {
        use pixel_index::verify::MatchLine;
        let matches = vec![
            MatchLine {
                path: "src/a_imports.rs".into(),
                line_number: 1,
                line: "imports".into(),
            },
            MatchLine {
                path: "src/z_bogus.rs".into(),
                line_number: 1,
                line: "imports".into(),
            },
        ];
        let ranked = rank_search_matches(&matches, r"\bimports\b", &None, None);
        assert_eq!(
            ranked[0].path, "src/a_imports.rs",
            "regex boundary b must not boost bogus"
        );
    }

    /// S1 fix: per-word filename scoring — "gain ledger" must match
    /// `ledger.ts` (the word "ledger" is a basename component), which
    /// whole-pattern basename containment (`basename.contains("gain ledger")`)
    /// never matched.
    #[test]
    fn rank_search_matches_scores_filename_per_word() {
        use pixel_index::verify::MatchLine;
        let matches = vec![
            MatchLine {
                path: "src/other.ts".into(),
                line_number: 1,
                line: "gain".into(),
            },
            MatchLine {
                path: "src/ledger.ts".into(),
                line_number: 1,
                line: "ledger".into(),
            },
        ];
        let ranked = rank_search_matches(&matches, "gain ledger", &None, None);
        assert_eq!(ranked[0].path, "src/ledger.ts");
        assert_eq!(ranked[1].path, "src/other.ts");
    }

    /// Bug 1a + Bug 4 regression: ranked search must consider the FULL
    /// bounded candidate pool, not just a path-order-sliced page, so a
    /// filename-signal match that sorts after every other candidate in
    /// plain path order still surfaces at a small `--limit`. Also pins the
    /// exact-basename-stem > longer-substring tie-break (Bug 4): before the
    /// fix, `bb.len().cmp(&ba.len())` sorted the LONGEST matching filename
    /// first, so `zzz_needle.rs` would have outranked `needle.rs`.
    #[test]
    fn search_scope_code_ranks_globally_beyond_small_page_and_prefers_exact_filename() {
        let root = tmpdir("search-scope-global-rank");
        for i in 0..30 {
            std::fs::write(
                root.join(format!("f{i:02}.rs")),
                format!("// needle mention {i}\n"),
            )
            .unwrap();
        }
        // Both sort AFTER all 30 `f*.rs` files alphabetically, so a
        // path-order bounded probe at `--limit 3` never even reaches them
        // pre-fix (candidates are visited in sorted path order and the
        // probe stops as soon as `limit + 1` matches are found among the
        // `f*.rs` files alone).
        std::fs::write(root.join("needle.rs"), "pub fn other() { /* needle */ }\n").unwrap();
        std::fs::write(
            root.join("zzz_needle.rs"),
            "pub fn another() { /* needle */ }\n",
        )
        .unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "init"]);

        let mut svc = Service::open(&root).unwrap();
        let resp = svc.handle(Request::Search {
            paths: None,
            pattern: "needle".into(),
            json: true,
            limit: Some(3),
            offset: None,
            scope: Some("code".into()),
        });
        assert!(resp.ok, "ranked search: {:?}", resp.error);
        let matches = resp
            .data()
            .get("matches")
            .and_then(Value::as_array)
            .unwrap();
        let paths: Vec<&str> = matches.iter().filter_map(|m| m["path"].as_str()).collect();
        assert_eq!(
            paths.len(),
            3,
            "expected a full page of 3 ranked matches, got {paths:?}"
        );
        assert_eq!(
            &paths[..2],
            &["needle.rs", "zzz_needle.rs"],
            "exact filename match must surface first, substring match second, despite \
             both sorting after every `f*.rs` file in plain path order; got {paths:?}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Bug 1b regression: walking every ranked page via `next_offset` must
    /// cover every true match exactly once — no duplicates (previously
    /// possible when a byte/row-limit page boundary disagreed with a rank
    /// reordering computed only within that already-sliced page) and no
    /// gaps.
    #[test]
    fn search_scope_code_pagination_has_no_duplicates_or_gaps() {
        const TOTAL: usize = 37;
        let root = tmpdir("search-scope-pagination");
        for i in 0..TOTAL {
            std::fs::write(
                root.join(format!("file{i:03}.rs")),
                format!("// banana occurrence {i}\n"),
            )
            .unwrap();
        }
        git(&root, &["init", "-q"]);
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "init"]);

        let mut svc = Service::open(&root).unwrap();
        let mut seen: Vec<String> = Vec::new();
        let mut offset = Some(0usize);
        let mut pages = 0;
        while let Some(o) = offset {
            let resp = svc.handle(Request::Search {
                paths: None,
                pattern: "banana".into(),
                json: true,
                limit: Some(5),
                offset: Some(o),
                scope: Some("code".into()),
            });
            assert!(resp.ok, "page at offset {o}: {:?}", resp.error);
            let matches = resp
                .data()
                .get("matches")
                .and_then(Value::as_array)
                .unwrap();
            for m in matches {
                seen.push(m["path"].as_str().unwrap().to_string());
            }
            offset = resp
                .data()
                .get("next_offset")
                .and_then(Value::as_u64)
                .map(|v| v as usize);
            pages += 1;
            assert!(
                pages <= TOTAL,
                "pagination did not terminate: seen={seen:?}"
            );
        }

        let unique: std::collections::HashSet<&String> = seen.iter().collect();
        assert_eq!(
            seen.len(),
            unique.len(),
            "ranked pagination must not repeat a row: {seen:?}"
        );
        assert_eq!(
            unique.len(),
            TOTAL,
            "ranked pagination must cover every match exactly once: got {} of {TOTAL}: {seen:?}",
            unique.len()
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Bug 2 regression: the symbol-signal must be deterministic based on
    /// "does graph.db exist on disk", never on whether some unrelated op
    /// happened to warm `self.graph` earlier in this same daemon process.
    #[test]
    fn search_scope_code_ranking_is_deterministic_regardless_of_prior_daemon_activity() {
        let root = tmpdir("search-scope-determinism");
        // Defines a symbol literally named `needle` -- its rank depends
        // entirely on the graph symbol signal (no filename hit).
        std::fs::write(
            root.join("b_defines.rs"),
            "pub fn needle() -> bool { true }\n",
        )
        .unwrap();
        // Sorts first alphabetically; mentions "needle" once in a comment
        // -- same content-density score as the line inside `b_defines.rs`,
        // no symbol, no filename signal.
        std::fs::write(root.join("a_mentions.rs"), "// needle mentioned here\n").unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "init"]);

        // Build graph.db via a throwaway Service so THIS test's Service
        // starts with `graph: None` in memory while graph.db already
        // exists on disk -- the `--no-daemon` / daemon-restart scenario
        // Bug 2 describes.
        {
            let mut builder = Service::open(&root).unwrap();
            let built = builder.handle(Request::Graph {});
            assert!(built.ok, "graph build: {:?}", built.error);
        }

        let mut svc = Service::open(&root).unwrap();
        let search = |svc: &mut Service| -> Vec<String> {
            let resp = svc.handle(Request::Search {
                paths: None,
                pattern: "needle".into(),
                json: true,
                limit: Some(10),
                offset: None,
                scope: Some("code".into()),
            });
            assert!(resp.ok, "search: {:?}", resp.error);
            resp.data()
                .get("matches")
                .and_then(Value::as_array)
                .unwrap()
                .iter()
                .map(|m| m["path"].as_str().unwrap().to_string())
                .collect()
        };

        // Call 1: `svc.graph` is still `None` in memory; only graph.db on
        // disk. The symbol signal must already apply.
        let first = search(&mut svc);
        assert_eq!(
            first.first().map(String::as_str),
            Some("b_defines.rs"),
            "symbol signal must apply from an on-disk graph.db even with no prior \
             in-process graph activity on this Service, got {first:?}"
        );

        // Unrelated daemon activity that happens to populate `self.graph`.
        let targets = svc.handle(Request::Targets {
            task: "needle".into(),
            limit: Some(5),
            max_tier: None,
            precision: false,
        });
        assert!(targets.ok, "targets: {:?}", targets.error);

        // Call 2: identical repo, identical query -- must be byte-for-byte
        // the same ranking as call 1, regardless of the intervening
        // `targets` call.
        let second = search(&mut svc);
        assert_eq!(
            first, second,
            "ranking must not depend on unrelated prior daemon activity"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Bug 5 regression: an unrecognized `scope` value must be a clear
    /// error, not a silent fallback to unranked search. Valid values stay
    /// case-insensitive.
    #[test]
    fn search_rejects_unknown_scope_instead_of_silently_falling_back() {
        let root = tmpdir("search-bad-scope");
        std::fs::write(root.join("a.rs"), "// needle\n").unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "init"]);

        let mut svc = Service::open(&root).unwrap();
        let resp = svc.handle(Request::Search {
            paths: None,
            pattern: "needle".into(),
            json: true,
            limit: Some(5),
            offset: None,
            scope: Some("banana".into()),
        });
        assert!(!resp.ok, "unknown scope must fail, not silently succeed");
        assert_eq!(
            resp.error.as_ref().map(|e| e.code),
            Some(pixel_proto::ErrorCode::InvalidInput)
        );

        let unranked = svc.handle(Request::Search {
            paths: None,
            pattern: "needle".into(),
            json: true,
            limit: Some(5),
            offset: None,
            scope: None,
        });
        assert!(
            unranked.ok,
            "no scope must remain valid: {:?}",
            unranked.error
        );

        let upper = svc.handle(Request::Search {
            paths: None,
            pattern: "needle".into(),
            json: true,
            limit: Some(5),
            offset: None,
            scope: Some("CODE".into()),
        });
        assert!(
            upper.ok,
            "scope must be case-insensitive: {:?}",
            upper.error
        );
        assert_eq!(
            upper.data().get("ranked").and_then(Value::as_bool),
            Some(true)
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Phase 3 item 1: `op_targets` must attach per-file content evidence
    /// (first ~2 match lines per keyword) so a caller can verify a target's
    /// content match without re-searching (S2 distrust loop).
    #[test]
    fn targets_attach_content_evidence() {
        let root = tmpdir("targets-evidence");
        std::fs::write(
            root.join("ledger.rs"),
            "// gain ledger entry\npub fn ledger() {}\n",
        )
        .unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "init"]);

        let mut svc = Service::open(&root).unwrap();
        let resp = svc.handle(Request::Targets {
            task: "gain ledger".into(),
            limit: Some(5),
            max_tier: None,
            precision: false,
        });
        assert!(resp.ok, "targets: {:?}", resp.error);
        let targets = resp
            .data()
            .get("targets")
            .and_then(Value::as_array)
            .unwrap();
        let ledger = targets
            .iter()
            .find(|t| t["path"].as_str() == Some("ledger.rs"))
            .expect("ledger.rs should be a target");
        let evidence = ledger.get("evidence").and_then(Value::as_array).unwrap();
        assert!(
            !evidence.is_empty(),
            "expected content evidence on ledger.rs"
        );
        assert!(
            evidence.iter().any(|e| {
                e["keyword"].as_str() == Some("ledger")
                    && e["text"].as_str().is_some_and(|t| t.contains("ledger"))
            }),
            "expected a 'ledger' evidence entry, got {evidence:?}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// S1 fix: ranked search must match a file whose basename contains a
    /// single word of a multi-word pattern ("gain ledger" → `ledger.ts`),
    /// not require the whole phrase to be a basename substring. Without the
    /// per-word filename signal, `ledger.ts` and `zzz_other.ts` (identical
    /// content) would tie on content density and sort by path — `zzz_other.ts`
    /// first. With it, `ledger.ts` ranks first on the filename word.
    #[test]
    fn search_scope_code_matches_per_word_filename() {
        let root = tmpdir("search-per-word-filename");
        std::fs::write(root.join("ledger.ts"), "// gain ledger here\n").unwrap();
        std::fs::write(root.join("zzz_other.ts"), "// gain ledger here\n").unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "init"]);

        let mut svc = Service::open(&root).unwrap();
        let resp = svc.handle(Request::Search {
            paths: None,
            pattern: "gain ledger".into(),
            json: true,
            limit: Some(10),
            offset: None,
            scope: Some("code".into()),
        });
        assert!(resp.ok, "ranked search: {:?}", resp.error);
        let matches = resp
            .data()
            .get("matches")
            .and_then(Value::as_array)
            .unwrap();
        let paths: Vec<&str> = matches.iter().filter_map(|m| m["path"].as_str()).collect();
        assert_eq!(
            paths.first().copied(),
            Some("ledger.ts"),
            "per-word filename signal must rank ledger.ts first, got {paths:?}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Phase 3 item 1 — the epistemics choke point: EVERY retrieval-class
    /// op's successful response must carry an `epistemics` object and a
    /// repo `snapshot`. This is the mechanical walk over `RETRIEVAL_OPS`
    /// that makes shipping a retrieval answer without epistemics a test
    /// failure, given the choke-point enforcement in `Service::handle`.
    #[test]
    fn every_retrieval_op_response_carries_epistemics_and_snapshot() {
        let root = tmpdir("epistemics-walk");
        std::fs::write(
            root.join("a.ts"),
            "export function alpha(x: number): number { return x + 1 }\n\
             export function beta(x: number): number { return alpha(x) }\n",
        )
        .unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "init"]);
        // Dirty edit so `changes` has something to report.
        std::fs::write(
            root.join("a.ts"),
            "export function alpha(x: number): number { return x + 2 }\n\
             export function beta(x: number): number { return alpha(x) }\n",
        )
        .unwrap();

        let mut svc = Service::open(&root).unwrap();
        let uid = {
            let sym = svc.handle(Request::Symbol {
                name: "alpha".into(),
            });
            sym.data()["symbols"][0]["uid"]
                .as_str()
                .unwrap()
                .to_string()
        };

        let requests: Vec<(&str, Request)> = vec![
            (
                "search",
                Request::Search {
                    pattern: "alpha".into(),
                    json: true,
                    limit: Some(10),
                    offset: None,
                    paths: None,
                    scope: None,
                },
            ),
            (
                "resolve",
                Request::Resolve {
                    phrase: "alpha".into(),
                    limit: Some(5),
                },
            ),
            (
                "targets",
                Request::Targets {
                    task: "alpha beta".into(),
                    limit: Some(5),
                    max_tier: None,
                    precision: false,
                },
            ),
            (
                "impact",
                Request::Impact {
                    uid_or_name: "alpha".into(),
                    direction: "upstream".into(),
                    depth: Some(2),
                },
            ),
            (
                "uses",
                Request::Uses {
                    uid_or_name: "alpha".into(),
                    role: "callers".into(),
                    offset: None,
                },
            ),
            (
                "trace",
                Request::Trace {
                    from: "beta".into(),
                    to: "alpha".into(),
                },
            ),
            (
                "changes",
                Request::Changes {
                    base: None,
                    offset: None,
                    include_tests: false,
                },
            ),
            (
                "context",
                Request::Context {
                    uid,
                    budget_tokens: Some(2000),
                },
            ),
            (
                "symbol",
                Request::Symbol {
                    name: "alpha".into(),
                },
            ),
            ("processes", Request::Processes { offset: None }),
            ("clusters", Request::Clusters { offset: None }),
            (
                "plan",
                Request::Plan {
                    prompt: None,
                    query: Some("dead-code".into()),
                    tag: None,
                    limit: None,
                },
            ),
        ];

        // The walk itself must cover the registry exactly — a new retrieval
        // op added to RETRIEVAL_OPS without a row here fails loudly.
        let walked: std::collections::HashSet<&str> =
            requests.iter().map(|(name, _)| *name).collect();
        for op in super::RETRIEVAL_OPS {
            assert!(
                walked.contains(op),
                "RETRIEVAL_OPS entry {op:?} not exercised by this test"
            );
        }

        for (name, req) in requests {
            assert_eq!(req.op_name(), name, "walk row mislabeled");
            let resp = svc.handle(req);
            assert!(resp.ok, "{name}: {:?}", resp.error);
            let epistemics = resp
                .epistemics
                .as_ref()
                .unwrap_or_else(|| panic!("{name}: retrieval response shipped WITHOUT epistemics"));
            assert!(
                !epistemics.basis.is_empty(),
                "{name}: epistemics.basis must name the answer's source"
            );
            // closed_world is always false: static analysis (tree-sitter)
            // is never complete — callbacks, dynamic dispatch, and
            // macro-generated calls are invisible to it. lower_bound still
            // flags resolution uncertainty (same-name unresolved calls).
            assert!(
                !epistemics.closed_world,
                "{name}: closed_world must always be false — static analysis is never complete: {epistemics:?}"
            );
            // The basis names the store the answer came from, per op: a
            // reader of `search` must not be told "code graph".
            let source = match name {
                "search" => "text index",
                "targets" | "resolve" => "text index + code graph",
                "changes" => "code graph + working-tree diff",
                _ => "code graph",
            };
            assert!(
                epistemics.basis.starts_with(source),
                "{name}: basis must open with {source:?}: {:?}",
                epistemics.basis
            );
            let snapshot = resp
                .snapshot
                .as_ref()
                .unwrap_or_else(|| panic!("{name}: retrieval response shipped WITHOUT snapshot"));
            assert!(snapshot.head.is_some(), "{name}: snapshot must carry HEAD");
            // Retrieval answers carry the compact form: the dirty tree is
            // counted, never enumerated (one untracked vendor tree used to
            // turn every `symbol` answer into 240 KB).
            assert!(
                snapshot.dirty.is_empty(),
                "{name}: retrieval snapshot must not enumerate dirty paths, got {:?}",
                snapshot.dirty
            );
            let expected = pixel_index::gitsync::status_porcelain(&root).len() as u64;
            assert!(expected >= 1, "fixture must have a dirty file");
            assert_eq!(
                snapshot.dirty_count,
                Some(expected),
                "{name}: snapshot must count every dirty path"
            );
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    fn delta(fresh: bool, changed: usize, indexed: usize) -> pixel_graph::build::TreeDelta {
        pixel_graph::build::TreeDelta {
            fresh,
            changed: (0..changed)
                .map(|i| (format!("src/f{i}.ts"), i as u64))
                .collect(),
            removed: Vec::new(),
            indexed_files: indexed,
            signature: "cafe".to_string(),
        }
    }

    /// The gate turns a measured delta into one of three actions, and each
    /// pair of them is a different promise to the caller.
    ///
    /// A fresh graph must be `Ready`, not `Incremental`: the incremental
    /// path takes the store away, calls `apply_tree_delta` — which re-signs
    /// it even with nothing to change — and reopens it, so treating fresh
    /// as drifted makes a question write to the graph it is asking about.
    /// A drifted graph must never be `Ready`, which would answer from rows
    /// the tree no longer matches. And drift past the threshold is `Stale`
    /// rather than a minutes-long rebuild nobody asked for.
    #[test]
    fn the_evaluate_gate_should_answer_from_a_fresh_graph_and_never_rewrite_it() {
        assert_eq!(
            gate_action(Some(&delta(true, 0, 10)), DEFAULT_GRAPH_INCREMENTAL_MAX_PCT),
            GateAction::Ready,
            "a fresh graph is answered from directly"
        );
        // Fresh wins even where the drift would also have been applicable:
        // the two questions are not the same one.
        assert_eq!(
            gate_action(
                Some(&delta(true, 1, 100)),
                DEFAULT_GRAPH_INCREMENTAL_MAX_PCT
            ),
            GateAction::Ready
        );
    }

    #[test]
    fn the_evaluate_gate_should_apply_drift_under_the_threshold_before_answering() {
        assert_eq!(
            gate_action(
                Some(&delta(false, 1, 100)),
                DEFAULT_GRAPH_INCREMENTAL_MAX_PCT
            ),
            GateAction::Incremental,
            "a drifted graph must not be answered from as-is"
        );
    }

    #[test]
    fn the_evaluate_gate_should_refuse_rather_than_rebuild_past_the_threshold() {
        assert_eq!(
            gate_action(
                Some(&delta(false, 99, 100)),
                DEFAULT_GRAPH_INCREMENTAL_MAX_PCT
            ),
            GateAction::Stale,
            "a full rebuild is never a side effect of a question"
        );
        assert_eq!(
            gate_action(Some(&delta(false, 1, 100)), 0),
            GateAction::Stale,
            "the incremental path disabled means stale, not ready"
        );
    }

    /// No delta at all is "built before signatures existed", or written by
    /// an update that withheld its signature. Either way the graph cannot
    /// be shown to describe the tree, so it is stale — never ready.
    #[test]
    fn a_graph_with_no_usable_signature_should_be_stale() {
        assert_eq!(
            gate_action(None, DEFAULT_GRAPH_INCREMENTAL_MAX_PCT),
            GateAction::Stale
        );
        assert_eq!(gate_action(None, 100), GateAction::Stale);
    }

    fn evaluation_with(
        depth_cap: u32,
        depth_cap_dropped_frontier: bool,
        time_budget_ms: u64,
        time_budget_hit: bool,
    ) -> pixel_graph::predicate::Evaluation {
        pixel_graph::predicate::Evaluation {
            status: pixel_graph::predicate::Status::AbsentInSnapshot,
            traversal: pixel_graph::predicate::Traversal::Callees,
            coverage: pixel_graph::predicate::Coverage {
                traversal_exhausted: true,
                depth_cap,
                depth_cap_dropped_frontier,
                time_budget_ms,
                time_budget_hit,
                visited: 3,
                unresolved_same_name_sites: 0,
                tiers: vec![pixel_graph::Tier::Exact],
                edge_kinds: Vec::new(),
            },
            witness: pixel_graph::predicate::Witness::None,
        }
    }

    /// A cap that fired is what turns an answer into a lower bound, and
    /// `derive_epistemics` reads exactly this list: empty means
    /// `lower_bound: false`, i.e. "nothing cut this answer short". So the
    /// list has to be empty when no cap fired and has to name the cap that
    /// did, with the number that would raise it — a placeholder string
    /// would set the flag while telling the caller nothing to act on.
    #[test]
    fn an_evaluation_that_hit_no_cap_should_claim_none() {
        assert!(evaluate_caps(&evaluation_with(8, false, 2_000, false)).is_empty());
    }

    #[test]
    fn a_dropped_frontier_should_be_named_as_a_cap_with_the_depth_to_raise() {
        let caps = evaluate_caps(&evaluation_with(8, true, 2_000, false));
        assert_eq!(caps.len(), 1, "{caps:?}");
        assert!(caps[0].contains("depth cap 8"), "{caps:?}");
        assert!(caps[0].contains("frontier"), "{caps:?}");
    }

    #[test]
    fn an_expired_time_budget_should_be_named_as_a_cap_with_its_duration() {
        let caps = evaluate_caps(&evaluation_with(8, false, 2_000, true));
        assert_eq!(caps.len(), 1, "{caps:?}");
        assert!(caps[0].contains("2000ms"), "{caps:?}");
        assert!(caps[0].contains("budget"), "{caps:?}");
    }

    /// Both caps firing must both be reported: a reader raising only the
    /// one they were told about would get the same truncated answer again.
    #[test]
    fn both_caps_firing_should_both_be_reported() {
        let caps = evaluate_caps(&evaluation_with(4, true, 50, true));
        assert_eq!(caps.len(), 2, "{caps:?}");
        assert!(caps.iter().any(|c| c.contains("depth cap 4")), "{caps:?}");
        assert!(caps.iter().any(|c| c.contains("50ms")), "{caps:?}");
    }

    /// The incremental/full decision is what keeps an agent's
    /// edit-then-`impact` loop at seconds instead of a full rebuild per
    /// cycle; the threshold is the documented `PIXEL_GRAPH_INCREMENTAL_MAX_PCT`
    /// contract (percentage of indexed files, `0` = always rebuild).
    #[test]
    fn incremental_allowed_follows_the_percentage_threshold() {
        // 2 of 10 files = 20 %: at the default threshold, incremental.
        assert!(incremental_allowed(
            2,
            10,
            DEFAULT_GRAPH_INCREMENTAL_MAX_PCT
        ));
        // 3 of 10 = 30 %: rebuild.
        assert!(!incremental_allowed(
            3,
            10,
            DEFAULT_GRAPH_INCREMENTAL_MAX_PCT
        ));
        // 1 of 5 = 20 % still incremental; 3 of 5 (the test fixture) is not.
        assert!(incremental_allowed(1, 5, 20));
        assert!(!incremental_allowed(3, 5, 20));
        // `0` disables the incremental path even for a single file.
        assert!(!incremental_allowed(1, 10_000, 0));
        // `100` never rebuilds for drift alone.
        assert!(incremental_allowed(10_000, 10_000, 100));
        // An empty graph has nothing to update incrementally.
        assert!(!incremental_allowed(1, 0, 20));
        // Removals count as drift too: 2 000 of 10 000 is the last
        // incremental size at 20 %, 2 001 is not.
        assert!(incremental_allowed(2_000, 10_000, 20));
        assert!(!incremental_allowed(2_001, 10_000, 20));
    }

    /// `impact`'s named caps must reach `derive_epistemics`: a report whose
    /// symbol lists were cut at the cap is a lower-bound answer with a
    /// `RESULT_CAPPED` warning, never a silently sampled one. A report with
    /// nothing cut adds neither.
    #[test]
    fn impact_caps_reach_epistemics() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let fid = store.replace_file("src/a.ts", "oid", "ts").unwrap();
        let handler = store
            .insert_symbol(
                fid,
                "src/a.ts#handler#function",
                "handler",
                "handler",
                SymbolKind::Function,
                1,
                5,
                "",
            )
            .unwrap();
        // 25 distinct referrers against the 20-item cap: the report lists 20
        // and names the cut with the real total.
        for i in 0..25u32 {
            let name = format!("caller{i}");
            let src = store
                .insert_symbol(
                    fid,
                    &format!("src/a.ts#{name}#function"),
                    &name,
                    &name,
                    SymbolKind::Function,
                    10 + i,
                    12 + i,
                    "",
                )
                .unwrap();
            store
                .insert_edge(&EdgeRow {
                    src_id: src,
                    dst_id: handler,
                    kind: EdgeKind::References,
                    tier: Tier::Probable,
                    site_line: 11 + i,
                    receiver: Some("on".to_string()),
                })
                .unwrap();
        }
        let other = store
            .insert_symbol(
                fid,
                "src/a.ts#other#function",
                "other",
                "other",
                SymbolKind::Function,
                100,
                105,
                "",
            )
            .unwrap();
        let single = store
            .insert_symbol(
                fid,
                "src/a.ts#single#function",
                "single",
                "single",
                SymbolKind::Function,
                110,
                115,
                "",
            )
            .unwrap();
        store
            .insert_edge(&EdgeRow {
                src_id: other,
                dst_id: single,
                kind: EdgeKind::References,
                tier: Tier::Probable,
                site_line: 106,
                receiver: None,
            })
            .unwrap();

        let cut = bridge::impact(&store, "src/a.ts#handler#function", "upstream", 3).unwrap();
        assert_eq!(cut["truncated"].as_bool(), Some(true), "{cut}");
        assert_eq!(cut["referenced_by_total"], 25, "{cut}");
        assert_eq!(
            cut["caps"],
            json!(["referenced_by truncated at 20 of 25 referencing symbols"]),
            "{cut}"
        );
        let (epistemics, warnings) = derive_epistemics("impact", &cut);
        assert!(epistemics.lower_bound, "{epistemics:?}");
        assert!(
            epistemics.basis.contains("truncated at 20 of 25"),
            "{epistemics:?}"
        );
        assert!(
            warnings.iter().any(|w| w.code == "RESULT_CAPPED"),
            "{warnings:?}"
        );

        // Nothing cut: no cap, no warning, no lower-bound downgrade.
        let plain = bridge::impact(&store, "src/a.ts#single#function", "upstream", 3).unwrap();
        assert_eq!(plain["truncated"].as_bool(), Some(false), "{plain}");
        assert_eq!(plain["referenced_by_total"], 1, "{plain}");
        assert_eq!(plain["caps"], json!([]), "{plain}");
        let (epistemics, warnings) = derive_epistemics("impact", &plain);
        assert!(!epistemics.lower_bound, "{epistemics:?}");
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    /// Phase 3 item 2 — targets honesty: when the bounded content probe
    /// cap fires for a keyword, the targets envelope must say lower_bound
    /// and NAME the cap; the "exhaustive" sentence must not be emitted.
    #[test]
    fn targets_probe_cap_sets_lower_bound_and_names_the_cap() {
        let root = tmpdir("targets-probe-cap");
        // 12 files x 100 lines = 1,200 word-bounded matches of "needle" —
        // comfortably beyond CONTENT_PROBE_LIMIT (currently 1,000).
        for f in 0..12 {
            let body: String = (0..100)
                .map(|i| format!("// needle occurrence {f}-{i}\n"))
                .collect();
            std::fs::write(root.join(format!("f{f}.rs")), body).unwrap();
        }
        git(&root, &["init", "-q"]);
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "many"]);

        let mut svc = Service::open(&root).unwrap();
        let resp = svc.handle(Request::Targets {
            task: "needle probe".into(),
            limit: Some(20),
            max_tier: None,
            precision: false,
        });
        assert!(resp.ok, "targets: {:?}", resp.error);
        let envelope = &resp.data()["envelope"];
        assert_eq!(
            envelope["lower_bound"].as_bool(),
            Some(true),
            "probe cap must force lower_bound: {envelope:?}"
        );
        let caps = envelope["caps"].as_array().unwrap();
        assert!(
            caps.iter().any(|c| {
                let s = c.as_str().unwrap_or_default();
                s.contains(&format!("content probe truncated at {CONTENT_PROBE_LIMIT}"))
                    && s.contains("'needle'")
            }),
            "the fired probe cap must be NAMED with its keyword: {caps:?}"
        );
        let closed_world = resp.data()["closed_world"].as_str().unwrap();
        assert!(
            !closed_world.contains("This list is exhaustive"),
            "capped probe must not claim exhaustiveness: {closed_world}"
        );
        assert!(
            closed_world.contains(&format!("content probe truncated at {CONTENT_PROBE_LIMIT}")),
            "the bounded phrasing must name the cap: {closed_world}"
        );
        // And the envelope-level epistemics must agree.
        let epistemics = resp.epistemics.as_ref().unwrap();
        assert!(!epistemics.closed_world && epistemics.lower_bound);
        assert!(
            epistemics
                .basis
                .contains(&format!("content probe truncated at {CONTENT_PROBE_LIMIT}"))
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Phase 3 item 2 — the content probe is word-bounded: keyword "auth"
    /// must not count "authorized"/"oauthToken" mentions as content signal.
    #[test]
    fn targets_content_probe_is_word_bounded() {
        let root = tmpdir("targets-word-bound");
        // Only substring mentions of "auth" — no word-bounded occurrence.
        std::fs::write(
            root.join("substr.rs"),
            "// authorized oauthToken authentication\n",
        )
        .unwrap();
        // A real word-bounded occurrence.
        std::fs::write(root.join("word.rs"), "// auth flow lives here\n").unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "init"]);

        let mut svc = Service::open(&root).unwrap();
        let resp = svc.handle(Request::Targets {
            task: "auth handling".into(),
            limit: Some(10),
            max_tier: None,
            precision: false,
        });
        assert!(resp.ok, "targets: {:?}", resp.error);
        let targets = resp.data()["targets"].as_array().unwrap();
        let content_reason = |path: &str| -> bool {
            targets
                .iter()
                .filter(|t| t["path"].as_str() == Some(path))
                .flat_map(|t| t["reasons"].as_array().cloned().unwrap_or_default())
                .any(|r| {
                    r.as_str().unwrap_or_default().contains("content matches")
                        && r.as_str().unwrap_or_default().contains("auth")
                })
        };
        assert!(
            content_reason("word.rs"),
            "word-bounded 'auth' occurrence must count as content signal: {targets:?}"
        );
        assert!(
            !content_reason("substr.rs"),
            "substring-only mentions must NOT count as 'auth' content signal: {targets:?}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Phase 3 item 3 — resolve surfaces the tier and scan-cap state in its
    /// serialized output, and an exact-unique identifier resolves as
    /// `resolved` (not permanently `ranked`).
    #[test]
    fn resolve_reports_tier_basis_and_resolved_confidence() {
        let root = tmpdir("resolve-honesty");
        std::fs::write(
            root.join("a.ts"),
            "export function uniqueTargetFn(): number { return 1 }\n",
        )
        .unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "init"]);

        let mut svc = Service::open(&root).unwrap();
        let resp = svc.handle(Request::Resolve {
            phrase: "uniqueTargetFn".into(),
            limit: Some(5),
        });
        assert!(resp.ok, "resolve: {:?}", resp.error);
        let data = resp.data();
        assert_eq!(
            data["confidence"].as_str(),
            Some("resolved"),
            "exact-unique identifier must resolve: {data:?}"
        );
        assert_eq!(data["scan_capped"].as_bool(), Some(false));
        assert!(
            data["basis"].as_str().unwrap_or_default().contains("ident"),
            "output must say which tier matched: {data:?}"
        );
        let epistemics = resp.epistemics.as_ref().unwrap();
        assert!(
            epistemics.basis.contains("ident"),
            "envelope epistemics must carry the tier basis: {epistemics:?}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn facts_lazy_ingest_failure_is_returned() {
        let root = tmpdir("facts-lazy-ingest-failure");
        std::fs::write(root.join("a.rs"), "pub fn a() {}\n").unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "init"]);

        let svc = Service::open(&root).unwrap();
        std::fs::remove_dir_all(root.join(".git")).unwrap();

        let error = match svc.facts_open_and_catch_up() {
            Ok(_) => panic!("lazy ingest must report a missing Git repository"),
            Err(error) => error,
        };
        assert!(error.contains("facts lazy ingest failed"), "{error}");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Two files, one commit, `login.rs` edited in a second commit and
    /// dirty in the tree: enough history for the signals and the facts
    /// index to have something to report.
    fn signals_repo(tag: &str) -> PathBuf {
        let root = tmpdir(tag);
        std::fs::write(
            root.join("login.rs"),
            "pub fn login(user: &str) -> bool { !user.is_empty() }\n",
        )
        .unwrap();
        std::fs::write(
            root.join("caller.rs"),
            "use crate::login::login;\npub fn go() { login(\"a\"); }\n",
        )
        .unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "init"]);
        std::fs::write(
            root.join("login.rs"),
            "pub fn login(user: &str) -> bool { user.len() > 1 }\n",
        )
        .unwrap();
        git(&root, &["commit", "-qam", "tighten login"]);
        std::fs::write(
            root.join("login.rs"),
            "pub fn login(user: &str) -> bool { user.len() > 2 }\n",
        )
        .unwrap();
        root
    }

    /// `status.facts` is how an agent sees whether history queries are
    /// answerable: phase A done, commit counts, diff text present.
    #[test]
    fn facts_visibility_reports_the_history_index_before_and_after_ingest() {
        let root = signals_repo("facts-visibility");
        let svc = Service::open(&root).unwrap();
        let before = svc.facts_visibility();
        assert_eq!(before["present"], true, "{before}");
        assert_eq!(before["phase_a_done"], false, "{before}");
        assert_eq!(before["total_commits"], 2, "rev-list count: {before}");
        assert_eq!(before["fresh"], false, "{before}");

        let mut facts = FactsStore::open(&root).unwrap();
        let report = pixel_facts::ingest::ingest_until_fresh_within(
            &mut facts,
            &pixel_facts::ingest::IngestOptions::default(),
            std::time::Duration::from_secs(30),
        )
        .unwrap();
        assert!(report.fresh, "{report:?}");
        let after = svc.facts_visibility();
        assert_eq!(after["phase_a_done"], true, "{after}");
        assert_eq!(after["commits_indexed"], 2, "{after}");
        assert_eq!(after["total_commits"], 2, "{after}");
        assert_eq!(after["fresh"], true, "{after}");
        assert!(after["hunks_with_text"].as_i64().unwrap() >= 1, "{after}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn op_note_sets_gets_lists_and_removes_annotations_keyed_by_repo_path() {
        let root = signals_repo("op-note");
        let mut svc = Service::open(&root).unwrap();
        // The service keys rows by the canonical root (macOS's temp dir is
        // a symlink), so the absolute form must be canonical too.
        let abs = root.canonicalize().unwrap().join("login.rs");
        let set = svc
            .op_note(
                "set",
                Some(abs.to_str().unwrap()),
                Some("login"),
                Some("guard first"),
            )
            .unwrap();
        assert_eq!(set["ok"], true);
        assert_eq!(
            set["file"], "login.rs",
            "absolute paths key the repo-relative row"
        );
        let got = svc
            .op_note("get", Some("./login.rs"), Some("login"), None)
            .unwrap();
        assert_eq!(got["note"], "guard first");
        let list = svc.op_note("list", None, None, None).unwrap();
        assert_eq!(list["total"], 1, "{list}");
        assert_eq!(list["capped"], false);
        let listed = svc.op_note("list", Some("caller.rs"), None, None).unwrap();
        assert_eq!(listed["total"], 0);
        let rm = svc
            .op_note("rm", Some("login.rs"), Some("login"), None)
            .unwrap();
        assert_eq!(rm["removed"], true);
        let rm_again = svc
            .op_note("rm", Some("login.rs"), Some("login"), None)
            .unwrap();
        assert_eq!(rm_again["removed"], false);
        assert!(
            svc.op_note("get", Some("login.rs"), Some("login"), None)
                .unwrap()["note"]
                .is_null()
        );
        assert!(svc.op_note("set", Some("login.rs"), None, None).is_err());
        assert!(svc.op_note("teleport", None, None, None).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn op_map_lists_every_file_with_its_symbols_in_line_order() {
        let root = signals_repo("op-map");
        let mut svc = Service::open(&root).unwrap();
        let map = svc.op_map(false).unwrap();
        assert_eq!(map["file_count"], 2, "{map}");
        assert_eq!(map["truncated"], false);
        assert!(map["symbol_count"].as_u64().unwrap() >= 2, "{map}");
        let files = map["files"].as_array().unwrap();
        let login = files
            .iter()
            .find(|f| f["path"] == "login.rs")
            .expect("login.rs");
        assert_eq!(login["symbols"][0]["name"], "login", "{login}");
        assert!(map.get("markdown").is_none());
        let md = svc.op_map(true).unwrap();
        assert!(md["markdown"].as_str().unwrap().contains("login"), "{md}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The reranker's signals: a file with commits behind it scores
    /// activity, an untouched candidate does not.
    #[test]
    fn engine_signals_scores_activity_for_files_with_history() {
        let root = signals_repo("engine-signals");
        let svc = Service::open(&root).unwrap();
        let bundle = svc.engine_signals(&["login.rs".to_string(), "caller.rs".to_string()]);
        let login = bundle.activity.get("login.rs").copied().unwrap_or(0.0);
        let caller = bundle.activity.get("caller.rs").copied().unwrap_or(0.0);
        assert!(login > 0.0, "{:?}", bundle.activity);
        assert!(login >= caller, "{:?}", bundle.activity);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A failure log speaks on the first failure and every doubling after
    /// it, so a permanently broken watcher cannot write one line per event.
    #[test]
    fn failure_log_reports_one_line_per_doubling() {
        let mut log = FailureLog::default();
        assert_eq!(log.count(), 0);
        let logged: Vec<bool> = (0..9).map(|_| log.record()).collect();
        assert_eq!(
            logged,
            [true, true, false, true, false, false, false, true, false]
        );
        assert_eq!(log.count(), 9);
    }

    #[test]
    fn ignore_control_refresh_reconciles_text_and_graph_stores() {
        let root = tmpdir("watcher-ignore-transition");
        std::fs::write(root.join("visible.rb"), "def visible\nend\n").unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "init"]);
        let mut svc = Service::open(&root).unwrap();
        let db = svc.graph_db_path();
        std::fs::create_dir_all(db.parent().unwrap()).unwrap();
        pixel_graph::build::build_graph(&root, &db).unwrap();

        std::fs::write(root.join("secret.rb"), "def watcherSecretNeedle\nend\n").unwrap();
        svc.refresh_file("secret.rb");
        assert_eq!(
            svc.index
                .read()
                .unwrap()
                .search("watcherSecretNeedle", None)
                .unwrap()
                .0
                .len(),
            1
        );
        assert!(
            GraphStore::open(&db)
                .unwrap()
                .file_by_path("secret.rb")
                .unwrap()
                .is_some()
        );

        std::fs::write(root.join(".gitignore"), "secret.rb\n").unwrap();
        svc.refresh_file(".gitignore");
        assert!(
            svc.index
                .read()
                .unwrap()
                .search("watcherSecretNeedle", None)
                .unwrap()
                .0
                .is_empty()
        );
        assert!(
            GraphStore::open(&db)
                .unwrap()
                .file_by_path("secret.rb")
                .unwrap()
                .is_none()
        );

        std::fs::write(root.join(".gitignore"), "").unwrap();
        svc.refresh_file(".gitignore");
        assert_eq!(
            svc.index
                .read()
                .unwrap()
                .search("watcherSecretNeedle", None)
                .unwrap()
                .0
                .len(),
            1
        );
        assert!(
            GraphStore::open(&db)
                .unwrap()
                .file_by_path("secret.rb")
                .unwrap()
                .is_some()
        );

        std::fs::write(root.join(".git/info/exclude"), "secret.rb\n").unwrap();
        svc.refresh_file(".git/info/exclude");
        assert!(
            svc.index
                .read()
                .unwrap()
                .search("watcherSecretNeedle", None)
                .unwrap()
                .0
                .is_empty()
        );
        assert!(
            GraphStore::open(&db)
                .unwrap()
                .file_by_path("secret.rb")
                .unwrap()
                .is_none()
        );
        std::fs::write(root.join(".git/info/exclude"), "").unwrap();
        svc.refresh_file(".git/info/exclude");
        assert_eq!(
            svc.index
                .read()
                .unwrap()
                .search("watcherSecretNeedle", None)
                .unwrap()
                .0
                .len(),
            1
        );
        assert!(
            GraphStore::open(&db)
                .unwrap()
                .file_by_path("secret.rb")
                .unwrap()
                .is_some()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A graph update that fails under the watcher is counted and surfaced
    /// in `status` instead of being dropped: the daemon serves a stale index
    /// until the next tree walk, so the failure must be visible.
    #[test]
    fn failed_graph_update_is_counted_in_status() {
        let root = tmpdir("watcher-failures");
        std::fs::write(root.join("login.rs"), "pub fn login() -> bool { true }\n").unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "init"]);
        let mut svc = Service::open(&root).unwrap();
        let graph_db = svc.graph_db_path();
        // The daemon graph path exists but is not a database (it is a
        // directory), so the update fails the way a locked or corrupt db
        // does while `graph_db_path().exists()` stays true.
        std::fs::create_dir_all(graph_db).unwrap();
        svc.refresh_file("login.rs");
        svc.refresh_files(&[("login.rs", false)]);
        // Through the transport hook the daemon loop calls, so the counting
        // path is covered end to end.
        crate::daemon::Corpus::watcher_error(&mut svc, "queue overflow");

        let status = svc.handle(Request::Status {});
        assert!(status.ok, "{status:?}");
        let data = status.into_data();
        assert_eq!(data["watcher"]["graph_update_failures"], 2, "{data}");
        assert_eq!(data["watcher"]["notify_errors"], 1, "{data}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The daemon adapter is the only wire between pixel-rank's Engine-3
    /// reranker and pixel-graph's pluggable `Reranker` — `resolve` reaches it
    /// through the trait object (invisible to the call graph), so nothing
    /// else covers it. An adapter that returned an empty vec would leave
    /// `resolve` reporting success with no matches at all; a no-op one would
    /// leave the candidate order pre-rerank. Pinned here: every candidate
    /// survives with its graph-side `id`/`tier`, and the order follows the
    /// weights table the adapter reads live.
    #[test]
    fn engine_reranker_preserves_candidates_and_applies_live_weights() {
        let signals = SignalBundle {
            activity: HashMap::from([("hot.rs".to_string(), 1.0)]),
            ..SignalBundle::default()
        };
        let candidates = vec![
            RankedCandidate {
                id: 11,
                path: "cold.rs".into(),
                rrf_score: 1.0,
                tier: "P0".into(),
            },
            RankedCandidate {
                id: 22,
                path: "hot.rs".into(),
                rrf_score: 1.0,
                tier: "P0".into(),
            },
            RankedCandidate {
                id: 33,
                path: "cold.rs".into(),
                rrf_score: 0.5,
                tier: "P1".into(),
            },
        ];

        let reranked = EngineReranker::new("tighten the login path").rerank(candidates, &signals);

        let ids: Vec<u64> = reranked.iter().map(|c| c.id).collect();
        assert_eq!(
            ids,
            vec![22, 11, 33],
            "activity must lift hot.rs over an equal-rrf cold.rs, and a P1 candidate stays below \
             P0 whatever its score: {reranked:?}"
        );
        assert_eq!(reranked[0].path, "hot.rs", "{reranked:?}");
        assert_eq!(reranked[0].tier, "P0", "{reranked:?}");
        assert_eq!(reranked[2].tier, "P1", "{reranked:?}");
        // The score is the shared formula applied through the live weights
        // table, not a constant that happens to sort the same way.
        let weights =
            pixel_rank::rerank::RerankWeights::from(&pixel_rank::signals::SignalOptions::default());
        let expected = 1.0 * (1.0 + weights.activity * 1.0);
        assert!(
            (reranked[0].rrf_score - expected).abs() < 1e-9,
            "expected 1.0 * (1 + activity_weight * 1.0) = {expected}, got {}",
            reranked[0].rrf_score
        );
    }

    /// The per-path test penalty is gated on the phrase: a task that is not
    /// about tests demotes a test file (enough to fall below a production
    /// file with a lower rrf), a task that names tests does not.
    #[test]
    fn engine_reranker_test_penalty_follows_the_phrase() {
        let candidates = vec![
            RankedCandidate {
                id: 1,
                path: "tests/login_test.rs".into(),
                rrf_score: 1.2,
                tier: "P0".into(),
            },
            RankedCandidate {
                id: 2,
                path: "login.rs".into(),
                rrf_score: 1.0,
                tier: "P0".into(),
            },
        ];
        let paths = |reranked: Vec<RankedCandidate>| -> Vec<String> {
            reranked.into_iter().map(|c| c.path).collect()
        };

        assert_eq!(
            paths(
                EngineReranker::new("tighten the login path")
                    .rerank(candidates.clone(), &SignalBundle::default())
            ),
            vec!["login.rs".to_string(), "tests/login_test.rs".to_string()],
            "1.2 * 0.7 = 0.84 must fall below 1.0 when the phrase is not about tests"
        );
        assert_eq!(
            paths(
                EngineReranker::new("tighten the login tests")
                    .rerank(candidates, &SignalBundle::default())
            ),
            vec!["tests/login_test.rs".to_string(), "login.rs".to_string()],
            "a phrase naming tests gates the penalty off, so the higher rrf wins"
        );
    }

    /// Rename fixture: a TS definition plus a caller that imports it.
    fn rename_root(tag: &str) -> PathBuf {
        let root = tmpdir(tag);
        std::fs::write(
            root.join("login.ts"),
            "export function loginUser(name: string): boolean {\n    return name.length > 0;\n}\n",
        )
        .unwrap();
        std::fs::write(
            root.join("caller.ts"),
            "import { loginUser } from \"./login\";\nexport function go(): boolean {\n    return loginUser(\"x\");\n}\n",
        )
        .unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "init"]);
        root
    }

    fn rename_req(name: &str, new_name: &str) -> Request {
        Request::Rename {
            name: name.to_string(),
            new_name: new_name.to_string(),
            file: None,
            uid: None,
            dry_run: false,
        }
    }

    /// The happy path end to end through the service: dry-run plans without
    /// writing, a real call rewrites the files and drops the cached graph.
    #[test]
    fn op_rename_dry_run_then_apply() {
        let root = rename_root("rename-apply");
        let mut svc = Service::open(&root).unwrap();
        let before = std::fs::read_to_string(root.join("caller.ts")).unwrap();

        let mut dry = rename_req("loginUser", "authenticate");
        if let Request::Rename { dry_run, .. } = &mut dry {
            *dry_run = true;
        }
        let resp = svc.handle(dry);
        assert!(resp.ok, "{resp:?}");
        let result = resp.result.unwrap();
        assert_eq!(result["dry_run"], json!(true));
        assert!(result["edit_count"].as_u64().unwrap() >= 3);
        assert_eq!(
            std::fs::read_to_string(root.join("caller.ts")).unwrap(),
            before,
            "dry-run must not write"
        );

        let resp = svc.handle(rename_req("loginUser", "authenticate"));
        assert!(resp.ok, "{resp:?}");
        let result = resp.result.unwrap();
        assert_eq!(result["dry_run"], json!(false));
        assert_eq!(result["applied"].as_array().unwrap().len(), 2);
        let caller = std::fs::read_to_string(root.join("caller.ts")).unwrap();
        assert!(caller.contains("import { authenticate }"), "{caller}");
        assert!(caller.contains("return authenticate("), "{caller}");
        // The graph handle was dropped: a follow-up symbol lookup sees the
        // new name, not the pre-rename snapshot.
        let sym = svc.handle(Request::Symbol {
            name: "authenticate".to_string(),
        });
        assert!(sym.ok, "{sym:?}");
        assert!(
            !sym.result.unwrap()["symbols"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    /// Identifier validation rejects names no grammar could emit.
    #[test]
    fn op_rename_rejects_non_identifiers() {
        let root = rename_root("rename-ident");
        let mut svc = Service::open(&root).unwrap();
        for bad in ["9bad", "a-b", "has space", ""] {
            let resp = svc.handle(rename_req("loginUser", bad));
            assert!(!resp.ok, "{bad:?} must fail: {resp:?}");
        }
        // And nothing was written on any rejection.
        assert!(
            std::fs::read_to_string(root.join("caller.ts"))
                .unwrap()
                .contains("loginUser")
        );
    }

    /// A shared name without disambiguation answers candidates; `--file`
    /// picks the declaration in that file only.
    #[test]
    fn op_rename_ambiguity_and_file_disambiguation() {
        let root = rename_root("rename-amb");
        std::fs::write(
            root.join("other.ts"),
            "export function loginUser(id: number): boolean { return id > 0; }\n",
        )
        .unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-qm", "other"]);
        let mut svc = Service::open(&root).unwrap();

        let resp = svc.handle(rename_req("loginUser", "authenticate"));
        assert!(resp.ok, "{resp:?}");
        let result = resp.result.unwrap();
        assert!(result["candidates"].as_array().unwrap().len() >= 2);

        let resp = svc.handle(Request::Rename {
            name: "loginUser".to_string(),
            new_name: "authenticate".to_string(),
            file: Some("other.ts".to_string()),
            uid: None,
            dry_run: false,
        });
        assert!(resp.ok, "{resp:?}");
        let other = std::fs::read_to_string(root.join("other.ts")).unwrap();
        assert!(other.contains("function authenticate("), "{other}");
        let login = std::fs::read_to_string(root.join("login.ts")).unwrap();
        assert!(login.contains("function loginUser("), "{login}");
    }

    /// Renaming a name that is not in the graph is an error, not a no-op.
    #[test]
    fn op_rename_unknown_name_errors() {
        let root = rename_root("rename-unknown");
        let mut svc = Service::open(&root).unwrap();
        let resp = svc.handle(rename_req("no_such_fn", "x"));
        assert!(!resp.ok, "{resp:?}");
        let resp = svc.handle(Request::Rename {
            name: "loginUser".to_string(),
            new_name: "x".to_string(),
            file: Some("missing.ts".to_string()),
            uid: None,
            dry_run: false,
        });
        assert!(!resp.ok, "{resp:?}");
        let resp = svc.handle(Request::Rename {
            name: "loginUser".to_string(),
            new_name: "x".to_string(),
            file: None,
            uid: Some("nope#1".to_string()),
            dry_run: false,
        });
        assert!(!resp.ok, "{resp:?}");
    }
}

//! `store.rs` — the `.pixel/history.db` SQLite store plus the shared fact
//! types every module in this crate returns.
//!
//! Schema (per PLAN.md Engine 2): `refs`, `commits`, `file_changes`, `hunks`,
//! `poison_paths`, `ingest_jobs`, `messages_fts` (FTS5, unicode61, NO prefix
//! index), and two contentless FTS5 trigram indexes (`diff_fts`, `path_fts`)
//! whose rowid is the `hunks` / `file_changes` row they index: a match is a
//! candidate rowid, verified against that row's text at fetch time, and a
//! stale rowid simply dies on join.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};

use pixel_git::GitRunner;

/// The on-disk history database file name, relative to the repo root.
pub const HISTORY_DB_FILE: &str = "history.db";

/// `diff_state` bit/values on `commits`:
///   0 = metadata present, diff not yet ingested
///   1 = diff ingested (hunks present)
///   2 = diff skipped with a recorded `skip_note` (never silent)
///   3 = diff evicted by budget (metadata retained, findable + live-probe)
pub const DIFF_STATE_PENDING: i64 = 0;
pub const DIFF_STATE_INDEXED: i64 = 1;
pub const DIFF_STATE_SKIPPED: i64 = 2;
pub const DIFF_STATE_EVICTED: i64 = 3;

/// `commits.reach` bitmask — "code that exists nowhere reachable" is a
/// filter flag, not a special case.
pub const REACH_BRANCH: i64 = 1;
pub const REACH_REMOTE: i64 = 2;
pub const REACH_TAG: i64 = 4;
pub const REACH_STASH: i64 = 8;
pub const REACH_REFLOG_ONLY: i64 = 16;

/// Default ceiling on the pages `history.db` holds (256 MiB), text and
/// indexes together. Past it the oldest diffs are evicted; commit metadata
/// always stays. `PIXEL_HISTORY_BUDGET_MB` overrides it.
pub const DEFAULT_HISTORY_BUDGET_BYTES: u64 = 268_435_456;

/// Default age window, in days, of the commits whose diff text is indexed.
/// Older commits keep their metadata (message, paths) but not their diff.
/// `PIXEL_HISTORY_WINDOW_DAYS` overrides it; `0` lifts the window.
pub const DEFAULT_DIFF_WINDOW_DAYS: u64 = 365;

/// `skip_note` of a commit whose diff is older than the window.
pub const SKIP_NOTE_OUTSIDE_WINDOW: &str = "outside-window";
/// `skip_note` of a commit whose diff did not fit the size budget.
pub const SKIP_NOTE_OVER_BUDGET: &str = "over-budget";

/// The on-disk schema version, stamped via `PRAGMA user_version`. Bump this
/// whenever the DDL changes. On open, a mismatch (or a pre-versioned DB that
/// already has rows) routes through the corrupt-rebuild path so every poisoned
/// DB self-heals on next open — no manual `rm` required.
///
/// History: 1 = trigram posting tables (`diff_grams`, `path_grams`), one row
/// per (gram, row) at ~52 bytes a posting, 14 times the text they indexed;
/// 2 = contentless FTS5 trigram indexes (`diff_fts`, `path_fts`), about 60
/// times smaller (pixel's own history: ~307 MB of postings to 4.8 MB), with
/// `auto_vacuum = INCREMENTAL` so eviction shrinks the file.
pub const FACTS_SCHEMA_VERSION: i64 = 2;

/// How much diff history the index keeps: a size budget over the whole
/// database and an age window over the commits whose diff is indexed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryLimits {
    /// Ceiling on the database's used pages, in bytes.
    pub budget_bytes: u64,
    /// Commits older than this many days keep only their metadata; `None`
    /// indexes every age.
    pub window_days: Option<u64>,
}

impl Default for HistoryLimits {
    fn default() -> Self {
        HistoryLimits {
            budget_bytes: DEFAULT_HISTORY_BUDGET_BYTES,
            window_days: Some(DEFAULT_DIFF_WINDOW_DAYS),
        }
    }
}

impl HistoryLimits {
    /// The limits from `PIXEL_HISTORY_BUDGET_MB` and
    /// `PIXEL_HISTORY_WINDOW_DAYS`, each falling back to its default.
    #[cfg_attr(test, mutants::skip)] // one-line adapter over the env; `from_values` is tested
    pub fn from_env() -> Self {
        Self::from_values(
            std::env::var("PIXEL_HISTORY_BUDGET_MB").ok().as_deref(),
            std::env::var("PIXEL_HISTORY_WINDOW_DAYS").ok().as_deref(),
        )
    }

    /// Parse the two settings: a budget in MiB and a window in days, where a
    /// window of `0` means no window. A missing or unparseable value keeps
    /// its default, so a typo never lifts a limit.
    pub fn from_values(budget_mb: Option<&str>, window_days: Option<&str>) -> Self {
        let defaults = Self::default();
        let budget_bytes = budget_mb
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map_or(defaults.budget_bytes, |mb| mb.saturating_mul(1_048_576));
        let window_days = match window_days.and_then(|v| v.trim().parse::<u64>().ok()) {
            Some(0) => None,
            Some(days) => Some(days),
            None => defaults.window_days,
        };
        HistoryLimits {
            budget_bytes,
            window_days,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FactsError {
    #[error("rusqlite: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("git: {0}")]
    Git(#[from] pixel_git::GitError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Msg(String),
}

pub type Result<T> = std::result::Result<T, FactsError>;

impl From<&str> for FactsError {
    fn from(s: &str) -> Self {
        FactsError::Msg(s.to_string())
    }
}

impl From<String> for FactsError {
    fn from(s: String) -> Self {
        FactsError::Msg(s)
    }
}

/// Every response carries this so callers know how much of history is covered
/// and whether the ingest thread has caught up to the current refs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IndexState {
    /// One of `"phase_a" | "phase_b" | "phase_c" | "fresh"`.
    pub phase: String,
    pub commits_indexed: u64,
    pub total_commits: u64,
    /// Fraction (0.0..=1.0) of commits whose diff text has been ingested.
    pub diff_indexed_pct: f64,
    /// True when ingest is caught up to the current refs (no pending work).
    pub fresh: bool,
    /// The on-disk schema version (PRAGMA user_version) this store was opened
    /// with — lets visibility report it.
    pub schema_version: i64,
    /// Commits whose diff text is not indexed because it was older than the
    /// window or did not fit the budget: their message and paths stay
    /// searchable, their diff content does not.
    #[serde(default)]
    pub diffs_evicted: u64,
    /// `committed_at` of the oldest commit whose diff is indexed: diff
    /// search sees nothing older. `None` when no diff is indexed.
    #[serde(default)]
    pub diff_coverage_since: Option<String>,
}

impl IndexState {
    pub fn empty() -> Self {
        IndexState {
            phase: "phase_a".into(),
            commits_indexed: 0,
            total_commits: 0,
            diff_indexed_pct: 0.0,
            fresh: false,
            schema_version: FACTS_SCHEMA_VERSION,
            diffs_evicted: 0,
            diff_coverage_since: None,
        }
    }
}

/// A single commit reference (shortened oid + subject + timestamp) used by
/// lifecycle and search responses.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommitRef {
    pub oid: String,
    pub at: String,
    pub subject: String,
}

/// The store: a SQLite connection to `.pixel/history.db` plus the git runner
/// used by the live-probe / present-at-HEAD paths that cannot be answered from
/// the index alone.
pub struct FactsStore {
    pub(crate) conn: Connection,
    root: PathBuf,
    runner: GitRunner,
    path: PathBuf,
}

impl FactsStore {
    /// Open (creating schema if needed) the history db at `root/.pixel/history.db`.
    /// Creates `.pixel` if absent. Self-healing: on corruption the db is
    /// removed and rebuilt from scratch (it is derived data, never
    /// load-bearing for correctness).
    pub fn open(root: &Path) -> Result<Self> {
        let root = root.to_path_buf();
        let pixel_dir = root.join(".pixel");
        std::fs::create_dir_all(&pixel_dir)?;
        // Ensure the .pixel directory is owner-only (0700) — it contains
        // history.db, index shards, and actions.jsonl, some of which may
        // carry fill values (passwords, OTPs) from flow replay.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&pixel_dir, std::fs::Permissions::from_mode(0o700));
        }
        let path = pixel_dir.join(HISTORY_DB_FILE);
        // Cross-process guard: without this, two concurrent pixel processes
        // (e.g. two agent sessions both running `pixel index --history`
        // against the same repo) can race the rebuild-decision + delete +
        // recreate sequence below — one process unlinks history.db/-wal/-shm
        // while the other still has a live WAL connection reading/writing
        // those exact inodes, which SQLite surfaces as "disk I/O error"
        // rather than a lock-contention error. Held only for this open
        // sequence, not for the store's lifetime, so it doesn't serialize
        // ongoing query traffic.
        let lock_path = pixel_dir.join(format!("{HISTORY_DB_FILE}.lock"));
        let lock_file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&lock_path)?;
        lock_file.lock()?;
        // Self-healing: rebuild on structural corruption OR a schema-version
        // mismatch OR a pre-versioned DB that already has rows. The db is
        // derived data, never load-bearing for correctness, so wiping it is
        // always safe — and this auto-heals every poisoned DB on next open
        // with no manual `rm` required.
        let rebuild = Self::needs_rebuild(&path).unwrap_or(true);
        let conn = if rebuild {
            Self::remove_db(&path);
            Self::open_conn(&path)?
        } else {
            match Self::open_conn(&path) {
                Ok(c) => c,
                Err(_) => {
                    Self::remove_db(&path);
                    Self::open_conn(&path)?
                }
            }
        };
        drop(lock_file);
        Ok(FactsStore {
            conn,
            runner: GitRunner::new(&root),
            path,
            root,
        })
    }

    /// Open the history db only when it already exists, for the read-only
    /// callers (`status`, `doctor`) that must report on it without creating
    /// it: an absent db means history was never asked for, and it is built
    /// on the first history query, not on a health check. `Ok(None)` when
    /// the file is absent.
    pub fn open_existing(root: &Path) -> Result<Option<Self>> {
        if !history_db_path(root).exists() {
            return Ok(None);
        }
        Self::open(root).map(Some)
    }

    /// True when the on-disk db at `path` must be rebuilt: the schema version
    /// (PRAGMA user_version) is missing or mismatched, or the db is
    /// pre-versioned (user_version 0) but already holds rows (a poisoned DB
    /// written by an older build). An empty pre-versioned db is fine — it just
    /// gets stamped on open.
    fn needs_rebuild(path: &Path) -> Result<bool> {
        if !path.exists() {
            return Ok(false);
        }
        // READ_WRITE (not READ_ONLY) so a WAL-mode db with a live -wal file is
        // readable; the file exists so CREATE is unnecessary.
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        // Security: verify the _pixel_marker table exists and has the correct
        // value. A db planted by a hostile repo (git add -f .pixel/history.db)
        // will not have this marker and is wiped before any of its data is
        // trusted or parsed.
        let has_marker: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='_pixel_marker'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if has_marker == 0 {
            return Ok(true); // No marker → foreign db → rebuild (wipe).
        }
        let marker_val: String = conn
            .query_row(
                "SELECT val FROM _pixel_marker WHERE key='created_by'",
                [],
                |r| r.get(0),
            )
            .unwrap_or_default();
        if marker_val != "pixel-facts" {
            return Ok(true); // Wrong marker → foreign db → rebuild (wipe).
        }
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap_or(0);
        if version == FACTS_SCHEMA_VERSION {
            return Ok(false);
        }
        if version == 0 {
            // Pre-versioned. Rebuild only if it already has rows; an empty one
            // is stamped in place on open.
            let has_rows: i64 = conn
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='commits'",
                    [],
                    |r| r.get(0),
                )
                .unwrap_or(0);
            if has_rows == 0 {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn remove_db(path: &Path) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{}", path.display(), suffix));
        }
    }

    fn open_conn(path: &Path) -> Result<Connection> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        // Before the first table exists, or it has no effect: freed pages
        // are then tracked so `incremental_vacuum` can hand them back after
        // an eviction, instead of the file keeping its largest size forever.
        conn.pragma_update(None, "auto_vacuum", "INCREMENTAL")?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        conn.execute_batch(DDL)?;
        conn.pragma_update(None, "user_version", FACTS_SCHEMA_VERSION)?;
        Ok(conn)
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    pub(crate) fn conn_mut(&mut self) -> &mut Connection {
        &mut self.conn
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn runner(&self) -> &GitRunner {
        &self.runner
    }

    /// Bytes held by the database's used pages (its page count minus the
    /// free list), WAL content included: what the size budget measures.
    pub fn used_bytes(&self) -> Result<u64> {
        let pragma = |name: &str| -> Result<i64> {
            Ok(self
                .conn
                .query_row(&format!("PRAGMA {name}"), [], |r| r.get(0))?)
        };
        let used_pages = pragma("page_count")? - pragma("freelist_count")?;
        Ok(u64::try_from(used_pages * pragma("page_size")?).unwrap_or(0))
    }

    /// Checkpoint the WAL file to keep -wal bounded in size during long-running sessions.
    pub fn wal_checkpoint(&self) -> Result<()> {
        let _ = self.conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE);");
        Ok(())
    }

    /// Snapshot of how much of history is indexed. Cheap: a few counts.
    pub fn index_state(&self) -> IndexState {
        let (total, indexed, _skipped, pending_diff) = self.diff_counts();
        let phase = self.current_phase();
        // diff_indexed_pct = fraction of non-structurally-skipped commits that
        // have their diff text ingested (phase C progress).
        let diff_total = indexed + pending_diff;
        let diff_indexed_pct = if diff_total == 0 {
            0.0
        } else {
            (indexed as f64) / (diff_total as f64)
        };
        IndexState {
            phase: phase.to_string(),
            commits_indexed: total as u64,
            total_commits: total as u64,
            diff_indexed_pct,
            fresh: pending_diff == 0 && self.phase_a_fresh() && self.phase_b_done(),
            schema_version: FACTS_SCHEMA_VERSION,
            diffs_evicted: self.count_in_state(DIFF_STATE_EVICTED) as u64,
            diff_coverage_since: self
                .conn
                .query_row(
                    "SELECT min(committed_at) FROM commits WHERE diff_state = ?1",
                    [DIFF_STATE_INDEXED],
                    |r| r.get(0),
                )
                .unwrap_or(None),
        }
    }

    fn count_in_state(&self, state: i64) -> i64 {
        self.conn
            .query_row(
                "SELECT count(*) FROM commits WHERE diff_state = ?1",
                [state],
                |r| r.get(0),
            )
            .unwrap_or(0)
    }

    fn current_phase(&self) -> &'static str {
        if !self.phase_a_fresh() {
            return "phase_a";
        }
        if !self.phase_b_done() {
            return "phase_b";
        }
        let pending_diff: i64 = self
            .conn
            .query_row(
                "SELECT count(*) FROM commits WHERE diff_state = ?1",
                [DIFF_STATE_PENDING],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if pending_diff > 0 { "phase_c" } else { "fresh" }
    }

    fn phase_a_done(&self) -> bool {
        let status: Option<String> = self
            .conn
            .query_row(
                "SELECT status FROM ingest_jobs WHERE phase = 'A'",
                [],
                |r| r.get(0),
            )
            .ok();
        matches!(status.as_deref(), Some("done"))
    }

    /// Phase A is 'done' AND its recorded refs hash still matches the current
    /// refs. A ref move since the last phase-A run makes us stale (phase_a
    /// again) even though the ingest_jobs row says 'done' — this is what fixes
    /// the frozen-at-commit-11 class permanently.
    fn phase_a_fresh(&self) -> bool {
        if !self.phase_a_done() {
            return false;
        }
        match self.current_refs_hash() {
            Ok(current) => {
                let stored: Option<String> = self
                    .conn
                    .query_row(
                        "SELECT ref_hash FROM ingest_jobs WHERE phase = 'A'",
                        [],
                        |r| r.get(0),
                    )
                    .ok();
                stored.as_deref() == Some(current.as_str())
            }
            Err(_) => false,
        }
    }

    /// xxh3 hash of the current refs state (`for-each-ref` + HEAD). Stored at
    /// phase-A completion; a differing hash means refs moved and phase A must
    /// re-run.
    pub(crate) fn current_refs_hash(&self) -> Result<String> {
        let mut buf = Vec::new();
        let refs = self
            .runner
            .run(&["for-each-ref", "--format=%(refname)%00%(objectname)"])?;
        buf.extend_from_slice(&refs);
        let head = self.runner.run(&["rev-parse", "HEAD"])?;
        buf.extend_from_slice(&head);
        let h = xxhash_rust::xxh3::xxh3_64(&buf);
        Ok(format!("{h:016x}"))
    }

    fn phase_b_done(&self) -> bool {
        let status: Option<String> = self
            .conn
            .query_row(
                "SELECT status FROM ingest_jobs WHERE phase = 'B'",
                [],
                |r| r.get(0),
            )
            .ok();
        matches!(status.as_deref(), Some("done"))
    }

    fn diff_counts(&self) -> (i64, i64, i64, i64) {
        let total = self
            .conn
            .query_row("SELECT count(*) FROM commits", [], |r| r.get(0))
            .unwrap_or(0);
        let indexed = self
            .conn
            .query_row(
                "SELECT count(*) FROM commits WHERE diff_state = ?1",
                [DIFF_STATE_INDEXED],
                |r| r.get(0),
            )
            .unwrap_or(0);
        let skipped = self
            .conn
            .query_row(
                "SELECT count(*) FROM commits WHERE diff_state = ?1",
                [DIFF_STATE_SKIPPED],
                |r| r.get(0),
            )
            .unwrap_or(0);
        let pending_diff = self
            .conn
            .query_row(
                "SELECT count(*) FROM commits WHERE diff_state = ?1",
                [DIFF_STATE_PENDING],
                |r| r.get(0),
            )
            .unwrap_or(0);
        (total, indexed, skipped, pending_diff)
    }

    /// Close the connection (flushes WAL).
    pub fn close(self) {
        let _ = self.conn.pragma_update(None, "wal_checkpoint", "TRUNCATE");
        drop(self.conn);
    }
}

/// The schema. Commit messages use FTS5 with unicode61 (no prefix index);
/// diff and path text use contentless FTS5 trigram indexes.
const DDL: &str = r#"
CREATE TABLE IF NOT EXISTS refs (
  ref    TEXT PRIMARY KEY,
  oid    TEXT NOT NULL,
  kind   TEXT NOT NULL,
  indexed_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS commits (
  id INTEGER PRIMARY KEY,
  oid TEXT NOT NULL UNIQUE,
  parents TEXT NOT NULL DEFAULT '',
  author TEXT NOT NULL DEFAULT '',
  committed_at TEXT NOT NULL DEFAULT '',
  message TEXT NOT NULL DEFAULT '',
  reach INTEGER NOT NULL DEFAULT 0,
  diff_state INTEGER NOT NULL DEFAULT 0,
  skip_note TEXT
);
CREATE INDEX IF NOT EXISTS commits_oid ON commits (oid);
CREATE INDEX IF NOT EXISTS commits_diff_state ON commits (diff_state);
CREATE INDEX IF NOT EXISTS commits_committed_at ON commits (committed_at);

CREATE TABLE IF NOT EXISTS file_changes (
  id INTEGER PRIMARY KEY,
  commit_id INTEGER NOT NULL REFERENCES commits (id),
  path TEXT NOT NULL,
  status TEXT NOT NULL,
  old_path TEXT,
  UNIQUE (commit_id, path)
);
CREATE INDEX IF NOT EXISTS file_changes_path ON file_changes (path);
CREATE INDEX IF NOT EXISTS file_changes_commit ON file_changes (commit_id);
CREATE INDEX IF NOT EXISTS file_changes_status ON file_changes (status);

CREATE TABLE IF NOT EXISTS hunks (
  id INTEGER PRIMARY KEY,
  commit_id INTEGER NOT NULL REFERENCES commits (id),
  path TEXT NOT NULL,
  added TEXT NOT NULL DEFAULT '',
  removed TEXT NOT NULL DEFAULT '',
  truncated INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS hunks_commit ON hunks (commit_id);

CREATE TABLE IF NOT EXISTS poison_paths (
  path TEXT PRIMARY KEY,
  reason TEXT NOT NULL,
  learned_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS ingest_jobs (
  id INTEGER PRIMARY KEY,
  phase TEXT NOT NULL UNIQUE,
  cursor TEXT,
  status TEXT NOT NULL DEFAULT 'pending',
  ref_hash TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL
);

-- Transient: reach bitmask accumulation keyed by oid during enumeration.
CREATE TABLE IF NOT EXISTS reach_map (
  oid TEXT PRIMARY KEY,
  bits INTEGER NOT NULL DEFAULT 0
);

-- FTS5 for commit messages only (unicode61, NO prefix index per PLAN.md).
CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
  message,
  content='commits',
  content_rowid='id',
  tokenize="unicode61 tokenchars '_'"
);

-- Trigram candidate indexes. `diff_fts` rowid = hunks.id over its
-- added/removed text, `path_fts` rowid = file_changes.id over its path.
-- Contentless (the text lives in hunks / file_changes, never twice),
-- detail=none (no positions: a match is "every trigram present", verified
-- against the text afterwards), contentless_delete so eviction can drop a
-- hunk's entries by rowid.
CREATE VIRTUAL TABLE IF NOT EXISTS diff_fts USING fts5(
  text,
  content='',
  contentless_delete=1,
  detail=none,
  tokenize='trigram'
);

CREATE VIRTUAL TABLE IF NOT EXISTS path_fts USING fts5(
  path,
  content='',
  contentless_delete=1,
  detail=none,
  tokenize='trigram'
);

-- Marker table: proves this db was created by pixel, not planted by a
-- hostile repo. Checked in needs_rebuild; a db missing this marker is
-- treated as foreign and wiped.
CREATE TABLE IF NOT EXISTS _pixel_marker (
  key TEXT PRIMARY KEY,
  val TEXT NOT NULL
);
INSERT OR IGNORE INTO _pixel_marker (key, val) VALUES ('created_by', 'pixel-facts');
"#;

/// `root/.pixel/history.db`.
pub fn history_db_path(root: &Path) -> PathBuf {
    root.join(".pixel").join(HISTORY_DB_FILE)
}

/// Shorten an oid to the conventional 12-char display form.
pub fn short_oid(oid: &str) -> String {
    let trimmed = oid.trim();
    if trimmed.len() > 12 {
        trimmed[..12].to_string()
    } else {
        trimmed.to_string()
    }
}

/// Extract the subject line (first line) of a commit message.
pub fn subject_of(message: &str) -> &str {
    message.lines().next().unwrap_or("").trim()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{commit, git, two_commit_repo};

    /// The refs hash is what tells phase A that history moved; it must be
    /// stable while refs stand still and change on any new commit or ref.
    #[test]
    fn current_refs_hash_is_stable_until_a_ref_or_head_moves() {
        let (dir, _, _) = two_commit_repo();
        let store = FactsStore::open(dir.path()).unwrap();
        let before = store.current_refs_hash().unwrap();
        assert_eq!(before.len(), 16);
        assert!(before.chars().all(|c| c.is_ascii_hexdigit()), "{before}");
        assert_eq!(store.current_refs_hash().unwrap(), before);
        commit(dir.path(), &[("b.txt", b"b\n")], "third");
        let after_commit = store.current_refs_hash().unwrap();
        assert_ne!(after_commit, before);
        git(dir.path(), &["tag", "v1"]);
        assert_ne!(store.current_refs_hash().unwrap(), after_commit);
    }

    #[test]
    fn history_limits_parse_budget_and_window_and_keep_defaults_on_garbage() {
        let d = HistoryLimits::default();
        assert_eq!(d.budget_bytes, 268_435_456);
        assert_eq!(d.window_days, Some(365));
        assert_eq!(HistoryLimits::from_values(None, None), d);
        assert_eq!(
            HistoryLimits::from_values(Some(" 64 "), Some("30")),
            HistoryLimits {
                budget_bytes: 67_108_864,
                window_days: Some(30),
            }
        );
        assert_eq!(
            HistoryLimits::from_values(None, Some("0")).window_days,
            None,
            "0 lifts the window"
        );
        assert_eq!(
            HistoryLimits::from_values(Some("lots"), Some("-1")),
            d,
            "a typo never lifts a limit"
        );
        assert_eq!(
            HistoryLimits::from_values(Some(&u64::MAX.to_string()), None).budget_bytes,
            u64::MAX,
            "saturates"
        );
    }

    /// `status` and `doctor` report on history without creating it.
    #[test]
    fn open_existing_never_creates_the_history_db() {
        let (dir, _, _) = two_commit_repo();
        let db = history_db_path(dir.path());
        assert!(FactsStore::open_existing(dir.path()).unwrap().is_none());
        assert!(!db.exists(), "a read-only check must not create {db:?}");
        drop(FactsStore::open(dir.path()).unwrap());
        let store = FactsStore::open_existing(dir.path())
            .unwrap()
            .expect("present");
        assert_eq!(store.path(), db.as_path());
    }

    /// Freed pages must be reclaimable, or an eviction never shrinks the
    /// file; and the budget measures used pages, not the free list.
    #[test]
    fn a_new_db_reclaims_incrementally_and_used_bytes_excludes_free_pages() {
        let (dir, _, _) = two_commit_repo();
        let store = FactsStore::open(dir.path()).unwrap();
        let pragma = |name: &str| -> i64 {
            store
                .conn()
                .query_row(&format!("PRAGMA {name}"), [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(pragma("auto_vacuum"), 2, "INCREMENTAL");
        assert_eq!(
            store.used_bytes().unwrap(),
            (pragma("page_count") * pragma("page_size")) as u64
        );
        store
            .conn()
            .execute_batch(
                "CREATE TABLE filler (b BLOB);
                 WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 40)
                 INSERT INTO filler SELECT randomblob(3000) FROM n;
                 DELETE FROM filler;",
            )
            .unwrap();
        let free = pragma("freelist_count");
        assert!(free >= 20, "{free}");
        assert_eq!(
            store.used_bytes().unwrap(),
            ((pragma("page_count") - free) * pragma("page_size")) as u64
        );
    }
}

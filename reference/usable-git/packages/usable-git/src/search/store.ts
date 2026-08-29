import { Database } from "bun:sqlite";
import { createHash } from "node:crypto";
import { chmodSync, mkdirSync, rmSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

export type SearchStoreOptions = { stateRoot?: string };

export type SearchStore = {
  db: Database;
  path: string;
  close: () => void;
};

const SCHEMA_VERSION = 1;

const defaultStateRoot = () =>
  process.env.XDG_STATE_HOME
    ? join(process.env.XDG_STATE_HOME, "usable-git")
    : join(homedir(), ".local", "state", "usable-git");

const digest = (value: string) => createHash("sha256").update(value).digest("hex");

// The index is derived data keyed by the repository common dir, so linked
// worktrees share one index and a corrupt file is always safe to rebuild.
export const searchStorePath = (commonDir: string, options: SearchStoreOptions = {}) =>
  join(
    options.stateRoot ?? defaultStateRoot(),
    "search",
    digest(commonDir),
    `index-v${SCHEMA_VERSION}.sqlite`,
  );

const DDL = `
CREATE TABLE IF NOT EXISTS meta (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS indexed_refs (
  ref TEXT PRIMARY KEY,
  tip_oid TEXT NOT NULL,
  indexed_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS commits (
  id INTEGER PRIMARY KEY,
  oid TEXT NOT NULL UNIQUE,
  parents TEXT NOT NULL,
  author_name TEXT NOT NULL,
  author_email TEXT NOT NULL,
  authored_at TEXT NOT NULL,
  committer_name TEXT NOT NULL,
  committer_email TEXT NOT NULL,
  committed_at TEXT NOT NULL,
  message TEXT NOT NULL,
  files_touched INTEGER NOT NULL DEFAULT 0,
  diff_state INTEGER NOT NULL DEFAULT 0,
  skip_note TEXT
);
CREATE INDEX IF NOT EXISTS commits_diff_state ON commits (diff_state);
CREATE TABLE IF NOT EXISTS file_changes (
  id INTEGER PRIMARY KEY,
  commit_id INTEGER NOT NULL REFERENCES commits (id),
  path TEXT NOT NULL,
  status TEXT NOT NULL,
  old_path TEXT
);
CREATE INDEX IF NOT EXISTS file_changes_path ON file_changes (path);
CREATE INDEX IF NOT EXISTS file_changes_commit ON file_changes (commit_id);
CREATE TABLE IF NOT EXISTS hunk_text (
  id INTEGER PRIMARY KEY,
  commit_id INTEGER NOT NULL REFERENCES commits (id),
  path TEXT NOT NULL,
  added TEXT NOT NULL DEFAULT '',
  removed TEXT NOT NULL DEFAULT '',
  truncated INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS hunk_text_commit ON hunk_text (commit_id);
CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
  message,
  content='commits',
  content_rowid='id',
  tokenize="unicode61 tokenchars '_'",
  prefix='2 3'
);
CREATE VIRTUAL TABLE IF NOT EXISTS paths_fts USING fts5(
  path,
  content='file_changes',
  content_rowid='id',
  tokenize='unicode61',
  prefix='2 3'
);
CREATE VIRTUAL TABLE IF NOT EXISTS diffs_fts USING fts5(
  added,
  removed,
  content='hunk_text',
  content_rowid='id',
  tokenize="unicode61 tokenchars '_'",
  prefix='2 3'
);
`;

const removeStoreFiles = (path: string) => {
  for (const suffix of ["", "-wal", "-shm"]) {
    rmSync(`${path}${suffix}`, { force: true });
  }
};

const openAt = (path: string): Database => {
  const database = new Database(path, { create: true });
  database.exec("PRAGMA journal_mode = WAL");
  database.exec("PRAGMA synchronous = NORMAL");
  database.exec("PRAGMA busy_timeout = 5000");
  const version = (database.query("PRAGMA user_version").get() as { user_version: number })
    .user_version;
  if (version !== 0 && version !== SCHEMA_VERSION) {
    database.close();
    throw new Error(`unsupported search index schema version ${version}`);
  }
  database.exec(DDL);
  if (version === 0) database.exec(`PRAGMA user_version = ${SCHEMA_VERSION}`);
  // Probe the pages the DDL alone may not touch so corruption is caught here.
  database.query("SELECT count(*) FROM commits").get();
  database.query("SELECT count(*) FROM indexed_refs").get();
  return database;
};

// Self-healing open: the index is derived data, so any corruption or schema
// mismatch is resolved by deleting the files and rebuilding from git — it
// must never surface to the caller as an error.
export const openSearchStore = (
  commonDir: string,
  options: SearchStoreOptions = {},
): SearchStore => {
  const path = searchStorePath(commonDir, options);
  mkdirSync(join(path, ".."), { recursive: true, mode: 0o700 });
  let db: Database;
  try {
    db = openAt(path);
  } catch {
    removeStoreFiles(path);
    db = openAt(path);
  }
  try {
    chmodSync(path, 0o600);
  } catch {
    // Permissions are best-effort on platforms that reject chmod.
  }
  return { db, path, close: () => db.close() };
};

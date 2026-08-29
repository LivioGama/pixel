import { createGitRunner, type GitRunner } from "../git/runner.ts";
import type { SearchStore } from "./store.ts";

export const DEFAULT_BUILD_BUDGET_MS = 8_000;
const INGEST_MAX_OUTPUT_BYTES = 8 * 1_048_576;
const PHASE_A_BATCH = 200;
const PHASE_B_BATCH = 50;
const ENUMERATION_MAX_COUNT = 100_000;
const FILE_TEXT_CAP_BYTES = 32 * 1_024;
const COMMIT_TEXT_CAP_BYTES = 256 * 1_024;

// Paths whose diff text is machine noise: recorded as skipped, never indexed.
const SKIPPED_DIFF_PATH = (path: string) => {
  const segments = path.split("/");
  const name = segments[segments.length - 1] ?? path;
  return (
    segments.some((segment) =>
      segment === "node_modules" || segment === "vendor" || segment === "dist",
    ) ||
    [
      "bun.lock",
      "bun.lockb",
      "package-lock.json",
      "yarn.lock",
      "pnpm-lock.yaml",
      "Cargo.lock",
      "composer.lock",
      "Gemfile.lock",
      "poetry.lock",
      "uv.lock",
    ].includes(name) ||
    /\.min\.[^.]+$/.test(name) ||
    name.endsWith(".map") ||
    name.endsWith(".snap")
  );
};

export type PhaseACommit = {
  oid: string;
  parents: string[];
  authorName: string;
  authorEmail: string;
  authoredAt: string;
  committerName: string;
  committerEmail: string;
  committedAt: string;
  message: string;
  changes: Array<{ status: string; path: string; oldPath?: string }>;
};

// Record layout probed against real git output: each record opens with \x1e,
// carries nine NUL-separated fields, then name-status entries where the first
// status token arrives with a leading newline.
export const parsePhaseA = (output: string): PhaseACommit[] => {
  const records = output.split("\x1e").filter((record) => record.length > 0);
  const commits: PhaseACommit[] = [];
  for (const record of records) {
    const fields = record.split("\0");
    const [
      oid,
      parents,
      authorName,
      authorEmail,
      authoredAt,
      committerName,
      committerEmail,
      committedAt,
      message,
    ] = fields;
    if (
      oid === undefined ||
      parents === undefined ||
      authorName === undefined ||
      authorEmail === undefined ||
      authoredAt === undefined ||
      committerName === undefined ||
      committerEmail === undefined ||
      committedAt === undefined ||
      message === undefined
    ) {
      throw new Error("Malformed NUL-delimited git log record");
    }
    const changes: PhaseACommit["changes"] = [];
    let index = 9;
    while (index < fields.length) {
      const rawStatus = fields[index]?.replace(/^\s+/, "") ?? "";
      if (rawStatus === "") {
        index += 1;
        continue;
      }
      const status = rawStatus[0]!;
      if (status === "R" || status === "C") {
        const oldPath = fields[index + 1];
        const path = fields[index + 2];
        if (oldPath === undefined || path === undefined) {
          throw new Error("Malformed rename entry in git log record");
        }
        changes.push({ status, path, oldPath });
        index += 3;
      } else {
        const path = fields[index + 1];
        if (path === undefined) throw new Error("Malformed status entry in git log record");
        changes.push({ status, path });
        index += 2;
      }
    }
    commits.push({
      oid,
      parents: parents ? parents.split(" ") : [],
      authorName,
      authorEmail,
      authoredAt,
      committerName,
      committerEmail,
      committedAt,
      message,
      changes,
    });
  }
  return commits;
};

export type PhaseBFile = {
  path: string;
  added: string;
  removed: string;
  truncated: boolean;
  binary: boolean;
};

export type PhaseBCommit = { oid: string; files: PhaseBFile[] };

// `git show -U0 --format=%x1e%H` output: records split on \x1e, first line is
// the oid, then plain unified diff text per file.
export const parsePhaseB = (output: string): PhaseBCommit[] => {
  const records = output.split("\x1e").filter((record) => record.length > 0);
  const commits: PhaseBCommit[] = [];
  for (const record of records) {
    const newline = record.indexOf("\n");
    const oid = (newline === -1 ? record : record.slice(0, newline)).trim();
    const body = newline === -1 ? "" : record.slice(newline + 1);
    const files: PhaseBFile[] = [];
    let current: PhaseBFile | undefined;
    for (const line of body.split("\n")) {
      const header = /^diff --git a\/(.*) b\/(.*)$/.exec(line);
      if (header) {
        current = { path: header[2]!, added: "", removed: "", truncated: false, binary: false };
        files.push(current);
        continue;
      }
      if (!current) continue;
      if (line.startsWith("Binary files ") || line === "GIT binary patch") {
        current.binary = true;
        continue;
      }
      if (current.binary) continue;
      if (line.startsWith("+++") || line.startsWith("---")) continue;
      const target = line.startsWith("+")
        ? ("added" as const)
        : line.startsWith("-")
          ? ("removed" as const)
          : undefined;
      if (!target) continue;
      if (current.added.length + current.removed.length >= FILE_TEXT_CAP_BYTES) {
        current.truncated = true;
        continue;
      }
      current[target] += `${line.slice(1)}\n`;
    }
    commits.push({ oid, files });
  }
  return commits;
};

export type IngestCounters = {
  indexedCommits: number;
  pendingCommits: number;
  pendingDiffCommits: number;
  skippedDiffCommits: number;
};

export type IngestOutcome = {
  tips: Array<{ ref: string; oid: string }>;
  counters: IngestCounters;
};

export type IngestOptions = {
  budgetMs?: number;
  now?: () => number;
  runner?: GitRunner;
};

export const createIngestRunner = () =>
  createGitRunner({ maxOutputBytes: INGEST_MAX_OUTPUT_BYTES });

const refreshTips = async (runner: GitRunner, root: string) => {
  const refs = await runner.runChecked(root, [
    "for-each-ref",
    "--format=%(refname)%00%(objectname)",
    "refs/heads",
  ]);
  const tips: Array<{ ref: string; oid: string }> = [];
  for (const line of refs.stdout.split("\n")) {
    if (!line) continue;
    const [ref, oid] = line.split("\0");
    if (ref && oid) tips.push({ ref, oid });
  }
  const head = await runner.run(root, ["rev-parse", "--verify", "--quiet", "HEAD"]);
  if (head.exitCode === 0) tips.push({ ref: "HEAD", oid: head.stdout.trim() });
  return tips;
};

const enumeratePending = async (
  runner: GitRunner,
  root: string,
  store: SearchStore,
  tips: Array<{ ref: string; oid: string }>,
) => {
  const tipOids = [...new Set(tips.map(({ oid }) => oid))];
  if (tipOids.length === 0) return { oids: [], complete: true };
  const recorded = (store.db.query("SELECT tip_oid FROM indexed_refs").all() as Array<{
    tip_oid: string;
  }>).map(({ tip_oid }) => tip_oid);
  const negation = [...new Set(recorded)].filter((oid) => !tipOids.includes(oid));
  const base = ["rev-list", "--reverse", `--max-count=${ENUMERATION_MAX_COUNT}`, ...tipOids];
  let result = negation.length > 0
    ? await runner.run(root, [...base, "--not", ...negation])
    : await runner.run(root, base);
  // Recorded tips can vanish after a rebase plus gc; correctness never depends
  // on negation because commit inserts are idempotent by oid.
  if (result.exitCode !== 0 && negation.length > 0) {
    result = await runner.run(root, base);
  }
  if (result.exitCode !== 0) {
    throw new Error(result.stderr.trim() || "git rev-list failed during search indexing");
  }
  const known = new Set(
    (store.db.query("SELECT oid FROM commits").all() as Array<{ oid: string }>).map(
      ({ oid }) => oid,
    ),
  );
  const oids = result.stdout.split("\n").filter((oid) => oid && !known.has(oid));
  const complete = !result.outputLimitExceeded &&
    result.stdout.split("\n").filter(Boolean).length < ENUMERATION_MAX_COUNT;
  return { oids, complete };
};

const insertPhaseABatch = (store: SearchStore, commits: PhaseACommit[]) => {
  const insertCommit = store.db.query(
    `INSERT OR IGNORE INTO commits (
       oid, parents, author_name, author_email, authored_at,
       committer_name, committer_email, committed_at, message,
       files_touched, diff_state, skip_note
     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)`,
  );
  const selectId = store.db.query("SELECT id FROM commits WHERE oid = ?1");
  const insertMessageFts = store.db.query(
    "INSERT INTO messages_fts (rowid, message) VALUES (?1, ?2)",
  );
  const insertChange = store.db.query(
    "INSERT INTO file_changes (commit_id, path, status, old_path) VALUES (?1, ?2, ?3, ?4)",
  );
  const insertPathFts = store.db.query("INSERT INTO paths_fts (rowid, path) VALUES (?1, ?2)");
  const transaction = store.db.transaction(() => {
    for (const commit of commits) {
      const isMerge = commit.parents.length > 1;
      const inserted = insertCommit.run(
        commit.oid,
        commit.parents.join(" "),
        commit.authorName,
        commit.authorEmail,
        commit.authoredAt,
        commit.committerName,
        commit.committerEmail,
        commit.committedAt,
        commit.message,
        commit.changes.length,
        isMerge ? 2 : 0,
        isMerge ? "merge" : null,
      );
      if (inserted.changes === 0) continue;
      const id = (selectId.get(commit.oid) as { id: number }).id;
      insertMessageFts.run(id, commit.message);
      for (const change of commit.changes) {
        const changed = insertChange.run(id, change.path, change.status, change.oldPath ?? null);
        insertPathFts.run(changed.lastInsertRowid as number, change.path);
      }
    }
  });
  transaction.immediate();
};

const ingestPhaseA = async (
  runner: GitRunner,
  root: string,
  store: SearchStore,
  oids: string[],
) => {
  const result = await runner.runChecked(root, [
    "log",
    "-z",
    "--no-walk=unsorted",
    "--format=%x1e%H%x00%P%x00%an%x00%ae%x00%aI%x00%cn%x00%ce%x00%cI%x00%B",
    "--name-status",
    "--end-of-options",
    ...oids,
  ]);
  insertPhaseABatch(store, parsePhaseA(result.stdout));
};

const insertPhaseBCommit = (store: SearchStore, commit: PhaseBCommit) => {
  const selectId = store.db.query("SELECT id FROM commits WHERE oid = ?1");
  const row = selectId.get(commit.oid) as { id: number } | null;
  if (!row) return;
  const insertHunk = store.db.query(
    `INSERT INTO hunk_text (commit_id, path, added, removed, truncated)
     VALUES (?1, ?2, ?3, ?4, ?5)`,
  );
  const insertDiffFts = store.db.query(
    "INSERT INTO diffs_fts (rowid, added, removed) VALUES (?1, ?2, ?3)",
  );
  const markState = store.db.query(
    "UPDATE commits SET diff_state = ?2, skip_note = ?3 WHERE id = ?1",
  );
  const transaction = store.db.transaction(() => {
    let commitBytes = 0;
    let overCap = false;
    for (const file of commit.files) {
      if (file.binary || SKIPPED_DIFF_PATH(file.path)) continue;
      if (commitBytes >= COMMIT_TEXT_CAP_BYTES) {
        overCap = true;
        break;
      }
      const hunk = insertHunk.run(
        row.id,
        file.path,
        file.added,
        file.removed,
        file.truncated ? 1 : 0,
      );
      insertDiffFts.run(hunk.lastInsertRowid as number, file.added, file.removed);
      commitBytes += file.added.length + file.removed.length;
    }
    markState.run(row.id, 1, overCap ? "over-cap" : null);
  });
  transaction.immediate();
};

const markCommitSkipped = (store: SearchStore, oid: string, note: string) => {
  store.db
    .query("UPDATE commits SET diff_state = 2, skip_note = ?2 WHERE oid = ?1 AND diff_state = 0")
    .run(oid, note);
};

const ingestPhaseB = async (
  runner: GitRunner,
  root: string,
  store: SearchStore,
  oids: string[],
): Promise<void> => {
  if (oids.length === 0) return;
  const result = await runner.run(root, [
    "show",
    "-U0",
    "--no-color",
    "--format=%x1e%H",
    "--diff-filter=AMDRT",
    "--find-renames",
    "--end-of-options",
    ...oids,
  ]);
  if (result.outputLimitExceeded) {
    if (oids.length === 1) {
      // A single commit whose diff exceeds the runner cap stays searchable by
      // metadata; the skip is recorded, never silent.
      markCommitSkipped(store, oids[0]!, "over-cap");
      return;
    }
    const half = Math.ceil(oids.length / 2);
    await ingestPhaseB(runner, root, store, oids.slice(0, half));
    await ingestPhaseB(runner, root, store, oids.slice(half));
    return;
  }
  if (result.exitCode !== 0) {
    if (oids.length === 1) {
      // A gc-pruned oid (post-rebase forensic row) can no longer produce a
      // diff; record the skip so the index never wedges on it.
      markCommitSkipped(store, oids[0]!, "unresolvable");
      return;
    }
    const half = Math.ceil(oids.length / 2);
    await ingestPhaseB(runner, root, store, oids.slice(0, half));
    await ingestPhaseB(runner, root, store, oids.slice(half));
    return;
  }
  for (const commit of parsePhaseB(result.stdout)) {
    insertPhaseBCommit(store, commit);
  }
};

const recordTips = (store: SearchStore, tips: Array<{ ref: string; oid: string }>) => {
  const upsert = store.db.query(
    `INSERT INTO indexed_refs (ref, tip_oid, indexed_at) VALUES (?1, ?2, ?3)
     ON CONFLICT (ref) DO UPDATE SET tip_oid = excluded.tip_oid, indexed_at = excluded.indexed_at`,
  );
  const clear = store.db.query("DELETE FROM indexed_refs WHERE ref NOT IN (SELECT value FROM json_each(?1))");
  const transaction = store.db.transaction(() => {
    for (const tip of tips) upsert.run(tip.ref, tip.oid, new Date().toISOString());
    clear.run(JSON.stringify(tips.map(({ ref }) => ref)));
  });
  transaction.immediate();
};

const storeCounters = (store: SearchStore, pendingCommits: number): IngestCounters => {
  const count = (sql: string) => (store.db.query(sql).get() as { n: number }).n;
  return {
    indexedCommits: count("SELECT count(*) AS n FROM commits"),
    pendingCommits,
    pendingDiffCommits: count("SELECT count(*) AS n FROM commits WHERE diff_state = 0"),
    skippedDiffCommits: count("SELECT count(*) AS n FROM commits WHERE diff_state = 2"),
  };
};

// Lazy per-call build: refresh tips, ingest metadata (Phase A) before diff
// text (Phase B), commit per batch so progress survives process exit, and
// stop at the budget — remaining work is reported, never hidden.
export const ingestSearchIndex = async (
  store: SearchStore,
  root: string,
  options: IngestOptions = {},
): Promise<IngestOutcome> => {
  const runner = options.runner ?? createIngestRunner();
  const now = options.now ?? (() => performance.now());
  const budgetMs = options.budgetMs ?? DEFAULT_BUILD_BUDGET_MS;
  const deadline = now() + budgetMs;

  const tips = await refreshTips(runner, root);
  const pending = await enumeratePending(runner, root, store, tips);
  let ingested = 0;
  while (ingested < pending.oids.length && now() < deadline) {
    const batch = pending.oids.slice(ingested, ingested + PHASE_A_BATCH);
    await ingestPhaseA(runner, root, store, batch);
    ingested += batch.length;
  }
  const phaseAComplete = ingested === pending.oids.length && pending.complete;
  if (phaseAComplete) recordTips(store, tips);

  while (phaseAComplete && now() < deadline) {
    const batch = (store.db
      .query("SELECT oid FROM commits WHERE diff_state = 0 ORDER BY id LIMIT ?1")
      .all(PHASE_B_BATCH) as Array<{ oid: string }>).map(({ oid }) => oid);
    if (batch.length === 0) break;
    await ingestPhaseB(runner, root, store, batch);
  }

  return {
    tips,
    counters: storeCounters(store, pending.oids.length - ingested),
  };
};

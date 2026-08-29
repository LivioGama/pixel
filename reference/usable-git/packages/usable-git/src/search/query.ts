import type {
  SearchHit,
  SearchLifecycle,
} from "../contracts/v1/search.ts";
import type { GitRunner } from "../git/runner.ts";
import type { SearchStore } from "./store.ts";

export type SearchScope = "message" | "path" | "diff" | "all";
export type QueryPass = "and" | "or";

// Raw user text never reaches MATCH: every unit is double-quoted, which
// neutralizes all FTS5 operators (NEAR/AND/OR/NOT/:/^/*), and only a
// deliberate trailing prefix star is appended outside the quotes.
export const sanitizeQuery = (raw: string): { units: string[]; prefixLast: boolean } | null => {
  const phrases: string[] = [];
  const remainder = raw.replace(/"([^"]*)"/g, (_, phrase: string) => {
    const cleaned = phrase.replace(/"/g, "").trim();
    if (cleaned.length > 1) phrases.push(cleaned);
    return " ";
  });
  const terms = remainder
    .split(/[^A-Za-z0-9_]+/)
    .filter((term) => term.length > 1);
  const units = [...phrases, ...terms];
  if (units.length === 0) return null;
  const last = units[units.length - 1]!;
  const prefixLast = phrases.length === 0 && last.length >= 3;
  return { units, prefixLast };
};

export const matchExpression = (
  sanitized: { units: string[]; prefixLast: boolean },
  pass: QueryPass,
): string => {
  const quoted = sanitized.units.map((unit, index) => {
    const escaped = `"${unit.replace(/"/g, "")}"`;
    return sanitized.prefixLast && index === sanitized.units.length - 1
      ? `${escaped}*`
      : escaped;
  });
  return quoted.join(pass === "and" ? " AND " : " OR ");
};

const KIND_WEIGHTS = {
  message: 3.0,
  path: 2.0,
  "diff-add": 1.2,
  "diff-del": 1.0,
} as const;

const PER_SCOPE_CANDIDATES = 200;

type CommitColumns = {
  id: number;
  oid: string;
  committed_at: string;
  author_name: string;
  message: string;
  files_touched: number;
};

type Candidate = {
  hit: SearchHit;
  score: number;
  committedAt: string;
};

const subjectOf = (message: string) => message.split("\n", 1)[0] ?? "";

// Commit ids follow rev-list --reverse insertion, so id order is history
// order. The epsilon (≤1e-3) is invisible next to any genuine relevance
// difference (normalized steps are ≥1/199 of a kind weight) but breaks true
// relevance ties toward recency — "was X dropped" wants the newest commit
// touching X first.
const RECENCY_EPSILON = 1e-3;

const recencyEpsilon = (id: number, maxId: number) =>
  maxId > 0 ? (id / maxId) * RECENCY_EPSILON : 0;

// Cross-scope scoring: `score = kindWeight * normalizedRelevance + recencyEpsilon`.
//
// Relevance is the summed occurrence count of the query units in the matched
// text, min-max normalized within each scope's result set (all-equal → 1.0).
// Raw bm25 was rejected twice, on measurement, not taste:
//   1. Raw magnitudes are not comparable across FTS tables — measured spread
//      ~1e-6 on messages vs ~1.0 on diffs for the same corpus and query.
//   2. Min-max-normalized -bm25 inverts the plan's "was svelte dropped"
//      regression fixture: both matching messages contain "svelte" exactly
//      once, yet bm25's document-length normalization scores them
//      -1.0476190476190476e-6 ("feat: svelte app shell", 4 tokens) vs
//      -7.586206896551725e-7 ("feat: single-pass HTML generation engine,
//      replace Svelte", 8 tokens); min-max amplifies that pure length-norm
//      delta to normalized 1.0 vs 0.0, final scores 3.00025 vs 0.001 — the
//      older incidental commit wins over the plan-mandated answer commit.
// Occurrence counting keeps the genuine relevance signal (a commit mentioning
// the terms three times outranks a single incidental mention regardless of
// recency) while scoring equal-mention documents as true ties, which the
// recency epsilon then breaks toward the newest commit. bm25 still orders
// each scope's candidate pool selection (ORDER BY rank LIMIT 200).
const occurrenceCount = (haystack: string, needle: string) => {
  if (!needle) return 0;
  return haystack.split(needle).length - 1;
};

const relevanceOf = (text: string, units: string[]) => {
  const lower = text.toLowerCase();
  return units.reduce((sum, unit) => sum + occurrenceCount(lower, unit.toLowerCase()), 0);
};

type ScopeRow = CommitColumns & { text: string; snip?: string; path?: string };

const toCandidates = (
  rows: ScopeRow[],
  matchKind: SearchHit["matchKind"],
  units: string[],
  maxId: number,
): Candidate[] => {
  if (rows.length === 0) return [];
  const raw = rows.map((row) => relevanceOf(row.text, units));
  const min = Math.min(...raw);
  const max = Math.max(...raw);
  return rows.map((row, index) => ({
    hit: {
      oid: row.oid.slice(0, 12),
      at: row.committed_at,
      subject: subjectOf(row.message),
      author: row.author_name,
      matchKind,
      ...(row.path ? { path: row.path } : {}),
      ...(row.snip ? { snippet: row.snip } : {}),
      filesTouched: row.files_touched,
    },
    score:
      KIND_WEIGHTS[matchKind] * (max === min ? 1 : (raw[index]! - min) / (max - min)) +
      recencyEpsilon(row.id, maxId),
    committedAt: row.committed_at,
  }));
};

const maxCommitId = (store: SearchStore) =>
  ((store.db.query("SELECT max(id) AS m FROM commits").get() as { m: number | null }).m ?? 0);

const messageCandidates = (
  store: SearchStore,
  match: string,
  units: string[],
  maxId: number,
): Candidate[] =>
  toCandidates(
    (store.db
      .query(
        `SELECT c.id, c.oid, c.committed_at, c.author_name, c.message, c.files_touched,
                c.message AS text, bm25(messages_fts) AS rank,
                snippet(messages_fts, 0, '«', '»', '…', 10) AS snip
         FROM messages_fts
         JOIN commits c ON c.id = messages_fts.rowid
         WHERE messages_fts MATCH ?1
         ORDER BY rank, c.id DESC LIMIT ${PER_SCOPE_CANDIDATES}`,
      )
      .all(match)) as ScopeRow[],
    "message",
    units,
    maxId,
  );

const pathCandidates = (
  store: SearchStore,
  match: string,
  units: string[],
  maxId: number,
): Candidate[] =>
  toCandidates(
    (store.db
      .query(
        `SELECT c.id, c.oid, c.committed_at, c.author_name, c.message, c.files_touched,
                f.path AS text, bm25(paths_fts) AS rank, f.path AS path
         FROM paths_fts
         JOIN file_changes f ON f.id = paths_fts.rowid
         JOIN commits c ON c.id = f.commit_id
         WHERE paths_fts MATCH ?1
         ORDER BY rank, c.id DESC LIMIT ${PER_SCOPE_CANDIDATES}`,
      )
      .all(match)) as ScopeRow[],
    "path",
    units,
    maxId,
  );

const diffCandidates = (
  store: SearchStore,
  match: string,
  column: "added" | "removed",
  units: string[],
  maxId: number,
): Candidate[] =>
  toCandidates(
    (store.db
      .query(
        `SELECT c.id, c.oid, c.committed_at, c.author_name, c.message, c.files_touched,
                h.${column} AS text, bm25(diffs_fts) AS rank, h.path AS path,
                snippet(diffs_fts, ${column === "added" ? 0 : 1}, '«', '»', '…', 10) AS snip
         FROM diffs_fts
         JOIN hunk_text h ON h.id = diffs_fts.rowid
         JOIN commits c ON c.id = h.commit_id
         WHERE diffs_fts MATCH ?1
         ORDER BY rank, c.id DESC LIMIT ${PER_SCOPE_CANDIDATES}`,
      )
      .all(`${column}: (${match})`)) as ScopeRow[],
    column === "added" ? "diff-add" : "diff-del",
    units,
    maxId,
  );

const candidatesFor = (
  store: SearchStore,
  scope: SearchScope,
  match: string,
  units: string[],
): Candidate[] => {
  const maxId = maxCommitId(store);
  return [
    ...(scope === "message" || scope === "all"
      ? messageCandidates(store, match, units, maxId)
      : []),
    ...(scope === "path" || scope === "all" ? pathCandidates(store, match, units, maxId) : []),
    ...(scope === "diff" || scope === "all"
      ? [
          ...diffCandidates(store, match, "added", units, maxId),
          ...diffCandidates(store, match, "removed", units, maxId),
        ]
      : []),
  ];
};

export type RankedSearch = {
  candidates: Candidate[];
  pass: QueryPass;
};

// Pass 1 requires every unit (implicit AND); pass 2 falls back to OR only
// when the strict pass returns nothing, and the chosen pass is part of the
// pagination snapshot so cursors never mix ranking modes.
export const rankedSearch = (
  store: SearchStore,
  scope: SearchScope,
  sanitized: { units: string[]; prefixLast: boolean },
  forcedPass?: QueryPass,
): RankedSearch => {
  const runPass = (pass: QueryPass) => {
    const merged = candidatesFor(store, scope, matchExpression(sanitized, pass), sanitized.units);
    const best = new Map<string, Candidate>();
    for (const candidate of merged) {
      const key = `${candidate.hit.oid}:${candidate.hit.matchKind}`;
      const existing = best.get(key);
      if (!existing || candidate.score > existing.score) best.set(key, candidate);
    }
    return [...best.values()].sort((left, right) =>
      right.score - left.score ||
      right.committedAt.localeCompare(left.committedAt) ||
      left.hit.oid.localeCompare(right.hit.oid),
    );
  };
  if (forcedPass) return { candidates: runPass(forcedPass), pass: forcedPass };
  const strict = runPass("and");
  if (strict.length > 0 || sanitized.units.length < 2) {
    return { candidates: strict, pass: "and" };
  }
  return { candidates: runPass("or"), pass: "or" };
};

type LifecycleRow = { oid: string; committed_at: string; message: string };

const reference = (row: LifecycleRow) => ({
  oid: row.oid.slice(0, 12),
  at: row.committed_at,
  subject: subjectOf(row.message),
});

export const pathLifecycle = async (
  store: SearchStore,
  root: string,
  runner: GitRunner,
  path: string,
): Promise<SearchLifecycle> => {
  const touches = store.db
    .query(
      `SELECT c.oid, c.committed_at, c.message, f.status
       FROM file_changes f JOIN commits c ON c.id = f.commit_id
       WHERE f.path = ?1 OR f.old_path = ?1
       ORDER BY c.committed_at ASC, c.id ASC`,
    )
    .all(path) as Array<LifecycleRow & { status: string }>;
  const removals = touches.filter(({ status }) => status === "D");
  const lastRemoval = removals[removals.length - 1];
  const lastTouch = touches[touches.length - 1];
  const removedIn = lastRemoval && lastTouch && lastRemoval.oid === lastTouch.oid
    ? lastRemoval
    : undefined;
  const presence = await runner.run(root, ["cat-file", "-e", `HEAD:${path}`]);
  return {
    ...(touches[0] ? { firstSeen: reference(touches[0]) } : {}),
    ...(lastTouch ? { lastChanged: reference(lastTouch) } : {}),
    ...(removedIn ? { removedIn: reference(removedIn) } : {}),
    presentAtHead: presence.exitCode === 0,
    totalTouches: touches.length,
  };
};

export const tokenLifecycle = async (
  store: SearchStore,
  root: string,
  runner: GitRunner,
  token: string,
): Promise<SearchLifecycle | null> => {
  const sanitized = sanitizeQuery(token);
  if (!sanitized) return null;
  const exact = { units: sanitized.units, prefixLast: false };
  const match = matchExpression(exact, "and");
  const rowsFor = (column: "added" | "removed") =>
    store.db
      .query(
        `SELECT DISTINCT c.oid, c.committed_at, c.message
         FROM diffs_fts
         JOIN hunk_text h ON h.id = diffs_fts.rowid
         JOIN commits c ON c.id = h.commit_id
         WHERE diffs_fts MATCH ?1
         ORDER BY c.committed_at ASC, c.id ASC`,
      )
      .all(`${column}: (${match})`) as LifecycleRow[];
  const additions = rowsFor("added");
  const removals = rowsFor("removed");
  const lastRemoval = removals[removals.length - 1];
  const lastAddition = additions[additions.length - 1];
  const removedIn = lastRemoval &&
      (!lastAddition || lastRemoval.committed_at >= lastAddition.committed_at)
    ? lastRemoval
    : undefined;
  const presence = await runner.run(root, [
    "grep",
    "-I",
    "-l",
    "--fixed-strings",
    "--end-of-options",
    token,
    "HEAD",
  ]);
  const touched = new Set([...additions, ...removals].map(({ oid }) => oid));
  return {
    ...(additions[0] ? { firstSeen: reference(additions[0]) } : {}),
    ...(lastAddition ? { lastChanged: reference(lastAddition) } : {}),
    ...(removedIn ? { removedIn: reference(removedIn) } : {}),
    presentAtHead: presence.exitCode === 0,
    totalTouches: touched.size,
  };
};

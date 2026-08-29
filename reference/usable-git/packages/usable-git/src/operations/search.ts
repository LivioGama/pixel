import { decodeCursor, digestValue, encodeCursor } from "../contracts/cursor.ts";
import {
  searchRequestSchema,
  searchResultSchema,
  type SearchHit,
  type SearchRequest,
  type SearchResult,
} from "../contracts/v1/search.ts";
import { UsableGitError } from "../errors.ts";
import { requireWorktreeRepository } from "../git/repository.ts";
import {
  createIngestRunner,
  ingestSearchIndex,
  type IngestOutcome,
} from "../search/ingest.ts";
import {
  pathLifecycle,
  rankedSearch,
  sanitizeQuery,
  tokenLifecycle,
  type QueryPass,
} from "../search/query.ts";
import { openSearchStore } from "../search/store.ts";

export type {
  SearchHit,
  SearchRequest,
  SearchResult,
} from "../contracts/v1/search.ts";

export type SearchOptions = {
  stateRoot?: string;
  buildBudgetMs?: number;
  now?: () => number;
};

const LIFECYCLE_CITATION_LIMIT = 5;

const indexProof = (outcome: IngestOutcome) => ({
  state:
    outcome.counters.pendingCommits === 0 && outcome.counters.pendingDiffCommits === 0
      ? ("fresh" as const)
      : ("partial" as const),
  ...outcome.counters,
});

// Snapshot binds a cursor to the exact indexed tips plus the ranking pass, so
// pages stay consistent over immutable commits and go STALE_STATE the moment
// new history lands between pages.
const snapshotFor = (outcome: IngestOutcome, pass: QueryPass) =>
  digestValue({
    tips: [...new Set(outcome.tips.map(({ oid }) => oid))].sort(),
    pass,
  });

const boundHits = (
  candidates: Array<{ hit: SearchHit }>,
  offset: number,
  limit: number,
  byteCap: number,
) => {
  const hits: SearchHit[] = [];
  let bytes = 0;
  let index = offset;
  while (index < candidates.length && hits.length < limit) {
    const hit = candidates[index]!.hit;
    const hitBytes = Buffer.byteLength(JSON.stringify(hit));
    if (bytes + hitBytes > byteCap) {
      if (hits.length === 0) {
        throw new UsableGitError(
          "INVALID_INPUT",
          "A single search hit exceeds the response byte cap",
        );
      }
      break;
    }
    hits.push(hit);
    bytes += hitBytes;
    index += 1;
  }
  return { hits, nextOffset: index, hasMore: index < candidates.length };
};

export const search = async (
  input: SearchRequest,
  options: SearchOptions = {},
): Promise<SearchResult> => {
  const request = searchRequestSchema.parse(input);
  const repository = await requireWorktreeRepository(request.repoPath);
  const runner = createIngestRunner();

  const requestDigest = digestValue({
    repoPath: repository.root,
    target: request.target,
    limit: request.limit,
    byteCap: request.byteCap,
  });
  const cursorPayload = request.cursor
    ? await decodeCursor(request.cursor, "search", options)
    : undefined;
  if (cursorPayload && cursorPayload.requestDigest !== requestDigest) {
    throw new UsableGitError("INVALID_INPUT", "Cursor belongs to a different search request");
  }
  if (cursorPayload && typeof cursorPayload.offset !== "number") {
    throw new UsableGitError("INVALID_INPUT", "Invalid search cursor offset");
  }

  const store = openSearchStore(repository.commonDir, options);
  try {
    const outcome = await ingestSearchIndex(store, repository.root, {
      runner,
      ...(options.buildBudgetMs !== undefined ? { budgetMs: options.buildBudgetMs } : {}),
      ...(options.now ? { now: options.now } : {}),
    });
    const headTip = outcome.tips.find(({ ref }) => ref === "HEAD");
    const head = headTip
      ? { kind: "oid" as const, oid: headTip.oid }
      : { kind: "unborn" as const };

    if (request.target.kind === "lifecycle") {
      const lifecycle = request.target.path !== undefined
        ? await pathLifecycle(store, repository.root, runner, request.target.path)
        : await tokenLifecycle(store, repository.root, runner, request.target.token!);
      if (!lifecycle) {
        throw new UsableGitError(
          "INVALID_INPUT",
          "Lifecycle token contains no searchable characters",
        );
      }
      const citationQuery = request.target.path ?? request.target.token!;
      const sanitized = sanitizeQuery(citationQuery);
      const citations = sanitized
        ? rankedSearch(store, "all", sanitized).candidates
            .slice(0, LIFECYCLE_CITATION_LIMIT)
            .map(({ hit }) => hit)
        : [];
      return searchResultSchema.parse({
        head,
        index: indexProof(outcome),
        hits: citations,
        lifecycle,
      });
    }

    const sanitized = sanitizeQuery(request.target.query);
    if (!sanitized) {
      throw new UsableGitError(
        "INVALID_INPUT",
        "Search query contains no searchable characters",
      );
    }
    // A resumed page replays the pass recorded in its snapshot so ranking
    // never silently switches between AND and OR mid-pagination.
    const resumedPass = cursorPayload
      ? ((["and", "or"] as const).find(
          (pass) => snapshotFor(outcome, pass) === cursorPayload.snapshot,
        ) ?? null)
      : undefined;
    if (resumedPass === null) {
      throw new UsableGitError(
        "STALE_STATE",
        "Indexed history moved after the cursor was issued; restart pagination",
      );
    }
    const ranked = rankedSearch(store, request.target.scope, sanitized, resumedPass);
    const snapshot = snapshotFor(outcome, ranked.pass);
    const offset = typeof cursorPayload?.offset === "number" ? cursorPayload.offset : 0;
    const page = boundHits(ranked.candidates, offset, request.limit, request.byteCap);
    return searchResultSchema.parse({
      head,
      index: indexProof(outcome),
      hits: page.hits,
      ...(page.hasMore
        ? {
            nextCursor: await encodeCursor({
              operation: "search",
              requestDigest,
              snapshot,
              offset: page.nextOffset,
            }, options),
          }
        : {}),
    });
  } finally {
    store.close();
  }
};

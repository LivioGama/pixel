import { decodeCursor, digestValue, encodeCursor } from "../contracts/cursor.ts";
import { historyRequestSchema, type HistoryRequest } from "../contracts/v1.ts";
import {
  historyResultSchema,
  type CompactHistoryCommit,
  type HistoryCommit,
  type HistoryResult,
} from "../contracts/v1/history.ts";
import { UsableGitError } from "../errors.ts";
import { requireWorktreeRepository } from "../git/repository.ts";
import { git } from "../git/runner.ts";

export type { HistoryCommit, HistoryResult } from "../contracts/v1/history.ts";

export type HistoryOptions = { stateRoot?: string };

const compactCommit = (commit: HistoryCommit): CompactHistoryCommit => ({
  oid: commit.oid.slice(0, 12),
  subject: commit.message.split("\n", 1)[0] ?? "",
  author: commit.author.name,
  at: commit.committedAt,
  ...(commit.parents.length > 1 ? { merge: true as const } : {}),
});

export const parseHistory = (output: string): HistoryCommit[] => {
  const fields = output.split("\0");
  const commits: HistoryCommit[] = [];
  let index = 0;
  while (index < fields.length) {
    while (fields[index] === "") index += 1;
    if (index >= fields.length) break;
    const oid = fields[index++];
    const parents = fields[index++];
    const name = fields[index++];
    const email = fields[index++];
    const authoredAt = fields[index++];
    const committerName = fields[index++];
    const committerEmail = fields[index++];
    const committedAt = fields[index++];
    const signatureStatus = fields[index++];
    const message = fields[index++];
    if (
      oid === undefined ||
      parents === undefined ||
      name === undefined ||
      email === undefined ||
      authoredAt === undefined ||
      committerName === undefined ||
      committerEmail === undefined ||
      committedAt === undefined ||
      signatureStatus === undefined ||
      message === undefined
    ) {
      throw new Error("Malformed NUL-delimited git history");
    }
    commits.push({
      oid,
      parents: parents ? parents.split(" ") : [],
      author: { name, email },
      committer: { name: committerName, email: committerEmail },
      authoredAt,
      committedAt,
      signatureStatus,
      message,
    });
  }
  return commits;
};

export const history = async (
  input: HistoryRequest,
  options: HistoryOptions = {},
): Promise<HistoryResult> => {
  const request = historyRequestSchema.parse(input);
  const repository = await requireWorktreeRepository(request.repoPath);
  const requestDigest = digestValue({
    repoPath: repository.root,
    ref: request.ref,
    limit: request.limit,
    detail: request.detail,
    byteCap: request.byteCap ?? null,
  });
  const cursor = request.cursor
    ? await decodeCursor(request.cursor, "history", options)
    : undefined;
  if (cursor && cursor.requestDigest !== requestDigest) {
    throw new UsableGitError("INVALID_INPUT", "Cursor belongs to a different history request");
  }
  if (cursor && typeof cursor.offset !== "number") {
    throw new UsableGitError("INVALID_INPUT", "Invalid history cursor offset");
  }
  const skip = typeof cursor?.offset === "number" ? cursor.offset : 0;
  const exists = await git.run(repository.root, ["rev-parse", "--verify", "--quiet", request.ref]);
  if (exists.exitCode === 1 && request.ref === "HEAD") {
    return historyResultSchema.parse({ head: { kind: "unborn" }, commits: [] });
  }
  if (exists.exitCode !== 0) throw new Error(exists.stderr.trim() || `Unknown revision: ${request.ref}`);
  const snapshot = exists.stdout.trim();
  if (cursor && cursor.snapshot !== snapshot) {
    throw new UsableGitError("STALE_STATE", "History ref changed after the cursor was issued");
  }

  const result = await git.runChecked(repository.root, [
    "log",
    `--max-count=${request.limit + 1}`,
    `--skip=${skip}`,
    "-z",
    "--format=%H%x00%P%x00%an%x00%ae%x00%aI%x00%cn%x00%ce%x00%cI%x00%G?%x00%B%x00",
    "--end-of-options",
    request.ref,
  ]);
  const parsed = parseHistory(result.stdout);
  const hasMoreByCount = parsed.length > request.limit;
  const candidates = parsed
    .slice(0, request.limit)
    .map((commit) => (request.detail === "full" ? commit : compactCommit(commit)));
  const byteCap = request.byteCap ?? 256_000;
  const commits: Array<HistoryCommit | CompactHistoryCommit> = [];
  let bytes = 0;
  for (const commit of candidates) {
    const commitBytes = Buffer.byteLength(JSON.stringify(commit));
    if (bytes + commitBytes > byteCap) {
      if (commits.length === 0) throw new Error("A commit exceeds the history response byte cap");
      break;
    }
    commits.push(commit);
    bytes += commitBytes;
  }
  const hasMore = hasMoreByCount || commits.length < candidates.length;
  return historyResultSchema.parse({
    head: { kind: "oid", oid: snapshot },
    commits,
    ...(hasMore
      ? {
          nextCursor: await encodeCursor({
            operation: "history",
            requestDigest,
            snapshot,
            offset: skip + commits.length,
          }, options),
        }
      : {}),
  });
};

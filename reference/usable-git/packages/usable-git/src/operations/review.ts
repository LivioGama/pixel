import { resolve } from "node:path";
import { decodeCursor, digestValue, encodeCursor } from "../contracts/cursor.ts";
import { reviewRequestSchema, type ReviewRequest } from "../contracts/v1.ts";
import {
  reviewResultSchema,
  type ReviewItem,
  type ReviewResult,
  type ReviewScope,
} from "../contracts/v1/review.ts";
import { UsableGitError } from "../errors.ts";
import { fingerprintChange } from "../git/fingerprint.ts";
import { paginatePatches, patchStatistics } from "../git/patches.ts";
import { validateLiteralFiles } from "../git/paths.ts";
import { requireWorktreeRepository } from "../git/repository.ts";
import { git } from "../git/runner.ts";
import { parsePorcelainV2, type StatusChange } from "../git/status.ts";

export type { ReviewItem, ReviewResult, ReviewScope } from "../contracts/v1/review.ts";

export type ReviewOptions = { stateRoot?: string };

type Cursor = { item: number; character: number };

const diffItem = async (
  root: string,
  change: StatusChange,
  scope: Exclude<ReviewScope, "untracked">,
): Promise<ReviewItem> => {
  const args = ["diff", "--no-ext-diff", "--no-textconv", "--binary"];
  if (scope === "staged") args.push("--cached");
  args.push("--", change.path);
  const result = await git.runChecked(root, args);
  const binary = result.stdout.includes("GIT binary patch") || result.stdout.includes("Binary files ");
  return {
    scope,
    path: change.path,
    ...(change.originalPath === undefined ? {} : { originalPath: change.originalPath }),
    patch: result.stdout,
    binary,
    ...patchStatistics(result.stdout),
    truncated: false,
  };
};

const untrackedItem = async (root: string, path: string): Promise<ReviewItem> => {
  const bytes = new Uint8Array(await Bun.file(resolve(root, path)).arrayBuffer());
  const binary = bytes.includes(0);
  const contents = binary ? "" : new TextDecoder().decode(bytes);
  const patch = binary
    ? `Binary untracked file ${JSON.stringify(path)}\n`
    : `--- /dev/null\n+++ ${JSON.stringify(path)}\n${contents
        .split("\n")
        .map((line, index, lines) => (index === lines.length - 1 && line === "" ? "" : `+${line}`))
        .filter(Boolean)
        .join("\n")}\n`;
  return {
    scope: "untracked",
    path,
    patch,
    binary,
    additions: binary ? 0 : contents.split("\n").filter((_, index, lines) => index < lines.length - 1 || lines[index] !== "").length,
    deletions: 0,
    truncated: false,
  };
};

const paginate = async (
  items: ReviewItem[],
  cursor: Cursor,
  byteCap: number,
  requestDigest: string,
  snapshot: string,
  options: ReviewOptions,
) => {
  const page = paginatePatches(items, cursor, byteCap);
  return {
    items: page.items,
    bytes: page.bytes,
    ...(page.next
      ? {
          nextCursor: await encodeCursor({
            operation: "review",
            requestDigest,
            snapshot,
            offset: page.next,
          }, options),
        }
      : {}),
  };
};

const changedInIndex = ({ indexStatus, conflicted }: StatusChange) =>
  !conflicted && ![".", " ", "?", "!"].includes(indexStatus);

const changedInWorktree = ({ worktreeStatus, conflicted }: StatusChange) =>
  !conflicted && ![".", " ", "?", "!"].includes(worktreeStatus);

export const review = async (
  input: ReviewRequest,
  options: ReviewOptions = {},
): Promise<ReviewResult> => {
  const request = reviewRequestSchema.parse(input);
  const repository = await requireWorktreeRepository(request.repoPath);
  const files = request.files
    ? await validateLiteralFiles(repository.root, request.files)
    : undefined;
  const requestDigest = digestValue({
    repoPath: repository.root,
    files: files ?? null,
    byteCap: request.byteCap,
  });
  const cursorPayload = request.cursor
    ? await decodeCursor(request.cursor, "review", options)
    : undefined;
  if (cursorPayload && cursorPayload.requestDigest !== requestDigest) {
    throw new UsableGitError("INVALID_INPUT", "Cursor belongs to a different review request");
  }
  const args = ["status", "--porcelain=v2", "-z", "--branch", "--untracked-files=all"];
  if (files) args.push("--", ...files);
  const statusResult = await git.runChecked(repository.root, args);
  const parsed = parsePorcelainV2(statusResult.stdout);
  const changes = parsed.changes;
  const snapshot = digestValue({
    branch: parsed.branch,
    changes: await Promise.all(
      changes.map(async (change) => ({
        ...change,
        fingerprint: await fingerprintChange(repository.root, change),
      })),
    ),
  });
  if (cursorPayload && cursorPayload.snapshot !== snapshot) {
    throw new UsableGitError("STALE_STATE", "Repository changed after the review cursor was issued");
  }
  const items: ReviewItem[] = [];

  for (const change of changes) {
    if (changedInIndex(change)) items.push(await diffItem(repository.root, change, "staged"));
    if (changedInWorktree(change)) items.push(await diffItem(repository.root, change, "unstaged"));
    if (files && change.kind === "untracked" && files.includes(change.path)) {
      items.push(await untrackedItem(repository.root, change.path));
    }
  }

  const offset = cursorPayload?.offset ?? { item: 0, character: 0 };
  if (
    typeof offset !== "object" ||
    !("item" in offset) ||
    !("character" in offset)
  ) {
    throw new UsableGitError("INVALID_INPUT", "Invalid review cursor offset");
  }
  return reviewResultSchema.parse(
    await paginate(items, offset as Cursor, request.byteCap, requestDigest, snapshot, options),
  );
};

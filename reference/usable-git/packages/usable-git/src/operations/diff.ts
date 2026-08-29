import { decodeCursor, digestValue, encodeCursor } from "../contracts/cursor.ts";
import {
  diffRequestSchema,
  diffResultSchema,
  type DiffItem,
  type DiffRequest,
  type DiffResult,
} from "../contracts/v1/diff.ts";
import { UsableGitError } from "../errors.ts";
import { paginatePatches, patchStatistics, type PatchCursor } from "../git/patches.ts";
import { requireWorktreeRepository } from "../git/repository.ts";
import { git } from "../git/runner.ts";

export type { DiffItem, DiffRequest, DiffResult } from "../contracts/v1/diff.ts";

export type DiffOptions = { stateRoot?: string };

const EMPTY_TREE = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

// Historical paths may not exist in the worktree, so diff validates literal
// path syntax only — no globs, no pathspec magic, no escapes.
const validateLiteralPathSyntax = (files: string[]) => {
  const unique = [...new Set(files)];
  if (unique.length !== files.length) {
    throw new UsableGitError("INVALID_PATH", "Duplicate file paths are unsupported");
  }
  for (const file of unique) {
    if (
      !file ||
      file === "." ||
      file.startsWith("/") ||
      file.startsWith(":") ||
      file.startsWith("-") ||
      /[*?[\]]/.test(file) ||
      file.split(/[\\/]/).includes("..")
    ) {
      throw new UsableGitError(
        "INVALID_PATH",
        `Invalid literal file path: ${JSON.stringify(file)}`,
      );
    }
  }
  return unique;
};

// Exact oids only — resolve the abbreviation, refuse anything else.
const resolveCommit = async (root: string, oid: string) => {
  const resolved = await git.run(root, [
    "rev-parse",
    "--verify",
    "--quiet",
    "--end-of-options",
    `${oid}^{commit}`,
  ]);
  if (resolved.exitCode !== 0) {
    throw new UsableGitError("INVALID_INPUT", "Unknown commit object id", { oid });
  }
  return resolved.stdout.trim();
};

const firstParent = async (root: string, commit: string) => {
  const parents = await git.runChecked(root, [
    "rev-list",
    "--parents",
    "-n",
    "1",
    commit,
  ]);
  const [, ...parentOids] = parents.stdout.trim().split(/\s+/);
  return parentOids[0] ?? null;
};

const changedPaths = async (
  root: string,
  base: string,
  target: string,
  files: string[] | undefined,
) => {
  const args = ["diff", "--name-only", "-z", "--no-ext-diff", base, target];
  if (files) args.push("--", ...files);
  const result = await git.runChecked(root, args);
  return result.stdout.split("\0").filter(Boolean);
};

const patchFor = async (
  root: string,
  base: string,
  target: string,
  path: string,
): Promise<DiffItem> => {
  const result = await git.runChecked(root, [
    "diff",
    "--no-ext-diff",
    "--no-textconv",
    "--binary",
    base,
    target,
    "--",
    path,
  ]);
  const binary =
    result.stdout.includes("GIT binary patch") || result.stdout.includes("Binary files ");
  return {
    path,
    patch: result.stdout,
    binary,
    ...patchStatistics(result.stdout),
    truncated: false,
  };
};

export const diff = async (
  input: DiffRequest,
  options: DiffOptions = {},
): Promise<DiffResult> => {
  const request = diffRequestSchema.parse(input);
  const repository = await requireWorktreeRepository(request.repoPath);
  const files = request.files ? validateLiteralPathSyntax(request.files) : undefined;

  const target = await resolveCommit(
    repository.root,
    request.target.kind === "range" ? request.target.targetOid : request.target.oid,
  );
  const base = request.target.kind === "range"
    ? await resolveCommit(repository.root, request.target.baseOid)
    : (await firstParent(repository.root, target)) ?? EMPTY_TREE;

  const requestDigest = digestValue({
    repoPath: repository.root,
    base,
    target,
    files: files ?? null,
    byteCap: request.byteCap,
  });
  const cursorPayload = request.cursor
    ? await decodeCursor(request.cursor, "diff", options)
    : undefined;
  if (cursorPayload && cursorPayload.requestDigest !== requestDigest) {
    throw new UsableGitError("INVALID_INPUT", "Cursor belongs to a different diff request");
  }
  // Commits are immutable, so the snapshot is the resolved pair itself.
  const snapshot = digestValue({ base, target });
  if (cursorPayload && cursorPayload.snapshot !== snapshot) {
    throw new UsableGitError("STALE_STATE", "Diff cursor belongs to different commits");
  }

  const paths = await changedPaths(repository.root, base, target, files);
  const items: DiffItem[] = [];
  for (const path of paths) {
    items.push(await patchFor(repository.root, base, target, path));
  }

  const offset = cursorPayload?.offset ?? { item: 0, character: 0 };
  if (typeof offset !== "object" || !("item" in offset) || !("character" in offset)) {
    throw new UsableGitError("INVALID_INPUT", "Invalid diff cursor offset");
  }
  const page = paginatePatches(items, offset as PatchCursor, request.byteCap);
  return diffResultSchema.parse({
    base,
    target,
    items: page.items,
    bytes: page.bytes,
    ...(page.next
      ? {
          nextCursor: await encodeCursor({
            operation: "diff",
            requestDigest,
            snapshot,
            offset: page.next,
          }, options),
        }
      : {}),
  });
};

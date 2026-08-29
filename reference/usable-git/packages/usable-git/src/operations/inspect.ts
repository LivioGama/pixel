import { z } from "zod";
import { inspectRequestSchema, type InspectRequest } from "../contracts/v1.ts";
import {
  inspectResultSchema,
  type InspectResult,
} from "../contracts/v1/inspect.ts";
import {
  branchStatusSchema,
  objectIdSchema,
  operationHeadSchema,
  resultPathListSchema,
  resultPathSchema,
  resultRepositorySchema,
} from "../contracts/v1/result-primitives.ts";
import { access } from "node:fs/promises";
import { join } from "node:path";
import { fingerprintChange } from "../git/fingerprint.ts";
import { validateLiteralFiles } from "../git/paths.ts";
import { requireWorktreeRepository } from "../git/repository.ts";
import { git } from "../git/runner.ts";
import { parsePorcelainV2, type StatusChange } from "../git/status.ts";
import { createSnapshotStore } from "../mutations/snapshot-store.ts";

export type { InspectResult } from "../contracts/v1/inspect.ts";

export type InspectOptions = {
  stateRoot?: string;
  recordSnapshot?: boolean;
};

// Internal rich shape consumed by publish/ship and the mutation machinery.
// The wire contract (`inspectResultSchema`) is a compact projection of this.
const detailedChangeSchema = z
  .object({
    path: resultPathSchema,
    originalPath: resultPathSchema.optional(),
    indexStatus: z.string().length(1),
    worktreeStatus: z.string().length(1),
    indexOid: objectIdSchema.optional(),
    kind: z.enum(["ordinary", "renamed", "unmerged", "untracked"]),
    conflicted: z.boolean(),
    fingerprint: z.string().regex(/^[a-f0-9]{64}$/),
  })
  .strict();

const detailedInspectResultSchema = z
  .object({
    repository: resultRepositorySchema,
    branch: branchStatusSchema,
    head: operationHeadSchema,
    snapshot: z.string().regex(/^[a-f0-9]{12}$/).optional(),
    stashCount: z.number().int().nonnegative(),
    inProgress: z.array(
      z.enum(["merge", "cherry-pick", "revert", "rebase", "bisect", "sequencer"]),
    ),
    staged: resultPathListSchema,
    unstaged: resultPathListSchema,
    untracked: resultPathListSchema,
    conflicted: resultPathListSchema,
    changes: z.array(detailedChangeSchema),
  })
  .strict();

export type DetailedInspectedChange = z.infer<typeof detailedChangeSchema>;
export type DetailedInspectResult = z.infer<typeof detailedInspectResultSchema>;

const exists = async (path: string) => access(path).then(() => true, () => false);

const inspectInProgress = async (gitDir: string) => {
  const markers = [
    ["merge", "MERGE_HEAD"],
    ["cherry-pick", "CHERRY_PICK_HEAD"],
    ["revert", "REVERT_HEAD"],
    ["rebase", "rebase-merge"],
    ["rebase", "rebase-apply"],
    ["bisect", "BISECT_LOG"],
    ["sequencer", "sequencer"],
  ] as const;
  const active = await Promise.all(
    markers.map(async ([name, marker]) => ({ name, active: await exists(join(gitDir, marker)) })),
  );
  return [...new Set(active.filter(({ active }) => active).map(({ name }) => name))];
};

const hasIndexChange = ({ indexStatus, conflicted }: StatusChange) =>
  !conflicted && ![".", " ", "?", "!"].includes(indexStatus);

const hasWorktreeChange = ({ worktreeStatus, conflicted }: StatusChange) =>
  !conflicted && ![".", " ", "?", "!"].includes(worktreeStatus);

export const inspectDetailed = async (
  input: InspectRequest,
  options: InspectOptions = {},
): Promise<DetailedInspectResult> => {
  const request = inspectRequestSchema.parse(input);
  const repository = await requireWorktreeRepository(request.repoPath);
  const files = request.files
    ? await validateLiteralFiles(repository.root, request.files)
    : undefined;
  const args = ["status", "--porcelain=v2", "-z", "--branch", "--untracked-files=all"];
  if (files) args.push("--", ...files);
  const statusResult = await git.runChecked(repository.root, args);
  const parsed = parsePorcelainV2(statusResult.stdout);
  const stash = await git.run(repository.root, ["rev-list", "--walk-reflogs", "--count", "refs/stash"]);
  const stashCount = stash.exitCode === 0 ? Number.parseInt(stash.stdout.trim(), 10) || 0 : 0;
  const inProgress = await inspectInProgress(repository.gitDir);
  const changes = await Promise.all(
    parsed.changes
      .filter(({ kind }) => kind !== "ignored")
      .map(async (change) => ({
        ...change,
        fingerprint: await fingerprintChange(repository.root, change),
      })),
  );

  // Snapshots recorded from a file-scoped inspect must not masquerade as a
  // whole-repository view, so scoped requests re-fingerprint the full status.
  const recordSnapshot = options.recordSnapshot ?? true;
  const snapshotChanges = !recordSnapshot
    ? []
    : files
      ? await Promise.all(
          parsePorcelainV2(
            (await git.runChecked(repository.root, [
              "status",
              "--porcelain=v2",
              "-z",
              "--branch",
              "--untracked-files=all",
            ])).stdout,
          ).changes
            .filter(({ kind }) => kind !== "ignored")
            .map(async (change) => ({
              ...change,
              fingerprint: await fingerprintChange(repository.root, change),
            })),
        )
      : changes;
  const snapshot = !recordSnapshot
    ? undefined
    : await createSnapshotStore({
        ...(options.stateRoot ? { stateRoot: options.stateRoot } : {}),
      }).record({
        root: repository.root,
        head: parsed.branch.oid,
        branch: parsed.branch.head,
        fingerprints: Object.fromEntries(
          snapshotChanges.map(({ path, fingerprint }) => [path, fingerprint]),
        ),
      });

  return detailedInspectResultSchema.parse({
    repository: {
      root: repository.root,
      gitDir: repository.gitDir,
      commonDir: repository.commonDir,
    },
    branch: parsed.branch,
    head: parsed.branch.oid === null
      ? { kind: "unborn" }
      : { kind: "oid", oid: parsed.branch.oid },
    ...(snapshot ? { snapshot } : {}),
    stashCount,
    inProgress,
    staged: changes.filter(hasIndexChange).map(({ path }) => path),
    unstaged: changes.filter(hasWorktreeChange).map(({ path }) => path),
    untracked: changes.filter(({ kind }) => kind === "untracked").map(({ path }) => path),
    conflicted: changes.filter(({ conflicted }) => conflicted).map(({ path }) => path),
    changes,
  });
};

const redactUrl = (url: string) =>
  url.replace(/([a-z][a-z0-9+.-]*:\/\/)[^@\s/]+@/gi, "$1[REDACTED]@");

const configuredRemotes = async (root: string) => {
  const result = await git.run(root, ["remote", "-v"]);
  if (result.exitCode !== 0) return [];
  const remotes = new Map<string, { fetchUrl: string | null; pushUrl: string | null }>();
  for (const line of result.stdout.split(/\r?\n/)) {
    const match = /^(\S+)\t(\S+) \((fetch|push)\)$/.exec(line);
    if (!match) continue;
    const entry = remotes.get(match[1]!) ?? { fetchUrl: null, pushUrl: null };
    if (match[3] === "fetch") entry.fetchUrl = redactUrl(match[2]!);
    else entry.pushUrl = redactUrl(match[2]!);
    remotes.set(match[1]!, entry);
  }
  return [...remotes.entries()].map(([name, urls]) => ({ name, ...urls }));
};

export const inspect = async (
  input: InspectRequest,
  options: InspectOptions = {},
): Promise<InspectResult> => {
  const detailed = await inspectDetailed(input, options);
  const remotes = await configuredRemotes(detailed.repository.root);
  return inspectResultSchema.parse({
    root: detailed.repository.root,
    branch: detailed.branch.head,
    head: detailed.branch.oid,
    ...(detailed.branch.upstream
      ? {
          upstream: {
            ref: detailed.branch.upstream,
            ahead: detailed.branch.ahead,
            behind: detailed.branch.behind,
          },
        }
      : {}),
    ...(detailed.inProgress.length > 0 ? { state: detailed.inProgress } : {}),
    ...(detailed.stashCount > 0 ? { stashes: detailed.stashCount } : {}),
    ...(detailed.snapshot ? { snapshot: detailed.snapshot } : {}),
    ...(remotes.length > 0 ? { remotes } : {}),
    changes: detailed.changes.map((change) => ({
      path: change.path,
      status: `${change.indexStatus}${change.worktreeStatus}`,
      ...(change.originalPath ? { from: change.originalPath } : {}),
    })),
  });
};

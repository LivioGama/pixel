import {
  syncRequestSchema,
  syncResultSchema,
  type SyncRequest,
  type SyncResult,
} from "../contracts/v1/sync.ts";
import { UsableGitError } from "../errors.ts";
import { discoverRepository } from "../git/repository.ts";
import { git, type GitRunner } from "../git/runner.ts";

export type { SyncRequest, SyncResult } from "../contracts/v1/sync.ts";

type SyncOptions = { runner?: GitRunner };

// sync is remote-refreshing but locally non-destructive: it writes only
// refs/remotes/<remote>/*, so it takes no journal (fetch is idempotent and
// converges) and no repository mutation lock (holding it through a slow
// network fetch would block publish for zero protection gain — Git's own
// per-ref locks handle races).

const refOid = async (root: string, ref: string, runner: GitRunner) => {
  const result = await runner.run(root, ["rev-parse", "--verify", "--quiet", ref]);
  return result.exitCode === 0 ? result.stdout.trim() : null;
};

const currentBranch = async (root: string, runner: GitRunner) => {
  const result = await runner.run(root, ["symbolic-ref", "--quiet", "--short", "HEAD"]);
  return result.exitCode === 0 ? result.stdout.trim() : null;
};

const upstreamBranchName = async (root: string, branch: string, remote: string, runner: GitRunner) => {
  const result = await runner.run(root, [
    "for-each-ref",
    "--format=%(upstream:short)",
    `refs/heads/${branch}`,
  ]);
  const upstream = result.exitCode === 0 ? result.stdout.trim() : "";
  const prefix = `${remote}/`;
  return upstream.startsWith(prefix) ? upstream.slice(prefix.length) : null;
};

const aheadBehind = async (root: string, branch: string, trackingRef: string, runner: GitRunner) => {
  const counts = await runner.run(root, [
    "rev-list",
    "--left-right",
    "--count",
    `refs/heads/${branch}...${trackingRef}`,
  ]);
  if (counts.exitCode !== 0) return null;
  const [ahead, behind] = counts.stdout.trim().split(/\s+/).map((value) => Number.parseInt(value, 10));
  if (!Number.isFinite(ahead) || !Number.isFinite(behind)) return null;
  return { ahead: ahead!, behind: behind! };
};

export const sync = async (
  input: SyncRequest,
  options: SyncOptions = {},
): Promise<SyncResult> => {
  const parsed = syncRequestSchema.safeParse(input);
  if (!parsed.success) {
    throw new UsableGitError("INVALID_INPUT", "Invalid sync request", {
      issues: parsed.error.issues.map(({ path, message }) => ({ path, message })),
    });
  }
  const request = parsed.data;
  const runner = options.runner ?? git;

  let repository;
  try {
    repository = await discoverRepository(request.repoPath, runner);
  } catch (error) {
    throw error instanceof UsableGitError
      ? error
      : new UsableGitError("INVALID_REPOSITORY", "repoPath is not a readable Git repository");
  }

  const remotes = await runner.run(repository.root, ["remote"]);
  const configured = remotes.stdout.split(/\r?\n/).filter(Boolean);
  if (!configured.includes(request.remote)) {
    throw new UsableGitError("INVALID_INPUT", `Remote is not configured: ${request.remote}`, {
      configuredRemotes: configured,
    });
  }

  const branchName = await currentBranch(repository.root, runner);
  const branches = request.branches ??
    (branchName
      ? [(await upstreamBranchName(repository.root, branchName, request.remote, runner)) ?? branchName]
      : undefined);
  if (!branches || branches.length === 0) {
    throw new UsableGitError(
      "INVALID_INPUT",
      "No branches to sync: detached HEAD and no explicit branches given",
    );
  }

  const before = new Map<string, string | null>();
  for (const name of branches) {
    const ref = `refs/remotes/${request.remote}/${name}`;
    before.set(name, await refOid(repository.root, ref, runner));
  }

  // Explicit refspecs only: never fetch-all, never prune, never tags. The
  // forced tracking-ref update is correct — tracking refs mirror the remote.
  const refspecs = branches.map(
    (name) => `+refs/heads/${name}:refs/remotes/${request.remote}/${name}`,
  );
  const fetched = await runner.run(repository.root, [
    "fetch",
    "--no-tags",
    "--no-write-fetch-head",
    request.remote,
    ...refspecs,
  ]);
  if (fetched.exitCode !== 0) {
    const diagnostic = fetched.stderr.slice(0, 2_000);
    // Fetch failure is retryable, never NETWORK_AMBIGUITY (reserved for push).
    if (/authentication|permission denied|access denied|401|403|could not read/i.test(diagnostic)) {
      throw new UsableGitError("AUTH_FAILED", "Fetch authentication failed", { diagnostic });
    }
    // A branch absent on the remote is a per-branch condition, not a failure —
    // but git exits nonzero when a refspec matches nothing. Distinguish below.
    if (!/couldn't find remote ref/i.test(diagnostic)) {
      throw new UsableGitError("GIT_FAILED", "Fetch failed; safe to retry", {
        exitCode: fetched.exitCode,
        diagnostic,
      });
    }
  }

  // One listing for every requested branch: absent-on-remote is a per-branch
  // success condition (newOid null), not an operation failure.
  const listed = await runner.run(repository.root, [
    "ls-remote",
    "--heads",
    request.remote,
    ...branches.map((name) => `refs/heads/${name}`),
  ]);
  const remoteHeads = new Set(
    listed.exitCode === 0
      ? listed.stdout
          .split(/\r?\n/)
          .filter(Boolean)
          .map((line) => line.split("\t")[1] ?? "")
      : [],
  );

  const results = [];
  for (const name of branches) {
    const ref = `refs/remotes/${request.remote}/${name}`;
    const oldOid = before.get(name) ?? null;
    const known = remoteHeads.has(`refs/heads/${name}`);
    const newOid = known ? await refOid(repository.root, ref, runner) : null;
    results.push({
      branch: name,
      ref,
      oldOid,
      newOid,
      updated: newOid !== null && newOid !== oldOid,
    });
  }

  let branchStatus = null;
  if (branchName) {
    const upstream = await upstreamBranchName(repository.root, branchName, request.remote, runner);
    const trackingBranch = upstream ?? (branches.includes(branchName) ? branchName : null);
    if (trackingBranch) {
      const counts = await aheadBehind(
        repository.root,
        branchName,
        `refs/remotes/${request.remote}/${trackingBranch}`,
        runner,
      );
      if (counts) branchStatus = { name: branchName, ...counts };
    }
  }

  return syncResultSchema.parse({
    remote: request.remote,
    fetched: results,
    branch: branchStatus,
  });
};

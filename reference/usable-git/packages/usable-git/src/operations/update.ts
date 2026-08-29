import { createHash } from "node:crypto";
import { access } from "node:fs/promises";
import { join } from "node:path";
import {
  updateRequestSchema,
  updateResultSchema,
  type UpdateRequest,
  type UpdateResult,
} from "../contracts/v1/update.ts";
import { UsableGitError } from "../errors.ts";
import { discoverRepository } from "../git/repository.ts";
import { git, type GitRunner } from "../git/runner.ts";
import {
  createOperationJournal,
  IdempotencyConflictError,
} from "../mutations/operation-journal.ts";
import {
  acquireRepositoryLock,
  RepositoryBusyError,
} from "../mutations/repository-lock.ts";

export type { UpdateRequest, UpdateResult } from "../contracts/v1/update.ts";

type UpdateOptions = {
  stateRoot?: string;
  runner?: GitRunner;
};

type StoredOutcome =
  | { kind: "success"; result: UpdateResult }
  | {
      kind: "error";
      error: { code: ConstructorParameters<typeof UsableGitError>[0]; message: string; details?: Record<string, unknown> };
    };

const digest = (value: string) => createHash("sha256").update(value).digest("hex");

const exists = async (path: string) => access(path).then(() => true, () => false);

const currentHead = async (root: string, runner: GitRunner) => {
  const result = await runner.run(root, ["rev-parse", "--verify", "--quiet", "HEAD"]);
  return result.exitCode === 0 ? result.stdout.trim() : null;
};

const currentBranch = async (root: string, runner: GitRunner) => {
  const result = await runner.run(root, ["symbolic-ref", "--quiet", "--short", "HEAD"]);
  return result.exitCode === 0 ? result.stdout.trim() : null;
};

const dirtyStatusPaths = async (root: string, runner: GitRunner) => {
  const status = await runner.runChecked(root, ["status", "--porcelain=v2", "-z"]);
  const paths: string[] = [];
  for (const record of status.stdout.split("\0")) {
    if (/^[12u] /.test(record)) {
      const fields = record.split(" ");
      paths.push(fields.slice(record.startsWith("2 ") ? 9 : record.startsWith("u ") ? 10 : 8).join(" "));
    }
  }
  return paths;
};

export const update = async (
  input: UpdateRequest,
  options: UpdateOptions = {},
): Promise<UpdateResult> => {
  const parsed = updateRequestSchema.safeParse(input);
  if (!parsed.success) {
    throw new UsableGitError("INVALID_INPUT", "Invalid update request", {
      issues: parsed.error.issues.map(({ path, message }) => ({ path, message })),
    });
  }
  const request = {
    ...parsed.data,
    requestId: parsed.data.requestId ??
      `auto-${crypto.randomUUID().replaceAll("-", "").slice(0, 12)}`,
  };
  const runner = options.runner ?? git;

  let repository;
  try {
    repository = await discoverRepository(request.repoPath, runner);
  } catch (error) {
    throw error instanceof UsableGitError
      ? error
      : new UsableGitError("INVALID_REPOSITORY", "repoPath is not a readable Git repository");
  }
  if (repository.isBare) {
    throw new UsableGitError("UNSUPPORTED_STATE", "Bare repositories are unsupported for update");
  }

  const repoKey = digest(repository.commonDir);
  const inputHash = digest(JSON.stringify(request));
  const journal = createOperationJournal({ stateRoot: options.stateRoot });
  let journalState;
  try {
    journalState = await journal.begin({
      requestId: request.requestId,
      operation: "update",
      repoKey,
      inputHash,
    });
  } catch (error) {
    if (error instanceof IdempotencyConflictError) {
      throw new UsableGitError("RECOVERY_CONFLICT", error.message);
    }
    throw error;
  }
  if (journalState.kind === "replay") {
    const outcome = journalState.result as StoredOutcome;
    if (outcome?.kind === "success") return updateResultSchema.parse(outcome.result);
    if (outcome?.kind === "error") {
      throw new UsableGitError(outcome.error.code, outcome.error.message, outcome.error.details);
    }
    throw new UsableGitError("RECOVERY_CONFLICT", "Stored update outcome is unreadable");
  }

  let lock;
  try {
    lock = await acquireRepositoryLock(repository.commonDir, { stateRoot: options.stateRoot });
  } catch (error) {
    if (error instanceof RepositoryBusyError) {
      throw new UsableGitError("BUSY_REPOSITORY", error.message);
    }
    throw error;
  }

  const terminal = async (outcome: StoredOutcome) => {
    await journal.complete(repoKey, request.requestId, outcome);
  };
  const fail = async (
    code: ConstructorParameters<typeof UsableGitError>[0],
    message: string,
    details?: Record<string, unknown>,
  ): Promise<never> => {
    const error = new UsableGitError(code, message, details);
    await terminal({ kind: "error", error: { code, message, ...(details ? { details } : {}) } });
    throw error;
  };

  try {
    const head = await currentHead(repository.root, runner);
    const branchName = await currentBranch(repository.root, runner);

    if (journalState.kind === "resume") {
      if (head === request.targetOid && branchName) {
        const counted = await runner.run(repository.root, [
          "rev-list",
          "--count",
          `${request.expectedHead.oid}..${request.targetOid}`,
        ]);
        const commitsAdvanced = Number.parseInt(counted.stdout.trim(), 10) || 1;
        const result = updateResultSchema.parse({
          branch: branchName,
          previousOid: request.expectedHead.oid,
          newOid: request.targetOid,
          commitsAdvanced,
        });
        await terminal({ kind: "success", result });
        return result;
      }
      if (head === request.expectedHead.oid) {
        await fail("GIT_FAILED", "Interrupted update was rolled back before HEAD moved; safe to retry with a new requestId");
      }
      await fail("RECOVERY_CONFLICT", "Interrupted update could not be verified");
    }

    if (!branchName) {
      await fail("UNSUPPORTED_STATE", "Detached HEAD is unsupported for update");
    }
    const inProgressMarkers = [
      "MERGE_HEAD",
      "CHERRY_PICK_HEAD",
      "REVERT_HEAD",
      "rebase-merge",
      "rebase-apply",
      "sequencer",
    ];
    for (const marker of inProgressMarkers) {
      if (await exists(join(repository.gitDir, marker))) {
        await fail("UNSUPPORTED_STATE", "An in-progress Git operation is unsupported for update");
      }
    }
    if (head !== request.expectedHead.oid) {
      await fail("STALE_STATE", "HEAD changed since inspection", {
        expectedHead: request.expectedHead,
        actualHead: head,
      });
    }

    const targetExists = await runner.run(repository.root, [
      "rev-parse",
      "--verify",
      "--quiet",
      `${request.targetOid}^{commit}`,
    ]);
    if (targetExists.exitCode !== 0) {
      await fail("INVALID_INPUT", "Target oid is not a known local commit; run sync first", {
        targetOid: request.targetOid,
      });
    }
    if (request.targetOid === head) {
      await fail("NOTHING_TO_COMMIT", "Branch is already at the target oid");
    }
    const isAncestor = await runner.run(repository.root, [
      "merge-base",
      "--is-ancestor",
      request.expectedHead.oid,
      request.targetOid,
    ]);
    if (isAncestor.exitCode !== 0) {
      const mergeBase = await runner.run(repository.root, [
        "merge-base",
        request.expectedHead.oid,
        request.targetOid,
      ]);
      await fail(
        "NON_FAST_FORWARD",
        "Local branch has diverged from the target; resolve outside usable-git",
        { mergeBase: mergeBase.exitCode === 0 ? mergeBase.stdout.trim() : null },
      );
    }

    // Stricter than git's own checkout refusal: refuse before mutating when
    // any incoming file overlaps any dirty path — unrelated work must survive.
    const incoming = await runner.runChecked(repository.root, [
      "diff",
      "--name-only",
      "-z",
      request.expectedHead.oid,
      request.targetOid,
    ]);
    const incomingPaths = new Set(incoming.stdout.split("\0").filter(Boolean));
    const dirty = await dirtyStatusPaths(repository.root, runner);
    const conflicting = dirty.filter((path) => incomingPaths.has(path));
    if (conflicting.length > 0) {
      await fail(
        "UNSUPPORTED_STATE",
        "Uncommitted changes overlap files changed by the update",
        { conflictingPaths: conflicting.slice(0, 100) },
      );
    }

    await journal.transition(repoKey, request.requestId, "ref_update_started");
    const merged = await runner.run(repository.root, [
      "merge",
      "--ff-only",
      request.targetOid,
    ]);
    const observedHead = await currentHead(repository.root, runner);
    if (merged.exitCode !== 0 && observedHead !== request.targetOid) {
      await fail("GIT_FAILED", "Fast-forward update failed", {
        exitCode: merged.exitCode,
        diagnostic: merged.stderr.slice(0, 2_000),
      });
    }

    const counted = await runner.run(repository.root, [
      "rev-list",
      "--count",
      `${request.expectedHead.oid}..${request.targetOid}`,
    ]);
    const result = updateResultSchema.parse({
      branch: branchName,
      previousOid: request.expectedHead.oid,
      newOid: request.targetOid,
      commitsAdvanced: Number.parseInt(counted.stdout.trim(), 10) || 1,
    });
    await terminal({ kind: "success", result });
    return result;
  } finally {
    await lock.release();
  }
};

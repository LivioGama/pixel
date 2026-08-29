import { createHash } from "node:crypto";
import {
  branchRequestSchema,
  branchResultSchema,
  type BranchRequest,
  type BranchResult,
} from "../contracts/v1/branch.ts";
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

export type { BranchRequest, BranchResult } from "../contracts/v1/branch.ts";

type BranchOptions = {
  stateRoot?: string;
  runner?: GitRunner;
};

type StoredOutcome =
  | { kind: "success"; result: BranchResult }
  | {
      kind: "error";
      error: { code: ConstructorParameters<typeof UsableGitError>[0]; message: string; details?: Record<string, unknown> };
    };

const digest = (value: string) => createHash("sha256").update(value).digest("hex");

const currentBranch = async (root: string, runner: GitRunner) => {
  const result = await runner.run(root, ["symbolic-ref", "--quiet", "--short", "HEAD"]);
  return result.exitCode === 0 ? result.stdout.trim() : null;
};

const currentHead = async (root: string, runner: GitRunner) => {
  const result = await runner.run(root, ["rev-parse", "--verify", "--quiet", "HEAD"]);
  return result.exitCode === 0 ? result.stdout.trim() : null;
};

const branchOid = async (root: string, name: string, runner: GitRunner) => {
  const result = await runner.run(root, [
    "rev-parse",
    "--verify",
    "--quiet",
    `refs/heads/${name}`,
  ]);
  return result.exitCode === 0 ? result.stdout.trim() : null;
};

const dirtyPaths = async (root: string, runner: GitRunner) => {
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

const assertExpectedHead = (request: BranchRequest, actualHead: string | null) => {
  if (
    (request.expectedHead.kind === "unborn" && actualHead !== null) ||
    (request.expectedHead.kind === "oid" && request.expectedHead.oid !== actualHead)
  ) {
    throw new UsableGitError("STALE_STATE", "HEAD changed since inspection", {
      expectedHead: request.expectedHead,
      actualHead,
    });
  }
};

export const branch = async (
  input: BranchRequest,
  options: BranchOptions = {},
): Promise<BranchResult> => {
  const parsed = branchRequestSchema.safeParse(input);
  if (!parsed.success) {
    throw new UsableGitError("INVALID_INPUT", "Invalid branch request", {
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
    throw new UsableGitError("UNSUPPORTED_STATE", "Bare repositories are unsupported for branch");
  }

  const repoKey = digest(repository.commonDir);
  const inputHash = digest(JSON.stringify(request));
  const journal = createOperationJournal({ stateRoot: options.stateRoot });
  let journalState;
  try {
    journalState = await journal.begin({
      requestId: request.requestId,
      operation: "branch",
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
    if (outcome?.kind === "success") return branchResultSchema.parse(outcome.result);
    if (outcome?.kind === "error") {
      throw new UsableGitError(outcome.error.code, outcome.error.message, outcome.error.details);
    }
    throw new UsableGitError("RECOVERY_CONFLICT", "Stored branch outcome is unreadable");
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
    if (outcome.kind === "error") {
      return new UsableGitError(outcome.error.code, outcome.error.message, outcome.error.details);
    }
    return null;
  };
  const fail = async (
    code: ConstructorParameters<typeof UsableGitError>[0],
    message: string,
    details?: Record<string, unknown>,
  ): Promise<never> => {
    await terminal({ kind: "error", error: { code, message, ...(details ? { details } : {}) } });
    throw new UsableGitError(code, message, details);
  };

  try {
    const head = await currentHead(repository.root, runner);
    const previousBranch = await currentBranch(repository.root, runner);

    if (journalState.kind === "resume") {
      // Both modes are single ref/HEAD updates: if HEAD now points at the
      // requested branch consistently with the request, replay success;
      // otherwise the interrupted attempt cannot be classified.
      const name = request.mode.name;
      const observedBranch = previousBranch;
      const observedOid = await branchOid(repository.root, name, runner);
      if (observedBranch === name && (request.mode.kind === "switch" || observedOid === head)) {
        const result = branchResultSchema.parse({
          name,
          oid: observedOid,
          previousBranch: null,
          created: request.mode.kind === "create",
        });
        await terminal({ kind: "success", result });
        return result;
      }
      await fail("RECOVERY_CONFLICT", "Interrupted branch operation could not be verified");
    }

    assertExpectedHead(request, head);

    if (request.mode.kind === "create") {
      const existing = await branchOid(repository.root, request.mode.name, runner);
      if (existing !== null) {
        await fail("REF_EXISTS", `Branch already exists: ${request.mode.name}`, {
          existingOid: existing,
        });
      }
      // Creating at current HEAD and switching touches zero worktree files —
      // also the safe exit from detached HEAD, guarded by expectedHead.
      const switched = await runner.run(repository.root, [
        "switch",
        "--create",
        request.mode.name,
      ]);
      if (switched.exitCode !== 0) {
        await fail("GIT_FAILED", "Git branch creation failed", {
          exitCode: switched.exitCode,
          diagnostic: switched.stderr.slice(0, 2_000),
        });
      }
      const result = branchResultSchema.parse({
        name: request.mode.name,
        oid: await currentHead(repository.root, runner),
        previousBranch,
        created: true,
      });
      await terminal({ kind: "success", result });
      return result;
    }

    if (request.expectedHead.kind === "unborn") {
      await fail("INVALID_INPUT", "Cannot switch branches on an unborn HEAD");
    }
    const targetOid = await branchOid(repository.root, request.mode.name, runner);
    if (targetOid === null) {
      await fail("INVALID_INPUT", `Branch does not exist: ${request.mode.name}`);
    }
    const dirty = await dirtyPaths(repository.root, runner);
    if (dirty.length > 0) {
      // No carry-changes-across-switch option — that is exactly the ambiguity
      // this tool exists to prevent. Publish or resolve the work first.
      await fail(
        "UNSUPPORTED_STATE",
        "Switching with uncommitted tracked changes is unsupported; publish or resolve them first",
        { dirtyPaths: dirty.slice(0, 100) },
      );
    }
    const switched = await runner.run(repository.root, ["switch", request.mode.name]);
    if (switched.exitCode !== 0) {
      await fail("GIT_FAILED", "Git branch switch failed", {
        exitCode: switched.exitCode,
        diagnostic: switched.stderr.slice(0, 2_000),
      });
    }
    const result = branchResultSchema.parse({
      name: request.mode.name,
      oid: targetOid,
      previousBranch,
      created: false,
    });
    await terminal({ kind: "success", result });
    return result;
  } finally {
    await lock.release();
  }
};

import { createHash } from "node:crypto";
import { constants } from "node:fs";
import { access } from "node:fs/promises";
import { resolve, join } from "node:path";
import {
  publishRequestSchema,
  publishResultSchema,
  type ParsedPublishRequest,
  type PublishRequest,
  type PublishResult,
} from "../contracts/v1/publish.ts";
import type { ErrorCode } from "../contracts/v1.ts";
import { UsableGitError } from "../errors.ts";
import { validateLiteralFiles } from "../git/paths.ts";
import { discoverRepository } from "../git/repository.ts";
import { git, type GitRunResult, type GitRunner } from "../git/runner.ts";
import {
  captureIndexSnapshot,
  indexChecksum,
  restoreIndexIfOwned,
  type IndexSnapshot,
} from "../mutations/publish-index.ts";
import {
  createPublishRecoveryStore,
  type PublishRecoveryState,
} from "../mutations/publish-recovery.ts";
import {
  createOperationJournal,
  IdempotencyConflictError,
} from "../mutations/operation-journal.ts";
import {
  acquireRepositoryLock,
  RepositoryBusyError,
} from "../mutations/repository-lock.ts";
import { createSnapshotStore } from "../mutations/snapshot-store.ts";
import { inspectDetailed, type DetailedInspectResult } from "./inspect.ts";
import type { ExpectedHead } from "../contracts/v1/publish.ts";

export type { PublishRequest, PublishResult } from "../contracts/v1/publish.ts";

export class PublishOperationError extends UsableGitError {
  constructor(
    code: ErrorCode,
    message: string,
    details?: Record<string, unknown>,
  ) {
    super(code, message, details);
    this.name = "PublishOperationError";
  }
}

type PublishOptions = {
  stateRoot?: string;
  runner?: GitRunner;
  mutationProbe?: (phase: PublishMutationPhase) => void | Promise<void>;
};

type PublishMutationPhase =
  | "journal:started"
  | "recovery:snapshotted"
  | "journal:index_staged"
  | "recovery:commit_started"
  | "journal:commit_observed"
  | "journal:terminal";

type SerializedOutcome =
  | { kind: "success"; result: PublishResult }
  | {
      kind: "error";
      error: { code: ErrorCode; message: string; details?: Record<string, unknown> };
    };

const digest = (value: string | Uint8Array) =>
  createHash("sha256").update(value).digest("hex");

export const generatedRequestId = () =>
  `auto-${crypto.randomUUID().replaceAll("-", "").slice(0, 12)}`;

const normalizedRequestHash = (request: PublishRequest) =>
  digest(
    JSON.stringify({
      ...request,
      ...(request.expected
        ? {
            expected: {
              head: request.expected.head,
              fingerprints: Object.fromEntries(
                Object.entries(request.expected.fingerprints).sort(([left], [right]) =>
                  left.localeCompare(right),
                ),
              ),
            },
          }
        : {}),
    }),
  );

type ResolvedExpectations = {
  head: ExpectedHead;
  fingerprints: Record<string, string>;
};

type ResolvedPublishRequest = ParsedPublishRequest & { requestId: string };

const resolveExpectations = async (
  request: PublishRequest,
  root: string,
  stateRoot?: string,
): Promise<ResolvedExpectations> => {
  if (request.expected) {
    return { head: request.expected.head, fingerprints: request.expected.fingerprints };
  }
  const store = createSnapshotStore({ ...(stateRoot ? { stateRoot } : {}) });
  const record = await store.read(root, request.snapshot ?? "");
  if (!record) {
    throw new PublishOperationError(
      "STALE_STATE",
      "Snapshot token is unknown or expired; re-run inspect to obtain a fresh one",
      { reason: "unknown-snapshot", snapshot: request.snapshot },
    );
  }
  const missing = request.files.filter((file) => !record.fingerprints[file]);
  if (missing.length > 0) {
    throw new PublishOperationError(
      "NOTHING_TO_COMMIT",
      `Selected paths had no committable change at snapshot time: ${JSON.stringify(missing)}`,
    );
  }
  return {
    head: record.head === null
      ? { kind: "unborn" }
      : { kind: "oid", oid: record.head },
    fingerprints: Object.fromEntries(
      request.files.map((file) => [file, record.fingerprints[file]!]),
    ),
  };
};

const currentHead = async (root: string, runner: GitRunner) => {
  const result = await runner.run(root, ["rev-parse", "--verify", "HEAD"]);
  return result.exitCode === 0 ? result.stdout.trim() : null;
};

const pathExists = async (path: string) => {
  try {
    await access(path);
    return true;
  } catch {
    return false;
  }
};

const configEnabled = async (
  root: string,
  key: string,
  runner: GitRunner,
) => {
  const result = await runner.run(root, ["config", "--bool", "--get", key]);
  return result.exitCode === 0 && result.stdout.trim() === "true";
};

const assertSupportedState = async (
  snapshot: DetailedInspectResult,
  gitDir: string,
  root: string,
  runner: GitRunner,
) => {
  if (snapshot.branch.oid && !snapshot.branch.head) {
    throw new PublishOperationError(
      "UNSUPPORTED_STATE",
      "Detached HEAD is unsupported for publish",
    );
  }
  if (snapshot.conflicted.length > 0) {
    throw new PublishOperationError(
      "UNSUPPORTED_STATE",
      "Unresolved conflicts are unsupported for publish",
    );
  }

  const sequencerPaths = [
    "MERGE_HEAD",
    "CHERRY_PICK_HEAD",
    "REVERT_HEAD",
    "rebase-apply",
    "rebase-merge",
    "sequencer",
  ];
  if (
    (await Promise.all(sequencerPaths.map((path) => pathExists(join(gitDir, path))))).some(
      Boolean,
    )
  ) {
    throw new PublishOperationError(
      "UNSUPPORTED_STATE",
      "An in-progress Git operation is unsupported for publish",
    );
  }

  if (
    (await configEnabled(root, "core.sparseCheckout", runner)) ||
    (await configEnabled(root, "index.sparse", runner)) ||
    (await configEnabled(root, "core.splitIndex", runner))
  ) {
    throw new PublishOperationError(
      "UNSUPPORTED_STATE",
      "Sparse and split indexes are unsupported for publish",
    );
  }
};

const assertExpectations = (
  files: string[],
  expected: ResolvedExpectations,
  snapshot: DetailedInspectResult,
) => {
  const actualHead = snapshot.branch.oid;
  if (
    (expected.head.kind === "unborn" && actualHead !== null) ||
    (expected.head.kind === "oid" && expected.head.oid !== actualHead)
  ) {
    throw new PublishOperationError("STALE_STATE", "HEAD changed since inspection", {
      expectedHead: expected.head,
      actualHead,
    });
  }

  const changes = new Map(snapshot.changes.map((change) => [change.path, change]));
  if (changes.size === 0) {
    throw new PublishOperationError(
      "NOTHING_TO_COMMIT",
      "Selected paths contain no committable changes",
    );
  }

  for (const file of files) {
    const change = changes.get(file);
    if (!change) {
      throw new PublishOperationError(
        "NOTHING_TO_COMMIT",
        `Selected path contains no committable change: ${JSON.stringify(file)}`,
      );
    }
    if (change.fingerprint !== expected.fingerprints[file]) {
      throw new PublishOperationError(
        "STALE_STATE",
        `Selected path changed since inspection: ${JSON.stringify(file)}`,
      );
    }
  }
};

const serializeSnapshot = (
  snapshot: IndexSnapshot,
): PublishRecoveryState["index"] =>
  snapshot.exists
    ? {
        exists: true,
        checksum: snapshot.checksum,
        bytesBase64: Buffer.from(snapshot.bytes).toString("base64"),
        mode: snapshot.mode,
      }
    : snapshot;

const deserializeSnapshot = (
  snapshot: PublishRecoveryState["index"],
): IndexSnapshot =>
  snapshot.exists
    ? {
        exists: true,
        checksum: snapshot.checksum,
        bytes: new Uint8Array(Buffer.from(snapshot.bytesBase64, "base64")),
        mode: snapshot.mode,
      }
    : snapshot;

const pathspecInput = (files: string[]) =>
  new TextEncoder().encode(`${files.join("\0")}\0`);

const hasActiveCommitHook = async (root: string, runner: GitRunner) => {
  for (const name of ["pre-commit", "prepare-commit-msg", "commit-msg"]) {
    const result = await runner.run(root, [
      "rev-parse",
      "--git-path",
      `hooks/${name}`,
    ]);
    if (result.exitCode !== 0) continue;
    try {
      await access(resolve(root, result.stdout.trim()), constants.X_OK);
      return true;
    } catch {
      // Missing or non-executable hooks are not active.
    }
  }
  return false;
};

const classifyCommitFailure = (
  result: GitRunResult,
  activeCommitHook: boolean,
) => {
  const diagnostic = `${result.stderr}\n${result.stdout}`.trim().slice(0, 4_096);
  if (/author identity unknown|unable to auto-detect email address/i.test(diagnostic)) {
    return new PublishOperationError("IDENTITY_MISSING", "Git author identity is missing", {
      exitCode: result.exitCode,
      diagnostic,
    });
  }
  if (/gpg failed to sign|failed to sign the data|signing failed/i.test(diagnostic)) {
    return new PublishOperationError("SIGNING_FAILED", "Git commit signing failed", {
      exitCode: result.exitCode,
      diagnostic,
    });
  }
  if (activeCommitHook || /hook|pre-commit|commit-msg/i.test(diagnostic)) {
    return new PublishOperationError("HOOK_FAILED", "A Git commit hook rejected publish", {
      exitCode: result.exitCode,
      diagnostic,
    });
  }
  if (/nothing to commit|no changes added to commit/i.test(diagnostic)) {
    return new PublishOperationError(
      "NOTHING_TO_COMMIT",
      "Selected paths contain no committable changes",
      { exitCode: result.exitCode, diagnostic },
    );
  }
  return new PublishOperationError("GIT_FAILED", "Git commit failed", {
    exitCode: result.exitCode,
    diagnostic,
  });
};

const terminalError = async (
  journal: ReturnType<typeof createOperationJournal>,
  repoKey: string,
  requestId: string,
  error: PublishOperationError,
) => {
  const outcome: SerializedOutcome = {
    kind: "error",
    error: {
      code: error.code,
      message: error.message,
      ...(error.details ? { details: error.details } : {}),
    },
  };
  await journal.complete(repoKey, requestId, outcome);
  return error;
};

const replayOutcome = (value: unknown): PublishResult => {
  const outcome = value as SerializedOutcome;
  if (outcome?.kind === "success") return publishResultSchema.parse(outcome.result);
  if (outcome?.kind === "error") {
    throw new PublishOperationError(
      outcome.error.code,
      outcome.error.message,
      outcome.error.details,
    );
  }
  throw new PublishOperationError(
    "RECOVERY_CONFLICT",
    "Stored publish outcome is unreadable",
  );
};

const changedPaths = async (
  root: string,
  commitOid: string,
  runner: GitRunner,
) => {
  const result = await runner.runChecked(root, [
    "diff-tree",
    "--root",
    "--no-commit-id",
    "--name-only",
    "-r",
    "-z",
    commitOid,
  ]);
  return result.stdout.split("\0").filter(Boolean).sort();
};

const commitParents = async (root: string, commitOid: string, runner: GitRunner) => {
  const ancestry = await runner.runChecked(root, [
    "rev-list",
    "--parents",
    "-n",
    "1",
    commitOid,
  ]);
  const [observed, ...parents] = ancestry.stdout.trim().split(/\s+/);
  return { observed, parents };
};

const verifyRecoveredCommit = async (
  request: ResolvedPublishRequest,
  recovery: PublishRecoveryState,
  commitOid: string,
  root: string,
  runner: GitRunner,
) => {
  if (recovery.phase !== "commit_started") {
    throw new PublishOperationError(
      "RECOVERY_CONFLICT",
      "HEAD changed before the interrupted publish reached commit execution",
    );
  }

  const amend = recovery.mode === "amend";
  const { observed, parents } = await commitParents(root, commitOid, runner);
  const expectedParents = amend && recovery.preHead
    ? (await commitParents(root, recovery.preHead, runner)).parents
    : recovery.preHead
      ? [recovery.preHead]
      : [];
  if (
    observed !== commitOid ||
    JSON.stringify(parents) !== JSON.stringify(expectedParents)
  ) {
    throw new PublishOperationError(
      "RECOVERY_CONFLICT",
      "Observed HEAD is not the exact child expected from interrupted publish",
    );
  }

  const committed = await changedPaths(root, commitOid, runner);
  if (amend) {
    // An amended commit carries the prior commit's paths too; a path can also
    // drop out if the amendment restores parent content. Require only that no
    // unexpected path appears.
    const allowed = new Set([
      ...(recovery.preHead ? await changedPaths(root, recovery.preHead, runner) : []),
      ...request.files,
    ]);
    if (committed.some((path) => !allowed.has(path))) {
      throw new PublishOperationError(
        "RECOVERY_CONFLICT",
        "Observed amended commit contains paths outside the interrupted publish scope",
      );
    }
  } else {
    const expectedPaths = [...request.files].sort();
    if (JSON.stringify(committed) !== JSON.stringify(expectedPaths)) {
      throw new PublishOperationError(
        "RECOVERY_CONFLICT",
        "Observed commit paths do not match interrupted publish scope",
      );
    }
  }

  const message = await runner.runChecked(root, [
    "show",
    "-s",
    "--format=%B",
    commitOid,
  ]);
  const expectedMessage = recovery.resolvedMessage ?? request.message ?? "";
  if (message.stdout.trimEnd() !== expectedMessage.trimEnd()) {
    throw new PublishOperationError(
      "RECOVERY_CONFLICT",
      "Observed commit message does not match interrupted publish input",
    );
  }
};

const statusForResult = (snapshot: DetailedInspectResult) => ({
  staged: snapshot.staged,
  unstaged: snapshot.unstaged,
  untracked: snapshot.untracked,
  conflicted: snapshot.conflicted,
});

const unrelatedFingerprints = (snapshot: DetailedInspectResult, files: Set<string>) =>
  new Map(
    snapshot.changes
      .filter(({ path }) => !files.has(path))
      .map(({ path, fingerprint }) => [path, fingerprint]),
  );

const finishObservedCommit = async (
  request: ResolvedPublishRequest,
  commitOid: string,
  before: DetailedInspectResult | null,
  runner: GitRunner,
  warnings: string[] = [],
  amend?: { amendedOid: string; allowedPaths: string[] },
): Promise<PublishResult> => {
  const expected = [...request.files].sort();
  try {
    const committed = await changedPaths(request.repoPath, commitOid, runner);
    if (amend) {
      const allowed = new Set([...amend.allowedPaths, ...request.files]);
      const unexpected = committed.filter((path) => !allowed.has(path));
      if (unexpected.length > 0) {
        warnings.push(
          `Observed amended commit contains unexpected paths: ${JSON.stringify(unexpected)}`,
        );
      }
    } else if (JSON.stringify(committed) !== JSON.stringify(expected)) {
      warnings.push(
        `Observed commit path verification failed: expected ${JSON.stringify(expected)}, observed ${JSON.stringify(committed)}`,
      );
    }
  } catch (error) {
    warnings.push(
      `Observed commit could not be fully verified: ${error instanceof Error ? error.message : String(error)}`.slice(
        0,
        4_096,
      ),
    );
  }

  let after: DetailedInspectResult | null = null;
  try {
    after = await inspectDetailed({ repoPath: request.repoPath }, { recordSnapshot: false });
    const selected = new Set(request.files);
    if (before) {
      const beforeUnrelated = unrelatedFingerprints(before, selected);
      const afterUnrelated = unrelatedFingerprints(after, selected);
      if (
        JSON.stringify([...beforeUnrelated]) !== JSON.stringify([...afterUnrelated])
      ) {
        warnings.push("Unrelated repository status changed while the commit was running");
      }
    }

    if (!after.branch.head || after.branch.oid !== commitOid) {
      warnings.push("HEAD moved again after the publish commit was observed");
    }
  } catch (error) {
    warnings.push(
      `Resulting repository status could not be inspected: ${error instanceof Error ? error.message : String(error)}`.slice(
        0,
        4_096,
      ),
    );
  }

  return publishResultSchema.parse({
    commitOid,
    ...(amend ? { amendedOid: amend.amendedOid } : {}),
    committedPaths: [...request.files],
    head: {
      oid: commitOid,
      branch: after?.branch.head ?? before?.branch.head ?? "(unknown)",
    },
    status: after
      ? statusForResult(after)
      : { staged: [], unstaged: [], untracked: [], conflicted: [] },
    warnings,
  });
};

const recoverInterruptedPublish = async (
  request: ResolvedPublishRequest,
  recovery: PublishRecoveryState | null,
  root: string,
  indexPath: string,
  runner: GitRunner,
) => {
  if (!recovery) {
    throw new PublishOperationError(
      "RECOVERY_CONFLICT",
      "Publish journal is active but recovery metadata is missing",
    );
  }

  const head = await currentHead(root, runner);
  if (head !== recovery.preHead && head !== null) {
    await verifyRecoveredCommit(request, recovery, head, root, runner);
    const amendRecovery = recovery.mode === "amend" && recovery.preHead
      ? {
          amendedOid: recovery.preHead,
          allowedPaths: await changedPaths(root, recovery.preHead, runner),
        }
      : undefined;
    const result = await finishObservedCommit(request, head, null, runner, [
      "Recovered a commit observed after an interrupted publish",
    ], amendRecovery);
    return { kind: "success" as const, result };
  }

  const restored = await restoreIndexIfOwned(
    indexPath,
    deserializeSnapshot(recovery.index),
    recovery.ownedIndexChecksum,
  );
  if (!restored) {
    throw new PublishOperationError(
      "RECOVERY_CONFLICT",
      "Index changed after an interrupted publish; recovery refused to overwrite it",
    );
  }

  throw new PublishOperationError(
    "GIT_FAILED",
    "Interrupted publish was safely rolled back before a commit was observed",
  );
};

export const publish = async (
  input: PublishRequest,
  options: PublishOptions = {},
): Promise<PublishResult> => {
  const parsed = publishRequestSchema.safeParse(input);
  if (!parsed.success) {
    throw new PublishOperationError("INVALID_INPUT", "Invalid publish request", {
      issues: parsed.error.issues.map(({ path, message }) => ({ path, message })),
    });
  }
  const request = {
    ...parsed.data,
    requestId: parsed.data.requestId ?? generatedRequestId(),
  };
  const runner = options.runner ?? git;
  let repository;
  try {
    repository = await discoverRepository(request.repoPath, runner);
  } catch (error) {
    throw new PublishOperationError(
      "INVALID_REPOSITORY",
      error instanceof Error ? error.message : "Invalid repository",
    );
  }
  if (repository.isBare) {
    throw new PublishOperationError(
      "UNSUPPORTED_STATE",
      "Bare repositories are unsupported for publish",
    );
  }

  const repoKey = digest(repository.commonDir);
  const inputHash = normalizedRequestHash(request);
  const journal = createOperationJournal({ stateRoot: options.stateRoot });
  const recoveryStore = createPublishRecoveryStore({ stateRoot: options.stateRoot });
  let journalState;
  try {
    journalState = await journal.begin({
      requestId: request.requestId,
      operation: "publish",
      repoKey,
      inputHash,
    });
  } catch (error) {
    if (error instanceof IdempotencyConflictError) {
      throw new PublishOperationError("RECOVERY_CONFLICT", error.message);
    }
    throw error;
  }
  if (journalState.kind === "replay") {
    try {
      await recoveryStore.remove(repoKey, request.requestId);
    } catch {
      // Terminal journal result remains authoritative; stale recovery data is harmless.
    }
    return replayOutcome(journalState.result);
  }
  if (journalState.kind === "started") {
    await options.mutationProbe?.("journal:started");
  }

  let lock;
  try {
    lock = await acquireRepositoryLock(repository.commonDir, {
      stateRoot: options.stateRoot,
    });
  } catch (error) {
    if (error instanceof RepositoryBusyError) {
      const publishError = new PublishOperationError("BUSY_REPOSITORY", error.message);
      throw await terminalError(journal, repoKey, request.requestId, publishError);
    }
    throw error;
  }

  const indexPath = join(repository.gitDir, "index");
  try {
    if (journalState.kind === "resume") {
      const recoveryState = await recoveryStore.read({
        requestId: request.requestId,
        repoKey,
        inputHash,
        // Snapshot-mode requests cannot re-derive the expected head without the
        // token still resolving; requestId + inputHash already bind identity.
        ...(request.expected
          ? {
              preHead:
                request.expected.head.kind === "oid"
                  ? request.expected.head.oid
                  : null,
            }
          : {}),
        files: request.files,
      });
      const canRestartBeforeMutation =
        journalState.record.phase === "started" && recoveryState === null;
      if (!canRestartBeforeMutation) {
      try {
        const recovered = await recoverInterruptedPublish(
          request,
          recoveryState,
          repository.root,
          indexPath,
          runner,
        );
        const outcome: SerializedOutcome = recovered;
        await journal.complete(repoKey, request.requestId, outcome);
        await recoveryStore.remove(repoKey, request.requestId);
        return recovered.result;
      } catch (error) {
        const publishError =
          error instanceof PublishOperationError
            ? error
            : new PublishOperationError("RECOVERY_CONFLICT", String(error));
        const terminal = await terminalError(
          journal,
          repoKey,
          request.requestId,
          publishError,
        );
        if (publishError.code === "GIT_FAILED") {
          await recoveryStore.remove(repoKey, request.requestId);
        }
        throw terminal;
      }
      }
    }

    let files;
    try {
      files = await validateLiteralFiles(repository.root, request.files, runner);
    } catch (error) {
      const publishError = new PublishOperationError(
        "INVALID_PATH",
        error instanceof Error ? error.message : "Invalid publish path",
      );
      throw await terminalError(journal, repoKey, request.requestId, publishError);
    }

    const amend = request.mode.kind === "amend";
    let before: DetailedInspectResult;
    let selectedBefore: DetailedInspectResult;
    let resolvedMessage: string;
    let amendContext: { amendedOid: string; allowedPaths: string[] } | undefined;
    try {
      const expected = await resolveExpectations(
        request,
        repository.root,
        options.stateRoot,
      );
      if (amend && expected.head.kind !== "oid") {
        throw new PublishOperationError(
          "INVALID_INPUT",
          "Amend requires an existing commit; an unborn HEAD has nothing to amend",
        );
      }
      [before, selectedBefore] = await Promise.all([
        inspectDetailed({ repoPath: repository.root }, { recordSnapshot: false }),
        inspectDetailed({ repoPath: repository.root, files }, { recordSnapshot: false }),
      ]);
      await assertSupportedState(before, repository.gitDir, repository.root, runner);
      assertExpectations(request.files, expected, selectedBefore);
      if (amend) {
        const amendedOid = before.branch.oid!;
        amendContext = {
          amendedOid,
          allowedPaths: await changedPaths(repository.root, amendedOid, runner),
        };
        resolvedMessage = request.message ?? (
          await runner.runChecked(repository.root, ["show", "-s", "--format=%B", amendedOid])
        ).stdout.trimEnd();
      } else {
        resolvedMessage = request.message!;
      }
    } catch (error) {
      const publishError =
        error instanceof PublishOperationError
          ? error
          : new PublishOperationError("INVALID_PATH", String(error));
      throw await terminalError(journal, repoKey, request.requestId, publishError);
    }

    const snapshot = await captureIndexSnapshot(indexPath);
    const activeCommitHook = await hasActiveCommitHook(repository.root, runner);
    let recovery: PublishRecoveryState = {
      schemaVersion: 1,
      requestId: request.requestId,
      repoKey,
      inputHash,
      phase: "snapshotted",
      preHead: before.branch.oid,
      files,
      index: serializeSnapshot(snapshot),
      ownedIndexChecksum: snapshot.checksum,
      mode: request.mode.kind,
      resolvedMessage,
    };
    await recoveryStore.write(recovery);
    await options.mutationProbe?.("recovery:snapshotted");

    const untracked = selectedBefore.changes
      .filter(({ path, kind }) => files.includes(path) && kind === "untracked")
      .map(({ path }) => path);
    if (untracked.length > 0) {
      const staged = await runner.run(repository.root, [
        "--literal-pathspecs",
        "add",
        "--intent-to-add",
        "--pathspec-from-file=-",
        "--pathspec-file-nul",
      ], pathspecInput(untracked));
      if (staged.exitCode !== 0) {
        const ownedIndexChecksum = await indexChecksum(indexPath);
        recovery = {
          ...recovery,
          phase: "index_staged",
          ownedIndexChecksum,
        };
        await recoveryStore.write(recovery);
        const restored = await restoreIndexIfOwned(
          indexPath,
          snapshot,
          ownedIndexChecksum,
        );
        if (!restored) {
          const error = new PublishOperationError(
            "RECOVERY_CONFLICT",
            "Index changed after intent-to-add failed; recovery refused to overwrite it",
          );
          throw await terminalError(journal, repoKey, request.requestId, error);
        }
        await recoveryStore.remove(repoKey, request.requestId);
        const error = new PublishOperationError("GIT_FAILED", "Git intent-to-add failed", {
          exitCode: staged.exitCode,
          diagnostic: staged.stderr.slice(0, 4_096),
        });
        throw await terminalError(journal, repoKey, request.requestId, error);
      }
    }

    recovery = {
      ...recovery,
      phase: "index_staged",
      ownedIndexChecksum: await indexChecksum(indexPath),
    };
    await recoveryStore.write(recovery);
    await journal.transition(repoKey, request.requestId, "index_staged");
    await options.mutationProbe?.("journal:index_staged");
    recovery = { ...recovery, phase: "commit_started" };
    await recoveryStore.write(recovery);
    await options.mutationProbe?.("recovery:commit_started");

    const commit = await runner.run(repository.root, [
      "--literal-pathspecs",
      "commit",
      ...(amend ? ["--amend"] : []),
      "--only",
      "--no-status",
      "-m",
      resolvedMessage,
      "--pathspec-from-file=-",
      "--pathspec-file-nul",
    ], pathspecInput(files));
    const observedHead = await currentHead(repository.root, runner);

    if (observedHead !== before.branch.oid && observedHead !== null) {
      const observedWarnings =
        commit.exitCode === 0
          ? []
          : [`Git exited ${commit.exitCode}, but commit ${observedHead} was observed`];
      if (
        amend &&
        before.branch.upstream !== null &&
        before.branch.ahead === 0
      ) {
        observedWarnings.push(
          "Amended a commit that exists on the upstream; pushing will require force-with-lease",
        );
      }
      let observedTransitioned = false;
      try {
        await journal.transition(repoKey, request.requestId, "commit_observed");
        observedTransitioned = true;
      } catch (error) {
        observedWarnings.push(
          `Commit was observed but journal transition failed: ${error instanceof Error ? error.message : String(error)}`.slice(
            0,
            4_096,
          ),
        );
      }
      if (observedTransitioned) {
        await options.mutationProbe?.("journal:commit_observed");
      }
      const result = await finishObservedCommit(
        request,
        observedHead,
        before,
        runner,
        observedWarnings,
        amendContext,
      );
      const outcome: SerializedOutcome = { kind: "success", result };
      let terminalWritten = false;
      try {
        await journal.complete(repoKey, request.requestId, outcome);
        terminalWritten = true;
      } catch (error) {
        result.warnings.push(
          `Commit was observed but terminal journal write failed: ${error instanceof Error ? error.message : String(error)}`.slice(
            0,
            4_096,
          ),
        );
      }
      if (terminalWritten) {
        await options.mutationProbe?.("journal:terminal");
      }
      try {
        await recoveryStore.remove(repoKey, request.requestId);
      } catch (error) {
        result.warnings.push(
          `Commit was observed but recovery cleanup failed: ${error instanceof Error ? error.message : String(error)}`.slice(
            0,
            4_096,
          ),
        );
      }
      return result;
    }

    const restored = await restoreIndexIfOwned(
      indexPath,
      snapshot,
      recovery.ownedIndexChecksum,
    );
    if (!restored) {
      const error = new PublishOperationError(
        "RECOVERY_CONFLICT",
        "Index changed during failed publish; recovery refused to overwrite it",
      );
      throw await terminalError(journal, repoKey, request.requestId, error);
    }
    await recoveryStore.remove(repoKey, request.requestId);
    throw await terminalError(
      journal,
      repoKey,
      request.requestId,
      classifyCommitFailure(commit, activeCommitHook),
    );
  } finally {
    await lock.release();
  }
};

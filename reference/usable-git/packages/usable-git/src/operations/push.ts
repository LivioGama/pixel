import { createHash } from "node:crypto";
import { access } from "node:fs/promises";
import { join } from "node:path";
import {
  pushRequestSchema,
  pushResultSchema,
  type PushRequest,
  type PushResult,
} from "@usable-git/contracts/v1/push.ts";
import { UsableGitError } from "@usable-git/errors.ts";
import { discoverRepository } from "@usable-git/git/repository.ts";
import { git, type GitRunResult, type GitRunner } from "@usable-git/git/runner.ts";
import {
  createOperationJournal,
  IdempotencyConflictError,
} from "@usable-git/mutations/operation-journal.ts";
import {
  acquireRepositoryLock,
  RepositoryBusyError,
} from "@usable-git/mutations/repository-lock.ts";

type PushOptions = {
  runner?: GitRunner;
  stateRoot?: string;
  mutationProbe?: (phase: PushMutationPhase) => void | Promise<void>;
};

type PushMutationPhase =
  | "journal:started"
  | "journal:push_started"
  | "remote:returned"
  | "journal:terminal";

type PushStartedRecovery = {
  schemaVersion: 1;
  kind: "push_started";
  remote: string;
  sourceRef: string;
  targetRef: string;
  sourceOid: string;
  oldTargetOid: string | null;
  recoveryHash: string;
};

type StoredPushOutcome =
  | {
      schemaVersion: 1;
      kind: "success";
      result: PushResult;
      resultHash: string;
    }
  | {
      schemaVersion: 1;
      kind: "error";
      error: {
        code: ConstructorParameters<typeof UsableGitError>[0];
        message: string;
        details?: Record<string, unknown>;
      };
      resultHash: string;
    };

const hashJson = (value: unknown) =>
  createHash("sha256").update(JSON.stringify(value)).digest("hex");

const oneLine = (value: string) => value.replace(/[\r\n]+$/, "");

const diagnostic = (value: string) =>
  value
    .replace(/([a-z][a-z0-9+.-]*:\/\/)[^@\s/]+@/gi, "$1[REDACTED]@")
    .replace(/([?&](?:access_token|token|key|password)=)[^&\s]+/gi, "$1[REDACTED]")
    .slice(0, 2_000)
    .trim();

const operationError = (
  code: ConstructorParameters<typeof UsableGitError>[0],
  message: string,
  result?: GitRunResult,
) =>
  new UsableGitError(code, message, {
    ...(result ? { exitCode: result.exitCode } : {}),
    ...(result && diagnostic(result.stderr)
      ? { diagnostic: diagnostic(result.stderr) }
      : {}),
  });

const exists = async (path: string) => {
  try {
    await access(path);
    return true;
  } catch {
    return false;
  }
};

const assertSupportedState = async (
  root: string,
  gitDir: string,
  runner: GitRunner,
) => {
  const symbolicHead = await runner.run(root, ["symbolic-ref", "-q", "HEAD"]);
  if (symbolicHead.exitCode !== 0) {
    throw new UsableGitError("UNSUPPORTED_STATE", "Detached HEAD is unsupported for push");
  }

  const status = await runner.run(root, ["status", "--porcelain=v2", "-z"]);
  if (status.exitCode !== 0) {
    throw operationError("GIT_FAILED", "Unable to inspect repository state", status);
  }
  if (status.stdout.split("\0").some((record) => record.startsWith("u "))) {
    throw new UsableGitError("UNSUPPORTED_STATE", "Unresolved conflicts are unsupported for push");
  }

  const inProgressPaths = [
    "MERGE_HEAD",
    "CHERRY_PICK_HEAD",
    "REVERT_HEAD",
    "rebase-merge",
    "rebase-apply",
    "sequencer",
  ];
  if (
    (await Promise.all(inProgressPaths.map((path) => exists(join(gitDir, path))))).some(
      Boolean,
    )
  ) {
    throw new UsableGitError("UNSUPPORTED_STATE", "In-progress Git operations are unsupported for push");
  }

  for (const key of ["core.sparseCheckout", "core.splitIndex"]) {
    const configured = await runner.run(root, ["config", "--bool", "--get", key]);
    if (configured.exitCode === 0 && oneLine(configured.stdout) === "true") {
      throw new UsableGitError("UNSUPPORTED_STATE", `${key} repositories are unsupported for push`);
    }
  }
};

const assertConfiguredRemote = async (
  root: string,
  remote: string,
  runner: GitRunner,
) => {
  const urls = await runner.run(root, ["remote", "get-url", "--push", "--all", remote]);
  const configuredUrls = urls.stdout.split(/\r?\n/).filter(Boolean);
  if (urls.exitCode !== 0 || configuredUrls.length !== 1) {
    throw operationError(
      "INVALID_INPUT",
      configuredUrls.length > 1
        ? `Remote ${remote} has multiple push destinations`
        : `Remote ${remote} is not configured`,
      urls,
    );
  }
};

const resolveSource = async (
  root: string,
  sourceRef: string,
  runner: GitRunner,
) => {
  const exactRef = await runner.run(root, ["show-ref", "--verify", "--hash", sourceRef]);
  if (exactRef.exitCode !== 0) {
    throw operationError("INVALID_INPUT", `Source ref does not exist: ${sourceRef}`, exactRef);
  }
  const commit = await runner.run(root, ["rev-parse", "--verify", `${sourceRef}^{commit}`]);
  if (commit.exitCode !== 0) {
    throw operationError("INVALID_INPUT", `Source ref is not a commit: ${sourceRef}`, commit);
  }
  return oneLine(commit.stdout);
};

type RemoteTarget =
  | { kind: "known"; oid: string | null }
  | { kind: "failed"; result: GitRunResult };

const queryRemoteTarget = async (
  root: string,
  remote: string,
  targetRef: string,
  runner: GitRunner,
): Promise<RemoteTarget> => {
  const result = await runner.run(root, ["ls-remote", "--refs", remote, targetRef]);
  if (result.exitCode !== 0) return { kind: "failed", result };
  const lines = result.stdout.split(/\r?\n/).filter(Boolean);
  if (lines.length === 0) return { kind: "known", oid: null };
  if (lines.length !== 1) return { kind: "failed", result };
  const [oid, ref] = lines[0]?.split("\t") ?? [];
  if (ref !== targetRef || !/^(?:[a-f0-9]{40}|[a-f0-9]{64})$/.test(oid ?? "")) {
    return { kind: "failed", result };
  }
  return { kind: "known", oid: oid ?? null };
};

const classifyPushFailure = (
  result: GitRunResult,
  lease: boolean,
) => {
  const output = `${result.stdout}\n${result.stderr}`.toLowerCase();
  if (lease && /(stale info|force-with-lease)/.test(output)) {
    return operationError("LEASE_REJECTED", "Remote branch rejected the exact lease", result);
  }
  if (/(non-fast-forward|fetch first|tip of your current branch is behind)/.test(output)) {
    return operationError("NON_FAST_FORWARD", "Remote branch is not a fast-forward", result);
  }
  if (
    /(authentication failed|authorization failed|permission denied|could not read username|repository not found|access denied)/.test(
      output,
    )
  ) {
    return operationError("AUTH_FAILED", "Remote authentication or authorization failed", result);
  }
  return operationError("GIT_FAILED", "Remote rejected or failed the push", result);
};

const failureCouldBeAmbiguous = (result: GitRunResult) => {
  const output = `${result.stdout}\n${result.stderr}`.toLowerCase();
  return /(hung up|disconnect|connection|timed out|timeout|unable to access|could not resolve|network|broken pipe|eof)/.test(
    output,
  );
};

const storedSuccess = (result: PushResult): StoredPushOutcome => ({
  schemaVersion: 1,
  kind: "success",
  result,
  resultHash: hashJson(result),
});

const storedError = (error: UsableGitError): StoredPushOutcome => {
  const payload = {
    code: error.code,
    message: error.message,
    ...(error.details ? { details: error.details } : {}),
  };
  return {
    schemaVersion: 1,
    kind: "error",
    error: payload,
    resultHash: hashJson(payload),
  };
};

const replayOutcome = (value: unknown) => {
  const outcome = value as Partial<StoredPushOutcome> | undefined;
  if (!outcome || outcome.schemaVersion !== 1 || !outcome.kind || !outcome.resultHash) {
    throw new UsableGitError("RECOVERY_CONFLICT", "Stored push outcome is invalid");
  }
  if (outcome.kind === "success") {
    const result = pushResultSchema.parse(outcome.result);
    if (hashJson(result) !== outcome.resultHash) {
      throw new UsableGitError("RECOVERY_CONFLICT", "Stored push outcome checksum mismatch");
    }
    return result;
  }
  if (outcome.kind === "error" && outcome.error) {
    if (hashJson(outcome.error) !== outcome.resultHash) {
      throw new UsableGitError("RECOVERY_CONFLICT", "Stored push error checksum mismatch");
    }
    throw new UsableGitError(
      outcome.error.code,
      outcome.error.message,
      outcome.error.details,
    );
  }
  throw new UsableGitError("RECOVERY_CONFLICT", "Stored push outcome is invalid");
};

const pushStartedRecovery = (
  request: PushRequest,
  sourceOid: string,
  oldTargetOid: string | null,
): PushStartedRecovery => {
  const payload = {
    schemaVersion: 1 as const,
    kind: "push_started" as const,
    remote: request.remote,
    sourceRef: request.sourceRef,
    targetRef: request.targetRef,
    sourceOid,
    oldTargetOid,
  };
  return { ...payload, recoveryHash: hashJson(payload) };
};

const parsePushStartedRecovery = (
  value: unknown,
  request: PushRequest,
): PushStartedRecovery => {
  const recovery = value as Partial<PushStartedRecovery> | undefined;
  if (
    !recovery ||
    recovery.schemaVersion !== 1 ||
    recovery.kind !== "push_started" ||
    recovery.remote !== request.remote ||
    recovery.sourceRef !== request.sourceRef ||
    recovery.targetRef !== request.targetRef ||
    recovery.sourceOid !== request.expectedSourceOid ||
    !recovery.recoveryHash
  ) {
    throw new UsableGitError(
      "RECOVERY_CONFLICT",
      "Stored push recovery metadata is invalid",
    );
  }
  const { recoveryHash, ...payload } = recovery;
  if (hashJson(payload) !== recoveryHash) {
    throw new UsableGitError(
      "RECOVERY_CONFLICT",
      "Stored push recovery metadata checksum mismatch",
    );
  }
  return recovery as PushStartedRecovery;
};

export const push = async (
  input: PushRequest,
  options: PushOptions = {},
): Promise<PushResult> => {
  const parsed = pushRequestSchema.safeParse(input);
  if (!parsed.success) {
    throw new UsableGitError("INVALID_INPUT", "Invalid push request", {
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
    throw new UsableGitError("UNSUPPORTED_STATE", "Bare repositories are unsupported for push");
  }

  let lock;
  try {
    lock = await acquireRepositoryLock(repository.commonDir, {
      stateRoot: options.stateRoot,
    });
  } catch (error) {
    if (error instanceof RepositoryBusyError) {
      throw new UsableGitError("BUSY_REPOSITORY", "Repository is busy with another mutation");
    }
    throw error;
  }

  const journal = createOperationJournal({ stateRoot: options.stateRoot });
  const repoKey = createHash("sha256").update(repository.commonDir).digest("hex");
  const inputHash = hashJson(request);

  try {
    let journalStart;
    try {
      journalStart = await journal.begin({
        requestId: request.requestId,
        operation: "push",
        repoKey,
        inputHash,
      });
    } catch (error) {
      if (error instanceof IdempotencyConflictError) {
        throw new UsableGitError(
          "RECOVERY_CONFLICT",
          "requestId was already used for a different mutation",
        );
      }
      throw error;
    }

    if (journalStart.kind === "replay") return replayOutcome(journalStart.result);
    if (journalStart.kind === "started") {
      await options.mutationProbe?.("journal:started");
    }
    if (
      journalStart.kind === "resume" &&
      journalStart.record.phase === "push_started"
    ) {
      const recovery = parsePushStartedRecovery(journalStart.record.result, request);
      const sourceOid = await resolveSource(
        repository.root,
        request.sourceRef,
        runner,
      );
      if (sourceOid !== recovery.sourceOid) {
        throw new UsableGitError(
          "STALE_STATE",
          "Source ref changed during interrupted push recovery",
        );
      }
      const remote = await queryRemoteTarget(
        repository.root,
        recovery.remote,
        recovery.targetRef,
        runner,
      );
      if (remote.kind === "known" && remote.oid === recovery.sourceOid) {
        const result = pushResultSchema.parse({
          remote: recovery.remote,
          sourceRef: recovery.sourceRef,
          targetRef: recovery.targetRef,
          oldTargetOid: recovery.oldTargetOid,
          newTargetOid: recovery.sourceOid,
          mode: request.mode.kind,
          confirmedAfterFailure: true,
        });
        await journal.complete(repoKey, request.requestId, storedSuccess(result));
        return result;
      }
      throw new UsableGitError(
        "NETWORK_AMBIGUITY",
        "Interrupted push did not produce the exact expected remote target; refusing to retry",
        {
          requestId: request.requestId,
          expectedTargetOid: recovery.sourceOid,
          actualTargetOid: remote.kind === "known" ? remote.oid : null,
        },
      );
    }
    if (
      journalStart.kind === "resume" &&
      journalStart.record.phase !== "started"
    ) {
      throw new UsableGitError(
        "NETWORK_AMBIGUITY",
        "Prior push with this requestId has an unknown nonterminal phase; refusing to retry",
        { requestId: request.requestId },
      );
    }

    await assertSupportedState(repository.root, repository.gitDir, runner);
    await assertConfiguredRemote(repository.root, request.remote, runner);
    const sourceOid = await resolveSource(repository.root, request.sourceRef, runner);
    if (sourceOid !== request.expectedSourceOid) {
      const error = new UsableGitError("STALE_STATE", "Source ref changed since inspection", {
        expectedSourceOid: request.expectedSourceOid,
        actualSourceOid: sourceOid,
      });
      await journal.complete(repoKey, request.requestId, storedError(error));
      throw error;
    }

    const before = await queryRemoteTarget(
      repository.root,
      request.remote,
      request.targetRef,
      runner,
    );
    if (before.kind === "failed") {
      const error = classifyPushFailure(before.result, false);
      await journal.complete(repoKey, request.requestId, storedError(error));
      throw error;
    }
    if (
      request.mode.kind === "force-with-lease" &&
      before.oid !== request.mode.expectedTargetOid
    ) {
      const error = new UsableGitError("LEASE_REJECTED", "Remote target does not match exact lease", {
        expectedTargetOid: request.mode.expectedTargetOid,
        actualTargetOid: before.oid,
      });
      await journal.complete(repoKey, request.requestId, storedError(error));
      throw error;
    }

    const sourceOidImmediatelyBeforePush = await resolveSource(
      repository.root,
      request.sourceRef,
      runner,
    );
    if (sourceOidImmediatelyBeforePush !== request.expectedSourceOid) {
      const error = new UsableGitError(
        "STALE_STATE",
        "Source ref changed immediately before push",
        {
          expectedSourceOid: request.expectedSourceOid,
          actualSourceOid: sourceOidImmediatelyBeforePush,
        },
      );
      await journal.complete(repoKey, request.requestId, storedError(error));
      throw error;
    }

    const refspec = `${request.sourceRef}:${request.targetRef}`;
    const arguments_ = [
      "push",
      "--porcelain",
      ...(request.mode.kind === "force-with-lease"
        ? [
            `--force-with-lease=${request.targetRef}:${request.mode.expectedTargetOid}`,
          ]
        : []),
      request.remote,
      refspec,
    ];
    await journal.transition(
      repoKey,
      request.requestId,
      "push_started",
      pushStartedRecovery(request, sourceOid, before.oid),
    );
    await options.mutationProbe?.("journal:push_started");
    const pushed = await runner.run(repository.root, arguments_);
    await options.mutationProbe?.("remote:returned");

    if (pushed.exitCode === 0) {
      const result = pushResultSchema.parse({
        remote: request.remote,
        sourceRef: request.sourceRef,
        targetRef: request.targetRef,
        oldTargetOid: before.oid,
        newTargetOid: sourceOid,
        mode: request.mode.kind,
        confirmedAfterFailure: false,
      });
      await journal.complete(repoKey, request.requestId, storedSuccess(result));
      await options.mutationProbe?.("journal:terminal");
      return result;
    }

    const classified = classifyPushFailure(
      pushed,
      request.mode.kind === "force-with-lease",
    );
    if (!failureCouldBeAmbiguous(pushed)) {
      await journal.complete(repoKey, request.requestId, storedError(classified));
      throw classified;
    }

    const after = await queryRemoteTarget(
      repository.root,
      request.remote,
      request.targetRef,
      runner,
    );
    if (after.kind === "known" && after.oid === sourceOid) {
      const result = pushResultSchema.parse({
        remote: request.remote,
        sourceRef: request.sourceRef,
        targetRef: request.targetRef,
        oldTargetOid: before.oid,
        newTargetOid: sourceOid,
        mode: request.mode.kind,
        confirmedAfterFailure: true,
      });
      await journal.complete(repoKey, request.requestId, storedSuccess(result));
      return result;
    }
    if (after.kind === "known" && after.oid === before.oid) {
    await journal.complete(repoKey, request.requestId, storedError(classified));
      throw classified;
    }
    throw new UsableGitError(
      "NETWORK_AMBIGUITY",
      "Push transport failed and explicit remote target state is ambiguous; retry with this requestId to resolve",
      { requestId: request.requestId },
    );
  } finally {
    await lock.release();
  }
};

import { z } from "zod";
import {
  operationSchema,
  v1EnvelopeSchema,
  type ErrorCode,
  type V1Envelope,
} from "./contracts/v1.ts";
import { parseOperationResult } from "./contracts/v1/results.ts";
import { UsableGitError } from "./errors.ts";
import {
  GitCommandError,
  gitSubprocessCountForError,
  withGitMetrics,
} from "./git/runner.ts";
import { history } from "./operations/history.ts";
import { inspect } from "./operations/inspect.ts";
import { review } from "./operations/review.ts";
import {
  createTelemetrySink,
  type TelemetryEventInput,
  type TelemetrySink,
} from "./telemetry/event.ts";
import { estimateGain } from "./gain/estimate.ts";
import { createGainLedger, type GainLedger } from "./gain/ledger.ts";

export type Operation = z.infer<typeof operationSchema>;
export type ServiceOptions = {
  transport: "mcp" | "cli";
  client?: TelemetryEventInput["client"];
  clientVersion?: string;
  telemetrySink?: TelemetrySink;
  gainLedger?: GainLedger;
};

const requestedPath = (input: unknown) =>
  input && typeof input === "object" && "repoPath" in input && typeof input.repoPath === "string"
    ? input.repoPath
    : "<missing>";

const requestId = (input: unknown) =>
  input && typeof input === "object" && "requestId" in input && typeof input.requestId === "string"
    ? input.requestId
    : undefined;

const MUTATION_OPERATIONS: ReadonlySet<Operation> = new Set([
  "publish",
  "push",
  "ship",
  "branch",
  "update",
]);

// Mutations get a server-generated requestId when the caller omits one, so the
// envelope can echo the id agents need for idempotent retry after ambiguity.
const withRequestId = (operation: Operation, input: unknown) =>
  MUTATION_OPERATIONS.has(operation) &&
  input && typeof input === "object" && !requestId(input)
    ? { ...input, requestId: `auto-${crypto.randomUUID().replaceAll("-", "").slice(0, 12)}` }
    : input;

const invoke = async (operation: Operation, input: unknown) => {
  let result: unknown;
  switch (operation) {
    case "inspect":
      result = await inspect(input as Parameters<typeof inspect>[0]);
      break;
    case "review":
      result = await review(input as Parameters<typeof review>[0]);
      break;
    case "history":
      result = await history(input as Parameters<typeof history>[0]);
      break;
    case "publish": {
      const { publish } = await import("./operations/publish.ts");
      result = await publish(input as Parameters<typeof publish>[0]);
      break;
    }
    case "push": {
      const { push } = await import("./operations/push.ts");
      result = await push(input as Parameters<typeof push>[0]);
      break;
    }
    case "ship": {
      const { ship } = await import("./operations/ship.ts");
      result = await ship(input as Parameters<typeof ship>[0]);
      break;
    }
    case "diff": {
      const { diff } = await import("./operations/diff.ts");
      result = await diff(input as Parameters<typeof diff>[0]);
      break;
    }
    case "branch": {
      const { branch } = await import("./operations/branch.ts");
      result = await branch(input as Parameters<typeof branch>[0]);
      break;
    }
    case "sync": {
      const { sync } = await import("./operations/sync.ts");
      result = await sync(input as Parameters<typeof sync>[0]);
      break;
    }
    case "update": {
      const { update } = await import("./operations/update.ts");
      result = await update(input as Parameters<typeof update>[0]);
      break;
    }
    case "search": {
      const { search } = await import("./operations/search.ts");
      result = await search(input as Parameters<typeof search>[0]);
      break;
    }
  }
  return parseOperationResult(operation, result);
};

const sanitizeDiagnostic = (message: string) =>
  message
    .replace(/([a-z][a-z0-9+.-]*:\/\/)[^\s/@]+:[^\s/@]+@/gi, "$1<redacted>@")
    .slice(0, 2_000);

const classifyError = (error: unknown): { code: ErrorCode; message: string } => {
  if (error instanceof UsableGitError) {
    return { code: error.code, message: sanitizeDiagnostic(error.message) };
  }
  if (error instanceof z.ZodError) {
    return { code: "INVALID_INPUT", message: "Request failed v1 validation" };
  }
  if (error instanceof GitCommandError) {
    const diagnostic = error.stderr || error.message;
    return {
      code: /not a git repository/i.test(diagnostic) ? "INVALID_REPOSITORY" : "GIT_FAILED",
      message: sanitizeDiagnostic(diagnostic.trim() || "Git command failed"),
    };
  }
  return {
    code: "INVARIANT_VIOLATION",
    message: error instanceof Error ? sanitizeDiagnostic(error.message) : "Unknown operation failure",
  };
};

const repositoryIdentity = (input: unknown, result?: unknown) => {
  const value = result && typeof result === "object" ? result as Record<string, unknown> : undefined;
  const repository = value?.repository && typeof value.repository === "object"
    ? value.repository as Record<string, unknown>
    : undefined;
  return typeof repository?.root === "string" ? repository.root : requestedPath(input);
};

const countArray = (value: unknown, key: string) => {
  if (!value || typeof value !== "object") return 0;
  const candidate = (value as Record<string, unknown>)[key];
  return Array.isArray(candidate) ? candidate.length : 0;
};

export type OperationMetrics = {
  operation: Operation;
  durationMs: number;
  gitSubprocessCount: number;
};

const emitTelemetry = async (
  envelope: V1Envelope,
  metrics: OperationMetrics,
  input: unknown,
  options: ServiceOptions,
) => {
  const sink = options.telemetrySink ?? createTelemetrySink({
    enabled: process.env.USABLE_GIT_TELEMETRY === "1",
  });
  const result = envelope.ok ? envelope.result : undefined;
  const selected = input && typeof input === "object" && "files" in input && Array.isArray(input.files)
    ? input.files.length
    : 0;
  try {
    await sink.emit({
      operation: metrics.operation,
      client: options.client ?? "other",
      transport: options.transport,
      durationMs: metrics.durationMs,
      gitSubprocessCount: metrics.gitSubprocessCount,
      resultCode: envelope.ok ? "success" : envelope.error.code,
      counts: {
        selected,
        staged: countArray(result, "staged"),
        unstaged: countArray(result, "unstaged"),
        untracked: countArray(result, "untracked"),
        conflicted: countArray(result, "conflicted"),
        commits: countArray(result, "commits") || (
          result && typeof result === "object" && "commitOid" in result ? 1 : 0
        ),
        warnings: envelope.warnings?.length ?? 0,
      },
      components: {
        usableGit: "0.1.0",
        bun: Bun.version,
        git: "unknown",
        client: options.clientVersion ?? "unknown",
      },
      repositoryIdentity: repositoryIdentity(input, result),
    });
  } catch {
    // Telemetry is best-effort and must never change repository semantics.
  }
};

// Gain ledger is best-effort: a failure to record savings must never change
// repository semantics. It is always-on (local-only metadata, no repo content)
// unless the caller injects a disabled ledger.
const emitGain = async (
  envelope: V1Envelope,
  metrics: OperationMetrics,
  input: unknown,
  options: ServiceOptions,
) => {
  const ledger = options.gainLedger ?? createGainLedger();
  const envelopeBytes = Buffer.byteLength(JSON.stringify(envelope), "utf8");
  const estimate = estimateGain(
    metrics.operation,
    envelopeBytes,
    metrics.gitSubprocessCount,
  );
  const result = envelope.ok ? envelope.result : undefined;
  try {
    await ledger.append({
      operation: metrics.operation,
      client: options.client ?? "other",
      transport: options.transport,
      resultCode: envelope.ok ? "success" : envelope.error.code,
      repositoryIdentity: repositoryIdentity(input, result),
      envelopeBytes: estimate.envelopeBytes,
      rawEquivalentBytes: estimate.rawEquivalentBytes,
      agentOpsRaw: estimate.agentOpsRaw,
      agentOpsActual: estimate.agentOpsActual,
      gitSubprocessesRaw: estimate.gitSubprocessesRaw,
      gitSubprocessesActual: estimate.gitSubprocessesActual,
      durationMs: metrics.durationMs,
      tokensSaved: estimate.tokensSaved,
    });
  } catch {
    // Gain is best-effort and must never change repository semantics.
  }
};

export const executeOperationWithMetrics = async (
  rawOperation: Operation,
  rawInput: unknown,
  options: ServiceOptions,
): Promise<{ envelope: V1Envelope; metrics: OperationMetrics }> => {
  const startedAt = performance.now();
  const operation = operationSchema.parse(rawOperation);
  const input = withRequestId(operation, rawInput);
  try {
    const measured = await withGitMetrics(() => invoke(operation, input));
    const envelope = v1EnvelopeSchema.parse({
      ok: true,
      ...(requestId(input) ? { requestId: requestId(input) } : {}),
      result: measured.result,
    });
    const metrics: OperationMetrics = {
      operation,
      durationMs: performance.now() - startedAt,
      gitSubprocessCount: measured.gitSubprocessCount,
    };
    await emitTelemetry(envelope, metrics, input, options);
    await emitGain(envelope, metrics, input, options);
    return { envelope, metrics };
  } catch (error) {
    const envelope = v1EnvelopeSchema.parse({
      ok: false,
      ...(requestId(input) ? { requestId: requestId(input) } : {}),
      error: classifyError(error),
    });
    const metrics: OperationMetrics = {
      operation,
      durationMs: performance.now() - startedAt,
      gitSubprocessCount: gitSubprocessCountForError(error),
    };
    await emitTelemetry(envelope, metrics, input, options);
    await emitGain(envelope, metrics, input, options);
    return { envelope, metrics };
  }
};

export const executeOperation = async (
  rawOperation: Operation,
  input: unknown,
  options: ServiceOptions,
): Promise<V1Envelope> =>
  (await executeOperationWithMetrics(rawOperation, input, options)).envelope;

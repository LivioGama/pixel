import {
  shipRequestSchema,
  shipResultSchema,
  type ShipRequest,
  type ShipResult,
} from "../contracts/v1/ship.ts";
import { UsableGitError } from "../errors.ts";
import type { GitRunner } from "../git/runner.ts";
import { generatedRequestId, publish } from "./publish.ts";
import { push } from "./push.ts";

export type { ShipRequest, ShipResult } from "../contracts/v1/ship.ts";

type ShipOptions = {
  stateRoot?: string;
  runner?: GitRunner;
};

// Composes publish then push under derived requestIds so each leg keeps its
// own journal and crash recovery. A push-leg failure never masks the landed
// commit: the result reports the commit with push.ok false instead of failing
// the whole operation, so agents retry only the push.
export const ship = async (
  input: ShipRequest,
  options: ShipOptions = {},
): Promise<ShipResult> => {
  const parsed = shipRequestSchema.safeParse(input);
  if (!parsed.success) {
    throw new UsableGitError("INVALID_INPUT", "Invalid ship request", {
      issues: parsed.error.issues.map(({ path, message }) => ({ path, message })),
    });
  }
  const request = parsed.data;
  const requestId = request.requestId ?? generatedRequestId();

  const published = await publish({
    repoPath: request.repoPath,
    files: request.files,
    message: request.message,
    requestId: `${requestId}.publish`,
    ...(request.snapshot !== undefined ? { snapshot: request.snapshot } : {}),
    ...(request.expected !== undefined ? { expected: request.expected } : {}),
  }, options);

  const warnings = [...published.warnings];
  if (published.head.branch === "(unknown)") {
    return shipResultSchema.parse({
      commit: published.commitOid,
      branch: published.head.branch,
      committedPaths: published.committedPaths,
      push: {
        ok: false,
        code: "UNSUPPORTED_STATE",
        message:
          `Commit ${published.commitOid} landed but the current branch could not be observed; ` +
          "push the commit explicitly with the push tool",
      },
      warnings,
    });
  }

  const sourceRef = `refs/heads/${published.head.branch}`;
  const targetRef = request.targetRef ?? sourceRef;
  try {
    const pushed = await push({
      repoPath: request.repoPath,
      remote: request.remote,
      sourceRef,
      targetRef,
      requestId: `${requestId}.push`,
      expectedSourceOid: published.commitOid,
      mode: request.mode ?? { kind: "fast-forward" },
    }, options);
    return shipResultSchema.parse({
      commit: published.commitOid,
      branch: published.head.branch,
      committedPaths: published.committedPaths,
      push: {
        ok: true,
        remote: pushed.remote,
        targetRef: pushed.targetRef,
        oldTargetOid: pushed.oldTargetOid,
        newTargetOid: pushed.newTargetOid,
      },
      warnings,
    });
  } catch (error) {
    const code = error instanceof UsableGitError ? error.code : "GIT_FAILED";
    const details = error instanceof UsableGitError ? error.details : undefined;
    const message = error instanceof Error ? error.message : String(error);
    return shipResultSchema.parse({
      commit: published.commitOid,
      branch: published.head.branch,
      committedPaths: published.committedPaths,
      push: {
        ok: false,
        code,
        message:
          `${message} — commit ${published.commitOid} exists locally; ` +
          `retry with the push tool or ship requestId ${requestId}`,
        ...(details ? { details } : {}),
      },
      warnings,
    });
  }
};

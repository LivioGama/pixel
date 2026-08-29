import { isAbsolute } from "node:path";
import { z } from "zod";

export const absolutePathSchema = z
  .string()
  .min(1)
  .refine(isAbsolute, "repoPath must be absolute");

export const literalFileSchema = z.string().min(1);

const repositoryRequestSchema = z.object({
  repoPath: absolutePathSchema,
});

export const inspectRequestSchema = repositoryRequestSchema.extend({
  files: z.array(literalFileSchema).min(1).optional(),
});

export const reviewRequestSchema = repositoryRequestSchema.extend({
  files: z.array(literalFileSchema).min(1).optional(),
  cursor: z.string().min(1).optional(),
  byteCap: z.number().int().min(128).max(1_000_000).default(64_000),
});

export const historyRequestSchema = repositoryRequestSchema.extend({
  ref: z.string().min(1).default("HEAD"),
  limit: z.number().int().min(1).max(100).default(20),
  detail: z.enum(["compact", "full"]).default("compact"),
  cursor: z.string().min(1).optional(),
  byteCap: z.number().int().min(1_024).max(1_000_000).optional(),
});

export const errorCodeSchema = z.enum([
  "INVALID_INPUT",
  "INVALID_REPOSITORY",
  "INVALID_PATH",
  "UNSUPPORTED_STATE",
  "STALE_STATE",
  "BUSY_REPOSITORY",
  "NOTHING_TO_COMMIT",
  "HOOK_FAILED",
  "SIGNING_FAILED",
  "IDENTITY_MISSING",
  "AUTH_FAILED",
  "NON_FAST_FORWARD",
  "LEASE_REJECTED",
  "NETWORK_AMBIGUITY",
  "RECOVERY_CONFLICT",
  "INVARIANT_VIOLATION",
  "REF_EXISTS",
  "GIT_FAILED",
]);

export const operationSchema = z.enum([
  "inspect",
  "review",
  "history",
  "diff",
  "publish",
  "push",
  "ship",
  "branch",
  "sync",
  "update",
  "search",
]);

export const warningSchema = z.object({
  code: z.string().min(1),
  message: z.string().min(1),
}).strict();

const envelopeBase = {
  requestId: z.string().min(1).optional(),
  warnings: z.array(warningSchema).min(1).optional(),
};

const operationErrorSchema = z.object({
  code: errorCodeSchema,
  message: z.string(),
  details: z.record(z.string(), z.unknown()).optional(),
}).strict();

export const v1EnvelopeSchema = z.discriminatedUnion("ok", [
  z.object({
    ...envelopeBase,
    ok: z.literal(true),
    result: z.unknown(),
  }).strict(),
  z.object({
    ...envelopeBase,
    ok: z.literal(false),
    error: operationErrorSchema,
  }).strict(),
]);

export const v1McpEnvelopeSchema = z.object({
  ...envelopeBase,
  ok: z.boolean(),
  result: z.unknown().optional(),
  error: operationErrorSchema.optional(),
}).strict().superRefine((value, context) => {
  const hasResult = Object.hasOwn(value, "result");
  const hasError = Object.hasOwn(value, "error");
  if (hasResult === hasError || value.ok !== hasResult) {
    context.addIssue({
      code: "custom",
      message: "ok must agree with exactly one of result or error",
    });
  }
});

export const createV1McpEnvelopeSchema = <TResult extends z.ZodType>(
  resultSchema: TResult,
) => z.object({
  ...envelopeBase,
  ok: z.boolean(),
  result: resultSchema.optional(),
  error: operationErrorSchema.optional(),
}).strict().superRefine((value, context) => {
  const hasResult = Object.hasOwn(value, "result");
  const hasError = Object.hasOwn(value, "error");
  if (hasResult === hasError || value.ok !== hasResult) {
    context.addIssue({
      code: "custom",
      message: "ok must agree with exactly one of result or error",
    });
  }
});

export type InspectRequest = z.infer<typeof inspectRequestSchema>;
export type ReviewRequest = z.input<typeof reviewRequestSchema>;
export type HistoryRequest = z.input<typeof historyRequestSchema>;
export type ErrorCode = z.infer<typeof errorCodeSchema>;
export type Operation = z.infer<typeof operationSchema>;
export type V1Envelope = z.infer<typeof v1EnvelopeSchema>;

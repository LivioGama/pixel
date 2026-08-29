import { isAbsolute, normalize } from "node:path";
import { z } from "zod";

const requestIdSchema = z
  .string()
  .regex(/^[A-Za-z0-9._-]{1,128}$/);

const objectIdSchema = z
  .string()
  .regex(/^(?:[a-f0-9]{40}|[a-f0-9]{64})$/);

const fingerprintSchema = z.string().regex(/^[a-f0-9]{64}$/);

const literalPublishFileSchema = z.string().min(1).superRefine((file, context) => {
  if (
    file === "." ||
    isAbsolute(file) ||
    file.startsWith(":") ||
    /[*?[\]]/.test(file) ||
    normalize(file) !== file ||
    file.split(/[\\/]/).includes("..")
  ) {
    context.addIssue({
      code: "custom",
      message: "publish files must be literal repository-relative file paths",
    });
  }
});

export const expectedHeadSchema = z.discriminatedUnion("kind", [
  z.object({ kind: z.literal("oid"), oid: objectIdSchema }).strict(),
  z.object({ kind: z.literal("unborn") }).strict(),
]);

export const snapshotTokenSchema = z.string().regex(/^[a-f0-9]{12}$/);

export const expectedStateSchema = z
  .object({
    head: expectedHeadSchema,
    fingerprints: z.record(z.string(), fingerprintSchema),
  })
  .strict();

export const publishModeSchema = z.discriminatedUnion("kind", [
  z.object({ kind: z.literal("append") }).strict(),
  z.object({ kind: z.literal("amend") }).strict(),
]);

export const publishRequestSchema = z
  .object({
    repoPath: z.string().min(1).refine(isAbsolute, "repoPath must be absolute"),
    files: z.array(literalPublishFileSchema).min(1).max(10_000),
    message: z.string().max(65_536).refine((value) => value.trim().length > 0, "message must not be blank").optional(),
    mode: publishModeSchema.default({ kind: "append" }),
    requestId: requestIdSchema.optional(),
    snapshot: snapshotTokenSchema.optional(),
    expected: expectedStateSchema.optional(),
  })
  .strict()
  .superRefine((request, context) => {
    const files = new Set(request.files);
    if (files.size !== request.files.length) {
      context.addIssue({
        code: "custom",
        path: ["files"],
        message: "publish files must be unique",
      });
    }

    if (request.mode.kind === "append" && request.message === undefined) {
      context.addIssue({
        code: "custom",
        path: ["message"],
        message: "append publish requires a commit message",
      });
    }

    if ((request.snapshot === undefined) === (request.expected === undefined)) {
      context.addIssue({
        code: "custom",
        message: "publish requires exactly one of snapshot or expected",
      });
    }

    if (request.expected) {
      const fingerprintPaths = Object.keys(request.expected.fingerprints);
      if (
        fingerprintPaths.length !== files.size ||
        fingerprintPaths.some((path) => !files.has(path))
      ) {
        context.addIssue({
          code: "custom",
          path: ["expected", "fingerprints"],
          message: "expected.fingerprints must contain exactly one entry for every file",
        });
      }
    }
  });

export const publishResultSchema = z
  .object({
    commitOid: objectIdSchema,
    amendedOid: objectIdSchema.optional(),
    committedPaths: z.array(literalPublishFileSchema),
    head: z
      .object({
        oid: objectIdSchema,
        branch: z.string().min(1),
      })
      .strict(),
    status: z
      .object({
        staged: z.array(literalPublishFileSchema),
        unstaged: z.array(literalPublishFileSchema),
        untracked: z.array(literalPublishFileSchema),
        conflicted: z.array(literalPublishFileSchema),
      })
      .strict(),
    warnings: z.array(z.string()),
  })
  .strict();

export type PublishRequest = z.input<typeof publishRequestSchema>;
export type ParsedPublishRequest = z.infer<typeof publishRequestSchema>;
export type ExpectedHead = z.infer<typeof expectedHeadSchema>;
export type PublishResult = z.infer<typeof publishResultSchema>;

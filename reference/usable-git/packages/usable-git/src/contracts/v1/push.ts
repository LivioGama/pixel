import { isAbsolute } from "node:path";
import { z } from "zod";

export const pushObjectIdSchema = z
  .string()
  .regex(/^(?:[a-f0-9]{40}|[a-f0-9]{64})$/);

const requestIdSchema = z.string().regex(/^[A-Za-z0-9._-]{1,128}$/);

export const remoteNameSchema = z
  .string()
  .min(1)
  .max(128)
  .regex(/^[A-Za-z0-9][A-Za-z0-9._-]*$/)
  .refine((value) => !value.includes(".."), "remote must be a configured remote name");

export const fullBranchRefSchema = z
  .string()
  .min("refs/heads/a".length)
  .max(1024)
  .superRefine((value, context) => {
    const suffix = value.slice("refs/heads/".length);
    const components = suffix.split("/");
    const invalid =
      !value.startsWith("refs/heads/") ||
      suffix.length === 0 ||
      suffix.startsWith(".") ||
      suffix.endsWith(".") ||
      suffix.endsWith("/") ||
      suffix.includes("..") ||
      suffix.includes("@{") ||
      suffix.includes("//") ||
      /[\u0000-\u0020\u007f~^:?*[\\]/.test(suffix) ||
      components.some(
        (component) =>
          component.length === 0 ||
          component.startsWith(".") ||
          component.endsWith(".lock"),
      );

    if (invalid) {
      context.addIssue({
        code: "custom",
        message: "push refs must be full literal refs under refs/heads/",
      });
    }
  });

export const pushModeSchema = z.discriminatedUnion("kind", [
  z.object({ kind: z.literal("fast-forward") }).strict(),
  z
    .object({
      kind: z.literal("force-with-lease"),
      expectedTargetOid: pushObjectIdSchema,
    })
    .strict(),
]);

export const pushRequestSchema = z
  .object({
    repoPath: z.string().min(1).refine(isAbsolute, "repoPath must be absolute"),
    remote: remoteNameSchema,
    sourceRef: fullBranchRefSchema,
    targetRef: fullBranchRefSchema,
    requestId: requestIdSchema.optional(),
    expectedSourceOid: pushObjectIdSchema,
    mode: pushModeSchema,
  })
  .strict();

export const pushResultSchema = z
  .object({
    remote: remoteNameSchema,
    sourceRef: fullBranchRefSchema,
    targetRef: fullBranchRefSchema,
    oldTargetOid: pushObjectIdSchema.nullable(),
    newTargetOid: pushObjectIdSchema,
    mode: z.enum(["fast-forward", "force-with-lease"]),
    confirmedAfterFailure: z.boolean(),
  })
  .strict();

export type PushRequest = z.infer<typeof pushRequestSchema>;
export type PushResult = z.infer<typeof pushResultSchema>;

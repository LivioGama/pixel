import { isAbsolute } from "node:path";
import { z } from "zod";
import { errorCodeSchema } from "../v1.ts";
import {
  expectedStateSchema,
  snapshotTokenSchema,
} from "./publish.ts";
import {
  fullBranchRefSchema,
  pushModeSchema,
  pushObjectIdSchema,
  remoteNameSchema,
} from "./push.ts";

const requestIdSchema = z.string().regex(/^[A-Za-z0-9._-]{1,128}$/);

const literalShipFileSchema = z.string().min(1).superRefine((file, context) => {
  if (
    file === "." ||
    isAbsolute(file) ||
    file.startsWith(":") ||
    /[*?[\]]/.test(file) ||
    file.split(/[\\/]/).includes("..")
  ) {
    context.addIssue({
      code: "custom",
      message: "ship files must be literal repository-relative file paths",
    });
  }
});

export const shipRequestSchema = z
  .object({
    repoPath: z.string().min(1).refine(isAbsolute, "repoPath must be absolute"),
    files: z.array(literalShipFileSchema).min(1).max(10_000),
    message: z.string().max(65_536).refine((value) => value.trim().length > 0, "message must not be blank"),
    requestId: requestIdSchema.optional(),
    snapshot: snapshotTokenSchema.optional(),
    expected: expectedStateSchema.optional(),
    remote: remoteNameSchema,
    targetRef: fullBranchRefSchema.optional(),
    mode: pushModeSchema.optional(),
  })
  .strict()
  .superRefine((request, context) => {
    if (new Set(request.files).size !== request.files.length) {
      context.addIssue({
        code: "custom",
        path: ["files"],
        message: "ship files must be unique",
      });
    }
    if ((request.snapshot === undefined) === (request.expected === undefined)) {
      context.addIssue({
        code: "custom",
        message: "ship requires exactly one of snapshot or expected",
      });
    }
  });

export const shipResultSchema = z
  .object({
    commit: pushObjectIdSchema,
    branch: z.string().min(1),
    committedPaths: z.array(z.string().min(1)),
    push: z.discriminatedUnion("ok", [
      z
        .object({
          ok: z.literal(true),
          remote: remoteNameSchema,
          targetRef: fullBranchRefSchema,
          oldTargetOid: pushObjectIdSchema.nullable(),
          newTargetOid: pushObjectIdSchema,
        })
        .strict(),
      z
        .object({
          ok: z.literal(false),
          code: errorCodeSchema,
          message: z.string(),
          details: z.record(z.string(), z.unknown()).optional(),
        })
        .strict(),
    ]),
    warnings: z.array(z.string()),
  })
  .strict();

export type ShipRequest = z.infer<typeof shipRequestSchema>;
export type ShipResult = z.infer<typeof shipResultSchema>;

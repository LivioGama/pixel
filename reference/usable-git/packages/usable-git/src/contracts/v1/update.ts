import { isAbsolute } from "node:path";
import { z } from "zod";
import { objectIdSchema } from "./result-primitives.ts";

const requestIdSchema = z.string().regex(/^[A-Za-z0-9._-]{1,128}$/);

// Fast-forward-only advance of the current branch to an exact target oid the
// agent observed via sync — the local mirror of push's lease design.
export const updateRequestSchema = z
  .object({
    repoPath: z.string().min(1).refine(isAbsolute, "repoPath must be absolute"),
    requestId: requestIdSchema.optional(),
    expectedHead: z.object({ kind: z.literal("oid"), oid: objectIdSchema }).strict(),
    targetOid: objectIdSchema,
  })
  .strict();

export const updateResultSchema = z
  .object({
    branch: z.string().min(1),
    previousOid: objectIdSchema,
    newOid: objectIdSchema,
    commitsAdvanced: z.number().int().positive(),
  })
  .strict();

export type UpdateRequest = z.infer<typeof updateRequestSchema>;
export type UpdateResult = z.infer<typeof updateResultSchema>;

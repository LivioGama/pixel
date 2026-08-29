import { z } from "zod";
import { absolutePathSchema } from "../v1.ts";

export const objectIdSchema = z
  .string()
  .regex(/^(?:[a-f0-9]{40}|[a-f0-9]{64})$/);

export const resultPathSchema = z.string().min(1);

export const resultPathListSchema = z.array(resultPathSchema);

export const operationHeadSchema = z.discriminatedUnion("kind", [
  z.object({ kind: z.literal("unborn") }).strict(),
  z.object({ kind: z.literal("oid"), oid: objectIdSchema }).strict(),
]);

export const branchStatusSchema = z
  .object({
    oid: objectIdSchema.nullable(),
    head: z.string().min(1).nullable(),
    upstream: z.string().min(1).nullable(),
    ahead: z.number().int().nonnegative(),
    behind: z.number().int().nonnegative(),
  })
  .strict();

export const resultRepositorySchema = z
  .object({
    root: absolutePathSchema,
    gitDir: absolutePathSchema,
    commonDir: absolutePathSchema,
  })
  .strict();

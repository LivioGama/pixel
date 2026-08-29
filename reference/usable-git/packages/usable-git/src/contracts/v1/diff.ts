import { isAbsolute } from "node:path";
import { z } from "zod";
import { literalFileSchema } from "../v1.ts";
import { reviewItemSchema } from "./review.ts";

// Exact object ids only — abbreviated (>=12 hex) or full. No ref names, no
// revision expressions: agents obtain oids from inspect/history/sync first.
export const diffOidSchema = z.string().regex(/^[a-f0-9]{12,64}$/);

export const diffRequestSchema = z
  .object({
    repoPath: z.string().min(1).refine(isAbsolute, "repoPath must be absolute"),
    target: z.discriminatedUnion("kind", [
      z.object({ kind: z.literal("range"), baseOid: diffOidSchema, targetOid: diffOidSchema }).strict(),
      z.object({ kind: z.literal("commit"), oid: diffOidSchema }).strict(),
    ]),
    files: z.array(literalFileSchema).min(1).optional(),
    cursor: z.string().min(1).optional(),
    byteCap: z.number().int().min(128).max(1_000_000).default(64_000),
  })
  .strict();

export const diffItemSchema = reviewItemSchema.omit({ scope: true });

export const diffResultSchema = z
  .object({
    base: z.string().regex(/^[a-f0-9]{40,64}$/),
    target: z.string().regex(/^[a-f0-9]{40,64}$/),
    items: z.array(diffItemSchema),
    bytes: z.number().int().nonnegative(),
    nextCursor: z.string().min(1).optional(),
  })
  .strict();

export type DiffRequest = z.input<typeof diffRequestSchema>;
export type DiffItem = z.infer<typeof diffItemSchema>;
export type DiffResult = z.infer<typeof diffResultSchema>;

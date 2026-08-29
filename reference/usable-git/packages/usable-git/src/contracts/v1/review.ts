import { z } from "zod";
import { resultPathSchema } from "./result-primitives.ts";

export const reviewItemSchema = z
  .object({
    scope: z.enum(["staged", "unstaged", "untracked"]),
    path: resultPathSchema,
    originalPath: resultPathSchema.optional(),
    patch: z.string(),
    binary: z.boolean(),
    additions: z.number().int().nonnegative(),
    deletions: z.number().int().nonnegative(),
    truncated: z.boolean(),
  })
  .strict();

export const reviewResultSchema = z
  .object({
    items: z.array(reviewItemSchema),
    bytes: z.number().int().nonnegative(),
    nextCursor: z.string().min(1).optional(),
  })
  .strict();

export type ReviewScope = z.infer<typeof reviewItemSchema>["scope"];
export type ReviewItem = z.infer<typeof reviewItemSchema>;
export type ReviewResult = z.infer<typeof reviewResultSchema>;

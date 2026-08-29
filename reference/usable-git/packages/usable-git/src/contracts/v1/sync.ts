import { isAbsolute } from "node:path";
import { z } from "zod";
import { branchNameSchema } from "./branch.ts";
import { remoteNameSchema } from "./push.ts";
import { objectIdSchema } from "./result-primitives.ts";

// Remote-refreshing, locally non-destructive: fetches exactly the named
// branches into refs/remotes/<remote>/, never touching worktree, index,
// local branches, HEAD, or tags.
export const syncRequestSchema = z
  .object({
    repoPath: z.string().min(1).refine(isAbsolute, "repoPath must be absolute"),
    remote: remoteNameSchema,
    branches: z.array(branchNameSchema).min(1).max(16).optional(),
  })
  .strict();

export const syncResultSchema = z
  .object({
    remote: remoteNameSchema,
    fetched: z.array(
      z
        .object({
          branch: z.string().min(1),
          ref: z.string().min(1),
          oldOid: objectIdSchema.nullable(),
          newOid: objectIdSchema.nullable(),
          updated: z.boolean(),
        })
        .strict(),
    ),
    branch: z
      .object({
        name: z.string().min(1),
        ahead: z.number().int().nonnegative(),
        behind: z.number().int().nonnegative(),
      })
      .strict()
      .nullable(),
  })
  .strict();

export type SyncRequest = z.infer<typeof syncRequestSchema>;
export type SyncResult = z.infer<typeof syncResultSchema>;

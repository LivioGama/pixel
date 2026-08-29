import { z } from "zod";
import { absolutePathSchema } from "../v1.ts";
import { objectIdSchema, resultPathSchema } from "./result-primitives.ts";

export const snapshotTokenSchema = z.string().regex(/^[a-f0-9]{12}$/);

// One compact entry per changed path. `status` is the porcelain v2 XY pair
// ("M.", ".M", "??", "UU", …) — the vocabulary agents already know from git.
export const inspectedChangeSchema = z
  .object({
    path: resultPathSchema,
    status: z.string().length(2),
    from: resultPathSchema.optional(),
  })
  .strict();

export const inspectRemoteSchema = z
  .object({
    name: z.string().min(1),
    fetchUrl: z.string().min(1).nullable(),
    pushUrl: z.string().min(1).nullable(),
  })
  .strict();

export const inspectResultSchema = z
  .object({
    root: absolutePathSchema,
    branch: z.string().min(1).nullable(),
    head: objectIdSchema.nullable(),
    upstream: z
      .object({
        ref: z.string().min(1),
        ahead: z.number().int().nonnegative(),
        behind: z.number().int().nonnegative(),
      })
      .strict()
      .optional(),
    state: z
      .array(z.enum(["merge", "cherry-pick", "revert", "rebase", "bisect", "sequencer"]))
      .min(1)
      .optional(),
    stashes: z.number().int().positive().optional(),
    snapshot: snapshotTokenSchema.optional(),
    remotes: z.array(inspectRemoteSchema).min(1).optional(),
    changes: z.array(inspectedChangeSchema),
  })
  .strict();

export type InspectedChange = z.infer<typeof inspectedChangeSchema>;
export type InspectResult = z.infer<typeof inspectResultSchema>;

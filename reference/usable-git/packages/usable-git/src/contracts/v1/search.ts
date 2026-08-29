import { isAbsolute } from "node:path";
import { z } from "zod";
import { operationHeadSchema } from "./result-primitives.ts";

// Short oid on the wire: hits feed the existing diff operation verbatim.
export const searchHitOidSchema = z.string().regex(/^[a-f0-9]{12}$/);

const textTargetSchema = z
  .object({
    kind: z.literal("text"),
    query: z.string().min(1).max(512),
    scope: z.enum(["message", "path", "diff", "all"]).default("all"),
  })
  .strict();

const lifecycleTargetSchema = z
  .object({
    kind: z.literal("lifecycle"),
    path: z.string().min(1).optional(),
    token: z.string().min(1).max(512).optional(),
  })
  .strict()
  .superRefine((value, context) => {
    if ((value.path === undefined) === (value.token === undefined)) {
      context.addIssue({
        code: "custom",
        message: "lifecycle target requires exactly one of path or token",
      });
    }
  });

export const searchRequestSchema = z
  .object({
    repoPath: z.string().min(1).refine(isAbsolute, "repoPath must be absolute"),
    target: z.discriminatedUnion("kind", [textTargetSchema, lifecycleTargetSchema]),
    limit: z.number().int().min(1).max(50).default(10),
    byteCap: z.number().int().min(1_024).max(1_000_000).default(32_000),
    cursor: z.string().min(1).optional(),
  })
  .strict();

// Partiality is proof, never an error: a lazily built index reports exactly
// how much history is searchable right now and how much work remains.
export const searchIndexStateSchema = z
  .object({
    state: z.enum(["fresh", "partial"]),
    indexedCommits: z.number().int().nonnegative(),
    pendingCommits: z.number().int().nonnegative(),
    pendingDiffCommits: z.number().int().nonnegative(),
    skippedDiffCommits: z.number().int().nonnegative(),
  })
  .strict();

export const searchHitSchema = z
  .object({
    oid: searchHitOidSchema,
    at: z.string().min(1),
    subject: z.string(),
    author: z.string(),
    matchKind: z.enum(["message", "path", "diff-add", "diff-del"]),
    path: z.string().min(1).optional(),
    snippet: z.string().optional(),
    filesTouched: z.number().int().nonnegative(),
  })
  .strict();

const lifecycleCommitReferenceSchema = z
  .object({
    oid: searchHitOidSchema,
    at: z.string().min(1),
    subject: z.string(),
  })
  .strict();

export const searchLifecycleSchema = z
  .object({
    firstSeen: lifecycleCommitReferenceSchema.optional(),
    lastChanged: lifecycleCommitReferenceSchema.optional(),
    removedIn: lifecycleCommitReferenceSchema.optional(),
    presentAtHead: z.boolean(),
    totalTouches: z.number().int().nonnegative(),
  })
  .strict();

export const searchResultSchema = z
  .object({
    head: operationHeadSchema,
    index: searchIndexStateSchema,
    hits: z.array(searchHitSchema),
    lifecycle: searchLifecycleSchema.optional(),
    nextCursor: z.string().min(1).optional(),
  })
  .strict();

export type SearchRequest = z.input<typeof searchRequestSchema>;
export type SearchTarget = z.infer<typeof searchRequestSchema>["target"];
export type SearchHit = z.infer<typeof searchHitSchema>;
export type SearchLifecycle = z.infer<typeof searchLifecycleSchema>;
export type SearchIndexState = z.infer<typeof searchIndexStateSchema>;
export type SearchResult = z.infer<typeof searchResultSchema>;

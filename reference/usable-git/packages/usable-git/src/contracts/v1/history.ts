import { z } from "zod";
import { objectIdSchema, operationHeadSchema } from "./result-primitives.ts";

const identitySchema = z
  .object({
    name: z.string(),
    email: z.string(),
  })
  .strict();

// Default wire shape: short oid, subject line, author name, committer date.
// `merge: true` replaces the full parent oid list for merge commits.
export const compactHistoryCommitSchema = z
  .object({
    oid: z.string().regex(/^[a-f0-9]{12}$/),
    subject: z.string(),
    author: z.string(),
    at: z.string().min(1),
    merge: z.literal(true).optional(),
  })
  .strict();

// `detail: "full"` restores the forensic shape.
export const historyCommitSchema = z
  .object({
    oid: objectIdSchema,
    parents: z.array(objectIdSchema),
    author: identitySchema,
    committer: identitySchema,
    authoredAt: z.string().min(1),
    committedAt: z.string().min(1),
    signatureStatus: z.string().length(1),
    message: z.string(),
  })
  .strict();

export const historyResultSchema = z
  .object({
    head: operationHeadSchema,
    commits: z.array(z.union([compactHistoryCommitSchema, historyCommitSchema])),
    nextCursor: z.string().min(1).optional(),
  })
  .strict();

export type CompactHistoryCommit = z.infer<typeof compactHistoryCommitSchema>;
export type HistoryCommit = z.infer<typeof historyCommitSchema>;
export type HistoryResult = z.infer<typeof historyResultSchema>;

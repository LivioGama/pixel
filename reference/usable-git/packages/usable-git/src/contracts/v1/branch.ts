import { isAbsolute } from "node:path";
import { z } from "zod";
import { expectedHeadSchema } from "./publish.ts";
import { objectIdSchema } from "./result-primitives.ts";

const requestIdSchema = z.string().regex(/^[A-Za-z0-9._-]{1,128}$/);

// Short branch name (no refs/heads/ prefix), same component rules as push refs.
export const branchNameSchema = z
  .string()
  .min(1)
  .max(1_000)
  .superRefine((value, context) => {
    const components = value.split("/");
    const invalid =
      value === "HEAD" ||
      value.startsWith(".") ||
      value.startsWith("-") ||
      value.endsWith(".") ||
      value.endsWith("/") ||
      value.startsWith("/") ||
      value.includes("..") ||
      value.includes("@{") ||
      value.includes("//") ||
      /[\u0000-\u0020\u007f~^:?*[\\]/.test(value) ||
      components.some(
        (component) =>
          component.length === 0 ||
          component.startsWith(".") ||
          component.endsWith(".lock"),
      );
    if (invalid) {
      context.addIssue({
        code: "custom",
        message: "branch name must be a valid short branch name",
      });
    }
  });

export const branchRequestSchema = z
  .object({
    repoPath: z.string().min(1).refine(isAbsolute, "repoPath must be absolute"),
    requestId: requestIdSchema.optional(),
    expectedHead: expectedHeadSchema,
    mode: z.discriminatedUnion("kind", [
      z.object({ kind: z.literal("create"), name: branchNameSchema }).strict(),
      z.object({ kind: z.literal("switch"), name: branchNameSchema }).strict(),
    ]),
  })
  .strict();

export const branchResultSchema = z
  .object({
    name: z.string().min(1),
    oid: objectIdSchema.nullable(),
    previousBranch: z.string().min(1).nullable(),
    created: z.boolean(),
  })
  .strict();

export type BranchRequest = z.infer<typeof branchRequestSchema>;
export type BranchResult = z.infer<typeof branchResultSchema>;

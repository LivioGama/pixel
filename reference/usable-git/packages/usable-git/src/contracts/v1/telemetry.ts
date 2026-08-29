import { z } from "zod";

import { errorCodeSchema } from "../v1.ts";

const operationSchema = z.enum([
  "inspect",
  "review",
  "history",
  "diff",
  "publish",
  "push",
  "ship",
  "branch",
  "sync",
  "update",
  "search",
]);
export const clientSchema = z.enum(["codex", "claude-code", "cursor-agent", "devin-cli", "other"]);
export const transportSchema = z.enum(["mcp", "cli"]);
const resultCodeSchema = z.union([z.literal("success"), errorCodeSchema]);

const aggregateCountsSchema = z
  .object({
    selected: z.number().int().nonnegative(),
    staged: z.number().int().nonnegative(),
    unstaged: z.number().int().nonnegative(),
    untracked: z.number().int().nonnegative(),
    conflicted: z.number().int().nonnegative(),
    commits: z.number().int().nonnegative(),
    warnings: z.number().int().nonnegative(),
  })
  .strict();

const componentVersionsSchema = z
  .object({
    usableGit: z.string().min(1).max(128),
    bun: z.string().min(1).max(128),
    git: z.string().min(1).max(128),
    client: z.string().min(1).max(128),
  })
  .strict();

export const telemetryEventSchema = z
  .object({
    version: z.literal("v1"),
    operation: operationSchema,
    client: clientSchema,
    transport: transportSchema,
    backend: z.literal("git-cli"),
    durationMs: z.number().finite().nonnegative(),
    gitSubprocessCount: z.number().int().nonnegative(),
    resultCode: resultCodeSchema,
    counts: aggregateCountsSchema,
    components: componentVersionsSchema,
    repositoryHash: z.string().regex(/^[a-f0-9]{64}$/),
  })
  .strict();

export const telemetryEventInputSchema = telemetryEventSchema
  .omit({ version: true, backend: true, repositoryHash: true })
  .extend({ repositoryIdentity: z.string().min(1) })
  .strict();

export type TelemetryEvent = z.infer<typeof telemetryEventSchema>;
export type TelemetryEventInput = z.infer<typeof telemetryEventInputSchema>;

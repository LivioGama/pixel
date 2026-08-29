import { z } from "zod";

import { operationSchema, errorCodeSchema } from "../v1.ts";
import { clientSchema, transportSchema } from "./telemetry.ts";

// Token-cost constants. 4 bytes/token is the OpenAI/Anthropic average for
// mixed text/JSON. 80 tokens per avoided agent-facing operation covers the
// command echo + tool-result wrapper + one reasoning step. 40 tokens per
// avoided git subprocess covers spawn noise + exit/status lines.
export const BYTES_PER_TOKEN = 4;
export const TOKENS_PER_AVOIDED_OP = 80;
export const TOKENS_PER_AVOIDED_SUBPROCESS = 40;

export const gainEventSchema = z
  .object({
    version: z.literal("v1"),
    timestamp: z.string().min(1),
    operation: operationSchema,
    client: clientSchema,
    transport: transportSchema,
    resultCode: z.union([z.literal("success"), errorCodeSchema]),
    repositoryHash: z.string().regex(/^[a-f0-9]{64}$/),
    envelopeBytes: z.number().int().nonnegative(),
    rawEquivalentBytes: z.number().int().nonnegative(),
    agentOpsRaw: z.number().int().nonnegative(),
    agentOpsActual: z.number().int().positive(),
    gitSubprocessesRaw: z.number().int().nonnegative(),
    gitSubprocessesActual: z.number().int().nonnegative(),
    durationMs: z.number().finite().nonnegative(),
    tokensSaved: z.number().finite(),
  })
  .strict();

export const gainEventInputSchema = gainEventSchema
  .omit({ version: true, timestamp: true, repositoryHash: true })
  .extend({ repositoryIdentity: z.string().min(1) })
  .strict();

export type GainEvent = z.infer<typeof gainEventSchema>;
export type GainEventInput = z.infer<typeof gainEventInputSchema>;

export const gainSummarySchema = z.object({
  totalOperations: z.number().int().nonnegative(),
  totalEnvelopeBytes: z.number().int().nonnegative(),
  totalRawEquivalentBytes: z.number().int().nonnegative(),
  totalTokensSaved: z.number().finite(),
  totalAgentOpsSaved: z.number().int(),
  totalSubprocessesSaved: z.number().int(),
  avgSavingsPct: z.number().finite().min(0).max(100),
  byOperation: z.array(
    z.object({
      operation: operationSchema,
      count: z.number().int().nonnegative(),
      tokensSaved: z.number().finite(),
      avgPct: z.number().finite(),
      envelopeBytes: z.number().int().nonnegative(),
      rawBytes: z.number().int().nonnegative(),
    }),
  ),
});

export type GainSummary = z.infer<typeof gainSummarySchema>;

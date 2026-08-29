import type { Operation } from "../contracts/v1.ts";
import {
  BYTES_PER_TOKEN,
  TOKENS_PER_AVOIDED_OP,
  TOKENS_PER_AVOIDED_SUBPROCESS,
} from "../contracts/v1/gain.ts";
import { baselineFor } from "./baselines.ts";

export type GainEstimate = {
  envelopeBytes: number;
  rawEquivalentBytes: number;
  agentOpsRaw: number;
  agentOpsActual: number;
  gitSubprocessesRaw: number;
  gitSubprocessesActual: number;
  tokensSaved: number;
};

// Compute the token gain for one operation invocation. `envelopeBytes` is the
// serialized JSON envelope size. `gitSubprocessesActual` is the measured
// subprocess count from withGitMetrics. The semantic operation itself counts
// as 1 agent-facing op.
export const estimateGain = (
  operation: Operation,
  envelopeBytes: number,
  gitSubprocessesActual: number,
): GainEstimate => {
  const baseline = baselineFor(operation);
  const agentOpsActual = 1;
  const agentOpsSaved = baseline.agentOpsRaw - agentOpsActual;
  const subprocessesSaved = baseline.gitSubprocessesRaw - gitSubprocessesActual;
  const bytesSaved = baseline.rawEquivalentBytes - envelopeBytes;
  const tokensSaved =
    bytesSaved / BYTES_PER_TOKEN +
    agentOpsSaved * TOKENS_PER_AVOIDED_OP +
    subprocessesSaved * TOKENS_PER_AVOIDED_SUBPROCESS;
  return {
    envelopeBytes,
    rawEquivalentBytes: baseline.rawEquivalentBytes,
    agentOpsRaw: baseline.agentOpsRaw,
    agentOpsActual,
    gitSubprocessesRaw: baseline.gitSubprocessesRaw,
    gitSubprocessesActual,
    tokensSaved,
  };
};

# Deterministic prompt fact-pack experiment

## Question

Does a bounded deterministic repository-fact packet reduce total agent
trajectory cost while preserving verified correctness? This protocol evaluates
the harness integration, not an optimization policy inside Pixel.

## Arms

Run the same capable model and harness over paired task families in four arms:

| Arm | Retrieval setup |
| --- | --- |
| Unconstrained | No Pixel prompt or hook. |
| Current Pixel | Current installed Pixel guidance/hook behavior. |
| Fact packet, automatic | Every substantive prompt receives the at-most-1 KiB packet when fresh facts are available. |
| Fact packet, on demand | The harness requests the canonical full JSON fact result only when it chooses to. |

The automatic packet comes from `targets_facts`: a pure read of a compatible
warm daemon's already-published index and graph. An unavailable, stale,
malformed, or timed-out response contributes no packet. The model may explore
beyond cited candidates in every arm.

## Frozen inputs and outputs

Record normalized task text, repository commit plus dirty-content signature,
index base/delta/overlay/tombstone state, graph generation/signature, algorithm
version, limit, and the disabled activity-reranking and semantic-fallback
flags. These identify a fact result. Do not include elapsed time in the fact
bytes; record it separately as telemetry.

For every trajectory, record model and harness versions, arm, task-family and
pair identifiers, verified success/failure/timeout, elapsed time, API/token
usage, Pixel/tool calls, files and source regions inspected, test time, edits,
and rework (edits or tests after an initially failing verification). Keep raw
artifacts redaction-reviewed and publish their manifest and counting method.

## Decision rule

Pre-register verification for each task. The candidate automatic and on-demand
arms must each be non-inferior to both baselines on verified success: one-sided
95% confidence bound greater than -5 percentage points. Subject to that bound,
require at least a 10% paired task-family improvement in verified completion
time, with its interval reported. Also report failures, timeouts, total cost
per verified completion, injected-packet tokens, unavailable-rate, misleading
candidates, and each trajectory-cost component. Observed best performance is
an empirical reference, not proof of a globally shortest path.

# Observed planning requests: bounded availability audit

2026-09-22, source HEAD `5c894b1b3a5db713615d906dc8604805c73813ae`.
Do not change or fit any of the previously frozen routing candidates.

## Population and retrieval

Find actual recorded Pixel planning calls, not public issue text or newly authored
examples. Use Pixel recall, never grep transcript stores. Start with this repo,
last 30 days, and an upper bound `2026-09-22T00:00:00Z` before the prior model
experiments. The initial tool-role exact-token search primarily found quoted
source/docs, so inspect assistant tool-call turns instead. One final widening
drops the repository filter; no more search reformulations. The final exact
`pixel plan ` assistant search returned 39 hits, uncapped. This is completeness
within that indexed query only: aliases, intervening global flags, direct daemon
calls, unindexed agents and older sessions can be missed.

The repo action log is separate corroboration: its 1,436 available events include
20 plan calls, most on/after the excluded experiment day. It rotates at 5 MiB to
5,000 lines, and args are truncated at 4,000 characters and flatten argv quoting.
It cannot independently recover all original command boundaries or historical
source HEADs. Never relabel it as an exhaustive usage history.

## Admission and privacy

Read each matching assistant turn through recall and parse only actual Bash tool
call JSON. Retain literal direct `pixel plan` argv, not mentions inside docs,
commit messages, code generation, heredocs, help, timing probes or substitutions.
Preserve explicit --query as a separate lane: it bypasses automatic classification,
and must not be paraphrased into a supposedly observed natural-language request.
Look for adjacent actual plan output or matching pre-cutoff action events. Record
uncertain/missing completion instead of inventing execution. Reject capped input
turns. Preserve source agent/session/turn references and tool-output hashes.
Group repeated/root-child tasks before interpreting sample size; no row split.

This is private local history: no transcript or historical request text will be
added to tracked fixtures or published. Store only the narrow derived requests,
references and admission receipts under the ignored run directory. Do not resolve
secrets or record unrelated shell arguments/output. Mark historical repository
HEAD unknown when the retained record does not prove it. Current-source replay
identity would be separate from the original invocation's repository state.

## Stop rule and decision

An independent read-only reviewer checks provenance/contamination, labels implicit
requests from operational semantics and groups task families. Explicit requests
are counted by their actual flag, not treated as implicit classifier positives.
If there are fewer than five independently supported implicit examples for each
specialized operation or fewer than five implicit mixed-intent families, stop
before another model replay. Repeating an all-concept diagnostic would not resolve
the evidence gap. Do not fabricate or keyword-enrich positives to pass the gate.
No accuracy/latency claims will be produced for an unevaluated corpus.

If the retained history cannot supply that evidence, finish with an executed
opt-in local capture utility that records future genuine plan requests with
source-binary identity, repository HEAD before/after, argv and completion hashes.
It must preserve the native command's stdout/stderr/exit status, mark explicit
queries and fixture-purpose runs, refuse overwrite, and never install hooks,
change global configuration or run automatically. Mechanism-test records are
excluded from observational data. A capture receipt is execution evidence, not
relevance gold or a license to skip later review.

Read existing project rules; bench/docs-only gates apply. Preserve all existing
work and source defaults. No neural weights/training, push, PR or publication.
New private evidence root: `.pixel/experiments/plan-routing-observed-v1/`.

# Phase 1 exploratory record — indexer, shards, query path

> Historical development observations only. Trial counts, raw samples,
> confidence intervals, commit SHAs, subprocess counts, agent-facing
> operations, and token measurements were not recorded. These numbers must
> not be used as a current performance claim or as evidence that gitpixel is
> faster than ripgrep or GitNexus.

Machine: Apple Silicon macOS (Darwin 27.0.0), rustc 1.96.0, release build.
Corpora: real repos on this machine.
- `omni-pr351-rebase`: 1,915 indexed files, 24.4 MB text
- `omni-dev-624`: 3,319 indexed files, 40.9 MB text (594 MB tree incl. ignored)

## Gate: sparse n-grams vs trigrams

Index size and build time (omni-pr351-rebase, 24.4 MB text; trigram baseline
9.0 MB / 255 ms):

| Extractor | Shard size | vs corpus | vs trigram | Build |
|---|---|---|---|---|
| trigram | 9.0 MB | 0.37× | 1.0× | 255 ms |
| sparse max=6 | 27.5 MB | 1.13× | 3.1× | 2.6 s |
| sparse max=8 | 37.1 MB | 1.52× | 4.1× | 1.4 s |
| sparse max=12 | 51.4 MB | 2.11× | 5.7× | 1.5 s |
| sparse max=16 | 61.1 MB | 2.50× | 6.8× | 2.7 s |
| sparse max=100 (default) | 92.9 MB | 3.81× | 10.3× | 3.2 s |

Confirmed at scale (omni-dev-624, 40.9 MB): trigram 12.8 MB / 299 ms; sparse
max=8 54.4 MB / 958 ms (4.2×).

Candidate sets (files to verify), omni-pr351-rebase:

| Query | trigram | sparse (any max ≥ 8) |
|---|---|---|
| `PocketBase` (46 matches) | 10 | 10 |
| `useEffect` (104) | 36 | 36 |
| `const` (9,420, pathological) | 1,289 | 974 |
| `export const [A-Z][a-zA-Z]+` (121) | 423 | 64 |

## Decision: TRIGRAM is the default; sparse stays behind `--extractor sparse`

Per the pre-registered rule (adopt sparse iff size ≤ ~1.5× trigram OR
materially better identifier latency): sparse fails both. The distinct-key
space of sparse grams makes the 20-byte-per-key lookup table dominate the
shard — 3–10× trigram size at every max-length setting — while candidate
sets for identifier queries (the agent workload) are identical, because
verification cost is bounded by matching files either way. Sparse's real win
(fewer, rarer lookups: 64 vs 423 candidates on the regex query) matters when
lookups are expensive (network, cold disk); with a local mmapped lookup
table it does not move end-to-end latency. This mirrors the research
caveat: nobody had published index-size numbers for sparse grams — now we
have ours.

Sparse remains fully supported behind the flag (`gitpixel index --extractor
sparse --max-gram N`) and shares the shard format; revisiting costs nothing
if a positional or network-attached design changes the trade-off later.

## Correctness (exit criterion)

`gitpixel search` vs `rg --line-number --no-heading` on omni-dev-624,
line-level diff after sort: **zero missed, zero extra** on all 4 queries
(46 / 104 / 10,855 / 122 matching lines).

## End-to-end latency (hyperfine, cold CLI process each run, warmup 2-3)

omni-dev-624 (594 MB tree), trigram index:

The original run observed lower wall-clock times for gitpixel on four local
queries, but the retained record is insufficient to reproduce or validate
that comparison. The numerical table is intentionally removed until a pinned
paired harness emits the required raw artifacts.

gitpixel cold includes process spawn + shard mmap + plan + posting resolve +
regex verification of candidates. rg must walk the tree every time; the
index amortizes that. The daemon (Phase 2) removes the remaining process
spawn + mmap cost from the warm path.

High variance was observed from page-cache sensitivity. No rerun command or
raw trial artifact was retained, so this section records design history only.

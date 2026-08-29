# Phase 0 exit record — gram core

Machine: Apple Silicon macOS (Darwin 27.0.0), rustc 1.96.0, single thread.
Corpus: 2 MiB of real Rust source (workspace files, repeated).
Command: `cargo bench -p gitpixel-bench` (criterion).

| Extractor | Throughput | Emission rate |
|---|---|---|
| sparse (crc32 pair-table weights) | ~100 MiB/s | 1.82 grams/byte (bound: 2N) |
| trigram | ~1.18 GiB/s | 1.0 grams/byte |

Tests: `cargo test -p gitpixel-core` — 14 passed, including property tests
against an O(n³) brute-force oracle of the selection predicate (uniform +
small-alphabet tie-heavy inputs, varied max_len for hull front-eviction),
the no-false-negatives covering property, and the ≤2N emission bound.

Notes:
- `crc32fast::hash` per 2-byte window was the initial bottleneck (66 MiB/s);
  a precomputed 65536-entry pair table (byte-identical values) recovered +50%.
- Remaining sparse cost is xxh3 per emission (~1.8/byte); candidate for
  Phase 1 profiling on real repos. Extraction parallelizes per-file.

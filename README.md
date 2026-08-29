# 📋 gitpixel

> **Fast, always-fresh code retrieval for LLM agents.**

A Rust sidecar that replaces grep-style scanning and stale code-graph tools with an indexed engine that stays correct mid-session. Built so an agent can search, trace, and reason about a codebase without re-reading it every turn.

![Status](https://img.shields.io/badge/status-active-success)
![Type](https://img.shields.io/badge/type-tool-blue)
![Language](https://img.shields.io/badge/Rust-2024%20edition-orange)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)

## ✨ Features

🔍 **Indexed regex search** — Trigram shard (mmapped, delta-varint postings), regex→boolean query planning (Cox algebra over `regex-syntax` HIR), candidate verification with ripgrep's matcher crates. Sound by construction: the index can only over-approximate; verification is authoritative.

⏱️ **Git-anchored freshness** — Base shard pinned to a commit OID, a committed-delta layer on HEAD moves (`git diff --name-status`), and an in-memory dirty overlay for uncommitted/agent edits fed by an fs watcher. Query merge subtracts tombstones before unioning the overlay; content-hash tests cover equal-size edits with restored mtimes.

🕸️ **Code graph** — Tree-sitter extraction (TS/TSX/JS, Rust, Go, Java, Python) into SQLite; tiered call resolution (exact same-file → import-resolved → unique-name probable) that **never fans out** ambiguous names into edges. Unresolved calls surface through an epistemic envelope (`lower_bound: true` + count) so an agent can tell "0 callers" from "resolver gave up".

📊 **Analyses** — Blast-radius `impact` (d1 WILL BREAK / d2 LIKELY / d3 MAY NEED TESTING, LOW..CRITICAL risk), `uses` (callers/callees with confidence tiers), `trace` A→B, `processes` (entry-point BFS execution flows), `clusters` (label-propagation functional areas), `changes` (git diff hunks → symbols → affected flows → risk), token-budgeted `context`.

⚡ **Serving** — One core `Service`; CLI one-shot commands that transparently use a warm Unix-socket daemon (NDJSON protocol, fs watcher, idle timeout) when available. MCP is a planned thin adapter over the same `Service`.

🎯 **Error sniper** — One-look error capture: a per-repository SQLite sink (WAL, dedup with repeat counters, retention) that collects runtime, HTTP-5xx, build, and test failures at throw-time with source-mapped frames, package provenance (duplicate-copy detection), run fingerprints, and HMR/lifecycle events. Queried in one call via `gitpixel sniper` or its stdio MCP server (`rmcp`); fed by `gitpixel sniper run -- <cmd>` (tsc-aware) and the `@gitpixel/sniper` JS companion in `js/sniper` (Vite dev plugin, browser client, vitest reporter).

🧠 **Transcript recall** — Machine-wide retrieval over every LLM CLI's transcripts (Claude Code incl. subagents, Codex, opencode, Cursor CLI, Devin, zcode, Gemini history): streaming/cursor-based incremental ingest into one SQLite corpus of turn-granular text, trigram segments reusing the shard engine (turn-rowid doc trick), and a semantic channel (multilingual-e5-small int8 ONNX via fastembed, i8-quantized brute-force mmap vector segments with exact metadata pre-filtering) fused by RRF. `gitpixel recall ask 'where did we discuss dropping svelte'` goes straight to the session; `maxtest` ranks remembered keywords by rarity to pin a session; a recall daemon watches the source stores, keeps the corpus fresh, and holds the model warm.

## 🔧 Installation

### From source (release build)

```bash
git clone https://github.com/LivioGama/gitpixel.git
cd gitpixel
cargo build --release
# → target/release/gitpixel
```

### Workspace layout

| Crate | Role |
|-------|------|
| `gitpixel-core` | Trigram/sparse index, shard, plan, verify, freshness overlay |
| `gitpixel-graph` | Tree-sitter extraction, call resolution, analyses |
| `gitpixel-context` | Token-budgeted context assembly |
| `gitpixel-serve` | `Service`, daemon, NDJSON API |
| `gitpixel-cli` | `gitpixel` binary — every command surface |
| `gitpixel-sniper` | Error sink: store, dedup, query layer, run wrapper, MCP server |
| `gitpixel-bench` | Criterion benchmarks (see `docs/bench/`) |

The `js/sniper` directory holds `@gitpixel/sniper`, the JS companion feeding the sniper sink from Vite dev servers, browsers, and vitest.

## 🚀 Quick Start

```bash
# Build the text index for a repo
target/release/gitpixel index /path/to/repo

# Regex search with candidate/timing stats
target/release/gitpixel search 'handleClick' /path/to/repo --stats

# Build the code graph (SQLite, tree-sitter)
target/release/gitpixel graph /path/to/repo

# Start a warm daemon + fs watcher for the repo
target/release/gitpixel daemon start /path/to/repo

# Agent bootstrap in one command: text index + graph + warm daemon
target/release/gitpixel ready /path/to/repo

# Capture every error from a command into the queryable sink
target/release/gitpixel sniper run -- bunx tsc --noEmit

# Search every LLM CLI transcript on the machine ("where did we discuss X?")
target/release/gitpixel recall index && target/release/gitpixel recall ask 'where did we discuss dropping svelte'
```

## 📖 Usage Examples

### Search

```bash
# Plain text matches (path:line:text)
target/release/gitpixel search 'fn\s+handle' /path/to/repo
# → src/main.rs:42:fn handle_request(req: Request) -> Response

# NDJSON matches for tooling
target/release/gitpixel search 'TODO' /path/to/repo --json
# → {"path":"src/api.rs","line":108,"text":"// TODO: rate limit"}

# Candidate/timing stats to stderr
target/release/gitpixel search 'handleClick' /path/to/repo --stats
# → candidates=12 matches=3 elapsed_us=14210

# Responses default to 100 matches; retrieve the next page explicitly
target/release/gitpixel search 'TODO' /path/to/repo --limit 100 --offset 100
```

### Code graph

```bash
# Look up symbols by name
target/release/gitpixel symbol handleClick /path/to/repo
# → function  handleClick  src/ui.rs:14-38  src/ui.rs#handleClick#function

# Token-budgeted context for a symbol uid
target/release/gitpixel context 'src/ui.rs#handleClick#function' /path/to/repo --budget 4000

# Blast radius — what breaks if I change this symbol?
target/release/gitpixel impact someFunction /path/to/repo --direction upstream
# → d1 WILL BREAK: 2   d2 LIKELY: 5   d3 MAY NEED TESTING: 11   risk: HIGH

# Direct callers / callees with confidence tiers
target/release/gitpixel uses someFunction /path/to/repo --role callers
# → callers: 4
#   [exact]   line 22  function  callerA  src/mod.rs:22:callerA
#   [import]  line 88  function  callerB  src/api.rs:88:callerB

# Call path between two symbols
target/release/gitpixel trace handlerA handlerB /path/to/repo

# Discovered execution flows (entry-point BFS)
target/release/gitpixel processes /path/to/repo

# Functional-area clusters (label propagation)
target/release/gitpixel clusters /path/to/repo

# What symbols/flows are affected by working-tree changes
target/release/gitpixel changes /path/to/repo

# Every capped list command supports continuation
target/release/gitpixel uses someFunction /path/to/repo --role callers --offset 20
target/release/gitpixel processes /path/to/repo --offset 5
target/release/gitpixel clusters /path/to/repo --offset 50
target/release/gitpixel changes /path/to/repo --offset 20
```

### Daemon

```bash
# Start (background, fs watcher, idle timeout)
target/release/gitpixel daemon start /path/to/repo
# → daemon started ($TMPDIR/gitpixel-<root-hash>.sock)

# Status
target/release/gitpixel daemon status /path/to/repo
# → daemon running (...)

# Stop
target/release/gitpixel daemon stop /path/to/repo
# → daemon stopped
```

Search and graph-analysis commands transparently use the warm daemon when one is up (NDJSON over a Unix socket, ~100ms ping gate) and fall back to an in-process `Service` otherwise. Pass `--no-daemon` on search to force in-process.

### Agent bootstrap

Use `ready` as the first GitPixel command for a repository. It discovers the
repository root, ensures the text index and code graph are usable, then starts
the warm daemon. Pass `--no-daemon` when only preparing index artifacts.

```bash
target/release/gitpixel ready /path/to/repo
target/release/gitpixel ready /path/to/repo --no-daemon --json
```

### Sniper targets — task scoping

Task description in, closed prioritized file list out. The list is the
agent's whole world for that task: P0 = start here (guaranteed relevant),
P1 = likely needed, P2 = peripheral and droppable. Lexical (filename,
symbol-name, content) and graph (callers/callees, imports, clusters)
signals fused with reciprocal-rank fusion; deterministic; ~20 ms against a
warm daemon.

```bash
# Scope a task: emits the tiered list AND activates .gitpixel/targets.json
gitpixel targets "fix rate limiting on the upload endpoint" /path/to/repo
# → P0 — primary (start here)
#     src/api/upload.ts        0.0921   filename match: upload · defines symbol `uploadHandler`
#   P1 — likely needed …  P2 — peripheral (droppable) …
#   closed list: 12 files (limit 20)

gitpixel targets "…" . --json --limit 30     # machine output, wider list
gitpixel targets "…" . --no-manifest         # dry run, no enforcement manifest
gitpixel targets --clear .                   # end scoping (delete the manifest)
```

The manifest (`{task, created_unix, head_oid, files:[{path,tier}]}`) is what
harness hooks enforce against; it goes stale automatically after 24 h.
Every report carries the epistemic envelope: `lower_bound: true` means the
graph could not close the world (unresolved same-name call sites, or
lexical-only mode) — unlisted files may then be involved.

### Rescue — surgical revert planner

"It was working before" is a retrieval problem, not a coding problem.
`rescue` locates the files a problem points at (same target engine), lists
each file's recent versions with the likely-breaking commit flagged, and
recommends a last-known-good candidate. **Plan only by default** — nothing
is written without `--apply`, and apply never touches the index or HEAD,
never `reset --hard`s, and never overwrites uncommitted work without an
explicit strategy.

```bash
# Plan: versions per target file + recommended last-good + decision block
gitpixel rescue "upload progress bar was working before" . [--json]
# → src/upload/progress.ts
#     a1b2c3d  rework upload pipeline   [SUSPECT]
#     e4f5a6b  add progress bar
#     → recommended: e4f5a6b (last version before suspect commit a1b2c3d)
#   revert: gitpixel rescue --apply e4f5a6b --file src/upload/progress.ts .
#   fix forward: keep current code and fix the bug in place

# Gated apply — working tree only, ordinary undoable diff
gitpixel rescue --apply e4f5a6b --file src/upload/progress.ts .
# Dirty file strategies (refused by default):
#   --merge       deterministic 3-way merge; keeps in-progress edits (may leave markers)
#   --stash-first `git stash push` the planned files before writing
#   --allow-dirty overwrite (loses in-progress work — explicit opt-in)
```

### Agent workflow contract

Paste into the adopting repo's `CLAUDE.md` / `AGENTS.md`:

> Before the first file read of any feature/bug task, run
> `gitpixel targets "<task>"`. It returns a closed prioritized file list
> (P0/P1/P2) and activates `.gitpixel/targets.json`. Work P0 first; P2 is
> droppable. While the manifest is active, never read, grep, or edit repo
> files outside the list — if a file seems missing, the task description was
> wrong: re-run `gitpixel targets` with a refined task. Run
> `gitpixel targets --clear` when the task ends.
> When the user says something **was working before** (or the fix is in git
> history), run `gitpixel rescue "<problem>"` — never `git reset --hard`,
> never raw historical checkouts over in-progress work.

Claude Code additionally enforces all of this mechanically via the
`gitpixel-targets-guard` PreToolUse hook (off-list reads/edits blocked,
edits without an active manifest blocked, `git reset --hard` blocked).
Kill switch for debugging the guard: `GITPIXEL_TARGETS_GUARD=0`.

### Error sniper

```bash
# Wrap any command; failures land in the sink as structured records
target/release/gitpixel sniper run --label typecheck -- bunx tsc --noEmit --pretty false
# → #1 [tsc] TS2322: Type 'string' is not assignable…  #3 [tsc] summary: 3 errors in 1 file

# Newest errors, one line each, with a cursor footer
target/release/gitpixel sniper last
# → #412  2s ago  ×3  [browser-rejection] TypeError: undefined is not an object (evaluating 'api.sessions.x')
#         @ src/routes/chat.tsx:88:14  ← via @tanstack/react-router@1.130.2  [!] 2 physical copies
#   cursor: 412

# The agent loop: anything new since my last check?
target/release/gitpixel sniper since 412

# Full detail: mapped frames, provenance, values, run fingerprint diff, ±30s events
target/release/gitpixel sniper show 412

# "Was my edit applied?" — HMR/reload/dep-optimization events
target/release/gitpixel sniper hmr --file src/routes/chat.tsx

# Stdio MCP server (tools: errors_since, error_show, errors_query, hmr_status, env_fingerprint)
target/release/gitpixel sniper mcp
```

Vite apps adopt capture in two lines with the JS companion — see [js/sniper/README.md](js/sniper/README.md).

### Transcript recall

```bash
# One-time bootstrap
gitpixel recall index                 # ingest all CLI transcript stores
gitpixel recall setup                 # download the embedding model (~110 MB)
gitpixel recall embed                 # semantic backfill (resumable)
gitpixel recall daemon start          # fresh corpus + warm model

# Straight-to-target queries
gitpixel recall ask 'where did we discuss dropping svelte'
gitpixel recall search 'dokploy-network' --repo ~/Documents/foo --since 3w
gitpixel recall maxtest 'svelte,trigram,dokploy'   # rarest keyword pins the session
gitpixel recall sessions --agent codex --since 7d
gitpixel recall show claude:75deefa9 --turn 0..5
gitpixel recall context 'the unix socket permissions bug' --budget 4000
gitpixel recall status
```

The corpus lives at `~/.local/share/gitpixel/recall/` (mode 0700 — it concentrates every
transcript on the machine). Text is stored denormalized so the corpus outlives source
rotation; tool outputs are capped at 4 KB with a `truncated` flag; harness-injected
"user" text is classified `orchestrator` and excluded from the semantic index; Cursor
timestamps are file mtimes and every output carries its `ts_source`.

## 🎨 Design Notes

- **Trigram is the default extractor.** Cursor-style sparse n-grams remain available through `--extractor sparse`; the historical exploratory measurements that informed the default are archived in [docs/bench/phase1.md](docs/bench/phase1.md), but are not a current reproducible performance claim.
- **Performance versus ripgrep or GitNexus is not yet claimed.** A publishable comparison still needs a pinned paired harness with raw trial artifacts, environment and component versions, commit SHAs, median/p95/confidence intervals, correctness oracles, subprocess counts, agent-facing operation counts, and measured token data.
- **Freshness has explicit regression coverage.** Tests exercise commit anchoring, dirty overlays, symlink rejection, equal-size edits with restored mtimes, deletions, and oversized-file bounds.
- **The graph never lies about ambiguity.** Ambiguous call names are not fanned out into edges; the resolver surfaces an epistemic envelope instead, so "0 callers" and "resolver gave up" are distinguishable.
- Derived-code attribution in [NOTICE](NOTICE) (hypergrep, MIT).

## 🌍 Platform Support

| Platform | Status | Notes |
|----------|--------|-------|
| macOS | ✅ | Primary dev target (Apple Silicon benchmarks) |
| Linux | ✅ | Unix-socket daemon, mmap shard |
| Windows | ⚠️ | Not yet tested — Unix-socket daemon path is Unix-only |

## ⚠️ Caveats — What's Verified vs What's Missing

### ✅ Verified (this build)

| Layer | Evidence |
|-------|----------|
| Text index + regex search | Workspace tests cover shard round-trips, regex planning, result limiting, malformed shards, symlink rejection, and oversized-file rejection |
| Freshness engine | Tests cover commit anchoring, dirty overlays, deletion, equal-size content changes with restored mtimes, and non-Git directory reopening |
| Code graph extraction | Tests cover extraction, receiver preservation, named-import specificity, wildcard-import ambiguity, incremental definition ambiguity, and test-container exclusion |
| Daemon | Tests cover Unicode framing, oversized-frame rejection, and absolute read deadlines; public start/status/search/stop and stale-protocol fallback paths are exercised before release |
| Token-budgeted context | Tests require the complete serialized response to remain inside 50- and 500-token budgets |
| Error sniper | 100+ workspace tests over store/dedup/query/format/MCP; CLI + stdio MCP smoke-tested live against real tsc/vite failures |
| Transcript recall (lexical) | Verified against the full real corpus on this machine: 1,016,849 turns / 7 CLIs ingested with 0 parse errors; session-set parity with `grep -r` ground truth on multiple terms; incremental re-index is a sub-second no-op; maxtest pins known sessions |
| Transcript recall (semantic) | Full-corpus backfill measured: 2m32s / 267 MB of i8 vectors (potion-multilingual, 256d) for ~700k eligible turns; hybrid `ask` hit@5 = 7/10 on a 10-query eval of real remembered sessions (lexical-only baseline 5/10); daemon watcher auto-ingests a live session within seconds (verified end-to-end) |

### ❌ Missing / Deferred (treat outputs as best-effort)

| Gap | Impact | Status |
|-----|--------|--------|
| **Graph correctness harnesses** | Call resolution tiers, epistemic envelopes not property-tested | Next milestone — do not trust graph outputs at scale until landed |
| **Daemon long-run stability** | No multi-hour soak test; idle timeout + watcher untested under load | Next milestone |
| **`processes` / `clusters` output quality** | Ran but not quality-checked on large repos; cluster boundaries may be coarse | Needs review on real monorepos |
| **`changes` symbol overlap** | Found dirty files but returned no symbol overlaps on a diff where hunks were outside indexed symbols | Worth a look when hardening — may miss hunks in non-indexed file types |
| **MCP adapter** | Not yet wired; planned as thin layer over `Service` | Planned |
| **Windows** | Unix-socket daemon path is Unix-only | Not supported |
| **Scale benchmarks** | Freshness daemon unbenchmarked on large monorepos | Next milestone |
| **GitNexus comparison** | No paired retrieval-quality, latency, subprocess, or token artifact exists yet | Required before claiming replacement performance |
| **Recall semantic quality** | hit@5 measured on a 10-query eval, but no large labeled benchmark; embedding-exclusion policy (orchestrator/tool noise) validated by inspection only | Expand the eval set |
| **Recall source coverage** | Cursor GUI chat bodies and Gemini conversation payloads are opaque binary blobs — not ingested; Codex encrypted `reasoning` records excluded by design | Blocked on reverse engineering |
| **Cursor timestamps** | Cursor CLI records carry no timestamps; turn times are file mtimes (`ts_source: "mtime"` disclosed in every output) | Inherent to the source |

### How to read graph outputs until harnesses land

- **`lower_bound: true`** in a response envelope = resolver gave up on N same-name call sites; the returned edges are a **lower bound**, not the full set. Treat "0 callers" + `lower_bound: true` as "unknown", not "unused".
- **`impact` risk tiers** (d1 WILL BREAK / d2 LIKELY / d3 MAY NEED TESTING) are structurally sound but depend on edge completeness — if the resolver dropped ambiguous edges, d2/d3 may under-count.
- **`changes`** only maps hunks that land inside indexed symbols (TS/TSX/JS/Rust/Go/Java/Python). Hunks in other file types or outside symbol ranges are reported as dirty files but produce no symbol overlaps.

## 📝 Status

Working v1 with bounded search/context responses, freshness regression coverage, tiered graph resolution, a persistent local daemon, the sniper error sink (CLI + stdio MCP), and machine-wide transcript recall (hybrid trigram + local-embedding retrieval over every LLM CLI's sessions, with its own watcher daemon). Graph property harnesses, long-run daemon testing, the main MCP transport, and reproducible comparisons remain open. See **Caveats** before relying on broad graph conclusions or performance claims.

## 🤝 Contributing

Contributions welcome. Especially useful right now — these close the gaps in **Caveats**:

1. **Correctness harnesses** for the graph layer (call resolution tiers, epistemic envelopes, property tests against a brute-force oracle).
2. **Scale benchmarks** for the freshness daemon on large monorepos (multi-hour soak, watcher under load).
3. **`changes` hunk coverage** — map hunks outside indexed symbols, handle non-indexed file types.
4. **MCP adapter** — thin layer over the existing `Service`.
5. **Language extractors** — additional tree-sitter grammars beyond the current set.

## 📝 License

MIT License — see [NOTICE](NOTICE) for derived-code attribution (hypergrep, MIT; ClickHouse sparse-grams algorithm, Apache-2.0).

## 🙋 Support

- **Issues**: Report bugs via [GitHub Issues](https://github.com/LivioGama/gitpixel/issues)
- **Discussions**: Ask questions in [GitHub Discussions](https://github.com/LivioGama/gitpixel/discussions)

---

<div align="center">

**Made with ❤️ for agents that need to read code fast**

[⭐ Star this repo](https://github.com/LivioGama/gitpixel) if it helps you!

</div>

# 🧬 usable-git

> **Semantic Git operations for coding agents — narrow on purpose, and measured rather than asserted.**

Coding agents drive Git through a shell, where one careless `git add -A` or `git checkout` quietly destroys work nobody asked them to touch. `usable-git` replaces that surface with ten structured operations that take explicit files, verify the repository still looks the way the agent thinks it does, and refuse rather than guess.

---

## ✨ Features

🎯 **Explicit by construction** — Literal file paths only. No globs, no directory expansion, no implicit upstreams, no multi-ref writes.

🔒 **Optimistic concurrency built in** — `inspect` returns a 12-hex snapshot token; mutations present it and fail cleanly if the repository moved underneath them.

🛡️ **Unrelated work is sacred** — Staged, unstaged, and untracked changes the agent did not name must survive every operation unchanged.

♻️ **Crash-recoverable mutations** — One lock and journal per Git common directory covers `publish`, `push`, `ship`, `branch`, and `update`.

🔁 **Two-call commit flow** — `inspect` → `ship` commits and pushes in one operation, with no fingerprint transcription in between.

🔌 **MCP first, CLI parity** — A local stdio MCP server is the primary transport; an identical JSON CLI covers every environment where MCP is unavailable.

📐 **Refusals are terminal** — A rejected mutation never silently falls back to a broader raw-Git command that bypasses the safety decision it just made.

---

## 📦 The eleven operations

| Operation | Purpose |
|---|---|
| `inspect` | Read repository, branch, and change state in one compact snapshot, returning a snapshot token for later mutations. |
| `review` | Return staged and unstaged evidence without mixing the two. |
| `history` | Read deterministic, paginated local commit history — compact by default, full on request. |
| `diff` | Return the patch between two exact commit OIDs, or one commit against its first parent. |
| `search` | Query the entire local history — messages, paths, diff text — in one ranked call, or ask a lifecycle question (`firstSeen` / `removedIn` / `presentAtHead`) about a path or token. |
| `publish` | Commit exact files (append or amend) while preserving every unrelated change. |
| `push` | Update exactly one explicit remote branch with fast-forward or exact-lease safety. |
| `ship` | Commit and push in one call, deriving push expectations from the fresh commit. |
| `branch` | Create a branch at the current HEAD, or switch when no uncommitted tracked work would be carried. |
| `sync` | Fetch exactly the named branches into remote-tracking refs; never touches local state. |
| `update` | Fast-forward the current branch to an exact target OID observed via `sync`. |

Responses use a minimal envelope — `ok`, optional `requestId`, optional `warnings`, and exactly one of `result` or `error`. Measured locally on small fixtures, the `inspect` envelope shrank from 1,841 to 290 bytes on a dirty repository, and `history --limit 20` on this repository from 9,118 to 2,881 bytes.

---

## 🔧 Installation

The public Homebrew tap is **not yet published** — see [Status](#-status). Work from a source checkout:

```bash
git clone https://github.com/LivioGama/usable-git.git
cd usable-git
bun install --frozen-lockfile
```

---

## 🚀 Quick Start

Read repository state, then commit and push the result — the whole flow is two calls:

```bash
# 1. Inspect returns a 12-hex snapshot token
bun packages/usable-git/src/cli.ts inspect --json --repo-path "$PWD"

# 2. Ship commits the named files and pushes the fresh commit
printf '%s\n' "{\"repoPath\":\"$PWD\",\"files\":[\"README.md\"],\"message\":\"docs: update\",\"snapshot\":\"<token-from-inspect>\",\"remote\":\"origin\"}" |
  bun packages/usable-git/src/cli.ts ship --input -
```

Start the MCP server for agent clients:

```bash
bun packages/usable-git/src/cli.ts mcp
```

---

## 📖 Usage Examples

### Reading state

```bash
# One compact snapshot of branch, changes, and a token for later mutations
bun packages/usable-git/src/cli.ts inspect --json --repo-path "$PWD"

# Staged and unstaged evidence, kept separate
bun packages/usable-git/src/cli.ts review --json --repo-path "$PWD"

# Bounded local history without touching the network
bun packages/usable-git/src/cli.ts history --json --repo-path "$PWD" --limit 20
```

### Same request as one JSON object on stdin

```bash
printf '%s\n' "{\"repoPath\":\"$PWD\"}" |
  bun packages/usable-git/src/cli.ts inspect --input -
```

### Recovering from a rejected push

```bash
# push returned NON_FAST_FORWARD — fetch exactly the branch you care about
printf '%s\n' "{\"repoPath\":\"$PWD\",\"remote\":\"origin\",\"branches\":[\"main\"]}" |
  bun packages/usable-git/src/cli.ts sync --input -

# then fast-forward to the exact OID sync reported, and retry the push
printf '%s\n' "{\"repoPath\":\"$PWD\",\"targetOid\":\"<oid-from-sync>\"}" |
  bun packages/usable-git/src/cli.ts update --input -
```

If the push leg of `ship` fails, the commit still stands: it returns `ok: true` with `result.push.ok: false` and retry guidance, so no work is lost and no state is ambiguous.

---

## 🛡️ Safety model

V1 is deliberately narrow:

- **Explicit literal files only** — no directories, globs, or implicit expansion.
- **Optimistic HEAD and change fingerprints** for mutations, presented as one snapshot token or an explicit expected block.
- **One lock and crash-recovery journal** per Git common directory for `publish`, `push`, `ship`, `branch`, and `update`. `sync` is deliberately lock-free: fetching is idempotent, and holding a lock through a slow fetch would block `publish` for zero protection.
- **Exact OIDs** for `diff` and `update` targets — never ref names or revision expressions.
- **Unrelated staged, unstaged, and untracked work must survive unchanged.**
- **Ambiguous remote outcomes are reported, never blindly retried.**
- Direct Git object writes are excluded from v1.

Unsupported operations may fall back to scoped raw Git. A *rejected* semantic mutation may not.

---

## 📊 Measured evidence

Two independent 40-trial matrices — **960 paired trials**, three real agent clients, two scenarios, measured on commit `237cd94`. Artifacts with per-trial results, environment, versions, medians, and p95 live in [`benchmarks/results/`](benchmarks/results/).

**What held, in both runs:**

| Guarantee | Result |
|---|---|
| Repository corruption (`git fsck --strict`) | **0 failures / 960 trials** |
| Unrelated work destroyed | **0 failures / 960 trials** |
| Correctness vs. raw Git | **Never worse, in all 12 cells** |

**What the numbers actually say, per client:**

| Client | Scenario | Correctness (semantic vs raw) | Adoption | Agent-facing ops | Git-related tokens |
|---|---|---|---|---|---|
| Codex 0.146.1 | inspect | 100% vs 100% | 100% | **−50%** | +5% worse |
| Codex 0.146.1 | publish | 100% vs 100% | 95% | **−60%** | +11% worse |
| Claude Code 2.1.226 | inspect | 100% vs 100% | 100% | 0% | **−22% better** |
| Claude Code 2.1.226 | publish | 100% vs 100% | 88% | −40% | +115% worse |
| Devin 3000.3.27 | inspect | 100% vs 100% | 100% | 0% | +45% worse |
| Devin 3000.3.27 | publish | 65% vs **65%** | 25% | — | unavailable |

### 🙅 The honest part

**Token usage came out worse, not better, in most cells.** A shell lets an agent batch several Git commands into one round-trip; individual tool calls cannot be batched, and the tool schemas themselves cost context. Anyone adopting this should expect to trade tokens for safety and fewer agent-facing operations — not to save tokens. Earlier drafts of this README projected a 30% token reduction; the measurement disagreed, and the measurement wins.

**Devin's 65% on the commit scenario is not a defect in this tool.** It scored *identically* using raw Git in both runs. Whether an agent can finish a task at all is a property of that agent; the claim made here is only that these operations never make it less likely, and that held everywhere.

---

## 🚦 Release gates

V1 will not be tagged until reproducible evidence shows:

- ✅ **Zero corruption and zero unrelated-work loss** — absolute, checked per trial, no comparative escape hatch.
- ✅ **Semantic correctness never below raw Git** for every client and scenario.
- ✅ **Complete matrix** across Codex, Claude Code, and Devin CLI, at a minimum of 10 paired trials per scenario/client, with real client sessions.
- ⬜ **100% clean-install activation** across those three clients.
- 📈 **Adoption, token, subprocess, and p95 figures are published as evidence, not gated.** They are properties of a client and model version on a given day; a silent model update must not block a release whose safety guarantees hold.

Every artifact must carry raw results, trial counts, environment and component versions, commit SHA, medians, p95, confidence intervals, and final-state oracles.

---

## 🗺️ Status

**Do not treat this checkout as a released service.**

The v1 release candidate implements the eleven operations, guarded mutation recovery, matching CLI/MCP transports, client registration, doctor diagnostics, metadata-only telemetry, semantic `git-mine` ingestion, property tests, and reproducible benchmark and Homebrew release gates.

Known open items:

- One benchmark run fails the gate on a **telemetry gap** — two Codex trials completed correctly but recorded no subprocess count. The other run passes. This is a measurement defect, not a correctness one, and it is deliberately left blocking.
- The `usable-git gain` report derives token figures from **fixed estimation constants**, not measurements. Its assumptions contradict the benchmark above and must be validated before its output is quoted.
- The public Homebrew tap remains unchanged until every gate passes.

Decision-complete specifications:

- 📘 [Product behavior](specs/usable-git-v1/PRODUCT.md)
- 📗 [Technical design and verification](specs/usable-git-v1/TECH.md)

---

## 🤝 Contributing

Contributions are welcome. The bar is evidence:

1. **Run the suite** — `bun test` must be green before a change is proposed.
2. **Carry proof, not adjectives** — a performance claim needs a reproducible paired artifact with trial counts, environment, commit SHA, medians, and p95.
3. **Never widen the safety surface silently** — new expansion of files, refs, or fallbacks needs an explicit decision record.

---

## 📝 License

MIT. See [LICENSE](LICENSE).

---

<div align="center">

**Built for agents that should not be trusted with `git add -A`**

[⭐ Star this repo](../../) if the idea is useful to you.

</div>

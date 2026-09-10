# 🟩 Pixel

> **A local control layer that helps coding agents spend less time on simple repository work.**

[![agent-config managed](https://img.shields.io/badge/agent--config-managed-blue)](https://github.com/LivioGama/pixel-rules)

<a href="https://liviogama.github.io/agent-config/redirect.html?url=https://raw.githubusercontent.com/LivioGama/pixel-rules/main/rules/pixel.md"><img src="https://raw.githubusercontent.com/LivioGama/agent-config/main/assets/install-badge-small.jpg" alt="Install pixel rules" height="40" /></a>

Pixel is not just a search box. It is the control layer for the whole path from a task to a safe Git change.

Pixel runs locally, connects repository structure with repository history, and returns evidence with boundaries. When it cannot prove that an answer is complete, it says so.

## 🎯 The goal

Make as much repository work deterministic as possible. If something can be answered or carried out from repository state, history, structure, or a proven flow, Pixel should do it directly—with evidence, clear boundaries, and safe recovery. Genuine ambiguity is where the agent should spend its time.

<p align="center">
  <img src="docs/pixel-line.svg" alt="A direct line from a repository to a highlighted answer" width="760" />
</p>

## ⭐ The Pixel flow

```text
task → targets → search / resolve → impact → edit → changes / review → reconcile / publish
```

Each step answers a different question:

| Step | What Pixel adds |
| --- | --- |
| **Search** | Find text with `search`, code by meaning with `ask`, or a phrase/error with `resolve`. |
| **Impact** | Use the code graph to see callers, callees, call paths, processes, clusters, and affected files. |
| **Flow** | Start with `targets`, keep a prioritized P0/P1/P2 task map, and restore it after context compaction. |
| **Flow replay** | Save, retrieve, revise, and replay proven browser, authentication, and configuration flows instead of rediscovering them. |
| **Git operations** | Combine inspection, review, branch reconciliation, commit, push, and shipping into explicit guarded operations. |
| **Recovery** | Search deleted code and past diffs with `excavate`, then plan a last-known-good restore with `rescue`. |
| **Truthfulness** | Mark answers as complete, capped, unresolved, or lower-bound instead of quietly presenting guesses as facts. |
| **Agent integration** | Rewire ordinary agent work toward indexed retrieval, impact analysis, flow replay, and safer Git operations. Strict blocking is not the default. |

That combination is Pixel’s purpose: not merely finding code, but helping an agent move from **“what is this task?”** to **“this change is understood, reviewed, and safely published.”**

## 🚀 Quick start

### 1. Install Pixel

**macOS (Apple Silicon) or Linux — via Homebrew:**

```bash
brew tap LivioGama/tap
brew install pixel
```

**From source (Intel Mac, or manual build):**

```bash
git clone https://github.com/LivioGama/pixel.git
cd pixel
cargo build --release -p pixel-cli
mkdir -p ~/.local/bin
cp target/release/pixel ~/.local/bin/pixel
```

### 2. Let Pixel prepare the repository

Pixel prepares a repository automatically when the agent first needs it. You can optionally warm it up or check its health yourself:

```bash
pixel ready .
pixel doctor .
```

`ready` is an optional warm-up. `doctor` is an optional health check.

### 3. Keep using your agent

You do not need to learn Pixel’s command vocabulary. After installation, your coding agent can use Pixel’s local tools through its hooks and integration.

```bash
pixel install
```

## ⏱️ What Pixel removes from the wait

An agent should not spend five minutes searching for one error, reconstructing a branch state, or wandering through Git history. Pixel turns those pauses into bounded local operations:

| The frustrating wait | What Pixel does instead |
| --- | --- |
| **“Where is this error or label?”** | Finds it through the local index and connects it to the relevant code. (`search`, `resolve`, `ask`) |
| **“What files do I actually need?”** | Builds a prioritized task map instead of reading the repository at random. (`targets`) |
| **“If I change this, what breaks?”** | Shows callers, callees, call paths, processes, and changed flows. (`impact`, `uses`, `trace`, `changes`) |
| **“It worked before. What happened?”** | Searches old diffs and deleted code, then identifies a likely last-known-good version. (`excavate`, `rescue`) |
| **“Can I sync, commit, and push this safely?”** | Combines repository inspection, review, reconciliation, commit, and push with state checks. (`inspect`, `review`, `reconcile`, `publish`, `ship`) |
| **“The agent forgot the task after compaction.”** | Restores the active task map and makes previous agent sessions and errors searchable. (hooks, `recall`, `sniper`) |

The result is less waiting for commands that should have taken milliseconds, fewer repeated explanations, and fewer risky “let me just try Git” detours.

## 👥 What this feels like as a person

You say to your coding agent:

> “The login worked before the last change. Find what broke, show me everything affected, and publish the fix safely.”

Without Pixel, that can become a long sequence of searches, file reads, `git log`, branch checks, conflict recovery, and repeated explanations.

With Pixel, the agent can follow one local path:

1. Find the relevant code and the change that removed the old behavior.
2. Show the callers and flows that could be affected.
3. Identify the smallest useful set of files to inspect.
4. Recover a last-known-good version if needed.
5. Review, reconcile, commit, and push with explicit safety checks.

You keep talking to the agent. Pixel takes care of the repository mechanics that should not require your attention.

## 🧭 How Pixel works

When you prepare a repository, Pixel builds a small local knowledge base:

1. **Text index** — finds literal matches quickly.
2. **Code graph** — records symbols, imports, calls, and related files.
3. **History index** — optional; makes deleted code and past diffs searchable.
4. **Local service** — keeps frequently used data warm between commands.

The data lives under `.pixel/` in the repository. Search and analysis stay local after any required model download. Git synchronization commands are the exception because they intentionally talk to your configured Git remote.

## 🛡️ Safety model

- Read operations do not modify your application files.
- `targets` can create `.pixel/targets.json` for task-scoping hooks.
- `rescue` plans before it restores anything.
- `publish`, `push`, `ship`, `reconcile`, and `rewrite` are explicit operations with state checks and recovery keys.
- Answers include boundaries and caveats when an index is stale, capped, or incomplete.
- The graph is an aid for navigation, not a replacement for tests or code review.

## 🤖 Agent integration

Pixel can install its rules and integrations for supported coding-agent CLIs:

```bash
pixel install
pixel doctor .
```

The installed guidance encourages agents to search and understand the repository before editing, then use Pixel’s impact, replay, and Git workflows when they help. Pixel is rewire-first: it steers an ordinary command toward a better local operation when that rewrite is safe, while leaving the original command available when it is not. Ordinary commands are not blocked by default.

Installation is additive. Pixel checks the current files first, preserves existing configuration and instructions, changes only its managed sections, and backs up a file before changing it. Re-running the install is safe.

### Two installation layers

`agent-config` and `pixel install` have different jobs:

- **agent-config** owns the canonical Pixel rule source and distributes it to the agent configuration directories. It provides guidance; by itself, it does not install all Pixel runtime integrations or hooks.
- **`pixel install`** owns the runtime layer: bounded task context, safe search routing, flow-replay hooks, per-agent settings, configuration backups, and installation checks through `pixel doctor`.

### How `pixel install` works

`pixel install` can work with or without agent-config being present:

1. If `~/.agent-config/rules/pixel.md` exists, Pixel uses the distributed rule source as its full agent guidance. If it is missing, Pixel installs a short fallback instruction block and still configures the supported runtime integrations.
2. Pixel places that guidance in existing `CLAUDE.md` or `AGENTS.md` files using managed markers.
3. If none exists, Pixel creates `~/.claude/CLAUDE.md`—inside Claude’s configuration directory—so a new machine still gets the guidance.
4. Pixel detects supported installed CLIs (or existing agent configs), then wires only the integrations that apply. Claude, Devin, Codex, Gemini, zcode, Cursor, and pi are supported where their interfaces allow it. Codex is supported through its native configuration at `~/.codex/hooks.json`.
5. Lifecycle hooks deliver context at session start and after compaction. Nontrivial prompts can receive up to 4 KB of ranked task context from a responsive local daemon. Retrieval has a 750 ms worker deadline, excluding process startup and input parsing; this is not a wall-clock latency guarantee under host contention. Cold/unavailable daemons and timeouts leave the prompt usable without retrieval context; task-boundary detection is independent.
6. Files that need changes are backed up, and `pixel doctor .` checks the resulting installation.

### Claude Code task runtime

For Claude Code, Pixel keeps a session packet at `.pixel/task-runtime.json`
and an append-only durable task ledger under `.pixel/tasks/`. Neither is a
read/edit allowlist. Packets preserve bounded ranked evidence across
compaction; the ledger records acceptance, sandbox ownership, worker lifecycle,
and promotion facts.

The automatic path is deliberately narrow: an explicit local coding imperative
may be accepted only after Pixel has created an isolated candidate snapshot and
started a real worker. Only then does the Claude `UserPromptSubmit` hook reject
the foreground prompt. Questions, planning, reviews, ambiguous prompts, empty
repositories, and every launch failure stay in the foreground. The initial
automatic candidate owns the full tracked snapshot; ranked targets never become
write permissions.

```bash
# Inspect the active Claude packet for a known Claude session ID
pixel task show --session <session-id> . --json

# Make the next meaningful prompt start a new task generation
pixel task reset --session <session-id> .

# Explicit task lifecycle for a controlled worker run
pixel task accept "fix parser behavior" . --json
pixel task sandbox-create <task-id> candidate-a --owned-path crates/pixel/src/main.rs .
pixel task worker-start <task-id> candidate-a . --json

# Validate a proposed fanout plan without executing it
pixel task plan-validate <task-id> --file plan.json . --json
```

Candidates are Git worktrees. Dirty tracked changes are snapshotted into a
task-owned overlay; untracked or credential-shaped WIP is refused rather than
copied. Promotion is compare-and-apply: it rejects no-op candidates,
out-of-ownership changes, and overlapping primary-worktree drift. Race workers
are capped at three pre-registered candidates and promote only an eligible
on-disk diff, never a model completion claim.

Pixel does not yet infer safe fanout from prose, treat a plan as a contract,
or use model output as completion evidence. `task plan-validate` accepts only
explicit lanes, paths, symbols, dependencies, and bounded candidate counts;
overlap or incomplete evidence remains serial. Worker output is intentionally
discarded until a dedicated Pixel log sink exists, so health/failover policy is
not yet connected to automatic worker retries.

Pixel is a CLI plus rewire-first integrations, not an MCP server.

### Automatic search routing: deliberately narrow

Claude Code, Codex, and Devin use provider-specific hooks. Supported standalone
literal `grep`/`rg` searches over one explicit indexed file can execute through
`pixel search-compat <rg|grep> -- <original arguments>`, preserving native output
and exit status. Ordinary `pixel search` remains the richer, bounded regex API.
Recursive searches, pipelines, unsupported flags, RTK-wrapped searches, uncertain
coverage, binary/CRLF files, and output overflow retain native execution. Pixel
does not drop flags or emit partial results before falling back.

Codex's rewrite protocol requires an explicit `allow` decision for these narrowly
supported read-only calls; this is not blanket authorization for other commands.
Codex also requires trust for each current unmanaged hook definition. Installing
or changing a hook does not grant that trust; review it through Codex's `/hooks`
interface. Pixel does not bypass trust or manufacture trusted hashes. See the
[Codex hook trust contract](https://learn.chatgpt.com/docs/hooks#review-and-trust-hooks).
Claude's recognized `rtk hook claude` registration can be coordinated behind one
Pixel hook, preserving the original RTK handler for unsupported commands. Unknown
overlapping hooks are preserved rather than competing to rewrite the same input.

For a project-local Codex configuration whose existing command hooks include
real deny guards, Pixel can adopt the full `PreToolUse` set into one composed
runtime. It snapshots the already-enabled handlers privately, replays the
original hook input to matching handlers, preserves any denial, and performs a
compatible read-only rewrite only when no foreign handler blocks or mutates the
input. Reinstall refuses a changed managed group; uninstall restores the exact
saved `PreToolUse` array. Unknown matcher or handler shapes remain untouched.

Read-only retrieval counts produce advisory warnings, never command rejection.
Ranked targets and graph edges are evidence to investigate, not proof of exhaustive
relevance or inevitable breakage. Doctor distinguishes registration/protocol checks
from live verification: a configured hook is not proof that an agent executed it.

### Other coding agents

`pixel install` detects supported installed CLIs and configures the matching integration where available, including Devin, Gemini, zcode, Cursor, and pi where supported. For an unsupported tool such as Warp CLI, there are no native Pixel hooks to install: load `~/.agent-config/rules/pixel.md` when available—or the fallback `~/.claude/CLAUDE.md`—into that tool’s global or project instructions, then use `pixel` directly.

## ⚙️ Useful maintenance commands

```bash
# Rebuild the text index
pixel index .

# Include commit metadata and diff text in the history index
pixel index . --history

# Rebuild the code graph
pixel graph .

# Check index and graph freshness
pixel status .

# Rebuild Pixel and replace the installed binary
pixel upgrade

# See Pixel’s recent actions and errors
pixel log .
```

Run `pixel --help` or `pixel <command> --help` for the complete command and option reference. Add `--json` to supported commands when another tool needs machine-readable output.

## 🌍 Platform

Pixel is written in Rust (requires Rust ≥ 1.85, edition 2024) and is designed for local macOS and Linux development environments. It uses a per-repository Unix-socket daemon where available and falls back to running in-process.

### Network access

Pixel is local-first, but two features require network access on first use:

- **Semantic search** (`pixel ask`, `pixel recall`): downloads an embedding model from Hugging Face on first use via `pixel recall setup`. After download, all inference is local (CPU). Disable with `--no-default-features` at build time if offline-only operation is required.
  - Source builds default to `fastembed` (ONNX) **and** `model2vec`.
  - **Prebuilt Linux binaries ship `model2vec` only.** `fastembed` depends on ONNX Runtime, which publishes no musl build, so the release binaries are built with `--no-default-features --features model2vec`. `model2vec` is pure Rust, but it still fetches its model from Hugging Face on first use — so a Linux install needs network access once before semantic search works.
- **Git remote operations** (`pixel publish`, `pixel push`, `pixel ship`): these intentionally talk to your configured Git remote.

All other operations — indexing, search, graph analysis, flow replay, history excavation — run entirely locally after any required model download.

## 📝 License

MIT. See [`NOTICE`](NOTICE) for attribution details for derived components.

---

<div align="center">

**Make repository work easier to see, understand, and recover.**

</div>

# Pixel architecture

This document describes how the workspace is put together: the crates, the
data that lives on disk, the daemon wire contract, and the paths a command
takes from the CLI to an answer. It is the map a contributor (human or agent)
should read before touching more than one crate.

For what Pixel does and why, read `README.md`. For per-turn project rules,
read `CLAUDE.md`.

## One-screen summary

```text
 agent CLI (Claude, Codex, Cursor, pi, …)
   │  agent prompt + shell wrappers         (pixel-install)
   ▼
 pixel binary (crates/pixel)  ── clap commands, prints text or --json
   │  Request = pixel_proto::Op   (NDJSON over a Unix socket)
   ▼
 pixel-daemon ── Service::handle(Op) -> Envelope<Value>
   │  in-process fallback when no daemon is reachable
   ├── pixel-index    trigram text index          .pixel/ shards
   ├── pixel-graph    symbols / imports / calls   .pixel/graph.db
   ├── pixel-facts    history facts + diffs       .pixel/history.db (lazy: built on first history query)
   ├── pixel-rank     task -> ranked file list    (scoring pure; signals read git + sniper)
   ├── pixel-context  token-budgeted rendering    (pure)
   ├── pixel-ops      guarded git mutations       .pixel/journal, snapshots
   ├── pixel-recall   transcript corpus + embeddings (machine-wide; lazy: ingested on first recall query)
   └── pixel-git      the only git subprocess wrapper
```

Everything below the CLI is a library crate. Only `crates/pixel` builds a
binary, and Pixel is deliberately a CLI plus hooks, not an MCP server.

## Crates

| Crate | Role | Depends on (pixel crates) |
| --- | --- | --- |
| `pixel-cli` (bin `pixel`, in `crates/pixel`) | Command-line surface. Parses argv with clap, talks to the daemon or runs the service in-process, prints text or JSON. Also hosts the hook entry points (`hook guard`, `hook session-start`, `hook prompt-submit`, `hook post-compaction`, `hook post-tool-use`), `rescue`, `recall`, and `sniper` sub-commands. | every library crate except `pixel-context` (reached through the daemon) and `pixel-bench` |
| `pixel-proto` | The shared contract crate: `Envelope`, `PixelError` and `ErrorCode`, `Epistemics`, `SnapshotInfo`, `Budget`, `Warning`, the `Op` request enum, and `commands::RENAMED_COMMANDS`, the old-to-new CLI subcommand names the CLI accepts as hidden aliases until 1.0. No I/O, no business logic. Every other crate that speaks the wire format depends on it, and it depends on nothing internal. | none |
| `pixel-daemon` | Transport-agnostic `Service` (`api.rs`) and the Unix-socket NDJSON daemon with filesystem watching (`daemon.rs`). Dispatches each `Op` to the right library, attaches snapshot and epistemics metadata, and is the one place a retrieval envelope is built. Also hosts the recall daemon service. | index, graph, context, rank, proto, ops, facts, session, recall, git |
| `pixel-index` | Sparse n-gram (trigram) text index: gram extraction, window weighting, posting-list algebra, git-anchored base and delta shards, working-tree overlay, query planner, verification, and the `gitsync` helpers that read HEAD, branch, and porcelain status. | git |
| `pixel-graph` | Code graph: tree-sitter extraction of symbols, imports, and call sites per file; import resolution; tiered call resolution with an epistemic envelope; and the analyses `impact`, `trace`, `process`, `cluster`, `changes`, `targets`. `store` owns the SQLite schema. | git, index |
| `pixel-facts` | History-wide fact and diff ingest, search, lifecycle, and rescue discovery. Owns `history.db` plus trigram history segments. Demand-driven: the daemon never ingests at startup — the first history query runs a bounded catch-up (`PIXEL_FACTS_QUERY_BUDGET_MS`, default 3 s) and spawns the low-priority keep-fresh loop that never blocks queries. `build-index --history` remains the explicit full build. Backs `excavate`, `lifecycle`, `history-search` and `rescue` discovery (`resolve` is the graph's concept index). | git, index |
| `pixel-rank` | Fusion core for `targets` and ranked `search`: task text and signal inputs in, closed prioritized P0/P1/P2 file list out. The scoring is pure; `compute_signals` gathers the activity channel itself (git log churn when facts have none, and a failed or capped scan is reported as unavailable rather than as an empty map) and takes the session and error-sink channels from its caller — the daemon feeds neither of those. | graph, git, session |
| `pixel-context` | Semantic compression of code-context items: layered renderings that fit a token budget instead of raw source dumps. | none |
| `pixel-ops` | Safe git mutation infrastructure ported from usable-git: snapshot store, repository lock, operation journal, recovery keys. Implements `inspect`, `review`, `history`, `diff`, `publish`, `push`, `ship`, `branch`, `update`, `sync`, `reconcile`, `rewrite`, `provenance`, `branches`, `env`. | git |
| `pixel-git` | The single git subprocess wrapper for the workspace. Replaced three earlier ad-hoc wrappers. Any crate that shells out to git goes through `GitRunner` (timeout, output cap, redacted stderr); `crates/pixel-git/tests/boundary.rs` fails the build on a `Command::new("git")` in any other crate's non-test code. | none |
| `pixel-recall` | Machine-wide LLM transcript retrieval: ingests Claude Code, Codex, opencode, Devin, Cursor, zcode, and Gemini transcript stores into one SQLite corpus, then serves lexical and semantic search. Demand-driven: nothing scans transcripts until a recall query runs — the in-process path then catches up per agent (last week cold, since-last-ingest warm, capped at 30 days; `recall index` for the full history). Owns the embedding backends (`fastembed` ONNX and pure-Rust `model2vec`, both behind features). | index, rank |
| `pixel-session` | One-look error capture: every error from every layer lands at throw time in one structured local SQLite sink, queryable in one call. | git |
| `pixel-actionlog` | Append-only local JSONL invocation records: measured command/outcome/duration/output volume plus versioned workflow estimates; backwards-compatible `pixel action-log` and `pixel token-savings` reporting. | none |
| `pixel-release` | `pixel check-release`: the consistency checks a release tag must pass (CLI version, `Cargo.lock` freshness, changelog cut). Pure functions over file contents. | none |
| `pixel-flow` | Deterministic browser and configuration flow replay: save, get, list, revise, replay, delete proven agent-browser paths. Flows live under `~/.local/share/pixel/flows/`. | none |
| `pixel-install` | Idempotent `pixel install`, `pixel uninstall`, `pixel doctor`: deploys the bundled prompt, the Claude shell wrapper, the Codex `developer_instructions` config key and the OpenCode `AGENTS.md` managed block, backs up changed files; retains legacy hook/routing and cleanup implementations without activating them. | proto, daemon, index, facts, git |
| `pixel-bench` | Criterion benches and a real-source corpus builder (gram extraction, latency, NDCG relevance). Not shipped. | index (dev: daemon, proto, recall) |

Dependency rule: `pixel-proto` and `pixel-git` are leaves (so are `pixel-context`, `pixel-actionlog`, `pixel-flow` and `pixel-release`; `pixel-session` depends on `pixel-git` only). `pixel-daemon` is
the integration point and is the only library crate allowed to depend on
almost everything. The CLI depends on the daemon plus whatever it needs for
commands that never touch the daemon (install, flow, actionlog, release-check).

## Command surface

Every subcommand of the built binary, one line each, in `pixel --help`
order. `crates/pixel/tests/cli/docs_drift.rs` fails when a command listed
by `--help` is missing here, or when any `` `pixel <name>` `` in README,
ARCHITECTURE, CONTRIBUTING, `docs/manual-setup.md`, the site's `website/content/docs.md` and `benchmarks.md`, or the bundled agent prompts
(`crates/pixel-install/assets/pixel-agent-prompt.md`,
`pixel-subagent-prompt.md`) names a command the binary does not have. The 45 names renamed after 0.2.4 still parse as hidden aliases until 1.0 (table in `pixel_proto::commands::RENAMED_COMMANDS`, README "Renamed commands"); `--help` and this table list only the current names.

| Command | Does |
| --- | --- |
| `pixel build-index` | Build (or rebuild) the text index for a directory tree |
| `pixel search-content` | Search the indexed tree with a regex pattern. |
| `pixel search-like-rg` | Native-output literal file search for automatic routing; unsupported inputs execute the original rg/grep command without modification |
| `pixel run-recipe` | Compile and execute one bounded deterministic retrieval recipe |
| `pixel search-meaning` | Semantic code search: embed a natural-language question ("how is authentication handled?") and rank files by semantic/lexical rank fusion. |
| `pixel scope-task` | Sniper target list: task description in, closed prioritized file list out (P0 = start here, P1 = likely, P2 = droppable). |
| `pixel plan-rollback` | Surgical revert planner: locate the files a problem points at, list recent versions with the likely-breaking commit flagged, recommend a last-known-good candidate. |
| `pixel find-symbol` | Look up symbols by name in the code graph |
| `pixel list-signatures` | All signatures in a file — the skeleton view at ~10% of Read cost |
| `pixel note` | Human notes on the map: durable annotations keyed by file + symbol name (or concept norm). |
| `pixel repo-map` | Structural repo map: every indexed file with its symbols. |
| `pixel pack-context` | Budget-fitted context for a symbol uid |
| `pixel impact` | Blast radius of a symbol (callers upstream / callees downstream) |
| `pixel who-calls` | Direct callers or callees of a symbol |
| `pixel rename` | IDE-style symbol rename: graph-resolved definition, call, reference, and import sites, each verified against a fresh tree-sitter parse before writing; unresolved same-name sites are reported, never guessed. `--dry-run` prints the edit set without touching files |
| `pixel call-path` | Call path between two symbols |
| `pixel evaluate` | Bounded predicate evaluation with a witness: does a path exist between two symbols in the indexed call graph, with the snapshot the answer is about, an exhaustive-traversal absence, or a typed reason for not answering |
| `pixel list-flows` | Discovered execution flows |
| `pixel list-areas` | Functional-area clusters |
| `pixel what-changed` | Symbols/flows affected by working-tree changes |
| `pixel rebuild-graph` | Force (re)build of the code graph db |
| `pixel status` | Index + graph freshness status |
| `pixel prepare-repo` | Make a repository ready for agent work: index, graph, and warm daemon |
| `pixel index-stats` | Show raw shard metadata (legacy) |
| `pixel daemon` | Manage the per-root background daemon |
| `pixel recall` | Search and browse LLM CLI transcripts (machine-wide corpus) |
| `pixel list-errors` | One-look error capture: query the sniper error sink |
| `pixel classify` | Zero-shot decision over a bounded label set through an OpenAI-compatible chat completion: `--remote-preset openrouter\|ollama\|local` picks the endpoint and key variable, `--remote-model` the model. The model verbalizes one probability per label (strict JSON schema), renormalized to sum 1; output always discloses `snapshot.deterministic=false` and `snapshot.provider`. `--context` keeps shared framing out of the state. State, context and criteria are capped separately with disclosure. No daemon; `--jsonl` serves one decision per stdin line |
| `pixel web-search` | Deterministic web lookup for terms the index cannot know — the refine step of a gated `pixel plan`. SearXNG alone when `PIXEL_WEB_SEARCH_URL` is set; otherwise DuckDuckGo, then Wikipedia while the hits are fewer than `--limit`. No LLM, no daemon |
| `pixel repo-state` | Show repo state: HEAD, branch, dirty files, fingerprints; `--include-clean` adds the capped tracked-clean list |
| `pixel review-changes` | Review working-tree changes (staged, unstaged, untracked, conflicted) |
| `pixel commit-history` | Commit history with detail levels and byte caps |
| `pixel diff` | Structured diff between two refs (or ref → working tree) |
| `pixel commit` | Stage files, commit, and optionally push (crash-safe, idempotent) |
| `pixel push` | Leased push to a remote (crash-safe, idempotent) |
| `pixel commit-and-push` | Publish + push in one op (commit then leased push) |
| `pixel new-branch` | Create a new branch from HEAD (or --from <ref>) |
| `pixel fast-forward` | Fast-forward merge to a target OID (refuses non-ff + dirty intersection) |
| `pixel fetch` | Fetch from a remote (idempotent) |
| `pixel find-code` | Engine 1: resolve a phrase to code via the concept index |
| `pixel search-history` | M3: history-wide fact + diff search |
| `pixel file-history` | Engine 2: lifecycle of a path or token |
| `pixel dig-history` | Engine 2: history-wide discovery (rescue v2) |
| `pixel sync-branch` | Engine 4: one-call deterministic branch sync |
| `pixel record-event` | M5: journal a session event (fire-and-forget) |
| `pixel install` | Idempotently deploy the agent prompt, the Claude shell wrapper, the Codex developer_instructions config key and — when `~/.config/opencode` exists — the managed prompt block in OpenCode's global `AGENTS.md` |
| `pixel uninstall` | Remove everything `pixel install` wrote: managed blocks from agent-config files, hook entries from all settings files, hook scripts, the pi guard extension, the rule source file, and the pixel binary itself. |
| `pixel check-release` | Check that a release tag is consistent with the tree before anything is built or published: crates/pixel/Cargo.toml carries the version, Cargo.lock is fresh for every workspace member, CHANGELOG.md has the `## [x.y.z]` heading and an empty Unreleased section. |
| `pixel self-update` | Rebuild the binary, stop the daemon, copy the new binary to the install path, and optionally restart the daemon. |
| `pixel doctor` | Health check: install state, daemon, index/graph/facts freshness |
| `pixel run-hook` | Hook entrypoints (guard, session-start, metrics relay) invoked by agent hooks |
| `pixel config` | Persistent layered settings — `metrics on|off` writes `<root>/.pixel/config.json` (`--global` → `~/.pixel/config.json`); bare reports the effective setting and its layer |
| `pixel task-state` | Inspect or reset Claude Code's local Pixel task-runtime packet |
| `pixel action-log` | Self-assessment: pixel's own action log (what ran, what went wrong). |
| `pixel token-savings` | Token-savings report: for retrieval-shaped commands (search/query/ context/resolve) that recorded snippet-vs-pool volumes, aggregate the fraction of the candidate pool the agent did NOT have to read. |
| `pixel squash-branch` | Squash every commit on the current branch since its base into ONE commit (crash-safe, backup-ref'd), optionally force-pushing with lease |
| `pixel who-wrote` | Per-region blame attribution: who introduced/owns each region of a file |
| `pixel list-branches` | One-call read-only branch inventory: ahead/behind, merged, stale, unpushed — the deterministic "did you push everything?" answer |
| `pixel edit-env` | Additive-only, key-level .env mutations with snapshots and restore. |
| `pixel plan` | Deterministic todo list generation from code analysis; persists findings in `.pixel/plan.json` so `--status`/`--done N`/`--undone N`/`--prune` track execution state across re-plans without the daemon |
| `pixel coverage` | Per-language index coverage: files on disk vs files indexed, with unrecognized extensions surfaced — the "what did the index miss?" answer |
| `pixel audit` | What an agent reads to learn what the largest source files contain: each whole file against its `list-signatures` outline in tokens (bytes / 4, floored), the total and the per-file median, files changed since indexing or with no signatures left out and counted, then per-language coverage. Builds the graph on a first run only; local, read-only, sends nothing |
| `pixel workspace` | Multi-repo registry (`.pixel/workspace.json` members add/remove/list); `--workspace` fans `impact`/`who-calls` out across registered repos with per-repo provenance |
| `pixel index-pack` | Freeze the index into one checksummed `.pxpack` bundle — CI builds once, teammates install instead of re-indexing |
| `pixel index-unpack` | Install a packed index bundle from a path or https:// URL, hash-verified, refusing to overwrite a live index without `--force` |
| `pixel mcp` | Serve this repo's index over MCP stdio — the single integration for every MCP-capable agent (search, resolve, impact, callers/callees, evaluate, context, status) |
| `pixel replay-flow` | Save, retrieve, list, revise, and replay proven agent-browser paths (auth flows, config flows) so the agent follows a deterministic shortcut instead of re-discovering the UI from scratch every time |
| `pixel help` | Print this message or the help of the given subcommand(s). |

## On-disk state

Per repository, under `.pixel/` (git-ignored):

| Path | Owner | Contents |
| --- | --- | --- |
| `base.shard`, `delta.shard`, `state.json`, `build.lock` | `pixel-index` | Base shard for all tracked files at a pinned commit, delta shard for files changed between that commit and HEAD, and `state.json` as the delta-layer sidecar (tombstones for superseded base paths). The dirty working-tree overlay is in memory only. First process to hold `build.lock` builds; others wait. |
| `graph.db` | `pixel-graph` | SQLite: files, symbols, edges with resolution tier. Built lazily on first graph command. |
| `history.db` (+ `-wal`, `-shm`, `history.db.lock`) | `pixel-facts` | SQLite: commit facts, diff text, lifecycle. Populated by `pixel build-index --history` or the daemon ingest thread. |
| `targets.json` | CLI `targets` | Active task map (version 2): tasks with ids, timestamps, and P0/P1/P2 paths. Read by the guard hook and re-injected after compaction. |
| `actions.jsonl` | `pixel-actionlog` | One line per invocation, with the route and phase timings of each request it served (`serve`). |
| `reconcile-conflict.json`, `env-snapshots/` | `pixel-ops` | Conflict marker left by `reconcile` for the guard, and the pre-mutation copies `env` takes. |
| `user-state.json` | `pixel-install` | Per-repository install state. |
| `calls.json` | CLI | Circuit breaker counters for repeated identical calls. |
| `task-runtime.json`, `tasks/<id>/task.json`, `tasks/<id>/events.jsonl` | CLI `task` and the hooks | Claude Code task-runtime packet: the active task record and its event log. |

The prompt-submit hook writes task boundary events to
`~/.pixel/inbox/task-boundary.json`, outside the repository.

Machine-wide:

- `~/.local/share/pixel/flows/`: saved flows (`pixel-flow`, `$PIXEL_FLOW_DIR`
  overrides).
- `~/.local/share/pixel/recall/` and `~/.local/share/pixel/models/`: the
  recall corpus (`$PIXEL_RECALL_DIR` overrides) and the embedding models,
  downloaded once from Hugging Face on `pixel recall setup`.
- `~/.local/state/pixel/sniper/<project key>/`: the `pixel-session` error sink
  (`errors-v1.sqlite` plus a `project.json` naming the root);
  `$PIXEL_SNIPER_STATE_ROOT` overrides the state root.
- `~/.local/state/pixel/` (`$XDG_STATE_HOME/pixel`): `pixel-ops` crash-safety
  state, keyed by a hash of the repository's canonical git common directory:
  `journals/`, `snapshots/` and `locks/<hash>.lock/owner.json`. Guarded git
  mutations write nothing under `.pixel/` except the two entries above.
- Daemon socket and pid: `$TMPDIR` on macOS, `$XDG_RUNTIME_DIR` on Linux
  (else `~/.cache/pixel/sockets/`), named
  `pixel-<xxh3 of canonical repo path>.sock`, `.pid` and `.lock`.

## Daemon and wire contract

`pixel-daemon` exposes one function that matters: `Service::handle(Op) ->
Envelope<Value>`. The Unix-socket daemon reads one JSON `Op` per line and
writes one JSON `Envelope` line back. Request handling is single-threaded; an
accept thread and a `notify` watcher feed one channel. The watcher debounces
filesystem events and refreshes the index and graph for changed files. The
daemon exits after thirty minutes idle.

Two version numbers exist and must not be conflated:

- `pixel_proto::ENVELOPE_PROTOCOL_VERSION`: the envelope schema (`protocol`
  field), currently 1.
- `pixel_daemon::api::PROTOCOL_VERSION`: the socket request/response format.
  Bump it when an older daemon process could not safely serve a newer CLI.
  The CLI pings first and compares.

The envelope:

```json
{
  "ok": true,
  "op": "search",
  "protocol": 1,
  "requestId": "…",          // optional
  "snapshot":   { "head": "…", "branch": "…", "dirty_count": 0 },   // `dirty: [paths]` on inspect/review only
  "epistemics": { "closed_world": false, "lower_bound": true, "staleness_ms": 0, "basis": "…" },
  "budget":     { "byteCap": 262144 },
  "result":     { … },        // present when ok
  "error":      { "code": "NOT_FOUND", "message": "…" },   // present when !ok
  "warnings":   []
}
```

`requestId` and `budget` are part of the schema but no response is built
with them yet, so their absence means "not reported". The caps that bite
today report themselves inside the op's own result (`byte_cap`,
`truncated`, `next_cursor`, `next_offset`) or as envelope `warnings`.
`error.code` is the code the daemon classified the message into; the
variants no producer reaches are listed on `pixel_proto::ErrorCode`.

Invariants enforced by `Service::handle`:

- Success carries `result`, failure carries `error`. Never both.
- Every retrieval op (`search`, `resolve`, `targets`, `impact`, `uses`,
  `trace`, `changes`, `context`, `symbol`, `processes`, `clusters`, `plan`) gets an
  `epistemics` object. Ops that hit a cap name it in `basis` and mirror it as
  a warning. Ops that attested nothing get a conservative not-closed-world
  default instead of an implied claim of completeness.
- Retrieval ops and git-state ops (`inspect`, `review`, `diff`, `status`,
  `changes`) get a `snapshot` so the caller can correlate the answer with the
  working tree it was computed against. Only `inspect` and `review` carry the
  `dirty` path list; every other op ships `dirty_count` instead
  (`SnapshotInfo::compact`), so an untracked `vendor/bundle` of 15 000 paths
  does not inflate a `symbol` answer to 240 KB.

Adding an op is one variant on `pixel_proto::Op` plus one arm in
`Service::dispatch`. `Op::op_name` must match the serde tag, and a unit test
in `pixel-proto` checks it.

## Request path from the CLI

1. `main.rs` parses argv with clap. Commands that need the repository call
   `execute(path, Op, no_daemon)`.
2. `execute` discovers the repo root, then tries the daemon: connect to the
   socket, ping with a short timeout, and send the op. If the socket is
   absent it spawns `pixel daemon start --foreground` in the background and
   polls the socket for up to five seconds. `PIXEL_DAEMON_AUTO_START=0`
   disables that.
3. If the daemon path fails, the CLI opens `Service` in-process and calls
   `handle` directly. Both paths return the same `Envelope`.
4. `unwrap_response` turns a failure envelope into an `Err(message)` that
   `main` prints to stderr with exit code 1. Under `--json` the CLI also
   answers on stdout with the failure envelope (`ok: false`, `error.code`,
   the same message) — classified by the daemon's `failure_response`, so a
   CLI-side failure carries the same code as a daemon one — unless the
   command owns stdout (`search-like-rg`, hooks, the statusline) or already
   wrote part of an answer (`check-release --json`). For a success envelope it
   takes `result` and folds `epistemics`, `snapshot`, and `warnings` into it
   without clobbering same-named keys the op emitted.
5. `print_data` serializes the result. With `--json` it is compact on one
   line, otherwise pretty. A global 256 KB cap protects the agent's context
   window (`PIXEL_OUTPUT_CAP_BYTES=<bytes>` overrides it, `0` lifts it). A
   `--json` answer over the cap is cut structurally: the largest arrays are
   shortened, every other field survives, and the object gains
   `truncated: true`, `cap_bytes` and `truncated_arrays` (path, kept,
   total). Only when no array trimming can fit the cap does the output fall
   back to a `{truncated, cap_bytes, note, partial}` wrapper. Human notes
   such as graph-build announcements and lower-bound caveats go to stderr,
   never stdout.

So the CLI's `--json` output is the envelope's `result` with the honesty
fields merged in, not the raw envelope. Anything that needs the full
envelope talks to the daemon socket directly.

## Indexes and freshness

- The text index is git-anchored: base shards correspond to a commit, delta
  shards to changes since, and an overlay covers the dirty working tree.
  `pixel status` reports whether each layer is fresh.
- The graph is built lazily on the first graph command and updated per file
  by the daemon watcher. Without a daemon (CI, `PIXEL_DAEMON_AUTO_START=0`,
  a copied `.pixel/`), the first graph command after an edit compares the
  tree's per-file content hashes with the stored ones in one walk and
  re-extracts only the added/edited files, drops the removed ones and
  re-resolves the calls that targeted them (`pixel_graph::build::tree_delta`
  / `apply_tree_delta`). A full rebuild remains the fallback when the db has
  no freshness signature or when the drift exceeds
  `PIXEL_GRAPH_INCREMENTAL_MAX_PCT` percent of the indexed files (default
  `20`; `0` always rebuilds). The answer's `graph_build` says which path ran
  (`incremental`, `changed_files`, `removed_files`, or `reason`), and the
  stderr notice reads `updated graph.db for N changed file(s)` versus
  `built graph.db on first use`. Call edges carry a resolution tier, and
  analyses report a lower bound when same-name call sites stay unresolved.
- History facts are ingested by a dedicated low-priority thread. Queries
  never wait on ingest; they answer from what is already in `history.db` and
  say so through epistemics.

## Agent integration

`pixel install` deliberately deploys the bundled `pixel-agent-prompt.md`, the
short `pixel-subagent-prompt.md`, a managed shell function for Claude Code, a managed
`developer_instructions` block for Codex and a managed block in Pi's
`~/.pi/agent/APPEND_SYSTEM.md`. It
preserves agent settings and rule files, does not activate legacy provider
hooks or routing, and separately registers the managed Codex `PostToolUse`
metrics hook (`$CODEX_HOME/hooks.json`, default `~/.codex/hooks.json`). The shell functions pass the prompt on a subsequent launch
through the loaded profile; already-running agents and direct executable launches
do not inherit it automatically. The `claude` function adds
`--append-subagent-system-prompt-file` only when `-p`/`--print` is among the
arguments: Claude Code sub-agents do not see the session prompt, and the flag is
honoured in print mode only. `install` writes that variant only when `claude
--version` reports 2.1.261 or newer (older releases exit on the unknown option),
keeps the previous decision when `claude` cannot be probed, and `doctor`
re-derives the expected block from the Claude Code found at check time.
Codex gets the prompt through `developer_instructions` in `~/.codex/config.toml`
(`$CODEX_HOME` honoured), the key it appends to its developer message while
keeping its own system prompt; `model_instructions_file` would replace that
system prompt (it becomes the base instructions). A config key reaches every
Codex front end (CLI, desktop app, extension, `spawn_agent` sub-agents) where
a shell function only fronts interactive shells, so there is no `codex`
function. Codex has no file-backed variant of the key: `install` embeds the
prompt as a TOML literal multi-line string between `<!-- pixel:managed:begin
-->`/`end` marker lines, rewriting only that key with `toml_edit` so the rest
of a file the desktop app also owns keeps its layout, refusing to touch a file
that does not parse, and keeping text outside the markers. `doctor`
(`install.codex-config`) compares the block with the bundled prompt; `uninstall`
removes the block, or the key when nothing else was in it. Pi reads
`~/.pi/agent/APPEND_SYSTEM.md` automatically; that file is shared the same way
(markers, text outside them kept, `install.pi-prompt` in `doctor`, block — not
the file — removed by `uninstall`), so a user's own pi instructions survive.
OpenCode — when `~/.config/opencode` (`$XDG_CONFIG_HOME` honoured) exists —
gets the prompt as a managed block in its global `AGENTS.md`, the one
mechanism both generations honour: v2 accepts the `instructions` config
field but never resolves it, and v1 reads the global AGENTS.md in the same
slot it would otherwise fill from `~/.claude/CLAUDE.md`. Because a *new*
file would shadow that v1 fallback, install seeds a created AGENTS.md with
the claude file's content — the winning file then carries everything the
shadowed one had, plus the pixel block (v2 has no fallback to shadow). The
same step sweeps `opencode.json` for two stale artifacts: `instructions`
entries naming the deployed prompt (dead on v2, a duplicate on v1) and
`plugin`/`plugins` entries whose `pixel.mjs` target no longer exists — a
guaranteed load failure; entries resolving to a real file are left alone.
A config that does not parse as strict JSON is skipped, not rewritten, and
never blocks the AGENTS.md write. `doctor`
(`install.opencode-agents-md`) verifies the block is current and skips
when OpenCode is absent; `uninstall` strips the block (deleting the file
when it held nothing else) and drops leftover instructions entries.

Existing hook entry points remain implemented, separately from active installation:

| Hook event | Command | Effect when independently registered |
| --- | --- | --- |
| `SessionStart` | `pixel run-hook session-start` | Emits the capability block from the op registry. |
| `UserPromptSubmit` | `pixel run-hook prompt-submit` | Task context/boundary detection and guarded task acceptance. |
| `PostCompaction` | `pixel run-hook post-compaction` | Re-injects the active task evidence as additional context. |
| `PreToolUse` | `pixel run-hook guard` | Bounded compatible command routing; native fallback and host permissions remain authoritative. |
| `PostToolUse` | `pixel run-hook post-tool-use` | After an edit, emits the dependants of what was just changed. |
| `PostToolUse` (Codex) | `pixel run-hook metrics` | Codex tool results drop stderr, so the finalized invocation's 🟩 metrics line is re-emitted as `additionalContext` — correlated to the action record by cwd + argv, silent on any miss, and suppressed by the same `metrics` opt-out. |
| (Codex install step) | `pixel run-hook composed-guard` | Runs a sealed install-time snapshot of a foreign hook before Pixel's Codex rewrite. |

`pixel doctor` checks current installation artifacts and distinguishes configured
or protocol-checked hooks from observed live execution. Dormant registration code
is not an installed feature. Legacy uninstall behavior remains available.
Every check is listed in `pixel_install::doctor::CHECKS` with a stable id and
the command that repairs it (`pixel doctor --list`): `--only`/`--skip` select
by id, and each yellow or red check reports that command as `fix`.
`--fix` runs them: `repair_plan` folds the flagged checks into one run of each
distinct catalogue command, in catalogue order, `run_repair` executes it with
the running binary, and the checks are re-run so each repair is judged
`fixed`, `not_converged` or `failed` from the new report, not from its exit
code. A command only one outcome names (the `rm` of an orphaned RTK backup)
is never run. The exit code carries the verdict: 0 when no check reaches `--fail-on` (default `red`),
1 when one does, 2 when the checks could not run.

### Invocation accounting and chat delivery

`pixel-actionlog` owns local metrics, not a parallel observability engine. A
top-level invocation correlates outcome, measured elapsed duration and rendered
output bytes with a versioned native-workflow estimate. Existing logs remain
readable. No additional retrieval, native comparison command, model request, or
repository sweep is justified solely by metrics calculation.

The byte approximation is roughly one token per four UTF-8 bytes and includes
reporting overhead. Measured output covers rendered CLI stdout, CLI-owned diagnostics and top-level
errors, not lower-level library or subprocess streams. V1's fallback volume policies are 4 KiB per assumed distinct
returned file read and 1 KiB per native-command output. They are assumptions, not
measured averages. `workflow-v2` measures the one case it can: `list-signatures`
stands in for reading one whole file, so its baseline is that file's size (a
`stat`, no source read) with no assumed command, and the live line states
`full read N tok, pixel answer M tok (-X%)` from the file and the stdout answer
(`answer_bytes`), both floored bytes / 4 as in `scripts/bench-read-savings.sh`.
Records keep the version they were written with. Estimates consider only returned evidence/relationships and
represented native steps. Partial results remain partial; meaningless comparisons
are unavailable; zero and negative savings are retained. There is no external
telemetry, hidden reasoning estimate, or monetary claim.

Time accounting is separate from byte accounting. An optional `time_estimate`
records `estimator_version: sequential-v1`, `round_trip_ms`, `native_command_ms: 0`,
`sequential_steps`, and signed `saved_ms`. Legacy records lacking these fields
remain unavailable for time comparisons; they are not silently recalculated.
`pixel token-savings` adds `time_estimates` groups keyed by token/time estimator
versions, effective assumptions and coverage, preserving earlier summaries.

Time savings are a separate `sequential-v1` workflow estimate, not measured
LLM latency. Let `steps = native_commands + distinct_files`; relationships do
not add round trips. A zero-step baseline is unavailable; otherwise the estimate
in milliseconds is:

```text
max(steps - 1, 0) * round_trip_ms - measured_pixel_duration_ms
```

One shared initial LLM/tool round trip cancels. The default policy assumes
**2000 ms per sequential round trip** and **0 ms of native command execution**.
`PIXEL_METRICS_ROUND_TRIP_MS` overrides the round-trip assumption with an unsigned
integer number of milliseconds (zero is allowed); unset, invalid, non-UTF-8, or
overflowing values use 2000. These assumptions and the estimator version are
recorded with each new invocation, not applied retroactively to old records.
Batching or parallel native workflows may require fewer round trips: this is
not a measured end-to-end speedup or a guarantee. Negative time savings are
retained; missing evidence is unavailable, and capped comparisons are partial.

The authoritative `🟩 Pixel · ...` line distinguishes `tokens saved (workflow
estimate)` from seconds `saved (sequential estimate)`. Both labels mark partial
comparisons. Measured execution duration stays distinct from both estimates;
no extra model call or native benchmark is run to compute the time estimate.
A missing comparison is never a silently dropped row: it renders
`unavailable: <reason>` (no policy baseline, failed operation, a render-cap or
depth-cap refusal, an uninitialized accumulator, or a zero-step baseline), and
a baseline that saves nothing renders `no estimated … saving`. The reason is
recorded as `comparison_gap` — a zero-step baseline is the one inferred from
the recorded evidence at render time — so a replay of the record states the
same cause.

Ordinary CLI boundaries emit an authoritative metrics line on stderr after the
result/error without changing JSON stdout. On a failure the `pixel: <error>`
diagnostic is repeated after that line, so the last stderr line still names the
failure when a caller reads only the tail. `--metrics=off` and `PIXEL_METRICS=0`
disable live reporting (and the repeat), not local accounting. Metrics failures cannot change success or safety behavior.
Exact-output search compatibility, hooks, protocol streams and statuslines remain
untouched; a separate host-supported channel is required for their live relay.
Protected paths lacking output-volume capture retain unavailable volumes rather
than fabricate counts.

The active prompt instructs an agent to copy the exact line from the same tool-call
result once, skipping an invocation already relayed by the host. A global latest
record is unsafe under concurrency and must never be used. The installer provides
no native automatic chat transport; mock-wrapper tests prove prompt delivery and
stream/exit preservation, not actual model adherence or live-host duplicate
suppression. Chat relay remains a host-supported, separately verifiable boundary.

## Testing and gates

- Unit tests live next to the code in each crate. `pixel-daemon` tests build
  small git fixtures in a temp dir and call `Service::handle` directly.
- CLI integration tests in `crates/pixel/tests/cli/` (one binary, one module per file) invoke the built binary
  through `CARGO_BIN_EXE_pixel` against a temp fixture repo.
- CI runs `cargo fmt --check`, `cargo clippy --all-targets` with warnings
  denied, `cargo nextest run --profile ci` (`.config/nextest.toml`: one
  process per test, retry once but fail on flaky, kill after 180 s) plus
  `cargo test --doc` for the workspace, then `cargo check` of the two
  reduced feature lanes (`--no-default-features`, `model2vec` only), the
  installer and gate-runner contract scripts, and separate MSRV and
  `cargo deny` jobs.
- After any change to `crates/` the project rule in `CLAUDE.md` applies:
  rebuild, reinstall the binary atomically, re-index, reinstall hooks, and
  run `pixel doctor`.

## Release gate

`pixel check-release <version|tag> [--repo <path>] [--json]`
(the `pixel-release` crate) is the first job of
`.github/workflows/release.yml` and a maintainer's last local step: it
reads `Cargo.toml`, every member's manifest, `Cargo.lock` and
`CHANGELOG.md` and reports three checks (`cli-version`, `cargo-lock`,
`changelog`), exit 1 on any failure. Pure functions over file contents;
no git, no network.

## Build provenance

`crates/pixel/build.rs` captures the commit (`-dirty` when tracked files
were modified), target triple, rustc version and build date at compile
time and `pixel --version` prints them under the version line (`pixel -V`
stays one line). Every value falls back to `unknown` rather than failing
the build. The script re-runs when `.git/HEAD`, the ref it points to, or
the index changes, so the flag follows commits without a `cargo clean`.

## Build features

`pixel-cli` defaults to `fastembed` and `model2vec`. `fastembed` needs ONNX
Runtime and cannot build for musl, so Linux release binaries are built with
`--no-default-features --features model2vec`. `--no-default-features` alone
gives an offline-only binary with no semantic search.

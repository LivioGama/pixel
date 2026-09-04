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
   │  hooks + managed rule block            (pixel-install)
   ▼
 pixel binary (crates/pixel)  ── clap commands, prints text or --json
   │  Request = pixel_proto::Op   (NDJSON over a Unix socket)
   ▼
 pixel-daemon ── Service::handle(Op) -> Envelope<Value>
   │  in-process fallback when no daemon is reachable
   ├── pixel-index    trigram text index          .pixel/ shards
   ├── pixel-graph    symbols / imports / calls   .pixel/graph.db
   ├── pixel-facts    history facts + diffs       .pixel/history.db
   ├── pixel-rank     task -> ranked file list    (pure, no I/O)
   ├── pixel-context  token-budgeted rendering    (pure)
   ├── pixel-ops      guarded git mutations       .pixel/journal, snapshots
   ├── pixel-recall   transcript corpus + embeddings (machine-wide)
   └── pixel-git      the only git subprocess wrapper
```

Everything below the CLI is a library crate. Only `crates/pixel` builds a
binary, and Pixel is deliberately a CLI plus hooks, not an MCP server.

## Crates

| Crate | Role | Depends on (pixel crates) |
| --- | --- | --- |
| `pixel` (bin `pixel-cli`) | Command-line surface. Parses argv with clap, talks to the daemon or runs the service in-process, prints text or JSON. Also hosts the hook entry points (`hook guard`, `hook session-start`, `hook prompt-submit`, `hook post-compaction`), `rescue`, `recall`, and `sniper` sub-commands. | every library crate |
| `pixel-proto` | The shared contract crate: `Envelope`, `PixelError` and `ErrorCode`, `Epistemics`, `SnapshotInfo`, `Budget`, `Warning`, and the `Op` request enum. No I/O, no business logic. Every other crate that speaks the wire format depends on it, and it depends on nothing internal. | none |
| `pixel-daemon` | Transport-agnostic `Service` (`api.rs`) and the Unix-socket NDJSON daemon with filesystem watching (`daemon.rs`). Dispatches each `Op` to the right library, attaches snapshot and epistemics metadata, and is the one place a retrieval envelope is built. Also hosts the recall daemon service. | index, graph, context, rank, proto, ops, facts, session, recall, git |
| `pixel-index` | Sparse n-gram (trigram) text index: gram extraction, window weighting, posting-list algebra, git-anchored base and delta shards, working-tree overlay, query planner, verification, and the `gitsync` helpers that read HEAD, branch, and porcelain status. | git |
| `pixel-graph` | Code graph: tree-sitter extraction of symbols, imports, and call sites per file; import resolution; tiered call resolution with an epistemic envelope; and the analyses `impact`, `trace`, `process`, `cluster`, `changes`, `targets`. `store` owns the SQLite schema. | git, index |
| `pixel-facts` | History-wide fact and diff ingest, search, lifecycle, and rescue discovery. Owns `history.db` plus trigram history segments, with a low-priority ingest thread that never blocks queries. Backs `excavate`, `lifecycle`, `history-search`, `resolve`. | git, index, proto |
| `pixel-rank` | Pure fusion core for `targets` and ranked `search`: task text and signal inputs in, closed prioritized P0/P1/P2 file list out. | graph, git, session |
| `pixel-context` | Semantic compression of code-context items: layered renderings that fit a token budget instead of raw source dumps. | none |
| `pixel-ops` | Safe git mutation infrastructure ported from usable-git: snapshot store, repository lock, operation journal, recovery keys. Implements `inspect`, `review`, `history`, `diff`, `publish`, `push`, `ship`, `branch`, `update`, `sync`, `reconcile`, `rewrite`, `provenance`, `branches`, `env`. | git, proto |
| `pixel-git` | The single git subprocess wrapper for the workspace. Replaced three earlier ad-hoc wrappers. Any crate that shells out to git goes through here. | none |
| `pixel-recall` | Machine-wide LLM transcript retrieval: ingests Claude Code, Codex, opencode, Devin, Cursor, zcode, and Gemini transcript stores into one SQLite corpus, then serves lexical and semantic search. Owns the embedding backends (`fastembed` ONNX and pure-Rust `model2vec`, both behind features). | index, rank |
| `pixel-session` | One-look error capture: every error from every layer lands at throw time in one structured local SQLite sink, queryable in one call. | none |
| `pixel-actionlog` | Append-only JSONL record of what Pixel itself did per invocation (command, outcome, error, duration) for later self-assessment (`pixel log`, `pixel savings`). | none |
| `pixel-flow` | Deterministic browser and configuration flow replay: save, get, list, revise, replay, delete proven agent-browser paths. Flows live under `~/.local/share/pixel/flows/`. | none |
| `pixel-install` | Idempotent `pixel install`, `pixel uninstall`, `pixel doctor`: detects installed agent CLIs, wires passive hooks, writes managed rule blocks with markers, backs up touched files. | proto, daemon, index, facts |
| `pixel-bench` | Criterion benches and a real-source corpus builder (gram extraction, latency, NDCG relevance). Not shipped. | index, daemon, proto, recall |

Dependency rule: `pixel-proto` and `pixel-git` are leaves. `pixel-daemon` is
the integration point and is the only library crate allowed to depend on
almost everything. The CLI depends on the daemon plus whatever it needs for
commands that never touch the daemon (install, flow, actionlog, rescue).

## On-disk state

Per repository, under `.pixel/` (git-ignored):

| Path | Owner | Contents |
| --- | --- | --- |
| trigram shards, `build.lock`, `.index.lock`, `meta.json`, `state.json` | `pixel-index` | Base and delta shards anchored to a git commit, plus a working-tree overlay. `state.json` is the delta-layer sidecar. First process to hold `build.lock` builds; others wait. |
| `graph.db` | `pixel-graph` | SQLite: files, symbols, edges with resolution tier. Built lazily on first graph command. |
| `history.db` | `pixel-facts` | SQLite: commit facts, diff text, lifecycle. Populated by `pixel index --history` or the daemon ingest thread. |
| `targets.json` | CLI `targets` | Active task map (version 2): tasks with ids, timestamps, and P0/P1/P2 paths. Read by the guard hook and re-injected after compaction. |
| `actions.jsonl` | `pixel-actionlog` | One line per invocation. |
| `journal.jsonl`, snapshots, `reconcile-conflict.json`, `owner.json` | `pixel-ops` | Operation journal and crash-safety state for guarded git mutations. |
| `calls.json` | CLI | Circuit breaker counters for repeated identical calls. |

The prompt-submit hook writes task boundary events to
`~/.pixel/inbox/task-boundary.json`, outside the repository.

Machine-wide:

- `~/.local/share/pixel/flows/`: saved flows (`pixel-flow`).
- Recall corpus and embedding models: `pixel-recall`, downloaded once from
  Hugging Face on `pixel recall setup`.
- Daemon socket and pid: `$TMPDIR` on macOS or the runtime dir on Linux, named
  `pixel-<xxh3 of canonical repo path>.sock` and `.pid`.

## Daemon and wire contract

`pixel-daemon` exposes one function that matters: `Service::handle(Op) ->
Envelope<Value>`. The Unix-socket daemon reads one JSON `Op` per line and
writes one JSON `Envelope` line back. Request handling is single-threaded; an
accept thread and a `notify` watcher feed one channel. The watcher debounces
filesystem events and refreshes the index and graph for changed files. The
daemon exits after thirty minutes idle.

Two version numbers exist and must not be conflated:

- `pixel_proto::ENVELOPE_PROTOCOL_VERSION`: the envelope schema (`protocol`
  field).
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
  "snapshot":   { "head": "…", "branch": "…", "dirty": [] },
  "epistemics": { "closed_world": false, "lower_bound": true, "staleness_ms": 0, "basis": "…" },
  "budget":     { "byteCap": 262144 },
  "result":     { … },        // present when ok
  "error":      { "code": "NOT_FOUND", "message": "…" },   // present when !ok
  "warnings":   []
}
```

Invariants enforced by `Service::handle`:

- Success carries `result`, failure carries `error`. Never both.
- Every retrieval op (`search`, `resolve`, `targets`, `impact`, `uses`,
  `trace`, `changes`, `context`, `symbol`, `processes`, `clusters`) gets an
  `epistemics` object. Ops that hit a cap name it in `basis` and mirror it as
  a warning. Ops that attested nothing get a conservative not-closed-world
  default instead of an implied claim of completeness.
- Retrieval ops and git-state ops (`inspect`, `review`, `diff`, `status`,
  `changes`) get a `snapshot` so the caller can correlate the answer with the
  working tree it was computed against.

Adding an op is one variant on `pixel_proto::Op` plus one arm in
`Service::dispatch`. `Op::op_name` must match the serde tag, and a unit test
in `pixel-proto` checks it.

## Request path from the CLI

1. `main.rs` parses argv with clap. Commands that need the repository call
   `execute(path, Op, no_daemon)`.
2. `execute` discovers the repo root, then tries the daemon: connect to the
   socket, ping with a short timeout, and send the op. If the socket is
   absent it spawns `pixel daemon start --foreground` in the background and
   retries once. `PIXEL_DAEMON_AUTO_START=0` disables that.
3. If the daemon path fails, the CLI opens `Service` in-process and calls
   `handle` directly. Both paths return the same `Envelope`.
4. `unwrap_response` turns a failure envelope into an `Err(message)` that
   `main` prints to stderr with exit code 1. For a success envelope it takes
   `result` and folds `epistemics`, `snapshot`, and `warnings` into it
   without clobbering same-named keys the op emitted.
5. `print_data` serializes the result. With `--json` it is compact on one
   line, otherwise pretty. A global 256 KB cap protects the agent's context
   window. Human notes such as graph-build announcements and lower-bound
   caveats go to stderr, never stdout.

So the CLI's `--json` output is the envelope's `result` with the honesty
fields merged in, not the raw envelope. Anything that needs the full
envelope talks to the daemon socket directly.

## Indexes and freshness

- The text index is git-anchored: base shards correspond to a commit, delta
  shards to changes since, and an overlay covers the dirty working tree.
  `pixel status` reports whether each layer is fresh.
- The graph is built lazily on the first graph command and updated per file
  by the daemon watcher. Call edges carry a resolution tier, and analyses
  report a lower bound when same-name call sites stay unresolved.
- History facts are ingested by a dedicated low-priority thread. Queries
  never wait on ingest; they answer from what is already in `history.db` and
  say so through epistemics.

## Agent integration

`pixel install` detects installed agent CLIs and, for each supported one,
adds a managed rule block (between markers, backed up first) and passive
hooks:

| Hook event | Command | Effect |
| --- | --- | --- |
| `SessionStart` | `pixel hook session-start` | Emits the capability block from the op registry. |
| `UserPromptSubmit` | `pixel hook prompt-submit` | Detects task boundaries from the prompt and recent context. |
| `PostCompaction` | `pixel hook post-compaction` | Re-injects the active `targets.json` as additional context. |
| `PreToolUse` | `pixel hook guard` | Targets enforcement and rewire-first steering of ordinary commands toward Pixel operations. Advisory by default, not blocking. |

`pixel doctor` verifies the result, including a parity check that every
command documented in the installed rule text parses against the real clap
definition.

## Testing and gates

- Unit tests live next to the code in each crate. `pixel-daemon` tests build
  small git fixtures in a temp dir and call `Service::handle` directly.
- CLI integration tests in `crates/pixel/tests/` invoke the built binary
  through `CARGO_BIN_EXE_pixel` against a temp fixture repo.
- CI runs `cargo fmt --check`, `cargo clippy --all-targets` with warnings
  denied, and `cargo test` for the workspace, with the same feature set as
  the release build.
- After any change to `crates/` the project rule in `CLAUDE.md` applies:
  rebuild, reinstall the binary atomically, re-index, reinstall hooks, and
  run `pixel doctor`.

## Build features

`pixel-cli` defaults to `fastembed` and `model2vec`. `fastembed` needs ONNX
Runtime and cannot build for musl, so Linux release binaries are built with
`--no-default-features --features model2vec`. `--no-default-features` alone
gives an offline-only binary with no semantic search.

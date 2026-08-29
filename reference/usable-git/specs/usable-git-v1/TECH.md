# usable-git v1 Technical Specification

## Context

The approved product behavior is defined in [PRODUCT.md](./PRODUCT.md). At baseline commit [`207efde`](https://github.com/LivioGama/usable-git/tree/207efde475b8cdac2ed5523869c2529e70b73d6f), tracked source contains only the README and prose rule; it exposes no semantic operation.

An untracked Bun/TypeScript `git-mine` prototype exists in `bin/git-mine.ts`, `src/`, and `tests/`. It parses Claude Code, Codex, Cursor, Devin, and OpenCode logs into SQLite and derives shell-Git episode trends. Preserve that working baseline while relocating it; it observes shell behavior but is not a repository service.

The v1 architecture is a Bun workspace with one typed operation core, two transports, and a telemetry companion:

```text
MCP stdio ─┐
           ├─ v1 schemas ─ operation service ─ guarded Git runner ─ repository
JSON CLI ──┘                    │
                               └─ opt-in redacted event sink ─ git-mine
```

Use Git CLI as the only repository backend in v1. Pin `@modelcontextprotocol/sdk@1.29.0` and `zod@4.4.3` in `bun.lock`. Direct `.git` object writes and embedded Git libraries remain out of scope.

## Proposed changes

### Workspace and ownership

- Convert the root into a private Bun workspace containing `packages/usable-git` and `packages/git-mine`; keep shared TypeScript settings at root.
- Relocate the current mining source and tests into `packages/git-mine` without behavior deletion. Complete relocation only after its existing 25-test baseline passes from the new path.
- Add `benchmarks/` for deterministic fixtures, paired agent scenarios, raw JSON results, and machine-readable environment manifests.
- Add an MIT license and ship only the `usable-git` runtime through Homebrew; do not publish an npm package.

### Versioned contracts

Create `packages/usable-git/src/contracts/v1/` as the only source of truth for request, result, envelope, error, cursor, repository-state, and telemetry-event schemas. Infer TypeScript types from Zod schemas; do not maintain parallel handwritten wire types.

The shared envelope is a discriminated union on `ok` carrying only agent-relevant bytes:

```ts
type OperationEnvelope<TResult> =
  | { ok: true; requestId?: string; warnings?: Warning[]; result: TResult }
  | { ok: false; requestId?: string; warnings?: Warning[]; error: OperationError };
```

Enforce exactly one of `result` or `error`; `ok` must agree with that branch. Version, operation name, backend, transport, duration, subprocess counts, and repository identity are deliberately absent from the wire; the telemetry event schema still records operation, duration, and subprocess counts locally. Define stable error codes named after PRODUCT invariants 4 and 8, including `REF_EXISTS` and a scoped `GIT_FAILED` (18 codes total). Carry sanitized Git exit status and a bounded diagnostic string when useful, but never expose environment variables or secrets.

Request contracts:

- `inspect`: absolute `repoPath`; optional unique literal `files`.
- `review`: `repoPath`; optional `files`; optional opaque `cursor`; `byteCap` with a conservative default and hard maximum defined in the schema.
- `history`: `repoPath`; `ref` default `HEAD`; `limit` default 20/max 100; `detail` default `compact` (`full` restores the forensic commit shape); optional opaque `cursor`.
- `diff`: `repoPath`; `target` union — `{kind: "range", baseOid, targetOid}` or `{kind: "commit", oid}` with exact object IDs of at least 12 hex, never ref names or revision expressions; optional `files`, `cursor`, `byteCap`.
- `search`: `repoPath`; `target` union — `{kind: "text", query (1–512 chars), scope: "message"|"path"|"diff"|"all" default "all"}` or `{kind: "lifecycle", path?|token?}` with exactly one of `path`/`token`; `limit` default 10/max 50; `byteCap` default 32,000; optional opaque `cursor` (text targets only).
- `publish`: `repoPath`; non-empty unique `files`; optional `message` (required for append mode via schema refinement); `mode` union (`append` default, `amend`); optional bounded `requestId`; exactly one of a 12-hex `snapshot` token or an explicit `expected` block (`head` union `oid`/`unborn` plus a fingerprint for every file).
- `push`: `repoPath`; configured `remote`; full `sourceRef` and `targetRef` under `refs/heads/`; optional `requestId`; expected source OID; `fast-forward` or `force-with-lease`, with exact expected target OID required for lease mode.
- `ship`: the publish shape plus `remote`, optional `targetRef`, and optional push `mode` defaulting to fast-forward; `sourceRef` and expected source OID are derived server-side from the fresh commit.
- `branch`: `repoPath`; optional `requestId`; `expectedHead`; `mode` union `create`/`switch`, each with a short branch name validated by Git ref rules.
- `sync`: `repoPath`; configured `remote`; optional list of up to 16 short branch names defaulting to the current branch/upstream.
- `update`: `repoPath`; optional `requestId`; `expectedHead` as an exact OID; exact `targetOid`.

When `requestId` is omitted on a mutating request, the service generates `auto-<12 hex>` and echoes it in the envelope so retries stay idempotent. Push's `NETWORK_AMBIGUITY` error details carry the effective request ID for the same reason.

Pagination cursors are short server-held handles matching `c_<10 hex>`. The handle names a durably written record under the state directory's `cursors/` folder containing operation, normalized-request digest, repository snapshot fingerprint, offset, and a corruption checksum; retention is bounded to 24 hours and 500 records. Validate every decoded field as untrusted input: malformed or cross-operation handles fail as invalid input, and unknown or expired handles fail with `STALE_STATE` telling the caller to restart pagination. The handle contains no path or content data and remains usable across separate CLI processes.

Inspect snapshot tokens follow the same server-held pattern: `inspect` fingerprints every change, persists one record per worktree root under the state directory's `snapshots/` folder (`$XDG_STATE_HOME/usable-git`, falling back to `~/.local/state/usable-git`; 24-hour/200-record retention), and returns a 12-hex token derived from root, HEAD, and sorted fingerprints. `publish` and `ship` resolve the token back to its recorded expectations, so fingerprints never transit the wire or the agent's context. An unknown or expired token fails with `STALE_STATE` before mutation.

### Guarded Git runner and parsers

Implement a single `GitRunner` that accepts argv arrays only and never invokes a shell. It must:

- Preserve `HOME` and normal system/global/local Git configuration so identity, signing, credentials, and hooks behave canonically. Set `GIT_TERMINAL_PROMPT=0`, `GIT_PAGER=cat`, and `PAGER=cat`; use explicit `--no-optional-locks` for reads.
- Remove inherited repository/config overrides including `GIT_DIR`, `GIT_WORK_TREE`, `GIT_INDEX_FILE`, object-directory overrides, external-diff variables, and `GIT_CONFIG_COUNT`/`GIT_CONFIG_KEY_*`/`GIT_CONFIG_VALUE_*` before setting operation-owned values.
- Disable color, external diff, and text conversion for machine-parsed reads; prevent lazy object fetching.
- Set locale to a deterministic value for parsed diagnostics while relying on machine formats for data.
- Capture stdout/stderr as bytes with operation-specific limits, count every Git subprocess, support cancellation/timeouts, and redact credentials from diagnostics.
- Never print protocol diagnostics to stdout; MCP stdout is JSON-RPC only.

Repository discovery resolves top-level path, Git directory, and common directory, then captures repository capabilities and in-progress state. Canonicalize the requested root once and validate every selected path against it.

Literal path validation must reject empty values, absolute paths, `.`, directories, duplicates, globs, pathspec magic, and `..` escapes. Publish additionally rejects ignored paths and gitlinks; read operations may classify them. Pass validated paths through NUL-delimited `--pathspec-from-file=- --pathspec-file-nul` with top-level `git --literal-pathspecs` where the subcommand supports it; otherwise pass argv after validation with `--`.

Dedicated parsers own Git's stable machine outputs:

- Porcelain v2 `-z` parser for branch/upstream and staged/unstaged/untracked/conflicted/rename state.
- Raw/name-status/numstat diff parsers for staged and unstaged evidence, including binary entries and unusual filenames.
- NUL/record-delimited log parser for OIDs, parents, identities, messages, timestamps, and signature status.
- `ls-files`, `check-ignore`, `rev-parse`, and `for-each-ref` parsers for validation and ref resolution.

Parsers accept bytes and return typed data. No operation module parses human-oriented output inline.

### Read operations

Implement operations in dependency order: `inspect`, `review`, `history`, `diff`, then `search`.

- `inspect` takes one coherent local snapshot and derives per-change fingerprints from normalized state plus selected content/index identities. The wire result is compact — root, branch, HEAD, optional upstream/state/stashes/remotes, and per-change `{path, status, from?}` entries using the porcelain v2 `XY` vocabulary — while the fingerprints go into the snapshot store behind the returned token. A file-scoped inspect re-fingerprints the full status before recording so a scoped view never masquerades as a whole-repository snapshot. It reports unborn HEAD without error and never contacts a remote.
- `review` independently obtains `HEAD→index` and `index→worktree` evidence. Read explicit untracked files directly only after path validation. Build deterministic pages by canonical path/order and byte accounting; bind cursor to the inspect snapshot.
- `history` validates a local ref/object without fetch, reads newest-first records, and paginates against a bound start OID. Compact commits carry 12-hex OID, subject, author name, timestamp, and a `merge` marker; `detail: "full"` restores parents, committer, full message, and signature status. The result no longer reports a byte count. Empty unborn history is successful.
- `diff` resolves both exact OIDs locally (first parent or empty tree for `{kind: "commit"}`), reuses the review patch pipeline without the staged/unstaged scope, and shares review's byte-cap and cursor behavior. It never fetches missing objects.
- `search` queries a lazily built local index instead of walking history per call. The index is a SQLite FTS5 database (`bun:sqlite`) under `<stateRoot>/search/<sha256(commonDir)>/index-v1.sqlite` (0o700 dir / 0o600 file, WAL, `user_version` guarded, self-healing: corruption or version mismatch deletes and rebuilds silently). Ingestion runs inside each call under a fixed budget (default 8,000 ms, injectable) with a dedicated 8 MiB-output git runner: Phase A batches `git log -z --no-walk=unsorted --name-status` for metadata/messages/paths (merge commits recorded as skipped), Phase B batches `git show -U0 --diff-filter=AMDRT --find-renames` for diff text with per-file (32 KiB) and per-commit (256 KiB) caps, halving batches on runner-cap overflow; binary files, lockfiles, and generated paths are recorded as skipped. A Phase B `git show` failure isolates the same way — halve, retry — and a single commit whose diff cannot be produced (a gc-pruned OID kept as post-rebase forensic evidence, or a diff over the runner output cap) is marked `diff_state=skipped` with `skip_note` `"unresolvable"` or `"over-cap"`: its metadata and message stay searchable, the skip is reported in `skippedDiffCommits`, and the index can never wedge on it. Commit inserts are `INSERT OR IGNORE` by OID so re-ingestion after rebase is idempotent and orphaned commits remain as forensic evidence. Queries sanitize user text into quoted FTS units (implicit AND, OR fallback), rank per-scope bm25 orderings weighted message > path > diff-add > diff-del with a recency epsilon, and return dual-bounded pages whose cursors bind to the sorted indexed tips plus the ranking pass. Lifecycle answers derive from `file_changes`/`diffs_fts` with `presentAtHead` verified via `git cat-file -e` or `git grep --fixed-strings` against HEAD. Remaining work is surfaced as `index: {state: "partial", …counts}`, never as an error.

Expose read tools with accurate MCP annotations: read-only, idempotent, and closed-world. Annotations are metadata, not authorization; client policy still controls writes.

### Mutation safety foundation

Before any mutation (`publish`, `push`, `ship`, `branch`, `update`), add:

- A lock file keyed by canonical Git common-dir, using atomic exclusive creation and bounded stale-lock diagnosis. Do not automatically steal a live lock.
- An external journal at `$XDG_STATE_HOME/usable-git`, falling back to `~/.local/state/usable-git`, keyed by repository hash and request ID. Never journal inside the target working tree.
- Request-id records containing normalized request hash, phase, observed pre-state, owned intermediate checksums, and terminal result. Reuse with a different request body is an error.
- Recovery that runs before a new mutation. Each phase resolves to confirmed success, safe rollback, known failure, explicit ambiguous outcome, or `recovery_conflict`.

Journal writes use write-to-temp, fsync, atomic rename, and parent-directory sync where supported. Keep completed records long enough to provide idempotency; bound retention by age and count without deleting active/ambiguous records. The journaled operations are `publish`, `push`, `branch`, and `update`. `sync` deliberately takes neither the mutation lock nor a journal entry: fetching into `refs/remotes/` is idempotent and touches nothing the lock protects, and holding the lock through a slow network fetch would block `publish` for zero added protection.

### Publish

Implement `publish` with canonical Git exact-path commit behavior, not direct object writes:

1. Validate the request, resolve expectations (snapshot token → stored record, or the explicit `expected` block), and check repository capabilities, the HEAD expectation, and every selected fingerprint.
2. Acquire common-dir lock and revalidate all expectations.
3. Snapshot exact index bytes, index metadata/checksum, HEAD, branch ref, and full pre-operation status fingerprint into the journal.
4. For selected untracked files, make them known with intent-to-add while preserving the original index snapshot.
5. Invoke `git commit --only` with literal selected paths and the supplied message so the commit tree contains complete current selected contents but excludes unrelated index entries. Preserve hooks, signing, identity, and repository configuration.
6. Observe HEAD immediately after Git returns. If changed, mark commit observed and never reset HEAD.
7. If no commit was observed, restore exact original index bytes only when the current index checksum matches the service-owned intermediate checksum. Otherwise return `recovery_conflict` without overwrite.
8. Verify resulting commit tree, selected paths, and unrelated status/index preservation; persist terminal result and release lock. Integration/property tests run `git fsck --strict` on the completed fixture.

Unborn HEAD follows the same path but uses the explicit `unborn` expectation. Add integration fixtures to prove unrelated pre-staged files remain staged and absent from the initial commit.

Amend mode (`mode: {kind: "amend"}`) shares the same lock, journal, and index-restore machinery but invokes `git commit --amend --only` against the existing tip. It requires an existing commit, reuses the tip's message when `message` is omitted, preserves parents, reports the replaced OID as `amendedOid`, and warns when the amended tip already exists on the configured upstream because the following push will need a lease.

### Push

Implement one explicit ref update per request:

1. Validate configured remote name and full branch refs; reject URLs, tags, deletions, wildcard/multiple refspecs, and implicit upstreams.
2. Acquire lock, resolve source, compare expected source OID, and record explicit target expectation.
3. Fast-forward mode pushes one `sourceRef:targetRef` without force. Lease mode adds exactly `--force-with-lease=<targetRef>:<expectedTargetOid>`; never use blind `--force` or an empty lease.
4. Record subprocess start and completion phases. On uncertain transport failure, query only the configured remote's explicit target ref with `ls-remote`.
5. Compare remote target to expected old/new OIDs and return confirmed success, confirmed failure, or `network_ambiguous`. Never retry the update automatically.

Mark the MCP push tool destructive and open-world. Apply explicit write approval/session policy during each client installation rather than relying on annotations.

### Ship, branch, sync, and update

- `ship` composes the existing publish and push implementations in one operation. The publish leg runs first with the request's snapshot/expected state; the push leg derives `sourceRef` and the expected source OID from the observed fresh commit. The partial-failure contract is asymmetric by design: a publish-leg failure returns `ok: false`, while a push-leg failure after a successful commit returns `ok: true` with `result.push.ok: false`, the push error code, and retry guidance — a commit that exists is never reported as a top-level error.
- `branch` validates the short name against Git ref-component rules, checks `expectedHead`, and either creates at the current HEAD and switches (allowed from detached HEAD; an existing name is `REF_EXISTS`) or switches to an existing branch only when no uncommitted tracked change would be carried (`UNSUPPORTED_STATE` with `dirtyPaths` otherwise). It holds the mutation lock and journals like push.
- `sync` builds explicit refspecs `+refs/heads/<branch>:refs/remotes/<remote>/<branch>` for each named branch (default: current branch/upstream), fetches with `--no-tags`, and never prunes. Only `refs/remotes/` is written. A branch missing on the remote is a success with `newOid: null`; a fetch failure is a retryable `GIT_FAILED`, never `NETWORK_AMBIGUITY`, which stays reserved for push. No lock, no journal (see the mutation safety section for why).
- `update` is the local lease mirror of push: it verifies the current branch head equals `expectedHead` (`STALE_STATE` otherwise), verifies `targetOid` is a descendant (`NON_FAST_FORWARD` with the merge base otherwise — divergence is resolved outside usable-git), and pre-checks the incoming file set against dirty working-tree paths (`UNSUPPORTED_STATE` with `conflictingPaths` on overlap) before fast-forwarding. It holds the mutation lock and journals like push.

### MCP, CLI, installer, and doctor

Expose one stdio MCP server with exactly eleven tools. Register each tool with its input schema, `outputSchema`, accurate annotations, and both `structuredContent` and a text content block. The text block carries the full envelope JSON by default so clients that read only `content` still receive complete results; `USABLE_GIT_MCP_TEXT=summary` restores the old one-line summary. Validate outgoing structured results before sending them.

Expose the same service through:

```text
usable-git inspect|review|history|diff|publish|push|ship|branch|sync|update --json [flags]
usable-git <operation> --input -
usable-git mcp
usable-git install --clients all [--force]
usable-git doctor --clients all
```

`--input -` reads one JSON request from stdin. JSON mode writes one envelope to stdout and diagnostics to stderr. Exit 0 only for `ok: true`; map all operation failures to a stable non-zero exit without changing the JSON error contract.

Installer behavior:

- Codex, Claude Code, and Devin: use each installed client's native MCP registration command non-interactively.
- Cursor: atomically merge the MCP entry into its JSON configuration because its CLI has no `mcp add` command.
- Preserve unrelated configuration byte-for-byte where the native client permits and structurally where JSON merge is required. Matching entries are idempotent; conflicts require `--force`.
- Register an absolute executable path and local stdio transport. Never embed secrets.

Doctor uses isolated temporary repositories and a local bare remote. It checks runtime versions, direct JSON CLI operations, raw MCP initialize/list/call and exact schemas, dirty-tree publish preservation, single-ref push, all client registrations, and a fresh-session semantic invocation from each requested client. Emit a structured pass/fail/skip report and fail if a required check fails.

### Routing rule and distribution

Replace the prose rule with a thin router: use semantic MCP when applicable, JSON CLI when MCP is unavailable, and exact-path raw Git only when the requested capability is outside v1. A semantic safety rejection is terminal; the rule must not bypass it with raw Git.

Install the rule only through the canonical `~/.agent-config/rules/usable-git.md`, then run `~/agent-config/build.sh` and verify generated client files. Never edit generated global agent files directly.

Publish through `LivioGama/homebrew-tap` as `Formula/usable-git.rb`, depending on Homebrew `bun` and `git`. Release automation updates version and SHA, runs `brew audit --strict`, and executes a formula test covering MCP handshake, dirty-tree publish, and local-bare-remote push on clean macOS and Linux environments.

### Telemetry and git-mine

Telemetry is disabled unless explicitly enabled. Emit one versioned event at the operation boundary containing only fields allowed by PRODUCT invariants 81–83; the event still records operation, duration, and subprocess counts even though the wire envelope no longer carries them, and its operation enum covers all ten operations. Salt and hash the canonical repository identity locally; never write raw paths or file identifiers.

Extend `git-mine` to ingest semantic MCP/CLI events alongside legacy shell episodes and report:

- Applicable semantic invocations versus raw fallbacks.
- Repeated-read elimination.
- Correctness/recovery outcomes.
- Agent-facing operation count.
- Git-related token estimate and end-to-end latency.
- Client, transport, backend, and version distribution.

Legacy migration creates a separate redacted database. Preserve the old database unless the user explicitly removes it.

## Testing and validation

- Contract tests validate every request/result/error schema, envelope branch, cursor rule, telemetry whitelist, and MCP annotation (PRODUCT 1–5, 81–83).
- Parser fixtures cover NUL-delimited porcelain v2, renames, conflicts, binary files, Unicode and newline filenames, SHA-1/SHA-256 OIDs, worktrees, and truncated/malformed output (PRODUCT 11–33).
- Read integration tests compare `inspect`, `review`, `history`, and `diff` results against canonical Git in clean, dirty, staged, untracked, conflicted, unborn, and paginated repositories. Assert zero mutation and network access (PRODUCT 6, 11–33).
- Publish differential tests compare HEAD, commit tree, exact index bytes, status, and unrelated file contents before/after; include add/modify/delete/rename, nested files, initial commits, amend, hooks, signing failure, missing identity, contention, stale expectations (including expired snapshot tokens), and every refused repository state (PRODUCT 7–10, 34–45).
- Push tests use local bare remotes and cover fast-forward, non-fast-forward, exact lease success/rejection, stale source, invalid refs/remotes, authentication classification fixtures, and injected ambiguous outcomes (PRODUCT 46–53). Ship tests additionally prove the partial-failure contract; branch, sync, and update tests cover ref-exists, dirty-switch refusal, absent remote branches, retryable fetch failure, divergence, and dirty-path overlap (PRODUCT 54–74).
- Crash injection executes every journal phase and proves recovery ends in confirmed success, safe rollback, known failure, or explicit ambiguity/conflict; never silent loss.
- Property testing creates at least 1,000 seeded randomized dirty repositories. Required oracle: zero unrelated paths staged, unstaged, committed, edited, deleted, or lost.
- Every successful mutation fixture verifies `git status --porcelain=v1`, readable `git log`, expected refs/tree/index, and `git fsck --strict`.
- Installer/doctor tests start from clean configs for all four clients, preserve sentinel unrelated entries, test conflict/force behavior, and perform fresh-session invocation (PRODUCT 75–80).
- Homebrew tests run on clean macOS and Linux and execute the real formula-installed MCP/publish/push paths.
- Paired agent benchmarks run at least 30 trials per scenario per client and record raw JSON, trial seed, hardware/OS, Bun/Git/client versions, commit SHA, median, p95, confidence intervals, subprocess counts, agent operations, token measurements, and final-state oracles.
- Release requires every gate in PRODUCT invariant 86. The benchmark gate is the local three-client policy: roll out trials Codex → Claude Code → Devin CLI with 40 paired trials per scenario/client; do not tag v1 until all three pass. Cursor Agent registration remains supported but is not a v1 gate.

Long randomized, cross-client, and Homebrew matrices run remotely after syncing the exact commit; quick module and fixture tests run locally. Do not use the build/dev-server commands prohibited by repository agent rules.

## Risks and mitigations

- **Git state corruption:** exact index snapshots, common-dir locks, phase journals, checksum-guarded restore, differential tests, crash injection, and `fsck` verification.
- **Hooks mutate repository state:** re-observe HEAD/index/status after commit and never overwrite an index whose checksum is no longer service-owned.
- **Ambiguous network success:** query only the explicit target ref and return ambiguity instead of retrying.
- **Path injection or scope widening:** reject non-literal paths and use argv plus NUL-delimited literal pathspecs.
- **Protocol corruption or secret leakage:** reserve stdout for protocol/JSON, bound diagnostics, redact secrets, and schema-test telemetry.
- **Client registration drift:** native registration where available, atomic Cursor merge, exact doctor schema checks, and fresh-session invocation.
- **Misleading performance claims:** retain historical numbers only as unverified context; publish new claims only from committed reproducible artifacts.

## Parallelization

Use parallel agents because the implementation separates cleanly after contracts land:

- **Core/read agent (local shared checkout `/Users/livio/Documents/usable-git`):** schemas, runner, discovery, parsers, `inspect`, `review`, `history`, and their tests.
- **Mutation agent (same shared checkout, disjoint `packages/usable-git/src/mutations` ownership):** begin after schemas/runner interfaces are frozen; locks, journals, `publish`, `push`, recovery, property/crash tests.
- **Telemetry/distribution agent (same shared checkout, disjoint `packages/git-mine`, installer/doctor, and delivery-file ownership):** `git-mine` relocation/extension, installer, doctor, Homebrew formula/release workflow, and client fixtures.

Subagents do not commit from the shared checkout. The parent stages explicit owned paths and lands one combined PR with linear commits; no merge commits. Agents own disjoint modules and exchange only frozen contract fixtures. The parent owns integration, routing-rule deployment, full matrix verification, and release-gate evidence. Sequence: contracts → parallel core/mutations/delivery → integration → remote matrices → release.

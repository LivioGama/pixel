# usable-git implementation handoff

## Post-redesign note (read first)

The wire-surface redesign has landed on `main` after this handoff was written
(208 tests pass). In brief:

1. **Envelope diet** — the wire envelope is now `{ok, requestId?, warnings?}` plus
   exactly one of `result`/`error`; version, operation, backend, transport, duration,
   subprocess counts, and repository metadata left the wire (telemetry still records
   them locally).
2. **Ten operations** — `inspect`, `review`, `history`, `diff`, `publish`, `push`,
   `ship`, `branch`, `sync`, `update` (was five).
3. **Snapshot tokens** — `inspect` returns a 12-hex `snapshot` token; `publish`/`ship`
   accept it instead of transcribed fingerprints. Publish gained `mode: append|amend`
   and an optional message (amend reuses the tip message).
4. **Compact reads** — compact `history` commits by default (`detail: "full"` restores
   the forensic shape); compact `inspect` changes using porcelain `XY` status.
5. **Short cursors** — server-held `c_<10hex>` handles replace long base64 cursor blobs.
6. **Optional request IDs** — auto-generated `auto-<12hex>` when omitted, echoed in the
   envelope; push ambiguity details carry the request ID for idempotent retry.
7. **New error code** — `REF_EXISTS` (18 codes total).
8. **MCP text content** — the text block now carries the full envelope JSON
   (`USABLE_GIT_MCP_TEXT=summary` restores the one-line summary), fixing clients that
   read only `content`.

The July benchmark artifact referenced below **predates all of this** and is stale.
The re-run required by steps 2–4 must use the new surface; scenarios may now use the
`ship`/snapshot-token flows. The existing steps below remain valid otherwise.

## Status

Implementation is complete through the v1 release-candidate stage. The ten semantic
operations, CLI/MCP transports, guarded mutation recovery, client installation and
diagnostics, telemetry ingestion, property/crash coverage, paired benchmark harness,
and Homebrew release tooling are present.

This is **not yet a public v1 release**. The source branch can be pushed, but no v1 tag,
GitHub release, or public Homebrew formula should be published until the real-agent
benchmark gate passes.

Implementation baseline before this handoff: `a5e72b4ed56cca527bcee92c50b5f759b2cd334a`
on `main`.

## Verified evidence

| Area | Observed result |
|---|---|
| Full remote test suite | Session-observed: 207 tests, 899 assertions, 0 failures; newer log was not persisted |
| TypeScript | `bun run typecheck` completed cleanly |
| Property coverage | 1,000 randomized cases passed |
| Crash recovery | 46 crash cases passed |
| macOS Homebrew candidate | Source archive audit, reinstall, formula test, and real MCP/publish/push paths passed |
| Linux Homebrew candidate | Source archive audit, reinstall, formula test, and real MCP/publish/push paths passed |
| Client activation | Session-observed: Codex, Claude Code, Cursor Agent, and Devin CLI doctors passed; combined result 14/14 |
| Installed executable | Stable path observed at `/opt/homebrew/bin/usable-git` |
| Source archive | SHA-256 `c4d154dc0b102bb8cc39b807a442b51211d971446090340d614a10d66303ae56` for baseline HEAD |
| Global routing rule | Deployed and pushed in `LivioGama/agent-config` commit `bd7b284e52cf70b2c31bd792c413ecf9e8979347` |
| Local non-Cursor benchmark | `benchmarks/results/usable-git-benchmark-20260714T203040Z.{json,md}`: 3 clients, 2 scenarios, 40 paired trials per client/scenario, 240 paired trials total; **not release-eligible** |

Latest local client versions recorded in the benchmark artifact:

- Codex: `0.144.4`
- Claude Code: `2.1.209`; `--model sonnet` resolved to `claude-sonnet-5`
- Devin CLI: `3000.1.27 (0d4bf12e)`

Final Linux Homebrew logs from the baseline run:

- `/tmp/usable-git-a5e72b4-reinstall.log`
- `/tmp/usable-git-a5e72b4-test.log`

The README deliberately labels historical prototype timings as unverified. Do not cite
them as v1 performance evidence.

## Remaining release work

### 1. Prepare the exact local revision

Use the same commit for every trial. Install locked dependencies with Bun. Record exact
versions for Bun, Git, Codex, Claude Code, and Devin in the benchmark
invocation/artifact.

This handoff is now scoped to the local Mac. Do not use `exodus`, and do not include
Cursor Agent in this local run.

### 2. Resolve local real-client benchmark blockers

The requested local non-Cursor matrix has been run and preserved at
`benchmarks/results/usable-git-benchmark-20260714T203040Z.{json,md}`. It is complete
for the requested local matrix but failed the release gate.

Observed blockers:

- Codex semantic sessions passed, but Codex raw-Git sessions did not emit the exact
  structured raw Git tool-call evidence the harness requires.
- Claude Code local fresh sessions failed to produce completed structured tool calls.
  The local model resolved to `claude-sonnet-5`; the stream reported the `usable-git`
  MCP server as `pending`.
- Devin fresh sessions were rate-limited and emitted no parseable structured evidence.
- The automated release gate now matches the local three-client policy: Codex,
  Claude Code, and Devin, with 40 paired trials per scenario/client.

### 3. Re-run the local real-agent paired benchmark matrix after fixes

Required matrix:

- 3 clients: Codex, Claude Code, Devin
- 2 scenarios: `inspect-dirty`, `publish-scoped`
- 40 paired trials per client/scenario
- 240 paired trials; 480 fresh client sessions total
- fixed seed: `20260714`

From the checked-out local revision:

```sh
USABLE_GIT_CLAUDE_MODEL=sonnet bun benchmarks/run.ts \
  --clients codex,claude-code,devin \
  --client-version codex=<exact-version> \
  --client-version claude-code=<exact-version> \
  --client-version devin=<exact-version> \
  --trials 40 \
  --seed 20260714 \
  --output benchmarks/results
```

Keep both generated raw JSON and Markdown report. Commit them only if the artifact contains
the complete requested local matrix and real structured evidence. Do not substitute the
short `harness` fixture for evidence.

### 4. Evaluate every release gate

All gates must pass together:

- 100% repository correctness, final-state oracle success, and recovery
- zero unrelated-work loss or corruption
- 100% clean-install activation across Codex, Claude Code, and Devin
- at least 95% semantic-tool adoption when applicable
- at least 50% fewer agent-facing Git operations
- at least 30% lower Git-related tokens
- at least 30% lower p95 end-to-end time
- measured tokens available for every required client/version
- paired artifact includes trial counts, environment/component versions, tested commit SHA,
  medians, p95 values, confidence intervals, subprocess counts, and final-state oracles

Any missing/unparseable usage data or incomplete client matrix is a release-gate failure,
not a result to estimate around.

The automated release gate is intentionally scoped to the local three-client policy.
Cursor Agent is no longer required for this v1 gate.

### 5. Publish only after a passing artifact

If every gate passes:

1. Commit the raw benchmark JSON and generated Markdown report.
2. Re-run the repository verification suite against that exact commit.
3. Generate and validate `Formula/usable-git.rb` through
   `packaging/homebrew/prepare-release.ts` and the existing Homebrew tests.
4. Update `LivioGama/homebrew-tap` only after checksum and macOS/Linux formula gates pass.
5. Create the v1 tag and GitHub release from the same verified commit.
6. Confirm clean installation and Codex, Claude Code, and Devin doctors from the public formula.

If a gate fails, keep the candidate unreleased, preserve the raw artifact, fix the measured
cause, and rerun the complete matrix. Do not publish a partial result.

## Safety and workspace notes

The following pre-existing untracked files are personal/tool configuration, not v1 source
artifacts. Preserve them locally and do not add them to the public repository:

- `.cursor/rules/use-bun-instead-of-node-vite-npm-pnpm.mdc`
- `AGENTS.md`
- `CLAUDE.md`

Use `usable-git inspect`, `review`, `history`, `diff`, `publish`, `push`, `ship`,
`branch`, `sync`, and `update` for their supported operations. A semantic safety
rejection is terminal; never bypass it with broader raw Git. Keep history linear.

Some older `/tmp` suite and standalone doctor artifacts predate the final fixes and show
stale failures or executable paths. Do not use them as final proof. The Linux Homebrew
logs named above and the exact source archive hash are tied to the baseline commit.

## Primary references

- `README.md`
- `benchmarks/README.md`
- `specs/usable-git-v1/PRODUCT.md`
- `specs/usable-git-v1/TECH.md`
- `packaging/homebrew/prepare-release.ts`

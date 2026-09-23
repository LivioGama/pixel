---
title: "Documentation"
description: "Install Pixel, wire it into your agents, and read what its answers promise and what they do not."
---

<!-- Every `pixel <command>` quoted here must exist: crates/pixel/tests/cli/docs_drift.rs reads this page. Keep it in step with README.md and ARCHITECTURE.md. -->

## Install

Pixel is a single binary for macOS and Linux. Pick one channel:

```bash
brew install LivioGama/tap/pixel
```

```bash
curl -fsSL https://github.com/LivioGama/pixel/releases/latest/download/install.sh | sh
```

The script downloads the latest release, checks its checksum and installs it into `$PIXEL_INSTALL_DIR` (default `~/.local/bin`). To build from source instead, see [CONTRIBUTING.md](https://github.com/LivioGama/pixel/blob/main/CONTRIBUTING.md).

Then let your agents use it, and check the result:

```bash
pixel install         # once, from anywhere
pixel prepare-repo .  # optional: index, graph and a warm daemon for this repository
pixel doctor .        # optional: health check
```

The index, the code graph and the optional history data live in `.pixel/` at the repository root. Nothing leaves the machine.

## What pixel install wires

`pixel install` is global: run it once, from anywhere. It deploys the agent prompt to `~/.local/share/pixel/` (`agent-prompt.md`, plus the short `subagent-prompt.md` for sub-agents) and wires it into the agents it knows:

| Agent | How the prompt reaches it |
| --- | --- |
| Claude Code | `SessionStart` and `SubagentStart` hooks in `~/.claude/settings.json` inject the prompt as context. No shell wrapper, never blocks. |
| Codex | the `developer_instructions` key of `~/.codex/config.toml`, which every Codex front end reads, plus a metrics `PostToolUse` hook |
| Pi | `~/.pi/agent/APPEND_SYSTEM.md`, read automatically |
| OpenCode | the prompt appended to `AGENTS.md` |
| Antigravity | its plugin, hooks and configuration activated |

`pixel uninstall` removes everything `pixel install` wrote, the binary included.

### Per-repository guards

`pixel install --repo <path>` writes project-local enforcement only and skips every global step: a guard hook for Claude Code (in the personal `.claude/settings.local.json`, never the shared settings), Codex, Devin and Pi. Each of those files names this machine's `pixel` binary, so the install lists them in the clone's `.git/info/exclude` and a `git add -A` cannot publish them.

The guard is advisory. It steers agents toward Pixel commands, for example with a notice before an untargeted read of a large source file, and never blocks a tool call. `pixel doctor <repo>` reports the global wiring and the per-repository guards as green, stale or missing.

## Updating

Upgrading replaces the binary only. The agent prompt and the per-agent config keys belong to you, not to the package manager, so they keep the old release's text until you refresh them.

| Installed with | Upgrade the binary |
| --- | --- |
| Homebrew | `brew update && brew upgrade LivioGama/tap/pixel` |
| mise | `mise upgrade pixel` |
| `install.sh` | run the same `curl … \| sh` line again |
| Source checkout | `pixel self-update` rebuilds and reinstalls the running binary |

Then, whatever the channel:

```bash
pixel install
pixel doctor .
```

`pixel doctor .` reports the wiring as stale until you do.

## Plugins

Each agent CLI below can load Pixel's protocol through its own plugin mechanism, with no `pixel install` step. The `pixel` binary still has to be installed: the plugin never installs it. When the binary is missing or too old for the commands the protocol names, the plugin injects a one-paragraph notice instead of the protocol.

| Tool | Install |
| --- | --- |
| Claude Code | `/plugin marketplace add LivioGama/pixel`, then `/plugin install pixel@pixel` |
| Codex | `codex plugin marketplace add LivioGama/pixel`, then `codex plugin add pixel@pixel` |
| Copilot CLI | `copilot plugin marketplace add LivioGama/pixel`, then `copilot plugin install pixel@pixel` |
| Devin | add `github.com/LivioGama/pixel` as a Devin plugin |
| Gemini CLI | `gemini extensions install https://github.com/LivioGama/pixel` |
| Pi | `pi install git:github.com/LivioGama/pixel` |
| OpenCode | `"plugin": ["@liviogama/pixel"]` in `opencode.json` |
| Cursor, Windsurf, Kiro, Cline, Qoder | rules ship under `.cursor/rules/`, `.windsurf/rules/`, `.kiro/steering/`, `.clinerules/` and `.qoder/rules/`: copy them into your project |

Any other agent: paste [`PIXEL.md`](https://github.com/LivioGama/pixel/blob/main/PIXEL.md), the plain-Markdown protocol, into whatever instruction surface it offers. [Manual setup](https://github.com/LivioGama/pixel/blob/main/docs/manual-setup.md) covers wiring the full prompt by hand.

## The workflow

The agent prompt walks every change through the same path:

```bash
pixel scope-task "<task>"        # first call on multi-file work: P0/P1/P2 targets
pixel find-code "<phrase>"       # before any free-text search for a name
pixel impact "<symbol>"          # before editing any symbol: its blast radius
pixel what-changed               # before an edit batch: what already differs
pixel review-changes             # the working tree, structured
pixel commit-and-push --files <f>... -m "msg" --request-id "id" origin HEAD
```

Two rules hold throughout. `pixel impact` runs before any edit, because editing blind is how callers you never saw break. And the agent never commits or pushes unless asked: every write takes a `--request-id`, which makes it crash-safe and idempotent.

## Commands

The most used commands, by job. `pixel --help` lists all of them, and [ARCHITECTURE.md](https://github.com/LivioGama/pixel/blob/main/ARCHITECTURE.md#command-surface) describes each in one line.

### Find code

| Instead of | Run |
| --- | --- |
| `grep`, `rg` | `pixel search-content "re" [path]`: the same regex, indexed and capped |
| grep for a function by name | `pixel find-code "name"`: a phrase resolved through the concept index |
| grep for a definition | `pixel find-symbol "Foo"`: the exact symbol, from the code graph |
| "how is auth handled?" | `pixel search-meaning "how is auth handled?"`: semantic, not regex |
| reading a whole file | `pixel list-signatures <file>` or `pixel pack-context <uid>`: the skeleton, or one symbol fitted to a budget |

### Scope and impact

| Question | Run |
| --- | --- |
| Which files does this task touch? | `pixel scope-task "task"` |
| What breaks if I change this? | `pixel impact "symbol"` |
| Who calls it, what does it call? | `pixel who-calls "X" --role callers` |
| How does A reach B? | `pixel call-path "A" "B"` |
| What did I already change? | `pixel what-changed` |
| A checklist for a multi-file fix | `pixel plan "task"` |

### History

| Instead of | Run |
| --- | --- |
| `git log -S "x"` | `pixel dig-history --phrase "x"` |
| `git log --grep "x"` | `pixel search-history "x"` |
| `git log --follow f` | `pixel file-history --file f` |
| `git blame f` | `pixel who-wrote f` |
| "it worked before" | `pixel plan-rollback "<problem>"`: flags the breaking commit, writes nothing without `--apply` |

### Git changes

| Instead of | Run |
| --- | --- |
| `git status` | `pixel repo-state` |
| `git diff` | `pixel review-changes` |
| `git log --oneline` | `pixel commit-history` |
| `git branch -a -vv` | `pixel list-branches` |
| `git pull --rebase` | `pixel sync-branch` |
| `git add` and `git commit` | `pixel commit --files <f>... -m "msg" --request-id "id"` |
| `git push` | `pixel push`: a leased push, never a raw `--force` |

### Past sessions

`pixel recall` searches the transcripts of every agent on the machine (Claude Code, Codex, Pi and others). `pixel recall search "token"` finds an exact string, `pixel recall ask "topic"` a topic in your own words, and `pixel recall show <ref> --turn N..M` reads the turns around a hit.

## Reading the answers

Every result carries a marker set by the system, not by the model:

- `complete`: every match was returned.
- `capped`: the answer was truncated and more matches may exist. Narrow the pattern or the path.
- `unresolved`: nothing was found. Try another query, or `pixel search-meaning`.

Graph answers (`pixel impact`, `pixel who-calls`, `pixel call-path`) also carry an `epistemics` object:

- `closed_world` is always `false`. Static analysis is never complete, so "0 callers" means none were found, not that none exist.
- `lower_bound: true` flags same-name call sites the resolver could not settle: more edges may exist.
- `extraction_limits` names the known blind spots: callbacks passed as arguments, dynamic dispatch, macro-generated calls, `eval`.

### When native tools are right

Pixel does not cover every job. Use the native command for grep flags Pixel lacks (`-l`, `-m`), pipelines, files outside the index (git-ignored, binary, or over 4 MiB), in-place edits with `sed`, interactive Git such as `rebase -i` and `stash`, and network operations such as `clone`.

## Token savings

`pixel token-savings` reports, for the retrieval commands you ran, the fraction of the candidate pool the agent did not have to read. It measures what reached the agent's context, not your invoice. The replay of [shunt](https://github.com/spotify/portal-ai-plugins/tree/main/plugins/shunt)'s benchmark on Pixel's own repository is on the [home page](../#savings), and its method in the [README](https://github.com/LivioGama/pixel#-token-savings--measured-no-second-model).

Each Pixel command also prints a `🟩 Pixel` line on stderr with its measured duration and two estimates: tokens saved against the native workflow, and time saved against sequential round trips. Both are estimates, and zero or negative values are valid. `--metrics=off` or `PIXEL_METRICS=0` turns the line off.

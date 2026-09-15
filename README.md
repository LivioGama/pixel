# 🟩 Pixel

> **A local control layer that helps coding agents spend less time on simple repository work.**

Pixel is not just a search box. It is the control layer for the whole path from a task to a safe Git change.

Pixel runs locally, connects repository structure with repository history, and returns evidence with boundaries. When it cannot prove that an answer is complete, it says so.

## 🎯 The goal

Make as much repository work deterministic as possible. If something can be answered or carried out from repository state, history, structure, or a proven flow, Pixel should do it directly—with evidence, clear boundaries, and safe recovery. Genuine ambiguity is where the agent should spend its time.

<p align="center">
  <img src="docs/pixel-line.svg" alt="A direct line from a repository to a highlighted answer" width="760" />
</p>

## ⭐ The Pixel Flow Power

```text
task → targets → resolve → impact → edit → changes → review → publish
```

<p align="center">
  <img src="docs/examples/pixel-workflow-comparison.gif" width="100%" alt="Animated comparison of manual repository rediscovery and Pixel’s bounded evidence workflow" />
</p>

## 💡 See the difference

The same repository questions, shown as realistic terminal work. These are workflow illustrations using actual Pixel command names and local output excerpts—not performance benchmarks.

<p align="center">
  <img src="docs/examples/01-resolve-impact.svg" width="100%" alt="Comparing manual discovery with Pixel resolve and impact" />
  <img src="docs/examples/02-task-map.svg" width="100%" alt="Comparing unguided exploration with Pixel targets" />
  <img src="docs/examples/03-review-changes.svg" width="100%" alt="Comparing manual change review with Pixel changes" />
  <img src="docs/examples/04-history-recovery.svg" width="100%" alt="Comparing manual history searching with Pixel history search" />
</p>

## 🚀 Start here

```bash
brew tap LivioGama/tap
brew install pixel
pixel prepare-repo .   # optional warm-up
pixel doctor .         # optional health check
```

Then install Pixel in the agent CLI you actually use — see [Install](#install). The `pixel` binary must be on PATH; the plugin manifests never install it for you.

| Need | Pixel command |
| --- | --- |
| Find text, a symbol, or a concept | `pixel search-content`, `pixel find-code`, `pixel search-meaning` |
| Scope work and see risk | `pixel scope-task`, `pixel impact`, `pixel what-changed` |
| Recover prior code or history | `pixel dig-history`, `pixel search-history`, `pixel plan-rollback` |
| Review or safely publish | `pixel repo-state`, `pixel review-changes`, `pixel sync-branch`, `pixel commit` |

Pixel is local-first. Its index, graph, and optional history data live under `.pixel/`; it reports boundaries when results are capped, stale, or incomplete. Tests and code review remain necessary.

## Install

Each agent CLI can install Pixel through its own native plugin mechanism — no `pixel install` step. The `pixel` binary still has to be on PATH; the plugin never installs it.

Always-on delivery for Claude, Codex and similar plugin CLIs is a `SessionStart`/`SubagentStart` hook (`hooks/pixel-context.sh`) that injects `PIXEL.md` as `additionalContext` — context only, it never blocks a tool call. Devin uses `AGENTS.md` as an always-on rule plus the `/pixel:pixel` skill. For editors and other agents, copy the generated rule files or `PIXEL.md` directly. When `pixel` is missing from PATH, or too old for the command names the protocol uses, the surface stays silent instead of issuing commands that would fail.

> [!NOTE]
> Native plugin manifests live on `develop` and ship with the next release; the default branch `main` does not carry them yet.

### Devin

```bash
devin plugins install github.com/LivioGama/pixel
```

Devin loads `AGENTS.md` as an always-on rule and exposes `/pixel:pixel` as a skill. Make sure `pixel` is on PATH and the repo is indexed (`pixel build-index`) before asking for pixel commands.

### Claude Code

```bash
/plugin marketplace add LivioGama/pixel
/plugin install pixel@pixel
```

Send the two `/plugin` commands as separate prompts. Claude Code also loads `.claude/rules` and `.claude/skills` from this checkout if you prefer a project-local setup.

### Codex

```bash
codex plugin marketplace add LivioGama/pixel
codex plugin add pixel@pixel
```

Run `codex`, open `/hooks`, review and trust the two lifecycle hooks, then start a new thread. This also covers the Codex desktop app after a restart.

### GitHub Copilot CLI

```bash
copilot plugin marketplace add LivioGama/pixel
copilot plugin install pixel@pixel
```

In an interactive Copilot CLI session you can use slash equivalents:

```bash
/plugin marketplace add LivioGama/pixel
/plugin install pixel@pixel
```

### Gemini CLI

```bash
gemini extensions install https://github.com/LivioGama/pixel
```

### Pi

```bash
pi install git:github.com/LivioGama/pixel
```

### OpenCode

Add to `opencode.json`:

```json
{ "plugin": ["@liviogama/pixel"] }
```

Or run from a checkout and point at the built-in `.opencode` surface:

```json
{ "plugin": ["./.opencode/plugins/pixel.mjs"] }
```

### Cursor, Windsurf, Kiro, Cline, Qoder

Copy or vendor the generated rule into your project:

| Tool | Copy this file |
| --- | --- |
| Cursor | `.cursor/rules/pixel.mdc` |
| Windsurf | `.windsurf/rules/pixel.md` |
| Kiro | `.kiro/steering/pixel.md` |
| Cline | `.clinerules/pixel.md` |
| Qoder | `.qoder/rules/pixel.md` |

### Anything else

[`PIXEL.md`](PIXEL.md) at the repo root is the plain-markdown protocol. Paste it into whatever instruction surface the agent offers. The single source of truth is [`crates/pixel-install/assets/pixel-agent-prompt.md`](crates/pixel-install/assets/pixel-agent-prompt.md); all generated surfaces are refreshed from it by `scripts/gen-plugin-assets.sh`.

For architecture and the full command surface, see [ARCHITECTURE.md](ARCHITECTURE.md) and `pixel --help`.
To build from source, run the gates, or open a pull request, see [CONTRIBUTING.md](CONTRIBUTING.md).

## 📝 License

MIT. See [`NOTICE`](NOTICE) for attribution details.

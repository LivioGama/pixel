<h1 align="center">🟩 Pixel</h1>

<p align="center">
  <strong>A local code index for AI coding agents.</strong><br />
  Your agent asks Pixel instead of reading files, and spends its tokens on the hard part.
</p>

<p align="center">
  <a href="https://github.com/LivioGama/pixel/releases/latest"><img src="https://img.shields.io/github/v/release/LivioGama/pixel?color=2ea043&label=release" alt="Latest release" /></a>
  <a href="https://github.com/LivioGama/pixel/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/LivioGama/pixel/ci.yml?branch=main&label=CI" alt="CI" /></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue" alt="MIT license" /></a>
</p>

<p align="center">
  <a href="https://liviogama.github.io/pixel/"><b>Website</b></a> ·
  <a href="https://liviogama.github.io/pixel/docs/"><b>Docs</b></a> ·
  <a href="https://liviogama.github.io/pixel/benchmarks/"><b>Benchmarks</b></a>
</p>

<p align="center">
  <img src="docs/examples/pixel-scope-comparison.webp" width="100%" alt="The same task without Pixel and with it: the agent wanders the repository, or starts from a ranked list of files" />
</p>

## Why

- **95% fewer tokens** to learn what a large file contains, measured on this repository, with no second model reading on the agent's behalf.
- **Evidence with boundaries.** Every answer says whether it is complete, capped or stale; a static call graph never claims it saw every caller.
- **Local and deterministic.** The index lives in `.pixel/` at the repository root. Nothing leaves the machine.
- **Safe Git.** `pixel impact` before an edit, crash-safe `pixel commit-and-push` after it, never a raw `--force`.

## Install

```bash
brew install LivioGama/tap/pixel
pixel install      # once: wires Claude Code, Codex, Pi, OpenCode and Antigravity
pixel doctor .     # optional health check
```

Other channels, per-agent plugins and manual setup are in the [docs](https://liviogama.github.io/pixel/docs/).

### Let your agent install it

Paste this into Claude Code, Codex, Pi or any coding agent with a shell:

```text
Install Pixel (https://github.com/LivioGama/pixel) for me: follow the
"For AI agents" section of its README, then tell me what changed and
whether `pixel doctor .` is green.
```

## For AI agents

If you are an LLM reading this to set Pixel up for a user, do this, in order:

1. **Check first.** `command -v pixel && pixel --version`. If it prints a version, skip to step 3.
2. **Install the binary** (macOS or Linux). With Homebrew: `brew install LivioGama/tap/pixel`. Without it: `curl -fsSL https://github.com/LivioGama/pixel/releases/latest/download/install.sh | sh`, which installs into `~/.local/bin` (override with `PIXEL_INSTALL_DIR`); make sure that directory is on `PATH`.
3. **Wire the agents.** Tell the user it writes to their home configuration (`~/.local/share/pixel/`, `~/.claude/settings.json`, `~/.codex/config.toml`, `~/.pi/agent/APPEND_SYSTEM.md`), then run `pixel install`. It is idempotent: running it again only refreshes what it wrote.
4. **Index the current repository** (optional, speeds up the first queries): `pixel prepare-repo .`
5. **Verify:** `pixel doctor . --json`. Report every check that is not green, with its message; do not claim success otherwise.
6. **Tell the user to restart the agent session.** The protocol is injected when a session starts, so the current one does not have it yet.

Then use Pixel the way [`PIXEL.md`](PIXEL.md) describes: it is the complete agent protocol, the same text `pixel install` deploys, and it says which command replaces `grep`, `git log`, `git blame` and whole-file reads, and when a native tool is still the right choice. An agent that `pixel install` does not wire (Cursor, Gemini CLI, Copilot…) needs that file in its own rules; see the [plugins table](https://liviogama.github.io/pixel/docs/#plugins).

## The flow

```text
task → scope-task → find-code → impact → edit → what-changed → review-changes → commit-and-push
```

## More

- [Documentation](https://liviogama.github.io/pixel/docs/): install, updating, plugins, every command
- [Benchmarks](https://liviogama.github.io/pixel/benchmarks/): every number with its method, losses included
- [ARCHITECTURE.md](ARCHITECTURE.md): crates and the full command surface
- [CONTRIBUTING.md](CONTRIBUTING.md): build from source, gates, pull requests

MIT licensed. See [`NOTICE`](NOTICE) for attribution.

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
pixel ready .       # optional warm-up
pixel doctor .      # optional health check
pixel install       # let your agent use Pixel
```

| Need | Pixel command |
| --- | --- |
| Find text, a symbol, or a concept | `pixel search`, `pixel resolve`, `pixel ask` |
| Scope work and see risk | `pixel targets`, `pixel impact`, `pixel changes` |
| Recover prior code or history | `pixel excavate`, `pixel history-search`, `pixel rescue` |
| Review or safely publish | `pixel inspect`, `pixel review`, `pixel reconcile`, `pixel publish` |

Pixel is local-first. Its index, graph, and optional history data live under `.pixel/`; it reports boundaries when results are capped, stale, or incomplete. Tests and code review remain necessary.

### Manual setup

Don't want to run `pixel install`? That's fine. You can deploy the agent
system prompt and wire it into your agent by hand — see
[Manual Setup](docs/manual-setup.md). The prompt itself lives at
[`crates/pixel-install/assets/pixel-agent-prompt.md`](crates/pixel-install/assets/pixel-agent-prompt.md)
(~266 lines / ~3 000 tokens). It's large because Pixel replaces a wide
range of native commands (`grep`, `rg`, `git log -S`, `git blame`, manual
caller tracing) with a single indexed workflow — and the token cost is
recovered in as little as one `pixel impact` call.

For architecture and the full command surface, see [ARCHITECTURE.md](ARCHITECTURE.md) and `pixel --help`.

## 📝 License

MIT. See [`NOTICE`](NOTICE) for attribution details.

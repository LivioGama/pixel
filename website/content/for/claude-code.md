---
title: "Pixel for Claude Code"
description: "What pixel install writes into Claude Code's settings, the per-repository guard, the plugin alternative, how to check the wiring and how to remove it."
agent: "claude-code"
---

<!-- Setup sections: data/agents.toml through layouts/shortcodes/agent-setup.html. The figure below restates content/benchmarks.md#on-whole-agent-tasks: change it there first. -->

{{% agent-setup %}}

## Measured

On one scoping task in Pixel's own repository, Claude Code with Pixel took a median **30% less wall time** than the same agent without it, over 11 runs per side (Claude Sonnet 5, Pixel 0.5.0, September 2026). That is one task on one repository: a direction, not a promise for yours. [The runs, their spread and the cases where Pixel is slower](../../benchmarks/#on-whole-agent-tasks)

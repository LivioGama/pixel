---
title: "Pixel for Claude Code"
description: "What pixel install writes into Claude Code's settings, the per-repository guard, the plugin alternative, how to check the wiring and how to remove it."
agent: "claude-code"
---

<!-- Setup sections: data/agents.toml through layouts/shortcodes/agent-setup.html. The figure below restates content/benchmarks.md#on-whole-agent-tasks: change it there first. -->

{{% agent-setup %}}

## Evidence and limits

The newer Opus medium trial with install hooks reported 42.9 s without Pixel and 47.6 s with it on one scoping task, with raw runs outside the repository. The older Sonnet demo used an appended prompt without hooks and included trace-reading contamination. Neither establishes a gain for your sessions. [Protocols, historical results and limits](../../benchmarks/#on-whole-agent-tasks)

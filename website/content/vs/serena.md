---
title: "Pixel vs Serena"
description: "Serena gives an agent type-resolved references and symbol-level edits through language servers; Pixel is one binary with one index, task scope and Git history. Not benchmarked."
tool: "serena"
---

## How they differ

Serena drives a language server per language (or its JetBrains plugin) and exposes it as MCP tools: find a symbol, its references, replace its body, insert beside it, rename it. Its answers are as exact as the language server's. Pixel parses every language it knows into one tree-sitter graph and a search index in `.pixel/`, and answers from the shell: `pixel who-calls`, `pixel impact`, `pixel scope-task`, with Git history through `pixel dig-history` or `pixel file-history`.

## Choosing

Use Serena when the agent must edit or refactor at the symbol level with a compiler's precision. Use Pixel to scope a task, measure a change's blast radius and read the history behind it without starting a server per language. They combine: nothing stops an agent from asking Pixel where to look and Serena how to edit.

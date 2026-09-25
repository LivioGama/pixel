---
title: "Pixel vs code-graph-rag"
description: "code-graph-rag answers plain-English questions through an LLM that writes Cypher over Memgraph and can edit code; Pixel needs no database, Docker or LLM key. Not benchmarked."
tool: "code-graph-rag"
---

## How they differ

code-graph-rag parses code with tree-sitter (and compiler front ends for some languages) into Memgraph, with a vector store beside it, both started in Docker. A plain-English question goes to an LLM that writes the Cypher; its fixed tools (callers, callees, definitions) query the graph directly. It can also edit code and merge runtime traces into the graph. Pixel is one binary: its graph and search index live in `.pixel/`, and `pixel who-calls`, `pixel impact` or `pixel search-meaning` answer without a model.

## Choosing

Use code-graph-rag when you want to ask the graph questions in plain English through an LLM, or have the tool edit code and follow runtime traces. Use Pixel when the agent should get callers, blast radius, task scope and Git history with no service to run and no key to configure.

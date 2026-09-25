---
title: "Pixel vs CodeGraphContext"
description: "Both build a call graph with tree-sitter; CodeGraphContext stores it in a graph database with raw Cypher and an HTML view, Pixel adds search, Git history and guarded Git writes. Not benchmarked."
tool: "codegraphcontext"
---

## How they differ

CodeGraphContext indexes code with tree-sitter, or a SCIP indexer, into a graph database you pick (FalkorDB Lite by default, KuzuDB or Neo4j) and serves it as MCP tools and the `cgc` CLI, including raw Cypher. Pixel keeps its graph and a text and embedding search index in `.pixel/`, answers from the shell (`pixel who-calls`, `pixel impact`, `pixel search-meaning`) and covers Git: `pixel dig-history`, `pixel commit`.

## Choosing

Use CodeGraphContext to query the graph in Cypher, pick its database, or see the graph drawn. Use Pixel for one binary with no Python and no database, plain-English search, and the Git history and writes an agent needs around its edits.

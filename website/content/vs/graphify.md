---
title: "Pixel vs graphify"
description: "graphify builds a knowledge graph of code, docs, PDFs and media for an agent to query; Pixel stays on code and Git, with blast radius, task scope and guarded Git writes. Not benchmarked."
tool: "graphify"
---

## How they differ

graphify parses code with tree-sitter into a graph written to local files, clusters it into communities, and adds docs, SQL schemas, PDFs and images through a model; video and audio are first transcribed locally with faster-whisper, and the transcripts then go through the same model. An agent reaches it through a `/graphify` skill, a CLI, hooks or an optional MCP server. Pixel indexes code only, and answers the questions an edit raises: `pixel impact` before it, `pixel scope-task` to find the files, `pixel dig-history` for why the code is the way it is, `pixel commit` to land it.

## Choosing

Use graphify when the answer lives outside the code too (a design doc, a schema, a PDF) or when you want to see the project as a graph. Use Pixel for the code-and-Git loop of an agent's edit. The two hook into the same agents, so they can run side by side.

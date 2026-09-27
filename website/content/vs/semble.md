---
title: "Pixel vs semble"
description: "Pixel and semble measured on the same plain-English queries: Pixel puts the right file first more often, semble keeps it in its top 10 every time and costs less context; Pixel adds the call graph and Git history."
tool: "semble"
---

## How they differ

semble is a search tool for questions in plain English. On the same 45 queries, `pixel search-meaning` in Pixel 0.6.0 ranks the right file first more often and answers faster; semble misses none in its top 10, where Pixel misses two, and it costs about a quarter of Pixel's context per turn. The queries are the code's own doc comments, which suits Pixel's chunks, cut along symbols with their comments. Pixel is also a wider index: `pixel who-calls`, `pixel impact` and `pixel scope-task` answer from the call graph, and its history and Git commands cover what a search tool does not.

## Using both

They combine. Route plain-English search to semble if a candidate list that never misses matters most, and the structural questions to Pixel; the short answer above gives what the pair costs in context.

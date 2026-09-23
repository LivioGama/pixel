---
title: "Benchmarks"
description: "Every number on the home page, with its method, its sample size and the cases where Pixel loses."
---

<!-- Figures come from docs/bench/; every section links its source. crates/pixel/tests/cli/docs_drift.rs reads this page, so every `pixel <command>` quoted here must exist. -->

Every number below links to its method and raw data in the repository, with the scripts to re-run it on your own code. Losses sit next to wins.

## Reading code

What reaches the agent's context when it needs to know what a file contains, measured on Pixel's own repository (138K lines of Rust), counting UTF-8 bytes divided by four. No second model reads the files instead.

| Scenario | Lines | Full reads | With Pixel | Saved |
| --- | --- | --- | --- | --- |
| Single large file | 4,661 | 48,465 tok | 2,172 tok | 95.5% |
| Multi-file cross-read | 7,106 | 68,934 tok | 3,768 tok | 94.5% |
| Source and test pair | 5,289 | 48,978 tok | 1,890 tok | 96.1% |
| Code-write context | 5,289 | 48,978 tok | 1,910 tok | 96.1% |

This counts what the agent reads, not your invoice. `pixel token-savings` reports the same ratio from your own sessions: 41 to 83% across 798 operations on the maintainer's machine. The scenarios replay the shape of [shunt](https://github.com/spotify/portal-ai-plugins/tree/main/plugins/shunt)'s benchmark, which reaches a similar ratio by rerouting reads through a paid second model. [Method and figures, as first published](https://github.com/LivioGama/pixel/blob/632b3685a97e941476cb42aa75333b79f0ed8955/README.md#-token-savings--measured-no-second-model)

## Against GitNexus

The jobs both tools do: 29 blast-radius cases on four repositories in Rust, TypeScript and Ruby, with callers found by grep as the ground truth. GitNexus 1.6.12 and Pixel 0.4.0, same machine, September 2026.

| | Pixel | GitNexus |
| --- | --- | --- |
| Callers found (recall, 29 cases) | 0.86 | 0.84 |
| Median time per answer | **153 ms** | 432 ms |
| Mean answer size | **4.5 KB** | 11.0 KB |
| Context cost on every turn | **~4,160 tokens** | ~19,700 tokens |
| Cold index of Pixel's repository | **9.3 s, 8.6 MB** | 28.7 s, 184 MB |
| Git history and Git operations | **Yes** | No |
| Cypher queries, taint analysis, API route maps | No | **Yes** |
| Callers in Ruby (two repositories) | 0.90 and 0.56 | **1.00 and 0.68** |
| Licence | **MIT** | PolyForm Noncommercial |

Recall is a tie at this sample size: Pixel finds every caller in Rust and TypeScript, GitNexus does better on Ruby. The index comparison covers one repository only. [Full method and raw rows](https://github.com/LivioGama/pixel/blob/main/docs/bench/vs-gitnexus.md)

## On whole agent tasks

Claude Code on real tasks in this repository, with Pixel and without, against a vanilla agent with no rules or hooks.

The home page's demo, September 2026: Claude Sonnet 5, Pixel 0.5.0, one scoping task ("retry a leased push when the remote branch moved: list the files to change"), 11 runs per side, each pair started together, the same bare setup on both sides except Pixel's agent prompt.

| Median over 11 runs | Without Pixel | With Pixel |
| --- | --- | --- |
| Wall time | 86.5 s | **60.7 s** (−30%) |
| Tokens read into context | 17,268 | **10,655** (−38%) |
| API cost | $0.394 | **$0.274** (−30%) |

Both sides named `push.rs` among their first two files in every run, and the Pixel side called Pixel in every run, 6 to 17 times. The spread is wide on both sides: 57 to 148 s without Pixel, 33 to 207 s with it. [Every run, its trace and the recording scripts](https://github.com/LivioGama/pixel/tree/main/docs/motion)

The August A/B runs, on an earlier release:

- **About 30% faster** to scope a multi-file task. Two independent A/B designs agree: 31% and 29%.
- **About 1.5 seconds slower** on a single lookup in the isolated run: the cost of reading Pixel's guidance before a one-shot answer, since the task never ran a Pixel command.
- **Still slower** at recovering deleted code from history. Open work.

Three runs per cell, so these are directions, not decimals. The agents of that release also called Pixel less than its protocol asks: given the protocol alone, with no hooks, the agent barely ran a Pixel command and still scoped tasks 29% faster, so part of the gain is the protocol's guidance rather than its answers.

[Agent A/B runs and their caveats](https://github.com/LivioGama/pixel/blob/main/docs/bench/measured-performance.md)

## Where a specialist wins

- **Natural-language search:** semble finds the right file in its top 10 for 100% of 45 queries, Pixel for 69%.
- **Compact repository map:** stacklit covers more directories for fewer tokens on 3 of 4 repositories.
- **Context cost:** Pixel is 4.7× lighter than GitNexus, but heavier than semble (~980 tokens) and stacklit (~420).

They combine: semble for search and Pixel for the graph, history and Git costs about 5,100 always-on tokens. [Full comparison](https://github.com/LivioGama/pixel/blob/main/docs/comparison.md)

## Coding decisions

`pixel classify` puts a bounded question to a model you configure (OpenRouter, Ollama Cloud or a local server) and returns one probability per label. On the 14 public coding items of JevBench:

| Model, through `pixel classify` | Coding accuracy | Median per item |
| --- | --- | --- |
| deepseek-v4.1-flash | **1.00** (14 of 14) | 1.4 s |
| gpt-oss:120b | **0.93** (13 of 14) | 2.0 s |
| deepseek-v4-flash | **0.93** (13 of 14) | 2.0 s |
| gpt-oss:20b | **0.93** (13 of 14) | 6.4 s |
| nemotron-3-ultra | **0.86** (12 of 14) | 8.1 s |
| Jev, published score | 0.839 (56 items) | |

Read it with its limits: Jev's 0.839 is its own published score on 56 coding items, not re-measured here, and 14 items is a small sample. Answers come from a remote model and are not deterministic; every answer says so. No off-the-shelf local model up to 575M parameters passed 0.50 on the same items. [The bake-off](https://github.com/LivioGama/pixel/blob/main/docs/bench/decide-bakeoff.md)

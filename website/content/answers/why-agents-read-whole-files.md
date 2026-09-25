---
title: "Why does my coding agent read whole files?"
description: "Coding agents open whole files because their read tool returns files, not answers. What that costs on well-known large files, and what an index answers instead."
answer: "why-agents-read-whole-files"
---

<!-- Figures: data/answers.toml and data/read_savings.toml. Every number here must be in the /benchmarks/ sections the entry names; the build fails otherwise. -->

## Where the tokens go

An agent learns a repository the way its tools let it: it searches for a name, gets a line back, then opens the file around that line to understand it. Its read tool has no notion of "the functions in this file" or "the class this method belongs to", so the whole file enters the context, and it stays there for every later turn of the session.

On a small file that costs little. On the large files every real codebase has, a trainer, a router or a query builder of several thousand lines, one read fills a large part of the context before the agent has written anything.

## What an index returns instead

`pixel list-signatures` answers "what does this file contain" from an index Pixel keeps in `.pixel/`: each module- and class-level definition with its line, and nothing else. The agent then reads the lines it needs with a targeted range. No second model reads the file in its place: the saving is the index's own answer, counted with the same rule on both sides (UTF-8 bytes divided by four).

To check it on a file of yours, [Measure it on your own code](../../benchmarks/#measure-it-on-your-own-code) gives the command pair: the whole file against `pixel list-signatures` on it, in bash or zsh.

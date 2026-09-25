---
title: "Pixel vs a language server"
description: "A language server resolves types and gives more exact references; Pixel covers every language in one binary, adds Git history and marks incomplete answers. Not benchmarked."
tool: "language-server"
---

## How they differ

A language server answers from resolved types, for one language and one cursor position at a time, inside the editor that started it. Pixel answers from a syntax graph parsed with tree-sitter, every language it knows in the same index, and takes a symbol name, a file or a task description: `pixel who-calls`, `pixel impact`, `pixel scope-task`. Its graph answers carry an `epistemics` object that says what static analysis could not see.

## Choosing

Use the language server for exact references in one language, a rename your editor drives, or type errors. Use Pixel when an agent in a shell needs callers, blast radius or history across a repository, with no server to start per language.

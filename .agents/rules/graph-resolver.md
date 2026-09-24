---
paths:
  - "crates/pixel-graph/**"
---

# Graph Extraction and Resolution

Loaded when `pixel-graph` is in play. A change to what an import, a call or
a symbol means has to hold on every path below, and on the language rule it
models; #255, #262 and #264 took eleven CodeRabbit findings between them
because each fix covered the path it started from.

## The chain a resolution change crosses

Walk it top to bottom and name, in the pull request, the test that covers
each link the change touches:

1. **Extraction** (`extract.rs`): what the walker records, per language
   (`RawImport`, `RawCall`, `RawReference`, `UseLeaf`).
2. **Storage** (`store.rs`): the column, its `ALTER TABLE` migration in
   `migrate`, and `EXTRACTOR_VERSION` in `build.rs` with a history line.
3. **The index** (`ResolveIndex::build`): what it loads from the rows.
4. **Every resolution path**, not only the first one:
   - `resolve_calls` (full build and changed files),
   - `resolve_references` and its `names_a_symbol` prefilter,
   - `resolve_all` (retries stored `unresolved_calls` rows),
   - `reconsider_resolved_calls` (a changed definition re-decides edges),
   - the incoming-edge demotion in `write_rows` (a rewritten target file),
   - the dangling-import re-resolution in `write_rows` (a file added after
     its importer).
   Each one rebuilds the decision from what it stored: a site line, a
   receiver, the name the call wrote (`edges.callee`). A path that drops one
   decides differently from the build that wrote the edge.
5. **`rename`**: `plan` finds sites through edges and import rows;
   `import_name_nodes` / `use_leaf_name_nodes` locate the text to rewrite.
6. **Diagnostics**: `tests/all/unresolved_diagnostic.rs` classifies
   unresolved rows with the same `decide_at` the resolver uses.

A test for a new input runs it through a full build **and** an incremental
update (`update_file` on the importer, on the target, and on a file that adds
a competing definition): #262's alias edge passed the build and failed the
update that rewrote the target.

## Language semantics before shortcuts

Model the rule the language states, cite it in the doc comment, and test the
cases that separate it from a file-wide approximation. For a Rust `use`
([reference](https://doc.rust-lang.org/reference/items/use-declarations.html)):

- **Scope:** a binding holds in its module or block; a nested inline module
  sees its parent's names only through `use super::*;`, and never a block's.
- **Shadowing:** the innermost applicable `use` wins; two that still tie and
  name different items leave the call unresolved, even in one file — never
  pick by symbol order.
- **Alias:** `use a::b as c` binds `c` to the source `b`; `b` is not in scope,
  and `c` never falls back to an unrelated same-name definition (T2).
- **Groups and globs:** each path of `use a::{b::x, c::y}` resolves to its
  own file; a glob binds no name T1 can prove.

Other languages get the same treatment for their import forms (TS `import {
a as b }`, re-exports, default imports; Ruby calls without receiver or
parentheses, #236).

## Tier honesty

An Exact edge asserts the callee. When the evidence does not single out one
definition — several candidate files, several applicable imports, a receiver
of unknown type — the answer is `Probable` or `Unresolved`, never the first
candidate: a missing edge lowers recall and is reported in the envelope, a
wrong Exact edge is believed.

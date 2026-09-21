# Coming from GitNexus

If you already run [GitNexus](https://github.com/abhigyanpatwari/GitNexus), you
have a graph-powered code intelligence layer that works. This page is for
deciding whether pixel is a better fit for your codebase — with the measurements
behind every number, including the ones pixel loses.

Everything here was measured on **GitNexus 1.6.12** (commit `737634705`, rebuilt
from source) against **pixel 0.4.0** on 2026-09-21. A newer GitNexus may differ.
Method, raw data and the fairness fixes applied to the harness are in
[`docs/bench/vs-gitnexus.md`](bench/vs-gitnexus.md); the scripts are in
[`scripts/vs-gitnexus/`](../scripts/vs-gitnexus/) so you can re-run the whole
thing on your own repository.

## The short version

| | pixel 0.4.0 | GitNexus 1.6.12 |
|---|---|---|
| Licence | MIT | PolyForm Noncommercial 1.0.0 |
| Install | one self-contained binary (65.5 MiB), no runtime | Node.js + `npm i -g`, native tree-sitter grammars built on install |
| Always-on context cost | ~4 160 tokens | ~19 700 tokens |
| `impact` latency (p50, 29 cases) | 153 ms | 432 ms |
| `impact` answer size (mean) | 4.5 KB | 11.0 KB |
| `impact` recall (29 cases, all langs) | 0.86 | 0.84 |
| `impact` recall, Rust / TypeScript | **1.00 / 1.00** | 0.87 / 0.88 |
| `impact` recall, Ruby | 0.56–0.90 | **0.68–1.00** |
| Git history archaeology | yes | — |
| Cypher, PDG/taint, API route maps | — | yes |

## The licence is probably the deciding factor

GitNexus ships under the [PolyForm Noncommercial
1.0.0](https://polyformproject.org/licenses/noncommercial/1.0.0) licence, which
permits use for noncommercial purposes. Using it inside a commercial product or
business requires a separate arrangement with its author. pixel is MIT: use it
at work, ship it, fork it, no conversation required.

If you are evaluating for a company, check this before anything else on this
page. If you are a hobbyist or researcher, it may not matter to you at all.

## Where pixel measurably wins

**It costs 4.7× less context, permanently.** GitNexus registers 17 MCP tools
whose schemas serialise to 75 202 bytes, plus a 3 727-byte block it writes into
your `CLAUDE.md` — about 19 700 tokens loaded on every single turn, whether the
agent uses them or not. Its `impact` schema alone is 21 946 bytes. pixel's
installed doctrine is 16 631 bytes, about 4 160 tokens, and a CLI costs nothing
further until it is actually invoked.
([measurement](bench/vs-gitnexus.md#context-tax))

**Answers arrive ~2.8× faster.** 153 ms vs 432 ms p50 across 29 blast-radius
queries, on every corpus, with no overlap between the distributions. Same story
on change mapping (185 ms vs 399 ms) and call-path finding (~155 ms vs ~341 ms).
([measurement](bench/vs-gitnexus.md#blast-radius-impact--impact--29-cases-4-repos-3-languages))

**Answers are smaller.** 4.5 KB vs 11.0 KB mean for the same question. On the
Rust corpus the gap is 6.4 KB vs 27.5 KB, and the worst single case is 12.6 KB
vs 76.3 KB — that is a quarter of a 300k context window spent on one blast-radius
query.

**Blast radius is complete on Rust and TypeScript.** pixel returned every true
caller in all 16 cases across both languages — including on GitNexus' own
TypeScript codebase, where GitNexus scored 0.88 and returned nothing at all for
`isHardcodedIgnoredDirectoryAtPath` despite five true caller files in its own
source (six call sites, two of them inside the defining file).

**Git history is a first-class surface.** `search-history`, `dig-history`,
`file-history`, `who-wrote` and `plan-rollback` answer "when did this break",
"who owns this region" and "what was the last good version" from an indexed
history. GitNexus does not implement this; you would drop back to
`git log -S` and `git blame`.

**Repo operations are part of the same tool.** `repo-state`, `review-changes`,
`commit`, `push`, `sync-branch`, `list-branches`, `fast-forward` — crash-safe and
idempotent, so an agent that dies mid-operation does not leave a half-committed
tree.

**Uncertainty is reported, not hidden.** Every graph answer carries an
`epistemics` object: `closed_world` is always `false`, and `lower_bound` flags
when call sites could not be resolved. This is not cosmetic — in the one corpus
where pixel loses badly, its own output announced the problem (below).

## Where GitNexus is still the better fit

These are measured or structural, not concessions for form's sake.

**Ruby.** GitNexus is better at it: recall 1.00 vs 0.90 on alonetone and 0.68 vs
0.56 on dd-trace-rb. The mechanism is specific — pixel's code graph does not
model call sites at the **top level of Ruby script files** (`appraisal/*.rb`,
`db/seeds/*.rb`), because there is no enclosing symbol to attach the edge to.
pixel's text index has those lines, and `pixel find-symbol` prints
`lower bound — 126 same-name call site(s) unresolved`, but `impact` returns
nothing. If your codebase is Ruby-heavy with significant top-level script code,
this will bite you today.

**Noisy answers on dynamic Ruby.** Asked about `application` in dd-trace-rb,
pixel returns 8 files at depth 1 and not one is a caller; GitNexus returns none.
Eight confident wrong answers cost an agent more than silence. (An earlier
revision of this page reported a corpus-level precision loss, 0.89 for GitNexus
against 0.85 for pixel. That figure averaged in the two Ruby corpora, whose
truth sets are incomplete by construction, and is withdrawn — over the 16 cases
where precision is scorable, pixel leads 0.98 to 0.94.)

**Program analysis pixel does not have at all.** Raw Cypher over the graph
(`cypher`), a persisted program dependence graph with taint findings
(`pdg_query`, `explain`), and the web-API tools (`route_map`, `shape_check`,
`api_impact`, `tool_map`). If you query the graph directly or rely on taint
analysis, pixel has no replacement and you should keep GitNexus.

**Multi-repo groups.** `group_list` / `group_sync` build a cross-repo contract
registry. pixel is single-repo.

**A web UI and generated wikis.** `gitnexus serve`, `gitnexus wiki`. pixel is
CLI and agent-facing only.

**Disk, if you enable history.** pixel's comparable index is 8.6 MB against
GitNexus' 184 MB on the same repo, but pixel's opt-in history database adds
~849 MB. That is the price of the archaeology commands; leave `--history` off and
the footprint stays small.

## Running them side by side

You do not have to choose on day one, but know exactly what the installer does.

**`pixel install` deletes GitNexus' generated section** from your `CLAUDE.md` and
`AGENTS.md` — the `# GitNexus — Code Intelligence` block and its subsections
(`crates/pixel-install/src/config.rs`, `strip_stale_blocks`). It is regenerable
with `gitnexus analyze`, but it does go. Removal is bounded to a real generated
section header: hand-written prose that merely *mentions* GitNexus survives
verbatim, and there is a regression test for exactly that case.

**Your MCP server and hooks are left alone.** pixel's hook router explicitly
recognises GitNexus' passive Claude context hook and coexists with it instead of
clobbering it (`crates/pixel-install/src/routing.rs`,
`passive_gitnexus_claude_hook`).

Practically: keep GitNexus' MCP server registered, install pixel, use both for a
week, and re-run `gitnexus analyze` if you want its `CLAUDE.md` block back. When
you want the context budget, unregister the MCP server — that is where the
~19 700 tokens live.

## Command mapping

| GitNexus | pixel |
|---|---|
| `impact <symbol>` | `pixel impact <symbol>` |
| `context <symbol>` | `pixel pack-context <uid>` / `pixel find-symbol` |
| `trace <from> <to>` | `pixel call-path <from> <to>` |
| `detect_changes` | `pixel what-changed` |
| `query <concept>` | `pixel find-code` / `pixel search-meaning` / `pixel list-flows` |
| `rename <old> <new>` | `pixel rename <old> <new>` |
| `check` | `pixel plan` (findings only — not a full structural check) |
| `list_repos` / `status` | `pixel status` / `pixel repo-state` |
| `analyze` | `pixel build-index` (+ `--history` for the archaeology commands) |
| `cypher`, `pdg_query`, `explain` | no equivalent |
| `route_map`, `shape_check`, `api_impact`, `tool_map` | no equivalent |
| `group_list`, `group_sync` | no equivalent |
| — | `search-content`, `scope-task`, `plan`, `repo-map`, `note` |
| — | `search-history`, `dig-history`, `file-history`, `who-wrote`, `plan-rollback` |
| — | `repo-state`, `review-changes`, `commit`, `push`, `sync-branch`, `list-branches` |
| — | `recall` (search your past agent sessions) |

## Rolling back

`pixel uninstall` removes everything `pixel install` wrote: its managed blocks
from agent config files, its hook entries from every settings file, the hook
scripts, the rule source file and the binary itself. It is idempotent.

It does **not** restore the GitNexus `CLAUDE.md` section that `pixel install`
removed — run `gitnexus analyze` for that. Nothing else of GitNexus' is touched
at any point.

## Verify this yourself

Nothing above asks to be taken on trust. On your own repository:

```sh
git clone https://github.com/LivioGama/pixel && cd pixel
export GITNEXUS_CLI=/path/to/GitNexus/gitnexus/dist/cli/index.js   # or put `gitnexus` on PATH
python3 scripts/vs-gitnexus/gen-truth.py /path/to/your/repo rust 8 target,tests > cases.json
python3 scripts/vs-gitnexus/bench-impact.py /path/to/your/repo cases.json
```

The ground truth is grep-derived from your source, not from either tool's graph,
so neither can be right by construction. The exact parameters behind the
committed fixtures are in
[`docs/bench/vs-gitnexus/cases/REGENERATE.md`](bench/vs-gitnexus/cases/REGENERATE.md) —
the Ruby ones need a wider window than the defaults. If pixel loses on your codebase, the
harness will say so — it did on Ruby.

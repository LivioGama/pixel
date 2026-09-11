# Philosophy completion: capability and closure map

## Contract and evidence boundary

Pixel remains the deterministic local control layer from **task → evidence →
understood change → safe publication**. Extend the existing CLI, daemon, crate
owners, native integrations and guarded workers. This map does not propose a
retrieval MCP, a replacement engine, general centralization, removal of task
orchestration, learned retrieval, external telemetry or paid evaluations.

This is a **source-backed gap inventory, not a parity or completion claim**.
`S` means implemented source inspected; `R` means a real CLI result observed in
this session; `U` means an explicit investigation/implementation/verification
task remains. Existing tests named below are test locations, not claimed passes.
Line numbers are locators at the audit baseline; function names are durable
locators. Every unresolved row has a closure test; none is silently optional.

### Inputs and reproducibility

- Pixel audit baseline: `bed86dc79b8408396380098a28de3a338c7be5d4`, branch
  `develop`, initially clean according to `pixel inspect`. The initial text
  index still named `ac4ff06f4906cd876bcbbcdf3a5c772fe26055a0`; source reads
  and returned function bodies, rather than absence in that index, ground this
  map. Concurrent implementation changes must be reviewed separately.
- Product and architecture: repository `README.md` and `ARCHITECTURE.md`.
- Supplied report: `/private/tmp/graft_req.txt`, seven-page “Pixel × Graft”,
  dated 2026-09-11, examining Pixel `71a3331`. Original rendered pages are in
  `/private/tmp/pixel-graft-reading.ytYvfu/`. Its performance figures are
  historical, unreplicated report claims, not measurements by this audit.
  Text SHA-256: `b7e60afb738335a071e0c4f65fd21a863d9a29d899faa6cda2d63fc7b8e5a75f`.
- Original plan did not supply a Graft commit. For reproducible comparison a
  **new audit pin**, not a purported recovery of that missing pin, was obtained:
  [`trailhq/Graft@f9e65396e638e517aecae0d731017f53084d70ed`](https://github.com/trailhq/Graft/tree/f9e65396e638e517aecae0d731017f53084d70ed),
  checked out at `/private/tmp/pixel-graft-pin-20260911`. Pixel preparation added
  an ignored-cache entry to that disposable checkout's `.gitignore`; upstream
  source files were not edited. **U-00:** reconcile any subsequently supplied
  original pin against this pin before claiming original-plan parity.
- Discovery used bounded `pixel targets/search/symbol/context`, with caps and
  stale-index caveats retained. No benchmark, model or hidden-reasoning estimate
  was used to establish capability correctness.

## Reconciliation of the nine report proposals

| Report proposal | Baseline reality | Closure |
| --- | --- | --- |
| Correctness evaluation | `docs/bench/`, `pixel-bench` already exist; historical latency is not correctness proof. | Use deterministic real-shape positive/negative/ambiguous fixtures and explicit results below. No paid benchmark campaign. |
| Generic extractor | `extract::walk_generic`, `generic_symbol_kind`, `generic_call`, `generic_import` exist; graph has 14 language tags, not report's eight. | Complete language matrix below without replacing extractor. |
| Post-edit blast radius | `guard::run_post_tool_use`, `post_tool_use_blast_radius` exist. | Resolve freshness, useful dependency evidence, host-specific delivery and installation; do not rebuild from nothing. |
| File skeleton | `Service::op_skeleton`, `GraphStore::symbols_in_file` and CLI `Skeleton` exist. | Preserve and verify signatures, scope, caps and source integrity. |
| Fan-in ranking | `signals::score_signals`, `rerank::rerank`, daemon `fan_in_counts` exist. | Preserve within-tier ranking and candidate-set normalization; measure fixture usefulness, not presumed superiority. |
| Statusline | CLI `Command::Status { statusline, .. }` renders `pixel status --statusline`; registration must be reconciled with current minimal installer. | Verify source/registration/live execution separately, preserve exact statusline stream. |
| Monorepos/submodules | `discover_root_follow_submodules` and shared `policy_walk` exist. | Root discovery is not federated multi-repo coverage; establish explicit policy and fixtures. |
| Persistent human notes | `GraphStore::set_annotation`, query APIs, and result/task integration exist. | Verify rebuild/move/delete survival and discoverable editing/export workflow. |
| Content-anchored Crux | `extract_crux`, fingerprint storage, full/incremental build insertion and context rendering exist. | Verify line-coordinate/size/freshness correctness and useful source-backed excerpts. |

## Discovery: languages and variants

Existing owner for every row: `pixel-graph/src/extract.rs::lang_of`,
`language_for`, `extract_inner`, its language walkers or `walk_generic`;
dependencies belong in the existing graph crate. Graft's pin is established
from `src/graph/extract.ts::EXTENSIONS`, `generic.ts::GENERIC_LANGS`, and
`container.ts::CONTAINER_LANGS`, **not its potentially stale language count**.
Graft lists Java in both depth/broad registries; that is one language, not two.

Each U-L row requires a real-shape fixture with a declaration/signature,
local call, imported call, same-name ambiguity and malformed-source fallback;
variants must test path dispatch as well as parsing. Positive syntax coverage
must not imply compiler-grade edges.

| ID / language | Pin-supported variants | Pixel baseline behavior | Smallest compatible extension / focused closure |
| --- | --- | --- | --- |
| U-L01 TypeScript | `.ts .mts .cts .tsx` | S: TS/TSX grammars; extension aliases present. | Verify all variants with scoped imports, JSX and overloaded/member calls. |
| U-L02 JavaScript | `.js .mjs .cjs .jsx` | S: JavaScript grammar, all variants mapped. | Verify JSX and module variants; no claim that TS/JS import resolution covers package aliases. |
| U-L03 Python | `.py .pyi` | S: `.py`; `.pyi` absent from `lang_of`. | Add stub variant through same grammar; fixture with overload/stub import. |
| U-L04 Go | `.go` | S: explicit walker/import resolver. | Verify package-local vs same-tail packages and receiver calls. |
| U-L05 Java | `.java` | S: explicit walker/import resolver. | Verify overload/class/member ambiguity and duplicate package suffixes. |
| U-L06 Kotlin | `.kt .kts` | U: no dispatch/grammar in baseline; graph Cargo manifest records older Kotlin crate/tree-sitter version incompatibility. | Verify a compatible grammar/version or binding before registration; generic fixture first, scoped extensions next. |
| U-L07 PHP | `.php` | S: generic grammar path. | Verify namespaces/import aliases; add scoped evidence, not guessed edges. |
| U-L08 Swift | `.swift` | S: generic grammar path. | Verify class/struct/enum/actor/protocol/extension attachment; extend walker as needed. |
| U-L09 R | `.R .r` | U: no dispatch/grammar in baseline. | Register grammar; plain functions, S3/S4/R6, roxygen exports, library/source imports; preserve uncertainty. |
| U-L10 Rust | `.rs` | S: explicit walker and resolver. | Verify `crate/self/super`, multi-crate roots, aliases, methods; fix ambiguous suffix fallback. |
| U-L11 C | `.c .h` | S: generic grammar path. | Verify declarators, function pointers, includes and preprocessor branches. |
| U-L12 C++ | `.cpp .cc .cxx .hpp .hh` | U: no dispatch/grammar in baseline. | Register C++ grammar; namespace/template/member/overload fixtures; do not parse as C. |
| U-L13 C# | `.cs` | S: explicit grammar/walker. | Verify namespaces, using aliases, nested/partial types and receiver uncertainty. |
| U-L14 Ruby | `.rb` | S: Ruby; additionally `.rake .gemspec .ru`. | Preserve extra variants and Rails fixture; verify require/load and singleton methods. |
| U-L15 Scala | `.scala .sc` | U: absent baseline dispatch. | Add existing generic-path grammar and object/trait/script fixtures. |
| U-L16 Elixir | `.ex .exs` | S: generic grammar path. | Verify defmodule/def/defp/pipelines, aliases and arity ambiguity. |
| U-L17 Solidity | `.sol` | U: absent baseline dispatch. | Add generic-path grammar, contract/function/inheritance/import fixtures. |
| U-L18 OCaml | `.ml .mli` | U: absent baseline dispatch. | Add grammar; implementation/interface/module and recursive binding fixtures. |
| U-L19 Zig | `.zig` | U: absent baseline dispatch. | Add grammar; functions/types/@import and call fixtures. |
| U-L20 Dart | `.dart` | U: absent baseline dispatch. | Add grammar; library/part/import, class/method and mixin fixtures. |
| U-L21 Clojure | `.clj .cljs .cljc .bb` | U: absent baseline dispatch. | Add grammar; namespace/defn/require alias and reader-conditional fixtures. |
| U-L22 Nix | `.nix` | U: absent baseline dispatch. | Add grammar; bindings/lambdas/import and attribute call uncertainty fixtures. |
| U-L23 Lua | `.lua` | S: generic grammar path. | Verify local/global functions, table methods, require and receiver distinctions. |
| U-L24 Vue container | `.vue` | U: no container dispatch baseline. | Reuse inner TS/JS extraction through existing graph owner; preserve source offsets for multiple script blocks, script setup and templates. |

Graft dispatch lowercases filenames in its generic/container paths; Pixel's
baseline exact extension match is case-sensitive. **U-L25:** decide/document
per-language filename-case policy and test uppercase variants explicitly;
do not extrapolate `.R` support from a generic lowercase comparison.

## Function-level product capability map

Every U row is retained work, not an optional omission. Existing functionality
must remain while its focused closure is implemented and verified.

| ID / capability | Existing owner → observed source behavior | Remaining gap → smallest compatible extension | Focused closure test |
| --- | --- | --- | --- |
| U-D01 Literal lookup | `pixel-index` query planner/verification; daemon `Service::handle/dispatch`; CLI `search_compat` → indexed regex API plus narrowly exact native-compatible searches. S/R: live source queries returned capped and complete results. | Verify cold/warm/dirty/deleted/unsupported-file boundaries; retain fallback instead of widening claims. | Temp repo: literal/regex/no-match/capped/CRLF/binary; compare supported native stream and exit exactly. |
| U-D02 Concept lookup | `concept::extract_concepts`, `concept_resolve`, `Service::op_resolve` → deterministic concepts and history-backed phrase resolution. | Language concept coverage differs from symbol coverage; query semantics must stay deterministic. | Label/error/string→owner fixture for each language, negative unmatched query, ambiguous concepts, capped pagination. |
| U-D03 Model-free retrieval contract | `pixel-recall` embedding backends, `recall_cmd`, README model setup/default features → existing ask/recall include learned embedding behavior. S. | Explicit current-contract drift. Inventory all embedding-dependent paths and replace retrieval/decision computation with existing lexical/concept/rank owners while preserving transcript and code lookup utility; no silent feature deletion. | Offline/no-model runtime for ask, recall, task-boundary path; lexical/concept success and meaningful unavailable/fallback cases; verify no model download/inference. |
| U-D04 File API | `Service::op_skeleton` (api.rs:1112), `GraphStore::symbols_in_file` → path-normalized indexed symbol/signature output. S. | Baseline collects all symbols; verify boundedness, ordering and path ambiguity rather than create another API. | Existing `skeleton_renders_signatures_without_bodies`; large file, Unicode, nested symbols, multiline signature, unknown file and JSON stream. |
| U-D05 Useful excerpts | `validated_context_source`, `context_item`, `render_context`; `pixel-context::render_crux` → hash-validated cached source, signatures, relationship items, bounded rendering and absolute Crux coordinates. S/R: focused and daemon tests verify current implementation. | Coordinate and stale-source leakage sub-gaps closed. Validation is conservative omission, not global warm-graph freshness repair; automatic incremental repair, broader language/duplicate-fingerprint coverage remain open. | Three focused context regressions plus full daemon suite: absolute coordinates, warm source change/shift, stale omission/lower bound, fresh reindex and budget behavior. |
| U-D06 Structural ranking | daemon `fan_in_counts`, `signals::score_signals`, `rerank::rerank` → graph degree normalized over candidates, weighted within existing tiers. S. | Preserve candidate and epistemic bounds; report evidence instead of treating importance as correctness. | Existing `fan_in_normalizes_over_candidate_set_and_ignores_external_hubs`, `fan_in_is_wired_into_rerank_and_never_promotes_across_tiers`; zero-edge fixture. |
| U-R01 Scoped imports | `imports::resolve_import/resolve_js/resolve_rust/resolve_python/resolve_go/resolve_java` → language dispatch; relative JS; path/suffix fallbacks elsewhere. S/R: this change makes `first_suffix_match` unique-only. | Baseline arbitrary suffix selection fixed and verified below; multi-crate `crate::` roots are still generic `src`/root. Scoped package roots and wildcard behavior remain open. | Four `import_resolution` tests cover Rust/Python/Java order-invariance, unique/exact paths and full/incremental graph uncertainty. Further closure: scoped roots and wildcard imports. |
| U-R02 Callers/callees and ambiguous receivers | `ResolveIndex::decide/decide_raw`, `resolve_calls/resolve_all/reconsider_resolved_calls`; `GraphStore::edges_to/edges_from` → real receiver Exact downgraded to Probable; ambiguous imports can remain unresolved; incremental reconsideration exists. S/R: source/impact context observed. | File/name resolution is not lexical binding/type proof, especially `self/this`, shadowing and imported aliases. Retain uncertainty; carry scope/binding evidence through current extraction/store. | Same-name methods in two classes, shadowed local/import, unknown receiver, alias, added conflicting symbol then incremental refresh. |
| U-R03 Traces and flows | `trace::trace`, `impact::impact`, `process::discover/list`, `cluster`; graph store edges → bounded relationship analysis with unresolved/lower-bound notes. S/R: `Service::op_uses` now counts unresolved outgoing calls for callee queries. | Callee-query false closed-world claim fixed and verified below; source presence is not full language/graph correctness. Other analyses still need directional uncertainty closure. | Uses outgoing/callers regression and old-daemon fallback verified. Remaining: cyclic graph, probable edge, unresolved outgoing impact/trace, depth cap and affected process membership. |
| U-R04 Compiler assistance | Graft `lsp::enrichWithLsp` adds optional typed evidence; no corresponding Pixel language-server resolution owner established in bounded audit. U. | Investigate existing protocol/subprocess owners, then optional graph evidence adapter with explicit provenance and timeout/failure fallback; not a parallel graph. | Fake language-server response fixture first; real installed server fixture later; no server, timeout, stale version and outside-root destination keep static graph honest. |
| U-C01 Monorepo scope | `discover_root`, graph `collect_files`, index `policy_walk` → one discovered root, shared ignore policy, bounded source collection. S. | Single-root traversal is not project-scoped federation. Define subproject identity/scope using existing index snapshots and rank inputs. | Sibling packages sharing filenames/symbols; scoped targets/search/context do not cross packages except evidenced relationships. |
| U-C02 Nested repositories | `policy_walk` prunes `.git`/`.pixel`, not proof of an explicit nested-repository boundary contract. S. | Establish follow/stop policy and report skipped/covered roots without silently treating parent freshness as child freshness. | Parent with nested repo, nested untracked/ignored state and separate HEAD; coverage/freshness matches declared policy. |
| U-C03 Submodules | `discover_root_follow_submodules`, `discover_root_impl` → optional climb to superproject. S. | Root discovery alone does not federate submodule indexes or gitlink revisions; compare Graft `workspace::readWorkspace/loadWorkspaceGraphs/federate*`. | Initialized/uninitialized submodule, dirty child, gitlink update and separate worktree; explicit partial coverage, no hidden mutation. |
| U-C04 Dirty/incremental state | `IndexSet`, base/delta/overlay, `Service::refresh_files`, graph `update_files`, `reconsider_resolved_calls` → existing incremental machinery. S/R: `changes` observed concurrent dirty test file. | Ensure snippets/Crux/imports/annotations share the changed snapshot; reuse parse/source work, do not refresh whole repo for every metric or excerpt. | Add/edit/delete/rename source; imports and callers relink, removed calls disappear; snapshot is correlated across outputs. |
| U-C05 Durable human annotations | `GraphStore::set_annotation/get_annotation/delete_annotation/annotations_for_file/annotations_for_symbols/annotations_for_norms`; daemon/task consumers → persistent notes separate from extracted facts. S. | Verify lifetime and accessible edit/export surface; notes are human data, never authoritative instructions or learned summaries. | Set note→full rebuild→incremental rebuild; symbol rename/file move/delete orphan handling; resolve/targets/task show useful scoped note. |
| U-T01 Task packets | `task_runtime::upsert_claude_task/read_claude_packet`, `render_context`, `prompt_submit::render_task_context` → bounded session/task packet. S. | Preserve task generation and evidence freshness; do not convert ranked paths into permissions. | Two sessions, task reset, scope/branch change, cap and empty repository; no stale packet becomes current evidence. |
| U-T02 Compaction recovery | `post_compaction::run`, prompt-submit context/targets state → restore active task context. S. | Hook source vs active registration vs observed host delivery are separate; respect deliberate minimal installer. | Protocol-shaped hook fixture with active packet and stale/missing packet; actual supported host delivery remains separate live gate. |
| U-T03 Historical discovery/recovery | `FactsStore::excavate`, facts search/lifecycle, CLI `rescue_cmd`, `pixel-ops::provenance` → indexed history and guarded recovery planning. S. | New retrieval must feed evidence/snapshot/caps through history without bypassing rescue safeguards. | Existing `excavate_rescue_v2`, `rescue_cli`, provenance fixtures: deleted phrase→past symbol→plan; dirty/ambiguous restore refused. |
| U-T04 Error correlation | `pixel-session::query::{last,since,show,search}`, session store and rank error signals → structured errors/cursors and queryable context. S. | Correlate errors, edits, invocation and snapshot where metadata already exists; don't invent a global latest-operation linkage. | Concurrent operations with distinct failures/cursors; query right source and request; unavailable sink is nonfatal. |
| U-T05 Proven flow reuse | `pixel-flow::replay`, store revision APIs and `execute` → saved flow validation/rendering/execution path. S. | Retain replay and variables; evidence of a stored flow is not proof it still executes against changed UI/auth. | Real-shape saved flow with required variable, revision, missing locator and drift; report blocked step without leaking credentials. |
| U-S01 Worker acceptance | `claude_controller::decide_handoff/build_worker_command_with_options`, `task_runtime::accept_task/prepare/transition` → narrow acceptance and durable task state. S. | Keep genuine ambiguity/launch failure in foreground; task plan validation is not agent intent inference. | Explicit imperative accepted only after candidate+worker exists; question/review/plan/empty repo/launch failure not rejected from foreground. |
| U-S02 Isolation/ownership | `task_sandbox::create/load/inspect` → task worktrees, tracked snapshot overlay, ownership checks. S. | Preserve unrelated WIP and credential/untracked refusals, no targets-as-allowlist shortcut. | Dirty tracked source preserved; untracked credentials refused; escaped path/symlink/out-of-ownership edit cannot promote. |
| U-S03 Scheduling/promotion | `task_scheduler::start/start_race/poll_race/status/stop`; `task_sandbox::promote/cleanup` → bounded registered workers and on-disk compare/apply. S. | Baseline README explicitly says no dedicated worker log sink/automatic health retry policy. Extend current ledger/scheduler, never trust model completion alone. | Competing eligible/invalid/no-op candidates, primary overlap drift, worker crash, race cap, idempotent cleanup; winner diff remains owned. |
| U-S04 Review/publication | `pixel-ops::{inspect,review,reconcile,publish,push,ship,branch,update,sync,rewrite}` → guarded Git state transitions. S. | Keep explicit approval/ownership/request IDs and linear-history policy; retrieval changes must not bypass mutation gates. | Existing publish properties, reconcile/rewrite matrices and CLI temp remote fixture; no implicit push/merge from analysis. |
| U-S05 Crash recovery | `PublishRecoveryStore::{write,read,has_pending}`, `capture_index_snapshot/restore_snapshot`; operation journal/lock/snapshot owners → durable recovery phases. S. | Extend only needed recovery metadata; metrics/reporting failures cannot modify journal semantics or exit status. | Existing `crash_matrix` plus interrupted metrics/log write around publication; restored index/ref and replay request identity. |
| U-I01 Automatic preparation | CLI execute path, daemon `Service::ensure_graph`, `ready`, index locks/watcher → lazy graph/index preparation and in-process fallback. S/R: disposable Graft checkout graph built on first targets query. | Verify first-use behavior with current installed prompt/wrapper paths; no artificial latency gate. | Cold repo→real search/context, daemon unavailable/incompatible, concurrent build lock; answer stays honest and fallback succeeds. |
| U-I02 Native context delivery | `pixel-install::install` baseline calls only `deploy_agent_prompt` and `install_shell_wrappers`. Source hooks/providers remain elsewhere. S; this session synchronizes README/ARCHITECTURE and adds exact-line metrics relay guidance to the installed prompt. | Documentation/installer checks do not establish host live delivery. Add only needed supported delivery; do not reactivate dormant routing wholesale. | Coordinator-observed installer tests below; real supported-host metrics/chat relay remains an independent gate. |
| U-I03 Post-edit information | `guard::run_post_tool_use/post_tool_use_blast_radius/post_edit_snapshot_note` → snapshot-based separate cross-file/same-file counts, up to 8 dependent paths, explicit freshness/uncertainty caveat and one advisory payload. S/R: five new hook tests plus existing hook/CLI regressions verified. | Misleading “elsewhere” count and non-actionable note sub-gaps closed. Source hook fixture success is not proof of active installation/live host delivery; current-source graph repair remains open. | Five focused tests plus 22 existing CLI and 70 guard unit tests; preserve no-graph/non-edit/host-stream fallback and bounded path output. |
| U-I04 Operation metrics | `pixel-actionlog::ActionLog`, `metrics`, CLI `operation_metrics` and log/savings paths → correlated final records, stderr metrics, versioned workflow estimates. S/R: actual `🟩 Pixel` line observed with partial estimate on uncertain uses. | Implemented accounting is explicitly scoped to CLI-owned rendered streams, excluding lower-level library/subprocess writes; not a whole-process byte measurement. Exact chat relay remains host-dependent; estimates are partial/unavailable when evidence requires it. | 18 actionlog and 11 CLI tests cover accounting/legacy/negative/partial/concurrency/disable/errors/stream contracts and safe publish replay/refusal; supported host end-to-end relay remains open. |

## Every advertised Graft agent: explicit delivery closure

Source: pinned `src/hosts/registry.ts::HOSTS`, `plan.ts::planInit`, plus README.
The generic `agents` target covers **Codex and OpenCode**, not a demonstrated
native hook for every editor. Graft's MCP transport is not a Pixel requirement.
For each row, equivalent CLI/instruction utility is acceptable, but installation
and live delivery must be stated accurately. Parent metrics work may extend
the installed guidance; this table records the pre-change baseline.

| ID / host | Existing Pixel owner/behavior | Smallest extension and exact closure task |
| --- | --- | --- |
| U-A01 Claude Code | `install::deploy_agent_prompt/install_shell_wrappers`; Claude hook/controller source retained. | Verify prompt wrapper in real invocation; add only required supported mechanical delivery. Isolated HOME additive/idempotent test; distinguish protocol vs live acceptance/context/metrics. |
| U-A02 Codex | Same installer wrapper; Codex provider/guard/composition source retained. | Preserve trust and permissions; verify actual instruction/config interface before adding delivery, no fabricated trusted hashes. Exact hook JSON + supported-host relay fixture. |
| U-A03 OpenCode | Transcript source exists in recall; not proof of active install integration. | Investigate current instruction/CLI interface; add managed guidance in existing installer and test preservation/relay availability. |
| U-A04 Cursor | Existing provider/config/routing code is not invoked by minimal baseline install. | Reconcile supported native instruction/post-edit interface; isolated config fixture and live relay test, or explicitly document instruction-only delivery. |
| U-A05 Gemini CLI | Existing detection/provider/transcript machinery; baseline installer is minimal. | Same additive guidance/current-interface investigation and cold-preparation/metrics-relay fixture; no wholesale dormant-hook reactivation. |
| U-A06 AdaL | No active baseline install path established. | Investigate native skill/instruction support; use existing installer owner; preserve user's file and verify one exact-line relay where host permits. |
| U-A07 Grok | No active baseline install path established. | Investigate skill/instruction interface, add bounded managed guidance and isolated-home tests; host live verification separately. |
| U-A08 Hermes Agent | No active baseline install path established. | Investigate current instruction entry point; add smallest installer target, idempotent preservation fixture, report native channel limits. |
| U-A09 Google Antigravity | No active baseline install path established. | Investigate supported global/project instructions; bounded installer target, preservation/uninstall and supported relay tests. |
| U-A10 GitHub Copilot | No active baseline install path established. | Verify `.github/copilot-instructions.md` host semantics; managed block via existing installer, preserve user content, report live channel limitations. |
| U-A11 Kiro | No active baseline install path established. | Verify steering interface; managed guidance, additive/uninstall fixture and host relay if supported. |
| U-A12 Windsurf | No active baseline install path established. | Verify rules interface; managed guidance, additive/uninstall fixture and host relay if supported. |

Pixel's additional advertised **Devin, zcode and pi** are retained U-A13–15:
their existing provider/config/recall paths require the same current-installer
reconciliation and supported-host checks. Removing them to match Graft would
violate preservation. **Warp** remains an explicitly documented instruction/CLI
fallback, not an invented native hook.

## Prioritized execution and closure ledger

1. **Evidence honesty first:** U-R01 unique suffix/scoped imports, U-I03 truthful
   post-edit facts, U-D05 excerpt coordinates and budgets. Small changes in
   existing owners, immediate unit→CLI fixture verification.
2. **Live metrics:** U-I04 in the existing actionlog/CLI; measured facts separate
   from estimates. v1 byte approximation and assumptions must be documented;
   native/JSON/protocol streams stay unchanged. Host relay cannot be guaranteed
   solely by a printed line.
3. **Complete deterministic coverage:** U-L01–25 language/variant fixtures,
   U-R02/R04 scope/compiler evidence, U-C01–05 repository/annotation coverage,
   U-D03 model-free retrieval migration. These are concrete unfinished tasks,
   not “optional” capabilities or implied completion from this document.
4. **Close integration/documentation drift:** U-I01–03 and U-A01–15, preserving
   minimal installation intent and each host's permissions. Re-run the changed
   real host path before reporting active delivery.
5. **Preserve complete workflow:** U-T01–05 and U-S01–05 focused regression
   fixtures, then installed CLI task→evidence→review/safe publication scenario
   against a temporary local repository/remote. Atomic reinstall, index/config
   refresh, `pixel install`, `pixel doctor` belong to final repository gate.

No broad closure here is satisfied merely by a source inspection, passing
syntax/type check, existing test name, fresh index, green doctor registration,
report recommendation, or Graft's claimed benchmark result.

## Session verification evidence

- `pixel status` and `pixel inspect` observed the baseline/index mismatch and
  initially clean branch; a later `pixel changes` observed concurrent installer
  test work rather than discarding it.
- `pixel search` exercised real source queries and returned explicit row caps;
  `pixel context <uid>` rendered actual `lang_of`, import resolution, install,
  post-edit and other source-backed functions/relationships.
- Disposable pinned Graft `pixel targets` completed graph preparation and
  returned scoped source owners; this demonstrates discovery, not Graft runtime
  behavior or language parity.
- **UNVERIFIED:** All U tasks remain open unless a later implementation entry
  supplies the exact executed fixture/CLI command and observed result. This
  document is non-visual and does not itself change runtime behavior.

### Implementation closure recorded in this session

The following closes specific sub-gaps, **not the entire product plan**:

- `first_suffix_match`: scratch Rust copy reproduced 3 arbitrary resolutions
  (Rust/Python/Java, exit 101); unique-only candidate returned `None` for all 3
  (exit 0). Production focused tests:
  `cargo test -p pixel-graph --test import_resolution` → **4 passed**;
  `cargo test -p pixel-graph` → **75 passed / 6 suites**. Fixtures cover positive,
  negative, file-order invariance, full graph and incremental rebuild callers.
- `Service::op_uses`: actual CLI callee response falsely asserted
  `closed_world=true` with one stored unresolved outgoing call. The change
  counts unresolved calls by `enclosing_symbol_id`, retains caller behavior and
  supplies `unresolved_outgoing` plus an explicit boundary note.
  `cargo test -p pixel-daemon uses_ -- --nocapture` → **2 passed**;
  `cargo test -p pixel-daemon` → **30 passed / 3 suites**.
- Candidate CLI (`cargo build -p pixel-cli`, exit 0) was exercised with 6 real
  Rust/Python fixture files at
  `/var/folders/mc/r22b45r95t97x33__2x3t7pm0000gn/T/pixel-import-cli-m7v_fkgn`:
  `pixel graph <fixture> --json` → **6 files, 6 symbols, 0 edges, 2 unresolved**;
  `pixel uses rust_caller <fixture> --role callees --json` and equivalent
  `python_caller` query → **empty edges, unresolved_outgoing=1,
  lower_bound=true, closed_world=false**, exit 0. The CLI retained JSON stdout
  and emitted metrics separately on stderr; estimates explicitly said partial.
- Protocol bumped **7→8** for the uses honesty fix, then **8→9** for validated
  context snapshots. A
  protocol-7 test double on the real fixture socket observed exactly
  **`ping`, `shutdown`**; the current CLI with
  `PIXEL_DAEMON_AUTO_START=0` fell back in-process and returned the corrected
  callee boundary (exit 0). This exercises the actual `try_daemon_inner` guard,
  not only a version-constant comparison. The current source protocol is 9.
- Context closure (accounting/coordinator-observed): **3 focused context
  regressions**, full `cargo test -p pixel-daemon` → **33 passed**. Stored
  Crux offsets remain body-relative; rendering translates with checked
  arithmetic to absolute file lines. `validated_context_source` caches one
  bounded source read per distinct returned file, compares its xxh3 hash with
  `FileRow::blob_oid`, and derives excerpts from those same bytes. A changed,
  missing or invalid snapshot omits context and reports a named lower-bound
  boundary; if that warning cannot fit the budget, the operation errors rather
  than silently asserting freshness. A fresh graph fixture shows the shifted
  function at line 21 and Crux at line 23 with `return 2`. Global warm-graph
  freshness/incremental repair is **not** claimed fixed.
- Post-edit closure (delivery/coordinator-observed): **5 new hook tests**, **22
  existing CLI tests**, **70 guard unit tests**. Read-only snapshot aggregation
  distinguishes same-file from cross-file dependants, includes at most 8 paths
  and an explicit cap/freshness caveat. No full-repository refresh is performed
  merely to compose the note; hook-stream fixtures do not prove live host use.
- Current metrics/integration checks (coordinator-observed):
  `cargo test -p pixel-actionlog` → **18 passed**;
  `cargo test -p pixel-cli --test metrics_cli` → **11 passed**, including safe
  publish replay/refusal; installer suite rerun → **51 passed**.
  Actual CLI readback from the Rust fixture, exit 0, separately emitted:
  `🟩 Pixel · uses · 170.9 ms · ~304 output tokens · ~976 saved (workflow estimate, partial) · id=79323-18d43e84da69d298-0`.
  These are this invocation's measurements/estimates, not a performance claim.
  `output_scope` denotes **CLI-owned rendered streams**; library/subprocess
  writes are excluded and reporting bytes are separate. UTF-8 bytes/4 includes
  reporting overhead. Estimates use returned evidence and documented workflow
  assumptions, never unseen results, repository-size denominators or hidden
  reasoning; partial and unavailable comparisons remain visible.
- Stable regression command (observed in this lane):
  `rtk cargo test -p pixel-ops -p pixel-git -p pixel-index -p pixel-facts -p pixel-flow -p pixel-rank -p pixel-recall -p pixel-session`
  → **388 passed, 0 failed, 0 ignored / 29 suites**, exit 0, **158.285 seconds**
  wall time. Ignored count independently checked with `--ignored --list`.
  Logs: `/tmp/pixel-philosophy-stable-tests.log` and
  `/tmp/pixel-philosophy-stable-tests-ignored.log`.
- `rustfmt --edition 2024 --check` on the changed graph helper/test files →
  exit 0. `cargo clippy -p pixel-graph --all-targets -- -D warnings` was blocked
  by **2 existing `collapsible_if` errors** in `pixel-git/src/discover.rs:92–93`,
  outside this lane's edits. This is not a green lint claim.
- No root `INVARIANTS.md` exists (direct existence check); bounded invariant
  targeting points to `ARCHITECTURE.md`, protocol envelopes and guarded-op
  tests. No capability was removed, no daemon architecture was replaced, and
  no externally hosted model or paid evaluation was initiated for this lane.
  Full-workspace tests, atomic reinstall, index/configuration refresh and doctor
  are still **pending coordinator-owned gates** at this document snapshot.

### Final delta and remaining contract boundary

This change adds correlated local operation accounting, corrects ambiguous
import resolution and outgoing-call uncertainty, validates source-backed
context/Crux coordinates, makes post-edit notes actionable without overstating
freshness, and reconciles installation documentation/guidance. Existing CLI,
daemon, graph/rank/context owners and guarded task/Git workflows remain.

Still open: complete language/variant and host coverage; model-free replacements
for existing embedding-dependent retrieval; scoped imports/compiler evidence;
nested-repo/submodule federation; global graph freshness/automatic repair; and
observed live host context/metrics relay. The U rows retain their focused
closure tasks; no broad parity or whole-plan completion is asserted here.

### Final installed consistency correction

`pixel index --history .` exposed a pre-existing CLI display error: the daemon
returns nested `index` counters, but the CLI looked up literal dotted keys and
printed zeroes. `run_command` now uses JSON pointers for those three counters.
`metrics_cli::daemon_reindex_reports_actual_nested_index_counts` exercises the
real warm-daemon path and compares its reported counts to `pixel status --json`.
This changes reporting only, not indexing or graph mechanics. The installed
index itself was verified populated (249 base files and 15 overlay files before
publication), not inferred from the incorrect message.

### Coordinator final verification gate

- `cargo test --workspace`: 830 passed, 0 failed/ignored, 57 suites, exit 0
  (before the final index-display correction; no parity inference).
- Exact final source: `cargo test -p pixel-cli -p pixel-actionlog -p pixel-daemon
  -p pixel-graph -p pixel-install -p pixel-context`: 394 passed, 0 failed/ignored,
  24 suites, exit 0. Includes 12 real metrics CLI tests.
- `cargo check --workspace`: exit 0; focused CLI clippy with `-D warnings`: clean.
  Dependency-inclusive strict clippy previously exposed existing unrelated
  pixel-git/context warnings; it is not represented as a fully clean gate.
- Final release build: exit 0, no warnings; atomically installed binary SHA256
  `7791b5efe4bad77aa1e8413933b0192afc8059c75d68184f8c5c2d2c96605517`.
- Required index/history and config/install refreshes executed in parallel;
  both succeeded. Reindex reported 249 base / 0 delta / 15 overlay files before
  publication, matching immediate JSON status; history 315/315 commits fresh.
- Installed CLI temporary-repository scenario: 7 assertions passed—real daemon
  reindex counts; absolute Crux lines; unresolved callee lower bound; changed
  source never shown as current stale excerpts; one valid post-edit JSON
  advisory; identical search JSON with metrics on/off; task→evidence→review→
  publication with idempotent replay and stale-HEAD refusal.
- `pixel doctor --json`: 11 green, 0 yellow, 0 red.
- Non-visual change; no browser, external telemetry, paid evaluations or
  competitive benchmark campaign. Installed host instructions and mock-wrapper
  delivery are verified; automatic obedience by every host/model is not.

These results close the specified implemented subchanges, not the remaining
U-ledger contracts. In particular, full language/host parity, model-free
retrieval migration, compiler assistance, repository federation and broader
freshness/continuity work remain explicit unfinished tasks.

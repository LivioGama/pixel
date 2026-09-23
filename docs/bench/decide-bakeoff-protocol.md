# decide-bakeoff protocol — small open decision models on Pixel's surfaces

Frozen before the first measurement run. The question: **is there an
off-the-shelf ≤500M local model that beats Jev 1.13 on coding decisions**, and
if not, which one is closest on Pixel's own decision surfaces? No training, no
4B-class model, no Jev API key — Jev numbers below are its published JevBench
rows, labelled non-reproduced.

## Why this exists

`pixel classify` already answers in Jev's shape (state + bounded labels +
criteria → probability distribution) — at the time of this protocol, using a
zero-shot static embedding
(Model2Vec potion-128M, cosine + fixed-temperature softmax; that local
backend was removed when classify went remote-only). Measured on
JevBench v1.3 public items: Intelligence 38.28, score 41.34
(`docs/bench/jevbench.md`). The published board (v1.2.16) shows Jev's coding
topic at 0.839 accuracy and no ≤500M open local model above 0.68. This pilot
measures whether that gap is real on *our* surfaces — and whether the
unmeasured `gavel-base` checkpoint changes it.

## Candidates (all open weights, CPU-runnable)

| Key | Checkpoint | Params | Decision shape | Published coding acc (v1.2.16, n=56 all tiers) |
|---|---|---|---|---|
| `pixel-static` | minishlab/potion-multilingual-128M via `pixel classify` | 128M | cosine+softmax | not on board (score 41.34 v1.3) |
| `gavel` | chukfinley/gavel-base | 150M | NLI entailment head, option→hypothesis | unmeasured |
| `verdict` | heman10x/rlcd-modernbert-151m (a.k.a. openjev-verdict-1.4 lineage) | 151M | GLiClass one-pass option scorer + abstention | 0.518–0.554 |
| `laya` | convaiinnovations/laya | 421M | [MASK]-per-option scorer | 0.589 |
| `gliformer` (jeff) | knowledgator/gliformer-large-v1 | ~400M | per-option span scorer | 0.679 (best ≤500M) |
| `openjev-deberta` | com-kotobalabs/open-jev-deberta-v3-large | 435M | [mean(q); mean(opt); product] head | 0.464 |
| `gte-reranker` | Alibaba-NLP/gte-reranker-modernbert-base | 149M | cross-encoder pair score → softmax | n/a (reranker class; intel 34.3 overall) |

Excluded and why: SemIf/fastjev & Qwen3.5-4B family (4B — out of scope),
djev / classifier.dev / decision-machine-1 (hosted APIs), kev/jqv/decider/smalljev
(≥0.5B decoders needing a serving stack, none above 0.68 anyway),
LambdaMART/LightGBM and teacher distillation (require training — out of scope).

## Evaluation sets

1. **coding-public**: the 14 public JevBench items whose `topics.json` topic is
   `coding` (2× adequacy noul, 4× routing choice, 1× long_policy score,
   1× trap choice, 6× judge_hard noul). Frozen as
   `scripts/fixtures/decide-coding-subset.jsonl` with sha256 recorded at freeze.
   Small — every metric carries its CI; this set anchors comparability to the
   published board, it does not by itself prove anything.
2. **plan-routing**: `scripts/fixtures/plan-routing-real-gold.jsonl` (47 real
   GitHub issues, gold multi-label sets over 5 PlanQuery kinds). Decomposed to
   Jev shape as five independent `noul` questions per item — one per label —
   using the label descriptions from `crates/pixel-graph/src/plan.rs`
   (`SOURCE_HASH` checked at run time, like the existing pilot). Exact-set
   match requires all five binary decisions correct; per-label FP/FN and a
   weighted omission cost (2×FN) reported as in `bench-plan-routing.py`.

Baselines: `pixel-static` (the then-shipped zero-shot backend, since
removed) on both sets;
published Jev 1.13 coding-topic accuracy 0.839 (n=56 incl. non-public items —
marked **not reproduced**, and the 14-item public subset is *not* the same
denominator, so Jev-vs-candidate on it is indicative only).

## Metrics (same discipline as the plan-routing pilots)

Per set and model: accuracy / exact-set, macro-F1, Brier, ECE (10-bin),
risk–coverage at min-confidence {0.5, 0.7, 0.9}, resident-model latency
p50/p95 (model loaded once, serial requests, idle machine), cold artifact
size, peak RSS, license. Bootstrap 95% CI on the accuracy delta vs
`pixel-static` (2,000 resamples stratified by family where families exist).

## Pass gates (fixed in advance)

- **Beats-Jev gate**: candidate coding-subset accuracy whose CI lower bound
  clears 0.839 — expected to fail on n=14; reported for honesty, and the
  real decision input is plan-routing + the full-suite sanity run.
- **Integration gate**: beats `pixel-static` on plan-routing exact-set with
  CI excluding 0, p95 resident latency ≤ 500 ms on CPU, artifact ≤ 2 GB on
  disk, OSI license.
- A model passing integration but not beating-Jev is reported as
  "best local option, below Jev" — not as a win.

## Method

- Each model loads once per process and answers the identical JSONL stream;
  the spec shape is `pixel classify`'s (`text`, `context`, `labels`,
  `criteria`) — adapters map to each model's native input and map its output
  back to `{label: probability}` covering the full label set. A model may not
  see gold labels or adapt its prompt per item.
- Verdict's abstention slot mass is dropped and the remainder renormalized
  (the author's own rule for bounded label sets, as upstream's
  `verdict_local` adapter does).
- Score-type criteria map list index → label string; noul maps
  `criteria["true"/"false"]` → `yes/no` labels, matching `pixel_local`.
- Model revisions are pinned by SHA-256 of the resolved snapshot; the harness
  records script, manifest, and dataset hashes in `manifest.json`.
- Latency is wall-clock per decision, model resident, single thread batch 1;
  first (warm-up) decision excluded from percentiles.

## What this pilot does not claim

- Comparability to the JevBench Score (that needs the maintainer-run 534-item
  set and their latency/cost conventions).
- Anything about ranking/retrieval quality — reranker candidates here are
  evaluated only on the decision task they were scored on upstream.

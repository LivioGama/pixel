# JevBench — Pixel as a measured decision system

[JevBench](https://benchmarkheaven.com/jev-models) ranks Jev-class decision
models: hand them a state and a bounded set of options, they return a typed
probability distribution. `pixel classify` answers in that shape — not as a
trained decision model, but as a deterministic zero-shot op over the static
embedding model Pixel already ships. The upstream harness is
[fstandhartinger/jevbench](https://github.com/fstandhartinger/jevbench); the
adapter is `pixel_local`.

## How the score is built (v1.3)

    Score = (Intelligence · Calibration · Speed · Cost)^(1/4) · m
    m = 1 when Intelligence >= 50, else (Intelligence / 50)^2

Intelligence is **accuracy above uniform guessing**, not raw accuracy:
`100 · (acc − chance) / (1 − chance)`, tier-weighted (easy .14, standard .28,
judge .28, hard .30) with absent tiers renormalised. The published tier
chances are easy .284, standard .317, judge .292, hard .336.

Below 50 the near-chance multiplier is quadratic, so the last points before
the cliff are worth more than everything above it: holding this op's
calibration, speed and cost fixed, Intelligence 46 scores ≈ 60 and
Intelligence 50 scores ≈ 72.

v1.2 scored raw accuracy with no such multiplier, so a number computed under
it is not comparable to the board. The first measurement of this op scored
68.6 under v1.2 and **8.48** under v1.3 — same run, same accuracies.

## Method (fixed before the run)

`pixel classify --jsonl` keeps the embedding model
(`minishlab/potion-multilingual-128M`, Model2Vec) resident and answers one
JSONL spec per line:

One request line (`original-policy-01-0`, reflowed here for width — the wire
format is one line):

```json
{"text": "Policy: refunds require a receipt and purchase within 30 days. A customer bought 12 days ago but has no receipt. Issue a refund.",
 "context": "Under the stated policy, is the requested action permitted? Treat unproved required conditions as not satisfied.",
 "labels": ["no", "yes"],
 "criteria": {"no": "A condition is missing or a prohibition applies.",
              "yes": "Every required condition is established and no prohibition applies."}}
```

and the answer it gets back:

```json
{"ok": true, "marker": "complete", "predicted": "no",
 "probs": {"no": 0.6786325892998001, "yes": 0.32136741070019986},
 "epistemics": {"closed_world": false, "lower_bound": false, "confidence": "complete",
                "basis": "zero-shot embedding similarity (cosine + softmax), not a trained classifier"},
 "snapshot": {"model": "minishlab/potion-multilingual-128M", "temperature": 0.07,
              "labels": ["no", "yes"]}}
```

Cosine similarity per candidate, softmax at a fixed temperature (`TAU =
0.07`, CLIP's value — never tuned on benchmark items). noul criteria map
`yes → criteria["true"]`, `no → criteria["false"]`; score criteria map the
list index to the label string. A label with no criterion embeds as its own
name.

**The instructions go in `context`, never in `text`.** They are identical for
every item of a family and say nothing about which label is right. Prefixing
them to the state changes and dilutes the query; `context` instead leaves the
state query untouched and replicates the framing into each candidate before
that candidate's own criterion. This is not mathematical cancellation. The
placement and component caps are documented in `crates/pixel/src/classify.rs`;
the historical measured difference is the table below.

## Public-item results (231/534; the judge tier and the held-out halves are maintainer-side)

Historically measured on an Apple M4 Max, model resident, serial, through the
unchanged upstream runner and v1.3 scoring. These figures describe those two
recorded runs, not every later branch head. Cost is the encoder-class estimate
carried by the submitted row ($0.005/M input tokens, chars/4), unchanged
between the two runs.

| | instructions in `text` | instructions in `context` |
|---|---|---|
| easy (48) | 31 — 64.6 % | **43 — 89.6 %** |
| standard (72) | 26 — 36.1 % | **39 — 54.2 %** |
| hard public (111) | 50 — 45.0 % | **53 — 47.8 %** |
| ECE (hard, 10 bins) | 0.2068 | **0.1148** |
| raw p50 / p95 | 0.344 / 2.818 ms | 0.392 / 2.948 ms |
| Intelligence | 19.51 | **38.28** |
| Calibration | 58.63 | **77.04** |
| Speed | 96.30 | 96.29 |
| Cost | 87.25 | 87.25 |
| **JevBench Score (v1.3)** | **8.48** | **41.34** |

Nothing else changed: same binary, same model, same temperature, same items,
both runs back to back on an otherwise idle machine. Candidates carry the
instructions now, so each candidate embed is a little longer, and the two
latency distributions are indistinguishable at this scale — Speed moves by
0.01 points.

Calibration was not a separate intervention: ECE moved from 0.2068 to 0.1148
in the same placement comparison. The measurements establish that observation,
not a causal explanation for how query placement changed the score gaps.

## Reproduce

The `pixel_local` adapter is in closed upstream
[PR 21](https://github.com/fstandhartinger/jevbench/pull/21), not the upstream
checkout. Pin the harness to v1.3 commit `75e6224ed8103bbc3485ca74820a2eaf7ce8abe0`
and acquire the adapter's immutable commit patch. PR 21's own base/head predates
v1.3 scoring, so checking out that head alone would select the wrong scorer.
Applying the patch locally does not reopen or submit that PR. Start in the
Pixel checkout, then choose a fresh path for the harness clone:

```bash
cargo build --release -p pixel-cli
PIXEL_BIN="$PWD/target/release/pixel"
git clone https://github.com/fstandhartinger/jevbench /path/to/jevbench
cd /path/to/jevbench
git switch --detach 75e6224ed8103bbc3485ca74820a2eaf7ce8abe0
gh api repos/fstandhartinger/jevbench/commits/5d9434cf48fcd5c83a3f02c335a846355468c0d3 \
  -H 'Accept: application/vnd.github.patch' > /tmp/jevbench-pr21.patch
git apply --check /tmp/jevbench-pr21.patch
git apply /tmp/jevbench-pr21.patch
```

Before running, edit `jevbench/adapters/pixel_local.py::build_request`:
send the item state alone as `"text": state` and add
`"context": q["instructions"]`, retaining its labels/criteria mapping.
The original adapter prefixes instructions onto the text; applying the patch
alone does not reproduce the context column above. Install the harness's
Python dependencies as its README specifies.

Use the same shell so `PIXEL_BIN` still points to the built classify-capable
binary, not an installed release without this command. Use fresh result and
raw-output paths on every run; run latency measurements on an idle machine
and do not pipe the runner into `head`.

```bash
TASKS=datasets/public/easy.jsonl,datasets/public/original.jsonl,datasets/public/hard.jsonl
python -m jevbench.cli run \
  --tasks "$TASKS" --adapter pixel_local --endpoint "$PIXEL_BIN" \
  --results out-context.jsonl --raw-dir raw-context/
python -m jevbench.cli summarize --tasks "$TASKS" --results out-context.jsonl
```

Interpret scores with upstream `jevbench/composite_v13.py`, not v1.2 math.
Patch checking and application were verified on these pinned revisions; both
the adapter and v1.3 scorer are present. These are reproduction pins, not
claimed revision identities for the historical measurements. No new benchmark
run accompanies the input-validation and test-harness changes.

## Where this can and cannot go

Every system on the board above Intelligence 50 runs an LLM or a fine-tuned
decision model. The best zero-shot encoder measured is OpenDecision
(ModernBERT-large) at 40.8; the best fine-tuned small encoders, Laya
(ModernBERT-large 421M) and jeff (GLiFormer 400M), reach 45.8 and 46.9. With
the placement above, static embeddings sit inside that band, and the band's
ceiling is below the cliff.

The hard tier is why. Its states are policy documents — 1.6 kB median,
15 kB at p100 — and its candidate criteria can differ by one number
(`pay_subject_to_10000_sublimit` against `pay_subject_to_15000_sublimit`).
A mean-pooled vector cannot represent that difference, and no placement
rule recovers it. Going past this band needs a model that reads the state.

A self-measured row is `partial: true` and is listed without a rank; ranking
requires the maintainers to run all 534 frozen decisions on their side.

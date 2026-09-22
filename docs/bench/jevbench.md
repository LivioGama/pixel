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
70.2 under v1.2 and **8.5** under v1.3 — same run, same accuracies.

## Method (fixed before the run)

`pixel classify --jsonl` keeps the embedding model
(`minishlab/potion-multilingual-128M`, Model2Vec) resident and answers one
JSONL spec per line:

```json
{"text": "<state>", "context": "<instructions>", "labels": [...],
 "criteria": {label: description}}
→ {"ok": true, "probs": {label: p}, "predicted": label}
```

Cosine similarity per candidate, softmax at a fixed temperature (`TAU =
0.07`, CLIP's value — never tuned on benchmark items). noul criteria map
`yes → criteria["true"]`, `no → criteria["false"]`; score criteria map the
list index to the label string. A label with no criterion embeds as its own
name.

**The instructions go in `context`, never in `text`.** They are identical for
every item of a family and say nothing about which label is right, so
mean-pooled into the state they dilute the only part that varies; carried by
every candidate they cancel. The mechanism is in the module docs of
`crates/pixel/src/classify.rs`, and the size of the effect is the table below.

## Public-item results (231/534; the judge tier and the held-out halves are maintainer-side)

Measured on an Apple M4 Max, model resident, serial, through the unchanged
upstream runner and v1.3 scoring. Cost is the encoder-class estimate carried
by the submitted row ($0.005/M input tokens, chars/4), unchanged between the
two runs.

| | instructions in `text` | instructions in `context` |
|---|---|---|
| easy (48) | 31 — 64.6 % | **43 — 89.6 %** |
| standard (72) | 26 — 36.1 % | **39 — 54.2 %** |
| hard public (111) | 50 — 45.0 % | **53 — 47.8 %** |
| ECE (hard, 10 bins) | 0.2068 | **0.1148** |
| raw p50 / p95 | 0.49 / 2.89 ms | 2.61 / 16.20 ms |
| Intelligence | 19.51 | **38.28** |
| Calibration | 58.63 | **77.04** |
| Speed | 96.29 | 95.48 |
| Cost | 87.25 | 87.25 |
| **JevBench Score (v1.3)** | **8.48** | **41.26** |

Nothing else changed: same binary, same model, same temperature, same items.
Candidates carry the instructions now, so each candidate embed is longer —
that is the whole latency difference, and it costs 0.8 points of Speed.

## Reproduce

```bash
git clone https://github.com/fstandhartinger/jevbench
cd jevbench
python -m jevbench.cli run \
  --tasks datasets/public/easy.jsonl,datasets/public/original.jsonl,datasets/public/hard.jsonl \
  --adapter pixel_local --endpoint "$(which pixel)" \
  --results out.jsonl --raw-dir raw/
python -m jevbench.cli summarize --tasks <same> --results out.jsonl
```

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

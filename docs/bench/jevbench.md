# JevBench — Pixel as a measured decision system

Pixel is on the [JevBench](https://benchmarkheaven.com/jev-models) board via a
`pixel_local` adapter in the upstream harness
([fstandhartinger/jevbench](https://github.com/fstandhartinger/jevbench)).
The system under test is `pixel classify` — not a trained decision model, but
a deterministic zero-shot op in the same output shape: a probability
distribution over a bounded label set.

## Method (fixed before the run)

`pixel classify --jsonl` keeps the embedding model
(`minishlab/potion-multilingual-128M`, Model2Vec) resident and answers one
JSONL spec per line:

```json
{"text": "<instructions>\n\n<state>", "labels": [...], "criteria": {label: desc}}
→ {"ok": true, "probs": {label: p}, "predicted": label}
```

Cosine similarity per criterion, softmax at a fixed temperature
(`TAU = 0.07`, CLIP's value — never tuned on benchmark items). noul criteria
map `yes → criteria["true"]`, `no → criteria["false"]`; score criteria map the
list index to the label string.

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

## Public-item results (231/534; judge tier and held-out halves are
maintainer-side only)

| Tier | Accuracy |
|---|---|
| easy (48) | 64.6 % |
| standard (72) | 36.1 % |
| hard public (111) | 45.0 % |

Latency raw p50 **0.32 ms** / p95 **2.4 ms** (Apple M4 Max, model resident);
calibration ECE 0.207, fidelity 68.7 on the 10 public probability items →
calibration 63.6. JevBench Score ≈ **68.6** on the available axes — a partial
row until the maintainers run the held-out items.

Submission: https://github.com/fstandhartinger/jevbench/pull/21

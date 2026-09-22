# Pre-prediction gold adjudication and limitations

This addendum was written after fitting the sole candidate and before running
any test prediction. Keep the original protocol and fit hashes unchanged.

The fresh-context corpus author had no access to the training rows or candidate
outputs. It supplied 40 English requests / 20 two-row families with source-line
rationales. Parent review accepts these labels from query semantics. Neither
reviewer is a human domain adjudicator; these are agent-authored synthetic
judgments, not executable proof or production traffic. Existing operational
Rust tests support what each query returns, not the preferred routing policy.

## Two findings that limit the experiment

1. The auditor independently labels four requests (`F08-A/B`, `F20-A/B`) with
   `by-concept` plus a specialized query. This is a legitimate request for two
   outputs. The frozen baseline and candidate both use exclusive concept
   fallback and cannot express these sets. Keep the gold unchanged. Correct
   the evaluator's overly restrictive gold validator, not the model. Maximum
   possible exact-set accuracy for these output shapes is 36/40. This changes
   the protocol's gold assumption; the original model/threshold remain frozen.
2. Independent authoring did **not** yield independent semantic families.
   F01-F04 share the core query-definition scenarios with training;
   F09/F10 share the two trained multilabel combinations; F11-F18 share quoted
   intent, negation, linker/priority/button homonyms, local bugs and targeted
   parser refactoring. F05 and F19 are also conservatively quarantined because
   named-code location and removal recur in training. Do not call all 40 rows
   a repository/family-held-out test, despite unique family identifiers.

Treat all 40 rows as an independently authored **diagnostic challenge**. Keep
32 overlapping-theme rows out of any held-out generalization interpretation.
Separately report F06, F07, F08, F20: eight rows/four novel label-combination
families absent from training. Both members stay together; four of these eight
have unrepresentable concept-plus-specialized gold. This is a deliberately
conservative compositional holdout, not a repository holdout. It is too small
for adoption. Do not tune, retrain, relabel, delete difficult rows or add a new
variant after seeing scores. The preregistered adoption screen is not available
as confirmatory evidence under this corpus limitation, whatever the scores.

Cross-split normalized duplicate rejection and nearest character/token-similarity
records remain useful diagnostics, not certificates of semantic independence.
All rows name the same Pixel source revision; no claim about unseen repositories,
forks, French requests, file relevance or downstream task success is possible.

## Infrastructure correction before test

The first fit attempt stopped before imports/training: macOS rejected
`setrlimit(RLIMIT_AS, 4 GiB)` with `ValueError: current limit exceeds maximum
limit`. The retry uses a 50 ms watchdog of process peak RSS and elapsed time,
plus a 900-second CPU hard limit. Sampled RSS enforcement can overshoot between
checks; actual recorded peak remained well below the bound. The successful
fit completed before test text was inspected. Original failure logs are retained.

The evaluation script was changed only to permit independently judged multilabel
concept gold, verify the frozen model digest independently of evaluator changes,
and add the predeclared strata/composition summaries. Baseline, candidate fit,
probability decoding, feature extraction and thresholds are unchanged. Retain
the original fit-time script and protocol in the raw artifact directory and
verify these function ASTs agree before execution.

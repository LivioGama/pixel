# Measurements That Can Be Trusted

Always loaded: what a number has to carry before it counts as evidence.

Every rule below was written after a claim that was wrong while looking
right. None of them is about being careful; each names a check that takes
seconds and would have caught the case beside it.

- **Read a run's identity before citing it.** `gh run view <id> --json
  headSha,displayTitle,event` — the job names are identical across pull
  requests, so a green run proves nothing until its head is yours. A run was
  quoted as "my branch merged with #207" while it belonged to an unrelated
  pull request — one adding a `classify` command, still in review — and the
  conclusion drawn from it ("the gate passes now, nothing to do") was false.
- **Report the number, never the colour.** "Mutants is green" is not a
  result; "107 tested, 79 caught, 28 unviable, 0 missed, verdict `tested`"
  is. A gate can pass having tested nothing at all — `scripts/mutants-gate.py`
  exists because that happened, and calls the two cases `not-applicable` and
  `vacuous`. The count is the only part of a green check that carries
  information.
- **Quote the command beside the number.** A count nobody can re-derive is a
  rumour. `git diff origin/main...HEAD` (three dots, the merge base) and
  `git diff origin/main` (two dots) differ by every commit the base gained
  since you branched: the two-dot form silently mutates other people's code
  and inflates the count, which reads as extra diligence.
- **An ablation changes one input, and you say which.** Renaming a function
  also moves the tokens of its name; folding a method into another moves its
  name instead of removing it. Both were reported as "the name is not the
  lever", both had moved a second input, and the two results looked like a
  contradiction for an hour. If you cannot name what was held constant, the
  result is about the pair, not about the variable.
- **Measure the baseline, not just the after.** "X moved the score" needs the
  score without X. A file was described as losing a near-tie by 0.0175 when
  the reference measurement showed it had never been in the ranking at all —
  a different phenomenon, with a different cause and a different fix.
- **A contradiction is not resolved by the newer measurement.** When two runs
  disagree, both are usually right and one description is wrong. Re-run the
  single control that separates them before correcting anything, and say
  which of the two you are retracting.
- **A published explanation is part of the deliverable.** A doc comment or a
  changelog entry that explains a trap wrongly is worse than one that says
  nothing: the next reader goes looking for the wrong cause. When a
  measurement corrects a merged claim, correct the merged text too.

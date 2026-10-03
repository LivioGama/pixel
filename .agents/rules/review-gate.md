---
description: pre-push review loop — rebase on the remote default, then fix every review-gate finding at CONCERN or above
---

# Review gate before pushing

`pixel review-gate` is the deterministic pre-review: it diffs the change set
(uncommitted diff on a dirty tree; the whole branch diff against the
merge-base with the remote default on a clean one) and lists findings as
`BLOCKER`/`CONCERN`/`SUGGESTION`/`NIT` with file:line, witness and fix.

Before every `git push` of a feature branch, in this order:

1. `git fetch origin && git rebase origin/<default>` — the remote default is
   fetched first; never review or push against a stale base. Resolve
   conflicts hunk by hunk; the rebase finishes before the review runs.
2. `pixel review-gate .` — read every finding.
3. Fix each `BLOCKER` and `CONCERN` (the finding's `fix:` line names the
   move). `SUGGESTION` and `NIT` items are judgement calls — fix the cheap
   ones, record why the rest stay.
4. Re-run until no `BLOCKER` or `CONCERN` remains, then push.

The tracked pre-push hook enforces both halves: it fetches the remote
default and refuses a push whose merge-base is behind it, then runs
`pixel review-gate . --fail-on concern` and refuses the push on any BLOCKER
or CONCERN finding. `git push --no-verify` is the explicit bypass.

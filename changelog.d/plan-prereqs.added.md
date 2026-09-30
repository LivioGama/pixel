**cli:** `pixel plan` emits verification gates — blocking `Gate:` checklist
items when the plan's file set is auth-gated, reads env keys, or imports a
provider SDK or database driver. Auth gates name a saved `auth`-tagged
`replay-flow`, or say to ask for a test account when none exists. Gates are
tracked in `.pixel/plan.json` like findings; `--no-gates` opts out.
Detection is a lower bound: no gates rendered means none were detected, not
that none are needed. See docs/design/plan-prereqs.md.

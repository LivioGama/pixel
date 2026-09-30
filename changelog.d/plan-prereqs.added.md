**cli:** `pixel plan` emits verification gates — blocking `Gate:` checklist
items when the plan's file set is auth-gated, reads env keys, or imports a
provider SDK or database driver. Auth gates name a saved `auth`-tagged
flow, or say to ask for a test account when none exists. Gates are
tracked in `.pixel/plan.json` like findings; `--no-gates` opts out.
Detection is a lower bound: no gates rendered means none were detected, not
that none are needed. See docs/design/plan-prereqs.md.

**cli:** `pixel replay-flow` becomes `pixel flow` — the inner verb `replay`
becomes `run` to match the new outer name. The auth gate wording reads
`run \`pixel flow run <name>\`` (or "ask the human for a test account"
when no `auth`/`login`-tagged flow is saved). `pixel replay-flow` stays
accepted as a hidden clap alias until 1.0; the rename-table row flipped
(`replay-flow` is now the former spelling). The run engine and module
(`pixel-flow/src/run.rs`) take the new spelling too.
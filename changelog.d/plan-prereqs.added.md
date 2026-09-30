**cli:** `pixel plan` emits blocking verification gates for auth-gated files,
environment keys, provider SDKs and database drivers. Gates are tracked in
`.pixel/plan.json`; `--no-gates` opts out. Detection remains a lower bound.
`pixel replay-flow` is now `pixel flow`, with `replay` renamed to `run` and
the old spelling retained as a hidden alias until 1.0. See
`docs/design/plan-prereqs.md`.

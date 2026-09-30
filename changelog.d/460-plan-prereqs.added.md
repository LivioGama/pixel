**cli:** `pixel plan` emits blocking verification gates for auth-gated files,
environment keys, provider SDKs and database drivers. Gates are tracked in
`.pixel/plan.json`; `--no-gates` opts out. Detection remains a lower bound.
`replay-flow` is now `flow`; the old spelling stays as a hidden alias until 1.0.
See `docs/design/plan-prereqs.md`
([#460](https://github.com/LivioGama/pixel/pull/460)).

**cli:** `pixel plan` emits blocking verification gates for auth-gated files,
environment keys, provider SDKs and database drivers. Gates are tracked in
`.pixel/plan.json`; `--no-gates` opts out. Detection remains a lower bound.
See `docs/design/plan-prereqs.md`
([#460](https://github.com/Pixel-CLI/pixel/pull/460)).

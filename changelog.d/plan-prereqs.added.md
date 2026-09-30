**cli:** `pixel plan` emits blocking verification gates for auth-gated files,
environment keys, provider SDKs and database drivers. Gates are tracked in
`.pixel/plan.json`; `--no-gates` opts out. Detection remains a lower bound.
`pixel replay-flow` is now `pixel flow` (the user-facing subcommand); the
internal `FlowAction::Replay` is renamed to `FlowAction::Run` and has no
user-visible effect. The old `replay-flow` spelling is retained as a hidden
alias until 1.0. See `docs/design/plan-prereqs.md`.

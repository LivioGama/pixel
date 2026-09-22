**install:** `pixel install` now wires OpenCode: when `~/.config/opencode`
exists (`$XDG_CONFIG_HOME` honoured), the bundled prompt lands as a managed
block in the global `AGENTS.md` — the one mechanism both OpenCode
generations honour (v2 accepts `instructions` but never resolves it; v1
reads AGENTS.md in the same global slot). A newly created file is seeded
with `~/.claude/CLAUDE.md` so a v1 install shadows nothing. The step also
sweeps stale `instructions` entries naming the deployed prompt and
`plugin`/`plugins` entries whose `pixel.mjs` target no longer exists.
`uninstall` strips the block, and `doctor` gains
`install.opencode-agents-md`. ([#220](https://github.com/LivioGama/pixel/pull/220))

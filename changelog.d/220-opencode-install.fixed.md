**install:** `pixel install` now wires OpenCode: when `~/.config/opencode`
exists (`$XDG_CONFIG_HOME` honoured), the deployed agent prompt is added to
the `instructions` array of `opencode.json`, which OpenCode merges into
every session — the additive mechanism that cannot shadow a migrating
user's `~/.claude/CLAUDE.md` the way a global `AGENTS.md` would. Foreign
keys and entries round-trip untouched, unparseable configs are refused,
`uninstall` drops only pixel's entries, and `doctor` gains
`install.opencode-instructions`. (#220)

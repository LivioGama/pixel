# Manual Setup

Prefer to control your own setup? You don't need `pixel install`.

`pixel install` does two things, and you can do both by hand:

1. **Deploy the agent system prompt** to `~/.local/share/pixel/agent-prompt.md`.
2. **Wrap `claude` / `codex`** in your shell profile so every invocation includes that prompt automatically.

## 1. Install the binary

```bash
brew tap LivioGama/tap
brew install pixel
```

Or build from source:

```bash
cargo build --release -p pixel-cli
cp target/release/pixel ~/.local/bin/pixel
```

To update an existing install later, prefer `pixel upgrade` over a manual
copy: it rebuilds, installs over the binary that is actually running (a
mise/asdf-managed install behind a shim, a Homebrew cellar, or
`~/.local/bin/pixel`), uses an atomic rename, stops the repo's daemon, and
warns when another `pixel` earlier on PATH would still shadow it.

## 2. Copy the system prompt

The prompt is bundled in the repo at
[`crates/pixel-install/assets/pixel-agent-prompt.md`](../crates/pixel-install/assets/pixel-agent-prompt.md).

```bash
mkdir -p ~/.local/share/pixel
cp crates/pixel-install/assets/pixel-agent-prompt.md ~/.local/share/pixel/agent-prompt.md
```

## 3. Wire it into your agent

### Claude Code

```bash
claude --append-system-prompt-file ~/.local/share/pixel/agent-prompt.md
```

Or add a shell function to `~/.zshrc` (or `~/.bashrc`) so it's automatic:

```bash
claude() { command claude --append-system-prompt-file "$HOME/.local/share/pixel/agent-prompt.md" "$@"; }
```

### Codex

```bash
codex -c 'model_instructions_file="~/.local/share/pixel/agent-prompt.md"'
```

Or as a shell function:

```bash
codex() { command codex -c "model_instructions_file=\"$HOME/.local/share/pixel/agent-prompt.md\"" "$@"; }
```

### Pi

Pi reads `~/.pi/agent/APPEND_SYSTEM.md` automatically — no flag needed:

```bash
mkdir -p ~/.pi/agent
cp crates/pixel-install/assets/pixel-agent-prompt.md ~/.pi/agent/APPEND_SYSTEM.md
```

## A note on prompt size

The system prompt is ~266 lines (~3 000 tokens). That is deliberate.

Pixel plays a central role: it replaces `grep`, `rg`, `git log -S`, `git blame`,
manual caller tracing, and exploratory file reading with a single indexed,
evidence-backed workflow. The prompt needs to map every native command to its
Pixel replacement and enforce the mandatory workflow phases — otherwise the
agent falls back to the slow, token-heavy habits the prompt exists to eliminate.

In practice the prompt pays for itself quickly: a single `pixel impact` call
can replace a dozen `grep` + file-read round trips, recovering the token cost
of the prompt in one command.

## Uninstall

Remove the prompt file, the Pi copy, and any shell functions you added:

```bash
rm -f ~/.local/share/pixel/agent-prompt.md
rm -f ~/.pi/agent/APPEND_SYSTEM.md
# then remove the claude()/codex() functions from your shell profile
```

Or just run `pixel uninstall`.

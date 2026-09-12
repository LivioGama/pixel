# Manual Setup

Prefer to control your own setup? You don't need `pixel install`.

`pixel install` does three things, and you can do all of them by hand:

1. **Deploy the agent system prompt** to `~/.local/share/pixel/agent-prompt.md`.
2. **Deploy the sub-agent prompt** to `~/.local/share/pixel/subagent-prompt.md`
   (a short version for Claude Code sub-agents, which see neither the session
   prompt nor its history).
3. **Wrap `claude` / `codex`** in your shell profile so every invocation includes
   those prompts automatically.

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
cp crates/pixel-install/assets/pixel-subagent-prompt.md ~/.local/share/pixel/subagent-prompt.md
```

## 3. Wire it into your agent

### Claude Code

```bash
claude --append-system-prompt-file ~/.local/share/pixel/agent-prompt.md
```

Sub-agents (the `Agent` tool: built-in agents, `.claude/agents/*.md`, agents
from a `--plugin-dir`) do not receive `--append-system-prompt-file`. In print
mode (`-p`/`--print`) Claude Code accepts a second flag for them; it is not
listed in `claude --help` and is ignored in interactive sessions:

```bash
claude -p --append-system-prompt-file ~/.local/share/pixel/agent-prompt.md \
          --append-subagent-system-prompt-file ~/.local/share/pixel/subagent-prompt.md \
          "your task"
```

Or add a shell function to `~/.zshrc` (or `~/.bashrc`) so it's automatic. It
adds the sub-agent flag only when `-p`/`--print` is among the arguments:

```bash
claude() {
  for _pixel_arg in "$@"; do
    case "$_pixel_arg" in
      -p|--print) command claude --append-system-prompt-file "$HOME/.local/share/pixel/agent-prompt.md" --append-subagent-system-prompt-file "$HOME/.local/share/pixel/subagent-prompt.md" "$@"; return $?;;
    esac
  done
  command claude --append-system-prompt-file "$HOME/.local/share/pixel/agent-prompt.md" "$@"
}
```

For fish, put the equivalent in `~/.config/fish/conf.d/pixel.fish`:

```fish
function claude
  if contains -- -p $argv; or contains -- --print $argv
    command claude --append-system-prompt-file "$HOME/.local/share/pixel/agent-prompt.md" --append-subagent-system-prompt-file "$HOME/.local/share/pixel/subagent-prompt.md" $argv
  else
    command claude --append-system-prompt-file "$HOME/.local/share/pixel/agent-prompt.md" $argv
  end
end
```

`pixel install --shell fish` writes exactly this block to `~/.config/fish/conf.d/pixel.fish`.

#### In CI (claude-code-action)

The action runs `claude -p`, so both flags apply. There is no shell wrapper
there: check the two prompt files into the repository (or copy them in a
previous step) and pass the flags through `claude_args`:

```yaml
- uses: anthropics/claude-code-action@v1
  with:
    claude_args: >-
      --append-system-prompt-file .pixel-prompts/agent-prompt.md
      --append-subagent-system-prompt-file .pixel-prompts/subagent-prompt.md
```

Paths are relative to the checkout. Sub-agents forked from the main session
(`subagent_type: "fork"`) inherit the session prompt instead and are not
affected by the sub-agent flag.

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
rm -f ~/.local/share/pixel/subagent-prompt.md
rm -f ~/.pi/agent/APPEND_SYSTEM.md
# then remove the claude()/codex() functions from your shell profile
```

Or just run `pixel uninstall`.

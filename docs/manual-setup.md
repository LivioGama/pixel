# Manual Setup

Prefer to control your own setup, or using an agent `pixel install` does not
wire (Cursor, Gemini CLI, Copilot, ...)? You don't need `pixel install`.

`pixel install` does four things, and you can do all of them by hand:

1. **Deploy the agent system prompt** to `~/.local/share/pixel/agent-prompt.md`.
2. **Deploy the sub-agent prompt** to `~/.local/share/pixel/subagent-prompt.md`
   (a short version for Claude Code sub-agents, which see neither the session
   prompt nor its history).
3. **Wrap `claude`** in your shell profile so every invocation includes
   those prompts automatically.
4. **Put the prompt into Codex's `config.toml`** as `developer_instructions`,
   so every Codex front end (CLI, desktop app, extension, sub-agents) gets it.

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

To update an existing install later, prefer `pixel self-update` over a manual
copy: it rebuilds, installs over the binary that is actually running (an
install behind a shim, `~/.cargo/bin`, or `~/.local/bin/pixel`), uses an
atomic rename, stops the repo's daemon, and warns when another `pixel`
earlier on PATH would still shadow it. It refuses a binary that mise or
Homebrew installed: update those with `mise`/`brew`, try a local build with
`pixel self-update --dev` (installed as `pixel-dev`), or pass
`--install-path` to overwrite on purpose.

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
listed in `claude --help` and is ignored in interactive sessions. **It needs
Claude Code 2.1.261 or newer**: older releases exit with `error: unknown
option`, so check `claude --version` before adding it.

```bash
claude -p --append-system-prompt-file ~/.local/share/pixel/agent-prompt.md \
          --append-subagent-system-prompt-file ~/.local/share/pixel/subagent-prompt.md \
          "your task"
```

Or add a shell function to `~/.zshrc` (or `~/.bashrc`) so it's automatic. It
adds the sub-agent flag only when `-p`/`--print` is among the arguments
(`pixel install` writes this block when `claude --version` is at least
2.1.261, and the plain one-liner from the previous section otherwise; with no
usable `claude` it keeps whatever an earlier install decided, or writes the
plain one-liner on a first install; `pixel doctor` tells you to re-run `pixel
install` once Claude Code crosses the line):

```bash
claude() {
  local _pixel_arg
  for _pixel_arg in "$@"; do
    case "$_pixel_arg" in
      -p*|-[!-]*p*|--print) command claude --append-system-prompt-file "$HOME/.local/share/pixel/agent-prompt.md" --append-subagent-system-prompt-file "$HOME/.local/share/pixel/subagent-prompt.md" "$@"; return $?;;
    esac
  done
  command claude --append-system-prompt-file "$HOME/.local/share/pixel/agent-prompt.md" "$@"
}
```

For fish, put the equivalent in `~/.config/fish/conf.d/pixel.fish`:

```fish
function claude
  if contains -- --print $argv; or string match -qr -- '^-[^-]*p' $argv
    command claude --append-system-prompt-file "$HOME/.local/share/pixel/agent-prompt.md" --append-subagent-system-prompt-file "$HOME/.local/share/pixel/subagent-prompt.md" $argv
  else
    command claude --append-system-prompt-file "$HOME/.local/share/pixel/agent-prompt.md" $argv
  end
end
```

`pixel install --shell fish` writes this function inside a
`# >>> pixel-managed >>>` block in `~/.config/fish/conf.d/pixel.fish`.
Both variants also treat a short-flag cluster containing `p` (`-pc`, `-cp`)
as print mode, since Claude Code splits those.

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

Codex takes the prompt through the `developer_instructions` key of
`~/.codex/config.toml` (`$CODEX_HOME/config.toml` when that variable is set).
The key is appended to Codex's developer message and leaves its own system
prompt in place; `model_instructions_file` looks similar but *replaces* that
system prompt (it becomes the base instructions, and the model loses its
native protocol and personality). A config key, unlike a shell function,
reaches the desktop app's bundled binary, the VS Code extension, scripts
that call the binary by path, and `spawn_agent` sub-agents (which inherit it
unless a role overrides it).

Codex has no file-backed variant of the key, so the prompt is embedded as a
TOML literal multi-line string, between two marker lines. Text you keep
outside the markers survives a re-install; `pixel install` refreshes only the
block, and `pixel uninstall` removes only the block (or the key, when nothing
else was in it):

```toml
developer_instructions = '''
Your own instructions, if any.

<!-- pixel:managed:begin -->
# Pixel Retrieval Layer — Mandatory Agent Protocol
... the content of ~/.local/share/pixel/agent-prompt.md ...
<!-- pixel:managed:end -->
'''
```

To do it by hand, paste `agent-prompt.md` between the markers. Codex reads the
file at session start, so a running session keeps the prompt it started with.
For a one-off session with different instructions, the CLI override wins:
`codex -c developer_instructions="..."`.

### Pi

Pi reads `~/.pi/agent/APPEND_SYSTEM.md` automatically — no flag needed:

```bash
mkdir -p ~/.pi/agent
cp crates/pixel-install/assets/pixel-agent-prompt.md ~/.pi/agent/APPEND_SYSTEM.md
```

### Any other agent

`pixel install` covers only the three agents above. For any other tool, put
the full text of `~/.local/share/pixel/agent-prompt.md` wherever that tool
reads always-on instructions: a rules file (`.cursor/rules`, `GEMINI.md`,
`.github/copilot-instructions.md`), a system-prompt flag, or a global
`AGENTS.md`. Copy the bundled prompt verbatim rather than a summary; it is
the single source of truth, and `pixel doctor` checks the deployed copy
against it. Re-copy it after each `pixel self-update`.

## Renamed commands

If you wrote Pixel commands into your own instructions before the rename
(a `CLAUDE.md`, a rules file, a CI script, a hand-wired hook such as
`<pixel> hook session-start`), you do not have to rewrite them yet: each old
name is a hidden alias of its new name until 1.0, and `pixel doctor` accepts
rule text that still uses the old names. Update them when convenient, using
the table in the [README](../README.md#-renamed-commands).

An old name prints one `note:` line on stderr with the new name. It never
touches stdout or `--json` output, and `--metrics off` or `PIXEL_METRICS=0`
silence it together with the metrics line. Hook invocations stay silent.

## A note on prompt size

The system prompt is ~270 lines (~3 950 tokens). That is deliberate.

Pixel plays a central role: it replaces `grep`, `rg`, `git log -S`, `git blame`,
manual caller tracing, and exploratory file reading with a single indexed,
evidence-backed workflow. The prompt needs to map every native command to its
Pixel replacement and enforce the mandatory workflow phases — otherwise the
agent falls back to the slow, token-heavy habits the prompt exists to eliminate.

In practice the prompt pays for itself quickly: a single `pixel impact` call
can replace a dozen `grep` + file-read round trips, recovering the token cost
of the prompt in one command.

## Uninstall

Remove the prompt files, the Pi copy, the `claude` shell function and the
Codex block:

```bash
rm -f ~/.local/share/pixel/agent-prompt.md
rm -f ~/.local/share/pixel/subagent-prompt.md
rm -f ~/.pi/agent/APPEND_SYSTEM.md
# then remove the claude() function from your shell profile and the
# pixel block from developer_instructions in ~/.codex/config.toml
```

Or just run `pixel uninstall`.

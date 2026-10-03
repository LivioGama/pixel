# Verify the Installed Binary Before Touching Unit Tests

Always loaded: when a change affects installed behavior — the pixel binary,
the hooks it writes (`~/.claude/settings*.json`, `~/.codex/hooks.json`, the
Antigravity plugin), the deployed prompt assets — the ground truth is what
the **installed** binary does with a **real** payload through the **real**
hook path, not what a hand-built unit call does.

- **Install first, then test.** Make the change, rebuild (`pixel self-update
  --repo . --build "cargo build --profile dev-release -p pixel-cli"`),
  reinstall (`pixel install` / `pixel install --repo .`), and drive a real
  payload through the installed `pixel run-hook …` (or a real `claude`,
  `codex`, `agy` subprocess when auth allows) to observe the behaviour you
  changed. Then, and only then, write or adjust the unit/integration test to
  pin what the installed binary actually did.
- **A unit test that passes on a hand-built call while the installed hook is
  broken proves nothing.** Every test in this repo must test the path the
  install produces: the exact hook whose command `pixel install` writes
  (`pixel_publish` strings in `routing.rs`), the exact payload the host
  sends, the exact binary on PATH. Do not reformulate the call so it passes
  around the seam the installed behaviour lives on.
- **A claim of "installed" is not a claim.** Show it: the emitter output of
  the installed `pixel run-hook metrics --provider <host>` for the exact
  command form the host issues (`rtk pixel …`, `grep …`), or the deployed
  `hooks.json`/`AGENTS.md` content. When headless subprocess auth is
  unavailable, say so and give the hook-level proof from the installed
  binary instead of falling back to test-only reasoning.
- **Do not churn tests while the real behaviour is unverified.** The rounds
  of "still don't see the metric in Claude Code" were scripted test updates
  ahead of confirming the installed relay delivered the `🟩` line; the four
  rounds each ended by install + a real-payload check. Ordering it the other
  way — real binary first, test second — closes the gap in one round.
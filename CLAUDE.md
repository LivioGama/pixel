# Project Rules

Build, gates, PR format and the definition of done live in [CONTRIBUTING.md](CONTRIBUTING.md). Read it before the first edit; the loop below is the per-turn addendum to it.

## Reinstall and Reconfig After Each Implementation Turn

After finishing any implementation turn in this repo (code edit + verify cycle):

1. **Rebuild and reinstall the pixel binary** so the installed CLI matches the working tree:
   ```bash
   pixel upgrade --repo .
   ```
   It builds `cargo build --release -p pixel-cli`, installs over the binary that is actually running (`pixel` resolved through any mise/asdf shim to its managed install dir; `~/.local/bin/pixel` only as a last resort — never copy there by hand, a second copy shadows the managed one), stops this repo's daemon, and warns if another `pixel` earlier on PATH would still be picked up. The install is an atomic rename: in-place `cp` over a mapped Mach-O invalidates the ad-hoc signature on macOS and SIGKILLs the next invocation.
2. **In parallel** (both only need the new binary, not each other):
   - **Track A:** `pixel index --history .` — rebuild the facts/history index.
   - **Track B:** `build-agent-config && pixel install` — propagate rule edits to tool directories, then reinstall hooks and managed blocks.
3. **Run `pixel doctor`** and confirm green (or explicitly report any non-green check).

### When to skip

- Pure read-only exploration (no edits to `crates/` or rules).
- The turn only touched docs, prompts, or bench scripts — nothing that changes binary behavior or installed rules.

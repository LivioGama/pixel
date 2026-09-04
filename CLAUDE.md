# Project Rules

## Reinstall and Reconfig After Each Implementation Turn

After finishing any implementation turn in this repo (code edit + verify cycle):

1. **Rebuild and reinstall the pixel binary** so the installed CLI matches the working tree:
   ```bash
   cargo build --release -p pixel-cli && cp target/release/pixel ~/.local/bin/.pixel.tmp.$$ && mv -f ~/.local/bin/.pixel.tmp.$$ ~/.local/bin/pixel
   ```
   (Atomic rename avoids corrupting the code signature of a running binary on macOS — in-place `cp` overwrites a mapped Mach-O, invalidating the ad-hoc signature and causing SIGKILL on next invocation.)
2. **In parallel** (both only need the new binary, not each other):
   - **Track A:** `pixel index --history .` — rebuild the facts/history index.
   - **Track B:** `build-agent-config && pixel install` — propagate rule edits to tool directories, then reinstall hooks and managed blocks.
3. **Run `pixel doctor`** and confirm green (or explicitly report any non-green check).

### When to skip

- Pure read-only exploration (no edits to `crates/` or rules).
- The turn only touched docs, prompts, or bench scripts — nothing that changes binary behavior or installed rules.

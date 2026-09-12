# Project Rules

Build, gates, PR format and the definition of done live in [CONTRIBUTING.md](CONTRIBUTING.md). Read it before the first edit; the loop below is the per-turn addendum to it.

## Mutation Testing Loop

Mutation tests are run with `cargo mutants --in-diff <(git diff develop...HEAD)` after the gates (`cargo fmt`, `cargo test`, `cargo clippy`) pass. They must pass: the CI workflow `Mutants` fails a pull request on any surviving mutant.

For each `MISSED` line either:

- add a test that fails under that exact mutation (an assertion on the observable contract, not a weaker one), or
- when the mutation cannot matter (a diagnostic formatter, a `main`, dead-by-design code), annotate the function with `#[cfg_attr(test, mutants::skip)]` plus a one-line reason, adding `mutants = { workspace = true }` to that crate's `[dependencies]` if it is the crate's first skip.

Re-run until the summary reports `0 missed`. Skipping a business rule because the test is hard is not an option; see CONTRIBUTING.md "Mutation testing" for the outcome table and exit codes.

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

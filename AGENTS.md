# Project Rules

Build, gates, PR format and the definition of done live in [CONTRIBUTING.md](CONTRIBUTING.md). Read it before the first edit; the loop below is the per-turn addendum to it.

## Mutation Testing Loop

Mutation testing runs in CI only: the `Mutants` workflow runs `cargo mutants --in-diff` against the PR's base and fails the pull request on any surviving mutant. Do not run `cargo mutants` locally on your own initiative; it holds the tree (`--in-place`) and a laptop for up to hours, which is what the workflow's runners are for. The local loop is: write the code in the shapes `.agents/rules/mutation-gate.md` describes, pass the fast gates (`cargo fmt`, `cargo test`, `cargo clippy`), push, open the PR, then read the `Mutants` job's `MISSED`/`TIMEOUT` lines (`gh run view --log` or the job annotations). A local `cargo mutants … -F '<fn>'` on one or two functions, bounded to a few minutes, is acceptable only when explicitly asked for.

For each `MISSED` line either:

- add a test that fails under that exact mutation (an assertion on the observable contract, not a weaker one), or
- when the mutation cannot matter (a diagnostic formatter, a `main`, dead-by-design code), annotate the function with `#[cfg_attr(test, mutants::skip)]` plus a one-line reason, adding `mutants = { workspace = true }` to that crate's `[dependencies]` if it is the crate's first skip.

Push the fix and let the workflow re-run until it reports `0 missed`. Skipping a business rule because the test is hard is not an option; see CONTRIBUTING.md "Mutation testing" for the outcome table and exit codes.


## Rules Directory

Scoped rules live in [`.agents/rules/`](.agents/rules/), one Markdown file
per concern, with an optional `paths:` front matter naming the globs they
apply to:

| File | Applies to | Content |
| --- | --- | --- |
| `mutation-gate.md` | `crates/**/*.rs` | writing code and tests that pass `cargo mutants` on the first run |
| `test-hygiene.md` | `crates/**/*.rs` | git fixtures, env vars, canonical paths, `clippy --fix` cleanup |
| `test-campaigns.md` | always | running long mutants/nextest campaigns without surprises |
| `rust-style.md` | `crates/**/*.rs` | the shapes the four pedantic lints expect (`uninlined_format_args`, `map_unwrap_or`, `redundant_closure_for_method_calls`, `items_after_statements`) and the cleanup after `clippy --fix` |

`.claude/rules` is a symlink to that directory (Claude Code loads it by
itself, honouring `paths:`), and `CLAUDE.md` is a symlink to this file. A
tool that does not auto-load a rules directory (Codex, pi, Devin) reads the
files listed above before its first edit; the `paths:` front matter tells it
which ones matter for the files it is about to touch. Add a rule as a new
file here, never as a second copy in a tool-specific directory.

## Reinstall and Reconfig After Each Implementation Turn

After finishing any implementation turn in this repo (code edit + verify cycle):

1. **Rebuild and reinstall the pixel binary** so the installed CLI matches the working tree:
   ```bash
   pixel upgrade --repo . --build "cargo build --profile dev-release -p pixel-cli"
   ```
   `dev-release` is the release profile without thin LTO and with 16 codegen units: an incremental rebuild takes seconds instead of a minute, and the binary is optimised the same way. Drop `--build` only when you need the exact shipped `release` profile. It runs that build, installs over the binary that is actually running (`pixel` resolved through any mise/asdf shim to its managed install dir; `~/.local/bin/pixel` only as a last resort — never copy there by hand, a second copy shadows the managed one), stops this repo's daemon, and warns if another `pixel` earlier on PATH would still be picked up. The install is an atomic rename: in-place `cp` over a mapped Mach-O invalidates the ad-hoc signature on macOS and SIGKILLs the next invocation.
2. **In parallel** (both only need the new binary, not each other):
   - **Track A:** `pixel index --history .` — rebuild the facts/history index.
   - **Track B:** `build-agent-config && pixel install` — propagate rule edits to tool directories, then reinstall hooks and managed blocks.
3. **Run `pixel doctor`** and confirm green (or explicitly report any non-green check).

### When to skip

- Pure read-only exploration (no edits to `crates/` or rules).
- The turn only touched docs, prompts, or bench scripts — nothing that changes binary behavior or installed rules.

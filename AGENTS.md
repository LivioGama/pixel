# Project Rules

Build, gates, PR format and the definition of done live in [CONTRIBUTING.md](CONTRIBUTING.md). Read it before the first edit; the loop below is the per-turn addendum to it.

## Mutation Testing Loop

Mutation tests are run with `cargo mutants --in-diff <(git diff develop...HEAD)` after the gates (`cargo fmt`, `cargo test`, `cargo clippy`) pass; `scripts/gates.sh --mutants` runs all four with laptop-safe job and thread caps and skips when no Rust-affecting path changed. They must pass: the CI workflow `Mutants` fails a pull request on any surviving mutant.

For each `MISSED` line either:

- add a test that fails under that exact mutation (an assertion on the observable contract, not a weaker one), or
- when the mutation cannot matter (a diagnostic formatter, a `main`, dead-by-design code), annotate the function with `#[cfg_attr(test, mutants::skip)]` plus a one-line reason, adding `mutants = { workspace = true }` to that crate's `[dependencies]` if it is the crate's first skip.

Re-run until the summary reports `0 missed`. Skipping a business rule because the test is hard is not an option; see CONTRIBUTING.md "Mutation testing" for the outcome table and exit codes.


## Code That Passes the Mutation Gate on the First Run

The gate mutates every function that has at least one line in the diff, not
only the lines you wrote. A one-token change (an inlined format argument, a
`map_or`) in an untested function puts that whole function under the gate.
Rules that make the first `cargo mutants` run come back clean:

- **Read the function's tests before touching it.** No test that would fail
  if the body were replaced by `Default::default()`? Write one first, on the
  observable contract (returned value, written file, emitted line), then edit.
  The budget of a "lint only" change is the missing tests, not the lint.
- **Give every loop a bound a test can set.** A function that loops "until
  fresh/ready/done" gets a sibling taking the wall-clock cap
  (`ingest_until_fresh_within(store, opts, cap)`); production calls it with
  the production cap, tests with a cap under 5 s. A mutant that breaks the
  loop body then fails in seconds instead of hanging until the timeout.
  Same for scanning loops: always advance past the current item so a wrong
  bound cannot spin (`pos = close.max(start) + tag.len()`).
- **Keep the test cap under 20 s.** CI runs cargo-mutants with its automatic
  timeout: five times the baseline test time, never below 20 s. A test that
  waits 30 s for a broken loop is reported TIMEOUT in CI while it passes
  locally with `--timeout 300`. Reproduce CI with `cargo mutants --timeout 20`.
- **Fake servers poll with a deadline.** A test that `accept()`s blockingly
  hangs forever under a mutant that never connects. `set_nonblocking(true)`,
  loop with a 5 s deadline, return on expiry so the assertion fails instead.
- **Put a seam where the code meets the outside.** Spawning a browser, a
  daemon socket, `PATH`, the embedding model: hide the call behind a trait
  (`Browser { run, pause }`), a parameter (`find_in_paths(name, path)`),
  or a pure helper (`script_block_bounds(text, start)`), and test the seam.
  `#[cfg_attr(test, mutants::skip)]` is for the one-line adapter over the
  real process, with a one-line reason, never for the logic behind it.
- **Name a comparison that a test cannot reach.** `t < cutoff` buried in a
  loop over git output becomes `is_stale(committer_unix, cutoff)` and gets
  the four cases: below, at, above, unknown. The equality edge is the one
  the gate flips (`>` to `>=`): a blob exactly at the cap is not over it, a
  branch exactly at the cutoff is not stale, a tie keeps the first region.
- **Write literal constants without operators.** `1 | 2 | 4` and
  `256 * 1024` survive as `^` and `+` because nothing can tell the values
  apart; `0b111` and `262_144 // 256 KiB` leave nothing to mutate.
- **Time helpers get the bracket test.** For `now_ms`/`now_unix`/`iso_now`:
  read the clock, call the helper, read it again, assert the value sits
  between the two and above a fixed floor (2020-01-01). It kills `0`, `1`
  and `"xyzzy"` at once.
- **A survivor that shows the code is wrong is a bug report.** When the test
  written for a mutant proves the function never worked (a `git cat-file
  --batch-check <object>` that git rejects, so every blob measured 0 bytes),
  fix the bug in its own PR with a CHANGELOG entry, below the PR that found
  it. Do not bend the test to the broken behaviour.
- **Re-run on the fixed functions only, then commit.** `cargo mutants
  --in-diff <diff> -F <function>` judges the functions you just covered in
  minutes; the full in-diff run is for the final state. Never edit the tree
  while a run is in flight: it mutates files in place. After a killed or
  crashed run, `grep -rl "changed by cargo-mutants" crates/` and restore
  before doing anything else.

## Test Hygiene That Keeps Suites Deterministic

- **One git fixture helper per crate**, in `#[cfg(test)] pub(crate) mod
  testutil` at the crate root: `git()` with author/committer identity and
  `GIT_CONFIG_GLOBAL=/dev/null`, `init_repo()`, `commit(root, files, msg)`.
  Real `git` in a temp dir, never a mocked runner: the parsers wrap real
  plumbing output.
- **Environment variables are process-global.** Under `cargo test` the tests
  of one binary share a process. A test that sets a variable uses a name
  unique to itself (`PIXEL_TEST_FLAG_{pid}_{line}`) with a `// SAFETY:`
  comment, or takes the crate-wide mutex when the variable has one owner
  (`store::ENV_MUTEX` for `PIXEL_FLOW_DIR`). Never read `PATH`, `HOME` or
  a config path inside the function under test when a parameter can carry it.
- **Compare canonical paths.** macOS's temp dir is a symlink
  (`/var` to `/private/var`); anything that stores `root.canonicalize()`
  will not match `temp_dir().join(..)`. Canonicalize the expectation.
- **Fixtures are literal.** Build multi-line protocol fixtures (blame
  porcelain, NDJSON, `git log -z` records) from an explicit list of lines
  joined with `"\n"`, not from a continued string literal: an indentation
  or escape mistake there parses as valid input and tests the wrong thing.
- **Clean up after `clippy --fix`.** It writes `std::string::ToString::to_string`
  and `std::vec::Vec::len`; shorten to `ToString::to_string`, `Vec::len`,
  `AsRef::as_ref`, `str::to_lowercase`. It does not move items: hoist a
  `const`/`struct`/`use` declared after statements to the top of its function
  or to module level with its comment attached.
- **`is_none_or`, `is_some_and`, `is_ok_and`** over `map().unwrap_or()`,
  and `map_or(default, f)` when the default is cheap. Both are what the
  enabled `map_unwrap_or` lint expects.

## Long Test Campaigns

- **Measure before you launch.** `cargo mutants --list --in-diff <diff> | wc -l`
  gives the mutant count; multiply by the crate's test time (`pixel-cli`
  about 30 s per mutant locally, 25 s in CI after a 3 min baseline build;
  library crates 2 to 10 s). A count that will not fit the 90-minute CI job
  means the PR must be split by file, never by weakening the gate.
- **Do not throttle the CLI suite.** `RUST_TEST_THREADS=4` made the
  process-spawning `pixel-cli` tests four times slower per mutant; the
  default thread count is right, and a flaky baseline is re-run, not
  worked around.
- **Watch the disk.** Every mutant rebuilds incrementally; `target/debug`
  reached 34 GB and the run aborted on a full disk, leaving a mutated file
  behind. `df -h .` before a run; `target/debug/incremental`, `target/release`
  and `target/dev-release` are safe to delete between campaigns.
- **Stacked PRs diff against their base**, not `develop`:
  `cargo mutants --in-diff <(git diff <base-branch>)`. A lower PR fixed after
  review gets a follow-up commit pushed to its branch; the branches above
  keep their diff and the merge order stays bottom-up.
- **Work on a lower branch from a second worktree**
  (`git worktree add /tmp/pxwt/<name> <branch>`, then `pixel publish ... <path>`)
  while a mutants run holds the main tree; remove it afterwards.
- **Finish with a daemon check.** `pgrep -fl "target/.*/pixel daemon"` must
  print nothing; a fixture that left a daemon serving it is a test bug.

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

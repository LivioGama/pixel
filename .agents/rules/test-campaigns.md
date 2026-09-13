# Long Test Campaigns

Always loaded: how to run the long gates without losing an afternoon.

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

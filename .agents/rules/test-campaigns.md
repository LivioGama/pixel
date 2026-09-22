# Long Test Campaigns

Always loaded: how to run the long gates without losing an afternoon.

- **Mutants run in CI, not on the laptop.** The `Mutants` workflow is the
  gate; the local machine is for writing code. A 231-mutant campaign held a
  laptop's tree for two hours (`--in-place` forbids edits meanwhile) for
  24 survivors that sat in six functions, all readable from the job log.
  Only when explicitly asked, run `cargo mutants --in-diff <diff> -F '<fn>'`
  on the one or two functions in question (minutes) and never the full diff.
- **Measure before you launch anything.** `cargo mutants --list --in-diff
  <diff> | wc -l` gives the mutant count; CI costs about 25 s per `pixel-cli`
  mutant after a 3 min baseline and 10 to 15 s per library-crate mutant. A
  count that will not fit the 90-minute CI job means the PR must be split by
  file, never by weakening the gate.
- **Count against the merge base, with three dots.** `git diff
  origin/main...HEAD` diffs the merge base against HEAD — what the branch
  changed. `git diff origin/main..HEAD` diffs the two commits and adds
  everything `main` gained since you branched; `git diff origin/main`, with
  no second revision, compares `main` to the working tree instead, so it adds
  uncommitted edits on top. Either of the last two has you mutating code you
  did not write: 113 mutants where the PR owed 103, ten of them in a file
  another pull request had just rewritten.
- **`--in-diff` compares the diff to the working tree, not to HEAD.** With an
  uncommitted edit it prints `Diff content doesn't match source file` and
  lists zero mutants, which reads like good news. Commit first, then write
  the diff.
- **`--timeout 20` breaks the baseline of crates with doctests**: rustdoc's
  doctest compile alone takes 15 to 20 s, and the cap applies to the
  baseline too. Use `--timeout 60` for a local `-F` run; CI's automatic cap
  is five times the measured baseline, so it is unaffected.
- **Do not throttle the CLI suite.** `RUST_TEST_THREADS=4` made the
  process-spawning `pixel-cli` tests four times slower per mutant; the
  default thread count is right, and a flaky baseline is re-run, not
  worked around.
- **Watch the disk.** Every mutant rebuilds incrementally; `target/debug`
  reached 34 GB and the run aborted on a full disk, leaving a mutated file
  behind. `df -h .` before a run; `target/debug/incremental`, `target/release`
  and `target/dev-release` are safe to delete between campaigns. `just disk`
  prices every worktree's build output, index and scratch in one list, and
  `just clean` takes the build output of all of them (CONTRIBUTING.md,
  "Reclaiming disk"); a `mutants.out` left by an aborted run is in that list.
- **Stacked PRs diff against their base**, not `main`:
  `cargo mutants --in-diff <(git diff <base-branch>)`. A lower PR fixed after
  review gets a follow-up commit pushed to its branch; the branches above
  keep their diff and the merge order stays bottom-up.
- **Work on a lower branch from a second worktree**
  (`git worktree add /tmp/pxwt/<name> <branch>`, then `pixel commit ... <path>`)
  while a mutants run holds the main tree; remove it afterwards.
- **Finish with a daemon check.** `pgrep -fl "target/.*/pixel daemon"` must
  print nothing; a fixture that left a daemon serving it is a test bug.

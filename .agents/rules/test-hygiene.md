---
paths:
  - "crates/**/*.rs"
---

# Test Hygiene That Keeps Suites Deterministic

Loaded when a Rust source file is in play. Companion of `mutation-gate.md`.

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
- **Spawn the CLI through `support::pixel_command()`**, or set
  `current_dir` to a fixture: cargo runs a test from `crates/<crate>`, and a
  `pixel` started there with no path (or a missing one) appends its action
  log to this checkout's `.pixel/actions.jsonl`. CI fails the run when
  that file exists after the tests. It also points `HOME` at an empty
  directory (`support::neutral_home()`): a test that spawns the binary
  itself sets `HOME` too, or a machine with another release's
  `pixel install` adds a stale-prompt note to every stderr it reads.
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
- **A "must not happen" test rules out every form it takes.** When the
  claim is that X never occurs, assert the absence of each observable shape
  of X: no edge of any kind, no `unresolved_calls` row, no stray file, no
  line on stderr. #262 checked that an alias left no unresolved row while a
  false `References` edge would still have passed.
- **Assert the value, not that there is one.** `!is_empty()`, `is_some()`
  and `len() > 0` let most mutants through; compare with the expected value
  or the expected list (#222's preset tests only checked non-emptiness).
- **Test the path production takes.** A test that builds its object by hand
  when production reaches it through an opener, an adapter or a constructor
  (`open_repo`, the real transport) proves the hand-built one: #229's test
  wrote the download marker itself, so `open_repo` could stop writing it
  unseen.
- **A new input gets one case per path that consumes it.** Adding a site
  line, a scope or a name to a decision means one test through the full
  build and one through each incremental route that re-derives it
  (`change-propagation.md`); the full build alone passed on #262 while the
  incremental update lost the edge.

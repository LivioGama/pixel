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

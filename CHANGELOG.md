# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- `pixel --version` now reports where the binary came from: after the `pixel x.y.z` line it prints `commit: <sha>` (`-dirty` appended when the tree had uncommitted tracked changes), `target:`, `rustc:` and `built:` (UTC date; `SOURCE_DATE_EPOCH` honoured). `pixel -V` keeps the one-line form. Values a build cannot determine (a tarball without `.git`, no `git` on PATH) read `unknown` instead of failing the build. This answers "which build is this?" from a bug report or a CI artifact, and lets a rebuild loop see that the installed binary lags the tree.
- `pixel release-check <version|tag> [--repo <path>] [--json]`: the release consistency gate. It checks that `crates/pixel/Cargo.toml` carries the tagged version, that `Cargo.lock` holds every workspace member at its manifest version (a stale lock used to fail `cargo build --locked` on the release runner, after the tests had passed), and that `CHANGELOG.md` has the `## [x.y.z]` heading with nothing left under `## [Unreleased]`. One `[ok  ]`/`[FAIL]` line per check with the fix in the failure text, exit 1 on any failure. The release workflow runs it before the tests; `v1.2.3` and `refs/tags/v1.2.3` are accepted.

### Changed
- The `fastembed` feature no longer enables fastembed's `image-models` default: pixel never embeds images, and that feature alone pulled the `image` crate with every codec (rav1e, exr, tiff, png…), 64 crates and 600 lock lines out of the CLI's dependency graph. Text embedding, model download and the TLS stack are unchanged.
- Building from source on macOS or Windows no longer compiles a vendored OpenSSL that was never linked: `pixel-recall` declares it only for the targets where native-tls actually uses it (`cfg(not(any(target_os = "windows", target_vendor = "apple")))`, native-tls's own gate). Linux builds with the `fastembed` feature are unchanged; about 90 s off a cold macOS build.
- `pixel upgrade` reads the built binary from `target/<profile>/pixel`, with the profile taken from the `--profile`/`--release` flags of its `--build` command, instead of always `target/release/pixel` (a `--build` on another profile installed whatever stale binary sat there, without error). The workspace gains a `dev-release` profile (`release` without thin LTO, 16 codegen units) for the local reinstall loop: an incremental rebuild after touching the CLI crate drops from 55 s to 9 s on an 8-core M-series; shipped binaries keep the `release` profile.
- Codex now receives the agent prompt through `developer_instructions` in `~/.codex/config.toml` (`$CODEX_HOME` honoured) instead of a `codex` shell function. A config key reaches every Codex front end — the desktop app's bundled binary, the VS Code extension, scripts calling the binary by path, `spawn_agent` sub-agents — where the function only fronted interactive shells that sourced the profile. `pixel install` embeds the prompt as a TOML literal multi-line string between `<!-- pixel:managed:begin -->`/`end` marker lines, rewrites only that key (the rest of the file keeps its layout and comments), keeps text outside the markers, and reports the step red without writing when the file does not parse. `pixel doctor` gains `install.codex-config` (red when the file, the key or the block is missing, or the block differs from the bundled prompt); `pixel uninstall` removes the block, or the key when nothing else was in it. The `claude` shell wrapper block no longer carries a `codex` line; re-run `pixel install` to update it.

### Fixed
- Graph and retrieval answers (`symbol`, `resolve`, `uses`, `impact`, `context`, `trace`, `changes`, `targets`, `search`, `processes`, `clusters`, plus `status` and `diff`) no longer embed the full dirty path list in `snapshot`: the daemon ships `head`, `branch` and `dirty_count` for every op but `inspect` and `review`, which own the list. On a CI checkout with an untracked `vendor/bundle` (15 000 paths) a `symbol --json` answer was 238 KB of paths around 500 bytes of symbol; it is now under 4 KB. `ready --json` reads the count from the compact snapshot (and still falls back to the list from an older daemon).
- The first graph command after an edit no longer rebuilds `graph.db` from scratch (100 s on a 10 000-file Rails app in CI, 36 s locally, for a one-line change). When a signed `graph.db` exists and the tree drifted, pixel re-extracts only the files whose content hash changed, drops the deleted ones and re-resolves the calls that targeted them, then re-signs the graph. The full rebuild remains the fallback when the db carries no freshness signature or when more than `PIXEL_GRAPH_INCREMENTAL_MAX_PCT` percent of the indexed files drifted (default `20`, `0` disables the incremental path). `graph_build` now carries `incremental` (with `changed_files`/`removed_files`) or a `reason` (`missing`, `no_signature`, `threshold`, `incremental_failed`), and the stderr notice distinguishes `updated graph.db for N changed file(s) (… ms)` from `built graph.db on first use (… ms)`. The incremental path (also the daemon watcher's) now matches a full rebuild on two points: imports to files added in the same batch, and dangling imports of unchanged files, resolve once the file exists (callers come out `Exact` rather than `Probable`), and the persisted `processes`/`clusters` are cleared so they are recomputed on demand instead of pointing at the re-extracted symbols' old ids.
- The `codex` shell wrapper written by `pixel install` (bash, zsh and fish) no longer replaces Codex's native system prompt. It passed the Pixel prompt through `-c model_instructions_file=...`, which Codex loads as its base instructions in place of the model's own (tools, format and personality guidance gone; a probe on codex-cli 0.154.0 ran ~3 400 tokens lighter, the size of the lost prompt). It now reads the prompt at call time and passes it as `-c developer_instructions=...`, which Codex appends to its developer message and which `spawn_agent` sub-agents inherit. `pixel doctor` reports the previous block as stale; re-run `pixel install`.

## [0.2.3] - 2026-09-12

### Added
- `pixel install` deploys a second, short (under 2 KB) prompt to `~/.local/share/pixel/subagent-prompt.md` and the `claude` shell wrapper passes it with `--append-subagent-system-prompt-file` whenever `-p`/`--print` is among the arguments (bash, zsh and fish). Claude Code sub-agents receive neither the session's `--append-system-prompt-file` nor its history, and honour the sub-agent flag in print mode only. The flag is written only when `claude --version` reports 2.1.261 or newer (the release that added it; older ones exit 1 on the unknown option): below that, `install` writes the plain wrapper and reports the step yellow with the version found; with no usable `claude` (not on PATH, no answer within 5 s, unparseable version) it keeps the flag decision of the existing block, or writes the plain wrapper on a first install, and reports yellow. A short-flag cluster containing `p` (`-pc`, `-cp`) counts as print mode. `pixel doctor` gains an `install.subagent-prompt` check, re-derives the expected wrapper from the Claude Code found at check time (red in both directions: flag in front of an old Claude Code, or missing in front of a new one; yellow when `claude` cannot be probed), and `pixel uninstall` removes the file. The task-worker command builder accepts a matching `subagent_prompt_file` option; nothing sets it yet.

### Fixed
- The bundled agent prompt (`agent-prompt.md`) no longer documents command lines the CLI rejects: `pixel uses "X" --callers|--callees` is `--role callers|callees`, `publish`/`ship`/`branch`/`update` take `--request-id` (there is no `-r`), `update` needs `--target-oid` and `--expected-head`, `lifecycle` takes `--file <path>`, `sync` needs a remote. The "installed at `~/.local/bin/pixel`" sentence, wrong for mise/Homebrew/CI installs, now points at `command -v pixel`.
- `pixel publish --files <path>` (and `ship`) no longer fails with `git add: pathspec '<path>' did not match any files` when `<path>` is a deletion already staged with `git rm`. Paths absent from both the worktree and the index skip the `git add` step and are committed by the pathspec-scoped `git commit`; a path git has never known is still rejected, and no commit is created.

## [0.2.2] - 2026-09-12

### Added
- `CONTRIBUTING.md`: build, gates, test layout, op-adding checklist, commit/PR format and a definition-of-done checklist written for humans and coding agents. Linked from README, AGENTS.md and CLAUDE.md.
- `PIXEL_OUTPUT_CAP_BYTES=<bytes>` overrides the global stdout cap; `0` lifts it (same convention as `PIXEL_INDEX_BUDGET_MS=0`).

### Fixed
- `pixel upgrade` no longer defaults to a fixed `~/.local/bin/pixel`. It installs over the binary running the command (a mise/asdf-managed install behind a shim, a Homebrew cellar, `~/.cargo/bin`), falls back to the first `pixel` on PATH outside `shims`/`target` directories, and only then to `~/.local/bin/pixel`. The chosen path and the reason are printed, and a warning names any other `pixel` earlier on PATH that would still shadow the upgraded one.
- `pixel ready --json` and `pixel status --json` no longer embed the full dirty file list: `ready` reports index/graph/daemon plus a `dirty_count`, and `status` collapses `snapshot.dirty` to `snapshot.dirty_count`. One untracked `vendor/bundle` used to push both answers past the output cap and replace them with a `{partial: "..."}` wrapper.
- `--json` output over the global 256 KB cap is now truncated structurally: the largest arrays are shortened, every other field survives, and the document gains `truncated: true`, `cap_bytes` and `truncated_arrays` (path, kept, total). The textual `{partial}` wrapper is only the fallback when no array can be cut.

### Changed
- CI: cache Rust builds (`Swatinem/rust-cache@v2`), install `cross` prebuilt (`taiki-e/install-action@v2`), cancel superseded PR runs, scope the workflow token to least privilege, run on pushes to `develop`, build with `--locked`, and fail packaging on a missing README or LICENSE.
- Release workflow: verify tag/version match and run tests before building, publish the generated Homebrew formula to the tap, and use the tag's CHANGELOG section as the release body.
- Dependabot: group cargo minor/patch bumps and all GitHub Actions updates into single PRs.

## [0.2.1] - 2026-09-12

### Added
- `--shell <shell>` on `pixel install`, `pixel uninstall` and `pixel doctor` — act on a shell other than `$SHELL`, which is not always the login shell when pixel is run from an agent's command tool, `env -i`, or cron.
- Retrieval ranking now handles whole words and identifier components, preserves equal lexical-coverage ties, reports semantic and ranking scores honestly, and includes filename-component evidence where source contents alone cannot express an import-oriented query.
- Repository audit coverage and regression fixtures for retrieval, resolver quality, install/update behavior, protocols, and integrations.

### Fixed
- `pixel install` now supports fish. The `claude`/`codex` wrapper block is written in fish syntax to `~/.config/fish/conf.d/pixel.fish`; previously a fish user got POSIX functions in `~/.zshrc` — a file fish never reads, holding syntax fish cannot parse. `pixel uninstall` removes the drop-in it created.
- `pixel doctor`'s `install.shell-wrappers` check compares the installed block against the block pixel would write for the detected shell, instead of looking for loose substrings in the POSIX profile. Wrappers written for another shell (or by an older pixel) now read red instead of green.
- Shell profile selection now identifies `bash` by executable basename, preventing a zsh path that merely contains the substring `bash` from being written to `.bashrc`.

### Changed
- Updated `tree-sitter` to 0.27.0.
- CI now routes feature pull requests to `develop`.

## [0.2.0] - 2026-09-11

### Security
- Removed hardcoded personal email aliases from `resolve_account_alias` — the `--account` flag now accepts full email addresses only.
- Scrubbed all personal emails from test fixtures (replaced with `@example.com`).
- Fixed daemon socket squatting on Linux: socket now lives in `XDG_RUNTIME_DIR` or `~/.cache/pixel/sockets/` (0700) instead of world-writable `/tmp`.
- Added `_pixel_marker` table to `history.db` — a db planted by a hostile repo is detected and wiped before any data is trusted.
- Fixed world-readable sidecar files: `.pixel/` directory is now 0700, flow files and action logs are 0600.
- Disclosed Hugging Face model download egress in README and NOTICE.

### Fixed
- Facts-backed history queries now return lazy-ingest failures instead of silently serving stale results.
- `install.sh`: atomic `mv` instead of in-place `cp` to avoid corrupting the code signature of a running binary on macOS.
- `install.sh`: helpful error message when no releases exist (suggests `cargo install`).
- Homebrew formula: now generated at release time with real SHA256 hashes, uploaded as a release asset. Removed stale `.github/pixel.rb` with placeholder hashes.
- Release CI: `fail-fast: true` + separate release job — a partial build failure no longer publishes a partial release.
- Retired `gitpixel` references in error strings — all user-facing messages now reference `pixel`.

### Added
- CI workflow (`.github/workflows/ci.yml`) — runs `cargo fmt --check`, `cargo clippy -D warnings`, and `cargo test --workspace` on every push and PR.
- `SECURITY.md` with security model documentation and vulnerability reporting instructions.
- `CHANGELOG.md`.
- `.github/dependabot.yml` for automated dependency update PRs.
- MSRV (`rust-version = "1.85"`) in `Cargo.toml`.
- Network access disclosure section in README.
- NOTICE now lists all bundled/transitive dependencies (SQLite, OpenSSL, ONNX Runtime, option-ext, tree-sitter grammars, embedding models).

### Changed
- README tagline softened from "stops coding agents" to "helps coding agents" to match measured performance results.

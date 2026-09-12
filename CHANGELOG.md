# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

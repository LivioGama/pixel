# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Security
- Removed hardcoded personal email aliases from `resolve_account_alias` — the `--account` flag now accepts full email addresses only.
- Scrubbed all personal emails from test fixtures (replaced with `@example.com`).
- Fixed daemon socket squatting on Linux: socket now lives in `XDG_RUNTIME_DIR` or `~/.cache/pixel/sockets/` (0700) instead of world-writable `/tmp`.
- Added `_pixel_marker` table to `history.db` — a db planted by a hostile repo is detected and wiped before any data is trusted.
- Fixed world-readable sidecar files: `.pixel/` directory is now 0700, flow files and action logs are 0600.
- Disclosed Hugging Face model download egress in README and NOTICE.

### Fixed
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

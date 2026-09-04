# Security Policy

## Supported versions

Only the latest release is supported with security fixes.

## Reporting a vulnerability

Email: Open a private security advisory on GitHub (https://github.com/LivioGama/pixel/security/advisories/new).

Do **not** open a public issue for security vulnerabilities.

Please include:
- A description of the vulnerability and its impact
- Steps to reproduce or a proof of concept
- Affected versions (if known)

You will receive a response within 48 hours.

## Security model

Pixel runs locally and processes repository data. Key security boundaries:

- **Daemon socket**: per-user directory (0700) on Linux, per-user TMPDIR on macOS. Socket file is 0600. No cross-user access.
- **History database**: a `_pixel_marker` table proves the db was created by pixel. A db planted by a hostile repo (e.g. `git add -f .pixel/history.db`) is detected and wiped before any data is trusted.
- **Sidecar files** (`.pixel/`): directory is 0700, flow files and action logs are 0600. Flow files may contain fill values (passwords, OTPs) from flow replay.
- **Command/argument injection**: all user input passed to shell commands is sanitized via `ref_guard`. Path traversal in `pixel-install` is blocked.
- **No telemetry**: pixel does not phone home or send usage data anywhere.
- **Network access**: limited to first-use model downloads (Hugging Face) and explicit Git remote operations. See README for details.

## Known limitations

- The daemon does not perform peer-credential checking on incoming socket connections. The 0700 directory permission is the primary access control. On a shared system where another user can bypass directory permissions (e.g. root), additional hardening may be needed.
- The `fastembed` feature downloads models from Hugging Face at runtime. This is opt-in (enabled by default, can be disabled with `--no-default-features`).

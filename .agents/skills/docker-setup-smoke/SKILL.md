---
name: docker-setup-smoke
description: Replay Pixel's new-user setup in Docker without an LLM — install through a release archive, install.sh, the Homebrew tap, or sources from main, a PR head or a pinned commit; then personal Claude/Codex/pi settings kept, idempotent reinstall, first pixel audit, doctor and --fix, project install, disabled classify and uninstall. Use for Linux setup smoke checks, not macOS behavior or real model inference.
---

# Docker setup smoke

Run a Linux binary with a fresh non-root user, pre-existing personal agent
settings and a small Git repository. Docker Desktop on macOS works; the tests
still exercise Linux. No Docker Agentic Platform or model key is needed.

From the repository root:

```bash
bash .agents/skills/docker-setup-smoke/scripts/run.sh
# Another published release archive:
bash .agents/skills/docker-setup-smoke/scripts/run.sh v0.6.1
# The published install.sh, into a dedicated directory:
bash .agents/skills/docker-setup-smoke/scripts/run.sh --installer
# The Homebrew tap on Linuxbrew:
bash .agents/skills/docker-setup-smoke/scripts/run.sh --brew
# Compile current upstream main:
bash .agents/skills/docker-setup-smoke/scripts/run.sh --source main
# Compile a PR's head, including a fork PR (not GitHub's merge ref):
bash .agents/skills/docker-setup-smoke/scripts/run.sh --pr 427
# Replay the exact source SHA recorded by a prior run:
bash .agents/skills/docker-setup-smoke/scripts/run.sh --source <40-character-SHA>
```

## Binary modes

- **Release archive** (default `v0.6.1`): download that exact release's
  architecture-specific archive and verify its published SHA-256.
- **`--installer`**: fetch `releases/latest/download/install.sh` and run it as
  the test user with `PIXEL_INSTALL_DIR=$HOME/opt/pixel/bin`. The installer
  always resolves the latest release, so this mode tests whatever that is on the
  day; `binary-version.txt` and `installer.log` record which one.
- **`--brew`**: `brew install LivioGama/tap/pixel` in the pinned `homebrew/brew`
  image, latest formula. Homebrew's prefix is private to its `linuxbrew` user, so
  that user runs the checks; uninstall must leave the Cellar binary to Homebrew.
  `brew-deps.txt` lists what the formula pulled in.
- **Source** (`--source`, `--pr`): fetch from `https://github.com/LivioGama/pixel.git`
  inside the container, check out the fetched commit detached, and build with
  pinned Rust 1.98.1, `--locked --no-default-features --features model2vec`, debug
  profile without debug info: this tests setup contracts, not release performance
  or musl compatibility. The checkout and build directory are discarded; each run
  is a cold build of several minutes. `pixel --version` must report the fetched SHA.

## What `checks.sh` asserts

1. Personal settings exist before Pixel: `~/.claude/settings.json` (model,
   permissions, a `PreToolUse` Edit|Write hook and a `SessionStart` hook),
   `~/.claude/CLAUDE.md`, `~/.codex/config.toml`, `~/.codex/hooks.json` with a
   user hook, `~/.codex/AGENTS.md`, `~/.pi/agent/{settings.json,AGENTS.md,APPEND_SYSTEM.md}`.
2. Global install keeps every personal key, hook and instruction line.
3. A second global install leaves every managed file byte-identical (backups
   excluded).
4. Metrics configuration persists without an env override.
5. First `pixel audit` in a never-indexed repository prints
   `no code graph yet, building it (first run only)` on stderr, reports coverage
   (`indexed: python 4/4`) and file counts; the second run does not rebuild.
6. `pixel doctor --fail-on yellow` fails in a never-prepared repository, every
   `[red]` line is followed by a `fix:` line, and after `--fix` doctor passes.
7. Project install keeps `AGENTS.md` user instructions and is repeatable;
   disabled classify fails with empty stdout and the disabled diagnostic;
   project uninstall restores `AGENTS.md` byte for byte.
8. Global uninstall restores the personal JSON files to equal values and the
   text files byte for byte, removes the prompt and a binary pixel owns (not a
   Homebrew one), and a second uninstall succeeds.
9. Files left behind that the user did not have before are listed in
   `residue.txt` and counted in a `NOTE` line, not failed.

A personal `PreToolUse` hook matching Bash makes `pixel install --repo` skip the
Claude guard (yellow `claude guard not installed`); the fixture matches
Edit|Write so the guard is installed.

## Environment and isolation

`scripts/Dockerfile` holds the environment only: the pinned base image, curl,
Git, Python and the `tester` user. The runner builds it without a build context
as `pixel-setup-smoke:<mode>`; Docker's layer cache skips the apt step after the
first run. Fetching or compiling pixel and the checks stay in `docker run`, so a
failed step still leaves a container to export evidence from and a moving ref
such as `main` is never served from a cache. The cached apt layer keeps the
packages of its first build: remove the image (`docker image rm
pixel-setup-smoke:<mode>`) to refresh them or reclaim space.

Only this skill's scripts are mounted, read-only. Host home, project, credentials
and Docker socket are not mounted. The runner removes its own container on
completion or interruption, never prunes other resources, and leaves the base
and smoke images cached.

## Evidence and limits

The runner prints a directory under `target/docker-setup-smoke/` containing the
checkout SHA, Docker version, base image digest, built image ID, test user,
invocation, binary provenance, image build log, complete run log and exit status,
plus every JSON report, audit and doctor output, the personal-settings snapshot
and the residue list, exported before container removal, including on failure.
Report the tested binary's version and commit separately from the runner checkout
SHA. Source mode tests fetched upstream code, not uncommitted local changes.

Not covered: macOS (Homebrew on macOS included), shells other than bash,
interactive setup, real agent sessions or model inference.

Prerequisites: Bash, Git, a running Docker engine with BuildKit (Docker Desktop's
default) and network access to GitHub, Docker Hub and crates.io, plus Debian or
Ubuntu mirrors until the smoke image is cached. The image build is not
time-bounded; bootstrap is bounded to 5 minutes (release, installer), 15 minutes
(brew) or 30 minutes (source), and checks to 5 minutes. CPU/memory limits are
4 CPUs/6 GiB.
On failure inspect the saved evidence and fix the cause before replaying.

When changing the runner, run `python3 scripts/test-runner.py` from this skill
directory and ShellCheck on its shell scripts. The contract tests verify mode
selection, invalid-selector rejection, failed-build and failed-run evidence
without Docker; also replay the lifecycle in Docker for the modes affected by the
change.

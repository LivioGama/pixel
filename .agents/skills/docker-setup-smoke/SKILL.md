---
name: docker-setup-smoke
description: Replay Pixel install, config, uninstall and disabled-classify checks in Docker without an LLM, using a published release or sources from main, a PR head or a pinned commit. Use for Linux setup smoke checks, not macOS behavior or real model inference.
---

# Docker setup smoke

Run a Linux binary with a fresh non-root user and a synthetic Git
repository. Docker Desktop on macOS works; the tests still exercise Linux.
No Docker Agentic Platform or model key is needed.

From the repository root:

```bash
bash .agents/skills/docker-setup-smoke/scripts/run.sh
# Another published release:
bash .agents/skills/docker-setup-smoke/scripts/run.sh v0.6.1
# Compile current upstream main:
bash .agents/skills/docker-setup-smoke/scripts/run.sh --source main
# Compile a PR's head, including a fork PR (not GitHub's merge ref):
bash .agents/skills/docker-setup-smoke/scripts/run.sh --pr 427
# Replay the exact source SHA recorded by a prior run:
bash .agents/skills/docker-setup-smoke/scripts/run.sh --source <40-character-SHA>
```

Default: `v0.6.1`, used for the original replay. Download that exact release's
architecture-specific archive and verify its published SHA-256. Do not use
`install.sh`: it resolves the latest release even when fetched from an older tag.

Source mode fetches from `https://github.com/LivioGama/pixel.git` inside the
container, checks out the fetched commit detached, and builds with pinned Rust
1.98.1, `--locked --no-default-features --features model2vec`. It uses the debug
profile without debug info: this tests setup contracts, not release performance
or musl compatibility. The source checkout and build directory are discarded;
there is no shared Cargo target or host Rust prerequisite. Each source run starts
with a cold build and can take several minutes. CPU/memory limits are 4 CPUs/6 GiB.

The script checks global install twice (successful reports, stable prompt),
persisted global metrics configuration without an env override, project install
preserving user instructions, disabled classify (nonzero exit, empty stdout,
disabled diagnostic), project uninstall restoring instructions exactly, and global
uninstall removing binary and prompt. A second global uninstall runs through a
temporary executable copy because the first deletes the installed binary.

Only this skill's scripts are mounted, read-only. Host home, project, credentials
and Docker socket are not mounted. The runner removes its own container on
completion or interruption, never prunes other resources, and leaves the base
image cached.

## Evidence and limits

The runner prints a directory under `target/docker-setup-smoke/` containing the
checkout SHA, Docker version, image digest, invocation, binary build provenance,
complete log and exit status. Source runs also save the fetched ref/SHA and verify
that `pixel --version` reports that SHA. JSON reports and classify output are exported before
container removal, including on failure. Report the tested binary's commit
separately from the runner checkout SHA. Source mode tests fetched upstream code,
not uncommitted local changes; release mode tests the published binary.
Disabled classify does not validate inference. The script does not exercise the
download installer, interactive setup, other shells or every deployed artifact.

Prerequisites: Bash, Git, a running Docker engine and network access to Debian
mirrors, GitHub, Docker Hub and crates.io. Release bootstrap is bounded to 5 minutes,
source bootstrap to 30 minutes and checks to 3 minutes. The resource limits require
sufficient Docker VM memory. Do not add model setup or a broader test suite to this
smoke sequence implicitly.
On failure inspect the saved evidence and fix the cause before replaying.

When changing the runner, run `python3 scripts/test-runner.py` from this skill
directory and ShellCheck on its shell scripts. The contract tests verify source
selection, invalid-selector rejection and failed-run evidence without Docker;
also replay the lifecycle in Docker for the binary modes affected by the change.

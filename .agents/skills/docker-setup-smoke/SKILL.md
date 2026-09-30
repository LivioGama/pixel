---
name: docker-setup-smoke
description: Replay Pixel install, config, uninstall and disabled-classify checks in a disposable Linux container on an existing Docker engine, without an LLM. Use for published-release setup smoke checks, not macOS behavior or real model inference.
---

# Docker setup smoke

Run the published Linux binary with a fresh non-root user and a synthetic Git
repository. Docker Desktop on macOS works; the tests still exercise Linux.
No Docker Agentic Platform or model key is needed.

From the repository root:

```bash
bash .agents/skills/docker-setup-smoke/scripts/run.sh
# Another published release:
bash .agents/skills/docker-setup-smoke/scripts/run.sh v0.6.1
```

Default: `v0.6.1`, used for the original replay. Download that exact release's
architecture-specific archive and verify its published SHA-256. Do not use
`install.sh`: it resolves the latest release even when fetched from an older tag.

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
complete log and exit status. JSON reports and classify output are exported before
container removal, including on failure. Report the tested binary's commit
separately from the checkout SHA: this validates a release, not the working tree.
Disabled classify does not validate inference. The script does not exercise the
download installer, interactive setup, other shells or every deployed artifact.

Prerequisites: Bash, Git, a running Docker engine and network access to Debian
mirrors and GitHub releases. Bootstrap is bounded to 5 minutes; checks to 3 minutes.
On failure inspect the saved evidence and fix the cause before replaying.

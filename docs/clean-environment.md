# Clean environment testing

Pixel's setup and benchmark results are meant to describe a fresh user's
machine. A machine configured over the years — global agent instructions,
installed skills, login state — skews what a new install actually sees
(issue #410's original concern: the OrbStack preset existed so clean-environment
tests stay repeatable). The [docker-setup-smoke
skill](../.agents/skills/docker-setup-smoke/SKILL.md) is that environment,
kept in this repository: it replays a new user's setup in a disposable Linux
container against a scripted fake model, with no real LLM and no changes to
the host.

The skill is self-contained: the runner, the Dockerfile, the checks, the
agent session script and the scripted fake model all live under
[`.agents/skills/docker-setup-smoke/`](../.agents/skills/docker-setup-smoke/),
so any clone of this repository with Docker can run the smoke test.

## One-line runbook

From the repository root, one command spins up the disposable container and
runs the smoke test, then tears the container down:

```bash
sh .agents/skills/docker-setup-smoke/scripts/run.sh
```

That default installs the pinned release archive (`v0.6.1`). The other
channels swap in as an argument: `--installer` (the published `install.sh`),
`--brew` (the Homebrew tap on Linuxbrew), `--source main` (compile current
upstream main), `--source <SHA>` (a pinned commit) or `--pr <number>`. Add
`--agents` first to also replay Claude Code, Codex and pi sessions against
the scripted fake model:

```bash
sh .agents/skills/docker-setup-smoke/scripts/run.sh --agents --source main
```

### Tearing down

Nothing to do: the runner removes its own container on completion or
interruption (a `docker rm -f` in its `EXIT` trap), so a failed run leaves
no container either. Evidence for every run — the run log, exit status, each
JSON report, audit and doctor output, and the saved personal-settings
snapshot — lands in `target/docker-setup-smoke/run-<...>/`, exported before
the container is removed. The built image stays cached so the next run skips
the apt step; remove it to reclaim space:

```bash
docker image rm pixel-setup-smoke:<mode>   # mode: release, installer, brew, source (add -agents)
```

## The full contract

[`SKILL.md`](../.agents/skills/docker-setup-smoke/SKILL.md) is the source of
truth: what the binary modes cover, what `checks.sh` asserts (personal
settings kept, byte-identical reinstall, first audit, `pixel doctor` and
`--fix`, project install, uninstall), how the `--agents` sessions verify
prompt delivery per agent, the evidence layout, prerequisites
(POSIX sh, Git, a running Docker engine with BuildKit) and the parts that are
not covered (macOS, real model inference).
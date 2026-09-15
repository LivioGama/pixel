# scripts/

Every script runs from the repo root. Shell scripts find `pixel` the same
way: `$PIXEL_BIN`, else the installed binary (`command -v pixel`, a
mise/asdf shim, Homebrew or `~/.cargo/bin`), else `target/dev-release/pixel`
then `target/release/pixel`. Install a build that matches the tree with

```bash
pixel self-update --repo . --build "cargo build --profile dev-release -p pixel-cli"
```

(never `cp` into `~/.local/bin` by hand; see CONTRIBUTING.md "Local install loop").

## Gates and contracts (CI runs these)

| Script | Run | What |
| --- | --- | --- |
| `gates.sh` | `scripts/gates.sh [--force] [--mutants]` | fmt, clippy, nextest/test with laptop-safe defaults; exits 0 without compiling when nothing Rust-affecting changed |
| `test-gates.py` | `python3 scripts/test-gates.py` | contract of `gates.sh` (stub cargo in a throwaway repo) |
| `test-prepare.py` | `python3 scripts/test-prepare.py` | contract of `.agents/skills/release/prepare.sh`'s pull request listing (stub gh/cargo, disposable repo, needs `jq`) |
| `test-install.py` | `python3 scripts/test-install.py` | contract of `install.sh` (fake curl/uname, local tarball) |
| `install.sh` | `curl -fsSL https://github.com/LivioGama/pixel/releases/latest/download/install.sh \| sh` | end-user installer, published as an asset of every release: latest GitHub release, checksum, atomic rename into `$PIXEL_INSTALL_DIR` (default `~/.local/bin`) |
| `refresh-guidelines.sh` | `scripts/refresh-guidelines.sh` | re-download the vendored Rust guidelines; exit 1 when rule headings moved |

## Smoke and audits (after `pixel self-update`, before a PR that touches the CLI, hooks or install)

| Script | Run | What |
| --- | --- | --- |
| `pixel-smoke-test.sh` | `scripts/pixel-smoke-test.sh` | the installed binary end to end: `--version`, the guard hook's advisory/rewrite/passthrough contract across Claude, Devin, Codex and Gemini tool names, session-start, `doctor --json`, the install surface, help of the mandatory workflows. Read-only. `PIXEL_SHELL=fish` when `doctor` must check another shell's wrapper |
| `system_audit.py` | `python3 scripts/system_audit.py --pixel target/dev-release/pixel --output /tmp/audit.json` | every CLI leaf against a disposable repo and `$HOME` (retrieval, mutations, env, sniper + MCP, hooks, tasks); JSON report, exit 1 on any FAIL. About one minute |
| `system_audit_recall.py` | `python3 scripts/system_audit_recall.py target/dev-release/pixel --output /tmp/recall-audit.json` | recall index/search/ask/export, both daemons, install/uninstall/doctor/migrate/upgrade in a disposable `$HOME`; no network, no model download |

Both audits set `PIXEL_DAEMON_AUTO_START=0` and clean up after themselves;
`pgrep -fl "pixel daemon"` afterwards must print nothing.

## Demos and benches (need a terminal; the benches need `claude` logged in)

| Script | Run | What |
| --- | --- | --- |
| `pixel-vs-manual.sh` | `scripts/pixel-vs-manual.sh [repo]` | five retrieval tasks, grep/git vs pixel, timings side by side; no agent. Indexes the repo on first run |
| `pixel-excavate-demo.sh` | `scripts/pixel-excavate-demo.sh /path/to/repo` (`PHRASE=…`) | history archaeology, `git log -S` vs `pixel dig-history`; the repo needs `pixel build-index --history` once |
| `pixel-demo.sh` | `SCENARIO=scope scripts/pixel-demo.sh [repo]` | one `claude -p` scenario, baseline (`--safe-mode`, pixel hooks stripped) vs pixel; a few minutes |
| `pixel-bench.sh` | `N=3 scripts/pixel-bench.sh [repo]` | the 4-scenario A/B matrix; 10 to 40 minutes, results in `docs/bench/pixel-bench-results.txt` |
| `pixel-bench-isolated.sh` | `scripts/pixel-bench-isolated.sh [N]` | pixel's doctrine alone vs a blank agent, both under `--safe-mode`; run `pixel-bench.sh` once first (it writes the prompt files) |

The pixel arm of the `claude -p` benches is given the deployed agent prompt
(`~/.local/share/pixel/agent-prompt.md`, the bundled
`crates/pixel-install/assets/pixel-agent-prompt.md` when nothing is
installed) through `--append-system-prompt-file`, exactly as the `claude`
shell wrapper written by `pixel install` does. The scripts call `claude` by
path, so a fish/zsh wrapper function never applies to them; without the
flag the "with pixel" arm would run without pixel's instructions.

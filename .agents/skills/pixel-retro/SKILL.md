---
name: pixel-retro
description: Mine the last 24 h (or a given window) of pixel usage — every repo's `.pixel/actions.jsonl` plus the agent transcripts indexed by `pixel recall` (claude, codex, pi…) — for frictions pixel actually caused, verify each one against the current binary and code, and propose ranked, evidence-backed improvements (bug fix, error message, flag, agent prompt, rule, doc). Suggests only; implements nothing without the user's pick. Use when the user says "/pixel-retro", "retro pixel", "qu'est-ce qui a coincé avec pixel", "améliorations pixel depuis les transcripts", "pixel friction", or asks what to improve in pixel from recent sessions.
---

# Pixel retro

Turn what pixel did to agents over a recent window into a short list of
improvements, each anchored to something that happened. No evidence, no
suggestion: an idea you cannot point to in an action log or a transcript
turn belongs in a separate conversation.

Argument: a window (`24h` by default, `48h`, `7d`, an ISO date). Written
`$W` below.

## Step 0 — Fresh corpus, known baseline

```bash
pixel recall daemon status          # not running → the corpus is stale
pixel recall index --stats          # incremental, a few seconds
pixel recall status                 # last ingest must be minutes old, per agent
pixel --version                     # the binary the suggestions are judged against
pixel commit-history                # what main gained inside the window
```

Stop and say so if an agent's last ingest predates the window: a quiet
window on a stale corpus is not a quiet window. Semantic search may be off
(`semantic: no vectors yet`); `pixel recall ask` then degrades to lexical,
so prefer `search` with explicit regexes.

Read the ledger `~/.local/state/pixel-retro/seen.tsv` (columns:
`date  fingerprint  verdict  ref`; a missing file means an empty ledger).
A fingerprint is `<command>|<error normalised: paths, numbers, ids → _>`.
Anything already `fixed`, `wontfix`, `suggested` or `not-pixel` less than 7 days ago is
skipped unless its count grew.

## Step 1 — Collect the raw signal

Two sources, in this order: the action log is structured and exact, the
transcripts explain what the agent did about it.

**A. Action logs.** Every pixel invocation appends to
`<root>/.pixel/actions.jsonl` (`ts_ms`, `command`, `args`, `cwd`,
`outcome`, `error`, `duration_ms`). The roots come from the sessions
themselves, so a repository outside the usual folders is not missed: take
every session `cwd` in the window, resolve it to its repository root, and
keep the roots that have a log. `pixel recall sessions` caps `--limit` at
200, so `roots.py` pages with `--until` until the window is covered
(subagent sessions included). The `find` adds the worktrees and fixtures no
session ran in:

```bash
python3 .agents/skills/pixel-retro/roots.py $W
find ~/code /tmp/pxwt -maxdepth 4 -path '*/.pixel/actions.jsonl' -mtime -1 2>/dev/null   # -mtime -2 for 48h, and so on
```

then per root `pixel action-log --errors-only --json --limit 500 <root>`,
keeping entries whose `ts_ms` falls in the window. Also pull the slow
successes: `pixel action-log --json --limit 2000 <root>` filtered on
`duration_ms` above 5 000 for read ops (`search-*`, `find-*`, `impact`,
`scope-task`, `pack-context`, `recall`) — a read op that takes seconds is a
friction even when it succeeds.

Drop the noise before counting (verified 2026-09):

- **pixel's own test suite.** `cargo test` spawns the CLI with `cwd` under
  `<pixel>/crates/…` or args under `/var/folders/…`, `$TMPDIR`, `/tmp/`, and
  those runs land in the real repo's `actions.jsonl` (37 of 40 errors in one
  sample were `check-release`/`classify` test cases). Exclude them from the
  counts; report the leak itself once as a finding while it lasts.
- **Refusals that are the contract.** `fast-forward` refusing a non-ff,
  `commit` refusing a dirty or empty stage, a `--request-id` replay: an
  error is a friction only if the agent had to work around it (Step 2 says
  how to tell).

**B. Transcripts.** Regex over the corpus, newest first, always with
`--since $W`. Run each probe once across all repos, then narrow with
`--repo` where the hits cluster:

| Probe | Command |
| --- | --- |
| pixel call that failed | `pixel recall search '(error|Error|unexpected argument|unrecognized|panicked|exited with code [1-9])' --role tool --since $W` then keep hits whose preceding assistant turn runs `pixel` |
| agent bypassed pixel | `pixel recall search '⋮tool Bash \{"command":"[^"]*\b(rg|grep -r|git (log -S|blame|diff|status))\b' --role assistant --since $W` |
| retry loop | three or more `pixel search-*` / `find-*` calls on the same topic within a few turns of one session (`pixel recall show <ref> --turn N..M`) |
| empty or truncated answer | `pixel recall search '\b(unresolved|capped)\b' --role tool --since $W` |
| human complaint | `pixel recall search 'pixel' --role user --human-only --since $W` (`--human-only` alone keeps assistant and tool turns, it only drops injected user text), read for "marche pas", "lent", "pourquoi", "bug", "encore", "wrong" |
| harness or install drift | `pixel recall search '(doctor|install\.|daemon (lock|not running)|stale)' --role tool --since $W` |

Sessions that ran in the pixel repo itself are mostly about pixel's code:
there, "capped" or "error" usually sits in a diff or a test, not in a
pixel answer. Count a hit only when the matched text is pixel's output to
the agent.

`recall` prints timestamps in UTC, the action log's `ts_ms` is epoch: convert
before matching the two.

For each action-log error worth keeping, find its transcript turn with a
distinctive token from `args` (a `--request-id`, a path, a pattern):
`pixel recall search '<token>' --since $W`. The turns after it show the
cost: how many calls the agent spent, and whether it fell back to a native
tool.

## Step 2 — Verify each candidate

Session memory and a single log line both lie. For every candidate:

1. **Reproduce on the current binary.** Re-run the same command only when
   it is a read op (`search-*`, `find-*`, `impact`, `who-calls`,
   `call-path`, `scope-task`, `pack-context`, `repo-state`, `review-changes`,
   `diff`, `commit-history`, `action-log` without `--clear`, `recall`
   `search`/`show`/`sessions`/`status`). A repository mutation (`commit`,
   `push`, `new-branch`, `fast-forward`, `sync-branch`, `build-index`) runs
   only inside a throwaway `git init` fixture, never in the user's repo.
   Never replay a host-wide command (`install`, `uninstall`, `self-update`,
   `daemon start`/`stop`, `recall setup`): read its code path instead. No longer reproduces → check `pixel commit-history` /
   `pixel search-history '<token>'` for the fix and mark it `fixed` with the
   commit, not as a suggestion.
2. **Name the cause in the code.** `pixel find-symbol` / `pixel
   search-content` on the error string, then `pixel pack-context` on the
   function that emits it. A suggestion names the file and function to
   change.
3. **Separate pixel from its surroundings.** An `index.lock` held by
   another git process, a pre-commit hook failing, a daemon lock held by a
   live daemon: pixel's part is at most the error wording or a retry, never
   the root cause. Say which.
4. **Measure the cost.** Occurrences, distinct sessions, distinct repos,
   agents, and turns or seconds spent working around it. Quote the command
   that produced each number (`.agents/rules/measuring.md`).

## Step 3 — Classify and rank

| Kind | Destination |
| --- | --- |
| Bug (wrong answer, crash, hang) | `crates/…` fix + a regression test that fails without it (`.agents/rules/mutation-gate.md`) |
| Error that does not say what to do next | the message at its emit site: name the flag, the valid value, the next command |
| Agent misuse of a flag or command | the prompt pixel installs: `crates/pixel-install/assets/pixel-agent-prompt.md` (and `pixel-subagent-prompt.md`), or the clap help text |
| Agent bypassed pixel because the answer was worse | the command's output (truth markers, caps, ranking) — not the prompt; a stronger "MUST" does not fix a weak answer |
| Slow read op | profile first, suggest only with a measured number |
| Install, doctor or daemon drift | `pixel-install` / `pixel doctor` check that would have caught it |
| Repo rule or skill gap | `.agents/rules/*.md` or `.agents/skills/*/SKILL.md` |
| Not pixel's (user repo, git, another tool) | one line in the report, then drop it |

Score = occurrences × cost per occurrence (turns or seconds) × spread
(repos × agents). Rank on it; break ties toward the smaller change. Keep
the top five; the rest go under "also seen" in one line each.

## Step 4 — Report, then stop

Terminal output, in French, one block per suggestion:

```
### N. <titre court> — <kind>, score S
Symptôme : ce que l'agent a vu (la commande, le message d'erreur exact)
Preuves : <n> occurrences, <s> sessions, <r> repos · agent:id #turn, invocation_id
Reproduit sur <version> : oui / non (corrigé par <sha>)
Cause : <fichier>:<fonction> — une phrase
Proposition : le changement, où, et le test qui le prouverait
Effort : S / M / L
```

End with the dropped items (not pixel's, already fixed, already in the
ledger) and ask which suggestions to implement. Implementation then follows
CONTRIBUTING.md and the PR doctrine, one branch per suggestion.

## Guardrails

- **Transcripts from other repos carry business data.** Quote only pixel
  commands, pixel output and error text. Never copy a customer, subscriber,
  plate, email, amount or commit message body from any repository other than
  pixel, neither into the local report nor into anything public: an issue or
  PR on the pixel repo gets a generic reproduction (`git init` fixture,
  placeholder paths).
- **Nothing leaves the machine without the user's go.** No `gh issue
  create`, no PR, no comment until the user picks a suggestion in the chat.
- **Read-only by default.** Reproductions of mutation ops run in a temp
  fixture; `pixel action-log --clear` is never part of a retro.
- **A miss is not an absence.** "No friction found" means none in the
  indexed agents over the window; say which agents were indexed and fresh.

## Ledger

After the user answers, append one row per reported or dropped item. The
values are data, never source: a fingerprint is built from error text,
which can hold `$(…)`, quotes or newlines. Write the items as a JSON list
with the file-writing tool (not through a shell command), in the scratchpad,
then hand the file to `ledger.py`, which validates every item (verdict one of
`suggested`, `picked`, `fixed`, `wontfix`, `not-pixel`; ref an `agent:id
#turn` or a GitHub URL), refuses the whole file on one bad item, and appends
with tabs and newlines flattened:

```json
[{"fingerprint": "<command>|<normalised error>", "verdict": "suggested", "ref": "claude:5e6585e2 #300"}]
```

```bash
python3 .agents/skills/pixel-retro/ledger.py <scratchpad>/pixel-retro-items.json
```

# Runbook: sniper-discovery experiment

Operational companion to [`sniper-discovery.md`](sniper-discovery.md). That
document is the spec — what is being measured and why. This one covers how to
run it, what the result will mean, and what to try if it produces nothing
useful.

## Running it

The experiment is implemented and executed on `a2` (AL-LINUX03, x86_64, 16
cores). The repository is checked out at `~/gitpixel`; agent trials run against
a second clone at `~/gitpixel-under-test` so that checking out parent commits
never disturbs the working tree.

```bash
# Watch the implementation agent (Ctrl-C stops watching, not the run)
ssh a2 'tail -f ~/devin-sniper.log'

# Has it finished?
ssh a2 'cat ~/gitpixel-experiment-summary.md 2>/dev/null || echo "still running"'

# Run, or re-run, the experiment by hand
ssh a2 'cd ~/gitpixel && git checkout experiment/sniper-discovery \
        && ls scripts/experiments/sniper/ && bash scripts/experiments/sniper/run.sh'

# Read the results
ssh a2 'cat ~/gitpixel/docs/bench/sniper-discovery.md'
```

The entrypoint name is not guaranteed — list `scripts/experiments/sniper/`
before invoking anything.

## Expected cost

| Phase | Estimate | Note |
|---|---|---|
| Install rustup, build `gitpixel` | 10–15 min | `a2` has no cargo; the sniper arm needs the binary for `gitpixel targets` |
| Write miner, shim, runner, scorer | 30–60 min | includes mining tasks from git history |
| 48 measured runs | 20–35 min | 8 tasks × 3 harnesses × 2 arms, capped at 4 concurrent |
| **Total** | **~1–2 h** | |

These are projections, not measurements. The rustup build on a fresh host is
the least predictable step.

## What each outcome means

| Outcome | Verdict |
|---|---|
| Discovery ops fall ≥50%, `gold_recall` within 5 points of control | ✅ The instruction works — enforce it broadly |
| Discovery ops fall **and** `gold_recall` falls >5 points | ❌ The instruction makes agents blind, not efficient. Worse than doing nothing |
| Discovery ops roughly unchanged | ❌ The instruction is ignored; enforcement is the only lever that matters |
| Harnesses disagree in direction | ⚠️ Harness-specific behaviour. Report per harness and claim nothing general |

The second row is the reason `gold_recall` and `gold_precision` are in the
metric set at all. An experiment that measured only "fewer file reads" would
report success while making agents confidently wrong, so a drop in discovery
operations is meaningless until read next to the accuracy numbers.

## If it produces nothing useful

Leads worth pursuing, with a rough confidence that each is where the problem
actually lies.

**85 — The instruction is ignored, and only mechanical enforcement changes
behaviour.** The most likely outcome. Agents are heavily trained to explore, and
one line of prose competes against that prior. If this is what the data shows,
the follow-up is already built: the `gitpixel-targets-guard` hook *blocks*
off-list reads rather than politely asking. Re-run the same design with the
guard active as a third arm — instruction versus enforcement is the interesting
comparison, and it is one this experiment does not currently make.

**70 — The instrument under-counts.** A `PATH` shim only sees processes that are
actually spawned. Harnesses with built-in file tools (gemini's `ReadFile` and
`Glob`, opencode's internal read) bypass it entirely, so their discovery counts
can look artificially low. If two harnesses disagree sharply, suspect the
instrument before believing the finding. The fix is per-harness transcript
parsing, reported in its own column and never summed with shim counts.

**60 — `targets` ranking is the real bottleneck.** If `gold_recall` drops in the
sniper arm, the instruction was followed and the file list was simply wrong.
That moves all the work into ranking quality inside `targets`. There is direct
precedent: the `recall ask` ranker had exactly this shape of defect — it dropped
the one discriminating token before searching, and no amount of downstream
tuning could have recovered it.

**45 — The mined tasks are too easy.** Commit subjects frequently name the module
they touch, so both arms locate the files trivially and any delta disappears
into the noise. Mitigation: strip identifiers and paths from task text more
aggressively, or mine larger cross-cutting commits where the answer is genuinely
distributed.

**30 — Two harnesses is too thin a base.** With gemini absent from `a2`, a single
idiosyncratic harness accounts for half the data. Install it, or treat the
result as an anecdote rather than a measurement.

**15 — Localization is the wrong proxy.** Naming the files is not the same as
doing the work, and the real cost may sit in re-reading during the edit phase
rather than in initial discovery. Testing that needs a full
implement-and-verify loop, which is far more expensive and only worth building
if the cheap version shows a signal first.

## Known limitations, stated up front

- One trial per cell. There is no variance estimate, so medians are directional
  and should never be quoted as precise.
- `gemini` is not installed on `a2` and `cursor-agent` hit a usage limit in an
  earlier trial. Any harness that does not run must be recorded as not-run
  rather than dropped from the writeup.
- The experiment measures localization only. Nothing here says anything about
  whether the agent would then make a correct change.

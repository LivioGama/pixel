# Experiment: does a sniper instruction change how agents find code?

## The question

Coding agents locate code by wandering: `ls` a directory, `grep` a guess, read a
file, read another, widen the search. It burns tokens and time, and it scales
badly with repo size.

`gitpixel targets "<task>"` proposes the opposite: hand the task description to
an index and get back a closed, prioritized file list (P0 start here / P1 likely
/ P2 droppable) in ~20 ms.

Nobody has measured whether telling an agent to do that actually changes its
behaviour. This experiment measures it.

**Hypothesis (H1):** an agent given the sniper instruction performs
substantially fewer discovery operations before it can name the files it needs.

**The hypothesis that must be tested alongside it (H2):** it is still just as
accurate. Fewer reads is worthless — actively harmful — if the agent ends up
naming the wrong files. An experiment that measures only H1 will "succeed"
while making agents confidently blind, so **H1 without H2 is a failed
experiment, not a positive result.**

## Design

Pure **localization** task. Agents are asked to *name the files they would
change*, never to change them. No repository mutation, no build, no test run.
That keeps every run fast, safe, and repeatable, and it isolates the thing under
study — discovery — from everything downstream of it.

### Ground truth from git history

Each task is mined from a real past commit:

- **task text** = the commit's subject reworded as a request, with any file
  paths and identifier names stripped (otherwise the answer is in the question)
- **gold file set** = the files that commit actually modified
- **checkout point** = that commit's **parent**, so the change is not yet present

Mine ~8 commits touching 2–8 files each. Skip merges, pure-formatting commits,
and commits that only touch docs or lockfiles.

### Arms

| Arm | Prompt |
|---|---|
| **A — control** | task only |
| **B — sniper** | task + "First run `gitpixel targets \"<task>\" .`. Work only from the returned P0/P1 list. Do not `ls`, `grep`, or read files outside it." |

Arm B must run `gitpixel index` + `gitpixel graph` once per checkout **before**
the timer starts — index build is a fixed setup cost, not part of the query, and
charging it to every run would confound the comparison. Report it separately.

### Instrumentation — a PATH shim, not transcript parsing

Every harness reports its own actions differently and some do not report them at
all. Do not try to parse three different transcript formats as the primary
instrument.

Instead, prepend a shim directory to `PATH` containing wrappers for `ls`, `find`,
`grep`, `rg`, `cat`, `head`, `tail`, `sed`, `awk`, `tree`, `wc`, and `gitpixel`.
Each wrapper appends `timestamp\tcommand\targs` to `$SHIM_LOG` and then `exec`s
the real binary. This is harness-agnostic and captures what actually happened.

Known limitation, and it must be stated in the writeup: harnesses with **built-in**
file tools (gemini's `ReadFile`/`Glob`, opencode's internal read) bypass the shim.
Parse those from the harness transcript as a secondary source and report
shim-captured and transcript-captured counts in separate columns — never silently
summed, since their coverage differs.

### Metrics

| Metric | Definition |
|---|---|
| `discovery_ops` | shim-logged discovery invocations before the answer is produced |
| `distinct_files_read` | unique paths opened |
| `bytes_read` | total bytes the agent pulled in |
| `wall_seconds` | end to end, excluding index build |
| `gold_recall` | ‖named ∩ gold‖ / ‖gold‖ |
| `gold_precision` | ‖named ∩ gold‖ / ‖named‖ |

`gold_recall` and `gold_precision` are the H2 guard rails.

### Harnesses

`codex`, `gemini`, `opencode` — the three that ran headless and unattended in an
earlier trial. `cursor-agent` hit a usage limit and `claude -p` was not logged in
on the test host; include either only if it authenticates cleanly on a2, and
otherwise record it as not-run rather than quietly dropping it.

Flags that were needed to get past workspace-trust gates:
`codex exec --skip-git-repo-check`, `gemini --yolo --skip-trust -p`,
`opencode run`.

### Scale

8 tasks × 3 harnesses × 2 arms = **48 runs**, plus 3 warmup runs that are
discarded. Run at most 4 concurrently — the earlier session produced badly
inflated latency numbers by running many agent processes at once on a loaded
machine, and that mistake must not be repeated here.

## Pre-registered success criteria

Decide these before looking at results.

| Outcome | Reading |
|---|---|
| `discovery_ops` median drops ≥50% **and** `gold_recall` within 5 points of control | ✅ sniper works |
| `discovery_ops` drops **and** `gold_recall` drops >5 points | ❌ sniper makes agents blind — worse than doing nothing |
| `discovery_ops` roughly unchanged | ❌ the instruction is ignored; enforcement, not instruction, is what matters |
| Results differ in direction across harnesses | ⚠️ harness-specific, not a general property — report per harness, claim nothing global |

## Deliverables

1. `scripts/experiments/sniper/` — task miner, shim generator, runner, scorer.
2. `docs/bench/sniper-discovery.md` — results, following the existing
   `docs/bench/phase0.md` template: machine, versions, exact rerun command,
   table, and an explicit statement of what was *not* measured.
3. Raw per-run logs kept as artifacts, gitignored, so numbers can be re-derived.

## Rules for whoever implements this

- **Report the result the data supports, including a null or negative one.** A
  finding that the instruction changes nothing is a valid, useful outcome and
  must be published as prominently as a positive one. Do not tune the prompt
  until the numbers improve and then report only the tuned run.
- **No cherry-picking tasks.** Fix the 8 commits up front, in a committed file,
  before the first measurement run.
- **Log every dropped run** (crash, timeout, auth failure) with its reason.
  Silent exclusion turns a weak result into a fake strong one.
- Single trial per cell gives no variance estimate; state that plainly rather
  than implying the medians are precise.
- Do not modify the repository under test. If a harness edits files anyway,
  discard that run, note it, and `git checkout -- .` before continuing.

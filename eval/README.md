# Injection-balance eval loop

Measures whether pixel's injected guidance helps or hurts a coding harness,
and gates any candidate payload so it can **never** score worse than running
with no pixel guidance at all.

## Shape

- `scenarios/*.json` — one natural question each, with a pattern rubric
  (`must` = earned points, `never` = penalties for known-wrong claims).
- `run.sh` — builds a scratch worktree + Claude config per arm, deploys the
  arm's payload to `~/.local/share/pixel/agent-prompt.md` (backed up, restored
  on exit), runs the CLIs, then scores.
- `score.py` — mechanical scoring of transcripts (answered? pattern hits?
  turns/tokens/cost). No LLM judge: reproducible.
- `gate.py --candidate <arm> --baseline baseline` — PASS only if the
  candidate answers every scenario, scores >= baseline everywhere, and burns
  <= 1.5x baseline turns. Any single regression fails.

## Arms

| arm | hooks | AGENTS.md block | session-start payload |
| --- | --- | --- | --- |
| `baseline` | none | stripped | n/a |
| `on` | one clean pixel set | committed (current `RULES_BODY`) | deployed asset (14.3 KB today — Claude Code truncates it to a ~2 KB preview) |
| `v<name>` | one clean pixel set | `variants/<name>/rules-body.md` | `variants/<name>/agent-prompt.md` |

## Run

```bash
CLIS=claude ARMS="baseline on" eval/run.sh          # defaults: 3 scenarios, 20-turn budget
CLIS=claude ARMS="vslim" eval/run.sh                # then:
python3 eval/gate.py --results eval/results --candidate vslim
```

## CLIs

`claude` is the scored default (headless `-p`, stream-json). `agy` works via
`-p --output-format stream-json` (its global pixel plugin is toggled off for
non-`on` arms and restored). `codex` and `pi` are extension points: drop an
executable `eval/clis/<name>.sh` that reads `$WT`, `$CFG`, `$PROMPT`, `$OUT`
and emits a final-answer JSON line; until then run.sh skips them with rc 9.

## Results

Transcripts land in `results/` (gitignored). `results/scores.json` is the
machine-readable scoreboard consumed by `gate.py`.

### Measured on this branch (claude / deepseek-v4-flash, 20-turn budget, Sept 2026)

| arm | hooks | s1 score | s2 score | s3 score | mean |
| --- | --- | --- | --- | --- | --- |
| `baseline` | none | 15.5 (n2) | see gate | 12.0 (n2) | 12.7–13.0 |
| `on` (14.3 KB doc, full hooks) | packet + relays | 11.5–13 | 6.5 | 11.5 | 11.3 |
| `vslim` (2 KB doc, full hooks) | packet + relays | 9.5–10 | 10.0–10.7 | 11–12 | 12.0 |
| `vquiet` (2 KB doc, session-start only) | none mid-session | 16.0 | see gate | 12.0 | 12.7+ |

Two findings survived repetition:

1. The **prompt-submit task packet and mid-session relays** — not the
   session-start text — drag answers down on mechanism questions: arms with
   the full hook set cluster below baseline on s1 regardless of payload size;
   the quiet arm matches or beats baseline everywhere it has runs, at lower
   cost (it injects ~2 KB once and saves exploration).
2. The pre-#475 payload was measured losing ~4 KB of doctrine to Claude
   Code's hook truncation; pixel's own `CLAUDE_INLINE_CONTEXT_LIMIT` (10 000
   UTF-16 units, #443) had already been exceeded by growth.

### Quiet profile (no code change needed)

```bash
pixel config task-boundary off   # silences the prompt-submit task packet
pixel config metrics off         # silences the PostToolUse metrics relay
```

`post-tool-use` (edit blast-radius) has no config toggle yet — tracked as
follow-up together with making the quiet set the install default.

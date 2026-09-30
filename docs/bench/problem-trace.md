# Problem chapter trace: one scoping task, with and without Pixel

The home's Problem chapter shows every tool call one agent made, with no
Pixel installed, before it answered a scoping task. This note is where that
trace comes from, and it records both arms of the recording, since the page
shows only one.

- **Command:** `REF=v0.6.1 PAR=3 EFFORT=medium docs/motion/scripts/record-demo.sh <out> 11 claude-opus-5-5`,
  then `docs/motion/scripts/trace.ts <out>` run from a copy of the script, so
  its `runs.json` did not overwrite `docs/motion/src/demo/` (AgentDemo keeps
  its own recording).
- **Recorded:** 2026-09-30 (UTC), Claude Code 2.1.286, `claude-opus-5-5` at
  medium effort, Pixel 0.6.1 (`ca1ce9e`), 11 runs per arm. Every run works in
  its own copy of a fresh repository holding `v0.6.1`'s history only, with its
  own daemon and `PIXEL_SESSION_ID` (`meta.txt`). The Pixel arm gets the hooks
  `pixel install` writes; its task packet reached all 11 runs (`packets.txt`).
- **Task (the prompt, verbatim):** "Task: retry a leased push when the remote
  branch moved. Before anyone edits anything, find the files that would need
  to change and list them, most important first, with one line each on why.
  Do not edit any file." The English-answer instruction goes in the system
  prompt (`--append-system-prompt`), not in the message.
- **Archived:** `problem-trace/runs.json` (every run of both arms: each tool
  call, its label as the agent issued it, the lines and bytes ÷ 4 of its
  result, the run's duration, turns and Claude Code's reported cost),
  `meta.txt`, `packets.txt`. The raw stream-json files are not archived.

## What the page shows

`scripts/problem-trace.py` writes `website/data/problem_trace.toml` from the
`vanilla` arm alone: the run whose call count is the arm's median (10), and
among those tied, the one whose lines of output are closest to the median
(`vanilla-3`, 1,206 lines, the median itself). `trace.ts` picks the run with
the median wall time instead, which here is `vanilla-9`, an 8-call run; the
page does not use that choice. Calls are classified by their first command
(the script's docstring has the rule). The page shows no duration and no
cost, and compares nothing: it states what one arm did.

## Both arms (medians over 11 runs)

| | without Pixel | with Pixel |
| --- | --- | --- |
| tool calls | 10 (7 to 15) | 7 (6 to 9) |
| searches (`Grep`, shell `grep`/`rg`) | 5 | 0 |
| reads (`Read`, shell `sed`/`cat`/`head`) | 4 | 3 |
| lines of tool output | 1,206 | 970 |
| tool-result tokens, bytes ÷ 4 | 14,074 | 11,060 |
| turns | 11 | 8 |
| wall time | 48.0 s | 44.7 s |
| Claude Code `total_cost_usd` | $0.40 | $0.32 |

`pixel` commands count as neither search nor read. One task, 11 runs per
arm, one model: the wall-time gap is inside the spread of either arm, and
none of these rows is a general claim about Pixel. Tokens are estimates of
tool results only, not the whole context; cost is the CLI's figure, not an
invoice.

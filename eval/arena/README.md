# eval/arena — head-to-head: raw codex vs retrieval tools vs Pixel

Seven dockerized arms on identical footing (same repo snapshot, codex version,
auth, sandbox, prompts): `raw` (no tool), `semble`, `graft`, `stacklit`,
`gitnexus`, `gortex`, `pixel`. Each tool is installed and wired to codex per
its own docs (MCP registration or instruction channel). Measures answer
quality (the shared scenario rubrics), token consumption (codex usage events),
token saving vs raw, and wall time; `rank.py` orders arms by quality then
tokens.

```bash
docker build -f eval/arena/Dockerfile.base -t pixel-arena-base:latest eval/arena
REPO_SNAPSHOT=/path/to/pixel-clone bash eval/arena.sh                 # all arms, all tasks
REPO_SNAPSHOT=... bash eval/arena.sh --tasks "s3-rename-impact"       # one task, all arms parallel
```

One container per arm runs all tasks (prep once — indexing, MCP wiring —
then the scored runs); arms launch in parallel. Per-arm rw snapshots keep one
tool's index artifacts out of another's repo. Model: `CODEX_MODEL`
(default `gpt-6-luna`) at `CODEX_EFFORT` (default `high`). Auth is mounted at
runtime, never baked.

## First round — task s3 (rename impact), gpt-6-luna @ high, n1 per arm

| rank | arm | score | tokens | time | saving vs raw |
| --- | --- | --- | --- | --- | --- |
| 1 | graft | 11/12 | 104,395 | 39s | −24,132 (−30%) |
| 1 | **pixel** | **11/12** | **114,962** | **29s** | −34,699 (−43%) |
| 3 | gitnexus | 10/12 | 286,794 | 53s | +206,531 (+257%) |
| 4 | stacklit | 7/12 | 80,868 | 22s | +605 (+1%) |
| 5 | raw | 3/12 | 80,263 | 26s | — |
| 5 | semble | 3/12 | 80,225 | 33s | +38 (+0%) |
| — | gortex | no data | — | — | daemon never registered the repo (track-before-ready hang) |

Raw's own n1 variance is large (the same task scored 11/12 in an earlier
probe) — rankings at n1 are indicative, not conclusive; n≥3 per arm is the
follow-up before any verdict. Semble's flat-vs-raw result is expected: its
lazy indexing only helps when the agent calls it, and codex needed to be told
its MCP tools existed (fair-wiring question per tool — see below).

Known first-round integration findings: gortex's `track` ran before its
daemon accepted registrations (hang — now fixed with a registration poll);
semble's lazy index only helps if the agent calls its MCP tools, which raw
codex had no reason to discover — the fair-wiring question per tool is
queued. Stacklit's derived map lands in `/root/.codex/AGENTS.md`, which
codex reads, so its 7/12 already includes that channel.

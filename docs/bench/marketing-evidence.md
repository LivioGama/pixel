# Marketing evidence audit — Task 398

Checked 2026-09-30 against main `0f36481` and the HTML served by
https://pixel-cli.dev/ (downloaded with `curl -L --fail`). The public home
showed −30% duration, −38% context and −30% API cost, also reused by its CTO
card, savings page, Claude Code page, answers and machine-readable metadata.
This change prepares the sources only; it does not deploy them.

| Claim → source | Protocol / version / limits | Treatment and consumers |
| --- | --- | --- |
| −30% duration, −38% “context”, −30% cost → `docs/motion/src/demo/{runs.json,meta.txt}`, `scripts/trace.ts` | 2026-09-23, Sonnet 5, Claude Code 2.1.281, Pixel 0.5.0 (`aaa1a3b`), repo `e585b69`, one scoping task, 11 paired runs per arm. Medians: 86.516/60.726 s, 17,268/10,655 estimated tool-result tokens, $0.3942892/$0.2735356 Claude-reported cost. Prompt appended, no hooks; trace labels confirm demo-file reads by `vanilla-9`, `pixel-7` and `pixel-4`. Tokens are rounded bytes ÷ 4 per tool result, not full context; cost is not an independently checked invoice. | Remove gains and replay from home; retain historical table, JSON, metadata and rendered assets with caveats. Correct `/answers/`, `/for/claude-code/`, FAQ JSON-LD and `llms.txt`; remove calculator calibration. |
| 42.9/47.6 s → `docs/motion/README.md` | Newer Opus medium summary with install hooks, same scoping task, no gain. Raw runs outside repo; exact versions, date, sample size and token/cost results unavailable. Model and wiring both differ from Sonnet trial. | Visible on home, benchmarks, Claude Code and token answer. No general speed claim or invented provenance. No favourable-task replacement. |
| −31%/−29% scoping, +1.5 s lookup, slower recovery → `docs/bench/measured-performance.md`, session log and raw text files | 2026-08-30, starting commit `865facf`, subsequent binary/doctrine fixes; 3 reps/cell. Clean/isolated tables use means. Full-stack arm loads global rules; isolated arm has no hooks and almost no Pixel calls. First table's baseline purity unknown. | Keep as dated historical evidence and disclose protocols; no headline extrapolation to current install. |
| 79.7–97.2%, median 94.5%; Transformers 57,058/1,649 (97.1%) → `read-savings.md`, `website/data/read_savings.toml`, `scripts/bench-read-savings.sh` | Pixel 0.5.0, September 2026, 8 retained pinned large files. UTF-8 file/output bytes ÷ 4, floored; no agent or tokenizer. Python signature counts are coarse checks, not completeness proofs. React 99.4% excluded for Flow parse failure (20/125 functions). Bodies and later reads omitted. | Main proof remains home/README, token wall, benchmarks, comparisons, answers and share card. Label volume and token estimation, archived version and selected-file limits; keep rows unchanged. |
| 94.5–96.1% on Pixel files → `website/data/savings.toml`, README at `632b3685` | Historical shunt-shaped file/answer comparison, byte-based token estimates. Exact binary version absent from cited publication. | Retain historical benchmark table, distinct from well-known-file sample and agent trials. |
| Requests 95,693/11,447 (−88%), `models.py` 10,365/641 (−94%) → website benchmarks | Archived Pixel 0.5.1, Requests `611c616`, `pixel audit` / `list-signatures`, estimated tokens = floored bytes ÷ 4. No agent or billed cost. | Keep own-code verification examples with explicit units/version. |
| 41–83%, 798 operations → same historical README | Maintainer action-log snippet/candidate-pool ratio, not actual agent baseline; no whole-session accounting. | Retain with historical label on benchmarks; remove from token answer as current-session proof. |
| Monthly tokens/dollars and README badge → `/savings/`, `kept-rates.html` | User inputs × archived kept-file ratios; dollars use chosen uncached input price (default $3 is a placeholder). Assumes each full read replaced by outline; excludes subsequent bodies and other session costs. Range is observed min/max, not a confidence interval. | Keep conditional estimate, remove agent/cost gain calibration; update SSR, JavaScript badge and `llms.txt` wording. Arithmetic and input keys unchanged. |
| README scope and six job animations → `docs/motion/src/PixelComparison.tsx`, `website/data/jobs.toml` | Scripted workflow diagrams, not measured A/B trials. “Measured” accounting concerns local output volume; token/time savings also use estimators. | Keep assets with explicit captions in README, home and motion notes. Archived AgentDemo retains original pixels/data as history, not current proof. |
| Meta description, FAQ/answer JSON-LD, OG/Twitter card → `hugo.toml`, `head.html`, data answers/objections, `og/card.html` | Descriptions and structured answers render from corrected data; image uses archived Transformers row, not a session; `_index.md` overrides the default home description. | Replace unqualified token/cost labels with volume/estimated tokens; regenerate card. |

Other home figures (GitNexus recall 0.86/0.84, 153/432 ms and ~4,160/~19,700
prompt/schema tokens; cold index 2/12 s) have separate protocols in
`vs-gitnexus.md` (Pixel 0.4.0, 2026-09-21, 29 cases) and `cold-index.md`
(2026-09-27, three cold clones of one private Rails repo, M2; binary version
not recorded). They are tool/index measurements, not evidence for agent
speed or invoiced cost. No new campaign was run for this audit.

# motion

[Remotion](https://www.remotion.dev) sources for the animations in the root
README and on the website, all rendered into `docs/examples/`. They share the
website's identity (`src/theme.ts`: forest-green ground, coral for what an
agent wastes, green for what Pixel hands back, Handjet for display text);
`src/fonts.ts` loads the three faces from `public/fonts/` before any frame
renders.

| Composition ID | Source | Output basename |
|---|---|---|
| `AgentDemo` | `src/AgentDemo.tsx` + `src/demo/*.json` | `pixel-agent-demo` |
| `PixelComparison` | `measuredSavingsSpec` | `pixel-measured-comparison` |
| `PixelImpact` | `impactSpec` | `pixel-impact-comparison` |
| `PixelScope` | `scopeSpec` | `pixel-scope-comparison` |
| `PixelRollback` | `rollbackSpec` | `pixel-rollback-comparison` |
| `PixelPublish` | `publishSpec` | `pixel-publish-comparison` |
| `PixelRewrite` | `rewriteSpec` | `pixel-rewrite-comparison` |

The six `Pixel*` compositions render `ComparisonScene` (`src/ComparisonScene.tsx`)
with one spec from `src/PixelComparison.tsx`, at 1600×1000, 30 fps, 8 s.

## The agent demo

`AgentDemo` replays two recorded Claude Code runs side by side on one clock:
the same task, the same model, the same bare setup, one side with the agent
prompt `pixel install` deploys. Nothing in it is written by hand: every
command, time and token count comes from a recording.

1. `scripts/record-demo.sh <dir> [reps] [model]` runs both arms `reps` times,
   each pair started together, and stores every stream-json event with its
   arrival time. The script's header lists what the two arms share.
2. `bun scripts/trace.ts <dir>` keeps each arm's median-time run (never the
   best) as `src/demo/{vanilla,pixel}.json`, writes every run to
   `src/demo/runs.json` and copies the recording's `meta.txt` (commit,
   model, CLI and Pixel versions, prompt hash, task).
3. The summary at the end shows the median of each metric over all runs, and
   its headline follows those medians rather than assuming a win.

Re-record after a change to the agent prompt or to the commands it names,
and update `recorded` in `src/Root.tsx`.

## Render

```bash
bun install
bunx remotion studio src/index.ts        # preview
scripts/render.sh                        # every composition
scripts/render.sh PixelScope AgentDemo   # some of them
```

`scripts/render.sh` needs `ffmpeg` and `img2webp` (`brew install ffmpeg webp`)
and writes three files per composition into `docs/examples/`:

- `<name>.mp4`: 1600×1000 H.264, played by the website;
- `<name>.jpg`: its last frame, the website's poster;
- `<name>.webp`: 800×500 at 15 fps, embedded by the root README, since GitHub
  renders an animated image inline but not a video.

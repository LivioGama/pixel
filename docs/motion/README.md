# motion

[Remotion](https://www.remotion.dev) sources for the animations in the root
README and on the website, all rendered into `docs/examples/`. They share the
website's identity (`src/theme.ts`: forest-green ground, coral for what an
agent wastes, green for what Pixel hands back, Handjet for display text);
`src/fonts.ts` loads the three faces from `public/fonts/` before any frame
renders.

| Composition ID | Source | Output basename |
|---|---|---|
| `PixelComparison` | `measuredSavingsSpec` | `pixel-measured-comparison` |
| `PixelImpact` | `impactSpec` | `pixel-impact-comparison` |
| `PixelScope` | `scopeSpec` | `pixel-scope-comparison` |
| `PixelRollback` | `rollbackSpec` | `pixel-rollback-comparison` |
| `PixelPublish` | `publishSpec` | `pixel-publish-comparison` |
| `PixelRewrite` | `rewriteSpec` | `pixel-rewrite-comparison` |

The six `Pixel*` compositions render `ComparisonScene` (`src/ComparisonScene.tsx`)
with one spec from `src/PixelComparison.tsx`, at 1600×1000, 30 fps, 8 s.

## Render

```bash
bun install
bunx remotion studio src/index.ts        # preview
scripts/render.sh                        # every composition
scripts/render.sh PixelScope PixelImpact # some of them
```

`scripts/render.sh` needs `ffmpeg` and `img2webp` (`brew install ffmpeg webp`)
and writes three files per composition into `docs/examples/`:

- `<name>.mp4`: 1600×1000 H.264, played by the website;
- `<name>.jpg`: its last frame, the website's poster;
- `<name>.webp`: 800×500 at 15 fps, embedded by the root README, since GitHub
  renders an animated image inline but not a video.

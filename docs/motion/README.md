# motion

[Remotion](https://www.remotion.dev) sources for the six comparison
animations in the root README (`docs/examples/pixel-*-comparison.webp`).
`src/index.ts` registers `src/Root.tsx`, where every composition renders
`ComparisonScene` (`src/ComparisonScene.tsx`) with one spec from
`src/PixelComparison.tsx`, at 1600×1000, 30 fps, 240 frames (8 s).

| Composition ID | Spec | README asset |
|---|---|---|
| `PixelComparison` | `measuredSavingsSpec` | `pixel-measured-comparison.webp` |
| `PixelImpact` | `impactSpec` | `pixel-impact-comparison.webp` |
| `PixelScope` | `scopeSpec` | `pixel-scope-comparison.webp` |
| `PixelRollback` | `rollbackSpec` | `pixel-rollback-comparison.webp` |
| `PixelPublish` | `publishSpec` | `pixel-publish-comparison.webp` |
| `PixelRewrite` | `rewriteSpec` | `pixel-rewrite-comparison.webp` |

Install the dependencies, from this directory:

```bash
bun install
```

Preview every composition in the Remotion Studio:

```bash
bunx remotion studio src/index.ts
```

Render one composition by its ID (`remotion.config.ts` sets PNG frames and
overwrites an existing output):

```bash
bunx remotion render src/index.ts PixelScope out/pixel-scope.mp4
bunx remotion render src/index.ts PixelScope out/pixel-scope.gif --codec=gif
```

The committed README assets are 800×500 animated WebPs converted from such a
render; that conversion step is not scripted here.

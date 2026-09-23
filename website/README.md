# website/

The Hugo site published at <https://liviogama.github.io/pixel/> by
`.github/workflows/pages.yml` on every push to `main` that touches
`website/**` or `docs/examples/**`.

## Run it locally

```bash
cd website
mise install                                # the Hugo pin in mise.toml
mise exec -- hugo server --disableFastRender
```

Plain CSS, no Sass and no Node: the standard Hugo build is enough.

## Where things live

- `layouts/index.html`: the whole landing page, with its two small scripts
  (the hero grid, the job tabs) inline at the bottom.
- `content/docs.md`: the `/docs/` page, rendered by `layouts/_default/single.html`.
  `crates/pixel/tests/cli/docs_drift.rs` reads it, so every `` `pixel <command>` ``
  quoted there must exist. Keep it in step with `README.md` when install or
  wiring changes.
- `data/scope.toml`: the real `pixel scope-task` run the hero grid replays.
  Refresh the task, the index size and every position together.
- `data/jobs.toml`, `data/savings.toml`: the six recordings and the token
  table, both from the README.
- `assets/css/main.css`: one stylesheet, tokens first; dark mode redefines
  the tokens only.
- The recordings are not copied: `hugo.toml` mounts `../docs/examples/` at
  `/examples/`.

`public/` and `resources/` are build output and are ignored.

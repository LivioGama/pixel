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
  inline at the bottom. The hero board draws one pad per indexed file and
  routes a copper trace from each P0 pad to its file name; the routing is in
  that script. The job tabs load each recording only when opened.
- `content/docs.md`: the `/docs/` page, rendered by `layouts/_default/single.html`.
  `crates/pixel/tests/cli/docs_drift.rs` reads it, so every `` `pixel <command>` ``
  quoted there must exist. Keep it in step with `README.md` when install or
  wiring changes.
- `data/scope.toml`: the real `pixel scope-task` run the hero grid replays.
  Refresh the task, the index size and every position together.
- `data/jobs.toml`, `data/savings.toml`: the six recordings and the token
  table, both from the README.
- `assets/css/main.css`: one stylesheet, tokens first. Dark is the default;
  a light system preference redefines the tokens only. Copper marks
  structure (pads, traces, steps), the brand green marks answers: keep that
  split when adding anything.
- The recordings are not copied: `hugo.toml` mounts `../docs/examples/` at
  `/examples/`.

`public/` and `resources/` are build output and are ignored.

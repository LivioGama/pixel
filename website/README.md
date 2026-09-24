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

- `layouts/index.html`: the whole landing page, with its scripts inline at
  the bottom. The token wall draws one square per 25 tokens of a full file
  read (`data/savings.toml`, first row) and burns down to what the same
  question costs through Pixel once it scrolls into view. The scope board
  draws one pad per indexed file and routes a trace from each P0 pad to its
  file name. The job tabs load each video only when opened, and the agent
  demo in "Measured on whole agent tasks" plays once when it scrolls into
  view. Videos never autoplay under reduced motion: the poster shows the
  finished state and the controls are there.
- The "Fair questions" block in `layouts/index.html` restates facts from
  `SECURITY.md`, `docs/bench/measured-performance.md` and the graph's
  grammars (`$langs`, from `crates/pixel-graph/src/extract.rs`): change it
  when they change.
- `content/benchmarks.md`: the `/benchmarks/` page. Every number the landing
  page shows lives here with its sample size, its source in `docs/bench/`,
  and the cases where Pixel loses. Add a claim to the landing page only once
  it is on this page.
- `content/docs.md`: the `/docs/` page, rendered by `layouts/_default/single.html`.
  Every fenced block gets a Copy button from
  `layouts/_default/_markup/render-codeblock.html`.
  `crates/pixel/tests/cli/docs_drift.rs` reads it and `benchmarks.md`, so
  every `` `pixel <command>` `` quoted in either must exist. Keep it in step with `README.md` when install or
  wiring changes.
- `data/scope.toml`: the real `pixel scope-task` run the hero grid replays.
  Refresh the task, the index size and every position together.
- `data/jobs.toml`, `data/savings.toml`: the six animations and the token
  table, both from the README.
- `layouts/404.html`: the not-found page GitHub Pages serves for any
  unknown path under the site.
- `assets/css/main.css`: one stylesheet, tokens first, dark only. Headlines
  use Handjet, a variable pixel face (`ELSH` 2 draws square elements; the
  hero title animates it from 0 once). `partials/head.html` requests only
  the font axes the stylesheet uses: widen the request before using a new
  weight or axis. Coral means tokens wasted, green
  means what Pixel returns: keep that split when adding anything.
- The videos are not copied: `hugo.toml` mounts the MP4s and JPEG posters
  of `../docs/examples/` at `/examples/`. `docs/motion/` renders them
  (`scripts/render.sh`), along with the WebPs the root README embeds.

`public/` and `resources/` are build output and are ignored.

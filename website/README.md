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
  the bottom. The hero opens on who Pixel is for (four agent marks and the
  count of the rest, from the `$agents` list the Compatibility grid also
  draws), then states the problem in the reader's words before naming the
  category. The token wall draws one square per 25 tokens of a full file
  read (`data/savings.toml`, first row) and burns down to what the same
  question costs through Pixel once it scrolls into view. A Without / With
  Pixel chip names the side shown, the saving (`saved`, floored as the
  stats round it) appears once the wall has burnt, and the caption turns
  a square into lines of that same file; the survivors
  gather into a P of exactly that many squares, built from the count by
  the script, so one square always means 25 tokens. The scope board
  draws one pad per indexed file and routes a trace from each P0 pad to its
  file name. The job tabs load each video only when opened, and the agent
  demo in "Measured on whole agent tasks" plays once when it scrolls into
  view. Videos never autoplay under reduced motion: the poster shows the
  finished state and the controls are there.
- Every `h2` of the landing page assembles out of pixels the first time it
  scrolls into view (the "Titles assemble" script in `layouts/index.html`,
  `.is-assembling` / `.is-assembled` in `main.css`): the title is sampled
  from a canvas drawn with Handjet at the browser's word positions, on a
  0.06 em grid phased onto the glyphs, and its green words light up last.
  The text never leaves the DOM; no JavaScript, reduced motion or a
  missing Handjet leaves the titles static. The hero title keeps its own
  `materialize` animation, which follows the splash.
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
  table, both from the README. A job's `text` holds one line on a wide
  screen (about 70 characters): the tab panel reserves one line there and
  clips past it, so a longer text would lose its end rather than move the
  video.
- `layouts/partials/backdrop.html`: the animated background on every page,
  one fixed canvas under the content (pixel dust, circuit traces with
  packets, drifting pixel agents, each layer scrolling at its own rate),
  kept faint by `.backdrop` in the stylesheet. It also darkens the nav once
  the page scrolls. Reduced motion gets one still frame.
- `assets/logos/`: the agents' marks in "Plugs into the agent you already
  use", in each brand's own colours (Lobe Icons' colour variants: Claude,
  Codex, Cursor, Gemini, Devin, Antigravity); the monochrome marks (Pi,
  Copilot, OpenCode, Windsurf) use `currentColor`, from Simple Icons and
  pi.dev's favicon. Add an agent there
  and in the `$agents` list of `layouts/index.html` together.
- Chapters: a landing section that opens on a `<p class="chapter">` gets a
  numbered divider (CSS counter in `main.css`) and a square in the
  right-edge rail (wide screens). A chapter name is a category, never the
  headline's own words ("Architecture" over "How it works."), and the home eases onto a section start only when scrolling stopped within
  80 px of it (a script, not CSS scroll-snap, whose `proximity` bounced).
  The nav's bottom edge fills with the scroll progress.
- Links: anything that leaves the page opens in a new tab, in the templates
  and in Markdown through `layouts/_default/_markup/render-link.html`; only
  same-page anchors (`#install`) stay in place. Keep it for new links.
- `static/cursor.svg`, `static/cursor-link.svg`: the pixel-arrow cursor, the
  green one over anything clickable, for fine pointers only.
- `layouts/partials/icon.html`: the pixel icons, 8x8 bitmaps drawn in
  `currentColor`. Add one as a new row list; call it with
  `(dict "name" "star" "size" 16)`.
- The agent demo never starts on its own: its poster is
  `pixel-agent-demo-start.jpg`, the recording's first frame, which
  `docs/motion/scripts/render.sh` writes beside the last-frame poster.
- `layouts/partials/splash.html`: the home's splash (pixels landing into
  the brand square, then the wordmark), about two seconds, skippable.
  `partials/head.html` decides it before the first paint: home only, once
  per tab session (`sessionStorage`), never under reduced motion. The token
  wall waits for its `pixel:splashdone` event. A CSS fallback fades it out
  at 4.5 s if the script never finishes.
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

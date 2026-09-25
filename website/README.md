# website/

The Hugo site published at <https://liviogama.github.io/pixel/> by
`.github/workflows/pages.yml` on every push to `main` that touches
`website/**`, `docs/examples/**` or `crates/pixel/Cargo.toml` (the version
the JSON-LD states).

## Run it locally

```bash
cd website
mise install                                # the Hugo pin in mise.toml
mise exec -- hugo server --disableFastRender
```

Plain CSS, no Sass and no Node: the standard Hugo build is enough.

After editing a shortcode, restart `hugo server`: it keeps serving the
pages that call it with the old output, even when the content file is
touched (Hugo 0.166; a plain `hugo` build is right).

## Where things live

- `layouts/index.html`: the whole landing page, with its scripts inline at
  the bottom. The hero opens on who Pixel is for (four agent marks and the
  count of the rest, from `data/agents.toml`, which the Compatibility grid
  also draws, each mark linking to its `/for/<slug>/` page), then states the problem in the reader's words before naming the
  category. The token wall draws one square per 25 tokens of a full file
  read of a well-known file (`data/read_savings.toml`, the row marked
  `wall`) and burns down to what the same
  question costs through Pixel once it scrolls into view. A Without / With
  Pixel chip names the side shown, the saving (`saved`, floored as the
  stats round it) appears once the wall has burnt, and the caption quotes
  the kept files' range and median; the survivors
  gather into a P of exactly that many squares, built from the count by
  the script, so one square always means 25 tokens. The order after the
  hero shows before it proves: the scope example comes first, then the
  numbers band (its comparisons with GitNexus mean little before a visitor
  has seen what Pixel does), then the chapters of proof. The scope board
  draws one pad per indexed file and routes a trace from each P0 pad to its
  file name. The job tabs load each video only when opened, and the agent
  demo in "Measured on whole agent tasks" plays once when it scrolls into
  view. Videos never autoplay under reduced motion: the poster shows the
  finished state and the controls are there.
- At rest until the first scroll: the scroll-driven effects (the
  `data-reveal` fades, the assembling titles, the counting numbers) prime
  only what is still below the fold when the visitor first scrolls
  (`afterFirstScroll` at the top of the page script, `.is-primed` in
  `main.css`). A full-page capture, a link preview or a crawler that never
  scrolls sees every block in its final state; keep that for any new
  effect.
- Every `h2` of the landing page assembles out of pixels the first time it
  scrolls into view (the "Titles assemble" script in `layouts/index.html`,
  `.is-assembling` / `.is-assembled` in `main.css`): the title is sampled
  from a canvas drawn with Handjet at the browser's word positions, on a
  0.06 em grid phased onto the glyphs, and its green words light up last.
  The text never leaves the DOM; no JavaScript, reduced motion or a
  missing Handjet leaves the titles static. The hero title keeps its own
  `materialize` animation, which follows the splash.
- The "Alternatives" and "Teams" chapters restate `content/benchmarks.md`
  (GitNexus, the well-known files, JevBench, the demo runs) and shunt's own
  README for its claim and requirements. Alternatives keeps, on every card,
  the rows where the other tool wins; Teams gives each role (developer, lead,
  harness engineer, CTO) three proofs, each a number or a command that
  exists. Change a figure on the benchmarks page first, then in
  `data/alternatives.toml` (the Alternatives cards and the `/vs/` pages)
  or the Teams chapter, then in the repository README's "Why" list and
  `static/llms.txt`, whose "Evaluating Pixel against alternatives" section
  gives the same results to an assistant comparing tools for its user and
  links each `/vs/` page.
- `data/alternatives.toml`: one entry per tool Pixel is compared with, read
  through `layouts/partials/alternatives.html` (placeholders such as
  `{kept.range}` or `{big.full}` filled from `data/read_savings.toml`, an
  unknown one fails the build). The tools marked `home` are the
  Alternatives cards; every tool is a `/vs/<slug>/` page
  (`content/vs/<slug>.md`, front matter `tool = "<slug>"`, rendered by
  `layouts/vs/single.html`; `/vs/` itself is `layouts/vs/list.html`). A
  page's Markdown is prose only: the short answer, the table and "Where …
  wins" (required, the build fails without it) come from the data. Each
  entry's `measure` is shown above its table: `head-to-head`,
  `published-figure` (different samples), `baseline` (grep) or `design`
  (not benchmarked, no figure). A tool or category `/benchmarks/` does not
  measure (a language server, an editor's index) gets a page only as
  `design`: no figure at all, "Not benchmarked" above its table, and the
  build fails on a digit in its answer, wins or rows, or on a `bench`.
  No figure `/benchmarks/` does not show: a card figure from the other
  tool's own docs (shunt's claim) gets a `page_other` the page shows
  instead. A page's "Updated" date is the later of its Git date and the
  entry's `checked`: bump `checked` when you change its rows. Each page
  carries its own JSON-LD (`WebPage`, `BreadcrumbList`) that points at the
  home's `#website` and `#software` by `@id` rather than declaring them
  again. `docs_drift.rs` reads `content/vs/*.md` and this file.
  `static/llms.txt` is not a template: its `/vs/` links spell the domain,
  so they change with `baseURL`.
- `data/objections.toml`: the "Fair questions" chapter, and the home's
  `FAQPage` JSON-LD, from one list (`layouts/partials/objections.html`
  renders it for both, so the markup never says what the page does not).
  Answers are HTML paragraphs; a figure that lives elsewhere is a
  placeholder (`{big.full}`, `{langs.count}`, `{bench}`…, listed at the top
  of the file) the partial fills in, and an unknown one fails the build.
  They restate `SECURITY.md`, `docs/bench/measured-performance.md` and the
  graph's grammars (`$langs` in the partial, from
  `crates/pixel-graph/src/extract.rs`): change them when those change.
  `crates/pixel/tests/cli/docs_drift.rs` reads this file and
  `layouts/index.html` too, so every `<code>pixel …</code>` must exist.
- `layouts/partials/wall-row.html`: the `data/read_savings.toml` row marked
  `wall`, for the token wall, the objections and the share card's alt text.
- `content/benchmarks.md`: the `/benchmarks/` page. Every number the landing
  page shows lives here with its sample size, its source in `docs/bench/`,
  and the cases where Pixel loses. Add a claim to the landing page only once
  it is on this page.
- `content/docs.md`: the `/docs/` page, rendered by `layouts/_default/single.html`.
  Every fenced block gets a Copy button from
  `layouts/_default/_markup/render-codeblock.html`.
  `crates/pixel/tests/cli/docs_drift.rs` reads it and `benchmarks.md`, so
  every `` `pixel <command>` `` quoted in either must exist. Keep it in step with `README.md` when install or
  wiring changes. Its "What pixel install wires" table is the
  `agents-install` shortcode, from `data/agents.toml`.
- `data/agents.toml`: the agents the site names, in the home's order, and
  what Pixel does for each: `wiring` (`install`, `plugin` or `rules`), the
  files `pixel install` and `pixel install --repo` write, the plugin command
  or rules file, the `pixel doctor` checks. The hero, the Compatibility
  grid, the `/docs/` wiring table and the `/for/` section all read it.
  `crates/pixel/tests/cli/docs_drift.rs` runs a real `pixel install` and
  `pixel install --repo` into empty directories and fails when the files
  they write differ from the lists, when a check id is not one of
  `pixel doctor`'s or an agent check of the doctor belongs to no agent, and
  when `static/llms.txt` misses an agent page: a pull request that changes
  what `pixel install` writes updates this file in the same commit.
- `content/for/`: one page per agent (`/for/<slug>/`), each a front matter
  naming its `agent` and the `{{% agent-setup %}}` shortcode
  (`layouts/shortcodes/agent-setup.html`), which writes the setup sections
  from the data: install, what `pixel install` writes, the plugin or rules
  file, the per-repository guard, the check, the removal.
  `layouts/for/single.html` adds the breadcrumb (visible and as
  `BreadcrumbList` JSON-LD) and the other agents; `layouts/for/list.html`
  groups `/for/` by wiring, and the build fails on an agent with no page or
  an unknown wiring. No figure on a page unless it is on `/benchmarks/` for
  that agent: today only Claude Code's.
- `data/scope.toml`: the real `pixel scope-task` run the hero grid replays.
  Refresh the task, the index size and every position together.
- `data/read_savings.toml`: the well-known files `scripts/bench-read-savings.sh`
  measures (method in `docs/bench/read-savings.md`). The token wall, its
  caption's range and median and the `/benchmarks/` table
  (`layouts/shortcodes/read-savings.html`) all read it: re-run the script
  and replace the rows, never one number by hand, then re-render the share
  card (`og/render.sh`), whose figures come from the wall's row.
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
  and in `data/agents.toml` together, with its page `content/for/<slug>.md`.
- Community and updates: the line under the Install block and the footer
  link to GitHub Discussions; the footer's "Follow releases" is the
  releases' Atom feed (no newsletter, nothing to sign up for), also
  announced by a `<link rel="alternate">` in the head.
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
- Search and share metadata, all in `layouts/partials/head.html`: titles
  that say "code index" (a bare "Pixel" is Google's phone), Open Graph and
  Twitter tags, and on the home one JSON-LD `@graph` (`WebSite`,
  `SoftwareApplication` with a `disambiguatingDescription`,
  `SoftwareSourceCode`, `FAQPage`). Its `softwareVersion` is the CLI
  crate's: `hugo.toml` mounts `../crates/pixel/Cargo.toml` as
  `data/cli.toml`, which the release's `prepare.sh` bumps, and the Pages
  workflow redeploys on it. Every absolute URL goes through `baseURL`
  (`absURL`, `.Permalink`), so a new domain is one line in `hugo.toml`.
- `layouts/robots.txt` names the sitemap Hugo writes (`/`, `/docs/`,
  `/benchmarks/`, `/vs/` and each comparison, `/for/` and each agent page;
  the 404 stays out). `enableGitInfo` dates each page by the
  last commit that touched its source: the sitemap's `lastmod` and the
  "Updated" line under the `/docs/`, `/benchmarks/` and `/for/` titles
  (`layouts/_default/single.html`, `layouts/for/`). The Pages workflow checks out the full
  history for it; a shallow clone would date every page by HEAD.
- `og/`: the share card. `og/render.sh` fills `og/card.html` with the
  wall's row of `data/read_savings.toml`, renders it at 1200x630 with
  `agent-browser` and writes `static/og.png`; re-run it when that row
  changes, never edit the PNG. It shows the wall's final state (one square
  per 25 tokens, the P built as the home builds it) and the same saving,
  floored, as the wall. Hugo ignores the folder.
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

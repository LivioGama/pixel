# HTML report format

One self-contained HTML file in `${TMPDIR:-/tmp}`, never in the repo.
Tailwind and Mermaid come from CDNs; everything else is inline. Adapted from
`mattpocock/skills` (MIT, see [LICENSE](LICENSE)).

## Scaffold

```html
<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <title>Architecture review: pixel</title>
    <script src="https://cdn.tailwindcss.com"></script>
    <script type="module">
      import mermaid from "https://cdn.jsdelivr.net/npm/mermaid@11/dist/mermaid.esm.min.mjs";
      mermaid.initialize({ startOnLoad: true, theme: "neutral" });
    </script>
    <style>
      .seam { stroke-dasharray: 4 4; }
      .leak { stroke: #dc2626; }
      .deep { background: linear-gradient(135deg, #0f172a, #1e293b); }
    </style>
  </head>
  <body class="bg-stone-50 text-slate-900 font-sans">
    <main class="max-w-5xl mx-auto px-4 sm:px-6 py-12 space-y-12">
      <header>...</header>
      <section id="candidates" class="space-y-10">...</section>
      <section id="top-recommendation">...</section>
    </main>
  </body>
</html>
```

## Header

The commit (`git rev-parse --short HEAD`), the date, the scope (`$A` or the
hot spots and the churn window), and a compact legend: solid box = module,
dashed line = seam, red arrow = leakage, thick dark box = deep module. No
introduction paragraph.

## Candidate card

One `<article>` per candidate. The diagram carries the weight; prose is
sparse.

- **Title**: names the deepening ("Fold the doctor checks behind one
  registry").
- **Badges**: strength (`Strong` emerald, `Worth exploring` amber,
  `Speculative` slate) and the dependency category (`in-process`,
  `local-substitutable`, `ports & adapters`, `mock`; DESIGN.md).
- **Files**: monospaced list (`font-mono text-sm`).
- **Before / after diagram**: two columns side by side on desktop, stacked
  under 640 px.
- **Problem**: one sentence.
- **Solution**: one sentence, no interface yet.
- **Wins**: bullets of six words or fewer, in glossary terms ("locality:
  one place to fix", "leverage: one interface, 14 callers").
- **Cost** (a small table, the numbers from SKILL.md Step 2): mutants listed
  for the rewritten files, estimated diff and PR split, `ARCHITECTURE.md`
  sections owed, `pixel impact` risk of the main symbol.
- **Settled-decision callout** when it applies: one amber line naming the
  `ARCHITECTURE.md` section, the rule or the closed issue it reopens, and
  why it is worth reopening.

If a diagram needs a paragraph to be understood, redraw the diagram.

## Diagram patterns

Pick per candidate and vary them; a report where every diagram looks the
same reads as generated.

- **Mermaid flowchart** for call flow and dependencies ("X calls Y calls Z,
  and look at the mess"); `classDef` colours leaks red and the deep module
  dark. A sequence diagram suits "before: six round trips; after: one".
- **Hand-built boxes and arrows** (divs plus an inline SVG overlay) when the
  "after" must read as one thick-bordered module with its internals faded,
  which Mermaid will not draw with the right weight.
- **Cross-section**: stacked bands for the layers a call crosses; before,
  six thin bands doing nothing; after, one thick band.
- **Mass diagram**: two rectangles per module, interface and
  implementation; before, nearly the same height; after, a short interface
  over a tall implementation. Illustration only: depth is not a lines ratio.
- **Call-graph collapse**: a tree of calls as nested boxes, then the same
  tree folded into one box with the now-internal calls faded inside.

Keep each diagram around 320 px tall so before and after fit side by side.
Module labels in `text-xs uppercase tracking-wider`, so they read as a
schematic, not as UI.

## Top recommendation

One larger card: the candidate's name, one sentence on why it comes first
(usually the best ratio of locality gained to cost), and an anchor link to
its card.

## Tone

Plain, concise, the architecture nouns from DESIGN.md without ceremony:
"The doctor module is shallow: its interface nearly matches its
implementation." "Install state leaks across the seam." "Two adapters
justify the seam: the socket in production, `Service::handle` in tests."
No "easier to maintain", no "cleaner code": those words are not in the
glossary and carry nothing. No hedging; if a sentence can be a bullet, make
it one, and if a bullet can go, cut it.

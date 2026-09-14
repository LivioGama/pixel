# @pixel/sniper

JS companion for the pixel **sniper** error sink. Every error from every
common surface — browser runtime, node process-level, HTTP 5xx, Vite
transform/HMR, vitest — lands at throw-time in ONE structured local store
(`pixel list-errors …`), enriched with source-mapped frames and package
provenance, so the agent's first look is the last look.

All records ship through the Rust core: `pixel list-errors report --json -`
(one serialized child process at a time; the sink never breaks your dev
server, your app, or your test run).

## Adoption — 2 lines

```ts
// vite.config.ts
import { sniperDevPlugin } from "@pixel/sniper/vite";
export default defineConfig({ plugins: [sniperDevPlugin(), /* … */] });
```

```ts
// src/main.tsx (or any app root — installs on import)
import "@pixel/sniper/client";
```

Optional — one vitest line:

```ts
// vitest.config.ts
import { SniperReporter } from "@pixel/sniper/vitest-reporter";
export default defineConfig({ test: { reporters: ["default", new SniperReporter()] } });
```

## What each piece captures

| Import | Captures |
| --- | --- |
| `@pixel/sniper/vite` | run fingerprint (pid, port, git HEAD, lockfile + `.vite/deps` hashes), node uncaught/unhandled, HTTP 5xx (4KB excerpt, 499 skipped), vite transform errors, hmr-update / full-reload / dep-optimized events, and the browser ingest endpoint `POST /__sniper/report` (dev-only, loopback-gated) |
| `@pixel/sniper/client` | window `error`, `unhandledrejection`, `console.error` (reentrancy-guarded patch), HMR rev tracking, plus `report(err, {values})` and `swallow(err, label, values?)` — the empty-catch replacement (depth-2 / 1KB serialization, secret-key redaction) |
| `@pixel/sniper/vitest-reporter` | ONE record per failing run (`"2 failed | 10 passed (3.1s)"` + structured failures capped at 50), `test-pass` event when green; disabled under `CI` |
| `pixel list-errors run -- <cmd>` | Wraps any command (no JS needed): tees its output live, mirrors its exit code, records structured errors on failure. Parsers: `tsc` (one record per TS code + a summary), Minitest and RSpec (see below), `rubocop` (one `lint` record per offense: `file`, `line`, `column`, `severity`, `cop`, `correctable`). Anything else: one `run-wrapper` record with the last 100 lines in `extra.tail` and the full output in raw_fallbacks |

### Ruby test runners under `sniper run`

The runner is detected from the output, not the command line, so
`pixel list-errors run -- bundle exec rails test`, `bin/rails test`,
`bundle exec rspec` and `parallel_rspec` all work. A failing run records:

- one record per failing test, surface `minitest` or `rspec`, `kind`
  `failure` (assertion) or `error` (exception). The message is
  `Class#test: first line`; `extra` carries `testClass`, `testName`, `file`,
  `line`, `message` (full text, 1 KB cap), `expected`/`actual` when the
  assertion printed them, and `rerun` (`bin/rails test path:line`,
  `rspec ./path:line`). `frames`/`stack_raw` hold the backtrace filtered to
  project paths (gems, the Ruby stdlib and `<internal:…>` frames dropped);
- one `summary` record last (`12 runs, 10 assertions, 1 failures, 1 errors,
  0 skips` / `7 examples, 2 failures, 1 pending`) with `extra.counters` and
  the failing tests (capped at 50), which is what `pixel list-errors test` shows.

A green run records a `test-pass` event with `runner` and the counters
(`runs`, `assertions`, `failures`, `errors`, `skips` for Minitest;
`examples`, `failures`, `pending` for RSpec; `passed` for both), so
`pixel list-errors test` answers `passing` for Ruby suites as it does for vitest.
Rails parallel workers and `parallel_tests` are handled: noise between
failure blocks is ignored and the last summary line (the total) wins.
Existing `tsc`/`vitest` records are unchanged.

Browser payloads are enriched **server-side** at ingest: V8/JSC stacks are
parsed, dev-server URLs are mapped back to physical files (vite module-graph
transform maps for `/src` modules, sibling `.map` files for
`/node_modules/.vite/deps` chunks), and every mapped frame gets package
provenance — `{name, version, path}` plus `dup_paths` when the same package
exists at more than one physical path (the duplicate-module smoking gun).

## Options

```ts
sniperDevPlugin({
  bin: "/path/to/pixel",     // default: $PIXEL_BIN, then `pixel` on PATH
  endpoint: "/__sniper/report", // default shown
});

new SniperReporter({
  bin: "/path/to/pixel",     // same resolution as the plugin
  repo: process.cwd(),          // passed as --repo
});
```

Binary resolution order everywhere: `options.bin` → `$PIXEL_BIN` →
`pixel` on PATH. Resolution failure at server start is a loud, actionable
error; every capture path after that is try/catch + reentrancy-guarded and
never throws.

## Agent workflow contract

Add to the adopting repo's CLAUDE.md / AGENTS.md:

> After any edit or failed run, one call — `pixel list-errors since <cursor>` —
> replaces dev-log reading, console polling, curl-polling, and test-log
> grepping. Every listing footer prints the next cursor. Drill down with
> `pixel list-errors show <id>` (frames + provenance + values + run fingerprint
> + ±30s correlated events); check "did my edit land?" with
> `pixel list-errors hmr --file src/x.tsx`.

## Development

```sh
bun install
bun test        # unit + live round-trip through target/debug/pixel + real vite dev server
bun run typecheck
```

The test suite builds the Rust binary (`cargo build -p pixel-cli`) once per
test process, prints its SHA-256, and pipes golden envelopes through the real `pixel list-errors report`
ingest path — the JSON contract is verified against the actual Rust parser,
not a mock.

To reuse a binary built in the current verification session, supply both
`PIXEL_TEST_BINARY` (absolute path) and `PIXEL_TEST_BINARY_SHA256` (its exact
SHA-256); a missing or mismatched digest fails before integration tests.
The sink reporter kills a stuck CLI child after 10 seconds so `flush()` and
subsequent queued reports cannot hang forever (`SinkReporterOptions.timeoutMs` is
configurable through its constructor options).

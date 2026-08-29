# @gitpixel/sniper

JS companion for the gitpixel **sniper** error sink. Every error from every
common surface — browser runtime, node process-level, HTTP 5xx, Vite
transform/HMR, vitest — lands at throw-time in ONE structured local store
(`gitpixel sniper …`), enriched with source-mapped frames and package
provenance, so the agent's first look is the last look.

All records ship through the Rust core: `gitpixel sniper report --json -`
(one serialized child process at a time; the sink never breaks your dev
server, your app, or your test run).

## Adoption — 2 lines

```ts
// vite.config.ts
import { sniperDevPlugin } from "@gitpixel/sniper/vite";
export default defineConfig({ plugins: [sniperDevPlugin(), /* … */] });
```

```ts
// src/main.tsx (or any app root — installs on import)
import "@gitpixel/sniper/client";
```

Optional — one vitest line:

```ts
// vitest.config.ts
import { SniperReporter } from "@gitpixel/sniper/vitest-reporter";
export default defineConfig({ test: { reporters: ["default", new SniperReporter()] } });
```

## What each piece captures

| Import | Captures |
| --- | --- |
| `@gitpixel/sniper/vite` | run fingerprint (pid, port, git HEAD, lockfile + `.vite/deps` hashes), node uncaught/unhandled, HTTP 5xx (4KB excerpt, 499 skipped), vite transform errors, hmr-update / full-reload / dep-optimized events, and the browser ingest endpoint `POST /__sniper/report` (dev-only, loopback-gated) |
| `@gitpixel/sniper/client` | window `error`, `unhandledrejection`, `console.error` (reentrancy-guarded patch), HMR rev tracking, plus `report(err, {values})` and `swallow(err, label, values?)` — the empty-catch replacement (depth-2 / 1KB serialization, secret-key redaction) |
| `@gitpixel/sniper/vitest-reporter` | ONE record per failing run (`"2 failed | 10 passed (3.1s)"` + structured failures capped at 50), `test-pass` event when green; disabled under `CI` |

Browser payloads are enriched **server-side** at ingest: V8/JSC stacks are
parsed, dev-server URLs are mapped back to physical files (vite module-graph
transform maps for `/src` modules, sibling `.map` files for
`/node_modules/.vite/deps` chunks), and every mapped frame gets package
provenance — `{name, version, path}` plus `dup_paths` when the same package
exists at more than one physical path (the duplicate-module smoking gun).

## Options

```ts
sniperDevPlugin({
  bin: "/path/to/gitpixel",     // default: $GITPIXEL_BIN, then `gitpixel` on PATH
  endpoint: "/__sniper/report", // default shown
});

new SniperReporter({
  bin: "/path/to/gitpixel",     // same resolution as the plugin
  repo: process.cwd(),          // passed as --repo
});
```

Binary resolution order everywhere: `options.bin` → `$GITPIXEL_BIN` →
`gitpixel` on PATH. Resolution failure at server start is a loud, actionable
error; every capture path after that is try/catch + reentrancy-guarded and
never throws.

## Agent workflow contract

Add to the adopting repo's CLAUDE.md / AGENTS.md:

> After any edit or failed run, one call — `gitpixel sniper since <cursor>` —
> replaces dev-log reading, console polling, curl-polling, and test-log
> grepping. Every listing footer prints the next cursor. Drill down with
> `gitpixel sniper show <id>` (frames + provenance + values + run fingerprint
> + ±30s correlated events); check "did my edit land?" with
> `gitpixel sniper hmr --file src/x.tsx`.

## Development

```sh
bun install
bun test        # unit + live round-trip through target/debug/gitpixel + real vite dev server
bun run typecheck
```

The test suite builds the Rust binary (`cargo build -p gitpixel-cli`) if it is
missing and pipes golden envelopes through the real `gitpixel sniper report`
ingest path — the JSON contract is verified against the actual Rust parser,
not a mock.

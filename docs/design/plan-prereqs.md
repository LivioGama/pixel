# `pixel plan` — verification prerequisites (`prereqs`)

Status: spec. Not implemented.

## Problem

`pixel plan` ends every checklist with `Verify all plan targets in the running
build`. The item is hollow: it does not say what verification *needs*. When the
plan targets an auth-gated route, a Stripe integration, or a DB-backed state,
an agent that never obtained a test account, env keys, or seeded data cannot
execute that item — and today nothing tells it so. The failure mode observed in
the field: agents mark the verify item done on unit-test evidence alone and
never ask for the credentials a human verifier would reach for first (log in
as the client, inspect real state, fix the cause, not the consequence).

## Goal

A plan must state its **verification preconditions** as tracked checklist
items, computed deterministically from the code the plan touches:

- auth-gated target → "verification needs a logged-in session: replay flow
  `X`, or ask the human for a test account"
- provider SDK in target → "verification needs `STRIPE_TEST_*` env keys"
- DB access in target → "reproduce with the client's real state before fixing"
- none detected → unchanged output

The prereq item is a gate artifact, not advice: it sits in `.pixel/plan.json`
like any finding, shows as undone in `pixel plan --status`, and stays undone
until a tool turn (a replayed flow, a provisioned key) lets the agent honestly
mark it `--done`.

## Non-goals

- Picking *which* account or *which* flow is correct — detection asserts that
  a precondition exists, the human or a tagged flow answers it.
- Executing anything: no browser, no DB, no network. Read-only like every
  other plan query.
- Blocking code edits — prereqs gate *verification claims*, not editing.
- DB state inspection — out of retrieval's domain; the item names the need.

## Design

### 1. Post-pass, not a query

`Prereqs` is **not** a `PlanQuery` variant. It runs after
`run_plan_queries` over the file set the findings produced, so every plan —
classified or explicit `--query` — gains preconditions without a new spelling.
Opt-out: `--no-prereqs` (symmetric to `--no-verify`).

Scan scope, in order:

1. Every distinct `file` in the findings.
2. One hop of the `imports` table: `resolved_file_id` of each finding file's
   import specs, plus files that import a finding file (a route delegating
   auth to middleware must still flag). Cap the closure at 50 files.

Files are read from disk under `root` — no new index, no new extraction.

### 2. Signal detection

Regex over file contents, per language by extension. Each hit yields
`Prereq { kind, evidence_file, evidence_line, detail }`.

| Kind | Signals (representative, catalog lives in code) |
| --- | --- |
| `Env` | `process.env.NAME`, `import.meta.env.NAME`, `Deno.env.get`, `std::env::var`, `env!`, `os.Getenv`, `os.LookupEnv`, `os.environ`, `os.getenv`, `ENV[`, `ENV.fetch`, `System.getenv` — collect var names |
| `Auth` | `getServerSession`, `auth(`, `requireAuth`, `currentUser`, `useSession`, `getSession`, `withAuth`, `clerkMiddleware`, imports of `next-auth`, `@clerk/*`, `@supabase/auth*` |
| `Provider` | import specifiers from a catalog: `stripe`, `@supabase/*`, `@clerk/*`, `openai`, `@anthropic-ai/*`, `twilio`, `@sendgrid/*`, `resend`, `@aws-sdk/*`, `@firebase/*` — each maps to an env prefix (`STRIPE_`, `SUPABASE_`, …) |
| `Db` | `@prisma/client`, `drizzle-orm`, `mongoose`, `sequelize`, `knex`, `pg`, `mysql2`, `sqlx`, `rusqlite`, `diesel`, `sea_orm`, `tokio_postgres` |

Dedup by `(kind, detail)`; keep the first evidence site per pair.

### 3. Flow matching

For each `Auth` prereq, call `pixel_flow::list` and match flows whose `tags`
contain `auth` or `login` (exact match on lowercased tag). First match names
the replay command in the item text. No match → the item says so:

```
BLOCKED: <file> is auth-gated — verify via `pixel replay-flow replay client-login`
BLOCKED: <file> is auth-gated — no login flow saved; ask the human for a test
         account or record one via `pixel replay-flow save`
```

That second line is the feature: the plan *asks for the account* because the
agent provably cannot verify without it.

### 4. Rendering

`PlanFinding` gains two fields:

```rust
pub enum FindingKind { Site, Prereq }
pub kind: FindingKind,        // default Site — old state files still load
pub blocking: bool,           // true for every Prereq
```

`#[serde(default)]` on both so existing `.pixel/plan.json` files deserialize.

Prereq items render **before** site findings, unnumbered gate block:

```
Prerequisites (verification gates):
- [ ] BLOCKED: app/billing/page.tsx is auth-gated — replay `client-login`
- [ ] env keys required by crates/pay/src/stripe.rs: STRIPE_SECRET_KEY, STRIPE_WEBHOOK_SECRET
- [ ] DB-backed state: reproduce with the client's real data before fixing

1. [ ] Map 4 findings across 3 files (ranked by fan-in)
2. [ ] …
n. [ ] Verify all plan targets in the running build (gates above first)
```

The verify line appends `(gates above first)` when prereqs exist.
`--format compact` prints `prereq: <text>` lines first; `--format json` adds
`"prereqs": [...]` beside `findings`, and `verify` keeps its meaning.

### 5. State

Prereqs merge into `plan_state` like findings (`merge` keyed on label).
`--done N` on a prereq = the human/agent confirms provisioning. `--prune`
drops prereqs the latest plan no longer detects — correct by construction.
Numbering: prereqs take their own `- [ ]` block, not the numbered list, so
site numbering stays stable across runs.

### 6. Epistemics honesty

Same envelope as the rest of the graph: detection is a lower bound. Regex
misses indirect auth (middleware two hops away, server-side-only gating) and
env read through wrappers. Item wording is "verification requires" — a
requirement claim, never "this will fail". Zero prereqs means "none detected",
not "none needed"; the spec text and item wording both say so.

## Files touched

| File | Change |
| --- | --- |
| `crates/pixel-graph/src/plan.rs` | `prereqs(store, root, files)` post-pass + signal catalog + tests |
| `crates/pixel/src/plan_cmd.rs` | `--no-prereqs`, render block, JSON field |
| `crates/pixel/src/cli.rs` (or wherever `PlanOptions` is built) | flag plumbing |
| `crates/pixel-install/assets/pixel-agent-prompt.md` | document the gate block + "prereq undone = cannot claim verified" |
| `ARCHITECTURE.md` | command surface: flag row only if the table lists flags |
| `changelog.d/plan-prereqs.added.md` | `**cli:** …` fragment |
| `crates/pixel/tests/cli/plan_cli.rs` (or nearest) | CLI coverage |

## Tests

- auth-gated fixture file + no saved flows → BLOCKED "ask the human" item.
- same + a flow tagged `auth` → item names `pixel replay-flow replay <name>`.
- `import Stripe from "stripe"` + `process.env.STRIPE_SECRET_KEY` → one env
  item listing the var, deduped across two files reading it.
- middleware one hop away (route imports `auth.ts`) still flags.
- `#[serde(default)]` round-trip: a pre-change `plan.json` loads.
- mutation-gate shapes per `.agents/rules/mutation-gate.md` — every branch in
  the classifier/catalog match asserts observable output.

## Open questions

1. Should `pixel scope-task` get the same post-pass (its P0 file list is the
   same shape)? Spec says yes eventually, out of scope for v1.
2. Does a prereq item deserve `--blocking` severity separate from `HIGH`, or
   is the `blocking` flag enough? Current answer: flag is enough, severity
   stays `HIGH` so it sorts first.
3. Flow tag vocabulary: `auth`/`login` only, or a `tags:` convention the
   `replay-flow save` docs should standardise? Lean: match both, document
   `auth` as the canonical tag.

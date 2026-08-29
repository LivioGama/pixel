import { afterEach, describe, expect, test } from "bun:test";
import { mkdtemp, realpath, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createIngestRunner, ingestSearchIndex } from "../src/search/ingest.ts";
import {
  matchExpression,
  pathLifecycle,
  rankedSearch,
  sanitizeQuery,
  tokenLifecycle,
} from "../src/search/query.ts";
import { openSearchStore, type SearchStore } from "../src/search/store.ts";
import {
  commitFile,
  createRepository,
  type TestRepository,
} from "./helpers/repository.ts";

const cleanups: Array<() => Promise<unknown>> = [];
afterEach(async () => {
  for (const cleanup of cleanups.splice(0)) await cleanup();
});

const indexedFixture = async (): Promise<{ repository: TestRepository; store: SearchStore }> => {
  const repository = await createRepository();
  cleanups.push(repository.cleanup);
  const stateRoot = await realpath(await mkdtemp(join(tmpdir(), "usable-git-search-query-")));
  cleanups.push(() => rm(stateRoot, { recursive: true, force: true }));
  const store = openSearchStore(join(repository.path, ".git"), { stateRoot });
  cleanups.push(async () => store.close());
  await commitFile(repository, "src/App.svelte", "<h1>hello svelte</h1>\n", "feat: svelte app shell");
  await commitFile(repository, "src/util.ts", "export const util = 1\n", "chore: unrelated noise");
  await commitFile(repository, "docs/notes.md", "plain notes\n", "docs: more noise");
  await repository.run("rm", "-q", "src/App.svelte");
  await commitFile(
    repository,
    "src/engine.ts",
    "export const render = () => 'single pass'\n",
    "feat: single-pass HTML generation engine, replace Svelte",
  );
  await ingestSearchIndex(store, repository.path);
  return { repository, store };
};

describe("query sanitizer", () => {
  test("splits plain terms, drops single characters, and keeps quoted phrases", () => {
    expect(sanitizeQuery("gain ledger")).toEqual({
      units: ["gain", "ledger"],
      prefixLast: true,
    });
    expect(sanitizeQuery('"exact phrase" extra x')).toEqual({
      units: ["exact phrase", "extra"],
      prefixLast: false,
    });
    expect(sanitizeQuery("a ! .")).toBeNull();
    expect(sanitizeQuery("run_all")).toEqual({ units: ["run_all"], prefixLast: true });
  });

  test("neutralizes every FTS operator so hostile input never reaches MATCH raw", () => {
    const hostile = [
      "foo:bar(",
      "NEAR(a b)",
      'col:"x" OR *',
      "a AND b NOT c",
      "^caret {set}",
      '""""',
      "-;drop table commits;--",
    ];
    for (const input of hostile) {
      const sanitized = sanitizeQuery(input);
      if (!sanitized) continue;
      for (const pass of ["and", "or"] as const) {
        const expression = matchExpression(sanitized, pass);
        expect(expression).toMatch(/^"[^"]+"\*?( (AND|OR) "[^"]+"\*?)*$/);
      }
    }
  });

  test("sanitized hostile inputs execute against real FTS tables without syntax errors", async () => {
    const { store } = await indexedFixture();
    const hostile = ["foo:bar(", "NEAR(a b)", "a* OR ^b", "engine){}[]", '"unterminated'];
    for (const input of hostile) {
      const sanitized = sanitizeQuery(input);
      if (!sanitized) continue;
      expect(() => rankedSearch(store, "all", sanitized)).not.toThrow();
    }
  });
});

describe("ranked search", () => {
  test("returns the replace-Svelte commit as the top hit with a wrapped snippet", async () => {
    const { store } = await indexedFixture();
    const sanitized = sanitizeQuery("svelte");
    expect(sanitized).not.toBeNull();
    const { candidates, pass } = rankedSearch(store, "all", sanitized!);
    expect(pass).toBe("and");
    expect(candidates.length).toBeGreaterThan(0);
    const top = candidates[0]!.hit;
    expect(top.subject).toBe("feat: single-pass HTML generation engine, replace Svelte");
    expect(top.matchKind).toBe("message");
    expect(top.snippet).toContain("«Svelte»");
  });

  test("path scope matches the svelte file through unicode61 separators", async () => {
    const { store } = await indexedFixture();
    const { candidates } = rankedSearch(store, "path", sanitizeQuery("svelte")!);
    expect(candidates.some(({ hit }) => hit.path === "src/App.svelte")).toBeTrue();
  });

  test("within one scope, repeated mentions outrank a newer incidental mention", async () => {
    // Discriminates normalized-relevance scoring from pure recency-ordinal
    // scoring: the recency-ordinal scheme would rank the newer incidental
    // commit first, and min-max-normalized raw bm25 would let document-length
    // normalization decide instead of mention count.
    const repository = await createRepository();
    cleanups.push(repository.cleanup);
    const stateRoot = await realpath(await mkdtemp(join(tmpdir(), "usable-git-search-query-")));
    cleanups.push(() => rm(stateRoot, { recursive: true, force: true }));
    const store = openSearchStore(join(repository.path, ".git"), { stateRoot });
    cleanups.push(async () => store.close());
    await commitFile(
      repository,
      "parser.ts",
      "export const parse = 1\n",
      "feat: pelican parser rewrite\n\nReplaces the pelican tokenizer and the pelican emitter.",
    );
    await commitFile(
      repository,
      "deps.txt",
      "deps\n",
      "chore: bump dependencies\n\nAlso touches pelican indirectly.",
    );
    await ingestSearchIndex(store, repository.path);

    const { candidates } = rankedSearch(store, "message", sanitizeQuery("pelican")!);
    expect(candidates).toHaveLength(2);
    expect(candidates[0]!.hit.subject).toBe("feat: pelican parser rewrite");
    expect(candidates[1]!.hit.subject).toBe("chore: bump dependencies");
  });

  test("falls back to OR only when the strict AND pass is empty", async () => {
    const { store } = await indexedFixture();
    const both = rankedSearch(store, "all", sanitizeQuery("svelte engine")!);
    expect(both.pass).toBe("and");
    const fallback = rankedSearch(store, "all", sanitizeQuery("svelte zzznonexistent")!);
    expect(fallback.pass).toBe("or");
    expect(fallback.candidates.length).toBeGreaterThan(0);
  });
});

describe("lifecycle", () => {
  test("path lifecycle reports first/last/removed and absence at HEAD", async () => {
    const { repository, store } = await indexedFixture();
    const runner = createIngestRunner();
    const lifecycle = await pathLifecycle(store, repository.path, runner, "src/App.svelte");
    expect(lifecycle.presentAtHead).toBeFalse();
    expect(lifecycle.firstSeen?.subject).toBe("feat: svelte app shell");
    expect(lifecycle.removedIn?.subject).toBe(
      "feat: single-pass HTML generation engine, replace Svelte",
    );
    expect(lifecycle.totalTouches).toBe(2);
  });

  test("token lifecycle answers was-it-dropped for diff text", async () => {
    const { repository, store } = await indexedFixture();
    const runner = createIngestRunner();
    const lifecycle = await tokenLifecycle(store, repository.path, runner, "svelte");
    expect(lifecycle).not.toBeNull();
    expect(lifecycle!.presentAtHead).toBeFalse();
    expect(lifecycle!.firstSeen?.subject).toBe("feat: svelte app shell");
    expect(lifecycle!.removedIn?.subject).toBe(
      "feat: single-pass HTML generation engine, replace Svelte",
    );
  });

  test("token lifecycle sees a token still present at HEAD", async () => {
    const { repository, store } = await indexedFixture();
    const runner = createIngestRunner();
    const lifecycle = await tokenLifecycle(store, repository.path, runner, "render");
    expect(lifecycle).not.toBeNull();
    expect(lifecycle!.presentAtHead).toBeTrue();
    expect(lifecycle!.removedIn).toBeUndefined();
  });
});

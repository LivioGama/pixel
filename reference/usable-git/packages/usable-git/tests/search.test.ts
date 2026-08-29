import { afterEach, describe, expect, test } from "bun:test";
import { mkdtemp, realpath, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { encodeCursor } from "../src/contracts/cursor.ts";
import { diff } from "../src/operations/diff.ts";
import { search } from "../src/operations/search.ts";
import {
  commitFile,
  createRepository,
  type TestRepository,
} from "./helpers/repository.ts";

const cleanups: Array<() => Promise<unknown>> = [];
afterEach(async () => {
  for (const cleanup of cleanups.splice(0)) await cleanup();
});

const fixture = async (): Promise<{ repository: TestRepository; stateRoot: string }> => {
  const repository = await createRepository();
  cleanups.push(repository.cleanup);
  const stateRoot = await realpath(await mkdtemp(join(tmpdir(), "usable-git-search-op-")));
  cleanups.push(() => rm(stateRoot, { recursive: true, force: true }));
  return { repository, stateRoot };
};

const svelteFixture = async () => {
  const { repository, stateRoot } = await fixture();
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
  return { repository, stateRoot };
};

describe("search operation", () => {
  test("regression: one text query answers 'was svelte dropped' and feeds diff", async () => {
    const { repository, stateRoot } = await svelteFixture();

    const result = await search({
      repoPath: repository.path,
      target: { kind: "text", query: "svelte" },
    }, { stateRoot });

    expect(result.index.state).toBe("fresh");
    expect(result.hits.length).toBeGreaterThan(0);
    const top = result.hits[0]!;
    expect(top.subject).toBe("feat: single-pass HTML generation engine, replace Svelte");
    expect(top.snippet).toContain("«Svelte»");
    const envelopeBytes = Buffer.byteLength(JSON.stringify({ ok: true, result }));
    expect(envelopeBytes).toBeLessThan(4_096);

    // The 12-hex hit oid is accepted verbatim by the existing diff operation.
    const patch = await diff({
      repoPath: repository.path,
      target: { kind: "commit", oid: top.oid },
    }, { stateRoot });
    expect(patch.items.some(({ path }) => path === "src/App.svelte")).toBeTrue();
  });

  test("regression: one lifecycle call reports removedIn and absence at HEAD", async () => {
    const { repository, stateRoot } = await svelteFixture();

    const result = await search({
      repoPath: repository.path,
      target: { kind: "lifecycle", token: "svelte" },
    }, { stateRoot });

    expect(result.lifecycle).toBeDefined();
    expect(result.lifecycle!.presentAtHead).toBeFalse();
    expect(result.lifecycle!.removedIn?.subject).toBe(
      "feat: single-pass HTML generation engine, replace Svelte",
    );
    expect(result.lifecycle!.firstSeen?.subject).toBe("feat: svelte app shell");
    expect(result.hits.length).toBeGreaterThan(0);
    expect(result.hits.length).toBeLessThanOrEqual(5);
  });

  test("lifecycle by path is rename-agnostic on the exact path", async () => {
    const { repository, stateRoot } = await svelteFixture();
    const result = await search({
      repoPath: repository.path,
      target: { kind: "lifecycle", path: "src/App.svelte" },
    }, { stateRoot });
    expect(result.lifecycle!.presentAtHead).toBeFalse();
    expect(result.lifecycle!.totalTouches).toBe(2);
  });

  test("rejects a lifecycle target carrying both path and token", async () => {
    const { repository, stateRoot } = await fixture();
    await expect(search({
      repoPath: repository.path,
      target: { kind: "lifecycle", path: "a.txt", token: "a" },
    }, { stateRoot })).rejects.toThrow();
  });

  test("rejects a query with no searchable characters", async () => {
    const { repository, stateRoot } = await svelteFixture();
    await expect(search({
      repoPath: repository.path,
      target: { kind: "text", query: "! . (" },
    }, { stateRoot })).rejects.toMatchObject({ code: "INVALID_INPUT" });
  });

  test("an unborn repository returns an empty fresh result without error", async () => {
    const { repository, stateRoot } = await fixture();
    const result = await search({
      repoPath: repository.path,
      target: { kind: "text", query: "anything" },
    }, { stateRoot });
    expect(result.head).toEqual({ kind: "unborn" });
    expect(result.hits).toEqual([]);
    expect(result.index).toMatchObject({ state: "fresh", indexedCommits: 0 });
  });

  test("a zero build budget reports partial and repeated calls converge to fresh", async () => {
    const { repository, stateRoot } = await svelteFixture();
    const partial = await search({
      repoPath: repository.path,
      target: { kind: "text", query: "svelte" },
    }, { stateRoot, buildBudgetMs: 0 });
    expect(partial.index.state).toBe("partial");
    expect(partial.index.pendingCommits).toBeGreaterThan(0);
    expect(partial.hits).toEqual([]);

    const converged = await search({
      repoPath: repository.path,
      target: { kind: "text", query: "svelte" },
    }, { stateRoot });
    expect(converged.index.state).toBe("fresh");
    expect(converged.hits.length).toBeGreaterThan(0);
  });

  test("a commit landed after the first call is visible on the next call", async () => {
    const { repository, stateRoot } = await fixture();
    await commitFile(repository, "a.txt", "one\n", "first commit");
    const before = await search({
      repoPath: repository.path,
      target: { kind: "text", query: "wombat" },
    }, { stateRoot });
    expect(before.hits).toEqual([]);

    await commitFile(repository, "b.txt", "two\n", "feat: introduce the wombat module");
    const after = await search({
      repoPath: repository.path,
      target: { kind: "text", query: "wombat" },
    }, { stateRoot });
    expect(after.hits.length).toBeGreaterThan(0);
    expect(after.hits[0]?.subject).toBe("feat: introduce the wombat module");
  });

  test("paginates with a cursor and honors the limit bound", async () => {
    const { repository, stateRoot } = await fixture();
    for (let index = 0; index < 6; index += 1) {
      await commitFile(repository, `f${index}.txt`, `${index}\n`, `feat: pelican step ${index}`);
    }
    const first = await search({
      repoPath: repository.path,
      target: { kind: "text", query: "pelican", scope: "message" },
      limit: 4,
    }, { stateRoot });
    expect(first.hits).toHaveLength(4);
    expect(first.nextCursor).toBeDefined();

    const second = await search({
      repoPath: repository.path,
      target: { kind: "text", query: "pelican", scope: "message" },
      limit: 4,
      cursor: first.nextCursor!,
    }, { stateRoot });
    expect(second.hits).toHaveLength(2);
    expect(second.nextCursor).toBeUndefined();
    const oids = new Set([...first.hits, ...second.hits].map(({ oid }) => oid));
    expect(oids.size).toBe(6);
  });

  test("a cursor goes STALE_STATE when new history lands between pages", async () => {
    const { repository, stateRoot } = await fixture();
    for (let index = 0; index < 4; index += 1) {
      await commitFile(repository, `f${index}.txt`, `${index}\n`, `feat: heron step ${index}`);
    }
    const first = await search({
      repoPath: repository.path,
      target: { kind: "text", query: "heron", scope: "message" },
      limit: 2,
    }, { stateRoot });
    expect(first.nextCursor).toBeDefined();

    await commitFile(repository, "late.txt", "late\n", "feat: heron lands late");
    await expect(search({
      repoPath: repository.path,
      target: { kind: "text", query: "heron", scope: "message" },
      limit: 2,
      cursor: first.nextCursor!,
    }, { stateRoot })).rejects.toMatchObject({ code: "STALE_STATE" });
  });

  test("rejects a cursor minted by a different operation", async () => {
    const { repository, stateRoot } = await svelteFixture();
    const foreign = await encodeCursor({
      operation: "history",
      requestDigest: "a".repeat(64),
      snapshot: "b".repeat(64),
      offset: 0,
    }, { stateRoot });
    await expect(search({
      repoPath: repository.path,
      target: { kind: "text", query: "svelte" },
      cursor: foreign,
    }, { stateRoot })).rejects.toMatchObject({ code: "INVALID_INPUT" });
  });

  test("honors the byte cap as a second bound independent of limit", async () => {
    const { repository, stateRoot } = await fixture();
    for (let index = 0; index < 5; index += 1) {
      await commitFile(
        repository,
        `f${index}.txt`,
        `${index}\n`,
        `feat: albatross step ${index} ${"padding ".repeat(30)}`,
      );
    }
    const capped = await search({
      repoPath: repository.path,
      target: { kind: "text", query: "albatross", scope: "message" },
      limit: 5,
      byteCap: 1_024,
    }, { stateRoot });
    expect(capped.hits.length).toBeGreaterThan(0);
    expect(capped.hits.length).toBeLessThan(5);
    expect(capped.nextCursor).toBeDefined();
  });
});

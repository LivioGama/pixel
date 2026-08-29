import { afterEach, describe, expect, test } from "bun:test";
import { mkdtemp, realpath, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { openSearchStore, searchStorePath } from "../src/search/store.ts";

const stateRoots: string[] = [];
afterEach(async () =>
  Promise.all(stateRoots.splice(0).map((root) => rm(root, { recursive: true, force: true }))),
);

const temporaryStateRoot = async () => {
  const created = await realpath(await mkdtemp(join(tmpdir(), "usable-git-search-state-")));
  stateRoots.push(created);
  return created;
};

describe("search store", () => {
  test("derives one index path per repository common dir under the state root", async () => {
    const stateRoot = await temporaryStateRoot();
    const first = searchStorePath("/repo/.git", { stateRoot });
    const again = searchStorePath("/repo/.git", { stateRoot });
    const other = searchStorePath("/other/.git", { stateRoot });
    expect(first).toBe(again);
    expect(first).not.toBe(other);
    expect(first.startsWith(join(stateRoot, "search"))).toBeTrue();
    expect(first.endsWith("index-v1.sqlite")).toBeTrue();
    expect(first).not.toContain("/repo");
  });

  test("creates the full schema with WAL and user_version 1", async () => {
    const stateRoot = await temporaryStateRoot();
    const store = openSearchStore("/repo/.git", { stateRoot });
    try {
      const tables = store.db
        .query("SELECT name FROM sqlite_master WHERE type IN ('table') ORDER BY name")
        .all() as Array<{ name: string }>;
      const names = tables.map(({ name }) => name);
      for (const required of [
        "meta",
        "indexed_refs",
        "commits",
        "file_changes",
        "hunk_text",
        "messages_fts",
        "paths_fts",
        "diffs_fts",
      ]) {
        expect(names).toContain(required);
      }
      const version = store.db.query("PRAGMA user_version").get() as { user_version: number };
      expect(version.user_version).toBe(1);
      const journal = store.db.query("PRAGMA journal_mode").get() as { journal_mode: string };
      expect(journal.journal_mode).toBe("wal");
    } finally {
      store.close();
    }
  });

  test("supports external-content FTS snippets over inserted rows", async () => {
    const stateRoot = await temporaryStateRoot();
    const store = openSearchStore("/repo/.git", { stateRoot });
    try {
      store.db
        .query(
          `INSERT INTO commits (oid, parents, author_name, author_email, authored_at,
             committer_name, committer_email, committed_at, message)
           VALUES (?1, '', 'a', 'a@e', '2026-01-01T00:00:00Z',
             'a', 'a@e', '2026-01-01T00:00:00Z', ?2)`,
        )
        .run("f".repeat(40), "feat: single-pass HTML generation engine, replace Svelte");
      const id = (store.db.query("SELECT id FROM commits WHERE oid = ?1").get("f".repeat(40)) as {
        id: number;
      }).id;
      store.db
        .query("INSERT INTO messages_fts (rowid, message) SELECT id, message FROM commits WHERE id = ?1")
        .run(id);
      const row = store.db
        .query(
          `SELECT snippet(messages_fts, 0, '«', '»', '…', 10) AS snip
           FROM messages_fts WHERE messages_fts MATCH '"svelte"'`,
        )
        .get() as { snip: string };
      expect(row.snip).toContain("«Svelte»");
    } finally {
      store.close();
    }
  });

  test("self-heals a garbage database file by rebuilding silently", async () => {
    const stateRoot = await temporaryStateRoot();
    const path = searchStorePath("/repo/.git", { stateRoot });
    await rm(dirname(path), { recursive: true, force: true });
    await (await import("node:fs/promises")).mkdir(dirname(path), { recursive: true });
    await writeFile(path, "this is not a sqlite database at all\n");
    const store = openSearchStore("/repo/.git", { stateRoot });
    try {
      const count = store.db.query("SELECT count(*) AS n FROM commits").get() as { n: number };
      expect(count.n).toBe(0);
    } finally {
      store.close();
    }
  });

  test("self-heals an unknown schema version by dropping and rebuilding", async () => {
    const stateRoot = await temporaryStateRoot();
    const first = openSearchStore("/repo/.git", { stateRoot });
    first.db.exec("PRAGMA user_version = 99");
    first.db
      .query(
        `INSERT INTO commits (oid, parents, author_name, author_email, authored_at,
           committer_name, committer_email, committed_at, message)
         VALUES (?1, '', 'a', 'a@e', 't', 'a', 'a@e', 't', 'stale row')`,
      )
      .run("a".repeat(40));
    first.close();
    const second = openSearchStore("/repo/.git", { stateRoot });
    try {
      const count = second.db.query("SELECT count(*) AS n FROM commits").get() as { n: number };
      expect(count.n).toBe(0);
      const version = second.db.query("PRAGMA user_version").get() as { user_version: number };
      expect(version.user_version).toBe(1);
    } finally {
      second.close();
    }
  });
});

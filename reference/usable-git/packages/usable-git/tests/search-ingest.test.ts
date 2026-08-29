import { afterEach, describe, expect, test } from "bun:test";
import { mkdtemp, realpath, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  ingestSearchIndex,
  parsePhaseA,
  parsePhaseB,
} from "../src/search/ingest.ts";
import { openSearchStore, type SearchStore } from "../src/search/store.ts";
import {
  commitFile,
  createRepository,
  writeFile,
  type TestRepository,
} from "./helpers/repository.ts";

const cleanups: Array<() => Promise<unknown>> = [];
afterEach(async () => {
  for (const cleanup of cleanups.splice(0)) await cleanup();
});

const temporaryStateRoot = async () => {
  const created = await realpath(await mkdtemp(join(tmpdir(), "usable-git-search-ingest-")));
  cleanups.push(() => rm(created, { recursive: true, force: true }));
  return created;
};

const fixture = async (): Promise<{ repository: TestRepository; store: SearchStore }> => {
  const repository = await createRepository();
  cleanups.push(repository.cleanup);
  const stateRoot = await temporaryStateRoot();
  const commonDir = join(repository.path, ".git");
  const store = openSearchStore(commonDir, { stateRoot });
  cleanups.push(async () => store.close());
  return { repository, store };
};

describe("search ingest parsers", () => {
  test("parses NUL-delimited metadata records with statuses and renames", () => {
    const output = [
      "\x1e",
      ["a".repeat(40), "", "Author", "a@e", "2026-01-01T00:00:00Z", "Committer", "c@e",
        "2026-01-02T00:00:00Z", "first commit\n"].join("\0"),
      "\0\nA\0a.txt\0",
      "\x1e",
      ["b".repeat(40), "a".repeat(40), "Author", "a@e", "2026-01-03T00:00:00Z", "Committer",
        "c@e", "2026-01-04T00:00:00Z", "second\n\nbody\n"].join("\0"),
      "\0\nM\0a.txt\0R100\0b.txt\0c.txt\0",
    ].join("");
    const commits = parsePhaseA(output);
    expect(commits).toHaveLength(2);
    expect(commits[0]).toMatchObject({
      oid: "a".repeat(40),
      parents: [],
      changes: [{ status: "A", path: "a.txt" }],
    });
    expect(commits[1]).toMatchObject({
      oid: "b".repeat(40),
      parents: ["a".repeat(40)],
      message: "second\n\nbody\n",
      changes: [
        { status: "M", path: "a.txt" },
        { status: "R", path: "c.txt", oldPath: "b.txt" },
      ],
    });
  });

  test("parses per-commit diff text and detects binary files", () => {
    const output = [
      "\x1e", "a".repeat(40), "\n\n",
      "diff --git a/a.txt b/a.txt\n",
      "index 111..222 100644\n",
      "--- a/a.txt\n",
      "+++ b/a.txt\n",
      "@@ -1 +1 @@\n",
      "-old line\n",
      "+new line\n",
      "diff --git a/img.png b/img.png\n",
      "Binary files a/img.png and b/img.png differ\n",
    ].join("");
    const commits = parsePhaseB(output);
    expect(commits).toHaveLength(1);
    expect(commits[0]?.files).toEqual([
      { path: "a.txt", added: "new line\n", removed: "old line\n", truncated: false, binary: false },
      { path: "img.png", added: "", removed: "", truncated: false, binary: true },
    ]);
  });
});

describe("search ingest against a real repository", () => {
  test("indexes messages, paths, diffs, and records merge and lockfile skips", async () => {
    const { repository, store } = await fixture();
    await commitFile(repository, "app.ts", "export const app = 1\n", "feat: initial app");
    await commitFile(repository, "bun.lock", "{}\n", "chore: lockfile");
    await repository.run("checkout", "-q", "-b", "feature");
    await commitFile(repository, "feature.ts", "export const feature = 2\n", "feat: feature work");
    await repository.run("checkout", "-q", "main");
    await commitFile(repository, "main.ts", "export const main = 3\n", "feat: main work");
    await repository.run("merge", "--no-ff", "-q", "-m", "merge feature", "feature");

    const outcome = await ingestSearchIndex(store, repository.path);
    expect(outcome.counters.indexedCommits).toBe(5);
    expect(outcome.counters.pendingCommits).toBe(0);
    expect(outcome.counters.pendingDiffCommits).toBe(0);

    const merge = store.db
      .query("SELECT diff_state, skip_note FROM commits WHERE message LIKE 'merge feature%'")
      .get() as { diff_state: number; skip_note: string };
    expect(merge).toEqual({ diff_state: 2, skip_note: "merge" });

    const lockfileHunks = store.db
      .query("SELECT count(*) AS n FROM hunk_text WHERE path = 'bun.lock'")
      .get() as { n: number };
    expect(lockfileHunks.n).toBe(0);
    const lockfileChange = store.db
      .query("SELECT count(*) AS n FROM file_changes WHERE path = 'bun.lock'")
      .get() as { n: number };
    expect(lockfileChange.n).toBe(1);

    const diffMatch = store.db
      .query("SELECT count(*) AS n FROM diffs_fts WHERE diffs_fts MATCH '\"feature\"'")
      .get() as { n: number };
    expect(diffMatch.n).toBeGreaterThan(0);
  });

  test("is idempotent and picks up new commits incrementally", async () => {
    const { repository, store } = await fixture();
    await commitFile(repository, "a.txt", "one\n", "first");
    await ingestSearchIndex(store, repository.path);
    const again = await ingestSearchIndex(store, repository.path);
    expect(again.counters.indexedCommits).toBe(1);

    await commitFile(repository, "b.txt", "two\n", "second");
    const updated = await ingestSearchIndex(store, repository.path);
    expect(updated.counters.indexedCommits).toBe(2);
    expect(updated.counters.pendingDiffCommits).toBe(0);
  });

  test("a zero budget leaves the index partial and later calls converge", async () => {
    const { repository, store } = await fixture();
    await commitFile(repository, "a.txt", "one\n", "first");
    await commitFile(repository, "b.txt", "two\n", "second");
    const partial = await ingestSearchIndex(store, repository.path, { budgetMs: 0 });
    expect(partial.counters.indexedCommits).toBe(0);
    expect(partial.counters.pendingCommits).toBe(2);

    const converged = await ingestSearchIndex(store, repository.path);
    expect(converged.counters.indexedCommits).toBe(2);
    expect(converged.counters.pendingCommits).toBe(0);
    expect(converged.counters.pendingDiffCommits).toBe(0);
  });

  test("survives a rebase-like history rewrite and keeps old commits as evidence", async () => {
    const { repository, store } = await fixture();
    await commitFile(repository, "a.txt", "one\n", "base");
    await commitFile(repository, "b.txt", "two\n", "dropped by reset");
    await ingestSearchIndex(store, repository.path);

    await repository.run("reset", "--hard", "-q", "HEAD~1");
    await commitFile(repository, "c.txt", "three\n", "rewritten tip");
    const outcome = await ingestSearchIndex(store, repository.path);
    expect(outcome.counters.indexedCommits).toBe(3);
    const dropped = store.db
      .query("SELECT count(*) AS n FROM commits WHERE message LIKE 'dropped%'")
      .get() as { n: number };
    expect(dropped.n).toBe(1);
  });

  test("an unborn repository ingests to an empty fresh index without error", async () => {
    const { repository, store } = await fixture();
    const outcome = await ingestSearchIndex(store, repository.path);
    expect(outcome.tips).toEqual([]);
    expect(outcome.counters).toEqual({
      indexedCommits: 0,
      pendingCommits: 0,
      pendingDiffCommits: 0,
      skippedDiffCommits: 0,
    });
  });

  test("a commit whose diff cannot be produced is skipped as unresolvable, never wedging", async () => {
    const { repository, store } = await fixture();
    await commitFile(repository, "a.txt", "one\n", "first");
    await ingestSearchIndex(store, repository.path);

    // Forensic row for an object git no longer has (the post-rebase gc case):
    // Phase B's `git show` fails on it, batch halving isolates it, and the
    // single-oid failure records the skip instead of erroring or looping.
    store.db
      .query(
        `INSERT INTO commits (oid, parents, author_name, author_email, authored_at,
           committer_name, committer_email, committed_at, message, diff_state)
         VALUES (?1, '', 'Ghost', 'g@e', '2026-01-01T00:00:00Z',
           'Ghost', 'g@e', '2026-01-01T00:00:00Z', 'gc-pruned forensic commit', 0)`,
      )
      .run("f".repeat(40));
    await commitFile(repository, "b.txt", "two\n", "second");

    const outcome = await ingestSearchIndex(store, repository.path);
    expect(outcome.counters.pendingDiffCommits).toBe(0);
    expect(outcome.counters.skippedDiffCommits).toBe(1);
    const ghost = store.db
      .query("SELECT diff_state, skip_note FROM commits WHERE oid = ?1")
      .get("f".repeat(40)) as { diff_state: number; skip_note: string };
    expect(ghost).toEqual({ diff_state: 2, skip_note: "unresolvable" });
    // The real commit sharing the failed batch was isolated and indexed.
    const real = store.db
      .query("SELECT diff_state FROM commits WHERE message LIKE 'second%'")
      .get() as { diff_state: number };
    expect(real.diff_state).toBe(1);
  });

  test("caps oversized per-file diff text and marks it truncated", async () => {
    const { repository, store } = await fixture();
    const huge = `${"const filler = 'x'\n".repeat(3_000)}`;
    await commitFile(repository, "big.ts", huge, "feat: huge file");
    await ingestSearchIndex(store, repository.path);
    const hunk = store.db
      .query("SELECT truncated, length(added) AS bytes FROM hunk_text WHERE path = 'big.ts'")
      .get() as { truncated: number; bytes: number };
    expect(hunk.truncated).toBe(1);
    expect(hunk.bytes).toBeLessThanOrEqual(33 * 1_024);
  });
});

import { afterEach, describe, expect, test } from "bun:test";
import { inspect, inspectDetailed } from "../src/operations/inspect.ts";
import {
  commitFile,
  createRepository,
  type TestRepository,
  writeFile,
} from "./helpers/repository.ts";

const repositories: TestRepository[] = [];
afterEach(async () => Promise.all(repositories.splice(0).map(({ cleanup }) => cleanup())));

const repository = async () => {
  const created = await createRepository();
  repositories.push(created);
  return created;
};

describe("inspect", () => {
  test("reports compact porcelain-style changes with a snapshot token", async () => {
    const repo = await repository();
    await commitFile(repo, "tracked.txt", "base\n", "initial");
    await writeFile(repo, "tracked.txt", "staged\n");
    await repo.run("add", "--", "tracked.txt");
    await writeFile(repo, "tracked.txt", "unstaged\n");
    await writeFile(repo, "new\nfile.txt", "untracked\n");

    const result = await inspect({ repoPath: repo.path });
    expect(result.branch).toBe("main");
    expect(result.head).toMatch(/^[a-f0-9]{40,64}$/);
    expect(result.snapshot).toMatch(/^[a-f0-9]{12}$/);
    expect(result.changes).toEqual([
      { path: "tracked.txt", status: "MM" },
      { path: "new\nfile.txt", status: "??" },
    ]);
    expect(result.state).toBeUndefined();
    expect(result.stashes).toBeUndefined();
  });

  test("keeps per-file fingerprints on the detailed internal shape only", async () => {
    const repo = await repository();
    await writeFile(repo, "file.txt", "one");
    const before = await inspectDetailed({ repoPath: repo.path });
    await writeFile(repo, "file.txt", "two");
    const after = await inspectDetailed({ repoPath: repo.path });
    expect(before.changes.every(({ fingerprint }) => /^[a-f0-9]{64}$/.test(fingerprint))).toBe(
      true,
    );
    expect(after.changes[0]?.fingerprint).not.toBe(before.changes[0]?.fingerprint);
  });

  test("reports unborn HEAD, stash count, and configured remotes explicitly", async () => {
    const repo = await repository();
    const unborn = await inspect({ repoPath: repo.path });
    expect(unborn.head).toBeNull();
    expect(unborn.stashes).toBeUndefined();
    expect(unborn.remotes).toBeUndefined();

    await commitFile(repo, "tracked.txt", "base\n", "initial");
    await writeFile(repo, "tracked.txt", "stashed\n");
    await repo.run("stash", "push", "--quiet");
    await repo.run("remote", "add", "origin", "https://user:secret@example.test/repo.git");
    const withStash = await inspect({ repoPath: repo.path });
    expect(withStash.head).toMatch(/^[a-f0-9]{40,64}$/);
    expect(withStash.stashes).toBe(1);
    expect(withStash.remotes).toEqual([
      {
        name: "origin",
        fetchUrl: "https://[REDACTED]@example.test/repo.git",
        pushUrl: "https://[REDACTED]@example.test/repo.git",
      },
    ]);
  });
});

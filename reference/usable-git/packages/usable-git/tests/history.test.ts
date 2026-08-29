import { afterEach, describe, expect, test } from "bun:test";
import { history } from "../src/operations/history.ts";
import {
  commitFile,
  createRepository,
  type TestRepository,
} from "./helpers/repository.ts";

const repositories: TestRepository[] = [];
afterEach(async () => Promise.all(repositories.splice(0).map(({ cleanup }) => cleanup())));

const repository = async () => {
  const created = await createRepository();
  repositories.push(created);
  return created;
};

describe("history", () => {
  test("returns compact newest-first commits and supports short cursors", async () => {
    const repo = await repository();
    await commitFile(repo, "one.txt", "one", "first\n\nbody");
    await commitFile(repo, "two.txt", "two", "second");

    const first = await history({ repoPath: repo.path, limit: 1 });
    expect(first.commits).toHaveLength(1);
    expect(first.commits[0]).toMatchObject({ subject: "second" });
    expect((first.commits[0] as { oid: string }).oid).toMatch(/^[a-f0-9]{12}$/);
    expect(first.head.kind).toBe("oid");
    expect(first.nextCursor).toMatch(/^c_[a-f0-9]{10}$/);

    const second = await history({ repoPath: repo.path, limit: 1, cursor: first.nextCursor });
    expect(second.commits[0]).toMatchObject({ subject: "first" });
  });

  test("restores the forensic shape with detail: full", async () => {
    const repo = await repository();
    await commitFile(repo, "one.txt", "one", "first\n\nbody");
    const result = await history({ repoPath: repo.path, detail: "full" });
    const commit = result.commits[0] as {
      oid: string;
      message: string;
      committer: { email: string };
      parents: string[];
    };
    expect(commit.oid).toMatch(/^[a-f0-9]{40,64}$/);
    expect(commit.message).toContain("body");
    expect(commit.committer.email).toBe("usable-git@example.test");
    expect(commit.parents).toEqual([]);
  });

  test("does not fetch while reading history", async () => {
    const repo = await repository();
    await commitFile(repo, "one.txt", "one", "first");
    const result = await history({ repoPath: repo.path, ref: "HEAD", limit: 20 });
    expect(result.commits).toHaveLength(1);
  });

  test("returns explicit unborn state", async () => {
    const repo = await repository();
    const result = await history({ repoPath: repo.path });
    expect(result).toMatchObject({ commits: [], head: { kind: "unborn" } });
  });

  test("rejects a cursor after the bound ref advances", async () => {
    const repo = await repository();
    await commitFile(repo, "one.txt", "one", "first");
    await commitFile(repo, "two.txt", "two", "second");
    const first = await history({ repoPath: repo.path, limit: 1 });
    await commitFile(repo, "three.txt", "three", "third");

    const error = await history({
      repoPath: repo.path,
      limit: 1,
      cursor: first.nextCursor,
    }).catch((caught) => caught);
    expect(error).toMatchObject({ code: "STALE_STATE" });
  });
});

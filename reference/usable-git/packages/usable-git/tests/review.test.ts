import { afterEach, describe, expect, test } from "bun:test";
import { review } from "../src/operations/review.ts";
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

describe("review", () => {
  test("separates staged and unstaged evidence and includes only selected untracked files", async () => {
    const repo = await repository();
    await commitFile(repo, "tracked.txt", "base\n", "initial");
    await writeFile(repo, "tracked.txt", "staged\n");
    await repo.run("add", "--", "tracked.txt");
    await writeFile(repo, "tracked.txt", "unstaged\n");
    await writeFile(repo, "selected.txt", "selected\n");
    await writeFile(repo, "secret.txt", "secret\n");

    const result = await review({
      repoPath: repo.path,
      files: ["tracked.txt", "selected.txt"],
      byteCap: 64_000,
    });
    expect(result.items.some(({ scope }) => scope === "staged")).toBe(true);
    expect(result.items.some(({ scope }) => scope === "unstaged")).toBe(true);
    expect(result.items.some(({ path }) => path === "selected.txt")).toBe(true);
    expect(result.items.some(({ path }) => path === "secret.txt")).toBe(false);
  });

  test("paginates deterministically within a byte cap", async () => {
    const repo = await repository();
    await commitFile(repo, "large.txt", "base\n", "initial");
    await writeFile(repo, "large.txt", `${"changed\n".repeat(100)}`);

    const first = await review({ repoPath: repo.path, byteCap: 180 });
    expect(first.nextCursor).toBeDefined();
    expect(first.bytes).toBeLessThanOrEqual(180);
    const second = await review({ repoPath: repo.path, byteCap: 180, cursor: first.nextCursor });
    expect(second.items).not.toEqual(first.items);
  });

  test("rejects cursors after the bound repository snapshot changes", async () => {
    const repo = await repository();
    await commitFile(repo, "large.txt", "base\n", "initial");
    await writeFile(repo, "large.txt", "changed\n".repeat(100));
    const first = await review({ repoPath: repo.path, byteCap: 180 });
    expect(first.nextCursor).toBeDefined();

    await writeFile(repo, "large.txt", "different\n".repeat(100));
    const error = await review({
      repoPath: repo.path,
      byteCap: 180,
      cursor: first.nextCursor,
    }).catch((caught) => caught);
    expect(error).toMatchObject({ code: "STALE_STATE" });
  });

  test("rejects a cursor reused with a different request", async () => {
    const repo = await repository();
    await commitFile(repo, "large.txt", "base\n", "initial");
    await writeFile(repo, "large.txt", "changed\n".repeat(100));
    const first = await review({ repoPath: repo.path, byteCap: 180 });

    const error = await review({
      repoPath: repo.path,
      files: ["large.txt"],
      byteCap: 180,
      cursor: first.nextCursor,
    }).catch((caught) => caught);
    expect(error).toMatchObject({ code: "INVALID_INPUT" });
  });
});

import { afterEach, describe, expect, test } from "bun:test";
import { mkdtemp, readdir, rm } from "node:fs/promises";
import { realpath } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createSnapshotStore, snapshotToken } from "../src/mutations/snapshot-store.ts";
import { inspect } from "../src/operations/inspect.ts";
import { publish, PublishOperationError } from "../src/operations/publish.ts";
import {
  commitFile,
  createRepository,
  type TestRepository,
  writeFile,
} from "./helpers/repository.ts";

const repositories: TestRepository[] = [];
const stateRoots: string[] = [];
afterEach(async () => {
  await Promise.all(repositories.splice(0).map(({ cleanup }) => cleanup()));
  await Promise.all(stateRoots.splice(0).map((root) => rm(root, { recursive: true, force: true })));
});

const repository = async () => {
  const created = await createRepository();
  repositories.push(created);
  return created;
};

const temporaryStateRoot = async () => {
  const created = await realpath(await mkdtemp(join(tmpdir(), "usable-git-snapshot-state-")));
  stateRoots.push(created);
  return created;
};

describe("snapshot store", () => {
  test("derives a deterministic content-based token", () => {
    const input = {
      root: "/repo",
      head: "a".repeat(40),
      fingerprints: { "b.txt": "b".repeat(64), "a.txt": "a".repeat(64) },
    };
    const reordered = {
      root: "/repo",
      head: "a".repeat(40),
      fingerprints: { "a.txt": "a".repeat(64), "b.txt": "b".repeat(64) },
    };
    expect(snapshotToken(input)).toBe(snapshotToken(reordered));
    expect(snapshotToken(input)).toMatch(/^[a-f0-9]{12}$/);
    expect(snapshotToken({ ...input, head: null })).not.toBe(snapshotToken(input));
  });

  test("records and reads back a snapshot keyed by worktree root", async () => {
    const stateRoot = await temporaryStateRoot();
    const store = createSnapshotStore({ stateRoot });
    const token = await store.record({
      root: "/repo",
      head: null,
      branch: "main",
      fingerprints: { "a.txt": "a".repeat(64) },
    });
    expect(await store.read("/repo", token)).toMatchObject({
      root: "/repo",
      head: null,
      fingerprints: { "a.txt": "a".repeat(64) },
    });
    expect(await store.read("/other-repo", token)).toBeNull();
    expect(await store.read("/repo", "000000000000")).toBeNull();
    expect(await store.read("/repo", "not-a-token")).toBeNull();
  });

  test("prunes snapshots beyond the retention count", async () => {
    const stateRoot = await temporaryStateRoot();
    const store = createSnapshotStore({ stateRoot, retentionMaxCount: 2 });
    for (let index = 0; index < 4; index += 1) {
      await store.record({
        root: "/repo",
        head: null,
        branch: "main",
        fingerprints: { [`file-${index}.txt`]: "a".repeat(64) },
      });
    }
    const directories = await readdir(join(stateRoot, "snapshots"));
    const files = await readdir(join(stateRoot, "snapshots", directories[0]!));
    expect(files.filter((name) => name.endsWith(".json")).length).toBeLessThanOrEqual(2);
  });

  test("publish succeeds from an inspect snapshot token alone", async () => {
    const repo = await repository();
    const stateRoot = await temporaryStateRoot();
    await commitFile(repo, "base.txt", "base\n", "initial");
    await writeFile(repo, "selected.txt", "selected\n");
    const inspected = await inspect({ repoPath: repo.path }, { stateRoot });
    expect(inspected.snapshot).toMatch(/^[a-f0-9]{12}$/);
    const result = await publish({
      repoPath: repo.path,
      files: ["selected.txt"],
      message: "snapshot-mode publish",
      requestId: `snapshot-publish-${crypto.randomUUID()}`,
      snapshot: inspected.snapshot!,
    }, { stateRoot });
    expect(result.committedPaths).toEqual(["selected.txt"]);
  });

  test("publish rejects an unknown snapshot token with terminal STALE_STATE", async () => {
    const repo = await repository();
    const stateRoot = await temporaryStateRoot();
    await writeFile(repo, "selected.txt", "selected\n");
    const attempt = publish({
      repoPath: repo.path,
      files: ["selected.txt"],
      message: "must fail",
      requestId: `snapshot-unknown-${crypto.randomUUID()}`,
      snapshot: "0123456789ab",
    }, { stateRoot });
    await attempt.then(
      () => {
        throw new Error("publish unexpectedly succeeded");
      },
      (error) => {
        expect(error).toBeInstanceOf(PublishOperationError);
        expect((error as PublishOperationError).code).toBe("STALE_STATE");
        expect((error as PublishOperationError).details).toMatchObject({
          reason: "unknown-snapshot",
        });
      },
    );
  });

  test("publish rejects a snapshot token when the file changed after inspect", async () => {
    const repo = await repository();
    const stateRoot = await temporaryStateRoot();
    await writeFile(repo, "selected.txt", "selected\n");
    const inspected = await inspect({ repoPath: repo.path }, { stateRoot });
    await writeFile(repo, "selected.txt", "changed after inspect\n");
    const attempt = publish({
      repoPath: repo.path,
      files: ["selected.txt"],
      message: "must fail",
      requestId: `snapshot-stale-${crypto.randomUUID()}`,
      snapshot: inspected.snapshot!,
    }, { stateRoot });
    await attempt.then(
      () => {
        throw new Error("publish unexpectedly succeeded");
      },
      (error) => {
        expect(error).toBeInstanceOf(PublishOperationError);
        expect((error as PublishOperationError).code).toBe("STALE_STATE");
      },
    );
  });

  test("publish rejects snapshot files that had no committable change", async () => {
    const repo = await repository();
    const stateRoot = await temporaryStateRoot();
    await commitFile(repo, "clean.txt", "clean\n", "initial");
    await writeFile(repo, "selected.txt", "selected\n");
    const inspected = await inspect({ repoPath: repo.path }, { stateRoot });
    const attempt = publish({
      repoPath: repo.path,
      files: ["clean.txt"],
      message: "must fail",
      requestId: `snapshot-clean-${crypto.randomUUID()}`,
      snapshot: inspected.snapshot!,
    }, { stateRoot });
    await attempt.then(
      () => {
        throw new Error("publish unexpectedly succeeded");
      },
      (error) => {
        expect(error).toBeInstanceOf(PublishOperationError);
        expect((error as PublishOperationError).code).toBe("NOTHING_TO_COMMIT");
      },
    );
  });
});

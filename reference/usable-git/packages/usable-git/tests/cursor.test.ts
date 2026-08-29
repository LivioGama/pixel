import { afterEach, describe, expect, test } from "bun:test";
import { mkdtemp, realpath, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { decodeCursor, encodeCursor } from "../src/contracts/cursor.ts";

const stateRoots: string[] = [];
afterEach(async () =>
  Promise.all(stateRoots.splice(0).map((root) => rm(root, { recursive: true, force: true }))),
);

const temporaryStateRoot = async () => {
  const created = await realpath(await mkdtemp(join(tmpdir(), "usable-git-cursor-state-")));
  stateRoots.push(created);
  return created;
};

describe("short server-held cursor", () => {
  test("round-trips bound pagination state behind a short handle", async () => {
    const stateRoot = await temporaryStateRoot();
    const cursor = await encodeCursor({
      operation: "review",
      requestDigest: "a".repeat(64),
      snapshot: "b".repeat(64),
      offset: { item: 2, character: 17 },
    }, { stateRoot });
    expect(cursor).toMatch(/^c_[a-f0-9]{10}$/);
    expect(await decodeCursor(cursor, "review", { stateRoot })).toEqual({
      version: 1,
      operation: "review",
      requestDigest: "a".repeat(64),
      snapshot: "b".repeat(64),
      offset: { item: 2, character: 17 },
    });
  });

  test("rejects unknown handles, malformed handles, and cross-operation reuse", async () => {
    const stateRoot = await temporaryStateRoot();
    const cursor = await encodeCursor({
      operation: "history",
      requestDigest: "a".repeat(64),
      snapshot: "b".repeat(40),
      offset: 3,
    }, { stateRoot });
    await expect(decodeCursor("c_0000000000", "history", { stateRoot })).rejects.toMatchObject({
      code: "STALE_STATE",
    });
    await expect(decodeCursor("not-a-handle", "history", { stateRoot })).rejects.toMatchObject({
      code: "INVALID_INPUT",
    });
    await expect(decodeCursor(cursor, "review", { stateRoot })).rejects.toMatchObject({
      code: "INVALID_INPUT",
    });
  });
});

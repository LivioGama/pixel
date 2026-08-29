import { describe, expect, test } from "bun:test";
import { createHash } from "node:crypto";
import { mkdir } from "node:fs/promises";
import { join } from "node:path";
import { createPublishRecoveryStore } from "../src/mutations/publish-recovery.ts";
import { withTempDirectory } from "./support/temp.ts";

describe("publish recovery state", () => {
  test("durably round-trips and removes mutation ownership metadata", async () =>
    withTempDirectory("usable-git-recovery-", async (stateRoot) => {
      const store = createPublishRecoveryStore({ stateRoot });
      const state = {
        schemaVersion: 1 as const,
        requestId: "request-1",
        repoKey: "repo",
        inputHash: "input",
        phase: "index_staged" as const,
        preHead: "a".repeat(40),
        files: ["file.txt"],
        index: {
          exists: true as const,
          checksum: "b".repeat(64),
          bytesBase64: Buffer.from([1, 2, 3]).toString("base64"),
          mode: 0o644,
        },
        ownedIndexChecksum: "c".repeat(64),
      };

      await store.write(state);
      expect(await store.read({
        requestId: state.requestId,
        repoKey: state.repoKey,
        inputHash: state.inputHash,
        preHead: state.preHead,
        files: state.files,
      })).toEqual(state);
      await store.remove(state.repoKey, state.requestId);
      expect(await store.read({
        requestId: state.requestId,
        repoKey: state.repoKey,
        inputHash: state.inputHash,
        preHead: state.preHead,
        files: state.files,
      })).toBeNull();
    }));

  test("scopes identical request IDs by repository", async () =>
    withTempDirectory("usable-git-recovery-repos-", async (stateRoot) => {
      const store = createPublishRecoveryStore({ stateRoot });
      const base = {
        schemaVersion: 1 as const,
        requestId: "shared",
        phase: "snapshotted" as const,
        preHead: null,
        files: ["file.txt"],
        index: { exists: false as const, checksum: null },
        ownedIndexChecksum: null,
      };
      await store.write({ ...base, repoKey: "repo-a", inputHash: "input-a" });
      await store.write({ ...base, repoKey: "repo-b", inputHash: "input-b" });

      expect(await store.read({
        requestId: "shared",
        repoKey: "repo-a",
        inputHash: "input-a",
        preHead: null,
        files: ["file.txt"],
      })).toMatchObject({ repoKey: "repo-a" });
      expect(await store.read({
        requestId: "shared",
        repoKey: "repo-b",
        inputHash: "input-b",
        preHead: null,
        files: ["file.txt"],
      })).toMatchObject({ repoKey: "repo-b" });
    }));

  test("rejects malformed or mismatched recovery metadata", async () =>
    withTempDirectory("usable-git-recovery-corrupt-", async (stateRoot) => {
      const hash = (value: string) => createHash("sha256").update(value).digest("hex");
      const directory = join(stateRoot, "publish-recovery", hash("repo-a"));
      await mkdir(directory, { recursive: true });
      await Bun.write(
        join(directory, `${hash("request-1")}.json`),
        JSON.stringify({
          schemaVersion: 1,
          requestId: "request-1",
          repoKey: "repo-a",
          inputHash: "wrong-input",
          phase: "commit_started",
          preHead: null,
          files: ["other.txt"],
          index: { exists: false, checksum: null },
          ownedIndexChecksum: null,
        }),
      );
      const store = createPublishRecoveryStore({ stateRoot });

      await expect(store.read({
        requestId: "request-1",
        repoKey: "repo-a",
        inputHash: "expected-input",
        preHead: null,
        files: ["file.txt"],
      })).rejects.toThrow("Recovery metadata identity does not match the request");
    }));
});

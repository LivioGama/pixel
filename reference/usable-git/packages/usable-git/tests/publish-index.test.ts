import { describe, expect, test } from "bun:test";
import { readFile } from "node:fs/promises";
import { join } from "node:path";
import {
  captureIndexSnapshot,
  indexChecksum,
  restoreIndexIfOwned,
} from "../src/mutations/publish-index.ts";
import { withTempDirectory } from "./support/temp.ts";

describe("publish index recovery", () => {
  test("restores exact original bytes only while the intermediate index is service-owned", async () =>
    withTempDirectory("usable-git-index-", async (directory) => {
      const indexPath = join(directory, "index");
      await Bun.write(indexPath, new Uint8Array([1, 2, 3, 4]));
      const snapshot = await captureIndexSnapshot(indexPath);
      await Bun.write(indexPath, new Uint8Array([5, 6, 7]));
      const ownedChecksum = await indexChecksum(indexPath);

      await restoreIndexIfOwned(indexPath, snapshot, ownedChecksum);

      expect(new Uint8Array(await readFile(indexPath))).toEqual(
        new Uint8Array([1, 2, 3, 4]),
      );
    }));

  test("refuses to overwrite an index changed by another actor", async () =>
    withTempDirectory("usable-git-index-", async (directory) => {
      const indexPath = join(directory, "index");
      await Bun.write(indexPath, new Uint8Array([1, 2, 3]));
      const snapshot = await captureIndexSnapshot(indexPath);
      await Bun.write(indexPath, new Uint8Array([4, 5, 6]));
      const ownedChecksum = await indexChecksum(indexPath);
      await Bun.write(indexPath, new Uint8Array([7, 8, 9]));

      expect(
        await restoreIndexIfOwned(indexPath, snapshot, ownedChecksum),
      ).toBe(false);
      expect(new Uint8Array(await readFile(indexPath))).toEqual(
        new Uint8Array([7, 8, 9]),
      );
    }));

  test("restores an originally absent index by removing the owned intermediate", async () =>
    withTempDirectory("usable-git-index-", async (directory) => {
      const indexPath = join(directory, "index");
      const snapshot = await captureIndexSnapshot(indexPath);
      await Bun.write(indexPath, new Uint8Array([4, 5, 6]));
      const ownedChecksum = await indexChecksum(indexPath);

      expect(
        await restoreIndexIfOwned(indexPath, snapshot, ownedChecksum),
      ).toBe(true);
      expect(await Bun.file(indexPath).exists()).toBe(false);
    }));
});

import { describe, expect, test } from "bun:test";
import { join } from "node:path";
import {
  RepositoryBusyError,
  acquireRepositoryLock,
} from "@usable-git/mutations/repository-lock";
import { withTempDirectory } from "./support/temp";

describe("repository mutation lock", () => {
  test("refuses a second writer for the same Git common directory", async () => {
    await withTempDirectory("usable-git-lock-", async (directory) => {
      const stateRoot = join(directory, "state");
      const commonDirectory = join(directory, "repo", ".git");
      const first = await acquireRepositoryLock(commonDirectory, { stateRoot });

      await expect(
        acquireRepositoryLock(commonDirectory, { stateRoot }),
      ).rejects.toBeInstanceOf(RepositoryBusyError);

      await first.release();
      const second = await acquireRepositoryLock(commonDirectory, { stateRoot });
      await second.release();
    });
  });

  test("allows different repositories to mutate concurrently", async () => {
    await withTempDirectory("usable-git-locks-", async (directory) => {
      const stateRoot = join(directory, "state");
      const first = await acquireRepositoryLock(join(directory, "a", ".git"), {
        stateRoot,
      });
      const second = await acquireRepositoryLock(join(directory, "b", ".git"), {
        stateRoot,
      });

      await Promise.all([first.release(), second.release()]);
    });
  });
});

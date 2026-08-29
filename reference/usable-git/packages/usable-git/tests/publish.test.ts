import { afterEach, describe, expect, test } from "bun:test";
import { chmod, mkdir, readFile, rename, rm } from "node:fs/promises";
import { join } from "node:path";
import {
  publish,
  PublishOperationError,
  type PublishRequest,
} from "../src/operations/publish.ts";
import { inspectDetailed } from "../src/operations/inspect.ts";
import { acquireRepositoryLock } from "../src/mutations/repository-lock.ts";
import { createGitRunner, type GitRunner } from "../src/git/runner.ts";
import {
  commitFile,
  createRepository,
  type TestRepository,
  writeFile,
} from "./helpers/repository.ts";
import { withTempDirectory } from "./support/temp.ts";

const repositories: TestRepository[] = [];

afterEach(async () =>
  Promise.all(repositories.splice(0).map(({ cleanup }) => cleanup())),
);

const repository = async () => {
  const created = await createRepository();
  repositories.push(created);
  await created.run("config", "user.name", "Usable Git Test");
  await created.run("config", "user.email", "usable-git@example.test");
  return created;
};

const requestFor = async (
  repo: TestRepository,
  files: string[],
  requestId: string,
): Promise<PublishRequest> => {
  const snapshot = await inspectDetailed({ repoPath: repo.path });
  const expectedFingerprints = Object.fromEntries(
    files.map((path) => {
      const change = snapshot.changes.find((candidate) => candidate.path === path);
      if (!change) throw new Error(`No inspected change for ${JSON.stringify(path)}`);
      return [path, change.fingerprint];
    }),
  );

  return {
    repoPath: repo.path,
    files,
    message: `publish ${requestId}`,
    requestId,
    expected: {
      head: snapshot.branch.oid
        ? { kind: "oid", oid: snapshot.branch.oid }
        : { kind: "unborn" },
      fingerprints: expectedFingerprints,
    },
  };
};

const expectPublishError = async (
  operation: Promise<unknown>,
  code: PublishOperationError["code"],
) => {
  try {
    await operation;
    throw new Error(`Expected ${code}`);
  } catch (error) {
    expect(error).toBeInstanceOf(PublishOperationError);
    expect((error as PublishOperationError).code).toBe(code);
  }
};

const interruptCommitRunner = (afterCommit: boolean): GitRunner => {
  const base = createGitRunner();
  return {
    run: async (cwd, args, input) => {
      if (args.includes("commit") && !afterCommit) {
        throw new Error("injected interruption before commit");
      }
      const result = await base.run(cwd, args, input);
      if (args.includes("commit") && afterCommit) {
        throw new Error("injected interruption after commit");
      }
      return result;
    },
    runChecked: base.runChecked,
  };
};

const failPostCommitVerificationRunner = (): GitRunner => {
  const base = createGitRunner();
  return {
    run: base.run,
    runChecked: async (cwd, args, input) => {
      if (args.includes("diff-tree")) {
        throw new Error("injected post-commit verification failure");
      }
      return base.runChecked(cwd, args, input);
    },
  };
};

const failPostCommitJournalRunner = (stateRoot: string): GitRunner => {
  const base = createGitRunner();
  return {
    run: async (cwd, args, input) => {
      const result = await base.run(cwd, args, input);
      if (args.includes("commit")) {
        await rm(stateRoot, { recursive: true, force: true });
        await Bun.write(stateRoot, "journal unavailable");
      }
      return result;
    },
    runChecked: base.runChecked,
  };
};

const failAfterIntentToAddRunner = (): GitRunner => {
  const base = createGitRunner();
  return {
    run: async (cwd, args, input) => {
      const result = await base.run(cwd, args, input);
      if (args.includes("add") && args.includes("--intent-to-add")) {
        return { ...result, exitCode: 1, stderr: "injected intent-to-add failure" };
      }
      return result;
    },
    runChecked: base.runChecked,
  };
};

describe("publish", () => {
  test("commits one modified path and preserves unrelated staged, unstaged, and untracked work", async () =>
    withTempDirectory("usable-git-publish-state-", async (stateRoot) => {
      const repo = await repository();
      await commitFile(repo, "selected.txt", "selected base\n", "initial");
      await commitFile(repo, "staged.txt", "staged base\n", "add staged");
      await commitFile(repo, "unstaged.txt", "unstaged base\n", "add unstaged");
      await writeFile(repo, "selected.txt", "selected published\n");
      await writeFile(repo, "staged.txt", "staged pending\n");
      await repo.run("add", "--", "staged.txt");
      await writeFile(repo, "unstaged.txt", "unstaged pending\n");
      await writeFile(repo, "loose.txt", "loose pending\n");
      const stagedEntryBefore = await repo.run("ls-files", "--stage", "--", "staged.txt");
      const request = await requestFor(repo, ["selected.txt"], "modify-selected");

      const result = await publish(request, { stateRoot });

      expect(result.committedPaths).toEqual(["selected.txt"]);
      expect(result.warnings).toEqual([]);
      expect(await repo.run("show", "HEAD:selected.txt")).toBe("selected published\n");
      expect(await repo.run("diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD")).toBe(
        "selected.txt\n",
      );
      expect(await repo.run("ls-files", "--stage", "--", "staged.txt")).toBe(
        stagedEntryBefore,
      );
      expect(await repo.run("diff", "--cached", "--name-only")).toBe("staged.txt\n");
      expect(await readFile(join(repo.path, "unstaged.txt"), "utf8")).toBe(
        "unstaged pending\n",
      );
      expect(await readFile(join(repo.path, "loose.txt"), "utf8")).toBe("loose pending\n");
      expect(await repo.run("fsck", "--strict")).toBe("");
    }));

  test("commits a selected new file without consuming unrelated staged work", async () =>
    withTempDirectory("usable-git-publish-state-", async (stateRoot) => {
      const repo = await repository();
      await commitFile(repo, "base.txt", "base\n", "initial");
      await writeFile(repo, "unrelated.txt", "staged\n");
      await repo.run("add", "--", "unrelated.txt");
      await writeFile(repo, "new.txt", "new\n");
      const request = await requestFor(repo, ["new.txt"], "new-selected");

      await publish(request, { stateRoot });

      expect(await repo.run("show", "HEAD:new.txt")).toBe("new\n");
      expect(await repo.run("ls-tree", "--name-only", "HEAD")).toBe(
        "base.txt\nnew.txt\n",
      );
      expect(await repo.run("diff", "--cached", "--name-only")).toBe("unrelated.txt\n");
      await repo.run("fsck", "--strict");
    }));

  test("publishes an unusual newline filename through NUL-delimited pathspec input", async () =>
    withTempDirectory("usable-git-publish-state-", async (stateRoot) => {
      const repo = await repository();
      await commitFile(repo, "base.txt", "base\n", "initial");
      const file = "new\nline.txt";
      await writeFile(repo, file, "unusual\n");
      const request = await requestFor(repo, [file], "unusual-path");

      const result = await publish(request, { stateRoot });

      expect(result.committedPaths).toEqual([file]);
      expect(await repo.run("show", `HEAD:${file}`)).toBe("unusual\n");
      expect(await repo.run("status", "--porcelain=v1", "-z")).toBe("");
      await repo.run("fsck", "--strict");
    }));

  test("commits a selected deletion", async () =>
    withTempDirectory("usable-git-publish-state-", async (stateRoot) => {
      const repo = await repository();
      await commitFile(repo, "deleted.txt", "remove me\n", "initial");
      await Bun.file(join(repo.path, "deleted.txt")).delete();
      const request = await requestFor(repo, ["deleted.txt"], "delete-selected");

      await publish(request, { stateRoot });

      expect(await repo.run("diff-tree", "-M", "--no-commit-id", "--name-status", "-r", "HEAD")).toBe(
        "D\tdeleted.txt\n",
      );
      await repo.run("fsck", "--strict");
    }));

  test("commits complete worktree contents over an older staged selected version", async () =>
    withTempDirectory("usable-git-publish-state-", async (stateRoot) => {
      const repo = await repository();
      await commitFile(repo, "selected.txt", "base\n", "initial");
      await writeFile(repo, "selected.txt", "staged old\n");
      await repo.run("add", "--", "selected.txt");
      await writeFile(repo, "selected.txt", "worktree current\n");
      const request = await requestFor(repo, ["selected.txt"], "staged-and-unstaged");

      await publish(request, { stateRoot });

      expect(await repo.run("show", "HEAD:selected.txt")).toBe("worktree current\n");
      expect(await repo.run("status", "--porcelain=v1")).toBe("");
      await repo.run("fsck", "--strict");
    }));

  test("commits both sides of a worktree rename as one exact selection", async () =>
    withTempDirectory("usable-git-publish-state-", async (stateRoot) => {
      const repo = await repository();
      await commitFile(repo, "old.txt", "renamed\n", "initial");
      await rename(join(repo.path, "old.txt"), join(repo.path, "new.txt"));
      const request = await requestFor(repo, ["old.txt", "new.txt"], "rename-selected");

      await publish(request, { stateRoot });

      expect(await repo.run("diff-tree", "-M", "--no-commit-id", "--name-status", "-r", "HEAD")).toBe(
        "R100\told.txt\tnew.txt\n",
      );
      await repo.run("fsck", "--strict");
    }));

  test("creates an unborn initial commit containing only selected files", async () =>
    withTempDirectory("usable-git-publish-state-", async (stateRoot) => {
      const repo = await repository();
      await writeFile(repo, "unrelated.txt", "staged\n");
      await repo.run("add", "--", "unrelated.txt");
      await writeFile(repo, "selected.txt", "selected\n");
      const request = await requestFor(repo, ["selected.txt"], "unborn-selected");

      await publish(request, { stateRoot });

      expect(await repo.run("ls-tree", "--name-only", "HEAD")).toBe("selected.txt\n");
      expect(await repo.run("diff", "--cached", "--name-only")).toBe("unrelated.txt\n");
      await repo.run("fsck", "--strict");
    }));

  test("restores the exact index and leaves HEAD unchanged when a hook rejects the commit", async () =>
    withTempDirectory("usable-git-publish-state-", async (stateRoot) => {
      const repo = await repository();
      await commitFile(repo, "base.txt", "base\n", "initial");
      await writeFile(repo, "selected.txt", "new\n");
      await mkdir(join(repo.path, ".git", "hooks"), { recursive: true });
      const hookPath = join(repo.path, ".git", "hooks", "pre-commit");
      await Bun.write(hookPath, "#!/bin/sh\nexit 1\n");
      await chmod(hookPath, 0o755);
      const request = await requestFor(repo, ["selected.txt"], "hook-rejected");
      const indexBefore = await readFile(join(repo.path, ".git", "index"));
      const headBefore = await repo.run("rev-parse", "HEAD");

      await expectPublishError(publish(request, { stateRoot }), "HOOK_FAILED");

      expect(await readFile(join(repo.path, ".git", "index"))).toEqual(indexBefore);
      expect(await repo.run("rev-parse", "HEAD")).toBe(headBefore);
      expect(await repo.run("status", "--porcelain=v1")).toBe("?? selected.txt\n");
    }));

  test("restores the exact index when intent-to-add reports failure after mutation", async () =>
    withTempDirectory("usable-git-publish-state-", async (stateRoot) => {
      const repo = await repository();
      await commitFile(repo, "base.txt", "base\n", "initial");
      await writeFile(repo, "selected.txt", "new\n");
      const request = await requestFor(repo, ["selected.txt"], "intent-add-failure");
      const indexBefore = await readFile(join(repo.path, ".git", "index"));

      await expectPublishError(
        publish(request, { stateRoot, runner: failAfterIntentToAddRunner() }),
        "GIT_FAILED",
      );

      expect(await readFile(join(repo.path, ".git", "index"))).toEqual(indexBefore);
      expect(await repo.run("status", "--porcelain=v1")).toBe("?? selected.txt\n");
      expect(await repo.run("rev-list", "--count", "HEAD")).toBe("1\n");
    }));

  test("classifies missing identity and signing failures without moving HEAD", async () =>
    withTempDirectory("usable-git-publish-state-", async (stateRoot) => {
      const identityRepo = await repository();
      await commitFile(identityRepo, "selected.txt", "base\n", "initial");
      await writeFile(identityRepo, "selected.txt", "pending\n");
      await identityRepo.run("config", "user.name", "");
      const identityRequest = await requestFor(
        identityRepo,
        ["selected.txt"],
        "identity-missing",
      );
      const identityHead = await identityRepo.run("rev-parse", "HEAD");

      await expectPublishError(
        publish(identityRequest, { stateRoot }),
        "IDENTITY_MISSING",
      );
      expect(await identityRepo.run("rev-parse", "HEAD")).toBe(identityHead);

      const signingRepo = await repository();
      await commitFile(signingRepo, "selected.txt", "base\n", "initial");
      await writeFile(signingRepo, "selected.txt", "pending\n");
      await signingRepo.run("config", "commit.gpgSign", "true");
      await signingRepo.run("config", "gpg.program", "/usr/bin/false");
      const signingRequest = await requestFor(
        signingRepo,
        ["selected.txt"],
        "signing-failed",
      );
      const signingHead = await signingRepo.run("rev-parse", "HEAD");

      await expectPublishError(
        publish(signingRequest, { stateRoot }),
        "SIGNING_FAILED",
      );
      expect(await signingRepo.run("rev-parse", "HEAD")).toBe(signingHead);
    }));

  test("refuses stale HEAD and stale fingerprints before creating a commit", async () =>
    withTempDirectory("usable-git-publish-state-", async (stateRoot) => {
      const repo = await repository();
      await commitFile(repo, "selected.txt", "base\n", "initial");
      await writeFile(repo, "selected.txt", "pending\n");
      const staleFingerprint = await requestFor(repo, ["selected.txt"], "stale-fingerprint");
      await writeFile(repo, "selected.txt", "changed again\n");
      const headBefore = await repo.run("rev-parse", "HEAD");

      await expectPublishError(publish(staleFingerprint, { stateRoot }), "STALE_STATE");
      expect(await repo.run("rev-parse", "HEAD")).toBe(headBefore);

      const staleHead = await requestFor(repo, ["selected.txt"], "stale-head");
      await writeFile(repo, "other.txt", "other\n");
      await repo.run("add", "--", "other.txt");
      await repo.run("commit", "--quiet", "-m", "move head");
      const movedHead = await repo.run("rev-parse", "HEAD");

      await expectPublishError(publish(staleHead, { stateRoot }), "STALE_STATE");
      expect(await repo.run("rev-parse", "HEAD")).toBe(movedHead);
    }));

  test("refuses detached HEAD and active sequencer state", async () =>
    withTempDirectory("usable-git-publish-state-", async (stateRoot) => {
      const repo = await repository();
      await commitFile(repo, "selected.txt", "base\n", "initial");
      await writeFile(repo, "selected.txt", "pending\n");
      const detachedRequest = await requestFor(repo, ["selected.txt"], "detached");
      await repo.run("checkout", "--quiet", "--detach");
      await expectPublishError(publish(detachedRequest, { stateRoot }), "UNSUPPORTED_STATE");

      await repo.run("checkout", "--quiet", "-");
      const sequencerRequest = await requestFor(repo, ["selected.txt"], "sequencer");
      await Bun.write(join(repo.path, ".git", "MERGE_HEAD"), "a".repeat(40) + "\n");
      await expectPublishError(publish(sequencerRequest, { stateRoot }), "UNSUPPORTED_STATE");
    }));

  test("rejects directories, ignored files, and gitlinks without moving HEAD", async () =>
    withTempDirectory("usable-git-publish-state-", async (stateRoot) => {
      const repo = await repository();
      await commitFile(repo, ".gitignore", "*.ignored\n", "initial");
      await mkdir(join(repo.path, "directory"));
      await writeFile(repo, "file.ignored", "ignored\n");
      const head = (await repo.run("rev-parse", "HEAD")).trim();
      const invalidRequest = (
        file: string,
        requestId: string,
      ): PublishRequest => ({
        repoPath: repo.path,
        files: [file],
        message: "must fail",
        requestId,
        expected: {
          head: { kind: "oid", oid: head },
          fingerprints: { [file]: "a".repeat(64) },
        },
      });

      await expectPublishError(
        publish(invalidRequest("directory", "directory-invalid"), { stateRoot }),
        "INVALID_PATH",
      );
      await expectPublishError(
        publish(invalidRequest("file.ignored", "ignored-invalid"), { stateRoot }),
        "INVALID_PATH",
      );

      await repo.run(
        "update-index",
        "--add",
        "--cacheinfo",
        `160000,${head},module`,
      );
      await expectPublishError(
        publish(invalidRequest("module", "gitlink-invalid"), { stateRoot }),
        "INVALID_PATH",
      );
      expect((await repo.run("rev-parse", "HEAD")).trim()).toBe(head);
    }));

  test("returns busy_repository under the shared mutation lock", async () =>
    withTempDirectory("usable-git-publish-state-", async (stateRoot) => {
      const repo = await repository();
      await commitFile(repo, "selected.txt", "base\n", "initial");
      await writeFile(repo, "selected.txt", "pending\n");
      const request = await requestFor(repo, ["selected.txt"], "contended");
      const snapshot = await inspectDetailed({ repoPath: repo.path });
      const lock = await acquireRepositoryLock(snapshot.repository.commonDir, { stateRoot });
      try {
        await expectPublishError(publish(request, { stateRoot }), "BUSY_REPOSITORY");
      } finally {
        await lock.release();
      }
    }));

  test("replays a completed request without creating another commit", async () =>
    withTempDirectory("usable-git-publish-state-", async (stateRoot) => {
      const repo = await repository();
      await commitFile(repo, "selected.txt", "base\n", "initial");
      await writeFile(repo, "selected.txt", "published\n");
      const request = await requestFor(repo, ["selected.txt"], "replay");

      const first = await publish(request, { stateRoot });
      const second = await publish(request, { stateRoot });

      expect(second).toEqual(first);
      expect(await repo.run("rev-list", "--count", "HEAD")).toBe("2\n");
    }));

  test("recovers an observed commit after interruption without creating a second commit", async () =>
    withTempDirectory("usable-git-publish-state-", async (stateRoot) => {
      const repo = await repository();
      await commitFile(repo, "selected.txt", "base\n", "initial");
      await writeFile(repo, "selected.txt", "published\n");
      const request = await requestFor(repo, ["selected.txt"], "recover-observed");

      await expect(
        publish(request, { stateRoot, runner: interruptCommitRunner(true) }),
      ).rejects.toThrow("injected interruption after commit");
      const recovered = await publish(request, { stateRoot });

      expect(recovered.warnings).toContain(
        "Recovered a commit observed after an interrupted publish",
      );
      expect(await repo.run("rev-list", "--count", "HEAD")).toBe("2\n");
      expect(await repo.run("show", "HEAD:selected.txt")).toBe("published\n");
      await repo.run("fsck", "--strict");
    }));

  test("refuses to claim an unrelated commit as interrupted publish success", async () =>
    withTempDirectory("usable-git-publish-state-", async (stateRoot) => {
      const repo = await repository();
      await commitFile(repo, "selected.txt", "base\n", "initial");
      await writeFile(repo, "selected.txt", "published\n");
      const request = await requestFor(repo, ["selected.txt"], "recover-unrelated");

      await expect(
        publish(request, { stateRoot, runner: interruptCommitRunner(false) }),
      ).rejects.toThrow("injected interruption before commit");
      await repo.run("add", "--", "selected.txt");
      await repo.run("commit", "-m", "unrelated actor commit");
      const unrelatedHead = (await repo.run("rev-parse", "HEAD")).trim();

      await expectPublishError(publish(request, { stateRoot }), "RECOVERY_CONFLICT");
      expect((await repo.run("rev-parse", "HEAD")).trim()).toBe(unrelatedHead);
      expect(await repo.run("rev-list", "--count", "HEAD")).toBe("2\n");
      await repo.run("fsck", "--strict");
    }));

  test("reports an observed commit when post-commit verification fails", async () =>
    withTempDirectory("usable-git-publish-state-", async (stateRoot) => {
      const repo = await repository();
      await commitFile(repo, "selected.txt", "base\n", "initial");
      await writeFile(repo, "selected.txt", "published\n");
      const request = await requestFor(repo, ["selected.txt"], "verify-after-observed");

      const result = await publish(request, {
        stateRoot,
        runner: failPostCommitVerificationRunner(),
      });

      expect(result.commitOid).toBe((await repo.run("rev-parse", "HEAD")).trim());
      expect(result.warnings.join("\n")).toContain(
        "injected post-commit verification failure",
      );
      expect(await repo.run("rev-list", "--count", "HEAD")).toBe("2\n");
      await repo.run("fsck", "--strict");
    }));

  test("reports an observed commit when the terminal journal write fails", async () =>
    withTempDirectory("usable-git-publish-state-", async (stateRoot) => {
      const repo = await repository();
      await commitFile(repo, "selected.txt", "base\n", "initial");
      await writeFile(repo, "selected.txt", "published\n");
      const request = await requestFor(repo, ["selected.txt"], "journal-after-observed");

      const result = await publish(request, {
        stateRoot,
        runner: failPostCommitJournalRunner(stateRoot),
      });

      expect(result.commitOid).toBe((await repo.run("rev-parse", "HEAD")).trim());
      expect(result.warnings.join("\n")).toContain("journal");
      expect(await repo.run("rev-list", "--count", "HEAD")).toBe("2\n");
      await repo.run("fsck", "--strict");
    }));

  test("safely rolls back an owned intent-to-add index after interruption before commit", async () =>
    withTempDirectory("usable-git-publish-state-", async (stateRoot) => {
      const repo = await repository();
      await commitFile(repo, "base.txt", "base\n", "initial");
      await writeFile(repo, "selected.txt", "new\n");
      const request = await requestFor(repo, ["selected.txt"], "recover-rollback");
      const indexBefore = await readFile(join(repo.path, ".git", "index"));

      await expect(
        publish(request, { stateRoot, runner: interruptCommitRunner(false) }),
      ).rejects.toThrow("injected interruption before commit");
      await expectPublishError(publish(request, { stateRoot }), "GIT_FAILED");

      expect(await readFile(join(repo.path, ".git", "index"))).toEqual(indexBefore);
      expect(await repo.run("status", "--porcelain=v1")).toBe("?? selected.txt\n");
      expect(await repo.run("rev-list", "--count", "HEAD")).toBe("1\n");
    }));

  test("refuses recovery when another actor changed the interrupted index", async () =>
    withTempDirectory("usable-git-publish-state-", async (stateRoot) => {
      const repo = await repository();
      await commitFile(repo, "base.txt", "base\n", "initial");
      await writeFile(repo, "selected.txt", "new\n");
      const request = await requestFor(repo, ["selected.txt"], "recover-conflict");

      await expect(
        publish(request, { stateRoot, runner: interruptCommitRunner(false) }),
      ).rejects.toThrow("injected interruption before commit");
      await writeFile(repo, "other.txt", "other actor\n");
      await repo.run("add", "--", "other.txt");
      const indexAfterOtherActor = await readFile(join(repo.path, ".git", "index"));

      await expectPublishError(
        publish(request, { stateRoot }),
        "RECOVERY_CONFLICT",
      );
      expect(await readFile(join(repo.path, ".git", "index"))).toEqual(
        indexAfterOtherActor,
      );
      expect(await repo.run("diff", "--cached", "--name-only")).toContain("other.txt");
    }));

  test("returns nothing_to_commit without creating an empty commit", async () =>
    withTempDirectory("usable-git-publish-state-", async (stateRoot) => {
      const repo = await repository();
      await commitFile(repo, "selected.txt", "base\n", "initial");
      const head = (await repo.run("rev-parse", "HEAD")).trim();
      const request: PublishRequest = {
        repoPath: repo.path,
        files: ["selected.txt"],
        message: "no changes",
        requestId: "nothing",
        expected: {
          head: { kind: "oid", oid: head },
          fingerprints: { "selected.txt": "a".repeat(64) },
        },
      };

      await expectPublishError(publish(request, { stateRoot }), "NOTHING_TO_COMMIT");
      expect((await repo.run("rev-parse", "HEAD")).trim()).toBe(head);
    }));
});

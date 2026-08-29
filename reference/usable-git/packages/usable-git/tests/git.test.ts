import { describe, expect, test } from "bun:test";
import { rm } from "node:fs/promises";
import { join } from "node:path";
import { discoverRepository } from "../src/git/repository.ts";
import { createGitRunner, withGitMetrics } from "../src/git/runner.ts";
import { parsePorcelainV2 } from "../src/git/status.ts";
import { validateLiteralFiles } from "../src/git/paths.ts";
import { commitFile, createRepository, writeFile } from "./helpers/repository.ts";

describe("Git primitives", () => {
  test("runner uses direct argv and stable non-interactive environment", async () => {
    const repository = await createRepository();
    try {
      const calls: string[][] = [];
      const runner = createGitRunner({
        onSpawn: ({ argv }) => calls.push(argv),
      });
      const result = await runner.run(repository.path, ["status", "--porcelain=v2", "-z"]);

      expect(result.exitCode).toBe(0);
      expect(calls[0]).toContain("--no-pager");
      expect(result.processCount).toBe(1);
    } finally {
      await repository.cleanup();
    }
  });

  test("aggregates Git subprocesses across one semantic operation", async () => {
    const repository = await createRepository();
    try {
      const runner = createGitRunner();
      const measured = await withGitMetrics(async () => {
        await runner.runChecked(repository.path, ["status", "--porcelain=v2"]);
        await runner.runChecked(repository.path, ["rev-parse", "--git-dir"]);
        return "done";
      });
      expect(measured).toEqual({ result: "done", gitSubprocessCount: 2 });
    } finally {
      await repository.cleanup();
    }
  });

  test("removes inherited Git config injection variables", async () => {
    const repository = await createRepository();
    const previous = {
      count: process.env.GIT_CONFIG_COUNT,
      key: process.env.GIT_CONFIG_KEY_0,
      value: process.env.GIT_CONFIG_VALUE_0,
    };
    try {
      process.env.GIT_CONFIG_COUNT = "1";
      process.env.GIT_CONFIG_KEY_0 = "usable-git.injected";
      process.env.GIT_CONFIG_VALUE_0 = "secret";
      const result = await createGitRunner().run(repository.path, [
        "config",
        "--get",
        "usable-git.injected",
      ]);
      expect(result.exitCode).toBe(1);
      expect(result.stdout).toBe("");
    } finally {
      if (previous.count === undefined) delete process.env.GIT_CONFIG_COUNT;
      else process.env.GIT_CONFIG_COUNT = previous.count;
      if (previous.key === undefined) delete process.env.GIT_CONFIG_KEY_0;
      else process.env.GIT_CONFIG_KEY_0 = previous.key;
      if (previous.value === undefined) delete process.env.GIT_CONFIG_VALUE_0;
      else process.env.GIT_CONFIG_VALUE_0 = previous.value;
      await repository.cleanup();
    }
  });

  test("strips the banned Anthropic API key from Git subprocesses", async () => {
    const repository = await createRepository();
    const previous = process.env.ANTHROPIC_API_KEY;
    try {
      process.env.ANTHROPIC_API_KEY = "must-never-reach-git";
      const result = await createGitRunner().run(repository.path, [
        "-c",
        "alias.check-env=!test -z \"$ANTHROPIC_API_KEY\"",
        "check-env",
      ]);
      expect(result.exitCode).toBe(0);
    } finally {
      if (previous === undefined) delete process.env.ANTHROPIC_API_KEY;
      else process.env.ANTHROPIC_API_KEY = previous;
      await repository.cleanup();
    }
  });

  test("bounds Git output and redacts credentials", async () => {
    const repository = await createRepository();
    try {
      const runner = createGitRunner({ maxOutputBytes: 96 });
      const result = await runner.run(repository.path, [
        "-c",
        "alias.leak=!printf 'https://user:secret@example.test Bearer supersecret ghp_abcdefghijklmnopqrstuvwxyz1234567890 padding-padding-padding-padding'",
        "leak",
      ]);

      expect(Buffer.byteLength(result.stdout)).toBeLessThanOrEqual(96);
      expect(result.stdout).not.toContain("secret");
      expect(result.stdout).not.toContain("supersecret");
      expect(result.stdout).not.toContain("ghp_abcdefghijklmnopqrstuvwxyz1234567890");
      expect(result.stdoutTruncated).toBe(true);
    } finally {
      await repository.cleanup();
    }
  });

  test("terminates Git commands after the configured timeout", async () => {
    const repository = await createRepository();
    try {
      const runner = createGitRunner({ timeoutMs: 25 });
      const startedAt = performance.now();
      const result = await runner.run(repository.path, [
        "-c",
        "alias.wait=!sleep 2",
        "wait",
      ]);

      expect(result.timedOut).toBe(true);
      expect(result.exitCode).not.toBe(0);
      expect(performance.now() - startedAt).toBeLessThan(1_000);
    } finally {
      await repository.cleanup();
    }
  });

  test("discovers the worktree and common Git directory", async () => {
    const repository = await createRepository();
    try {
      const discovered = await discoverRepository(join(repository.path, "."));
      expect(discovered.root).toBe(repository.path);
      expect(discovered.isBare).toBe(false);
      expect(discovered.commonDir).toEndWith("/.git");
    } finally {
      await repository.cleanup();
    }
  });

  test("validates literal files and preserves unusual names", async () => {
    const repository = await createRepository();
    try {
      await writeFile(repository, "line\nbreak.txt", "hello");
      expect(await validateLiteralFiles(repository.path, ["line\nbreak.txt"])).toEqual([
        "line\nbreak.txt",
      ]);
      await expect(validateLiteralFiles(repository.path, ["."])).rejects.toThrow();
      await expect(validateLiteralFiles(repository.path, ["../escape"])).rejects.toThrow();
      await expect(validateLiteralFiles(repository.path, [":(glob)*"])).rejects.toThrow();
      await expect(
        validateLiteralFiles(repository.path, ["line\nbreak.txt", "line\nbreak.txt"]),
      ).rejects.toThrow(/duplicate/i);
    } finally {
      await repository.cleanup();
    }
  });

  test("accepts an exact path whose deletion is already staged", async () => {
    const repository = await createRepository();
    try {
      await commitFile(repository, "deleted.txt", "before\n", "baseline");
      await rm(join(repository.path, "deleted.txt"));
      await repository.run("add", "--", "deleted.txt");

      expect(await validateLiteralFiles(repository.path, ["deleted.txt"])).toEqual([
        "deleted.txt",
      ]);
    } finally {
      await repository.cleanup();
    }
  });

  test("parses NUL-delimited ordinary, renamed, conflicted, and untracked records", () => {
    const status = [
      "# branch.oid abc",
      "# branch.head main",
      "1 M. N... 100644 100644 100644 aaa bbb staged.txt",
      "2 R. N... 100644 100644 100644 aaa bbb R100 new\nname.txt",
      "old\nname.txt",
      "u UU N... 100644 100644 100644 100644 aaa bbb ccc conflict.txt",
      "? odd\nfile.txt",
      "",
    ].join("\0");

    const parsed = parsePorcelainV2(status);
    expect(parsed.branch.head).toBe("main");
    expect(parsed.changes.map(({ path }) => path)).toEqual([
      "staged.txt",
      "new\nname.txt",
      "conflict.txt",
      "odd\nfile.txt",
    ]);
    expect(parsed.changes[1]?.originalPath).toBe("old\nname.txt");
    expect(parsed.changes[2]?.conflicted).toBe(true);
  });
});

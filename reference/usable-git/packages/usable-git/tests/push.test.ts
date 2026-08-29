import { afterEach, describe, expect, setDefaultTimeout, test } from "bun:test";
import { createHash } from "node:crypto";
import { mkdir, mkdtemp, realpath, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { GitRunner } from "@usable-git/git/runner.ts";
import { createGitRunner } from "@usable-git/git/runner.ts";
import { createOperationJournal } from "@usable-git/mutations/operation-journal.ts";
import { push } from "@usable-git/operations/push.ts";
import { UsableGitError } from "@usable-git/errors.ts";

const cleanups: Array<() => Promise<void>> = [];
setDefaultTimeout(30_000);
const gitEnvironment = {
  ...process.env,
  GIT_AUTHOR_NAME: "Usable Git Push Test",
  GIT_AUTHOR_EMAIL: "usable-git@example.test",
  GIT_COMMITTER_NAME: "Usable Git Push Test",
  GIT_COMMITTER_EMAIL: "usable-git@example.test",
};

const runGit = async (cwd: string, ...args: string[]) => {
  const child = Bun.spawn(["git", ...args], {
    cwd,
    env: gitEnvironment,
    stdout: "pipe",
    stderr: "pipe",
  });
  const [stdout, stderr, exitCode] = await Promise.all([
    new Response(child.stdout).text(),
    new Response(child.stderr).text(),
    child.exited,
  ]);
  if (exitCode !== 0) throw new Error(`git ${args.join(" ")} failed: ${stderr}`);
  return stdout.trim();
};

const createFixture = async () => {
  const root = await realpath(await mkdtemp(join(tmpdir(), "usable-git-push-")));
  const local = join(root, "local");
  const remote = join(root, "remote.git");
  const stateRoot = join(root, "state");
  await mkdir(local);
  await runGit(local, "init", "--quiet", "--initial-branch=main");
  await Bun.write(join(local, "tracked.txt"), "base\n");
  await runGit(local, "add", "--", "tracked.txt");
  await runGit(local, "commit", "--quiet", "-m", "base");
  await runGit(root, "init", "--quiet", "--bare", remote);
  await runGit(local, "remote", "add", "origin", remote);
  await runGit(local, "push", "--quiet", "origin", "refs/heads/main:refs/heads/main");
  await runGit(remote, "branch", "other", "refs/heads/main");

  cleanups.push(() => rm(root, { recursive: true, force: true }));
  return { root, local, remote, stateRoot };
};

const commit = async (repository: string, contents: string, message: string) => {
  await Bun.write(join(repository, "tracked.txt"), contents);
  await runGit(repository, "add", "--", "tracked.txt");
  await runGit(repository, "commit", "--quiet", "-m", message);
  return runGit(repository, "rev-parse", "HEAD");
};

const fastForwardRequest = (
  fixture: Awaited<ReturnType<typeof createFixture>>,
  expectedSourceOid: string,
  requestId: string,
) => ({
  repoPath: fixture.local,
  remote: "origin",
  sourceRef: "refs/heads/main",
  targetRef: "refs/heads/main",
  requestId,
  expectedSourceOid,
  mode: { kind: "fast-forward" as const },
});

afterEach(async () => {
  await Promise.all(cleanups.splice(0).map((cleanup) => cleanup()));
});

describe("push", () => {
  test("updates exactly one configured remote branch and replays idempotently", async () => {
    const fixture = await createFixture();
    const otherBefore = await runGit(fixture.remote, "rev-parse", "refs/heads/other");
    const sourceOid = await commit(fixture.local, "local change\n", "local change");
    const pushArgv: string[][] = [];
    const runner = createGitRunner({
      onSpawn: ({ argv }) => {
        if (argv.includes("push")) pushArgv.push(argv);
      },
    });
    const request = fastForwardRequest(fixture, sourceOid, "push-fast-forward");

    const result = await push(request, { runner, stateRoot: fixture.stateRoot });
    const replay = await push(request, { runner, stateRoot: fixture.stateRoot });

    expect(result).toEqual({
      remote: "origin",
      sourceRef: "refs/heads/main",
      targetRef: "refs/heads/main",
      oldTargetOid: otherBefore,
      newTargetOid: sourceOid,
      mode: "fast-forward",
      confirmedAfterFailure: false,
    });
    expect(replay).toEqual(result);
    expect(pushArgv).toHaveLength(1);
    expect(pushArgv[0]?.filter((argument) => argument.includes(":"))).toEqual([
      "refs/heads/main:refs/heads/main",
    ]);
    expect(await runGit(fixture.remote, "rev-parse", "refs/heads/main")).toBe(sourceOid);
    expect(await runGit(fixture.remote, "rev-parse", "refs/heads/other")).toBe(otherBefore);
    const repoKey = createHash("sha256")
      .update(await realpath(join(fixture.local, ".git")))
      .digest("hex");
    const journal = await createOperationJournal({ stateRoot: fixture.stateRoot }).read(
      repoKey,
      request.requestId,
    );
    expect(journal?.inputHash).toMatch(/^[a-f0-9]{64}$/);
    expect(
      (journal?.result as { resultHash?: string } | undefined)?.resultHash,
    ).toMatch(/^[a-f0-9]{64}$/);
    await runGit(fixture.remote, "fsck", "--strict");
  });

  test("rejects a stale local source before contacting the remote", async () => {
    const fixture = await createFixture();
    const sourceOid = await commit(fixture.local, "new\n", "new");
    let remoteContacted = false;
    const runner = createGitRunner({
      onSpawn: ({ argv }) => {
        if (argv.includes("push") || argv.includes("ls-remote")) remoteContacted = true;
      },
    });

    await expect(
      push(fastForwardRequest(fixture, "a".repeat(40), "push-stale"), {
        runner,
        stateRoot: fixture.stateRoot,
      }),
    ).rejects.toMatchObject({ code: "STALE_STATE" });
    expect(sourceOid).not.toBe("a".repeat(40));
    expect(remoteContacted).toBe(false);
  });

  test("returns non-fast-forward and never retries with force", async () => {
    const fixture = await createFixture();
    const base = await runGit(fixture.local, "rev-parse", "HEAD");
    const sourceOid = await commit(fixture.local, "local\n", "local");
    const competitor = join(fixture.root, "competitor");
    await runGit(fixture.root, "clone", "--quiet", fixture.remote, competitor);
    await runGit(competitor, "switch", "--quiet", "main");
    await commit(competitor, "remote\n", "remote");
    await runGit(competitor, "push", "--quiet", "origin", "refs/heads/main:refs/heads/main");
    expect(await runGit(fixture.local, "rev-parse", "HEAD^")).toBe(base);

    const pushArgv: string[][] = [];
    const runner = createGitRunner({
      onSpawn: ({ argv }) => {
        if (argv.includes("push")) pushArgv.push(argv);
      },
    });
    const request = fastForwardRequest(fixture, sourceOid, "push-non-ff");
    await expect(
      push(request, {
        runner,
        stateRoot: fixture.stateRoot,
      }),
    ).rejects.toMatchObject({ code: "NON_FAST_FORWARD" });
    await expect(
      push(request, { runner, stateRoot: fixture.stateRoot }),
    ).rejects.toMatchObject({ code: "NON_FAST_FORWARD" });
    expect(pushArgv).toHaveLength(1);
    expect(pushArgv[0]?.some((argument) => argument.startsWith("--force"))).toBe(false);
  });

  test("uses an exact force-with-lease and rejects a stale lease", async () => {
    const fixture = await createFixture();
    const base = await runGit(fixture.local, "rev-parse", "HEAD");
    const sourceOid = await commit(fixture.local, "local lease\n", "local lease");
    const competitor = join(fixture.root, "competitor");
    await runGit(fixture.root, "clone", "--quiet", fixture.remote, competitor);
    await runGit(competitor, "switch", "--quiet", "main");
    const remoteOid = await commit(competitor, "remote lease\n", "remote lease");
    await runGit(competitor, "push", "--quiet", "origin", "refs/heads/main:refs/heads/main");

    const leaseRequest = {
      ...fastForwardRequest(fixture, sourceOid, "push-lease-stale"),
      mode: { kind: "force-with-lease" as const, expectedTargetOid: base },
    };
    await expect(
      push(leaseRequest, { stateRoot: fixture.stateRoot }),
    ).rejects.toMatchObject({ code: "LEASE_REJECTED" });

    const argv: string[][] = [];
    const runner = createGitRunner({
      onSpawn: ({ argv: command }) => {
        if (command.includes("push")) argv.push(command);
      },
    });
    const result = await push(
      {
        ...leaseRequest,
        requestId: "push-lease-exact",
        mode: { kind: "force-with-lease", expectedTargetOid: remoteOid },
      },
      { runner, stateRoot: fixture.stateRoot },
    );
    expect(result.oldTargetOid).toBe(remoteOid);
    expect(result.newTargetOid).toBe(sourceOid);
    expect(argv[0]).toContain(
      `--force-with-lease=refs/heads/main:${remoteOid}`,
    );
    expect(argv[0]).not.toContain("--force");
    expect(await runGit(fixture.remote, "rev-parse", "refs/heads/main")).toBe(sourceOid);
    await runGit(fixture.remote, "fsck", "--strict");
  });

  test("confirms uncertain transport success by querying only the target ref", async () => {
    const fixture = await createFixture();
    const sourceOid = await commit(fixture.local, "uncertain\n", "uncertain");
    const baseRunner = createGitRunner();
    const calls: string[][] = [];
    const runner = {
      run: async (cwd: string, args: string[], input?: Uint8Array) => {
        calls.push(args);
        const result = await baseRunner.run(cwd, args, input);
        return args[0] === "push"
          ? { ...result, exitCode: 1, stderr: "fatal: the remote end hung up unexpectedly" }
          : result;
      },
      runChecked: baseRunner.runChecked,
    } as GitRunner;

    const result = await push(
      fastForwardRequest(fixture, sourceOid, "push-confirmed-after-failure"),
      { runner, stateRoot: fixture.stateRoot },
    );
    expect(result.confirmedAfterFailure).toBe(true);
    expect(calls.filter(([command]) => command === "push")).toHaveLength(1);
    expect(calls.filter(([command]) => command === "ls-remote").at(-1)).toEqual([
      "ls-remote",
      "--refs",
      "origin",
      "refs/heads/main",
    ]);
  });

  test("returns network ambiguity when target state cannot be established", async () => {
    const fixture = await createFixture();
    const sourceOid = await commit(fixture.local, "ambiguous\n", "ambiguous");
    const baseRunner = createGitRunner();
    let remoteQueries = 0;
    let pushAttempts = 0;
    const runner = {
      run: async (cwd: string, args: string[], input?: Uint8Array) => {
        if (args[0] === "ls-remote") remoteQueries += 1;
        if (args[0] === "push") pushAttempts += 1;
        if (args[0] === "push" || (args[0] === "ls-remote" && remoteQueries > 1)) {
          return {
            stdout: "",
            stderr: "fatal: unable to access remote: connection reset",
            exitCode: 128,
            processCount: 1,
          };
        }
        return baseRunner.run(cwd, args, input);
      },
      runChecked: baseRunner.runChecked,
    } as GitRunner;

    await expect(
      push(fastForwardRequest(fixture, sourceOid, "push-ambiguous"), {
        runner,
        stateRoot: fixture.stateRoot,
      }),
    ).rejects.toMatchObject({ code: "NETWORK_AMBIGUITY" });
    await expect(
      push(fastForwardRequest(fixture, sourceOid, "push-ambiguous"), {
        runner,
        stateRoot: fixture.stateRoot,
      }),
    ).rejects.toMatchObject({ code: "NETWORK_AMBIGUITY" });
    expect(pushAttempts).toBe(1);
  });

  test("classifies authentication failure before push without attempting an update", async () => {
    const fixture = await createFixture();
    const sourceOid = await commit(fixture.local, "auth\n", "auth");
    const baseRunner = createGitRunner();
    let pushAttempts = 0;
    const runner = {
      run: async (cwd: string, args: string[], input?: Uint8Array) => {
        if (args[0] === "push") pushAttempts += 1;
        if (args[0] === "ls-remote") {
          return {
            stdout: "",
            stderr: "fatal: Authentication failed for 'https://user:secret@example.test/repo.git'",
            exitCode: 128,
            processCount: 1,
          };
        }
        return baseRunner.run(cwd, args, input);
      },
      runChecked: baseRunner.runChecked,
    } as GitRunner;

    await expect(
      push(fastForwardRequest(fixture, sourceOid, "push-auth"), {
        runner,
        stateRoot: fixture.stateRoot,
      }),
    ).rejects.toMatchObject({
      code: "AUTH_FAILED",
      details: {
        diagnostic: expect.not.stringContaining("user:secret"),
      },
    });
    expect(pushAttempts).toBe(0);
  });

  test("rejects an unconfigured remote name", async () => {
    const fixture = await createFixture();
    const sourceOid = await runGit(fixture.local, "rev-parse", "HEAD");
    await expect(
      push(
        {
          ...fastForwardRequest(fixture, sourceOid, "push-no-remote"),
          remote: "missing",
        },
        { stateRoot: fixture.stateRoot },
      ),
    ).rejects.toBeInstanceOf(UsableGitError);
    await expect(
      push(
        {
          ...fastForwardRequest(fixture, sourceOid, "push-no-remote-2"),
          remote: "missing",
        },
        { stateRoot: fixture.stateRoot },
      ),
    ).rejects.toMatchObject({ code: "INVALID_INPUT" });
  });
});

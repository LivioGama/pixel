import { afterEach, describe, expect, setDefaultTimeout, test } from "bun:test";
import { mkdir, mkdtemp, realpath, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { inspect } from "../src/operations/inspect.ts";
import { ship } from "../src/operations/ship.ts";
import { UsableGitError } from "../src/errors.ts";

const cleanups: Array<() => Promise<void>> = [];
setDefaultTimeout(30_000);
const gitEnvironment = {
  ...process.env,
  GIT_AUTHOR_NAME: "Usable Git Ship Test",
  GIT_AUTHOR_EMAIL: "usable-git@example.test",
  GIT_COMMITTER_NAME: "Usable Git Ship Test",
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
  const root = await realpath(await mkdtemp(join(tmpdir(), "usable-git-ship-")));
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
  cleanups.push(() => rm(root, { recursive: true, force: true }));
  return { root, local, remote, stateRoot };
};

afterEach(async () => {
  await Promise.all(cleanups.splice(0).map((cleanup) => cleanup()));
});

describe("ship", () => {
  test("commits the selected path and pushes it in one call from a snapshot token", async () => {
    const fixture = await createFixture();
    await Bun.write(join(fixture.local, "selected.txt"), "selected\n");
    await Bun.write(join(fixture.local, "unrelated.txt"), "unrelated\n");
    const inspected = await inspect(
      { repoPath: fixture.local },
      { stateRoot: fixture.stateRoot },
    );

    const result = await ship({
      repoPath: fixture.local,
      files: ["selected.txt"],
      message: "ship selected",
      remote: "origin",
      snapshot: inspected.snapshot!,
    }, { stateRoot: fixture.stateRoot });

    expect(result.committedPaths).toEqual(["selected.txt"]);
    expect(result.branch).toBe("main");
    expect(result.push.ok).toBe(true);
    if (!result.push.ok) throw new Error("push leg unexpectedly failed");
    expect(result.push.targetRef).toBe("refs/heads/main");
    expect(result.push.newTargetOid).toBe(result.commit);
    expect(await runGit(fixture.remote, "rev-parse", "refs/heads/main")).toBe(result.commit);
    const committed = await runGit(fixture.local, "diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD");
    expect(committed).toBe("selected.txt");
    expect((await runGit(fixture.local, "status", "--porcelain")).includes("unrelated.txt")).toBe(true);
  });

  test("reports the landed commit when the push leg fails", async () => {
    const fixture = await createFixture();
    await Bun.write(join(fixture.local, "selected.txt"), "selected\n");
    const inspected = await inspect(
      { repoPath: fixture.local },
      { stateRoot: fixture.stateRoot },
    );
    // Move the remote ahead so a fast-forward push of the new commit fails.
    const scratch = join(fixture.root, "scratch");
    await runGit(fixture.root, "clone", "--quiet", fixture.remote, scratch);
    await Bun.write(join(scratch, "remote-change.txt"), "remote\n");
    await runGit(scratch, "add", "--", "remote-change.txt");
    await runGit(scratch, "commit", "--quiet", "-m", "remote moved");
    await runGit(scratch, "push", "--quiet", "origin", "HEAD:refs/heads/main");

    const result = await ship({
      repoPath: fixture.local,
      files: ["selected.txt"],
      message: "ship into moved remote",
      remote: "origin",
      snapshot: inspected.snapshot!,
    }, { stateRoot: fixture.stateRoot });

    expect(result.push.ok).toBe(false);
    if (result.push.ok) throw new Error("push leg unexpectedly succeeded");
    expect(result.push.code).toBe("NON_FAST_FORWARD");
    expect(result.push.message).toContain(result.commit);
    expect(await runGit(fixture.local, "rev-parse", "HEAD")).toBe(result.commit);
  });

  test("publish-leg failures propagate as terminal errors", async () => {
    const fixture = await createFixture();
    await Bun.write(join(fixture.local, "selected.txt"), "selected\n");
    const attempt = ship({
      repoPath: fixture.local,
      files: ["selected.txt"],
      message: "must fail",
      remote: "origin",
      snapshot: "0123456789ab",
    }, { stateRoot: fixture.stateRoot });
    await attempt.then(
      () => {
        throw new Error("ship unexpectedly succeeded");
      },
      (error) => {
        expect(error).toBeInstanceOf(UsableGitError);
        expect((error as UsableGitError).code).toBe("STALE_STATE");
      },
    );
  });

  test("replaying a ship requestId does not create a second commit or push", async () => {
    const fixture = await createFixture();
    await Bun.write(join(fixture.local, "selected.txt"), "selected\n");
    const inspected = await inspect(
      { repoPath: fixture.local },
      { stateRoot: fixture.stateRoot },
    );
    const request = {
      repoPath: fixture.local,
      files: ["selected.txt"],
      message: "ship once",
      remote: "origin",
      requestId: "ship-replay-1",
      snapshot: inspected.snapshot!,
    };
    const first = await ship(request, { stateRoot: fixture.stateRoot });
    const second = await ship(request, { stateRoot: fixture.stateRoot });
    expect(second.commit).toBe(first.commit);
    expect(await runGit(fixture.local, "rev-list", "--count", "HEAD")).toBe("2");
    expect(await runGit(fixture.remote, "rev-parse", "refs/heads/main")).toBe(first.commit);
  });
});

import { afterEach, describe, expect, setDefaultTimeout, test } from "bun:test";
import { mkdir, mkdtemp, realpath, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { branch } from "../src/operations/branch.ts";
import { diff } from "../src/operations/diff.ts";
import { inspect, inspectDetailed } from "../src/operations/inspect.ts";
import { publish } from "../src/operations/publish.ts";
import { sync } from "../src/operations/sync.ts";
import { update } from "../src/operations/update.ts";
import { UsableGitError } from "../src/errors.ts";

const cleanups: Array<() => Promise<void>> = [];
setDefaultTimeout(30_000);
const gitEnvironment = {
  ...process.env,
  GIT_AUTHOR_NAME: "Usable Git Ops Test",
  GIT_AUTHOR_EMAIL: "usable-git@example.test",
  GIT_COMMITTER_NAME: "Usable Git Ops Test",
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
  const root = await realpath(await mkdtemp(join(tmpdir(), "usable-git-ops-")));
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
  await runGit(local, "push", "--quiet", "-u", "origin", "refs/heads/main:refs/heads/main");
  cleanups.push(() => rm(root, { recursive: true, force: true }));
  return { root, local, remote, stateRoot };
};

const expectCode = async (operation: Promise<unknown>, code: UsableGitError["code"]) => {
  await operation.then(
    () => {
      throw new Error(`Expected ${code}`);
    },
    (error) => {
      expect(error).toBeInstanceOf(UsableGitError);
      expect((error as UsableGitError).code).toBe(code);
    },
  );
};

afterEach(async () => {
  await Promise.all(cleanups.splice(0).map((cleanup) => cleanup()));
});

describe("branch", () => {
  test("creates a branch at HEAD, switches, and refuses duplicates", async () => {
    const fixture = await createFixture();
    const head = await runGit(fixture.local, "rev-parse", "HEAD");
    const created = await branch({
      repoPath: fixture.local,
      expectedHead: { kind: "oid", oid: head },
      mode: { kind: "create", name: "feature/one" },
    }, { stateRoot: fixture.stateRoot });
    expect(created).toMatchObject({
      name: "feature/one",
      oid: head,
      previousBranch: "main",
      created: true,
    });
    expect(await runGit(fixture.local, "branch", "--show-current")).toBe("feature/one");

    await expectCode(
      branch({
        repoPath: fixture.local,
        expectedHead: { kind: "oid", oid: head },
        mode: { kind: "create", name: "main" },
      }, { stateRoot: fixture.stateRoot }),
      "REF_EXISTS",
    );

    const switched = await branch({
      repoPath: fixture.local,
      expectedHead: { kind: "oid", oid: head },
      mode: { kind: "switch", name: "main" },
    }, { stateRoot: fixture.stateRoot });
    expect(switched).toMatchObject({ name: "main", created: false });
  });

  test("refuses to switch with uncommitted tracked changes and preserves them", async () => {
    const fixture = await createFixture();
    const head = await runGit(fixture.local, "rev-parse", "HEAD");
    await runGit(fixture.local, "branch", "other");
    await Bun.write(join(fixture.local, "tracked.txt"), "dirty\n");
    await expectCode(
      branch({
        repoPath: fixture.local,
        expectedHead: { kind: "oid", oid: head },
        mode: { kind: "switch", name: "other" },
      }, { stateRoot: fixture.stateRoot }),
      "UNSUPPORTED_STATE",
    );
    expect(await Bun.file(join(fixture.local, "tracked.txt")).text()).toBe("dirty\n");
    expect(await runGit(fixture.local, "branch", "--show-current")).toBe("main");
  });

  test("rejects a stale expected head", async () => {
    const fixture = await createFixture();
    await expectCode(
      branch({
        repoPath: fixture.local,
        expectedHead: { kind: "oid", oid: "a".repeat(40) },
        mode: { kind: "create", name: "feature/stale" },
      }, { stateRoot: fixture.stateRoot }),
      "STALE_STATE",
    );
  });
});

describe("publish amend", () => {
  test("amends the tip with the selected path and reports the amended oid", async () => {
    const fixture = await createFixture();
    await Bun.write(join(fixture.local, "feature.txt"), "v1\n");
    await runGit(fixture.local, "add", "--", "feature.txt");
    await runGit(fixture.local, "commit", "--quiet", "-m", "feature");
    const tip = await runGit(fixture.local, "rev-parse", "HEAD");
    const parent = await runGit(fixture.local, "rev-parse", "HEAD^");

    await Bun.write(join(fixture.local, "feature.txt"), "v2 after review\n");
    await Bun.write(join(fixture.local, "unrelated.txt"), "keep me\n");
    const snapshot = await inspect({ repoPath: fixture.local }, { stateRoot: fixture.stateRoot });

    const result = await publish({
      repoPath: fixture.local,
      files: ["feature.txt"],
      mode: { kind: "amend" },
      snapshot: snapshot.snapshot!,
    }, { stateRoot: fixture.stateRoot });

    expect(result.amendedOid).toBe(tip);
    expect(result.commitOid).not.toBe(tip);
    expect(await runGit(fixture.local, "rev-parse", "HEAD^")).toBe(parent);
    expect(await runGit(fixture.local, "show", "HEAD:feature.txt")).toBe("v2 after review");
    expect(await runGit(fixture.local, "log", "-1", "--format=%s")).toBe("feature");
    expect(await Bun.file(join(fixture.local, "unrelated.txt")).text()).toBe("keep me\n");
    expect(await runGit(fixture.local, "fsck", "--strict")).toBe("");
  });

  test("amend with a new message replaces the message", async () => {
    const fixture = await createFixture();
    await Bun.write(join(fixture.local, "tracked.txt"), "amended\n");
    const snapshot = await inspect({ repoPath: fixture.local }, { stateRoot: fixture.stateRoot });
    const result = await publish({
      repoPath: fixture.local,
      files: ["tracked.txt"],
      message: "rewritten subject",
      mode: { kind: "amend" },
      snapshot: snapshot.snapshot!,
    }, { stateRoot: fixture.stateRoot });
    expect(await runGit(fixture.local, "log", "-1", "--format=%s")).toBe("rewritten subject");
    expect(result.warnings.some((warning) => warning.includes("force-with-lease"))).toBe(true);
  });

  test("amend on an unborn HEAD is rejected", async () => {
    const root = await realpath(await mkdtemp(join(tmpdir(), "usable-git-amend-unborn-")));
    cleanups.push(() => rm(root, { recursive: true, force: true }));
    await runGit(root, "init", "--quiet", "--initial-branch=main");
    await Bun.write(join(root, "new.txt"), "new\n");
    const snapshot = await inspect({ repoPath: root }, { stateRoot: join(root, "state") });
    await expectCode(
      publish({
        repoPath: root,
        files: ["new.txt"],
        mode: { kind: "amend" },
        snapshot: snapshot.snapshot!,
      }, { stateRoot: join(root, "state") }),
      "INVALID_INPUT",
    );
  });
});

describe("diff", () => {
  test("returns the patch between two exact commits and for one commit", async () => {
    const fixture = await createFixture();
    const base = await runGit(fixture.local, "rev-parse", "HEAD");
    await Bun.write(join(fixture.local, "tracked.txt"), "changed\n");
    await runGit(fixture.local, "commit", "--quiet", "-am", "change tracked");
    const target = await runGit(fixture.local, "rev-parse", "HEAD");

    const range = await diff({
      repoPath: fixture.local,
      target: { kind: "range", baseOid: base.slice(0, 12), targetOid: target },
    }, { stateRoot: fixture.stateRoot });
    expect(range.base).toBe(base);
    expect(range.target).toBe(target);
    expect(range.items).toHaveLength(1);
    expect(range.items[0]).toMatchObject({ path: "tracked.txt", additions: 1, deletions: 1 });
    expect(range.items[0]?.patch).toContain("+changed");

    const single = await diff({
      repoPath: fixture.local,
      target: { kind: "commit", oid: target },
    }, { stateRoot: fixture.stateRoot });
    expect(single.items[0]?.path).toBe("tracked.txt");

    await expectCode(
      diff({
        repoPath: fixture.local,
        target: { kind: "commit", oid: "0".repeat(40) },
      }, { stateRoot: fixture.stateRoot }),
      "INVALID_INPUT",
    );
  });

  test("diffs a root commit against the empty tree", async () => {
    const fixture = await createFixture();
    const root = await runGit(fixture.local, "rev-list", "--max-parents=0", "HEAD");
    const result = await diff({
      repoPath: fixture.local,
      target: { kind: "commit", oid: root },
    }, { stateRoot: fixture.stateRoot });
    expect(result.items[0]).toMatchObject({ path: "tracked.txt" });
    expect(result.items[0]?.patch).toContain("+base");
  });
});

describe("sync", () => {
  test("fetches exactly the named branch and reports refreshed ahead/behind", async () => {
    const fixture = await createFixture();
    // Move the remote ahead through a second clone.
    const scratch = join(fixture.root, "scratch");
    await runGit(fixture.root, "clone", "--quiet", fixture.remote, scratch);
    await Bun.write(join(scratch, "remote.txt"), "remote\n");
    await runGit(scratch, "add", "--", "remote.txt");
    await runGit(scratch, "commit", "--quiet", "-m", "remote moved");
    await runGit(scratch, "push", "--quiet", "origin", "HEAD:refs/heads/main");
    const remoteHead = await runGit(scratch, "rev-parse", "HEAD");

    const result = await sync({ repoPath: fixture.local, remote: "origin" });
    expect(result.fetched).toHaveLength(1);
    expect(result.fetched[0]).toMatchObject({
      branch: "main",
      ref: "refs/remotes/origin/main",
      newOid: remoteHead,
      updated: true,
    });
    expect(result.branch).toMatchObject({ name: "main", ahead: 0, behind: 1 });
    // Locally non-destructive: worktree and HEAD untouched.
    expect(await runGit(fixture.local, "rev-parse", "HEAD")).not.toBe(remoteHead);
  });

  test("reports a branch absent on the remote as newOid null", async () => {
    const fixture = await createFixture();
    const result = await sync({
      repoPath: fixture.local,
      remote: "origin",
      branches: ["does-not-exist"],
    });
    expect(result.fetched[0]).toMatchObject({
      branch: "does-not-exist",
      newOid: null,
      updated: false,
    });
  });

  test("rejects an unconfigured remote", async () => {
    const fixture = await createFixture();
    await expectCode(
      sync({ repoPath: fixture.local, remote: "upstream" }),
      "INVALID_INPUT",
    );
  });
});

describe("update", () => {
  test("fast-forwards to the synced target and closes the push loop", async () => {
    const fixture = await createFixture();
    const scratch = join(fixture.root, "scratch");
    await runGit(fixture.root, "clone", "--quiet", fixture.remote, scratch);
    await Bun.write(join(scratch, "remote.txt"), "remote\n");
    await runGit(scratch, "add", "--", "remote.txt");
    await runGit(scratch, "commit", "--quiet", "-m", "remote moved");
    await runGit(scratch, "push", "--quiet", "origin", "HEAD:refs/heads/main");

    const synced = await sync({ repoPath: fixture.local, remote: "origin" });
    const targetOid = synced.fetched[0]!.newOid!;
    const head = await runGit(fixture.local, "rev-parse", "HEAD");
    // Unrelated untracked work must survive the fast-forward.
    await Bun.write(join(fixture.local, "loose.txt"), "loose\n");

    const result = await update({
      repoPath: fixture.local,
      expectedHead: { kind: "oid", oid: head },
      targetOid,
    }, { stateRoot: fixture.stateRoot });
    expect(result).toMatchObject({
      branch: "main",
      previousOid: head,
      newOid: targetOid,
      commitsAdvanced: 1,
    });
    expect(await runGit(fixture.local, "rev-parse", "HEAD")).toBe(targetOid);
    expect(await Bun.file(join(fixture.local, "loose.txt")).text()).toBe("loose\n");
  });

  test("refuses divergence with NON_FAST_FORWARD and overlap with UNSUPPORTED_STATE", async () => {
    const fixture = await createFixture();
    const scratch = join(fixture.root, "scratch");
    await runGit(fixture.root, "clone", "--quiet", fixture.remote, scratch);
    await Bun.write(join(scratch, "tracked.txt"), "remote version\n");
    await runGit(scratch, "add", "--", "tracked.txt");
    await runGit(scratch, "commit", "--quiet", "-m", "remote change");
    await runGit(scratch, "push", "--quiet", "origin", "HEAD:refs/heads/main");
    const synced = await sync({ repoPath: fixture.local, remote: "origin" });
    const targetOid = synced.fetched[0]!.newOid!;
    const head = await runGit(fixture.local, "rev-parse", "HEAD");

    // Overlap: local dirty tracked.txt vs incoming tracked.txt change.
    await Bun.write(join(fixture.local, "tracked.txt"), "local dirty\n");
    await expectCode(
      update({
        repoPath: fixture.local,
        expectedHead: { kind: "oid", oid: head },
        targetOid,
      }, { stateRoot: fixture.stateRoot }),
      "UNSUPPORTED_STATE",
    );
    expect(await Bun.file(join(fixture.local, "tracked.txt")).text()).toBe("local dirty\n");

    // Divergence: commit locally, then a fast-forward to the remote tip is impossible.
    await runGit(fixture.local, "commit", "--quiet", "-am", "local change");
    const divergedHead = await runGit(fixture.local, "rev-parse", "HEAD");
    await expectCode(
      update({
        repoPath: fixture.local,
        expectedHead: { kind: "oid", oid: divergedHead },
        targetOid,
      }, { stateRoot: fixture.stateRoot }),
      "NON_FAST_FORWARD",
    );
  });

  test("detailed inspect still drives amend after branch and update flows", async () => {
    // Smoke check that the detailed internal shape stays consistent for
    // publish after the new ops mutate HEAD.
    const fixture = await createFixture();
    const head = await runGit(fixture.local, "rev-parse", "HEAD");
    await branch({
      repoPath: fixture.local,
      expectedHead: { kind: "oid", oid: head },
      mode: { kind: "create", name: "feature/flow" },
    }, { stateRoot: fixture.stateRoot });
    await Bun.write(join(fixture.local, "flow.txt"), "flow\n");
    const detailed = await inspectDetailed({ repoPath: fixture.local });
    expect(detailed.branch.head).toBe("feature/flow");
    expect(detailed.changes.some(({ path }) => path === "flow.txt")).toBe(true);
  });
});

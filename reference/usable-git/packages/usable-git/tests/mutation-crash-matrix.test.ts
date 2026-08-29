import { afterEach, describe, expect, setDefaultTimeout, test } from "bun:test";
import { mkdtemp, mkdir, readFile, realpath, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { inspectDetailed } from "@usable-git/operations/inspect.ts";
import {
  publish,
  PublishOperationError,
  type PublishRequest,
} from "@usable-git/operations/publish.ts";
import { push } from "@usable-git/operations/push.ts";
import {
  commitFile,
  createRepository,
  type TestRepository,
  writeFile,
} from "./helpers/repository.ts";

setDefaultTimeout(30_000);

const cleanups: Array<() => Promise<void>> = [];
const gitEnvironment = {
  ...process.env,
  GIT_AUTHOR_NAME: "Usable Git Crash Test",
  GIT_AUTHOR_EMAIL: "usable-git@example.test",
  GIT_COMMITTER_NAME: "Usable Git Crash Test",
  GIT_COMMITTER_EMAIL: "usable-git@example.test",
};

class InjectedCrash extends Error {
  constructor(readonly phase: string) {
    super(`injected crash at ${phase}`);
  }
}

const crashAt = (target: string) => {
  let injected = false;
  const observed: string[] = [];
  return {
    observed,
    probe: async (phase: string) => {
      observed.push(phase);
      if (!injected && phase === target) {
        injected = true;
        throw new InjectedCrash(phase);
      }
    },
  };
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
  return stdout;
};

const publishFixture = async () => {
  const repo = await createRepository();
  cleanups.push(repo.cleanup);
  await repo.run("config", "user.name", "Usable Git Crash Test");
  await repo.run("config", "user.email", "usable-git@example.test");
  await commitFile(repo, "base.txt", "base\n", "base");
  await commitFile(repo, "staged.txt", "staged base\n", "staged base");
  await commitFile(repo, "unstaged.txt", "unstaged base\n", "unstaged base");
  await writeFile(repo, "selected.txt", "selected\n");
  await writeFile(repo, "staged.txt", "staged pending\n");
  await repo.run("add", "--", "staged.txt");
  await writeFile(repo, "unstaged.txt", "unstaged pending\n");
  await writeFile(repo, "loose.txt", "loose pending\n");
  const snapshot = await inspectDetailed({ repoPath: repo.path });
  const selected = snapshot.changes.find(({ path }) => path === "selected.txt");
  if (!selected) throw new Error("selected file was not inspected");
  const request: PublishRequest = {
    repoPath: repo.path,
    files: ["selected.txt"],
    message: "crash-matrix publish",
    requestId: `publish-${crypto.randomUUID()}`,
    expected: {
      head: { kind: "oid", oid: snapshot.branch.oid! },
      fingerprints: { "selected.txt": selected.fingerprint },
    },
  };
  const stateRoot = await realpath(await mkdtemp(join(tmpdir(), "usable-git-state-")));
  cleanups.push(() => rm(stateRoot, { recursive: true, force: true }));
  return { repo, request, stateRoot };
};

const snapshotLocalState = async (repo: TestRepository) => ({
  head: await repo.run("rev-parse", "HEAD"),
  tree: await repo.run("ls-tree", "-r", "HEAD"),
  index: Buffer.from(await readFile(join(repo.path, ".git", "index"))).toString("base64"),
  status: await repo.run("status", "--porcelain=v2", "-z"),
  staged: await repo.run("show", ":staged.txt"),
  unstaged: await readFile(join(repo.path, "unstaged.txt"), "utf8"),
  loose: await readFile(join(repo.path, "loose.txt"), "utf8"),
});

const expectUnrelatedState = async (repo: TestRepository) => {
  expect(await repo.run("show", ":staged.txt")).toBe("staged pending\n");
  expect(await readFile(join(repo.path, "unstaged.txt"), "utf8")).toBe(
    "unstaged pending\n",
  );
  expect(await readFile(join(repo.path, "loose.txt"), "utf8")).toBe("loose pending\n");
  expect(await repo.run("diff", "--cached", "--name-only")).toBe("staged.txt\n");
  await repo.run("fsck", "--strict");
};

const pushFixture = async () => {
  const root = await realpath(await mkdtemp(join(tmpdir(), "usable-git-push-crash-")));
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
  await runGit(remote, "branch", "unrelated", "refs/heads/main");
  const oldTarget = (await runGit(remote, "rev-parse", "refs/heads/main")).trim();
  const unrelated = (await runGit(remote, "rev-parse", "refs/heads/unrelated")).trim();
  await Bun.write(join(local, "tracked.txt"), "next\n");
  await runGit(local, "add", "--", "tracked.txt");
  await runGit(local, "commit", "--quiet", "-m", "next");
  await Bun.write(join(local, "unrelated.txt"), "local pending\n");
  const sourceOid = (await runGit(local, "rev-parse", "HEAD")).trim();
  cleanups.push(() => rm(root, { recursive: true, force: true }));
  return {
    root,
    local,
    remote,
    stateRoot,
    oldTarget,
    unrelated,
    sourceOid,
    request: {
      repoPath: local,
      remote: "origin",
      sourceRef: "refs/heads/main",
      targetRef: "refs/heads/main",
      requestId: `push-${crypto.randomUUID()}`,
      expectedSourceOid: sourceOid,
      mode: { kind: "fast-forward" as const },
    },
  };
};

afterEach(async () => {
  await Promise.all(cleanups.splice(0).map((cleanup) => cleanup()));
});

describe("publish durable crash matrix", () => {
  const phases = [
    "journal:started",
    "recovery:snapshotted",
    "journal:index_staged",
    "recovery:commit_started",
    "journal:commit_observed",
    "journal:terminal",
  ] as const;

  for (const phase of phases) {
    test(`classifies ${phase} and preserves unrelated state`, async () => {
      const { repo, request, stateRoot } = await publishFixture();
      const before = await snapshotLocalState(repo);
      const injection = crashAt(phase);

      await expect(
        publish(request, { stateRoot, mutationProbe: injection.probe }),
      ).rejects.toBeInstanceOf(InjectedCrash);
      expect(injection.observed).toContain(phase);

      if (
        phase === "recovery:snapshotted" ||
        phase === "journal:index_staged" ||
        phase === "recovery:commit_started"
      ) {
        await expect(publish(request, { stateRoot })).rejects.toMatchObject({
          code: "GIT_FAILED",
        } satisfies Partial<PublishOperationError>);
        expect(await snapshotLocalState(repo)).toEqual(before);
      } else {
        const result = await publish(request, { stateRoot });
        expect(result.commitOid).toBe((await repo.run("rev-parse", "HEAD")).trim());
        expect(await repo.run("show", "HEAD:selected.txt")).toBe("selected\n");
        expect(await repo.run("diff-tree", "--name-only", "--no-commit-id", "-r", "HEAD")).toBe(
          "selected.txt\n",
        );
      }

      await expectUnrelatedState(repo);
      expect(
        await Array.fromAsync(
          new Bun.Glob("publish-recovery/**/*.json").scan({ cwd: stateRoot }),
        ),
      ).toEqual([]);
    });
  }
});

describe("push durable crash matrix", () => {
  const phases = [
    "journal:started",
    "journal:push_started",
    "remote:returned",
    "journal:terminal",
  ] as const;

  for (const phase of phases) {
    test(`classifies ${phase} without changing unrelated refs or local state`, async () => {
      const fixture = await pushFixture();
      const injection = crashAt(phase);

      await expect(
        push(fixture.request, {
          stateRoot: fixture.stateRoot,
          mutationProbe: injection.probe,
        }),
      ).rejects.toBeInstanceOf(InjectedCrash);
      expect(injection.observed).toContain(phase);

      if (phase === "journal:push_started") {
        await expect(
          push(fixture.request, { stateRoot: fixture.stateRoot }),
        ).rejects.toMatchObject({ code: "NETWORK_AMBIGUITY" });
        expect((await runGit(fixture.remote, "rev-parse", "refs/heads/main")).trim()).toBe(
          fixture.oldTarget,
        );
      } else {
        const recovered = await push(fixture.request, { stateRoot: fixture.stateRoot });
        expect(recovered.newTargetOid).toBe(fixture.sourceOid);
        expect((await runGit(fixture.remote, "rev-parse", "refs/heads/main")).trim()).toBe(
          fixture.sourceOid,
        );
      }

      expect(
        (await runGit(fixture.remote, "rev-parse", "refs/heads/unrelated")).trim(),
      ).toBe(fixture.unrelated);
      expect(await readFile(join(fixture.local, "unrelated.txt"), "utf8")).toBe(
        "local pending\n",
      );
      expect(await runGit(fixture.local, "status", "--porcelain=v1")).toBe(
        "?? unrelated.txt\n",
      );
      expect(await runGit(fixture.remote, "fsck", "--strict")).toBe("");
    });
  }
});

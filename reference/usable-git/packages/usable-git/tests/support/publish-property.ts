import { deepStrictEqual, strictEqual } from "node:assert/strict";
import { mkdtemp, readFile, realpath, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { inspectDetailed, type DetailedInspectResult } from "../../src/operations/inspect.ts";
import { publish, type PublishRequest } from "../../src/operations/publish.ts";
import {
  createRepository,
  type TestRepository,
  writeFile,
} from "../helpers/repository.ts";

type SelectedMutation = {
  kind: "modified" | "deleted" | "new";
  path: string;
  stagedBeforePublish: boolean;
  contents: string | null;
};

export type PublishPropertyCase = {
  seed: number;
  caseIndex: number;
  token: string;
  selected: SelectedMutation[];
};

export type PublishPropertySummary = {
  seed: number;
  cases: number;
  failures: 0;
  oracleChecks: number;
  checksPerCase: number;
};

type FileState = { exists: false } | { exists: true; bytesBase64: string };

const unrelatedPaths = [
  "unrelated-staged.txt",
  "unrelated-unstaged.txt",
  "unrelated-mixed.txt",
  "unrelated-untracked.txt",
  "unrelated-staged-deletion.txt",
  "unrelated-unstaged-deletion.txt",
];

const trackedPaths = [
  "selected modified.txt",
  "selected-deleted.txt",
  "sentinel.txt",
  ...unrelatedPaths.filter((path) => path !== "unrelated-untracked.txt"),
];

const createRandom = (seed: number, caseIndex: number) => {
  let state =
    (seed ^ Math.imul(caseIndex + 1, 0x9e3779b9) ^ 0xa5a5a5a5) >>> 0;
  if (state === 0) state = 0x6d2b79f5;
  return () => {
    state += 0x6d2b79f5;
    let value = state;
    value = Math.imul(value ^ (value >>> 15), value | 1);
    value ^= value + Math.imul(value ^ (value >>> 7), value | 61);
    return (value ^ (value >>> 14)) >>> 0;
  };
};

const shuffled = <Value>(values: Value[], random: () => number) => {
  const result = [...values];
  for (let index = result.length - 1; index > 0; index -= 1) {
    const target = random() % (index + 1);
    [result[index], result[target]] = [result[target]!, result[index]!];
  }
  return result;
};

export const generatePublishPropertyCase = (
  seed: number,
  caseIndex: number,
): PublishPropertyCase => {
  const random = createRandom(seed, caseIndex);
  const token = `${caseIndex.toString(36)}-${random().toString(16).padStart(8, "0")}`;
  const candidates: SelectedMutation[] = [
    {
      kind: "modified",
      path: "selected modified.txt",
      stagedBeforePublish: random() % 2 === 0,
      contents: `selected modified ${token} ${random()}\n`,
    },
    {
      kind: "deleted",
      path: "selected-deleted.txt",
      stagedBeforePublish: random() % 2 === 0,
      contents: null,
    },
    {
      kind: "new",
      path: `selected-new-${caseIndex.toString(36)}.txt`,
      stagedBeforePublish: random() % 2 === 0,
      contents: `selected new ${token} ${random()}\n`,
    },
  ];
  const selectedCount = 1 + (random() % candidates.length);

  return {
    seed,
    caseIndex,
    token,
    selected: shuffled(candidates, random).slice(0, selectedCount),
  };
};

const fileState = async (path: string): Promise<FileState> => {
  try {
    return {
      exists: true,
      bytesBase64: Buffer.from(await readFile(path)).toString("base64"),
    };
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") return { exists: false };
    throw error;
  }
};

const worktreeStates = async (repository: TestRepository, paths: string[]) =>
  Object.fromEntries(
    await Promise.all(
      paths.map(async (path) => [path, await fileState(join(repository.path, path))]),
    ),
  );

const fingerprintMap = (snapshot: DetailedInspectResult, excluded: Set<string>) =>
  Object.fromEntries(
    snapshot.changes
      .filter(({ path }) => !excluded.has(path))
      .map(({ path, fingerprint }): [string, string] => [path, fingerprint])
      .sort(([left], [right]) => left.localeCompare(right)),
  );

const treeMap = async (repository: TestRepository, revision: string) => {
  const output = await repository.run("ls-tree", "-rz", "--full-tree", revision);
  return Object.fromEntries(
    output
      .split("\0")
      .filter(Boolean)
      .map((entry) => {
        const separator = entry.indexOf("\t");
        const metadata = entry.slice(0, separator);
        const path = entry.slice(separator + 1);
        return [path, metadata] as [string, string];
      })
      .sort(([left], [right]) => left.localeCompare(right)),
  );
};

const withoutSelected = (
  tree: Record<string, string>,
  selected: Set<string>,
) =>
  Object.fromEntries(
    Object.entries(tree).filter(([path]) => !selected.has(path)),
  );

const indexEntryBytes = (bytes: Buffer, paths: Set<string>) => {
  strictEqual(bytes.subarray(0, 4).toString("ascii"), "DIRC");
  strictEqual(bytes.readUInt32BE(4), 2);
  const entries = new Map<string, string>();
  const count = bytes.readUInt32BE(8);
  let offset = 12;
  for (let index = 0; index < count; index += 1) {
    const start = offset;
    const pathLength = bytes.readUInt16BE(start + 60) & 0x0fff;
    const pathStart = start + 62;
    const pathEnd =
      pathLength < 0x0fff ? pathStart + pathLength : bytes.indexOf(0, pathStart);
    if (pathEnd < pathStart || bytes[pathEnd] !== 0) {
      throw new Error(`Unreadable Git index entry ${index}`);
    }
    const path = bytes.subarray(pathStart, pathEnd).toString("utf8");
    offset = start + Math.ceil((pathEnd + 1 - start) / 8) * 8;
    if (paths.has(path)) {
      entries.set(path, bytes.subarray(start, offset).toString("base64"));
    }
  }
  return Object.fromEntries([...entries].sort(([left], [right]) => left.localeCompare(right)));
};

const selectedState = async (
  repository: TestRepository,
  mutation: SelectedMutation,
) => {
  if (mutation.kind === "deleted") {
    const process = Bun.spawn(
      ["git", "cat-file", "-e", `HEAD:${mutation.path}`],
      { cwd: repository.path, stdout: "pipe", stderr: "pipe" },
    );
    const exitCode = await process.exited;
    return { exists: exitCode === 0, contents: null };
  }
  return {
    exists: true,
    contents: await repository.run("show", `HEAD:${mutation.path}`),
  };
};

const makeDirty = async (
  repository: TestRepository,
  descriptor: PublishPropertyCase,
) => {
  const suffix = `${descriptor.token} ${descriptor.seed}`;
  await writeFile(repository, "unrelated-staged.txt", `staged ${suffix}\n`);
  await repository.run("add", "--", "unrelated-staged.txt");
  await writeFile(repository, "unrelated-unstaged.txt", `unstaged ${suffix}\n`);
  await writeFile(repository, "unrelated-mixed.txt", `mixed staged ${suffix}\n`);
  await repository.run("add", "--", "unrelated-mixed.txt");
  await writeFile(repository, "unrelated-mixed.txt", `mixed worktree ${suffix}\n`);
  await writeFile(repository, "unrelated-untracked.txt", `untracked ${suffix}\n`);
  await Bun.file(join(repository.path, "unrelated-staged-deletion.txt")).delete();
  await repository.run("add", "-u", "--", "unrelated-staged-deletion.txt");
  await Bun.file(join(repository.path, "unrelated-unstaged-deletion.txt")).delete();

  for (const mutation of descriptor.selected) {
    if (mutation.kind === "deleted") {
      await Bun.file(join(repository.path, mutation.path)).delete();
      if (mutation.stagedBeforePublish) {
        await repository.run("add", "-u", "--", mutation.path);
      }
      continue;
    }

    if (mutation.stagedBeforePublish) {
      await writeFile(repository, mutation.path, `older staged ${suffix}\n`);
      await repository.run("add", "--", mutation.path);
    }
    await writeFile(repository, mutation.path, mutation.contents!);
  }
};

const requestFor = async (
  repository: TestRepository,
  descriptor: PublishPropertyCase,
): Promise<PublishRequest> => {
  const files = descriptor.selected.map(({ path }) => path);
  const snapshot = await inspectDetailed({ repoPath: repository.path, files });
  const expectedFingerprints = Object.fromEntries(
    files.map((path) => {
      const change = snapshot.changes.find((candidate) => candidate.path === path);
      if (!change) throw new Error(`Missing selected change for ${JSON.stringify(path)}`);
      return [path, change.fingerprint];
    }),
  );
  return {
    repoPath: repository.path,
    files,
    message: `property publish ${descriptor.seed}:${descriptor.caseIndex}`,
    requestId: `property-${descriptor.seed}-${descriptor.caseIndex}`,
    expected: {
      head: snapshot.branch.oid
        ? { kind: "oid", oid: snapshot.branch.oid }
        : { kind: "unborn" },
      fingerprints: expectedFingerprints,
    },
  };
};

const runPublishPropertyCase = async (descriptor: PublishPropertyCase) => {
  const repository = await createRepository();
  const stateRoot = await realpath(
    await mkdtemp(join(tmpdir(), "usable-git-property-state-")),
  );
  let checks = 0;
  const check = (actual: unknown, expected: unknown) => {
    deepStrictEqual(actual, expected);
    checks += 1;
  };

  try {
    await repository.run("config", "user.name", "Usable Git Property Test");
    await repository.run("config", "user.email", "property@example.test");
    await repository.run("config", "commit.gpgSign", "false");
    await repository.run("config", "index.version", "2");
    for (const path of trackedPaths) {
      await writeFile(repository, path, `base ${path} ${descriptor.token}\n`);
    }
    await repository.run("add", "--", ...trackedPaths);
    await repository.run("commit", "--quiet", "-m", "property base");
    await makeDirty(repository, descriptor);

    const selected = new Set(descriptor.selected.map(({ path }) => path));
    const before = await inspectDetailed({ repoPath: repository.path });
    const beforeHead = (await repository.run("rev-parse", "HEAD")).trim();
    const beforeCount = Number((await repository.run("rev-list", "--count", "HEAD")).trim());
    const beforeTree = await treeMap(repository, beforeHead);
    const beforeIndex = await repository.run(
      "ls-files",
      "--stage",
      "-z",
      "--",
      ...unrelatedPaths,
    );
    const beforeIndexEntries = indexEntryBytes(
      await readFile(join(repository.path, ".git", "index")),
      new Set(unrelatedPaths),
    );
    const beforeWorktree = await worktreeStates(repository, unrelatedPaths);
    const beforeFingerprints = fingerprintMap(before, selected);
    const request = await requestFor(repository, descriptor);

    const result = await publish(request, { stateRoot });

    const after = await inspectDetailed({ repoPath: repository.path });
    const afterHead = (await repository.run("rev-parse", "HEAD")).trim();
    const afterTree = await treeMap(repository, afterHead);
    const afterIndex = await repository.run(
      "ls-files",
      "--stage",
      "-z",
      "--",
      ...unrelatedPaths,
    );
    const afterIndexEntries = indexEntryBytes(
      await readFile(join(repository.path, ".git", "index")),
      new Set(unrelatedPaths),
    );
    const afterWorktree = await worktreeStates(repository, unrelatedPaths);
    const changedPaths = (await repository.run(
      "diff-tree",
      "--root",
      "--no-commit-id",
      "--name-only",
      "-r",
      "-z",
      afterHead,
    ))
      .split("\0")
      .filter(Boolean)
      .sort();

    check(result.committedPaths, request.files);
    check(result.warnings, []);
    check(
      (await repository.run("rev-list", "--parents", "-n", "1", afterHead)).trim(),
      `${afterHead} ${beforeHead}`,
    );
    check(
      Number((await repository.run("rev-list", "--count", "HEAD")).trim()),
      beforeCount + 1,
    );
    check(
      (await repository.run("show", "-s", "--format=%s", afterHead)).trim(),
      request.message,
    );
    check(changedPaths, [...selected].sort());
    check(withoutSelected(afterTree, selected), withoutSelected(beforeTree, selected));
    check(
      await Promise.all(
        descriptor.selected.map((mutation) => selectedState(repository, mutation)),
      ),
      descriptor.selected.map((mutation) =>
        mutation.kind === "deleted"
          ? { exists: false, contents: null }
          : { exists: true, contents: mutation.contents },
      ),
    );
    check(afterIndex, beforeIndex);
    check(afterIndexEntries, beforeIndexEntries);
    check(afterWorktree, beforeWorktree);
    check(fingerprintMap(after, selected), beforeFingerprints);
    check(result.status, {
      staged: after.staged,
      unstaged: after.unstaged,
      untracked: after.untracked,
      conflicted: after.conflicted,
    });
    check(await repository.run("fsck", "--strict", "--no-dangling"), "");
    check((await repository.run("cat-file", "-t", afterHead)).trim(), "commit");
    strictEqual(result.commitOid, afterHead);
    checks += 1;

    return checks;
  } catch (error) {
    throw new Error(
      `Publish property failure seed=${descriptor.seed} case=${descriptor.caseIndex}: ${error instanceof Error ? error.message : String(error)}; descriptor=${JSON.stringify(descriptor)}`,
      { cause: error },
    );
  } finally {
    await Promise.all([
      repository.cleanup(),
      rm(stateRoot, { recursive: true, force: true }),
    ]);
  }
};

export const runPublishPropertyMatrix = async ({
  seed,
  caseCount,
}: {
  seed: number;
  caseCount: number;
}): Promise<PublishPropertySummary> => {
  if (!Number.isSafeInteger(seed) || seed < 1) {
    throw new Error("seed must be a positive safe integer");
  }
  if (!Number.isSafeInteger(caseCount) || caseCount < 1) {
    throw new Error("caseCount must be a positive safe integer");
  }

  let checksPerCase = 0;
  for (let caseIndex = 0; caseIndex < caseCount; caseIndex += 1) {
    const checks = await runPublishPropertyCase(
      generatePublishPropertyCase(seed, caseIndex),
    );
    if (checksPerCase === 0) checksPerCase = checks;
    strictEqual(checks, checksPerCase);
  }

  return {
    seed,
    cases: caseCount,
    failures: 0,
    oracleChecks: caseCount * checksPerCase,
    checksPerCase,
  };
};

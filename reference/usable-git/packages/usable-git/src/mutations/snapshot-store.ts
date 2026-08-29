import { createHash, randomUUID } from "node:crypto";
import { mkdir, open, readdir, readFile, rename, rm } from "node:fs/promises";
import { homedir } from "node:os";
import { dirname, join } from "node:path";

export type SnapshotRecord = {
  schemaVersion: 1;
  root: string;
  head: string | null;
  branch: string | null;
  createdAt: string;
  fingerprints: Record<string, string>;
};

type SnapshotStoreOptions = {
  stateRoot?: string;
  retentionMaxAgeMs?: number;
  retentionMaxCount?: number;
  now?: () => Date;
};

const defaultStateRoot = () =>
  process.env.XDG_STATE_HOME
    ? join(process.env.XDG_STATE_HOME, "usable-git")
    : join(homedir(), ".local", "state", "usable-git");

const digest = (value: string) => createHash("sha256").update(value).digest("hex");

export const snapshotTokenPattern = /^[a-f0-9]{12}$/;

const sortedFingerprints = (fingerprints: Record<string, string>) =>
  Object.fromEntries(
    Object.entries(fingerprints).sort(([left], [right]) => left.localeCompare(right)),
  );

export const snapshotToken = (input: {
  root: string;
  head: string | null;
  fingerprints: Record<string, string>;
}) =>
  digest(
    JSON.stringify({
      root: input.root,
      head: input.head,
      fingerprints: sortedFingerprints(input.fingerprints),
    }),
  ).slice(0, 12);

const isFingerprint = (value: unknown) =>
  typeof value === "string" && /^[a-f0-9]{64}$/.test(value);

const isObjectId = (value: unknown) =>
  typeof value === "string" && /^(?:[a-f0-9]{40}|[a-f0-9]{64})$/.test(value);

const validateRecord = (value: unknown): SnapshotRecord | null => {
  if (!value || typeof value !== "object") return null;
  const record = value as Record<string, unknown>;
  if (
    record.schemaVersion !== 1 ||
    typeof record.root !== "string" ||
    !(record.head === null || isObjectId(record.head)) ||
    !(record.branch === null || typeof record.branch === "string") ||
    typeof record.createdAt !== "string" ||
    !record.fingerprints ||
    typeof record.fingerprints !== "object" ||
    !Object.entries(record.fingerprints).every(
      ([path, fingerprint]) => path.length > 0 && isFingerprint(fingerprint),
    )
  ) {
    return null;
  }
  return value as SnapshotRecord;
};

const writeDurably = async (path: string, record: SnapshotRecord) => {
  await mkdir(dirname(path), { recursive: true });
  const temporaryPath = `${path}.${process.pid}.${randomUUID()}.tmp`;
  const file = await open(temporaryPath, "wx", 0o600);
  try {
    await file.writeFile(`${JSON.stringify(record)}\n`, "utf8");
    await file.sync();
  } finally {
    await file.close();
  }
  try {
    await rename(temporaryPath, path);
  } finally {
    await rm(temporaryPath, { force: true });
  }
};

export const createSnapshotStore = (options: SnapshotStoreOptions = {}) => {
  const stateRoot = options.stateRoot ?? defaultStateRoot();
  const retentionMaxAgeMs = options.retentionMaxAgeMs ?? 24 * 60 * 60 * 1_000;
  const retentionMaxCount = options.retentionMaxCount ?? 200;
  const now = options.now ?? (() => new Date());
  // Snapshots describe worktree file state, so they are keyed by worktree
  // root — linked worktrees sharing a common dir must not share snapshots.
  const repositoryDirectory = (root: string) => join(stateRoot, "snapshots", digest(root));
  const pathFor = (root: string, token: string) =>
    join(repositoryDirectory(root), `${token}.json`);

  const record = async (input: {
    root: string;
    head: string | null;
    branch: string | null;
    fingerprints: Record<string, string>;
  }) => {
    const token = snapshotToken(input);
    await writeDurably(pathFor(input.root, token), {
      schemaVersion: 1,
      root: input.root,
      head: input.head,
      branch: input.branch,
      createdAt: now().toISOString(),
      fingerprints: sortedFingerprints(input.fingerprints),
    });
    await prune(input.root);
    return token;
  };

  const read = async (root: string, token: string): Promise<SnapshotRecord | null> => {
    if (!snapshotTokenPattern.test(token)) return null;
    let parsed: unknown;
    try {
      parsed = JSON.parse(await readFile(pathFor(root, token), "utf8"));
    } catch {
      return null;
    }
    const validated = validateRecord(parsed);
    return validated && validated.root === root ? validated : null;
  };

  const prune = async (root: string) => {
    const directory = repositoryDirectory(root);
    let entries;
    try {
      entries = await readdir(directory, { withFileTypes: true });
    } catch {
      return { deleted: 0 };
    }
    const records: Array<{ path: string; createdAt: number }> = [];
    for (const entry of entries) {
      if (!entry.isFile() || !entry.name.endsWith(".json")) continue;
      const path = join(directory, entry.name);
      try {
        const parsed = validateRecord(JSON.parse(await readFile(path, "utf8")));
        if (!parsed) continue;
        const createdAt = Date.parse(parsed.createdAt);
        if (!Number.isFinite(createdAt)) continue;
        records.push({ path, createdAt });
      } catch {
        // Unreadable snapshots are retained for diagnosis; publish treats them as unknown.
      }
    }
    records.sort((left, right) => right.createdAt - left.createdAt);
    const cutoff = now().getTime() - retentionMaxAgeMs;
    const deleted = records.filter(
      (entry, index) => entry.createdAt < cutoff || index >= retentionMaxCount,
    );
    await Promise.all(deleted.map(({ path }) => rm(path, { force: true })));
    return { deleted: deleted.length };
  };

  return { record, read, prune };
};

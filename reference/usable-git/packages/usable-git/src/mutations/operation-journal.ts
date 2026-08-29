import { createHash, randomUUID } from "node:crypto";
import {
  mkdir,
  link,
  open,
  readdir,
  readFile,
  rename,
  rm,
} from "node:fs/promises";
import { homedir } from "node:os";
import { dirname, join } from "node:path";

export type JournalOperation = "publish" | "push" | "branch" | "update";
export type JournalPhase =
  | "started"
  | "index_staged"
  | "commit_observed"
  | "push_started"
  | "ref_update_started"
  | "terminal";

export interface JournalRecord {
  schemaVersion: 1;
  requestId: string;
  operation: JournalOperation;
  repoKey: string;
  inputHash: string;
  phase: JournalPhase;
  createdAt: string;
  updatedAt: string;
  result?: unknown;
}

interface BeginJournalInput {
  requestId: string;
  operation: JournalOperation;
  repoKey: string;
  inputHash: string;
}

interface OperationJournalOptions {
  stateRoot?: string;
  retentionMaxAgeMs?: number;
  retentionMaxCount?: number;
  now?: () => Date;
}

export class IdempotencyConflictError extends Error {
  readonly code = "IDEMPOTENCY_CONFLICT";

  constructor(requestId: string) {
    super(`Request ID was already used with different input: ${requestId}`);
    this.name = "IdempotencyConflictError";
  }
}

const getStateRoot = () =>
  process.env.XDG_STATE_HOME
    ? join(process.env.XDG_STATE_HOME, "usable-git")
    : join(homedir(), ".local", "state", "usable-git");

const validateRequestId = (requestId: string) => {
  if (!/^[A-Za-z0-9._-]{1,128}$/.test(requestId)) {
    throw new TypeError(
      "requestId must contain 1-128 letters, numbers, dots, underscores, or hyphens",
    );
  }
};

const hashKey = (value: string) =>
  createHash("sha256").update(value).digest("hex");

const journalFileName = (requestId: string) => `${hashKey(requestId)}.json`;

const writeDurably = async (path: string, value: JournalRecord) => {
  await mkdir(dirname(path), { recursive: true });
  const temporaryPath = `${path}.${process.pid}.${randomUUID()}.tmp`;
  const file = await open(temporaryPath, "wx", 0o600);

  try {
    await file.writeFile(`${JSON.stringify(value)}\n`, "utf8");
    await file.sync();
  } finally {
    await file.close();
  }

  try {
    await rename(temporaryPath, path);
    const directory = await open(dirname(path), "r");
    try {
      await directory.sync();
    } finally {
      await directory.close();
    }
  } finally {
    await rm(temporaryPath, { force: true });
  }
};

const writeNewDurably = async (path: string, value: JournalRecord) => {
  await mkdir(dirname(path), { recursive: true });
  const temporaryPath = `${path}.${process.pid}.${randomUUID()}.new`;
  const file = await open(temporaryPath, "wx", 0o600);
  try {
    await file.writeFile(`${JSON.stringify(value)}\n`, "utf8");
    await file.sync();
  } finally {
    await file.close();
  }

  try {
    try {
      await link(temporaryPath, path);
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code === "EEXIST") return false;
      throw error;
    }
    const directory = await open(dirname(path), "r");
    try {
      await directory.sync();
    } finally {
      await directory.close();
    }
    return true;
  } finally {
    await rm(temporaryPath, { force: true });
  }
};

export const createOperationJournal = (
  options: OperationJournalOptions = {},
) => {
  const stateRoot = options.stateRoot ?? getStateRoot();
  const journalRoot = join(stateRoot, "journals");
  const retentionMaxAgeMs = options.retentionMaxAgeMs ?? 30 * 24 * 60 * 60 * 1_000;
  const retentionMaxCount = options.retentionMaxCount ?? 1_000;
  const now = options.now ?? (() => new Date());
  const journalPath = (repoKey: string, requestId: string) =>
    join(journalRoot, hashKey(repoKey), journalFileName(requestId));

  const read = async (repoKey: string, requestId: string) => {
    validateRequestId(requestId);
    try {
      return JSON.parse(
        await readFile(journalPath(repoKey, requestId), "utf8"),
      ) as JournalRecord;
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code === "ENOENT") return null;
      throw error;
    }
  };

  const begin = async (input: BeginJournalInput) => {
    validateRequestId(input.requestId);
    const existingOutcome = (existing: JournalRecord) => {
      if (
        existing.operation !== input.operation ||
        existing.repoKey !== input.repoKey ||
        existing.inputHash !== input.inputHash
      ) {
        throw new IdempotencyConflictError(input.requestId);
      }

      if (existing.phase === "terminal") {
        return { kind: "replay" as const, result: existing.result };
      }

      return { kind: "resume" as const, record: existing };
    };
    const existing = await read(input.repoKey, input.requestId);
    if (existing) return existingOutcome(existing);

    const timestamp = now().toISOString();
    const record: JournalRecord = {
      schemaVersion: 1,
      ...input,
      phase: "started",
      createdAt: timestamp,
      updatedAt: timestamp,
    };
    const path = journalPath(input.repoKey, input.requestId);
    const created = await writeNewDurably(path, record);
    if (!created) {
      const concurrent = await read(input.repoKey, input.requestId);
      if (!concurrent) throw new Error("Concurrent journal creation produced no readable record");
      return existingOutcome(concurrent);
    }
    return { kind: "started" as const, record };
  };

  const transition = async (
    repoKey: string,
    requestId: string,
    phase: JournalPhase,
    recoveryMetadata?: unknown,
  ) => {
    const existing = await read(repoKey, requestId);
    if (!existing) throw new Error(`Unknown request ID: ${requestId}`);
    const next: JournalRecord = {
      ...existing,
      phase,
      ...(recoveryMetadata === undefined ? {} : { result: recoveryMetadata }),
      updatedAt: now().toISOString(),
    };
    await writeDurably(journalPath(repoKey, requestId), next);
    return next;
  };

  const complete = async (repoKey: string, requestId: string, result: unknown) => {
    const existing = await read(repoKey, requestId);
    if (!existing) throw new Error(`Unknown request ID: ${requestId}`);
    const next: JournalRecord = {
      ...existing,
      phase: "terminal",
      result,
      updatedAt: now().toISOString(),
    };
    await writeDurably(journalPath(repoKey, requestId), next);
    await prune();
    return next;
  };

  const prune = async () => {
    const completed: Array<{ path: string; updatedAt: number }> = [];
    let repositories;
    try {
      repositories = await readdir(journalRoot, { withFileTypes: true });
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code === "ENOENT") {
        return { deleted: 0, retainedCompleted: 0 };
      }
      throw error;
    }

    for (const repository of repositories) {
      if (!repository.isDirectory()) continue;
      const repositoryPath = join(journalRoot, repository.name);
      for (const entry of await readdir(repositoryPath, { withFileTypes: true })) {
        if (!entry.isFile() || !entry.name.endsWith(".json")) continue;
        const path = join(repositoryPath, entry.name);
        try {
          const record = JSON.parse(await readFile(path, "utf8")) as JournalRecord;
          if (record.schemaVersion !== 1 || record.phase !== "terminal") continue;
          const updatedAt = Date.parse(record.updatedAt);
          if (!Number.isFinite(updatedAt)) continue;
          completed.push({ path, updatedAt });
        } catch {
          // Corrupt and active/ambiguous records are retained for diagnosis.
        }
      }
    }

    completed.sort((left, right) => right.updatedAt - left.updatedAt);
    const cutoff = now().getTime() - retentionMaxAgeMs;
    const deleted = completed.filter(
      (record, index) => record.updatedAt < cutoff || index >= retentionMaxCount,
    );
    await Promise.all(deleted.map(({ path }) => rm(path, { force: true })));
    return {
      deleted: deleted.length,
      retainedCompleted: completed.length - deleted.length,
    };
  };

  return { begin, transition, complete, read, prune };
};

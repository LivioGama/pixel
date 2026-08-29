import { createHash, randomBytes } from "node:crypto";
import { mkdir, open, readFile, rm } from "node:fs/promises";
import { homedir } from "node:os";
import { join } from "node:path";

import {
  gainEventSchema,
  type GainEvent,
  type GainEventInput,
} from "../contracts/v1/gain.ts";
import type { Operation } from "../contracts/v1.ts";

const getDefaultStateRoot = () =>
  process.env.XDG_STATE_HOME ?? join(homedir(), ".local", "state");

export type LedgerOptions = {
  stateRoot?: string;
};

export type LedgerAppendResult =
  | { written: false; reason: "disabled" }
  | { written: true; repositoryHash: string };

const ledgerDirectory = (stateRoot: string) => join(stateRoot, "usable-git");
const ledgerPath = (stateRoot: string) => join(ledgerDirectory(stateRoot), "gain-v1.jsonl");
const saltPath = (stateRoot: string) => join(ledgerDirectory(stateRoot), "gain-v1.salt");

const getOrCreateSalt = async (directory: string, saltFilePath: string) => {
  try {
    return (await readFile(saltFilePath, "utf8")).trim();
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error;
  }
  const salt = randomBytes(32).toString("hex");
  try {
    const handle = await open(saltFilePath, "wx", 0o600);
    try {
      await handle.writeFile(`${salt}\n`, "utf8");
      await handle.sync();
    } finally {
      await handle.close();
    }
    return salt;
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "EEXIST") {
      return (await readFile(saltFilePath, "utf8")).trim();
    }
    throw error;
  }
};

export const hashRepository = (salt: string, repositoryIdentity: string) =>
  createHash("sha256").update(salt).update("\0").update(repositoryIdentity).digest("hex");

export type GainLedger = {
  append: (input: GainEventInput & { repositoryIdentity: string }) => Promise<LedgerAppendResult>;
  read: () => Promise<GainEvent[]>;
  readForRepository: (repositoryIdentity: string) => Promise<GainEvent[]>;
  reset: () => Promise<void>;
  resolveRepositoryHash: (repositoryIdentity: string) => Promise<string>;
  path: string;
};

export const createGainLedger = (options: LedgerOptions = {}): GainLedger => {
  const stateRoot = options.stateRoot ?? getDefaultStateRoot();
  const directory = ledgerDirectory(stateRoot);
  const file = ledgerPath(stateRoot);

  const resolveRepositoryHash = async (repositoryIdentity: string) => {
    await mkdir(directory, { recursive: true, mode: 0o700 });
    const salt = await getOrCreateSalt(directory, saltPath(stateRoot));
    return hashRepository(salt, repositoryIdentity);
  };

  const readAll = async (): Promise<GainEvent[]> => {
    let serialized: string;
    try {
      serialized = await readFile(file, "utf8");
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code === "ENOENT") return [];
      throw error;
    }
    return serialized
      .trim()
      .split("\n")
      .filter(Boolean)
      .map((line) => gainEventSchema.parse(JSON.parse(line)));
  };

  return {
    path: file,

    append: async (input) => {
      await mkdir(directory, { recursive: true, mode: 0o700 });
      const salt = await getOrCreateSalt(directory, saltPath(stateRoot));
      const repositoryHash = hashRepository(salt, input.repositoryIdentity);
      const event = gainEventSchema.parse({
        version: "v1",
        timestamp: new Date().toISOString(),
        operation: input.operation,
        client: input.client,
        transport: input.transport,
        resultCode: input.resultCode,
        repositoryHash,
        envelopeBytes: input.envelopeBytes,
        rawEquivalentBytes: input.rawEquivalentBytes,
        agentOpsRaw: input.agentOpsRaw,
        agentOpsActual: input.agentOpsActual,
        gitSubprocessesRaw: input.gitSubprocessesRaw,
        gitSubprocessesActual: input.gitSubprocessesActual,
        durationMs: input.durationMs,
        tokensSaved: input.tokensSaved,
      });
      const handle = await open(file, "a", 0o600);
      try {
        await handle.writeFile(`${JSON.stringify(event)}\n`, "utf8");
        await handle.sync();
      } finally {
        await handle.close();
      }
      return { written: true, repositoryHash };
    },

    read: readAll,

    readForRepository: async (repositoryIdentity) => {
      const targetHash = await resolveRepositoryHash(repositoryIdentity);
      const events = await readAll();
      return events.filter((event) => event.repositoryHash === targetHash);
    },

    reset: async () => {
      await rm(file, { force: true });
    },

    resolveRepositoryHash,
  };
};

// Aggregation types
export type OperationBreakdown = {
  operation: Operation;
  count: number;
  tokensSaved: number;
  avgPct: number;
  envelopeBytes: number;
  rawBytes: number;
};

export type TimeBucket = {
  bucket: string;
  count: number;
  tokensSaved: number;
};

export type LedgerAggregate = {
  totalOperations: number;
  totalEnvelopeBytes: number;
  totalRawEquivalentBytes: number;
  totalTokensSaved: number;
  totalAgentOpsSaved: number;
  totalSubprocessesSaved: number;
  avgSavingsPct: number;
  byOperation: OperationBreakdown[];
  byDay: TimeBucket[];
  byWeek: TimeBucket[];
  byMonth: TimeBucket[];
  recent: GainEvent[];
};

const isoDay = (timestamp: string) => timestamp.slice(0, 10);
const isoWeek = (timestamp: string) => {
  const date = new Date(timestamp);
  const day = date.getUTCDay() || 7;
  date.setUTCDate(date.getUTCDate() - day + 1);
  return date.toISOString().slice(0, 10);
};
const isoMonth = (timestamp: string) => timestamp.slice(0, 7);

const bucketEvents = (events: GainEvent[], keyFn: (e: GainEvent) => string): TimeBucket[] => {
  const map = new Map<string, TimeBucket>();
  for (const event of events) {
    const key = keyFn(event);
    const existing = map.get(key);
    if (existing) {
      existing.count += 1;
      existing.tokensSaved += event.tokensSaved;
    } else {
      map.set(key, { bucket: key, count: 1, tokensSaved: event.tokensSaved });
    }
  }
  return [...map.values()].sort((a, b) => a.bucket.localeCompare(b.bucket));
};

export const aggregateEvents = (events: GainEvent[]): LedgerAggregate => {
  const totalOperations = events.length;
  let totalEnvelopeBytes = 0;
  let totalRawEquivalentBytes = 0;
  let totalTokensSaved = 0;
  let totalAgentOpsSaved = 0;
  let totalSubprocessesSaved = 0;

  const byOpMap = new Map<Operation, OperationBreakdown>();
  for (const event of events) {
    totalEnvelopeBytes += event.envelopeBytes;
    totalRawEquivalentBytes += event.rawEquivalentBytes;
    totalTokensSaved += event.tokensSaved;
    totalAgentOpsSaved += event.agentOpsRaw - event.agentOpsActual;
    totalSubprocessesSaved += event.gitSubprocessesRaw - event.gitSubprocessesActual;

    const existing = byOpMap.get(event.operation);
    if (existing) {
      existing.count += 1;
      existing.tokensSaved += event.tokensSaved;
      existing.envelopeBytes += event.envelopeBytes;
      existing.rawBytes += event.rawEquivalentBytes;
    } else {
      byOpMap.set(event.operation, {
        operation: event.operation,
        count: 1,
        tokensSaved: event.tokensSaved,
        envelopeBytes: event.envelopeBytes,
        rawBytes: event.rawEquivalentBytes,
        avgPct: 0,
      });
    }
  }

  const byOperation = [...byOpMap.values()]
    .map((entry) => ({
      ...entry,
      avgPct: entry.rawBytes > 0
        ? Math.max(0, (1 - entry.envelopeBytes / entry.rawBytes) * 100)
        : 0,
    }))
    .sort((a, b) => b.tokensSaved - a.tokensSaved);

  const avgSavingsPct = totalRawEquivalentBytes > 0
    ? Math.max(0, (1 - totalEnvelopeBytes / totalRawEquivalentBytes) * 100)
    : 0;

  const recent = [...events].slice(-20);

  return {
    totalOperations,
    totalEnvelopeBytes,
    totalRawEquivalentBytes,
    totalTokensSaved,
    totalAgentOpsSaved,
    totalSubprocessesSaved,
    avgSavingsPct,
    byOperation,
    byDay: bucketEvents(events, (e) => isoDay(e.timestamp)),
    byWeek: bucketEvents(events, (e) => isoWeek(e.timestamp)),
    byMonth: bucketEvents(events, (e) => isoMonth(e.timestamp)),
    recent,
  };
};

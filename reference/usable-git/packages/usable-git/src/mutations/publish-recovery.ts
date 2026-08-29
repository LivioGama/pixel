import { createHash, randomUUID } from "node:crypto";
import { mkdir, open, readFile, rename, rm } from "node:fs/promises";
import { homedir } from "node:os";
import { dirname, join } from "node:path";

type SerializedIndexSnapshot =
  | { exists: false; checksum: null }
  | {
      exists: true;
      checksum: string;
      bytesBase64: string;
      mode: number;
    };

export type PublishRecoveryState = {
  schemaVersion: 1;
  requestId: string;
  repoKey: string;
  inputHash: string;
  phase: "snapshotted" | "index_staged" | "commit_started";
  preHead: string | null;
  files: string[];
  index: SerializedIndexSnapshot;
  ownedIndexChecksum: string | null;
  mode?: "append" | "amend";
  resolvedMessage?: string;
};

type PublishRecoveryStoreOptions = { stateRoot?: string };

export type PublishRecoveryIdentity = Pick<
  PublishRecoveryState,
  "requestId" | "repoKey" | "inputHash" | "files"
> & {
  // Omitted (not null) means the caller cannot re-derive the expected head —
  // snapshot-mode requests — and identity is bound by requestId + inputHash.
  preHead?: string | null;
};

export class RecoveryMetadataError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "RecoveryMetadataError";
  }
}

const defaultStateRoot = () =>
  process.env.XDG_STATE_HOME
    ? join(process.env.XDG_STATE_HOME, "usable-git")
    : join(homedir(), ".local", "state", "usable-git");

const hashKey = (value: string) =>
  createHash("sha256").update(value).digest("hex");

const fileName = (requestId: string) => `${hashKey(requestId)}.json`;

const isObjectId = (value: unknown) =>
  typeof value === "string" && /^(?:[a-f0-9]{40}|[a-f0-9]{64})$/.test(value);

const isChecksum = (value: unknown) =>
  typeof value === "string" && /^[a-f0-9]{64}$/.test(value);

const isSerializedIndex = (value: unknown): value is SerializedIndexSnapshot => {
  if (!value || typeof value !== "object") return false;
  const index = value as Record<string, unknown>;
  if (index.exists === false) return index.checksum === null;
  if (index.exists !== true) return false;
  if (!isChecksum(index.checksum) || typeof index.bytesBase64 !== "string") {
    return false;
  }
  if (!Number.isInteger(index.mode) || (index.mode as number) < 0) return false;
  try {
    return Buffer.from(index.bytesBase64, "base64").toString("base64") === index.bytesBase64;
  } catch {
    return false;
  }
};

const validateState = (value: unknown): PublishRecoveryState => {
  if (!value || typeof value !== "object") {
    throw new RecoveryMetadataError("Recovery metadata is unreadable");
  }
  const state = value as Record<string, unknown>;
  const files = state.files;
  const validFiles =
    Array.isArray(files) &&
    files.length > 0 &&
    files.every((file) => typeof file === "string" && file.length > 0) &&
    new Set(files).size === files.length;
  if (
    state.schemaVersion !== 1 ||
    typeof state.requestId !== "string" ||
    typeof state.repoKey !== "string" ||
    typeof state.inputHash !== "string" ||
    !["snapshotted", "index_staged", "commit_started"].includes(
      state.phase as string,
    ) ||
    !(state.preHead === null || isObjectId(state.preHead)) ||
    !validFiles ||
    !isSerializedIndex(state.index) ||
    !(state.ownedIndexChecksum === null || isChecksum(state.ownedIndexChecksum)) ||
    !(state.mode === undefined || state.mode === "append" || state.mode === "amend") ||
    !(state.resolvedMessage === undefined || typeof state.resolvedMessage === "string")
  ) {
    throw new RecoveryMetadataError("Recovery metadata is unreadable");
  }
  return value as PublishRecoveryState;
};

const sameIdentity = (
  state: PublishRecoveryState,
  expected: PublishRecoveryIdentity,
) =>
  state.requestId === expected.requestId &&
  state.repoKey === expected.repoKey &&
  state.inputHash === expected.inputHash &&
  (expected.preHead === undefined || state.preHead === expected.preHead) &&
  JSON.stringify(state.files) === JSON.stringify(expected.files);

const writeDurably = async (path: string, state: PublishRecoveryState) => {
  await mkdir(dirname(path), { recursive: true });
  const temporaryPath = `${path}.${process.pid}.${randomUUID()}.tmp`;
  const file = await open(temporaryPath, "wx", 0o600);
  try {
    await file.writeFile(`${JSON.stringify(state)}\n`, "utf8");
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

export const createPublishRecoveryStore = (
  options: PublishRecoveryStoreOptions = {},
) => {
  const root = options.stateRoot ?? defaultStateRoot();
  const pathFor = (repoKey: string, requestId: string) =>
    join(root, "publish-recovery", hashKey(repoKey), fileName(requestId));

  const read = async (
    expected: PublishRecoveryIdentity,
  ): Promise<PublishRecoveryState | null> => {
    try {
      const state = validateState(
        JSON.parse(await readFile(pathFor(expected.repoKey, expected.requestId), "utf8")),
      );
      if (!sameIdentity(state, expected)) {
        throw new RecoveryMetadataError(
          "Recovery metadata identity does not match the request",
        );
      }
      return state;
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code === "ENOENT") return null;
      if (error instanceof RecoveryMetadataError) throw error;
      if (error instanceof SyntaxError) {
        throw new RecoveryMetadataError("Recovery metadata is unreadable");
      }
      throw error;
    }
  };

  const write = (state: PublishRecoveryState) => {
    const validated = validateState(state);
    return writeDurably(pathFor(validated.repoKey, validated.requestId), validated);
  };

  const remove = (repoKey: string, requestId: string) =>
    rm(pathFor(repoKey, requestId), { force: true });

  return { read, write, remove };
};

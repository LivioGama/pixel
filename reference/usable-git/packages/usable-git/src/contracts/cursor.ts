import { createHash } from "node:crypto";
import { mkdir, readdir, readFile, rm } from "node:fs/promises";
import { homedir } from "node:os";
import { join } from "node:path";
import { z } from "zod";
import { UsableGitError } from "../errors.ts";

const hexDigestSchema = z.string().regex(/^[a-f0-9]{40,64}$/);
const offsetSchema = z.union([
  z.number().int().nonnegative(),
  z.record(z.string(), z.number().int().nonnegative()),
]);

const payloadSchema = z.object({
  version: z.literal(1),
  operation: z.enum(["review", "history", "diff", "search"]),
  requestDigest: hexDigestSchema,
  snapshot: hexDigestSchema,
  offset: offsetSchema,
});

const wireSchema = z.object({
  payload: payloadSchema,
  checksum: z.string().regex(/^[a-f0-9]{64}$/),
  createdAt: z.string().min(1),
});

export type CursorPayload = z.infer<typeof payloadSchema>;
export type CursorInput = Omit<CursorPayload, "version">;
export type CursorOptions = { stateRoot?: string };

// Agents echo cursors back verbatim, so the wire form is a 12-character
// handle; the integrity-checked payload stays server-side in the state dir.
export const cursorHandlePattern = /^c_[a-f0-9]{10}$/;

const CURSOR_RETENTION_MS = 24 * 60 * 60 * 1_000;
const CURSOR_RETENTION_COUNT = 500;

const defaultStateRoot = () =>
  process.env.XDG_STATE_HOME
    ? join(process.env.XDG_STATE_HOME, "usable-git")
    : join(homedir(), ".local", "state", "usable-git");

const cursorDirectory = (options?: CursorOptions) =>
  join(options?.stateRoot ?? defaultStateRoot(), "cursors");

const canonicalize = (value: unknown): unknown => {
  if (Array.isArray(value)) return value.map(canonicalize);
  if (value && typeof value === "object") {
    return Object.fromEntries(
      Object.entries(value as Record<string, unknown>)
        .sort(([left], [right]) => left.localeCompare(right))
        .map(([key, nested]) => [key, canonicalize(nested)]),
    );
  }
  return value;
};

export const digestValue = (value: unknown) =>
  createHash("sha256").update(JSON.stringify(canonicalize(value))).digest("hex");

const pruneCursors = async (directory: string) => {
  let entries;
  try {
    entries = await readdir(directory, { withFileTypes: true });
  } catch {
    return;
  }
  const records: Array<{ path: string; createdAt: number }> = [];
  for (const entry of entries) {
    if (!entry.isFile() || !entry.name.endsWith(".json")) continue;
    const path = join(directory, entry.name);
    try {
      const wire = wireSchema.parse(JSON.parse(await readFile(path, "utf8")));
      const createdAt = Date.parse(wire.createdAt);
      if (!Number.isFinite(createdAt)) continue;
      records.push({ path, createdAt });
    } catch {
      await rm(path, { force: true });
    }
  }
  records.sort((left, right) => right.createdAt - left.createdAt);
  const cutoff = Date.now() - CURSOR_RETENTION_MS;
  await Promise.all(
    records
      .filter((record, index) => record.createdAt < cutoff || index >= CURSOR_RETENTION_COUNT)
      .map(({ path }) => rm(path, { force: true })),
  );
};

export const encodeCursor = async (
  input: CursorInput,
  options?: CursorOptions,
): Promise<string> => {
  const payload = payloadSchema.parse({ version: 1, ...input });
  const checksum = digestValue(payload);
  const handle = `c_${checksum.slice(0, 10)}`;
  const directory = cursorDirectory(options);
  await mkdir(directory, { recursive: true });
  await Bun.write(
    join(directory, `${handle}.json`),
    `${JSON.stringify({ payload, checksum, createdAt: new Date().toISOString() })}\n`,
  );
  await pruneCursors(directory);
  return handle;
};

export const decodeCursor = async (
  encoded: string,
  operation: CursorPayload["operation"],
  options?: CursorOptions,
): Promise<CursorPayload> => {
  if (!cursorHandlePattern.test(encoded)) {
    throw new UsableGitError("INVALID_INPUT", "Invalid pagination cursor");
  }
  let wire: z.infer<typeof wireSchema>;
  try {
    wire = wireSchema.parse(
      JSON.parse(await readFile(join(cursorDirectory(options), `${encoded}.json`), "utf8")),
    );
  } catch {
    throw new UsableGitError(
      "STALE_STATE",
      "Unknown or expired pagination cursor; restart pagination from the first page",
    );
  }
  if (wire.checksum !== digestValue(wire.payload)) {
    throw new UsableGitError("INVALID_INPUT", "Invalid pagination cursor");
  }
  if (wire.payload.operation !== operation) {
    throw new UsableGitError("INVALID_INPUT", "Invalid pagination cursor");
  }
  return wire.payload;
};

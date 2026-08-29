import { createHash, randomBytes } from "node:crypto";
import { mkdir, open, readFile } from "node:fs/promises";
import { homedir } from "node:os";
import { join } from "node:path";
import {
  telemetryEventInputSchema,
  telemetryEventSchema,
  type TelemetryEventInput,
} from "@usable-git/contracts/v1/telemetry";

export {
  telemetryEventInputSchema,
  telemetryEventSchema,
  type TelemetryEvent,
  type TelemetryEventInput,
} from "@usable-git/contracts/v1/telemetry";

export type TelemetryWriteResult =
  | { written: false; reason: "disabled" }
  | { written: true; repositoryHash: string };

export interface TelemetrySink {
  emit: (input: TelemetryEventInput) => Promise<TelemetryWriteResult>;
}

export interface TelemetrySinkOptions {
  enabled?: boolean;
  stateRoot?: string;
}

const getDefaultStateRoot = () =>
  process.env.XDG_STATE_HOME ?? join(homedir(), ".local", "state");

const getOrCreateSalt = async (directory: string) => {
  const saltPath = join(directory, "telemetry-v1.salt");

  try {
    return (await readFile(saltPath, "utf8")).trim();
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code !== "ENOENT") {
      throw error;
    }
  }

  const salt = randomBytes(32).toString("hex");

  try {
    const handle = await open(saltPath, "wx", 0o600);
    try {
      await handle.writeFile(`${salt}\n`, "utf8");
      await handle.sync();
    } finally {
      await handle.close();
    }
    return salt;
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "EEXIST") {
      return (await readFile(saltPath, "utf8")).trim();
    }
    throw error;
  }
};

const hashRepository = (salt: string, repositoryIdentity: string) =>
  createHash("sha256").update(salt).update("\0").update(repositoryIdentity).digest("hex");

export const createTelemetrySink = (options: TelemetrySinkOptions = {}): TelemetrySink => {
  const enabled = options.enabled === true;
  const directory = join(options.stateRoot ?? getDefaultStateRoot(), "usable-git");

  return {
    emit: async (rawInput) => {
      if (!enabled) {
        return { written: false, reason: "disabled" };
      }

      const input = telemetryEventInputSchema.parse(rawInput);
      await mkdir(directory, { recursive: true, mode: 0o700 });
      const salt = await getOrCreateSalt(directory);
      const repositoryHash = hashRepository(salt, input.repositoryIdentity);
      const event = telemetryEventSchema.parse({
        version: "v1",
        operation: input.operation,
        client: input.client,
        transport: input.transport,
        backend: "git-cli",
        durationMs: input.durationMs,
        gitSubprocessCount: input.gitSubprocessCount,
        resultCode: input.resultCode,
        counts: input.counts,
        components: input.components,
        repositoryHash,
      });
      const eventPath = join(directory, "telemetry-v1.jsonl");
      const handle = await open(eventPath, "a", 0o600);
      try {
        await handle.writeFile(`${JSON.stringify(event)}\n`, "utf8");
        await handle.sync();
      } finally {
        await handle.close();
      }

      return { written: true, repositoryHash };
    },
  };
};

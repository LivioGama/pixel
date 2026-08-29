import { describe, expect, test } from "bun:test";
import { exists, readFile } from "node:fs/promises";
import { join } from "node:path";
import {
  telemetryEventSchema,
  type TelemetryEventInput,
} from "@usable-git/contracts/v1/telemetry";
import { createTelemetrySink } from "@usable-git/telemetry/event";
import { withTempDirectory } from "./support/temp";

const eventInput: Omit<TelemetryEventInput, "repositoryIdentity"> = {
  operation: "inspect",
  client: "codex",
  transport: "mcp",
  durationMs: 12.5,
  gitSubprocessCount: 2,
  resultCode: "success",
  counts: {
    selected: 0,
    staged: 1,
    unstaged: 2,
    untracked: 3,
    conflicted: 0,
    commits: 0,
    warnings: 0,
  },
  components: {
    usableGit: "0.1.0",
    bun: "1.3.14",
    git: "2.54.0",
    client: "0.114.0",
  },
};

describe("operation telemetry", () => {
  test("is disabled by default and creates no state", async () => {
    await withTempDirectory("usable-git-telemetry-disabled-", async (directory) => {
      const stateRoot = join(directory, "state");
      const sink = createTelemetrySink({ stateRoot });

      const outcome = await sink.emit({
        ...eventInput,
        repositoryIdentity: "/private/repositories/customer-secret",
      });

      expect(outcome).toEqual({ written: false, reason: "disabled" });
      expect(await exists(stateRoot)).toBe(false);
    });
  });

  test("writes only strict allowlisted metadata with a stable salted repository hash", async () => {
    await withTempDirectory("usable-git-telemetry-enabled-", async (directory) => {
      const stateRoot = join(directory, "state");
      const sink = createTelemetrySink({ enabled: true, stateRoot });
      const repositoryIdentity = "/private/repositories/customer-secret";

      const first = await sink.emit({ ...eventInput, repositoryIdentity });
      const second = await sink.emit({ ...eventInput, repositoryIdentity });

      expect(first.written).toBe(true);
      expect(second.written).toBe(true);

      const telemetryPath = join(stateRoot, "usable-git", "telemetry-v1.jsonl");
      const serialized = await readFile(telemetryPath, "utf8");
      const events = serialized.trim().split("\n").map((line) => telemetryEventSchema.parse(JSON.parse(line)));

      expect(events).toHaveLength(2);
      expect(events[0]!.repositoryHash).toMatch(/^[a-f0-9]{64}$/);
      expect(events[0]!.repositoryHash).toBe(events[1]!.repositoryHash);
      expect(serialized).not.toContain(repositoryIdentity);
      expect(serialized).not.toContain("customer-secret");
      expect(Object.keys(events[0]!).sort()).toEqual([
        "backend",
        "client",
        "components",
        "counts",
        "durationMs",
        "gitSubprocessCount",
        "operation",
        "repositoryHash",
        "resultCode",
        "transport",
        "version",
      ]);
    });
  });

  test("rejects any field that could retain repository or command content", () => {
    const valid = {
      version: "v1",
      operation: "publish",
      client: "claude-code",
      transport: "cli",
      backend: "git-cli",
      durationMs: 20,
      gitSubprocessCount: 3,
      resultCode: "HOOK_FAILED",
      counts: eventInput.counts,
      components: eventInput.components,
      repositoryHash: "a".repeat(64),
    };

    for (const forbidden of [
      "repoPath",
      "fileName",
      "message",
      "prompt",
      "reasoning",
      "patch",
      "command",
      "argv",
      "stderr",
      "remoteUrl",
      "environment",
      "requestId",
    ]) {
      expect(telemetryEventSchema.safeParse({ ...valid, [forbidden]: "secret" }).success).toBe(false);
    }
  });

  test("uses a different hash when the local salt differs", async () => {
    await withTempDirectory("usable-git-telemetry-salts-", async (directory) => {
      const repositoryIdentity = "/same/repository";
      const first = createTelemetrySink({ enabled: true, stateRoot: join(directory, "one") });
      const second = createTelemetrySink({ enabled: true, stateRoot: join(directory, "two") });

      const firstResult = await first.emit({ ...eventInput, repositoryIdentity });
      const secondResult = await second.emit({ ...eventInput, repositoryIdentity });

      expect(firstResult.written).toBe(true);
      expect(secondResult.written).toBe(true);
      if (!firstResult.written || !secondResult.written) {
        throw new Error("enabled telemetry did not write");
      }
      expect(firstResult.repositoryHash).not.toBe(secondResult.repositoryHash);
    });
  });
});

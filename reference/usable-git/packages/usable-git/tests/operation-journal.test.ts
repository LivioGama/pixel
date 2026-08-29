import { describe, expect, test } from "bun:test";
import { join } from "node:path";
import {
  IdempotencyConflictError,
  createOperationJournal,
} from "@usable-git/mutations/operation-journal";
import { withTempDirectory } from "./support/temp";

describe("operation journal", () => {
  test("persists phases and replays a terminal result for the same request", async () => {
    await withTempDirectory("usable-git-journal-", async (directory) => {
      const journal = createOperationJournal({
        stateRoot: join(directory, "state"),
      });
      const started = await journal.begin({
        requestId: "request-1",
        operation: "publish",
        repoKey: "repo-a",
        inputHash: "input-a",
      });

      expect(started.kind).toBe("started");
      await journal.transition("repo-a", "request-1", "index_staged");
      await journal.complete("repo-a", "request-1", { commitOid: "abc123" });

      const replay = await journal.begin({
        requestId: "request-1",
        operation: "publish",
        repoKey: "repo-a",
        inputHash: "input-a",
      });

      expect(replay).toEqual({
        kind: "replay",
        result: { commitOid: "abc123" },
      });
    });
  });

  test("durably attaches recovery metadata to a nonterminal transition", async () => {
    await withTempDirectory("usable-git-journal-checkpoint-", async (directory) => {
      const journal = createOperationJournal({ stateRoot: join(directory, "state") });
      await journal.begin({
        requestId: "push-checkpoint",
        operation: "push",
        repoKey: "repo-a",
        inputHash: "input-a",
      });
      const checkpoint = {
        schemaVersion: 1,
        sourceOid: "a".repeat(40),
        oldTargetOid: "b".repeat(40),
      };

      await journal.transition(
        "repo-a",
        "push-checkpoint",
        "push_started",
        checkpoint,
      );

      expect(await journal.read("repo-a", "push-checkpoint")).toMatchObject({
        phase: "push_started",
        result: checkpoint,
      });
    });
  });

  test("rejects request ID reuse with different canonical input", async () => {
    await withTempDirectory("usable-git-journal-conflict-", async (directory) => {
      const journal = createOperationJournal({
        stateRoot: join(directory, "state"),
      });
      await journal.begin({
        requestId: "request-2",
        operation: "push",
        repoKey: "repo-a",
        inputHash: "input-a",
      });

      await expect(
        journal.begin({
          requestId: "request-2",
          operation: "push",
          repoKey: "repo-a",
          inputHash: "input-b",
        }),
      ).rejects.toBeInstanceOf(IdempotencyConflictError);
    });
  });

  test("creates a request atomically under concurrent begin calls", async () => {
    await withTempDirectory("usable-git-journal-race-", async (directory) => {
      const journal = createOperationJournal({ stateRoot: join(directory, "state") });
      const input = {
        requestId: "request-race",
        operation: "publish" as const,
        repoKey: "repo-a",
        inputHash: "input-a",
      };
      const outcomes = await Promise.all([journal.begin(input), journal.begin(input)]);
      expect(outcomes.map(({ kind }) => kind).sort()).toEqual(["resume", "started"]);
      expect(await journal.read(input.repoKey, input.requestId)).toMatchObject({
        requestId: input.requestId,
        phase: "started",
      });
    });
  });

  test("scopes the same request ID independently per repository", async () => {
    await withTempDirectory("usable-git-journal-repos-", async (directory) => {
      const journal = createOperationJournal({ stateRoot: join(directory, "state") });
      const first = await journal.begin({
        requestId: "shared-request",
        operation: "publish",
        repoKey: "repo-a",
        inputHash: "input-a",
      });
      const second = await journal.begin({
        requestId: "shared-request",
        operation: "push",
        repoKey: "repo-b",
        inputHash: "input-b",
      });

      expect(first.kind).toBe("started");
      expect(second.kind).toBe("started");
      expect(await journal.read("repo-a", "shared-request")).toMatchObject({
        repoKey: "repo-a",
        operation: "publish",
      });
      expect(await journal.read("repo-b", "shared-request")).toMatchObject({
        repoKey: "repo-b",
        operation: "push",
      });
    });
  });

  test("prunes completed records by age and count without deleting active records", async () => {
    await withTempDirectory("usable-git-journal-retention-", async (directory) => {
      let now = Date.parse("2026-01-01T00:00:00.000Z");
      const journal = createOperationJournal({
        stateRoot: join(directory, "state"),
        retentionMaxAgeMs: 2_000,
        retentionMaxCount: 1,
        now: () => new Date(now),
      });

      for (const requestId of ["old-terminal", "new-terminal"]) {
        await journal.begin({
          requestId,
          operation: "publish",
          repoKey: "repo-a",
          inputHash: requestId,
        });
        await journal.complete("repo-a", requestId, { requestId });
        now += 750;
      }
      await journal.begin({
        requestId: "active",
        operation: "publish",
        repoKey: "repo-a",
        inputHash: "active",
      });
      now += 750;

      expect(await journal.read("repo-a", "old-terminal")).toBeNull();
      const result = await journal.prune();

      expect(result).toEqual({ deleted: 0, retainedCompleted: 1 });
      expect(await journal.read("repo-a", "new-terminal")).not.toBeNull();
      expect(await journal.read("repo-a", "active")).toMatchObject({ phase: "started" });
    });
  });
});

import { afterEach, describe, expect, test } from "bun:test";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { executeOperation, executeOperationWithMetrics } from "../src/service.ts";
import {
  commitFile,
  createRepository,
  type TestRepository,
  writeFile,
} from "./helpers/repository.ts";

const repositories: TestRepository[] = [];
afterEach(async () => Promise.all(repositories.splice(0).map(({ cleanup }) => cleanup())));

const repository = async () => {
  const created = await createRepository();
  repositories.push(created);
  return created;
};

describe("semantic operation service", () => {
  test("returns a lean v1 envelope and aggregated Git process count in metrics", async () => {
    const repo = await repository();
    await commitFile(repo, "tracked.txt", "base\n", "initial");
    await writeFile(repo, "tracked.txt", "changed\n");

    const { envelope, metrics } = await executeOperationWithMetrics(
      "inspect",
      { repoPath: repo.path },
      { transport: "cli" },
    );

    expect(envelope.ok).toBe(true);
    if (!envelope.ok) throw new Error("inspect unexpectedly failed");
    expect(Object.keys(envelope).sort()).toEqual(["ok", "result"]);
    expect(metrics.operation).toBe("inspect");
    expect(metrics.gitSubprocessCount).toBeGreaterThan(0);
    expect(metrics.durationMs).toBeGreaterThanOrEqual(0);
  });

  test("converts validation failures into stable structured errors", async () => {
    const envelope = await executeOperation(
      "inspect",
      { repoPath: "relative" },
      { transport: "mcp" },
    );
    expect(envelope).toMatchObject({
      ok: false,
      error: { code: "INVALID_INPUT" },
    });
  });

  test("reports Git subprocesses consumed before an operation failure", async () => {
    const nonRepository = await mkdtemp(join(tmpdir(), "usable-git-non-repo-"));
    try {
      const { envelope, metrics } = await executeOperationWithMetrics(
        "inspect",
        { repoPath: nonRepository },
        { transport: "cli" },
      );

      expect(envelope).toMatchObject({
        ok: false,
        error: { code: "INVALID_REPOSITORY" },
      });
      expect(metrics.gitSubprocessCount).toBeGreaterThan(0);
    } finally {
      await rm(nonRepository, { recursive: true, force: true });
    }
  });

  test("keeps CLI and MCP operation semantics equivalent", async () => {
    const repo = await repository();
    await writeFile(repo, "new.txt", "new\n");
    const cli = await executeOperation("inspect", { repoPath: repo.path }, { transport: "cli" });
    const mcp = await executeOperation("inspect", { repoPath: repo.path }, { transport: "mcp" });
    expect(cli.ok).toBe(true);
    expect(mcp.ok).toBe(true);
    if (!cli.ok || !mcp.ok) throw new Error("inspect unexpectedly failed");
    expect(mcp.result).toEqual(cli.result);
  });

  test("preserves typed mutation error codes in the shared envelope", async () => {
    const envelope = await executeOperation(
      "publish",
      { repoPath: "/tmp/missing-required-publish-fields" },
      { transport: "cli" },
    );
    expect(envelope).toMatchObject({
      ok: false,
      error: { code: "INVALID_INPUT" },
    });
  });

  test("emits exactly one allowlisted telemetry event at the operation boundary", async () => {
    const repo = await repository();
    await writeFile(repo, "new.txt", "private contents\n");
    const events: unknown[] = [];
    await executeOperation("inspect", { repoPath: repo.path }, {
      transport: "cli",
      client: "codex",
      telemetrySink: {
        emit: async (event) => {
          events.push(event);
          return { written: true, repositoryHash: "a".repeat(64) };
        },
      },
    });
    expect(events).toHaveLength(1);
    expect(events[0]).toMatchObject({
      operation: "inspect",
      client: "codex",
      transport: "cli",
      resultCode: "success",
      repositoryIdentity: repo.path,
    });
    expect(JSON.stringify(events[0])).not.toContain("private contents");
    expect(JSON.stringify(events[0])).not.toContain("new.txt");
  });

  test("generates and echoes a requestId when a mutation omits one", async () => {
    const repo = await repository();
    await writeFile(repo, "selected.txt", "selected\n");
    const inspected = await executeOperation(
      "inspect",
      { repoPath: repo.path },
      { transport: "cli" },
    );
    if (!inspected.ok) throw new Error("inspect failed");
    const envelope = await executeOperation("publish", {
      repoPath: repo.path,
      files: ["selected.txt"],
      message: "publish without an explicit requestId",
      snapshot: (inspected.result as { snapshot: string }).snapshot,
    }, { transport: "cli" });
    expect(envelope.ok).toBe(true);
    expect(envelope.requestId).toMatch(/^auto-[a-f0-9]{12}$/);
  });

  test("returns a valid success envelope for the real publish path", async () => {
    const repo = await repository();
    await writeFile(repo, "selected.txt", "selected\n");
    const inspected = await executeOperation(
      "inspect",
      { repoPath: repo.path, files: ["selected.txt"] },
      { transport: "cli" },
    );
    if (!inspected.ok) throw new Error("inspect failed");
    const inspectedResult = inspected.result as { snapshot: string };
    const requestId = `service-publish-${crypto.randomUUID()}`;
    const envelope = await executeOperation("publish", {
      repoPath: repo.path,
      files: ["selected.txt"],
      message: "publish through service",
      requestId,
      snapshot: inspectedResult.snapshot,
    }, { transport: "cli" });
    expect(envelope).toMatchObject({
      ok: true,
      requestId,
      result: { committedPaths: ["selected.txt"] },
    });
  });
});

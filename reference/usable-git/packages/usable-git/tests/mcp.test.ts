import { afterEach, describe, expect, test } from "bun:test";
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { InMemoryTransport } from "@modelcontextprotocol/sdk/inMemory.js";
import { join } from "node:path";
import { createMcpServer } from "../src/mcp.ts";
import type { TelemetryEventInput } from "../src/contracts/v1/telemetry.ts";
import {
  createRepository,
  type TestRepository,
  writeFile,
} from "./helpers/repository.ts";

const repositories: TestRepository[] = [];
afterEach(async () => Promise.all(repositories.splice(0).map(({ cleanup }) => cleanup())));

const connect = async () => {
  const server = createMcpServer();
  const client = new Client({ name: "usable-git-test", version: "1.0.0" });
  const [clientTransport, serverTransport] = InMemoryTransport.createLinkedPair();
  await server.connect(serverTransport);
  await client.connect(clientTransport);
  return { server, client };
};

describe("usable-git MCP server", () => {
  test("exits promptly when the stdio client disconnects", async () => {
    const child = Bun.spawn([
      process.execPath,
      join(import.meta.dir, "..", "src", "cli.ts"),
      "mcp",
    ], {
      stdin: "pipe",
      stdout: "pipe",
      stderr: "pipe",
    });
    child.stdin.end();
    const exitCode = await Promise.race([
      child.exited,
      new Promise<number>((resolve) => setTimeout(() => resolve(-1), 1_000)),
    ]);
    if (exitCode === -1) child.kill("SIGKILL");
    expect(exitCode).toBe(0);
  });

  test("exposes exactly eleven tools with accurate safety annotations and output schemas", async () => {
    const { server, client } = await connect();
    try {
      const listed = await client.listTools();
      expect(listed.tools.map(({ name }) => name)).toEqual([
        "inspect",
        "review",
        "history",
        "publish",
        "push",
        "ship",
        "diff",
        "search",
        "branch",
        "sync",
        "update",
      ]);
      for (const tool of listed.tools) {
        expect(tool.inputSchema.type).toBe("object");
        expect(tool.outputSchema?.type).toBe("object");
      }
      expect(listed.tools.find(({ name }) => name === "inspect")?.annotations).toMatchObject({
        readOnlyHint: true,
        idempotentHint: true,
        openWorldHint: false,
        destructiveHint: false,
      });
      expect(listed.tools.find(({ name }) => name === "push")?.annotations).toMatchObject({
        readOnlyHint: false,
        idempotentHint: true,
        openWorldHint: true,
        destructiveHint: true,
      });
    } finally {
      await client.close();
      await server.close();
    }
  });

  test("teaches the error-recovery loops in the descriptions the model reads at runtime", async () => {
    const { server, client } = await connect();
    try {
      const byName = new Map(
        (await client.listTools()).tools.map((tool) => [tool.name, tool.description ?? ""]),
      );

      // Tool descriptions are the only usable-git documentation a model sees at
      // runtime, so every recovery loop has to be stated there rather than in
      // the repository's prose rule.
      for (const name of ["publish", "ship"]) {
        expect(byName.get(name)).toContain("STALE_STATE");
        expect(byName.get(name)).toContain("inspect");
      }
      expect(byName.get("publish")).toContain("expected");
      expect(byName.get("push")).toContain("NON_FAST_FORWARD");
      expect(byName.get("push")).toContain("sync");
      expect(byName.get("inspect")).toContain("snapshot token");
      expect(byName.get("branch")).toContain("REF_EXISTS");
      expect(byName.get("search")).toContain("partial");
      expect(byName.get("search")).toContain("lifecycle");
      expect(byName.get("search")).toContain("Never fetches");

      // Descriptions are paid on every session, so keep the surface bounded.
      const tokens = JSON.stringify([...byName]).length / 4;
      expect(tokens).toBeLessThan(8_500);
    } finally {
      await client.close();
      await server.close();
    }
  });

  test("serializes the full envelope into the text block for clients that ignore structuredContent", async () => {
    const repository = await createRepository();
    repositories.push(repository);
    await writeFile(repository, "new.txt", "new\n");
    const { server, client } = await connect();
    try {
      const response = await client.callTool({
        name: "inspect",
        arguments: { repoPath: repository.path },
      });
      expect(response.structuredContent).toMatchObject({ ok: true });
      const content = response.content as Array<{ type: string; text: string }>;
      expect(content).toHaveLength(1);
      expect(content[0]?.type).toBe("text");
      expect(JSON.parse(content[0]?.text ?? "")).toEqual(response.structuredContent);
    } finally {
      await client.close();
      await server.close();
    }
  });

  test("returns the compact summary text block when USABLE_GIT_MCP_TEXT=summary", async () => {
    const repository = await createRepository();
    repositories.push(repository);
    const previous = process.env["USABLE_GIT_MCP_TEXT"];
    process.env["USABLE_GIT_MCP_TEXT"] = "summary";
    const { server, client } = await connect();
    try {
      const response = await client.callTool({
        name: "inspect",
        arguments: { repoPath: repository.path },
      });
      const content = response.content as Array<{ type: string; text: string }>;
      expect(content).toEqual([
        expect.objectContaining({ type: "text", text: expect.stringContaining("inspect: ok") }),
      ]);
    } finally {
      if (previous === undefined) delete process.env["USABLE_GIT_MCP_TEXT"];
      else process.env["USABLE_GIT_MCP_TEXT"] = previous;
      await client.close();
      await server.close();
    }
  });

  test("attributes telemetry to the connected client implementation", async () => {
    const repository = await createRepository();
    repositories.push(repository);
    const events: TelemetryEventInput[] = [];
    const server = createMcpServer({
      telemetrySink: {
        emit: async (event) => {
          events.push(event);
          return { written: false, reason: "disabled" };
        },
      },
    });
    const client = new Client({ name: "codex-mcp", version: "0.114.0" });
    const [clientTransport, serverTransport] = InMemoryTransport.createLinkedPair();
    await server.connect(serverTransport);
    await client.connect(clientTransport);
    try {
      await client.callTool({
        name: "inspect",
        arguments: { repoPath: repository.path },
      });
      expect(events).toHaveLength(1);
      expect(events[0]).toMatchObject({
        client: "codex",
        transport: "mcp",
        components: { client: "0.114.0" },
      });
    } finally {
      await client.close();
      await server.close();
    }
  });
});

import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import type { CallToolResult } from "@modelcontextprotocol/sdk/types.js";
import {
  historyRequestSchema,
  inspectRequestSchema,
  reviewRequestSchema,
  type V1Envelope,
} from "./contracts/v1.ts";
import { branchRequestSchema } from "./contracts/v1/branch.ts";
import { diffRequestSchema } from "./contracts/v1/diff.ts";
import { publishRequestSchema } from "./contracts/v1/publish.ts";
import { pushRequestSchema } from "./contracts/v1/push.ts";
import { shipRequestSchema } from "./contracts/v1/ship.ts";
import { searchRequestSchema } from "./contracts/v1/search.ts";
import { syncRequestSchema } from "./contracts/v1/sync.ts";
import { updateRequestSchema } from "./contracts/v1/update.ts";
import { operationMcpOutputSchemas } from "./contracts/v1/results.ts";
import { executeOperation, type Operation } from "./service.ts";
import type { TelemetryEventInput } from "./contracts/v1/telemetry.ts";
import type { TelemetrySink } from "./telemetry/event.ts";

const toolAnnotations = {
  read: {
    readOnlyHint: true,
    destructiveHint: false,
    idempotentHint: true,
    openWorldHint: false,
  },
  localMutation: {
    readOnlyHint: false,
    destructiveHint: false,
    idempotentHint: true,
    openWorldHint: false,
  },
  publish: {
    readOnlyHint: false,
    destructiveHint: true,
    idempotentHint: true,
    openWorldHint: false,
  },
  push: {
    readOnlyHint: false,
    destructiveHint: true,
    idempotentHint: true,
    openWorldHint: true,
  },
  sync: {
    readOnlyHint: false,
    destructiveHint: false,
    idempotentHint: true,
    openWorldHint: true,
  },
} as const;

const compactSummary = (operation: Operation, envelope: V1Envelope) =>
  envelope.ok
    ? `${operation}: ok`
    : `${operation}: ${envelope.error.code} — ${envelope.error.message}`;

const textContentMode = (): "summary" | "json" =>
  process.env["USABLE_GIT_MCP_TEXT"] === "summary" ? "summary" : "json";

const toolResult = (operation: Operation, envelope: V1Envelope): CallToolResult => ({
  content: [{
    type: "text" as const,
    text: textContentMode() === "json"
      ? JSON.stringify(envelope)
      : compactSummary(operation, envelope),
  }],
  structuredContent: envelope as unknown as Record<string, unknown>,
  ...(envelope.ok ? {} : { isError: true }),
});

const telemetryClient = (name = ""): TelemetryEventInput["client"] => {
  if (/codex/i.test(name)) return "codex";
  if (/claude/i.test(name)) return "claude-code";
  if (/cursor/i.test(name)) return "cursor-agent";
  if (/devin/i.test(name)) return "devin-cli";
  return "other";
};

export const createMcpServer = (options: { telemetrySink?: TelemetrySink } = {}) => {
  const server = new McpServer({ name: "usable-git", version: "0.1.0" });
  const handler = (operation: Operation) => async (input: unknown): Promise<CallToolResult> => {
    const client = server.server.getClientVersion();
    return toolResult(operation, await executeOperation(operation, input, {
      transport: "mcp",
      client: telemetryClient(client?.name),
      clientVersion: client?.version,
      ...(options.telemetrySink ? { telemetrySink: options.telemetrySink } : {}),
    }));
  };
  server.registerTool("inspect", {
    description:
      "Inspect one local repository snapshot without mutation or network access. " +
      "Returns a 12-hex snapshot token — machine-local, ~24h — that publish and " +
      "ship take instead of per-file fingerprints. Call this first.",
    inputSchema: inspectRequestSchema.shape,
    outputSchema: operationMcpOutputSchemas.inspect,
    annotations: toolAnnotations.read,
  }, handler("inspect"));
  server.registerTool("review", {
    description:
      "Return staged and unstaged repository evidence with bounded pagination. " +
      "On STALE_STATE the repository moved: restart from the first page.",
    inputSchema: reviewRequestSchema.shape,
    outputSchema: operationMcpOutputSchemas.review,
    annotations: toolAnnotations.read,
  }, handler("review"));
  server.registerTool("history", {
    description:
      "Read bounded history from an existing local ref without fetching. " +
      'Compact by default; detail:"full" restores parents, both identities, ' +
      "and signature status.",
    inputSchema: historyRequestSchema.shape,
    outputSchema: operationMcpOutputSchemas.history,
    annotations: toolAnnotations.read,
  }, handler("history"));
  server.registerTool("publish", {
    description:
      "Commit exactly the selected paths after optimistic state validation. " +
      "Pass the snapshot token from inspect — no fingerprint copying needed. " +
      "On STALE_STATE, re-run inspect and retry with the fresh token. When the " +
      "state directory is not shared across calls (containers, CI, sandboxes), " +
      "pass expected:{head,fingerprints} instead of snapshot. " +
      'mode:{kind:"amend"} rewrites the tip commit instead of adding one.',
    inputSchema: publishRequestSchema.shape,
    outputSchema: operationMcpOutputSchemas.publish,
    annotations: toolAnnotations.publish,
  }, handler("publish"));
  server.registerTool("push", {
    description:
      "Update exactly one configured remote branch with fast-forward or an exact " +
      "lease. Remote names come from inspect.remotes. On NON_FAST_FORWARD, call " +
      "sync, then update, then retry this push.",
    inputSchema: pushRequestSchema.shape,
    outputSchema: operationMcpOutputSchemas.push,
    annotations: toolAnnotations.push,
  }, handler("push"));
  server.registerTool("ship", {
    description:
      "Commit the selected paths and push the resulting commit in one call — the " +
      "preferred commit-and-push flow (inspect then ship). Pass the snapshot token " +
      "from inspect. If the push leg fails the commit still stands: the envelope " +
      "stays ok:true and result.push.ok is false with retry guidance, so retry only " +
      "the push. On STALE_STATE, re-run inspect and retry.",
    inputSchema: shipRequestSchema.shape,
    outputSchema: operationMcpOutputSchemas.ship,
    annotations: toolAnnotations.push,
  }, handler("ship"));
  server.registerTool("diff", {
    description:
      "Return the patch between two exact commit oids, or one commit against " +
      "its first parent. Oids come from inspect, history, or sync — ref names " +
      "and revision expressions are rejected.",
    inputSchema: diffRequestSchema.shape,
    outputSchema: operationMcpOutputSchemas.diff,
    annotations: toolAnnotations.read,
  }, handler("diff"));
  server.registerTool("search", {
    description:
      "Search the entire local git history in one call: commit messages, file " +
      "paths, and diff text, ranked and compact. target:{kind:\"text\",query} " +
      "answers \"where did X happen\"; target:{kind:\"lifecycle\",path|token} " +
      "answers \"was X dropped\" with firstSeen/removedIn/presentAtHead. Never " +
      "fetches. The index builds lazily: result.index.state \"partial\" means " +
      "call again to index more history. Hit oids feed diff directly. On " +
      "STALE_STATE the history moved: restart pagination.",
    inputSchema: searchRequestSchema.shape,
    outputSchema: operationMcpOutputSchemas.search,
    annotations: toolAnnotations.read,
  }, handler("search"));
  server.registerTool("branch", {
    description:
      "Create a branch at the current HEAD and switch to it, or switch to an " +
      "existing branch. Start feature work here. Switching refuses while tracked " +
      "changes are uncommitted — publish them first; untracked files never block. " +
      "REF_EXISTS means that branch name is already taken.",
    inputSchema: branchRequestSchema.shape,
    outputSchema: operationMcpOutputSchemas.branch,
    annotations: toolAnnotations.localMutation,
  }, handler("branch"));
  server.registerTool("sync", {
    description:
      "Fetch exactly the named branches from one configured remote into " +
      "remote-tracking refs and report refreshed ahead/behind. Never touches " +
      "worktree, index, local branches, or HEAD. Use it to obtain the exact " +
      "target oid that update requires.",
    inputSchema: syncRequestSchema.shape,
    outputSchema: operationMcpOutputSchemas.sync,
    annotations: toolAnnotations.sync,
  }, handler("sync"));
  server.registerTool("update", {
    description:
      "Fast-forward the current branch to an exact target oid observed via sync. " +
      "Refuses divergence and any overlap with uncommitted changes. " +
      "NON_FAST_FORWARD here means the branch has diverged: resolve that outside " +
      "usable-git.",
    inputSchema: updateRequestSchema.shape,
    outputSchema: operationMcpOutputSchemas.update,
    annotations: toolAnnotations.localMutation,
  }, handler("update"));
  return server;
};

export const runMcpServer = async () => {
  const server = createMcpServer();
  const transport = new StdioServerTransport();
  await server.connect(transport);
  try {
    await new Promise<void>((resolve, reject) => {
      if (process.stdin.readableEnded || process.stdin.destroyed) {
        resolve();
        return;
      }
      const finish = () => resolve();
      process.stdin.once("end", finish);
      process.stdin.once("close", finish);
      process.stdin.once("error", reject);
    });
  } finally {
    await server.close();
  }
};

import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StdioClientTransport } from "@modelcontextprotocol/sdk/client/stdio.js";
import { mkdtemp, mkdir, realpath, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";

import {
  inspectCursorRegistration,
} from "@usable-git/install/cursor.ts";
import {
  inspectNativeClientRegistration,
  type InstallRunner,
} from "@usable-git/install/native.ts";
import type {
  InstallClient,
  InstallClientSelection,
} from "@usable-git/install/index.ts";

const CLIENT_ORDER = ["codex", "claude", "cursor", "devin"] as const;
const TOOL_ORDER = [
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
] as const;

const TOOL_INPUT_PROPERTIES: Record<(typeof TOOL_ORDER)[number], string[]> = {
  inspect: ["files", "repoPath"],
  review: ["byteCap", "cursor", "files", "repoPath"],
  history: ["byteCap", "cursor", "detail", "limit", "ref", "repoPath"],
  publish: [
    "expected",
    "files",
    "message",
    "mode",
    "repoPath",
    "requestId",
    "snapshot",
  ],
  push: ["expectedSourceOid", "mode", "remote", "repoPath", "requestId", "sourceRef", "targetRef"],
  ship: [
    "expected",
    "files",
    "message",
    "mode",
    "remote",
    "repoPath",
    "requestId",
    "snapshot",
    "targetRef",
  ],
  diff: ["byteCap", "cursor", "files", "repoPath", "target"],
  search: ["byteCap", "cursor", "limit", "repoPath", "target"],
  branch: ["expectedHead", "mode", "repoPath", "requestId"],
  sync: ["branches", "remote", "repoPath"],
  update: ["expectedHead", "repoPath", "requestId", "targetOid"],
};

const TOOL_REQUIRED_PROPERTIES: Record<(typeof TOOL_ORDER)[number], string[]> = {
  inspect: ["repoPath"],
  review: ["repoPath"],
  history: ["repoPath"],
  publish: ["files", "repoPath"],
  push: ["expectedSourceOid", "mode", "remote", "repoPath", "sourceRef", "targetRef"],
  ship: ["files", "message", "remote", "repoPath"],
  diff: ["repoPath", "target"],
  search: ["repoPath", "target"],
  branch: ["expectedHead", "mode", "repoPath"],
  sync: ["remote", "repoPath"],
  update: ["expectedHead", "repoPath", "targetOid"],
};

const ENVELOPE_PROPERTIES = [
  "error",
  "ok",
  "requestId",
  "result",
  "warnings",
];

export type DoctorCheckStatus = "pass" | "fail" | "skip";

export type DoctorCheck = {
  id: string;
  status: DoctorCheckStatus;
  required: boolean;
  durationMs: number;
  summary: string;
  reason?: string;
  details?: Record<string, unknown>;
};

export type DoctorReport = {
  version: "v1";
  ok: boolean;
  clients: InstallClient[];
  activatedClients: InstallClient[];
  executablePath: string;
  checks: DoctorCheck[];
  summary: {
    passed: number;
    failed: number;
    skipped: number;
  };
};

export type DoctorProcessRequest = {
  command: string;
  args: string[];
  env?: Record<string, string>;
  cwd?: string;
  stdin?: string;
  timeoutMs?: number;
};

export type DoctorProcessResult = {
  exitCode: number;
  stdout: string;
  stderr: string;
};

export type DoctorProcessRunner = (
  request: DoctorProcessRequest,
) => Promise<DoctorProcessResult>;

export type DoctorClientInvocation =
  | {
      available: false;
      invoked: false;
      reason: string;
    }
  | {
      available: true;
      invoked: boolean;
      operation?: "inspect";
      transport?: "mcp";
      diagnostic?: string;
    };

export type DoctorClientInvoker = (request: {
  client: InstallClient;
  executablePath: string;
  home: string;
  repoPath: string;
  processRunner: DoctorProcessRunner;
}) => Promise<DoctorClientInvocation>;

export type RunDoctorOptions = {
  clients: InstallClientSelection;
  executablePath: string;
  home?: string;
  runner?: InstallRunner;
  processRunner?: DoctorProcessRunner;
  clientInvoker?: DoctorClientInvoker;
};

const environment = (overrides: Record<string, string> = {}) =>
  Object.fromEntries(
    Object.entries({ ...process.env, ...overrides }).filter(
      (entry): entry is [string, string] =>
        entry[0] !== "ANTHROPIC_API_KEY" && typeof entry[1] === "string",
    ),
  );

const decode = async (stream: unknown) =>
  stream instanceof ReadableStream
    ? new TextDecoder().decode(await new Response(stream).arrayBuffer())
    : "";

export const createDoctorProcessRunner = (): DoctorProcessRunner => async ({
  command,
  args,
  env,
  cwd,
  stdin,
  timeoutMs = 30_000,
}) => {
  let child: ReturnType<typeof Bun.spawn>;
  try {
    child = Bun.spawn([command, ...args], {
      ...(cwd ? { cwd } : {}),
      env: environment(env),
      stdin: stdin === undefined ? "ignore" : "pipe",
      stdout: "pipe",
      stderr: "pipe",
    });
  } catch (error) {
    return {
      exitCode: 127,
      stdout: "",
      stderr: error instanceof Error ? error.message : String(error),
    };
  }

  const inputSink = child.stdin;
  if (stdin !== undefined && inputSink && typeof inputSink !== "number") {
    inputSink.write(stdin);
    inputSink.end();
  }

  let timeout: ReturnType<typeof setTimeout> | undefined;
  const timedOut = new Promise<"timeout">((resolveTimeout) => {
    timeout = setTimeout(() => {
      child.kill();
      resolveTimeout("timeout");
    }, timeoutMs);
  });
  const completed = child.exited.then((exitCode) => ({ exitCode }));
  const outcome = await Promise.race([completed, timedOut]);
  if (timeout) clearTimeout(timeout);
  const [stdout, stderr] = await Promise.all([decode(child.stdout), decode(child.stderr)]);
  return outcome === "timeout"
    ? { exitCode: 124, stdout, stderr: `${stderr}\nprocess timed out`.trim() }
    : { exitCode: outcome.exitCode, stdout, stderr };
};

const normalizeClients = (selection: InstallClientSelection): InstallClient[] => {
  if (selection === "all") return [...CLIENT_ORDER];
  if (!Array.isArray(selection) || selection.length === 0) {
    throw new Error("doctor client selection must include at least one client");
  }
  if (selection.some((client) => !CLIENT_ORDER.includes(client))) {
    throw new Error("doctor client selection contains an unsupported client");
  }
  if (new Set(selection).size !== selection.length) {
    throw new Error("doctor client selection contains a duplicate client");
  }
  const selected = new Set(selection);
  return CLIENT_ORDER.filter((client) => selected.has(client));
};

const bounded = (value: string) =>
  value
    .replace(/([a-z][a-z0-9+.-]*:\/\/)[^\s/@]+:[^\s/@]+@/gi, "$1<redacted>@")
    .replace(/([?&](?:access_token|token|key|password)=)[^&\s]+/gi, "$1<redacted>")
    .replace(/\s+/g, " ")
    .trim()
    .slice(0, 1_000);

const check = async (
  id: string,
  run: () => Promise<{ summary: string; details?: Record<string, unknown> }>,
): Promise<DoctorCheck> => {
  const startedAt = performance.now();
  try {
    const result = await run();
    return {
      id,
      status: "pass",
      required: true,
      durationMs: performance.now() - startedAt,
      ...result,
    };
  } catch (error) {
    return {
      id,
      status: "fail",
      required: true,
      durationMs: performance.now() - startedAt,
      summary: "Required doctor check failed",
      reason: bounded(error instanceof Error ? error.message : String(error)),
    };
  }
};

const skip = (id: string, reason: string): DoctorCheck => ({
  id,
  status: "skip",
  required: true,
  durationMs: 0,
  summary: "Selected client is unavailable",
  reason: bounded(reason),
});

const requireSuccess = (result: DoctorProcessResult, operation: string) => {
  if (result.exitCode !== 0) {
    throw new Error(`${operation} exited ${result.exitCode}: ${bounded(result.stderr || result.stdout)}`);
  }
  return result.stdout;
};

const git = async (
  runner: DoctorProcessRunner,
  cwd: string,
  args: string[],
  env?: Record<string, string>,
) => requireSuccess(await runner({ command: "git", args, cwd, env }), `git ${args[0] ?? ""}`);

const parseEnvelope = (result: DoctorProcessResult, operation: string) => {
  let value: unknown;
  try {
    value = JSON.parse(result.stdout);
  } catch {
    throw new Error(`${operation} did not return one JSON envelope: ${bounded(result.stdout)}`);
  }
  if (
    result.exitCode !== 0 ||
    !value ||
    typeof value !== "object" ||
    !("ok" in value) ||
    value.ok !== true ||
    !("result" in value)
  ) {
    throw new Error(`${operation} returned a failed or invalid envelope`);
  }
  return value as Record<string, unknown>;
};

const invokeCli = async (
  runner: DoctorProcessRunner,
  executablePath: string,
  operation: string,
  input: unknown,
  env: Record<string, string>,
) => parseEnvelope(
  await runner({
    command: executablePath,
    args: [operation, "--input", "-"],
    env,
    stdin: JSON.stringify(input),
  }),
  operation,
);

type Fixture = {
  directory: string;
  repoPath: string;
  remotePath: string;
  env: Record<string, string>;
};

const createFixture = async (
  runner: DoctorProcessRunner,
  home: string,
): Promise<Fixture> => {
  const directory = await realpath(await mkdtemp(join(tmpdir(), "usable-git-doctor-")));
  const repoPath = join(directory, "repository");
  const remotePath = join(directory, "remote.git");
  const env = environment({
    HOME: home,
    XDG_STATE_HOME: join(directory, "state"),
    GIT_AUTHOR_NAME: "Usable Git Doctor",
    GIT_AUTHOR_EMAIL: "usable-git-doctor@example.test",
    GIT_COMMITTER_NAME: "Usable Git Doctor",
    GIT_COMMITTER_EMAIL: "usable-git-doctor@example.test",
  });
  await mkdir(repoPath, { recursive: true });
  await git(runner, repoPath, ["init", "--quiet", "--initial-branch=main"], env);
  await git(runner, repoPath, ["config", "user.name", "Usable Git Doctor"], env);
  await git(runner, repoPath, ["config", "user.email", "usable-git-doctor@example.test"], env);
  await Promise.all([
    writeFile(join(repoPath, "selected.txt"), "selected base\n"),
    writeFile(join(repoPath, "unrelated-staged.txt"), "staged base\n"),
    writeFile(join(repoPath, "unrelated-unstaged.txt"), "unstaged base\n"),
  ]);
  await git(runner, repoPath, ["add", "--", "selected.txt", "unrelated-staged.txt", "unrelated-unstaged.txt"], env);
  await git(runner, repoPath, ["commit", "--quiet", "-m", "doctor baseline"], env);
  await Promise.all([
    writeFile(join(repoPath, "selected.txt"), "selected published\n"),
    writeFile(join(repoPath, "unrelated-staged.txt"), "staged pending\n"),
    writeFile(join(repoPath, "unrelated-unstaged.txt"), "unstaged pending\n"),
    writeFile(join(repoPath, "unrelated-untracked.txt"), "untracked pending\n"),
  ]);
  await git(runner, repoPath, ["add", "--", "unrelated-staged.txt"], env);
  await git(runner, directory, ["init", "--bare", "--quiet", "--initial-branch=main", remotePath], env);
  await git(runner, repoPath, ["remote", "add", "doctor-origin", remotePath], env);
  return { directory, repoPath, remotePath, env };
};

const inspectResult = (envelope: Record<string, unknown>) => {
  if (!envelope.result || typeof envelope.result !== "object") {
    throw new Error("inspect result is missing");
  }
  return envelope.result as Record<string, unknown>;
};

const inspectRequestForPublish = (result: Record<string, unknown>) => {
  if (typeof result.snapshot !== "string" || !/^[a-f0-9]{12}$/.test(result.snapshot)) {
    throw new Error("inspect did not return a snapshot token");
  }
  const changes = Array.isArray(result.changes) ? result.changes : [];
  const selected = changes.find(
    (change) => change && typeof change === "object" && "path" in change && change.path === "selected.txt",
  );
  if (!selected) {
    throw new Error("inspect did not report the selected.txt change");
  }
  return { snapshot: result.snapshot };
};

const checkToolSchemas = (tools: Array<Record<string, unknown>>) => {
  const names = tools.map(({ name }) => name);
  if (JSON.stringify(names) !== JSON.stringify(TOOL_ORDER)) {
    throw new Error(`MCP tool list differs from v1: ${JSON.stringify(names)}`);
  }
  for (const tool of tools) {
    const name = tool.name as (typeof TOOL_ORDER)[number];
    const input = tool.inputSchema as Record<string, unknown> | undefined;
    const output = tool.outputSchema as Record<string, unknown> | undefined;
    const properties = input?.properties && typeof input.properties === "object"
      ? Object.keys(input.properties).sort()
      : [];
    const required = Array.isArray(input?.required)
      ? input.required.filter((value): value is string => typeof value === "string").sort()
      : [];
    const outputProperties = output?.properties && typeof output.properties === "object"
      ? Object.keys(output.properties).sort()
      : [];
    if (JSON.stringify(properties) !== JSON.stringify(TOOL_INPUT_PROPERTIES[name])) {
      throw new Error(`${name} MCP input schema properties differ from v1`);
    }
    if (JSON.stringify(required) !== JSON.stringify(TOOL_REQUIRED_PROPERTIES[name])) {
      throw new Error(`${name} MCP required properties differ from v1`);
    }
    if (
      output?.type !== "object" ||
      JSON.stringify(outputProperties) !== JSON.stringify(ENVELOPE_PROPERTIES)
    ) {
      throw new Error(`${name} MCP output schema differs from the v1 envelope`);
    }
  }
};

const mcpCheck = async (
  executablePath: string,
  fixture: Fixture,
) => {
  const client = new Client({ name: "usable-git-doctor", version: "1.0.0" });
  const transport = new StdioClientTransport({
    command: executablePath,
    args: ["mcp"],
    env: fixture.env,
    cwd: fixture.repoPath,
    stderr: "pipe",
  });
  try {
    await client.connect(transport);
    const server = client.getServerVersion();
    if (server?.name !== "usable-git") throw new Error("MCP initialize returned wrong server identity");
    const listed = await client.listTools();
    checkToolSchemas(listed.tools as Array<Record<string, unknown>>);
    const called = await client.callTool({
      name: "inspect",
      arguments: { repoPath: fixture.repoPath },
    });
    const structured = called.structuredContent as Record<string, unknown> | undefined;
    if (structured?.ok !== true || !structured.result || typeof structured.result !== "object") {
      throw new Error("MCP inspect call did not return a successful structured v1 envelope");
    }
    return {
      summary: "MCP initialize, exact five-tool schema, and inspect call passed",
      details: {
        initialized: true,
        serverVersion: server?.version,
        tools: [...TOOL_ORDER],
        called: "inspect",
      },
    };
  } finally {
    await client.close().catch(() => undefined);
  }
};

const defaultClientCommand = (client: InstallClient) => client === "cursor" ? "agent" : client;

const containsSemanticInspectTrace = (output: string) =>
  /mcp(?:__|[\s:/-])+usable-git(?:__|[\s:/-])+inspect/i.test(output) ||
  /(?:tool_name|toolName|name)[^\n]{0,80}usable-git[^\n]{0,80}inspect/i.test(output);

const containsCompletedCodexInspect = (output: string) => output
  .split(/\r?\n/)
  .some((line) => {
    try {
      const event = JSON.parse(line) as Record<string, unknown>;
      const item = event.item && typeof event.item === "object"
        ? event.item as Record<string, unknown>
        : undefined;
      return event.type === "item.completed" &&
        item?.type === "mcp_tool_call" &&
        item.server === "usable-git" &&
        item.tool === "inspect" &&
        item.status === "completed" &&
        item.result !== null &&
        item.result !== undefined;
    } catch {
      return false;
    }
  });

const containsCompletedCursorInspect = (output: string) => output
  .split(/\r?\n/)
  .some((line) => {
    try {
      const event = JSON.parse(line) as Record<string, unknown>;
      const toolCall = event.tool_call && typeof event.tool_call === "object"
        ? event.tool_call as Record<string, unknown>
        : undefined;
      const mcpCall = toolCall?.mcpToolCall && typeof toolCall.mcpToolCall === "object"
        ? toolCall.mcpToolCall as Record<string, unknown>
        : undefined;
      const args = mcpCall?.args && typeof mcpCall.args === "object"
        ? mcpCall.args as Record<string, unknown>
        : undefined;
      const result = mcpCall?.result && typeof mcpCall.result === "object"
        ? mcpCall.result as Record<string, unknown>
        : undefined;
      const success = result?.success && typeof result.success === "object"
        ? result.success as Record<string, unknown>
        : undefined;
      return event.type === "tool_call" &&
        event.subtype === "completed" &&
        args?.providerIdentifier === "usable-git" &&
        args.toolName === "inspect" &&
        success?.isError === false;
    } catch {
      return false;
    }
  });

const DEVIN_EXPORT_MAX_BYTES = 1_048_576;

const isDevinInspectToolUse = (value: unknown): boolean => {
  if (Array.isArray(value)) return value.some(isDevinInspectToolUse);
  if (!value || typeof value !== "object") return false;
  const record = value as Record<string, unknown>;
  const argumentsRecord = record.arguments && typeof record.arguments === "object"
    ? record.arguments as Record<string, unknown>
    : undefined;
  if (
    record.function_name === "mcp_call_tool" &&
    argumentsRecord?.server_name === "usable-git" &&
    argumentsRecord.tool_name === "inspect"
  ) return true;
  if (
    record.type === "tool_use" &&
    typeof record.name === "string" &&
    /^(?:mcp(?:__|[\s:/-])+)?usable-git(?:__|[\s:/-])+inspect$/i.test(record.name)
  ) return true;
  return Object.values(record).some(isDevinInspectToolUse);
};

const containsExportedDevinInspect = async (path: string) => {
  try {
    const artifact = Bun.file(path);
    if (!(await artifact.exists()) || artifact.size > DEVIN_EXPORT_MAX_BYTES) return false;
    return isDevinInspectToolUse(JSON.parse(await artifact.text()));
  } catch {
    return false;
  }
};

export const createDoctorClientInvoker = (): DoctorClientInvoker => async ({
  client,
  executablePath,
  home,
  repoPath,
  processRunner,
}) => {
  const command = defaultClientCommand(client);
  const version = await processRunner({ command, args: ["--version"], env: { HOME: home } });
  if (version.exitCode === 127) {
    return { available: false, invoked: false, reason: `${client} executable not found` };
  }
  if (version.exitCode !== 0) {
    return { available: false, invoked: false, reason: `${client} version check failed` };
  }

  const prompt = [
    `Call the configured MCP tool usable-git.inspect exactly once with arguments ${JSON.stringify({ repoPath })}.`,
    "Do not execute shell commands. Return only the operation name and success state.",
  ].join(" ");
  const devinExportPath = client === "devin"
    ? join(tmpdir(), `usable-git-doctor-devin-${crypto.randomUUID()}.json`)
    : undefined;
  const args = client === "codex"
    ? [
        "exec",
        "--json",
        "--ephemeral",
        "--ignore-user-config",
        "-c",
        `mcp_servers.usable-git.command=${JSON.stringify(executablePath)}`,
        "-c",
        'mcp_servers.usable-git.args=["mcp"]',
        "--sandbox",
        "read-only",
        "--skip-git-repo-check",
        "-C",
        repoPath,
        "-",
      ]
    : client === "claude"
      ? ["--output-format", "stream-json", "--verbose", "--permission-mode", "dontAsk", "-p", prompt]
      : client === "devin"
        ? [
            "--permission-mode",
            "dangerous",
            "--respect-workspace-trust",
            "false",
            "--export",
            devinExportPath as string,
            "-p",
            prompt,
          ]
        : [
            "-p",
            "--force",
            "--trust",
            "--approve-mcps",
            "--output-format",
            "stream-json",
            prompt,
          ];
  try {
    const result = await processRunner({
      command,
      args,
      cwd: repoPath,
      env: { HOME: home },
      ...(client === "codex" ? { stdin: prompt } : {}),
      timeoutMs: 120_000,
    });
    const diagnostic = bounded(`${result.stdout}\n${result.stderr}`);
    const output = `${result.stdout}\n${result.stderr}`;
    const invoked = client === "devin"
      ? result.exitCode === 0 && await containsExportedDevinInspect(devinExportPath as string)
      : containsCompletedCodexInspect(output) ||
        (client === "cursor"
          ? containsCompletedCursorInspect(output)
          : result.exitCode === 0 && containsSemanticInspectTrace(output));
    return {
      available: true,
      invoked,
      ...(invoked ? { operation: "inspect" as const, transport: "mcp" as const } : {}),
      diagnostic,
    };
  } finally {
    if (devinExportPath) await rm(devinExportPath, { force: true });
  }
};

const registrationCheck = async (
  client: InstallClient,
  executablePath: string,
  home: string,
  runner?: InstallRunner,
) => {
  const registration = client === "cursor"
    ? await inspectCursorRegistration({ executablePath, home })
    : await inspectNativeClientRegistration({
      client,
      executablePath,
      home,
      ...(runner ? { runner } : {}),
    });
  if (registration.state !== "matching") {
    throw new Error(`${client} registration is ${registration.state}`);
  }
  return {
    summary: `${client} stdio MCP registration matches executable`,
    details: { client, state: registration.state },
  };
};

export const runDoctor = async ({
  clients: selection,
  executablePath,
  home = process.env.HOME ?? tmpdir(),
  runner,
  processRunner = createDoctorProcessRunner(),
  clientInvoker = createDoctorClientInvoker(),
}: RunDoctorOptions): Promise<DoctorReport> => {
  const clients = normalizeClients(selection);
  const checks: DoctorCheck[] = [];
  checks.push(await check("runtime.bun", async () => {
    const result = await processRunner({ command: process.execPath, args: ["--version"] });
    const version = requireSuccess(result, "bun --version").trim();
    if (!/^\d+\.\d+\.\d+/.test(version)) throw new Error(`Invalid Bun version: ${version}`);
    return { summary: `Bun ${version} available`, details: { version } };
  }));
  checks.push(await check("runtime.git", async () => {
    const result = await processRunner({ command: "git", args: ["--version"] });
    const output = requireSuccess(result, "git --version").trim();
    const version = output.replace(/^git version\s+/, "");
    if (!/^\d+\.\d+/.test(version)) throw new Error(`Invalid Git version: ${output}`);
    return { summary: `Git ${version} available`, details: { version } };
  }));

  let fixture: Fixture | undefined;
  let fixtureError: unknown;
  try {
    fixture = await createFixture(processRunner, home);
  } catch (error) {
    fixtureError = error;
  }

  const withFixture = <T>(run: (value: Fixture) => Promise<T>) => {
    if (!fixture) throw fixtureError ?? new Error("Unable to create doctor repository fixture");
    return run(fixture);
  };

  let inspected: Record<string, unknown> | undefined;
  checks.push(await check("cli.inspect", () => withFixture(async (active) => {
    const envelope = await invokeCli(
      processRunner,
      executablePath,
      "inspect",
      { repoPath: active.repoPath },
      active.env,
    );
    inspected = inspectResult(envelope);
    return {
      summary: "Direct JSON CLI inspect returned one successful v1 envelope",
      details: { transport: "cli", operation: "inspect" },
    };
  })));
  checks.push(await check("mcp.protocol", () => withFixture(
    (active) => mcpCheck(executablePath, active),
  )));
  checks.push(await check("repository.publish-preservation", () => withFixture(async (active) => {
    const snapshot = inspected ?? inspectResult(await invokeCli(
      processRunner,
      executablePath,
      "inspect",
      { repoPath: active.repoPath },
      active.env,
    ));
    const expectation = inspectRequestForPublish(snapshot);
    const stagedBefore = await git(processRunner, active.repoPath, ["ls-files", "--stage", "--", "unrelated-staged.txt"], active.env);
    const envelope = await invokeCli(processRunner, executablePath, "publish", {
      repoPath: active.repoPath,
      files: ["selected.txt"],
      message: "doctor exact-path publish",
      requestId: `doctor-publish-${crypto.randomUUID()}`,
      snapshot: expectation.snapshot,
    }, active.env);
    const result = envelope.result as Record<string, unknown>;
    const committedPaths = result?.committedPaths;
    const stagedAfter = await git(processRunner, active.repoPath, ["ls-files", "--stage", "--", "unrelated-staged.txt"], active.env);
    const commitPaths = await git(processRunner, active.repoPath, ["diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD"], active.env);
    const unstaged = await Bun.file(join(active.repoPath, "unrelated-unstaged.txt")).text();
    const untracked = await Bun.file(join(active.repoPath, "unrelated-untracked.txt")).text();
    const fsck = await git(processRunner, active.repoPath, ["fsck", "--strict"], active.env);
    if (
      JSON.stringify(committedPaths) !== JSON.stringify(["selected.txt"]) ||
      commitPaths !== "selected.txt\n" ||
      stagedAfter !== stagedBefore ||
      unstaged !== "unstaged pending\n" ||
      untracked !== "untracked pending\n" ||
      fsck !== ""
    ) {
      throw new Error("Publish did not preserve exact unrelated repository state");
    }
    return {
      summary: "Dirty-repository publish committed one path and preserved unrelated work",
      details: { fsck: true, committedPaths: ["selected.txt"], unrelatedStagedPreserved: true },
    };
  })));
  checks.push(await check("repository.push-one-ref", () => withFixture(async (active) => {
    const sourceOid = (await git(processRunner, active.repoPath, ["rev-parse", "refs/heads/main"], active.env)).trim();
    const envelope = await invokeCli(processRunner, executablePath, "push", {
      repoPath: active.repoPath,
      remote: "doctor-origin",
      sourceRef: "refs/heads/main",
      targetRef: "refs/heads/main",
      requestId: `doctor-push-${crypto.randomUUID()}`,
      expectedSourceOid: sourceOid,
      mode: { kind: "fast-forward" },
    }, active.env);
    const result = envelope.result as Record<string, unknown>;
    const refs = (await git(processRunner, active.remotePath, ["for-each-ref", "--format=%(refname)", "refs/heads"], active.env))
      .split(/\r?\n/)
      .filter(Boolean);
    const remoteOid = (await git(processRunner, active.remotePath, ["rev-parse", "refs/heads/main"], active.env)).trim();
    await git(processRunner, active.repoPath, ["fsck", "--strict"], active.env);
    await git(processRunner, active.remotePath, ["fsck", "--strict"], active.env);
    if (
      refs.length !== 1 ||
      refs[0] !== "refs/heads/main" ||
      remoteOid !== sourceOid ||
      result?.newTargetOid !== sourceOid
    ) {
      throw new Error("Push did not update exactly refs/heads/main");
    }
    return {
      summary: "Local bare push updated exactly one branch and both repositories passed fsck",
      details: { fsck: true, refCount: 1, targetRef: "refs/heads/main" },
    };
  })));

  const activatedClients: InstallClient[] = [];
  for (const client of clients) {
    checks.push(await check(
      `client.${client}.registration`,
      () => registrationCheck(client, executablePath, home, runner),
    ));
    if (!fixture) {
      checks.push(await check(`client.${client}.fresh-session`, async () => {
        throw fixtureError ?? new Error("Unable to create doctor repository fixture");
      }));
      continue;
    }
    const startedAt = performance.now();
    try {
      const invocation = await clientInvoker({
        client,
        executablePath,
        home,
        repoPath: fixture.repoPath,
        processRunner,
      });
      if (!invocation.available) {
        checks.push(skip(`client.${client}.fresh-session`, invocation.reason));
      } else if (
        invocation.invoked &&
        invocation.operation === "inspect" &&
        invocation.transport === "mcp"
      ) {
        activatedClients.push(client);
        checks.push({
          id: `client.${client}.fresh-session`,
          status: "pass",
          required: true,
          durationMs: performance.now() - startedAt,
          summary: `${client} fresh session invoked usable-git inspect over MCP`,
          details: { client, operation: "inspect", transport: "mcp" },
        });
      } else {
        throw new Error(invocation.diagnostic || `${client} did not emit semantic inspect evidence`);
      }
    } catch (error) {
      checks.push({
        id: `client.${client}.fresh-session`,
        status: "fail",
        required: true,
        durationMs: performance.now() - startedAt,
        summary: `${client} fresh-session semantic invocation failed`,
        reason: bounded(error instanceof Error ? error.message : String(error)),
      });
    }
  }

  if (fixture) await rm(fixture.directory, { recursive: true, force: true });
  const summary = {
    passed: checks.filter(({ status }) => status === "pass").length,
    failed: checks.filter(({ status }) => status === "fail").length,
    skipped: checks.filter(({ status }) => status === "skip").length,
  };
  return {
    version: "v1",
    ok: checks.every(({ status, required }) => !required || status === "pass"),
    clients,
    activatedClients,
    executablePath,
    checks,
    summary,
  };
};

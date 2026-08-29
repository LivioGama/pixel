import { mkdir, open, readFile, rename, rm } from "node:fs/promises";
import { homedir } from "node:os";
import { dirname, isAbsolute, join } from "node:path";

import {
  InstallConfigError,
  InstallConflictError,
  type ClientRegistrationState,
} from "@usable-git/install/cursor.ts";

const SERVER_NAME = "usable-git";

export type NativeClient = "codex" | "claude" | "devin";
export type NativeInstallStatus = "installed" | "replaced" | "unchanged";

export type InstallRunnerRequest = {
  command: string;
  args: string[];
  env: Record<string, string>;
};

export type InstallRunnerResult = {
  exitCode: number;
  stdout: string;
  stderr: string;
};

export type InstallRunner = (request: InstallRunnerRequest) => Promise<InstallRunnerResult>;

export type InstallNativeClientOptions = {
  client: NativeClient;
  executablePath: string;
  force?: boolean;
  home?: string;
  runner?: InstallRunner;
};

export type InspectNativeClientOptions = Omit<InstallNativeClientOptions, "force">;

export type NativeInstallResult = {
  client: NativeClient;
  status: NativeInstallStatus;
};

export class InstallCommandError extends Error {
  readonly code = "client_registration_failed";
  readonly client: NativeClient;
  readonly exitCode: number | null;

  constructor(client: NativeClient, exitCode: number | null, cause?: unknown) {
    super(`${client} native MCP registration command failed`, { cause });
    this.name = "InstallCommandError";
    this.client = client;
    this.exitCode = exitCode;
  }
}

type JsonObject = Record<string, unknown>;

const isJsonObject = (value: unknown): value is JsonObject =>
  typeof value === "object" && value !== null && !Array.isArray(value);

const isMatchingEntry = (value: unknown, executablePath: string) =>
  isJsonObject(value) &&
  value.command === executablePath &&
  Array.isArray(value.args) &&
  value.args.length === 1 &&
  value.args[0] === "mcp" &&
  (value.type === undefined || value.type === "stdio") &&
  (value.transport === undefined || value.transport === "stdio");

const decodeOutput = async (stream: ReadableStream<Uint8Array> | null) =>
  stream ? new TextDecoder().decode(await new Response(stream).arrayBuffer()) : "";

export const createInstallRunner = (): InstallRunner => async ({ command, args, env }) => {
  const environment = { ...process.env, ...env };
  delete environment.ANTHROPIC_API_KEY;
  const child = Bun.spawn([command, ...args], {
    env: environment,
    stdin: "ignore",
    stdout: "pipe",
    stderr: "pipe",
  });
  const [exitCode, stdout, stderr] = await Promise.all([
    child.exited,
    decodeOutput(child.stdout),
    decodeOutput(child.stderr),
  ]);
  return { exitCode, stdout, stderr };
};

const run = async (
  client: NativeClient,
  runner: InstallRunner,
  request: InstallRunnerRequest,
) => {
  try {
    return await runner(request);
  } catch (error) {
    throw new InstallCommandError(client, null, error);
  }
};

const runChecked = async (
  client: NativeClient,
  runner: InstallRunner,
  request: InstallRunnerRequest,
) => {
  const result = await run(client, runner, request);
  if (result.exitCode !== 0) {
    throw new InstallCommandError(client, result.exitCode);
  }
  return result;
};

const resolveEnvironment = (
  client: NativeClient,
  home: string | undefined,
): Record<string, string> => {
  if (home === undefined) {
    return {};
  }
  if (!isAbsolute(home)) {
    throw new InstallConfigError("home path must be absolute");
  }
  if (client === "codex") {
    return { CODEX_HOME: join(home, ".codex"), HOME: home };
  }
  if (client === "devin") {
    return { HOME: home, XDG_CONFIG_HOME: join(home, ".config") };
  }
  return { HOME: home };
};

const resolveActualHome = (home: string | undefined) =>
  home ?? process.env.HOME ?? homedir();

const resolveJsonConfigPath = (client: "claude" | "devin", home: string | undefined) => {
  const actualHome = resolveActualHome(home);
  if (client === "claude") {
    return join(actualHome, ".claude.json");
  }
  const configRoot = home === undefined && process.env.XDG_CONFIG_HOME
    ? process.env.XDG_CONFIG_HOME
    : join(actualHome, ".config");
  // Devin CLI keeps MCP server registrations in a dedicated mcp_config.json,
  // separate from its main config.json (which only holds an mcp.enabled flag).
  return join(configRoot, "devin", "mcp_config.json");
};

const readJsonExistingState = async (
  client: "claude" | "devin",
  executablePath: string,
  home: string | undefined,
): Promise<ClientRegistrationState> => {
  const configPath = resolveJsonConfigPath(client, home);
  if (!(await Bun.file(configPath).exists())) {
    return "missing";
  }

  let parsed: unknown;
  try {
    parsed = JSON.parse(await readFile(configPath, "utf8"));
  } catch (error) {
    throw new InstallConfigError(`${client} MCP config is not valid JSON: ${configPath}`, error);
  }
  if (!isJsonObject(parsed)) {
    throw new InstallConfigError(`${client} MCP config must contain a JSON object: ${configPath}`);
  }
  const configuredServers = parsed.mcpServers;
  if (configuredServers === undefined) {
    return "missing";
  }
  if (!isJsonObject(configuredServers)) {
    throw new InstallConfigError(`${client} mcpServers must contain a JSON object: ${configPath}`);
  }
  const existing = configuredServers[SERVER_NAME];
  if (existing === undefined) {
    return "missing";
  }
  return isMatchingEntry(existing, executablePath) ? "matching" : "conflicting";
};

const readCodexExistingState = async (
  executablePath: string,
  runner: InstallRunner,
  env: Record<string, string>,
): Promise<ClientRegistrationState> => {
  const result = await run("codex", runner, {
    command: "codex",
    args: ["mcp", "get", SERVER_NAME, "--json"],
    env,
  });
  if (result.exitCode !== 0) {
    const diagnostic = `${result.stdout}\n${result.stderr}`;
    if (/No MCP server named ['"]usable-git['"] found\.?/i.test(diagnostic)) {
      return "missing";
    }
    throw new InstallCommandError("codex", result.exitCode);
  }

  let parsed: unknown;
  try {
    parsed = JSON.parse(result.stdout);
  } catch (error) {
    throw new InstallConfigError("codex mcp get returned invalid JSON", error);
  }
  if (!isJsonObject(parsed) || !isJsonObject(parsed.transport)) {
    throw new InstallConfigError("codex mcp get returned an invalid server configuration");
  }
  return isMatchingEntry(parsed.transport, executablePath) ? "matching" : "conflicting";
};

const addArguments = (client: NativeClient, executablePath: string) => {
  if (client === "codex") {
    return ["mcp", "add", SERVER_NAME, "--", executablePath, "mcp"];
  }
  if (client === "claude") {
    return [
      "mcp",
      "add",
      "--scope",
      "user",
      "--transport",
      "stdio",
      SERVER_NAME,
      "--",
      executablePath,
      "mcp",
    ];
  }
  return ["mcp", "add", "--scope", "user", SERVER_NAME, "--", executablePath, "mcp"];
};

const clientCommand = (client: NativeClient) => client;

const restoreFileAtomically = async (path: string, contents: Uint8Array) => {
  const directory = dirname(path);
  await mkdir(directory, { recursive: true });
  const temporaryPath = join(directory, `.usable-git-restore.${process.pid}.${crypto.randomUUID()}`);
  let handle: Awaited<ReturnType<typeof open>> | undefined;
  try {
    handle = await open(temporaryPath, "wx", 0o600);
    await handle.writeFile(contents);
    await handle.sync();
    await handle.close();
    handle = undefined;
    await rename(temporaryPath, path);
  } catch (error) {
    await handle?.close().catch(() => undefined);
    await rm(temporaryPath, { force: true }).catch(() => undefined);
    throw new InstallConfigError(`unable to restore client configuration after failed registration: ${path}`, error);
  }
};

const replaceClaude = async (
  executablePath: string,
  home: string | undefined,
  env: Record<string, string>,
  runner: InstallRunner,
) => {
  const configPath = resolveJsonConfigPath("claude", home);
  const original = new Uint8Array(await readFile(configPath));
  await runChecked("claude", runner, {
    command: "claude",
    args: ["mcp", "remove", SERVER_NAME, "--scope", "user"],
    env,
  });
  const postRemove = (await Bun.file(configPath).exists())
    ? new Uint8Array(await readFile(configPath))
    : undefined;

  try {
    await runChecked("claude", runner, {
      command: "claude",
      args: addArguments("claude", executablePath),
      env,
    });
  } catch (error) {
    const current = (await Bun.file(configPath).exists())
      ? new Uint8Array(await readFile(configPath))
      : undefined;
    const unchangedAfterRemove =
      (current === undefined && postRemove === undefined) ||
      (current !== undefined &&
        postRemove !== undefined &&
        current.byteLength === postRemove.byteLength &&
        current.every((byte, index) => byte === postRemove[index]));
    if (!unchangedAfterRemove) {
      throw new InstallConfigError(
        `Claude config changed during failed replacement; refusing unsafe rollback: ${configPath}`,
        error,
      );
    }
    await restoreFileAtomically(configPath, original);
    throw error;
  }
};

export const inspectNativeClientRegistration = async ({
  client,
  executablePath,
  home,
  runner = createInstallRunner(),
}: InspectNativeClientOptions) => {
  if (!isAbsolute(executablePath)) {
    throw new InstallConfigError("usable-git executable path must be absolute");
  }
  const env = resolveEnvironment(client, home);
  const state = client === "codex"
    ? await readCodexExistingState(executablePath, runner, env)
    : await readJsonExistingState(client, executablePath, home);
  return { client, state };
};

export const installNativeClient = async ({
  client,
  executablePath,
  force = false,
  home,
  runner = createInstallRunner(),
}: InstallNativeClientOptions): Promise<NativeInstallResult> => {
  const env = resolveEnvironment(client, home);
  const { state: existingState } = await inspectNativeClientRegistration({
    client,
    executablePath,
    home,
    runner,
  });

  if (existingState === "matching") {
    return { client, status: "unchanged" };
  }
  if (existingState === "conflicting" && !force) {
    throw new InstallConflictError(client);
  }

  if (client === "claude" && existingState === "conflicting") {
    await replaceClaude(executablePath, home, env, runner);
  } else {
    await runChecked(client, runner, {
      command: clientCommand(client),
      args: addArguments(client, executablePath),
      env,
    });
  }

  return {
    client,
    status: existingState === "missing" ? "installed" : "replaced",
  };
};

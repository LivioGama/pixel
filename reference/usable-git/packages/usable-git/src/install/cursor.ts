import {
  chmod,
  mkdir,
  open,
  readFile,
  rename,
  rm,
  stat,
} from "node:fs/promises";
import { homedir } from "node:os";
import { dirname, isAbsolute, join } from "node:path";

const SERVER_NAME = "usable-git";

type JsonObject = Record<string, unknown>;

export type CursorInstallStatus = "installed" | "replaced" | "unchanged";
export type ClientRegistrationState = "missing" | "matching" | "conflicting";

export type CursorInstallResult = {
  client: "cursor";
  configPath: string;
  status: CursorInstallStatus;
};

export type InstallCursorOptions = {
  executablePath: string;
  force?: boolean;
  home?: string;
};

export type InspectCursorOptions = Omit<InstallCursorOptions, "force">;

export class InstallConflictError extends Error {
  readonly code = "install_conflict";

  constructor(client: string) {
    super(`${client} already has a conflicting ${SERVER_NAME} MCP entry; rerun with force to replace it`);
    this.name = "InstallConflictError";
  }
}

export class InstallConfigError extends Error {
  readonly code = "invalid_client_config";

  constructor(message: string, cause?: unknown) {
    super(message, { cause });
    this.name = "InstallConfigError";
  }
}

const isJsonObject = (value: unknown): value is JsonObject =>
  typeof value === "object" && value !== null && !Array.isArray(value);

const isMatchingEntry = (value: unknown, executablePath: string) => {
  if (!isJsonObject(value) || value.command !== executablePath || !Array.isArray(value.args)) {
    return false;
  }

  return value.args.length === 1 && value.args[0] === "mcp";
};

const readConfig = async (configPath: string) => {
  if (!(await Bun.file(configPath).exists())) {
    return { config: {} as JsonObject, mode: 0o600 };
  }

  const source = await readFile(configPath, "utf8");
  try {
    const parsed: unknown = JSON.parse(source);
    if (!isJsonObject(parsed)) {
      throw new InstallConfigError(`Cursor MCP config must contain a JSON object: ${configPath}`);
    }
    const fileStat = await stat(configPath);
    return { config: parsed, mode: fileStat.mode & 0o777 };
  } catch (error) {
    if (error instanceof InstallConfigError) {
      throw error;
    }
    throw new InstallConfigError(`Cursor MCP config is not valid JSON: ${configPath}`, error);
  }
};

const writeConfigAtomically = async (configPath: string, config: JsonObject, mode: number) => {
  const configDirectory = dirname(configPath);
  await mkdir(configDirectory, { recursive: true, mode: 0o700 });
  const tempPath = join(
    configDirectory,
    `.mcp.json.${process.pid}.${crypto.randomUUID()}.tmp`,
  );
  let handle: Awaited<ReturnType<typeof open>> | undefined;

  try {
    handle = await open(tempPath, "wx", mode);
    await handle.writeFile(`${JSON.stringify(config, null, 2)}\n`, "utf8");
    await handle.sync();
    await handle.close();
    handle = undefined;
    await chmod(tempPath, mode);
    await rename(tempPath, configPath);

    try {
      const directoryHandle = await open(configDirectory, "r");
      await directoryHandle.sync();
      await directoryHandle.close();
    } catch {
      // The rename remains atomic on platforms that do not allow syncing directories.
    }
  } catch (error) {
    await handle?.close().catch(() => undefined);
    await rm(tempPath, { force: true }).catch(() => undefined);
    throw new InstallConfigError(`Unable to atomically update Cursor MCP config: ${configPath}`, error);
  }
};

const validatePaths = (executablePath: string, home: string) => {
  if (!isAbsolute(executablePath)) {
    throw new InstallConfigError("usable-git executable path must be absolute");
  }
  if (!isAbsolute(home)) {
    throw new InstallConfigError("home path must be absolute");
  }
};

const inspectCursorDetails = async (executablePath: string, home: string) => {
  validatePaths(executablePath, home);
  const configPath = join(home, ".cursor", "mcp.json");
  const { config, mode } = await readConfig(configPath);
  const configuredServers = config.mcpServers;
  if (configuredServers !== undefined && !isJsonObject(configuredServers)) {
    throw new InstallConfigError(`Cursor mcpServers must contain a JSON object: ${configPath}`);
  }

  const mcpServers = configuredServers ?? {};
  const existing = mcpServers[SERVER_NAME];
  const state: ClientRegistrationState = existing === undefined
    ? "missing"
    : isMatchingEntry(existing, executablePath)
      ? "matching"
      : "conflicting";
  return { config, configPath, mcpServers, mode, state };
};

export const inspectCursorRegistration = async ({
  executablePath,
  home = homedir(),
}: InspectCursorOptions) => {
  const { configPath, state } = await inspectCursorDetails(executablePath, home);
  return { client: "cursor" as const, configPath, state };
};

export const installCursor = async ({
  executablePath,
  force = false,
  home = homedir(),
}: InstallCursorOptions): Promise<CursorInstallResult> => {
  const { config, configPath, mcpServers, mode, state } = await inspectCursorDetails(
    executablePath,
    home,
  );
  if (state === "matching") {
    return { client: "cursor", configPath, status: "unchanged" };
  }
  if (state === "conflicting" && !force) {
    throw new InstallConflictError("Cursor");
  }

  const status: CursorInstallStatus = state === "missing" ? "installed" : "replaced";
  const nextConfig: JsonObject = {
    ...config,
    mcpServers: {
      ...mcpServers,
      [SERVER_NAME]: {
        command: executablePath,
        args: ["mcp"],
      },
    },
  };
  await writeConfigAtomically(configPath, nextConfig, mode);
  return { client: "cursor", configPath, status };
};

import {
  InstallConfigError,
  InstallConflictError,
  inspectCursorRegistration,
  installCursor,
  type CursorInstallResult,
} from "@usable-git/install/cursor.ts";
import {
  inspectNativeClientRegistration,
  installNativeClient,
  type InstallRunner,
  type NativeClient,
  type NativeInstallResult,
} from "@usable-git/install/native.ts";

const CLIENT_ORDER = ["codex", "claude", "cursor", "devin"] as const;

export type InstallClient = (typeof CLIENT_ORDER)[number];
export type InstallClientSelection = "all" | InstallClient[];
export type InstallResult = CursorInstallResult | NativeInstallResult;

export type InstallClientsOptions = {
  clients: InstallClientSelection;
  executablePath: string;
  force?: boolean;
  home?: string;
  runner?: InstallRunner;
};

const isInstallClient = (value: unknown): value is InstallClient =>
  typeof value === "string" && CLIENT_ORDER.includes(value as InstallClient);

const normalizeClients = (selection: InstallClientSelection) => {
  if (selection === "all") {
    return [...CLIENT_ORDER];
  }
  if (!Array.isArray(selection) || selection.length === 0) {
    throw new InstallConfigError("client selection must include at least one client");
  }
  if (!selection.every(isInstallClient)) {
    throw new InstallConfigError("client selection contains an unsupported client");
  }
  if (new Set(selection).size !== selection.length) {
    throw new InstallConfigError("client selection contains a duplicate client");
  }
  const selected = new Set(selection);
  return CLIENT_ORDER.filter((client) => selected.has(client));
};

export const installClients = async ({
  clients,
  executablePath,
  force = false,
  home,
  runner,
}: InstallClientsOptions): Promise<InstallResult[]> => {
  const selectedClients = normalizeClients(clients);
  const inspections = await Promise.all(
    selectedClients.map((client) =>
      client === "cursor"
        ? inspectCursorRegistration({ executablePath, home })
        : inspectNativeClientRegistration({
          client: client as NativeClient,
          executablePath,
          home,
          runner,
        })
    ),
  );
  const conflict = inspections.find(({ state }) => state === "conflicting");
  if (conflict && !force) {
    throw new InstallConflictError(conflict.client);
  }

  const results: InstallResult[] = [];
  for (const client of selectedClients) {
    results.push(
      client === "cursor"
        ? await installCursor({ executablePath, force, home })
        : await installNativeClient({
          client: client as NativeClient,
          executablePath,
          force,
          home,
          runner,
        }),
    );
  }
  return results;
};

export {
  InstallCommandError,
  createInstallRunner,
  type InstallRunner,
  type InstallRunnerRequest,
} from "@usable-git/install/native.ts";
export {
  InstallConfigError,
  InstallConflictError,
} from "@usable-git/install/cursor.ts";

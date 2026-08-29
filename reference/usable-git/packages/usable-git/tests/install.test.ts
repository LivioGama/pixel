import { afterEach, describe, expect, test } from "bun:test";
import { mkdtemp, mkdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { InstallConflictError } from "@usable-git/install/cursor.ts";
import { installClients } from "@usable-git/install/index.ts";
import type { InstallRunner, InstallRunnerRequest } from "@usable-git/install/native.ts";

const homes: string[] = [];
const executablePath = "/opt/homebrew/bin/usable-git";

const createHome = async () => {
  const home = await mkdtemp(join(tmpdir(), "usable-git-install-all-"));
  homes.push(home);
  return home;
};

const createMissingRunner = () => {
  const calls: InstallRunnerRequest[] = [];
  const runner: InstallRunner = async (request) => {
    calls.push(structuredClone(request));
    if (request.command === "codex" && request.args[1] === "get") {
      return {
        exitCode: 1,
        stderr: "Error: No MCP server named 'usable-git' found.",
        stdout: "",
      };
    }
    return { exitCode: 0, stderr: "", stdout: "" };
  };
  return { calls, runner };
};

afterEach(async () => {
  await Promise.all(homes.splice(0).map((home) => rm(home, { recursive: true, force: true })));
});

describe("installClients", () => {
  test("installs all clients in stable order", async () => {
    const home = await createHome();
    const fake = createMissingRunner();

    const result = await installClients({
      clients: "all",
      executablePath,
      home,
      runner: fake.runner,
    });

    expect(result.map(({ client, status }) => ({ client, status }))).toEqual([
      { client: "codex", status: "installed" },
      { client: "claude", status: "installed" },
      { client: "cursor", status: "installed" },
      { client: "devin", status: "installed" },
    ]);
    expect(fake.calls.filter(({ args }) => args[1] === "add").map(({ command }) => command)).toEqual([
      "codex",
      "claude",
      "devin",
    ]);
    expect(JSON.parse(await Bun.file(join(home, ".cursor", "mcp.json")).text())).toEqual({
      mcpServers: {
        "usable-git": { command: executablePath, args: ["mcp"] },
      },
    });
  });

  test("installs only an explicit unique client subset", async () => {
    const home = await createHome();
    const fake = createMissingRunner();

    const result = await installClients({
      clients: ["cursor", "codex"],
      executablePath,
      home,
      runner: fake.runner,
    });

    expect(result.map(({ client }) => client)).toEqual(["codex", "cursor"]);
    expect(fake.calls.some(({ command }) => command === "claude" || command === "devin")).toBe(false);
  });

  test("preflights every selected config before the first mutation", async () => {
    const home = await createHome();
    const fake = createMissingRunner();
    await mkdir(join(home), { recursive: true });
    await writeFile(
      join(home, ".claude.json"),
      `${JSON.stringify({
        sentinel: "preserve",
        mcpServers: { "usable-git": { command: "/tmp/wrong", args: ["mcp"] } },
      })}\n`,
    );

    await expect(
      installClients({ clients: "all", executablePath, home, runner: fake.runner }),
    ).rejects.toBeInstanceOf(InstallConflictError);

    expect(fake.calls.filter(({ args }) => args[1] === "add")).toHaveLength(0);
    expect(await Bun.file(join(home, ".cursor", "mcp.json")).exists()).toBe(false);
  });

  test("rejects duplicate and empty client selections before doing work", async () => {
    const home = await createHome();
    const fake = createMissingRunner();

    await expect(
      installClients({ clients: ["codex", "codex"], executablePath, home, runner: fake.runner }),
    ).rejects.toThrow("duplicate");
    await expect(
      installClients({ clients: [], executablePath, home, runner: fake.runner }),
    ).rejects.toThrow("at least one");
    expect(fake.calls).toHaveLength(0);
  });
});

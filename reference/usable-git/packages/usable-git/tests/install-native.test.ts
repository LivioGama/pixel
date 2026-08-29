import { afterEach, describe, expect, test } from "bun:test";
import { mkdtemp, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { InstallConflictError } from "@usable-git/install/cursor.ts";
import {
  createInstallRunner,
  InstallCommandError,
  installNativeClient,
  type InstallRunner,
  type InstallRunnerRequest,
} from "@usable-git/install/native.ts";

const homes: string[] = [];
const executablePath = "/opt/homebrew/bin/usable-git";

const createHome = async () => {
  const home = await mkdtemp(join(tmpdir(), "usable-git-native-install-"));
  homes.push(home);
  return home;
};

const createRunner = (
  implementation?: (request: InstallRunnerRequest) => Promise<{
    exitCode: number;
    stderr?: string;
    stdout?: string;
  }>,
) => {
  const calls: InstallRunnerRequest[] = [];
  const runner: InstallRunner = async (request) => {
    calls.push(structuredClone(request));
    return {
      exitCode: 0,
      stderr: "",
      stdout: "",
      ...(implementation ? await implementation(request) : {}),
    };
  };
  return { calls, runner };
};

const writeClaudeConfig = async (home: string, entry: unknown) => {
  const path = join(home, ".claude.json");
  await writeFile(path, `${JSON.stringify({ sentinel: "keep", mcpServers: { "usable-git": entry } }, null, 2)}\n`);
  return path;
};

test("never forwards a banned ANTHROPIC_API_KEY to client subprocesses", async () => {
  const previous = process.env.ANTHROPIC_API_KEY;
  process.env.ANTHROPIC_API_KEY = "banned-test-value";
  try {
    const result = await createInstallRunner()({
      command: "printenv",
      args: ["ANTHROPIC_API_KEY"],
      env: {},
    });
    expect(result.exitCode).toBe(1);
    expect(result.stdout).toBe("");
  } finally {
    if (previous === undefined) delete process.env.ANTHROPIC_API_KEY;
    else process.env.ANTHROPIC_API_KEY = previous;
  }
});

const writeDevinConfig = async (home: string, entry: unknown) => {
  const path = join(home, ".config", "devin", "mcp_config.json");
  await mkdir(join(home, ".config", "devin"), { recursive: true });
  await writeFile(path, `${JSON.stringify({ sentinel: "keep", mcpServers: { "usable-git": entry } }, null, 2)}\n`);
  return path;
};

afterEach(async () => {
  await Promise.all(homes.splice(0).map((home) => rm(home, { recursive: true, force: true })));
});

describe("installNativeClient", () => {
  test("uses exact installed native registration argv for every supported client", async () => {
    const codexHome = await createHome();
    const claudeHome = await createHome();
    const devinHome = await createHome();
    const codex = createRunner(async (request) =>
      request.args[1] === "get"
        ? { exitCode: 1, stderr: "No MCP server named 'usable-git' found." }
        : { exitCode: 0 },
    );
    const claude = createRunner();
    const devin = createRunner();

    expect(
      await installNativeClient({
        client: "codex",
        executablePath,
        home: codexHome,
        runner: codex.runner,
      }),
    ).toMatchObject({ client: "codex", status: "installed" });
    expect(
      await installNativeClient({
        client: "claude",
        executablePath,
        home: claudeHome,
        runner: claude.runner,
      }),
    ).toMatchObject({ client: "claude", status: "installed" });
    expect(
      await installNativeClient({
        client: "devin",
        executablePath,
        home: devinHome,
        runner: devin.runner,
      }),
    ).toMatchObject({ client: "devin", status: "installed" });

    expect(codex.calls).toEqual([
      {
        args: ["mcp", "get", "usable-git", "--json"],
        command: "codex",
        env: { CODEX_HOME: join(codexHome, ".codex"), HOME: codexHome },
      },
      {
        args: ["mcp", "add", "usable-git", "--", executablePath, "mcp"],
        command: "codex",
        env: { CODEX_HOME: join(codexHome, ".codex"), HOME: codexHome },
      },
    ]);
    expect(claude.calls).toEqual([
      {
        args: [
          "mcp",
          "add",
          "--scope",
          "user",
          "--transport",
          "stdio",
          "usable-git",
          "--",
          executablePath,
          "mcp",
        ],
        command: "claude",
        env: { HOME: claudeHome },
      },
    ]);
    expect(devin.calls).toEqual([
      {
        args: ["mcp", "add", "--scope", "user", "usable-git", "--", executablePath, "mcp"],
        command: "devin",
        env: { HOME: devinHome, XDG_CONFIG_HOME: join(devinHome, ".config") },
      },
    ]);
  });

  test("does not invoke add when native configuration already matches", async () => {
    const codexHome = await createHome();
    const claudeHome = await createHome();
    const devinHome = await createHome();
    const codex = createRunner(async () => ({
      exitCode: 0,
      stdout: JSON.stringify({
        transport: { type: "stdio", command: executablePath, args: ["mcp"], env: null },
      }),
    }));
    const claude = createRunner();
    const devin = createRunner();
    const claudePath = await writeClaudeConfig(claudeHome, {
      type: "stdio",
      command: executablePath,
      args: ["mcp"],
      env: {},
    });
    const devinPath = await writeDevinConfig(devinHome, {
      command: executablePath,
      args: ["mcp"],
      transport: "stdio",
    });
    const claudeBefore = await readFile(claudePath, "utf8");
    const devinBefore = await readFile(devinPath, "utf8");

    const results = await Promise.all([
      installNativeClient({ client: "codex", executablePath, home: codexHome, runner: codex.runner }),
      installNativeClient({ client: "claude", executablePath, home: claudeHome, runner: claude.runner }),
      installNativeClient({ client: "devin", executablePath, home: devinHome, runner: devin.runner }),
    ]);

    expect(results.map(({ status }) => status)).toEqual(["unchanged", "unchanged", "unchanged"]);
    expect(codex.calls).toHaveLength(1);
    expect(claude.calls).toHaveLength(0);
    expect(devin.calls).toHaveLength(0);
    expect(await readFile(claudePath, "utf8")).toBe(claudeBefore);
    expect(await readFile(devinPath, "utf8")).toBe(devinBefore);
  });

  test("rejects every conflicting native entry before mutation", async () => {
    const codexHome = await createHome();
    const claudeHome = await createHome();
    const devinHome = await createHome();
    const codex = createRunner(async () => ({
      exitCode: 0,
      stdout: JSON.stringify({ transport: { type: "stdio", command: "/tmp/wrong", args: ["mcp"] } }),
    }));
    const claude = createRunner();
    const devin = createRunner();
    const claudePath = await writeClaudeConfig(claudeHome, { command: "/tmp/wrong", args: ["mcp"] });
    const devinPath = await writeDevinConfig(devinHome, { command: "/tmp/wrong", args: ["mcp"] });
    const claudeBefore = await readFile(claudePath, "utf8");
    const devinBefore = await readFile(devinPath, "utf8");

    for (const request of [
      { client: "codex" as const, home: codexHome, runner: codex.runner },
      { client: "claude" as const, home: claudeHome, runner: claude.runner },
      { client: "devin" as const, home: devinHome, runner: devin.runner },
    ]) {
      await expect(installNativeClient({ ...request, executablePath })).rejects.toBeInstanceOf(
        InstallConflictError,
      );
    }

    expect(codex.calls).toHaveLength(1);
    expect(claude.calls).toHaveLength(0);
    expect(devin.calls).toHaveLength(0);
    expect(await readFile(claudePath, "utf8")).toBe(claudeBefore);
    expect(await readFile(devinPath, "utf8")).toBe(devinBefore);
  });

  test("force uses native replacement commands and preserves exact argv", async () => {
    const codexHome = await createHome();
    const claudeHome = await createHome();
    const devinHome = await createHome();
    const codex = createRunner(async (request) =>
      request.args[1] === "get"
        ? { exitCode: 0, stdout: JSON.stringify({ transport: { command: "/tmp/wrong", args: [] } }) }
        : { exitCode: 0 },
    );
    const claude = createRunner();
    const devin = createRunner();
    await writeClaudeConfig(claudeHome, { command: "/tmp/wrong", args: [] });
    await writeDevinConfig(devinHome, { command: "/tmp/wrong", args: [] });

    const results = await Promise.all([
      installNativeClient({ client: "codex", executablePath, force: true, home: codexHome, runner: codex.runner }),
      installNativeClient({ client: "claude", executablePath, force: true, home: claudeHome, runner: claude.runner }),
      installNativeClient({ client: "devin", executablePath, force: true, home: devinHome, runner: devin.runner }),
    ]);

    expect(results.map(({ status }) => status)).toEqual(["replaced", "replaced", "replaced"]);
    expect(codex.calls.at(-1)?.args).toEqual(["mcp", "add", "usable-git", "--", executablePath, "mcp"]);
    expect(claude.calls.map(({ args }) => args)).toEqual([
      ["mcp", "remove", "usable-git", "--scope", "user"],
      ["mcp", "add", "--scope", "user", "--transport", "stdio", "usable-git", "--", executablePath, "mcp"],
    ]);
    expect(devin.calls[0]?.args).toEqual([
      "mcp",
      "add",
      "--scope",
      "user",
      "usable-git",
      "--",
      executablePath,
      "mcp",
    ]);
  });

  test("restores Claude config bytes if forced native replacement fails", async () => {
    const home = await createHome();
    const configPath = await writeClaudeConfig(home, { command: "/tmp/wrong", args: ["old"] });
    const before = await readFile(configPath, "utf8");
    const fake = createRunner(async (request) => {
      if (request.args[1] === "remove") {
        await writeFile(configPath, '{"mcpServers":{}}\n');
        return { exitCode: 0 };
      }
      return { exitCode: 7, stderr: "synthetic registration failure" };
    });

    await expect(
      installNativeClient({
        client: "claude",
        executablePath,
        force: true,
        home,
        runner: fake.runner,
      }),
    ).rejects.toBeInstanceOf(InstallCommandError);
    expect(await readFile(configPath, "utf8")).toBe(before);
  });

  test("restores Claude config when native remove deletes the file before add fails", async () => {
    const home = await createHome();
    const configPath = await writeClaudeConfig(home, { command: "/tmp/wrong", args: ["old"] });
    const before = await readFile(configPath, "utf8");
    const fake = createRunner(async (request) => {
      if (request.args[1] === "remove") {
        await rm(configPath);
        return { exitCode: 0 };
      }
      return { exitCode: 9, stderr: "synthetic registration failure" };
    });

    await expect(
      installNativeClient({
        client: "claude",
        executablePath,
        force: true,
        home,
        runner: fake.runner,
      }),
    ).rejects.toBeInstanceOf(InstallCommandError);
    expect(await readFile(configPath, "utf8")).toBe(before);
  });
});

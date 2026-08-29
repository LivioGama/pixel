import { describe, expect, test } from "bun:test";
import { chmod, mkdir, writeFile } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";

import {
  createDoctorClientInvoker,
  createDoctorProcessRunner,
  runDoctor,
  type DoctorClientInvoker,
  type DoctorProcessRunner,
} from "@usable-git/doctor/index.ts";
import type {
  InstallRunner,
  InstallRunnerRequest,
} from "@usable-git/install/native.ts";
import { withTempDirectory } from "./support/temp.ts";

const sourceCli = resolve(import.meta.dir, "../src/cli.ts");

const executableFixture = async (home: string) => {
  const executablePath = join(home, "bin", "usable-git");
  await mkdir(dirname(executablePath), { recursive: true });
  await writeFile(
    executablePath,
    `#!/bin/sh\nexec ${JSON.stringify(process.execPath)} ${JSON.stringify(sourceCli)} "$@"\n`,
    { mode: 0o755 },
  );
  await chmod(executablePath, 0o755);
  return executablePath;
};

const matchingInstallRunner = (executablePath: string): InstallRunner =>
  async (request: InstallRunnerRequest) => {
    if (request.command !== "codex" || request.args.join(" ") !== "mcp get usable-git --json") {
      throw new Error(`Unexpected registration request: ${JSON.stringify(request)}`);
    }
    return {
      exitCode: 0,
      stderr: "",
      stdout: JSON.stringify({
        transport: {
          type: "stdio",
          command: executablePath,
          args: ["mcp"],
          env: null,
        },
      }),
    };
  };

const writeMatchingConfigs = async (home: string, executablePath: string) => {
  const entry = { command: executablePath, args: ["mcp"] };
  const files = [
    [join(home, ".claude.json"), { sentinel: "claude", mcpServers: { "usable-git": entry } }],
    [join(home, ".cursor", "mcp.json"), { sentinel: "cursor", mcpServers: { "usable-git": entry } }],
    [join(home, ".config", "devin", "mcp_config.json"), { sentinel: "devin", mcpServers: { "usable-git": entry } }],
  ] as const;
  for (const [path, value] of files) {
    await mkdir(dirname(path), { recursive: true });
    await writeFile(path, `${JSON.stringify(value)}\n`);
  }
};

describe("runDoctor", () => {
  test("uses explicit noninteractive MCP approval and exported evidence for Devin", async () => {
    const requests: Array<{ command: string; args: string[] }> = [];
    const processRunner: DoctorProcessRunner = async ({ command, args }) => {
      requests.push({ command, args });
      if (command === "devin" && !args.includes("--version")) {
        const exportPath = args.at(args.indexOf("--export") + 1);
        if (!exportPath) throw new Error("missing Devin export path");
        await writeFile(exportPath, JSON.stringify({
          schema_version: "1",
          steps: [{
            source: "agent",
            tool_calls: [{
              function_name: "mcp_call_tool",
              tool_call_id: "devin-inspect-1",
              arguments: {
                server_name: "usable-git",
                tool_name: "inspect",
                arguments: { repoPath: "/tmp/repository" },
              },
            }],
          }],
        }));
        return { exitCode: 0, stdout: "Operation: inspect — success\n", stderr: "" };
      }
      if (command === "agent" && !args.includes("--version")) {
        return {
          exitCode: 0,
          stdout: `${JSON.stringify({
            type: "tool_call",
            subtype: "completed",
            tool_call: {
              mcpToolCall: {
                args: { providerIdentifier: "usable-git", toolName: "inspect" },
                result: { success: { isError: false } },
              },
            },
          })}\n`,
          stderr: "",
        };
      }
      return args.includes("--version")
        ? { exitCode: 0, stdout: "1.0.0\n", stderr: "" }
        : { exitCode: 0, stdout: "mcp__usable-git__inspect ok\n", stderr: "" };
    };
    const invoke = createDoctorClientInvoker();

    for (const client of ["cursor", "devin"] as const) {
      const result = await invoke({
        client,
        executablePath: "/opt/homebrew/bin/usable-git",
        home: "/tmp/home",
        repoPath: "/tmp/repository",
        processRunner,
      });
      expect(result).toMatchObject({ available: true, invoked: true, operation: "inspect" });
    }

    const cursor = requests.find(({ command, args }) => command === "agent" && !args.includes("--version"));
    expect(cursor?.args).toContain("--force");
    expect(cursor?.args).toContain("--trust");
    expect(cursor?.args).toContain("--approve-mcps");
    expect(cursor?.args).toContain("stream-json");
    const devin = requests.find(({ command, args }) => command === "devin" && !args.includes("--version"));
    expect(devin?.args).toContain("dangerous");
    expect(devin?.args).toContain("--export");
    const exportPath = devin?.args.at((devin?.args.indexOf("--export") ?? -2) + 1);
    expect(exportPath).toBeTruthy();
    expect(await Bun.file(exportPath as string).exists()).toBe(false);
  });

  test("rejects Devin plain-text success without exported structured tool evidence", async () => {
    const requests: Array<{ command: string; args: string[] }> = [];
    const processRunner: DoctorProcessRunner = async ({ command, args }) => {
      requests.push({ command, args });
      if (command === "devin" && !args.includes("--version")) {
        const exportPath = args.at(args.indexOf("--export") + 1);
        if (!exportPath) throw new Error("missing Devin export path");
        await writeFile(exportPath, JSON.stringify({
          type: "result",
          messages: [{
            role: "assistant",
            content: [{ type: "text", text: "mcp__usable-git__inspect succeeded" }],
          }],
        }));
        return { exitCode: 0, stdout: "Operation: inspect — ✅ success\n", stderr: "" };
      }
      return args.includes("--version")
        ? { exitCode: 0, stdout: "1.0.0\n", stderr: "" }
        : { exitCode: 0, stdout: "Operation: inspect — ✅ success\n", stderr: "" };
    };

    await expect(createDoctorClientInvoker()({
      client: "devin",
      executablePath: "/opt/homebrew/bin/usable-git",
      home: "/tmp/home",
      repoPath: "/tmp/repository",
      processRunner,
    })).resolves.toMatchObject({
      available: true,
      invoked: false,
    });

    const invocation = requests.find(({ args }) => !args.includes("--version"));
    expect(invocation?.args).toContain("--export");
  });

  test("accepts a completed Codex MCP inspect even when the client hangs after the tool result", async () => {
    const invoke = createDoctorClientInvoker();
    const processRunner: DoctorProcessRunner = async ({ args }) => args.includes("--version")
      ? { exitCode: 0, stdout: "0.114.0\n", stderr: "" }
      : {
          exitCode: 124,
          stdout: JSON.stringify({
            type: "item.completed",
            item: {
              type: "mcp_tool_call",
              server: "usable-git",
              tool: "inspect",
              status: "completed",
              result: { content: [{ type: "text", text: "inspect: ok (6 git subprocesses)" }] },
            },
          }),
          stderr: "process timed out",
        };

    await expect(invoke({
      client: "codex",
      executablePath: "/opt/homebrew/bin/usable-git",
      home: "/tmp/home",
      repoPath: "/tmp/repository",
      processRunner,
    })).resolves.toMatchObject({
      available: true,
      invoked: true,
      operation: "inspect",
      transport: "mcp",
    });
  });

  test("isolates Codex from user MCP config and injects only usable-git", async () => {
    const requests: Array<{ command: string; args: string[]; stdin?: string }> = [];
    const executablePath = "/opt/homebrew/bin/usable-git";
    const processRunner: DoctorProcessRunner = async ({ command, args, stdin }) => {
      requests.push({ command, args, ...(stdin === undefined ? {} : { stdin }) });
      return args.includes("--version")
        ? { exitCode: 0, stdout: "0.114.0\n", stderr: "" }
        : {
            exitCode: 0,
            stdout: `${JSON.stringify({
              type: "item.completed",
              item: {
                type: "mcp_tool_call",
                server: "usable-git",
                tool: "inspect",
                status: "completed",
                result: { structuredContent: { ok: true, operation: "inspect" } },
              },
            })}\n`,
            stderr: "",
          };
    };

    await expect(createDoctorClientInvoker()({
      client: "codex",
      executablePath,
      home: "/tmp/home",
      repoPath: "/tmp/repository",
      processRunner,
    })).resolves.toMatchObject({
      available: true,
      invoked: true,
      operation: "inspect",
      transport: "mcp",
    });

    const invocation = requests.find(({ args }) => !args.includes("--version"));
    expect(invocation?.command).toBe("codex");
    expect(invocation?.args).toContain("--ignore-user-config");
    expect(invocation?.args).toContain(
      `mcp_servers.usable-git.command=${JSON.stringify(executablePath)}`,
    );
    expect(invocation?.args).toContain('mcp_servers.usable-git.args=["mcp"]');
    expect(invocation?.args.filter((arg) => arg.startsWith("mcp_servers."))).toHaveLength(2);
    expect(invocation?.args.at(-1)).toBe("-");
    expect(invocation?.stdin).toBe(
      'Call the configured MCP tool usable-git.inspect exactly once with arguments {"repoPath":"/tmp/repository"}. ' +
      "Do not execute shell commands. Return only the operation name and success state.",
    );
    expect(invocation?.args).not.toContain(invocation?.stdin);
  });


  test("proves the real CLI, raw MCP, publish preservation, push, registrations, and fresh clients", async () =>
    withTempDirectory("usable-git-doctor-home-", async (home) => {
      const executablePath = await executableFixture(home);
      await writeMatchingConfigs(home, executablePath);
      const invoked: string[] = [];
      const clientInvoker: DoctorClientInvoker = async ({ client, repoPath }) => {
        invoked.push(client);
        expect(repoPath).toContain("usable-git-doctor-");
        return {
          available: true,
          invoked: true,
          operation: "inspect",
          transport: "mcp",
          diagnostic: "fresh session called mcp usable-git inspect",
        };
      };

      const report = await runDoctor({
        clients: "all",
        executablePath,
        home,
        runner: matchingInstallRunner(executablePath),
        clientInvoker,
      });

      expect(report.version).toBe("v1");
      expect(report.ok).toBe(true);
      expect(report.clients).toEqual(["codex", "claude", "cursor", "devin"]);
      expect(report.summary).toEqual({ passed: 14, failed: 0, skipped: 0 });
      expect(report.checks.map(({ id, status }) => ({ id, status }))).toEqual([
        { id: "runtime.bun", status: "pass" },
        { id: "runtime.git", status: "pass" },
        { id: "cli.inspect", status: "pass" },
        { id: "mcp.protocol", status: "pass" },
        { id: "repository.publish-preservation", status: "pass" },
        { id: "repository.push-one-ref", status: "pass" },
        { id: "client.codex.registration", status: "pass" },
        { id: "client.codex.fresh-session", status: "pass" },
        { id: "client.claude.registration", status: "pass" },
        { id: "client.claude.fresh-session", status: "pass" },
        { id: "client.cursor.registration", status: "pass" },
        { id: "client.cursor.fresh-session", status: "pass" },
        { id: "client.devin.registration", status: "pass" },
        { id: "client.devin.fresh-session", status: "pass" },
      ]);
      expect(invoked).toEqual(["codex", "claude", "cursor", "devin"]);
      expect(report.checks.find(({ id }) => id === "mcp.protocol")?.details).toMatchObject({
        initialized: true,
        tools: [
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
        ],
        called: "inspect",
      });
      expect(report.checks.find(({ id }) => id === "repository.publish-preservation")?.details)
        .toMatchObject({ fsck: true, committedPaths: ["selected.txt"], unrelatedStagedPreserved: true });
      expect(report.checks.find(({ id }) => id === "repository.push-one-ref")?.details)
        .toMatchObject({ fsck: true, refCount: 1, targetRef: "refs/heads/main" });
    }), 15_000);

  test("fails overall on a required check failure and skips only an unavailable selected client", async () =>
    withTempDirectory("usable-git-doctor-failure-", async (home) => {
      const executablePath = await executableFixture(home);
      await writeMatchingConfigs(home, executablePath);
      const baseProcessRunner = createDoctorProcessRunner();
      const processRunner: DoctorProcessRunner = async (request) =>
        request.command === "git" && request.args[0] === "--version"
          ? { exitCode: 127, stdout: "", stderr: "git unavailable" }
          : baseProcessRunner(request);
      const clientInvoker: DoctorClientInvoker = async () => ({
        available: false,
        invoked: false,
        reason: "client executable not found",
      });

      const report = await runDoctor({
        clients: ["codex"],
        executablePath,
        home,
        runner: matchingInstallRunner(executablePath),
        processRunner,
        clientInvoker,
      });

      expect(report.ok).toBe(false);
      expect(report.summary.failed).toBe(1);
      expect(report.summary.skipped).toBe(1);
      expect(report.checks.find(({ id }) => id === "runtime.git")).toMatchObject({
        required: true,
        status: "fail",
      });
      expect(report.checks.find(({ id }) => id === "client.codex.fresh-session")).toMatchObject({
        required: true,
        status: "skip",
        reason: "client executable not found",
      });
      expect(report.activatedClients).toEqual([]);
    }), 15_000);

  test("treats an unavailable selected client as an incomplete doctor run", async () =>
    withTempDirectory("usable-git-doctor-skip-", async (home) => {
      const executablePath = await executableFixture(home);
      await writeMatchingConfigs(home, executablePath);

      const report = await runDoctor({
        clients: ["codex"],
        executablePath,
        home,
        runner: matchingInstallRunner(executablePath),
        clientInvoker: async () => ({
          available: false,
          invoked: false,
          reason: "client executable not found",
        }),
      });

      expect(report.ok).toBe(false);
      expect(report.summary).toEqual({ passed: 7, failed: 0, skipped: 1 });
      expect(report.activatedClients).toEqual([]);
    }), 15_000);

  test("removes the banned Anthropic API key from doctor subprocesses", async () => {
    const original = process.env.ANTHROPIC_API_KEY;
    process.env.ANTHROPIC_API_KEY = "must-not-leak";
    try {
      const result = await createDoctorProcessRunner()({
        command: "printenv",
        args: ["ANTHROPIC_API_KEY"],
      });
      expect(result.exitCode).toBe(1);
      expect(result.stdout).toBe("");
    } finally {
      if (original === undefined) delete process.env.ANTHROPIC_API_KEY;
      else process.env.ANTHROPIC_API_KEY = original;
    }
  });
});

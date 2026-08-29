import { describe, expect, test } from "bun:test";
import { mkdir, mkdtemp, readFile, realpath, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  benchmarkClientIds,
  createBenchmarkClientProcessRunner,
  createClientInvocation,
  parseClientEvidence,
  runBenchmarkClientSession,
  type BenchmarkClientProcessRunner,
} from "../../../benchmarks/clients.ts";

const temporaryStateRoot = async () =>
  realpath(await mkdtemp(join(tmpdir(), "usable-git-benchmark-clients-")));

const writeTelemetry = async (stateRoot: string, gitSubprocessCounts: number[]) => {
  const directory = join(stateRoot, "usable-git");
  await mkdir(directory, { recursive: true });
  const lines = gitSubprocessCounts.map((count) =>
    JSON.stringify({ version: "v1", operation: "inspect", gitSubprocessCount: count })
  );
  await Bun.write(join(directory, "telemetry-v1.jsonl"), `${lines.join("\n")}\n`);
};

describe("real benchmark client adapters", () => {
  test("builds non-interactive structured invocations for every supported client", async () => {
    const stateRoot = await temporaryStateRoot();
    try {
      const inputs = {
        repoPath: "/tmp/repo",
        prompt: "perform the isolated Git task",
        artifactPath: "/tmp/devin-export.json",
        mutating: true,
        stateRoot,
      };
      expect(benchmarkClientIds).toEqual(["codex", "claude-code", "cursor", "devin"]);
      const invocations = {
        codex: createClientInvocation("codex", inputs),
        "claude-code": createClientInvocation("claude-code", inputs),
        cursor: createClientInvocation("cursor", inputs),
        devin: createClientInvocation("devin", inputs),
      };

      expect(invocations.codex.command).toBe("codex");
      expect(invocations.codex.args).toContain("--json");
      expect(invocations.codex.args).toContain("--ephemeral");
      expect(invocations["claude-code"].command).toBe("claude");
      expect(invocations["claude-code"].args).toContain("--model");
      expect(invocations["claude-code"].args).toContain("sonnet");
      expect(invocations["claude-code"].args).toContain("stream-json");
      expect(invocations["claude-code"].args).toContain("--no-session-persistence");
      expect(invocations.cursor.command).toBe("agent");
      expect(invocations.cursor.args).toContain("stream-json");
      expect(invocations.cursor.args).toContain("--approve-mcps");
      expect(invocations.devin.command).toBe("devin");
      expect(invocations.devin.args).toContain("--export");
      expect(invocations.devin.artifactPath).toBe("/tmp/devin-export.json");
      expect(createClientInvocation("devin", { ...inputs, mutating: false }).args).toContain("auto");
      expect(createClientInvocation("devin", {
        ...inputs,
        mutating: false,
        semantic: true,
      }).args).toContain("dangerous");
      expect(Object.hasOwn(invocations.codex.env, "ANTHROPIC_API_KEY")).toBe(false);
      expect(invocations.codex.env.USABLE_GIT_TELEMETRY).toBe("1");
      expect(invocations.codex.env.XDG_STATE_HOME).toBe(stateRoot);
    } finally {
      await rm(stateRoot, { recursive: true, force: true });
    }
  });

  test("pins both arms to an isolated usable-git tool surface with telemetry reachable", async () => {
    const stateRoot = await temporaryStateRoot();
    try {
      const base = {
        repoPath: "/tmp/repo",
        prompt: "task",
        artifactPath: "/tmp/export.json",
        mutating: false,
        stateRoot,
        executablePath: "/opt/homebrew/bin/usable-git",
      };

      // Codex: dotted env overrides are silently dropped, so the telemetry env
      // must ride in a TOML inline table or gitSubprocessCount is never logged.
      const codex = createClientInvocation("codex", base);
      expect(codex.args).toContain("--ignore-user-config");
      expect(codex.args.join(" ")).toContain(
        'mcp_servers.usable-git.env={USABLE_GIT_TELEMETRY="1"',
      );

      // Claude: strict config pins the surface; env must be declared per server.
      const claude = createClientInvocation("claude-code", base);
      expect(claude.args).toContain("--strict-mcp-config");
      const configPath = claude.args[claude.args.indexOf("--mcp-config") + 1]!;
      const config = JSON.parse(await readFile(configPath, "utf8"));
      expect(config.mcpServers["usable-git"].env).toEqual({
        USABLE_GIT_TELEMETRY: "1",
        XDG_STATE_HOME: stateRoot,
      });

      // Raw arm keeps shell git; only the semantic arm has it denied.
      expect(createClientInvocation("claude-code", base).args).not.toContain("--disallowedTools");
      const semantic = createClientInvocation("claude-code", { ...base, semantic: true });
      expect(semantic.args[semantic.args.indexOf("--disallowedTools") + 1]).toBe("Bash(git:*)");
    } finally {
      await rm(stateRoot, { recursive: true, force: true });
    }
  });

  test("bounds captured client output and terminates the isolated process", async () => {
    const runner = createBenchmarkClientProcessRunner({ maxOutputBytes: 64 });
    const result = await runner({
      command: process.execPath,
      args: ["-e", "process.stdout.write('x'.repeat(4096)); setTimeout(() => {}, 60000)"],
      cwd: process.cwd(),
      env: { ...process.env } as Record<string, string>,
      timeoutMs: 5_000,
    });

    expect(result.exitCode).toBe(125);
    expect(result.stdout.length).toBeLessThanOrEqual(64);
    expect(result.stdoutTruncated).toBe(true);
  });

  test("extracts measured Codex semantic calls and aggregate usage", () => {
    const evidence = parseClientEvidence("codex", {
      exitCode: 0,
      stderr: "",
      stdout: [
        JSON.stringify({ type: "thread.started", thread_id: "t1" }),
        JSON.stringify({
          type: "item.completed",
          item: {
            id: "call-1",
            type: "mcp_tool_call",
            server: "usable-git",
            tool: "inspect",
            result: { structuredContent: { ok: true } },
          },
        }),
        JSON.stringify({
          type: "turn.completed",
          usage: { input_tokens: 120, cached_input_tokens: 20, output_tokens: 30 },
        }),
      ].join("\n"),
    });

    expect(evidence.structured).toBe(true);
    expect(evidence.semanticToolCalls).toBe(1);
    expect(evidence.rawGitToolCalls).toBe(0);
    expect(evidence.agentFacingOperations).toBe(1);
    // The v1 envelope no longer carries gitSubprocessCount on the wire, so a
    // bare parseClientEvidence() call (no telemetry log) can't recover it —
    // only runBenchmarkClientSession(), which reads the telemetry log, can.
    expect(evidence.gitSubprocesses).toEqual({ value: null, source: "unavailable" });
    expect(evidence.tokenUsage).toEqual({
      inputTokens: 120,
      outputTokens: 30,
      totalTokens: 150,
      source: "codex-json-usage",
    });
  });

  test("recovers semantic gitSubprocesses from the per-trial telemetry log", async () => {
    const stateRoot = await temporaryStateRoot();
    try {
      await writeTelemetry(stateRoot, [2, 1]);
      const runner: BenchmarkClientProcessRunner = async () => ({
        exitCode: 0,
        durationMs: 12,
        stderr: "",
        stdout: [
          JSON.stringify({
            type: "item.completed",
            item: {
              id: "call-1",
              type: "mcp_tool_call",
              server: "usable-git",
              tool: "inspect",
              result: { ok: true },
            },
          }),
          JSON.stringify({
            type: "turn.completed",
            usage: { input_tokens: 20, output_tokens: 5 },
          }),
        ].join("\n"),
      });
      const result = await runBenchmarkClientSession({
        client: "codex",
        repoPath: "/tmp/repo",
        prompt: "inspect through usable-git",
        artifactPath: "/tmp/export.json",
        mutating: false,
        expectedMethod: "semantic",
        expectedSemanticOperations: ["inspect"],
        processRunner: runner,
        stateRoot,
      });
      expect(result.gitSubprocesses).toEqual({ value: 3, source: "telemetry" });
    } finally {
      await rm(stateRoot, { recursive: true, force: true });
    }
  });

  test("accepts completed Codex tool evidence when the client hangs until timeout", async () => {
    const runner: BenchmarkClientProcessRunner = async () => ({
      exitCode: 124,
      durationMs: 120_000,
      stderr: "",
      stdout: JSON.stringify({
        type: "item.completed",
        item: {
          id: "call-timeout",
          type: "mcp_tool_call",
          server: "usable-git",
          tool: "inspect",
          status: "completed",
          result: { metrics: { gitSubprocessCount: 2 } },
        },
      }),
    });
    const result = await runBenchmarkClientSession({
      client: "codex",
      repoPath: "/tmp/repo",
      prompt: "inspect through usable-git",
      artifactPath: "/tmp/export.json",
      mutating: false,
      expectedMethod: "semantic",
      expectedSemanticOperations: ["inspect"],
      processRunner: runner,
      stateRoot: "/tmp/usable-git-benchmark-clients-test-no-telemetry",
    });

    expect(result.success).toBe(true);
    expect(result.semanticAdopted).toBe(true);
    expect(result.gitRelatedTokens.value).toBeNull();

    const rawTimeout = parseClientEvidence("codex", {
      exitCode: 124,
      stderr: "",
      stdout: JSON.stringify({
        type: "item.completed",
        item: {
          id: "raw-timeout",
          type: "command_execution",
          command: "git status --porcelain=v1",
        },
      }),
    });
    expect(rawTimeout.terminalSuccess).toBe(false);
  });

  test("extracts Claude raw Git calls and measured result usage without retaining commands", () => {
    const evidence = parseClientEvidence("claude-code", {
      exitCode: 0,
      stderr: "",
      stdout: [
        JSON.stringify({
          type: "assistant",
          message: {
            content: [{
              type: "tool_use",
              id: "tool-1",
              name: "Bash",
              input: { command: "git status --porcelain=v2" },
            }],
          },
        }),
        JSON.stringify({
          type: "result",
          subtype: "success",
          usage: { input_tokens: 90, output_tokens: 10 },
        }),
      ].join("\n"),
    });

    expect(evidence.rawGitToolCalls).toBe(1);
    expect(evidence.gitSubprocesses).toEqual({ value: 1, source: "structured-command" });
    expect(evidence.tokenUsage?.totalTokens).toBe(100);
    expect(JSON.stringify(evidence)).not.toContain("git status");
  });

  test("correlates Claude MCP tool results by tool_use_id without needing a subprocess count", () => {
    const evidence = parseClientEvidence("claude-code", {
      exitCode: 0,
      stderr: "",
      stdout: [
        JSON.stringify({
          type: "assistant",
          message: {
            content: [{
              type: "tool_use",
              id: "tool-semantic",
              name: "mcp__usable-git__inspect",
              input: {},
            }],
          },
        }),
        JSON.stringify({
          type: "user",
          message: {
            content: [{
              type: "tool_result",
              tool_use_id: "tool-semantic",
              content: JSON.stringify({ ok: true }),
            }],
          },
        }),
        JSON.stringify({
          type: "result",
          subtype: "success",
          usage: { input_tokens: 50, output_tokens: 10 },
        }),
      ].join("\n"),
    });

    expect(evidence.semanticOperations).toEqual(["inspect"]);
    expect(evidence.gitSubprocesses).toEqual({ value: null, source: "unavailable" });
  });

  test("uses completed Cursor MCP events and fails token evidence when JSON omits usage", () => {
    const evidence = parseClientEvidence("cursor", {
      exitCode: 0,
      stderr: "",
      stdout: [
        JSON.stringify({ type: "system", subtype: "init", model: "test" }),
        JSON.stringify({
          type: "tool_call",
          subtype: "completed",
          call_id: "cursor-call-1",
          tool_call: {
            mcpToolCall: {
              args: { server: "usable-git", tool: "inspect" },
              result: { success: { ok: true } },
            },
          },
        }),
        JSON.stringify({ type: "result", subtype: "success", is_error: false }),
      ].join("\n"),
    });

    expect(evidence.semanticToolCalls).toBe(1);
    expect(evidence.gitSubprocesses.value).toBeNull();
    expect(evidence.tokenUsage).toBeNull();
    expect(evidence.errors).toContain("client JSON did not expose complete token usage");
  });

  test("reads Devin export evidence and rejects unstructured successful stdout", () => {
    const exported = parseClientEvidence("devin", {
      exitCode: 0,
      stderr: "",
      stdout: "done",
      artifactJson: JSON.stringify({
        type: "result",
        usage: { inputTokens: 40, outputTokens: 8, totalTokens: 48 },
        messages: [{
          role: "assistant",
          content: [{
            type: "tool_use",
            id: "devin-call-1",
            name: "mcp__usable-git__inspect",
            input: {},
          }],
        }],
      }, null, 2),
    });
    const unstructured = parseClientEvidence("devin", {
      exitCode: 0,
      stderr: "",
      stdout: "looks good",
    });

    expect(exported.semanticToolCalls).toBe(1);
    expect(exported.tokenUsage?.totalTokens).toBe(48);
    expect(unstructured.structured).toBe(false);
    expect(unstructured.tokenUsage).toBeNull();
    expect(unstructured.errors).toContain("no parseable structured client evidence");
  });

  test("runs through an injected process adapter and proves semantic adoption", async () => {
    const requests: Array<{ command: string; args: string[] }> = [];
    const runner: BenchmarkClientProcessRunner = async (request) => {
      requests.push({ command: request.command, args: request.args });
      return {
        exitCode: 0,
        durationMs: 12,
        stderr: "",
        stdout: [
          JSON.stringify({
            type: "item.completed",
            item: {
              id: "call-1",
              type: "mcp_tool_call",
              server: "usable-git",
              tool: "inspect",
              result: { metrics: { gitSubprocessCount: 2 } },
            },
          }),
          JSON.stringify({
            type: "turn.completed",
            usage: { input_tokens: 20, output_tokens: 5 },
          }),
        ].join("\n"),
      };
    };

    const result = await runBenchmarkClientSession({
      client: "codex",
      repoPath: "/tmp/repo",
      prompt: "inspect through usable-git",
      artifactPath: "/tmp/export.json",
      mutating: false,
      expectedMethod: "semantic",
      processRunner: runner,
      stateRoot: "/tmp/usable-git-benchmark-clients-test-no-telemetry",
    });

    expect(requests).toHaveLength(1);
    expect(result.success).toBe(true);
    expect(result.semanticAdopted).toBe(true);
    expect(result.gitRelatedTokens.value).toBe(25);
    expect(result.gitRelatedTokens.scope).toBe("isolated-git-task-session-total");
  });

  test("requires every expected semantic operation before claiming adoption", async () => {
    const runner: BenchmarkClientProcessRunner = async () => ({
      exitCode: 0,
      durationMs: 12,
      stderr: "",
      stdout: [
        JSON.stringify({
          type: "item.completed",
          item: {
            id: "call-1",
            type: "mcp_tool_call",
            server: "usable-git",
            tool: "inspect",
            result: { metrics: { gitSubprocessCount: 2 } },
          },
        }),
        JSON.stringify({
          type: "turn.completed",
          usage: { input_tokens: 20, output_tokens: 5 },
        }),
      ].join("\n"),
    });
    const result = await runBenchmarkClientSession({
      client: "codex",
      repoPath: "/tmp/repo",
      prompt: "inspect then publish through usable-git",
      artifactPath: "/tmp/export.json",
      mutating: true,
      expectedMethod: "semantic",
      expectedSemanticOperations: ["inspect", "publish"],
      processRunner: runner,
      stateRoot: "/tmp/usable-git-benchmark-clients-test-no-telemetry",
    });

    expect(result.success).toBe(false);
    expect(result.semanticAdopted).toBe(false);
    expect(result.evidenceErrors).toContain(
      "expected semantic operations inspect,publish in order without extras, observed inspect",
    );
  });

  test("accepts a batched raw arm but rejects one that ran no Git at all", async () => {
    const runner: BenchmarkClientProcessRunner = async () => ({
      exitCode: 0,
      durationMs: 12,
      stderr: "",
      stdout: [
        JSON.stringify({
          type: "item.completed",
          item: {
            id: "raw-call-1",
            type: "command_execution",
            command: "git status --porcelain=v2 --branch",
          },
        }),
        JSON.stringify({
          type: "turn.completed",
          usage: { input_tokens: 20, output_tokens: 5 },
        }),
      ].join("\n"),
    });
    const result = await runBenchmarkClientSession({
      client: "codex",
      repoPath: "/tmp/repo",
      prompt: "run both raw Git inspection operations",
      artifactPath: "/tmp/export.json",
      mutating: false,
      expectedMethod: "raw-git",
      expectedRawGitToolCalls: 2,
      processRunner: runner,
      stateRoot: "/tmp/usable-git-benchmark-clients-test-no-telemetry",
    });

    // One batched shell call doing real Git work is a legitimate way to run
    // the raw arm; the final-state oracle proves the task was accomplished.
    expect(result.success).toBe(true);
    expect(result.evidenceErrors).toEqual([]);

    const noGitRunner: BenchmarkClientProcessRunner = async () => ({
      exitCode: 0,
      durationMs: 12,
      stderr: "",
      stdout: [
        JSON.stringify({
          type: "item.completed",
          item: { id: "noop", type: "command_execution", command: "ls -la" },
        }),
        JSON.stringify({ type: "turn.completed", usage: { input_tokens: 20, output_tokens: 5 } }),
      ].join("\n"),
    });
    const noGit = await runBenchmarkClientSession({
      client: "codex",
      repoPath: "/tmp/repo",
      prompt: "run both raw Git inspection operations",
      artifactPath: "/tmp/export.json",
      mutating: false,
      expectedMethod: "raw-git",
      expectedRawGitToolCalls: 2,
      processRunner: noGitRunner,
      stateRoot: "/tmp/usable-git-benchmark-clients-test-no-telemetry",
    });
    expect(noGit.success).toBe(false);
    expect(noGit.evidenceErrors.join(" ")).toContain("at least one raw Git tool call");
  });

  test("allows a repeated inspect but rejects operations outside the expected set", async () => {
    const runner: BenchmarkClientProcessRunner = async () => ({
      exitCode: 0,
      durationMs: 12,
      stderr: "",
      stdout: [
        ...["call-1", "call-2"].map((id) => JSON.stringify({
          type: "item.completed",
          item: {
            id,
            type: "mcp_tool_call",
            server: "usable-git",
            tool: "inspect",
            result: { metrics: { gitSubprocessCount: 2 } },
          },
        })),
        JSON.stringify({
          type: "turn.completed",
          usage: { input_tokens: 20, output_tokens: 5 },
        }),
      ].join("\n"),
    });
    const result = await runBenchmarkClientSession({
      client: "codex",
      repoPath: "/tmp/repo",
      prompt: "inspect exactly once through usable-git",
      artifactPath: "/tmp/export.json",
      mutating: false,
      expectedMethod: "semantic",
      expectedSemanticOperations: ["inspect"],
      processRunner: runner,
      stateRoot: "/tmp/usable-git-benchmark-clients-test-no-telemetry",
    });

    // Re-inspecting is legitimate agent behaviour (after STALE_STATE, or just
    // re-reading state before mutating) and must not be scored as a failure.
    expect(result.success).toBe(true);
    expect(result.semanticAdopted).toBe(true);
    expect(result.evidenceErrors).toEqual([]);

    // An operation outside the expected set is still rejected.
    const strayRunner: BenchmarkClientProcessRunner = async () => ({
      exitCode: 0,
      durationMs: 12,
      stderr: "",
      stdout: [
        JSON.stringify({
          type: "item.completed",
          item: { id: "c1", type: "mcp_tool_call", server: "usable-git", tool: "inspect" },
        }),
        JSON.stringify({
          type: "item.completed",
          item: { id: "c2", type: "mcp_tool_call", server: "usable-git", tool: "push" },
        }),
        JSON.stringify({ type: "turn.completed", usage: { input_tokens: 20, output_tokens: 5 } }),
      ].join("\n"),
    });
    const stray = await runBenchmarkClientSession({
      client: "codex",
      repoPath: "/tmp/repo",
      prompt: "inspect exactly once through usable-git",
      artifactPath: "/tmp/export.json",
      mutating: false,
      expectedMethod: "semantic",
      expectedSemanticOperations: ["inspect"],
      processRunner: strayRunner,
      stateRoot: "/tmp/usable-git-benchmark-clients-test-no-telemetry",
    });
    expect(stray.success).toBe(false);
    expect(stray.evidenceErrors.join(" ")).toContain("in order without extras");
  });

  test("counts shell-wrapped and wrapper-prefixed git invocations as raw Git usage", () => {
    // Real clients emit `/bin/zsh -lc 'rtk git status …'`; anchoring on a
    // statement separator silently reported zero raw Git usage and failed the
    // raw arm of every codex pair.
    const evidence = parseClientEvidence("codex", {
      exitCode: 0,
      stderr: "",
      stdout: [
        JSON.stringify({
          type: "item.completed",
          item: {
            id: "wrapped",
            type: "command_execution",
            command: "/bin/zsh -lc 'rtk git status --porcelain=v2 -z --branch'",
          },
        }),
        JSON.stringify({ type: "turn.completed", usage: { input_tokens: 10, output_tokens: 2 } }),
      ].join("\n"),
    });
    expect(evidence.rawGitToolCalls).toBe(1);
  });

  test("reads Devin ATIF exports for tool calls and session token totals", () => {
    // Devin's export nests calls under steps[].tool_calls[] and reports totals
    // in final_metrics, matching none of the other clients' shapes.
    const evidence = parseClientEvidence("devin", {
      exitCode: 0,
      stderr: "",
      stdout: "",
      artifactJson: JSON.stringify({
        schema_version: "ATIF-v1.7",
        steps: [{
          source: "agent",
          tool_calls: [{
            tool_call_id: "call-exec-1",
            function_name: "exec",
            arguments: { command: "git status --porcelain=v2", workdir: "/tmp/repo" },
          }],
        }],
        final_metrics: { total_prompt_tokens: 84_742, total_completion_tokens: 491 },
      }),
    });
    expect(evidence.rawGitToolCalls).toBe(1);
    expect(evidence.terminalSuccess).toBe(true);
    expect(evidence.tokenUsage?.totalTokens).toBe(85_233);
    expect(evidence.errors).toEqual([]);
  });
});

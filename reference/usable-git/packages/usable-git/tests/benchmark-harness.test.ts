import { describe, expect, test } from "bun:test";
import type { BenchmarkClientProcessRunner } from "../../../benchmarks/clients.ts";
import { runBenchmarkMatrix } from "../../../benchmarks/runner.ts";

describe("paired benchmark harness", () => {
  test("proves equivalent final state for inspect and publish fixtures", async () => {
    const artifact = await runBenchmarkMatrix({
      clients: ["harness"],
      scenarios: ["inspect-dirty", "publish-scoped"],
      trials: 2,
      seed: 7_331,
      allowShortRun: true,
    });

    expect(artifact.schemaVersion).toBe("usable-git-benchmark-v1");
    expect(artifact.trials).toHaveLength(8);
    expect(artifact.trials.every(({ oracle }) => oracle.equivalent && oracle.valid)).toBe(true);
    expect(artifact.summary.every(({ successRate }) => successRate === 1)).toBe(true);
    expect(artifact.environment.clientVersions.harness).toBeNull();
    expect(artifact.trials.every(({ gitRelatedTokens }) => gitRelatedTokens.value === null)).toBe(
      true,
    );
    const publish = artifact.summary.filter(({ scenario }) => scenario === "publish-scoped");
    const raw = publish.find(({ method }) => method === "raw-git")!;
    const semantic = publish.find(({ method }) => method === "semantic")!;
    expect(raw.agentFacingOperations.median).toBeGreaterThanOrEqual(4);
    expect(semantic.agentFacingOperations.median).toBeLessThanOrEqual(2);
    expect(1 - semantic.agentFacingOperations.median / raw.agentFacingOperations.median)
      .toBeGreaterThanOrEqual(0.5);
  }, 20_000);

  test("refuses release artifacts below the release trial minimum", async () => {
    await expect(
      runBenchmarkMatrix({
        clients: ["codex"],
        scenarios: ["inspect-dirty"],
        trials: 2,
        seed: 1,
      }),
    ).rejects.toThrow("at least 10 trials");
  });

  test("blocks release eligibility when the required scenario matrix is incomplete", async () => {
    const artifact = await runBenchmarkMatrix({
      clients: ["codex", "claude-code", "devin"],
      clientVersions: {
        codex: "test-version",
        "claude-code": "test-version",
        devin: "test-version",
      },
      scenarios: [],
      trials: 40,
      seed: 11,
    });

    expect(artifact.releaseGate.pass).toBe(false);
    expect(artifact.releaseGate.reasons).toContain(
      "benchmark matrix is missing required client/scenario/method trials",
    );
  });

  test("executes paired real-client sessions and records measured adoption evidence", async () => {
    const requests: string[][] = [];
    const processRunner: BenchmarkClientProcessRunner = async (request) => {
      requests.push(request.args);
      const semantic = request.args.some((argument) => argument.includes("usable-git MCP"));
      return {
        exitCode: 0,
        durationMs: semantic ? 40 : 100,
        stderr: "",
        stdout: [
          JSON.stringify(semantic
            ? {
                type: "item.completed",
                item: {
                  id: "semantic-call",
                  type: "mcp_tool_call",
                  server: "usable-git",
                  tool: "inspect",
                  result: { ok: true },
                },
              }
            : {
                type: "item.completed",
                item: {
                  id: "raw-call",
                  type: "command_execution",
                  command: "git status --porcelain=v2 --branch",
                },
              }),
          ...(!semantic
            ? [JSON.stringify({
                type: "item.completed",
                item: {
                  id: "raw-call-2",
                  type: "command_execution",
                  command: "git rev-list --walk-reflogs --count refs/stash",
                },
              })]
            : []),
          JSON.stringify({
            type: "turn.completed",
            usage: semantic
              ? { input_tokens: 35, output_tokens: 5 }
              : { input_tokens: 80, output_tokens: 20 },
          }),
        ].join("\n"),
      };
    };

    const artifact = await runBenchmarkMatrix({
      clients: ["codex"],
      clientVersions: { codex: "test-version" },
      scenarios: ["inspect-dirty"],
      trials: 1,
      seed: 12,
      allowShortRun: true,
      clientProcessRunner: processRunner,
    });

    expect(requests).toHaveLength(2);
    expect(artifact.trials.every(({ execution }) => execution === "real-client-session")).toBe(true);
    const raw = artifact.trials.find(({ method }) => method === "raw-git")!;
    const semantic = artifact.trials.find(({ method }) => method === "semantic")!;
    expect(raw.gitRelatedTokens.value).toBe(100);
    expect(raw.rawGitToolCalls).toBe(2);
    expect(raw.evidenceErrors.some((error) =>
      error.startsWith("expected semantic operation sequence")
    ))
      .toBe(false);
    expect(semantic.gitRelatedTokens.value).toBe(40);
    expect(semantic.semanticAdopted).toBe(true);
    expect(semantic.semanticToolCalls).toBe(1);
    // The mock process runner never invokes usable-git for real, so no
    // telemetry log exists for this trial's stateRoot — gitSubprocesses is
    // legitimately unmeasured here, unlike a real client session.
    expect(semantic.gitSubprocesses).toBeNull();
    expect(artifact.summary.find(({ method }) => method === "semantic")?.semanticAdoptionRate)
      .toBe(1);
    expect(artifact.releaseGate.reasons).not.toContain(
      "Git-related client token measurements unavailable",
    );
    expect(artifact.releaseGate.reasons).toContain(
      "client matrix must include codex, claude-code, and devin",
    );
  }, 20_000);

  test("blocks release eligibility when a client omits parseable token evidence", async () => {
    const processRunner: BenchmarkClientProcessRunner = async (request) => {
      const semantic = request.args.some((argument) => argument.includes("usable-git MCP"));
      return {
        exitCode: 0,
        durationMs: 10,
        stderr: "",
        stdout: [
          JSON.stringify(semantic
          ? {
              type: "item.completed",
              item: {
                id: "semantic-call",
                type: "mcp_tool_call",
                server: "usable-git",
                tool: "inspect",
                result: { metrics: { gitSubprocessCount: 2 } },
              },
            }
          : {
              type: "item.completed",
              item: {
                id: "raw-call",
                type: "command_execution",
                command: "git status --porcelain=v2 --branch",
              },
            }),
          ...(!semantic
            ? [JSON.stringify({
                type: "item.completed",
                item: {
                  id: "raw-call-2",
                  type: "command_execution",
                  command: "git rev-list --walk-reflogs --count refs/stash",
                },
              })]
            : []),
          JSON.stringify({ type: "turn.completed" }),
        ].join("\n"),
      };
    };
    const artifact = await runBenchmarkMatrix({
      clients: ["codex"],
      clientVersions: { codex: "test-version" },
      scenarios: ["inspect-dirty"],
      trials: 1,
      seed: 13,
      allowShortRun: true,
      clientProcessRunner: processRunner,
    });

    expect(artifact.releaseGate.pass).toBe(false);
    expect(artifact.releaseGate.reasons).toContain(
      "Git-related client token measurements unavailable",
    );
    expect(artifact.trials.every(({ evidenceErrors }) =>
      evidenceErrors.includes("client JSON did not expose complete token usage")
    )).toBe(true);
  }, 20_000);

  test("rejects semantic publish evidence that never called publish", async () => {
    const processRunner: BenchmarkClientProcessRunner = async () => ({
      exitCode: 0,
      durationMs: 10,
      stderr: "",
      stdout: [
        JSON.stringify({
          type: "item.completed",
          item: {
            id: "inspect-only",
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
    const artifact = await runBenchmarkMatrix({
      clients: ["codex"],
      clientVersions: { codex: "test-version" },
      scenarios: ["publish-scoped"],
      trials: 1,
      seed: 14,
      allowShortRun: true,
      clientProcessRunner: processRunner,
    });
    const semantic = artifact.trials.find(({ method }) => method === "semantic")!;

    expect(semantic.success).toBe(false);
    expect(semantic.semanticAdopted).toBe(false);
    expect(semantic.evidenceErrors).toContain(
      "expected semantic operations inspect,publish in order without extras, observed inspect",
    );
  }, 20_000);
});

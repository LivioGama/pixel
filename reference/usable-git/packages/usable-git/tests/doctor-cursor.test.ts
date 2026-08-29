import { describe, expect, test } from "bun:test";

import {
  createDoctorClientInvoker,
  type DoctorProcessRunner,
} from "@usable-git/doctor/index.ts";

const cursorEvent = (result: Record<string, unknown>) => `${JSON.stringify({
  type: "tool_call",
  subtype: "completed",
  tool_call: {
    mcpToolCall: {
      args: {
        providerIdentifier: "usable-git",
        serverIdentifier: "usable-git",
        toolName: "inspect",
      },
      result,
    },
  },
})}\n`;

const invokeCursor = async (stdout: string) => {
  const requests: Array<{ args: string[] }> = [];
  const processRunner: DoctorProcessRunner = async ({ args }) => {
    requests.push({ args });
    return args.includes("--version")
      ? { exitCode: 0, stdout: "2026.07.14\n", stderr: "" }
      : { exitCode: 0, stdout, stderr: "" };
  };
  const result = await createDoctorClientInvoker()({
    client: "cursor",
    executablePath: "/opt/homebrew/bin/usable-git",
    home: "/tmp/home",
    repoPath: "/tmp/repository",
    processRunner,
  });
  return {
    result,
    args: requests.find(({ args }) => !args.includes("--version"))?.args ?? [],
  };
};

describe("Cursor doctor invocation", () => {
  test("forces noninteractive tools and requires a structured successful MCP completion", async () => {
    const { result, args } = await invokeCursor(cursorEvent({
      success: {
        content: [{ text: { text: "inspect: ok (6 git subprocesses)" } }],
        isError: false,
      },
    }));

    expect(args).toContain("--force");
    expect(args).toContain("--output-format");
    expect(args.at(args.indexOf("--output-format") + 1)).toBe("stream-json");
    expect(result).toMatchObject({
      available: true,
      invoked: true,
      operation: "inspect",
      transport: "mcp",
    });
  });

  test("does not treat a rejected structured MCP call as activation", async () => {
    const { result } = await invokeCursor(cursorEvent({
      rejected: { reason: "User rejected MCP: usable-git-inspect", isReadonly: false },
    }));

    expect(result).toMatchObject({ available: true, invoked: false });
  });
});

import { afterEach, describe, expect, test } from "bun:test";
import { SniperReporter } from "../src/vitest-reporter.ts";
import type { ErrorEnvelope, EventEnvelope } from "../src/report.ts";

const prevCI = process.env.CI;
afterEach(() => {
  if (prevCI === undefined) delete process.env.CI;
  else process.env.CI = prevCI;
});

const failingFile = (n: number, filepath = "/proj/tests/math.test.ts") => ({
  filepath,
  tasks: [
    {
      type: "suite",
      name: "math",
      tasks: Array.from({ length: n }, (_, i) => ({
        type: "test",
        name: `case ${i}`,
        result: {
          state: "fail",
          duration: 10,
          errors: [
            {
              message: `expected 2 to be ${i}`,
              expected: i,
              actual: 2,
              stack: `AssertionError: nope\n    at ${filepath}:${20 + i}:5`,
            },
          ],
        },
      })),
    },
    {
      type: "test",
      name: "passing one",
      result: { state: "pass", duration: 5 },
    },
  ],
});

describe("SniperReporter", () => {
  test("failing run ships ONE vitest error envelope with structured failures", () => {
    delete process.env.CI;
    const shipped: Array<ErrorEnvelope | EventEnvelope> = [];
    const reporter = new SniperReporter({ shipImpl: (e) => shipped.push(e) });
    reporter.onFinished([failingFile(2)] as never);
    expect(shipped.length).toBe(1);
    const env = shipped[0] as ErrorEnvelope;
    expect(env.surface).toBe("vitest");
    expect(env.kind).toBe("test-failures");
    expect(env.message).toMatch(/^2 failed \| 1 passed \(0\.0s\)$/);
    const extra = env.extra as { failures: Array<Record<string, unknown>> };
    expect(extra.failures.length).toBe(2);
    expect(extra.failures[0]).toMatchObject({
      test: "math > case 0",
      file: "/proj/tests/math.test.ts",
      line: 20,
      expected: "0",
      received: "2",
    });
  });

  test("failures cap at 50 with truncatedCount", () => {
    delete process.env.CI;
    const shipped: Array<ErrorEnvelope | EventEnvelope> = [];
    const reporter = new SniperReporter({ shipImpl: (e) => shipped.push(e) });
    reporter.onFinished([failingFile(60)] as never);
    const extra = (shipped[0] as ErrorEnvelope).extra as {
      failures: unknown[];
      truncatedCount: number;
    };
    expect(extra.failures.length).toBe(50);
    expect(extra.truncatedCount).toBe(10);
  });

  test("green run ships a test-pass event", () => {
    delete process.env.CI;
    const shipped: Array<ErrorEnvelope | EventEnvelope> = [];
    const reporter = new SniperReporter({ shipImpl: (e) => shipped.push(e) });
    reporter.onFinished([
      {
        filepath: "/proj/tests/ok.test.ts",
        tasks: [
          { type: "test", name: "a", result: { state: "pass", duration: 3 } },
          { type: "test", name: "b", result: { state: "pass", duration: 4 } },
        ],
      },
    ] as never);
    expect(shipped.length).toBe(1);
    const env = shipped[0] as EventEnvelope;
    expect(env.type).toBe("event");
    expect(env.kind).toBe("test-pass");
    expect(env.data).toMatchObject({ passed: 2, durationMs: 7 });
  });

  test("CI gates the reporter off entirely", () => {
    process.env.CI = "1";
    const shipped: Array<ErrorEnvelope | EventEnvelope> = [];
    const reporter = new SniperReporter({ shipImpl: (e) => shipped.push(e) });
    reporter.onFinished([failingFile(1)] as never);
    expect(shipped.length).toBe(0);
  });
});

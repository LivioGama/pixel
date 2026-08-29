import { describe, expect, test } from "bun:test";
import {
  historyRequestSchema,
  inspectRequestSchema,
  reviewRequestSchema,
  v1EnvelopeSchema,
} from "../src/contracts/v1.ts";

describe("v1 read contracts", () => {
  test("requires absolute repository paths", () => {
    expect(() => inspectRequestSchema.parse({ repoPath: "relative" })).toThrow();
    expect(
      inspectRequestSchema.parse({ repoPath: "/repo", files: ["hello.txt"] }),
    ).toEqual({ repoPath: "/repo", files: ["hello.txt"] });
  });

  test("bounds review and history requests", () => {
    expect(() => reviewRequestSchema.parse({ repoPath: "/repo", byteCap: 0 })).toThrow();
    expect(() => historyRequestSchema.parse({ repoPath: "/repo", limit: 101 })).toThrow();
    expect(historyRequestSchema.parse({ repoPath: "/repo" })).toEqual({
      repoPath: "/repo",
      ref: "HEAD",
      limit: 20,
      detail: "compact",
    });
  });

  test("accepts structured successful envelopes", () => {
    const success = {
      ok: true,
      requestId: "request-1",
      result: {},
    } as const;
    expect(JSON.parse(JSON.stringify(v1EnvelopeSchema.parse(success)))).toEqual(
      JSON.parse(JSON.stringify(success)),
    );
    expect(() =>
      v1EnvelopeSchema.parse({
        ...success,
        error: { code: "GIT_FAILED", message: "must not coexist" },
      }),
    ).toThrow();
  });

  test("rejects legacy envelope transport fields", () => {
    expect(() =>
      v1EnvelopeSchema.parse({
        ok: true,
        result: {},
        transport: "mcp",
        durationMs: 1,
      }),
    ).toThrow();
  });

  test("omits warnings entirely instead of sending an empty list", () => {
    expect(() => v1EnvelopeSchema.parse({ ok: true, result: {}, warnings: [] })).toThrow();
    expect(
      v1EnvelopeSchema.parse({
        ok: true,
        result: {},
        warnings: [{ code: "example", message: "one warning" }],
      }),
    ).toMatchObject({ warnings: [{ code: "example" }] });
  });

  test("requires the typed error branch to agree with ok", () => {
    const failure = {
      ok: false,
      requestId: "request-2",
      error: { code: "INVALID_REPOSITORY", message: "not a repository" },
    } as const;
    expect(JSON.parse(JSON.stringify(v1EnvelopeSchema.parse(failure)))).toEqual(
      JSON.parse(JSON.stringify(failure)),
    );
    expect(() => v1EnvelopeSchema.parse({ ...failure, ok: true })).toThrow();
  });
});

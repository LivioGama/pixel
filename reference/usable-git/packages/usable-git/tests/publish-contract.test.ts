import { describe, expect, test } from "bun:test";
import { publishRequestSchema } from "../src/contracts/v1/publish.ts";

describe("publish contract", () => {
  test("accepts an exact publish request with explicit expectations", () => {
    const request = publishRequestSchema.parse({
      repoPath: "/tmp/repository",
      files: ["nested/file.txt"],
      message: "message",
      requestId: "request-1",
      expected: {
        head: { kind: "unborn" },
        fingerprints: { "nested/file.txt": "a".repeat(64) },
      },
    });

    expect(request.files).toEqual(["nested/file.txt"]);
  });

  test("accepts a snapshot-token publish request", () => {
    const request = publishRequestSchema.parse({
      repoPath: "/tmp/repository",
      files: ["nested/file.txt"],
      message: "message",
      requestId: "request-1",
      snapshot: "a1b2c3d4e5f6",
    });

    expect(request.snapshot).toBe("a1b2c3d4e5f6");
  });

  test("requires exactly one of snapshot or expected", () => {
    const base = {
      repoPath: "/tmp/repository",
      files: ["file.txt"],
      message: "message",
      requestId: "request-1",
    };
    expect(() => publishRequestSchema.parse(base)).toThrow();
    expect(() =>
      publishRequestSchema.parse({
        ...base,
        snapshot: "a1b2c3d4e5f6",
        expected: {
          head: { kind: "unborn" },
          fingerprints: { "file.txt": "a".repeat(64) },
        },
      }),
    ).toThrow();
  });

  test("requires unique literal files and a fingerprint for every path", () => {
    const base = {
      repoPath: "/tmp/repository",
      message: "message",
      requestId: "request-1",
    };
    const expected = (fingerprints: Record<string, string>) => ({
      head: { kind: "unborn" as const },
      fingerprints,
    });

    for (const value of [
      {
        ...base,
        files: ["file.txt", "file.txt"],
        expected: expected({ "file.txt": "a".repeat(64) }),
      },
      { ...base, files: ["file.txt"], expected: expected({}) },
      {
        ...base,
        files: ["."],
        expected: expected({ ".": "a".repeat(64) }),
      },
      {
        ...base,
        files: ["../file.txt"],
        expected: expected({ "../file.txt": "a".repeat(64) }),
      },
      {
        ...base,
        files: ["*.txt"],
        expected: expected({ "*.txt": "a".repeat(64) }),
      },
    ]) {
      expect(() => publishRequestSchema.parse(value)).toThrow();
    }
  });

  test("validates nonblank commit messages without rewriting them", () => {
    const request = publishRequestSchema.parse({
      repoPath: "/tmp/repository",
      files: ["file.txt"],
      message: "  subject with intentional spacing  ",
      requestId: "request-message",
      expected: {
        head: { kind: "unborn" },
        fingerprints: { "file.txt": "a".repeat(64) },
      },
    });
    expect(request.message).toBe("  subject with intentional spacing  ");
    expect(() => publishRequestSchema.parse({ ...request, message: "   \n" })).toThrow();
  });
});

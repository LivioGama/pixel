import { describe, expect, test } from "bun:test";
import { pushRequestSchema } from "@usable-git/contracts/v1/push.ts";

const oid = "a".repeat(40);

const validFastForward = {
  repoPath: "/tmp/repository",
  remote: "origin",
  sourceRef: "refs/heads/main",
  targetRef: "refs/heads/main",
  requestId: "push-001",
  expectedSourceOid: oid,
  mode: { kind: "fast-forward" as const },
};

describe("push v1 contract", () => {
  test("accepts strict fast-forward and exact lease requests", () => {
    expect(pushRequestSchema.parse(validFastForward)).toEqual(validFastForward);

    const lease = {
      ...validFastForward,
      mode: {
        kind: "force-with-lease" as const,
        expectedTargetOid: "b".repeat(40),
      },
    };
    expect(pushRequestSchema.parse(lease)).toEqual(lease);
  });

  test.each([
    ["raw URL", { remote: "https://example.test/repository.git" }],
    ["short source ref", { sourceRef: "main" }],
    ["tag target", { targetRef: "refs/tags/v1" }],
    ["wildcard ref", { targetRef: "refs/heads/*" }],
    ["deletion refspec input", { sourceRef: "" }],
  ])("rejects %s", (_label, override) => {
    expect(() => pushRequestSchema.parse({ ...validFastForward, ...override })).toThrow();
  });

  test("requires exact target OID only in lease mode", () => {
    expect(() =>
      pushRequestSchema.parse({
        ...validFastForward,
        mode: { kind: "force-with-lease" },
      }),
    ).toThrow();
    expect(() =>
      pushRequestSchema.parse({
        ...validFastForward,
        mode: { kind: "fast-forward", expectedTargetOid: oid },
      }),
    ).toThrow();
  });
});

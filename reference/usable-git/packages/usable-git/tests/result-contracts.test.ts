import { describe, expect, test } from "bun:test";
import { historyResultSchema } from "../src/contracts/v1/history.ts";
import { inspectResultSchema } from "../src/contracts/v1/inspect.ts";
import { publishResultSchema } from "../src/contracts/v1/publish.ts";
import { reviewResultSchema } from "../src/contracts/v1/review.ts";
import { parseOperationResult } from "../src/contracts/v1/results.ts";

const oid = "a".repeat(40);

const inspectResult = {
  root: "/tmp/repository",
  branch: "main",
  head: oid,
  snapshot: "a1b2c3d4e5f6",
  remotes: [{ name: "origin", fetchUrl: "https://example.test/repo.git", pushUrl: null }],
  changes: [
    {
      path: "file.txt",
      status: "M.",
    },
  ],
};

const reviewResult = {
  items: [
    {
      scope: "staged" as const,
      path: "file.txt",
      patch: "diff --git a/file.txt b/file.txt\n",
      binary: false,
      additions: 1,
      deletions: 0,
      truncated: false,
    },
  ],
  bytes: 35,
  nextCursor: "cursor",
};

const historyResult = {
  head: { kind: "oid" as const, oid },
  commits: [
    {
      oid: oid.slice(0, 12),
      subject: "subject",
      author: "A",
      at: "2026-07-14T12:00:00+00:00",
    },
    {
      oid,
      parents: [],
      author: { name: "A", email: "a@example.test" },
      committer: { name: "C", email: "c@example.test" },
      authoredAt: "2026-07-14T12:00:00+00:00",
      committedAt: "2026-07-14T12:00:00+00:00",
      signatureStatus: "N",
      message: "subject\n",
    },
  ],
};

const publishResult = {
  commitOid: oid,
  committedPaths: ["file.txt"],
  head: { oid, branch: "main" },
  status: {
    staged: [],
    unstaged: [],
    untracked: [],
    conflicted: [],
  },
  warnings: [],
};

describe("v1 operation result contracts", () => {
  test("accepts each operation's exact runtime result shape", () => {
    expect(inspectResultSchema.parse(inspectResult)).toEqual(inspectResult);
    expect(reviewResultSchema.parse(reviewResult)).toEqual(reviewResult);
    expect(historyResultSchema.parse(historyResult)).toEqual(historyResult);
    expect(publishResultSchema.parse(publishResult)).toEqual(publishResult);
  });

  test("rejects unknown result fields at every operation boundary", () => {
    expect(() => inspectResultSchema.parse({ ...inspectResult, leaked: true })).toThrow();
    expect(() =>
      reviewResultSchema.parse({
        ...reviewResult,
        items: [{ ...reviewResult.items[0], leaked: true }],
      }),
    ).toThrow();
    expect(() =>
      historyResultSchema.parse({
        ...historyResult,
        commits: [{ ...historyResult.commits[0], leaked: true }],
      }),
    ).toThrow();
    expect(() => publishResultSchema.parse({ ...publishResult, leaked: true })).toThrow();
  });

  test("dispatches validation by operation instead of accepting unknown results", () => {
    expect(parseOperationResult("inspect", inspectResult)).toEqual(inspectResult);
    expect(parseOperationResult("review", reviewResult)).toEqual(reviewResult);
    expect(parseOperationResult("history", historyResult)).toEqual(historyResult);
    expect(parseOperationResult("publish", publishResult)).toEqual(publishResult);
    expect(() => parseOperationResult("inspect", reviewResult)).toThrow();
    expect(() => parseOperationResult("publish", {})).toThrow();
  });
});

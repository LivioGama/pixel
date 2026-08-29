import { describe, expect, test } from "bun:test";
import { mkdtemp, readdir, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

describe("benchmark CLI", () => {
  test("runs a short fixture matrix and writes paired artifacts", async () => {
    const outputDirectory = await mkdtemp(join(tmpdir(), "usable-git-benchmark-cli-"));
    try {
      const child = Bun.spawn([
        process.execPath,
        "benchmarks/run.ts",
        "--clients",
        "harness",
        "--scenarios",
        "inspect-dirty",
        "--trials",
        "1",
        "--seed",
        "91",
        "--allow-short-run",
        "--output",
        outputDirectory,
      ], {
        cwd: join(import.meta.dir, "../../.."),
        stdout: "pipe",
        stderr: "pipe",
      });
      const [stdout, stderr, exitCode] = await Promise.all([
        new Response(child.stdout).text(),
        new Response(child.stderr).text(),
        child.exited,
      ]);
      expect(exitCode).toBe(2);
      expect(stderr).toBe("");
      expect(stdout).toContain("NOT RELEASE-ELIGIBLE");
      expect((await readdir(outputDirectory)).sort()).toEqual([
        expect.stringMatching(/\.json$/),
        expect.stringMatching(/\.md$/),
      ]);
    } finally {
      await rm(outputDirectory, { recursive: true, force: true });
    }
  }, 20_000);
});

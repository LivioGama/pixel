import { describe, expect, test } from "bun:test";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { writeBenchmarkArtifact } from "../../../benchmarks/report.ts";
import { runBenchmarkMatrix } from "../../../benchmarks/runner.ts";

describe("benchmark report", () => {
  test("writes timestamped raw JSON and a truthful Markdown report", async () => {
    const outputDirectory = await mkdtemp(join(tmpdir(), "usable-git-benchmark-report-"));
    try {
      const artifact = await runBenchmarkMatrix({
        clients: ["harness"],
        scenarios: ["inspect-dirty"],
        trials: 1,
        seed: 8,
        allowShortRun: true,
      });
      const written = await writeBenchmarkArtifact(artifact, outputDirectory);
      expect(written.jsonPath).toMatch(/usable-git-benchmark-\d{8}T\d{6}Z\.json$/);
      expect(written.markdownPath).toMatch(/usable-git-benchmark-\d{8}T\d{6}Z\.md$/);

      const raw = JSON.parse(await readFile(written.jsonPath, "utf8"));
      const markdown = await readFile(written.markdownPath, "utf8");
      expect(raw.trials).toHaveLength(2);
      expect(markdown).toContain("NOT RELEASE-ELIGIBLE");
      expect(markdown).toContain("Git-related client token measurements unavailable");
      expect(markdown).toContain("Raw trial artifact");
    } finally {
      await rm(outputDirectory, { recursive: true, force: true });
    }
  }, 20_000);
});

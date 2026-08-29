import { describe, expect, test } from "bun:test";
import { join } from "node:path";
import { exists, readFile } from "node:fs/promises";

import { gainEventSchema } from "@usable-git/contracts/v1/gain";
import { baselineFor } from "@usable-git/gain/baselines";
import { estimateGain } from "@usable-git/gain/estimate";
import {
  createGainLedger,
  aggregateEvents,
} from "@usable-git/gain/ledger";
import {
  formatTextSummary,
  formatTextHistory,
  formatJson,
  formatCsv,
} from "@usable-git/gain/report";
import { runGainCli } from "@usable-git/gain/cli";
import { withTempDirectory } from "./support/temp";

const inspectBaseline = baselineFor("inspect");
const historyBaseline = baselineFor("history");
const shipBaseline = baselineFor("ship");

const baseEventInput = {
  operation: "inspect" as const,
  client: "codex" as const,
  transport: "mcp" as const,
  resultCode: "success" as const,
  envelopeBytes: 290,
  rawEquivalentBytes: inspectBaseline.rawEquivalentBytes,
  agentOpsRaw: inspectBaseline.agentOpsRaw,
  agentOpsActual: 1,
  gitSubprocessesRaw: inspectBaseline.gitSubprocessesRaw,
  gitSubprocessesActual: 0,
  durationMs: 0.89,
  tokensSaved: 0,
};

describe("gain baselines", () => {
  test("every operation has a baseline entry", () => {
    const operations = [
      "inspect", "review", "history", "diff",
      "publish", "push", "ship", "branch", "sync", "update", "search",
    ] as const;
    for (const op of operations) {
      const baseline = baselineFor(op);
      expect(baseline.rawEquivalentBytes).toBeGreaterThan(0);
      expect(baseline.agentOpsRaw).toBeGreaterThanOrEqual(1);
      expect(baseline.gitSubprocessesRaw).toBeGreaterThanOrEqual(1);
    }
  });
});

describe("gain estimate", () => {
  test("inspect with small envelope saves tokens from all three dimensions", () => {
    const estimate = estimateGain("inspect", 290, 0);
    expect(estimate.envelopeBytes).toBe(290);
    expect(estimate.rawEquivalentBytes).toBe(1841);
    expect(estimate.agentOpsActual).toBe(1);
    expect(estimate.agentOpsRaw).toBe(2);
    expect(estimate.gitSubprocessesActual).toBe(0);
    expect(estimate.gitSubprocessesRaw).toBe(2);
    // bytes saved: (1841-290)/4 = 387.75
    // ops saved: 1 * 80 = 80
    // subprocs saved: 2 * 40 = 80
    expect(estimate.tokensSaved).toBeCloseTo(387.75 + 80 + 80, 1);
  });

  test("ship saves the most because it replaces 7 ops and 8 subprocesses", () => {
    const estimate = estimateGain("ship", 500, 2);
    expect(estimate.tokensSaved).toBeGreaterThan(500);
  });

  test("an envelope larger than the raw baseline can go negative on bytes but ops still save", () => {
    const estimate = estimateGain("history", 12000, 1);
    // bytes negative, but 0 ops saved, 0 subprocs saved → only byte cost
    expect(estimate.tokensSaved).toBeLessThan(0);
  });
});

describe("gain ledger", () => {
  test("append writes a valid gain event and read returns it", async () => {
    await withTempDirectory("usable-git-gain-append-", async (directory) => {
      const stateRoot = join(directory, "state");
      const ledger = createGainLedger({ stateRoot });
      const result = await ledger.append({
        ...baseEventInput,
        repositoryIdentity: "/private/repo",
      });
      expect(result.written).toBe(true);
      if (result.written) {
        expect(result.repositoryHash).toMatch(/^[a-f0-9]{64}$/);
      }
      const events = await ledger.read();
      expect(events).toHaveLength(1);
      expect(events[0]!.operation).toBe("inspect");
      expect(events[0]!.repositoryHash).toBe(result.written ? result.repositoryHash : "");
      // No repo path leaks into the ledger file
      const raw = await readFile(ledger.path, "utf8");
      expect(raw).not.toContain("/private/repo");
    });
  });

  test("read returns empty array when ledger does not exist", async () => {
    await withTempDirectory("usable-git-gain-empty-", async (directory) => {
      const ledger = createGainLedger({ stateRoot: join(directory, "state") });
      const events = await ledger.read();
      expect(events).toEqual([]);
    });
  });

  test("readForRepository filters by salted hash", async () => {
    await withTempDirectory("usable-git-gain-filter-", async (directory) => {
      const stateRoot = join(directory, "state");
      const ledger = createGainLedger({ stateRoot });
      await ledger.append({ ...baseEventInput, repositoryIdentity: "/repo/alpha" });
      await ledger.append({ ...baseEventInput, repositoryIdentity: "/repo/beta" });

      const alphaEvents = await ledger.readForRepository("/repo/alpha");
      expect(alphaEvents).toHaveLength(1);
      const betaEvents = await ledger.readForRepository("/repo/beta");
      expect(betaEvents).toHaveLength(1);
      const allEvents = await ledger.read();
      expect(allEvents).toHaveLength(2);
    });
  });

  test("reset removes the ledger file", async () => {
    await withTempDirectory("usable-git-gain-reset-", async (directory) => {
      const stateRoot = join(directory, "state");
      const ledger = createGainLedger({ stateRoot });
      await ledger.append({ ...baseEventInput, repositoryIdentity: "/repo" });
      expect(await exists(ledger.path)).toBe(true);
      await ledger.reset();
      expect(await exists(ledger.path)).toBe(false);
      expect(await ledger.read()).toEqual([]);
    });
  });

  test("same repository identity produces the same hash across ledgers with the same salt", async () => {
    await withTempDirectory("usable-git-gain-salt-", async (directory) => {
      const stateRoot = join(directory, "state");
      const ledger = createGainLedger({ stateRoot });
      const h1 = await ledger.resolveRepositoryHash("/repo/x");
      const h2 = await ledger.resolveRepositoryHash("/repo/x");
      expect(h1).toBe(h2);
    });
  });

  test("different state roots produce different hashes (different salts)", async () => {
    await withTempDirectory("usable-git-gain-salts-", async (directory) => {
      const a = createGainLedger({ stateRoot: join(directory, "a") });
      const b = createGainLedger({ stateRoot: join(directory, "b") });
      const ha = await a.resolveRepositoryHash("/same/repo");
      const hb = await b.resolveRepositoryHash("/same/repo");
      expect(ha).not.toBe(hb);
    });
  });
});

describe("gain aggregation", () => {
  test("aggregates totals and per-operation breakdowns", async () => {
    await withTempDirectory("usable-git-gain-agg-", async (directory) => {
      const stateRoot = join(directory, "state");
      const ledger = createGainLedger({ stateRoot });
      const inspectEstimate = estimateGain("inspect", 290, 0);
      await ledger.append({
        ...baseEventInput,
        tokensSaved: inspectEstimate.tokensSaved,
        repositoryIdentity: "/repo",
      });
      const historyEstimate = estimateGain("history", 2881, 1);
      await ledger.append({
        ...baseEventInput,
        operation: "history",
        envelopeBytes: 2881,
        rawEquivalentBytes: historyBaseline.rawEquivalentBytes,
        agentOpsRaw: historyBaseline.agentOpsRaw,
        gitSubprocessesRaw: historyBaseline.gitSubprocessesRaw,
        gitSubprocessesActual: 1,
        tokensSaved: historyEstimate.tokensSaved,
        repositoryIdentity: "/repo",
      });
      const events = await ledger.read();
      const agg = aggregateEvents(events);
      expect(agg.totalOperations).toBe(2);
      expect(agg.byOperation).toHaveLength(2);
      expect(agg.totalTokensSaved).toBeGreaterThan(0);
      expect(agg.byDay.length).toBeGreaterThanOrEqual(1);
    });
  });

  test("empty events produce zeroed aggregate", () => {
    const agg = aggregateEvents([]);
    expect(agg.totalOperations).toBe(0);
    expect(agg.totalTokensSaved).toBe(0);
    expect(agg.byOperation).toEqual([]);
  });
});

describe("gain report formatters", () => {
  test("text summary renders header and operation table", () => {
    const events = [
      gainEventSchema.parse({
        version: "v1",
        timestamp: "2026-08-04T03:00:00.000Z",
        ...baseEventInput,
        tokensSaved: 547.75,
        repositoryHash: "a".repeat(64),
      }),
    ];
    const agg = aggregateEvents(events);
    const text = formatTextSummary(agg);
    expect(text).toContain("usable-git Token Savings");
    expect(text).toContain("inspect");
    expect(text).toContain("Tokens saved:");
  });

  test("text history renders recent operations", () => {
    const events = [
      gainEventSchema.parse({
        version: "v1",
        timestamp: "2026-08-04T03:00:00.000Z",
        ...baseEventInput,
        tokensSaved: 547.75,
        repositoryHash: "a".repeat(64),
      }),
    ];
    const text = formatTextHistory(events);
    expect(text).toContain("Recent Operations");
    expect(text).toContain("inspect");
  });

  test("json output is valid JSON", () => {
    const events = [
      gainEventSchema.parse({
        version: "v1",
        timestamp: "2026-08-04T03:00:00.000Z",
        ...baseEventInput,
        tokensSaved: 547.75,
        repositoryHash: "a".repeat(64),
      }),
    ];
    const json = formatJson(aggregateEvents(events));
    expect(() => JSON.parse(json)).not.toThrow();
  });

  test("csv output has header and one row per event", () => {
    const events = [
      gainEventSchema.parse({
        version: "v1",
        timestamp: "2026-08-04T03:00:00.000Z",
        ...baseEventInput,
        tokensSaved: 547.75,
        repositoryHash: "a".repeat(64),
      }),
    ];
    const csv = formatCsv(events);
    const lines = csv.trim().split("\n");
    expect(lines[0]).toContain("timestamp,operation,client");
    expect(lines).toHaveLength(2);
  });
});

describe("gain CLI", () => {
  test("gain with no events prints empty summary", async () => {
    await withTempDirectory("usable-git-gain-cli-empty-", async (directory) => {
      let out = "";
      let err = "";
      const code = await runGainCli([], {
        stateRoot: join(directory, "state"),
        repositoryIdentity: "/repo",
        writeStdout: (v) => { out += v; },
        writeStderr: (v) => { err += v; },
      });
      expect(code).toBe(0);
      expect(out).toContain("empty");
    });
  });

  test("gain after appends shows totals and operation table", async () => {
    await withTempDirectory("usable-git-gain-cli-data-", async (directory) => {
      const stateRoot = join(directory, "state");
      const ledger = createGainLedger({ stateRoot });
      await ledger.append({ ...baseEventInput, repositoryIdentity: "/repo" });
      await ledger.append({
        ...baseEventInput,
        operation: "ship",
        envelopeBytes: 500,
        rawEquivalentBytes: shipBaseline.rawEquivalentBytes,
        agentOpsRaw: shipBaseline.agentOpsRaw,
        gitSubprocessesRaw: shipBaseline.gitSubprocessesRaw,
        repositoryIdentity: "/repo",
      });

      let out = "";
      const code = await runGainCli([], {
        stateRoot,
        repositoryIdentity: "/repo",
        writeStdout: (v) => { out += v; },
        writeStderr: () => {},
      });
      expect(code).toBe(0);
      expect(out).toContain("usable-git Token Savings");
      expect(out).toContain("inspect");
      expect(out).toContain("ship");
    });
  });

  test("gain --format json produces valid JSON", async () => {
    await withTempDirectory("usable-git-gain-cli-json-", async (directory) => {
      const stateRoot = join(directory, "state");
      const ledger = createGainLedger({ stateRoot });
      await ledger.append({ ...baseEventInput, repositoryIdentity: "/repo" });

      let out = "";
      const code = await runGainCli(["--format", "json"], {
        stateRoot,
        repositoryIdentity: "/repo",
        writeStdout: (v) => { out += v; },
        writeStderr: () => {},
      });
      expect(code).toBe(0);
      expect(() => JSON.parse(out)).not.toThrow();
      const parsed = JSON.parse(out);
      expect(parsed.totalOperations).toBe(1);
    });
  });

  test("gain --format csv produces CSV with header", async () => {
    await withTempDirectory("usable-git-gain-cli-csv-", async (directory) => {
      const stateRoot = join(directory, "state");
      const ledger = createGainLedger({ stateRoot });
      await ledger.append({ ...baseEventInput, repositoryIdentity: "/repo" });

      let out = "";
      const code = await runGainCli(["--format", "csv"], {
        stateRoot,
        repositoryIdentity: "/repo",
        writeStdout: (v) => { out += v; },
        writeStderr: () => {},
      });
      expect(code).toBe(0);
      expect(out).toContain("timestamp,operation");
    });
  });

  test("gain --history shows recent operations", async () => {
    await withTempDirectory("usable-git-gain-cli-history-", async (directory) => {
      const stateRoot = join(directory, "state");
      const ledger = createGainLedger({ stateRoot });
      await ledger.append({ ...baseEventInput, repositoryIdentity: "/repo" });

      let out = "";
      const code = await runGainCli(["--history"], {
        stateRoot,
        repositoryIdentity: "/repo",
        writeStdout: (v) => { out += v; },
        writeStderr: () => {},
      });
      expect(code).toBe(0);
      expect(out).toContain("Recent Operations");
      expect(out).toContain("inspect");
    });
  });

  test("gain --reset without --yes refuses", async () => {
    await withTempDirectory("usable-git-gain-cli-reset-guard-", async (directory) => {
      let err = "";
      const code = await runGainCli(["--reset"], {
        stateRoot: join(directory, "state"),
        repositoryIdentity: "/repo",
        writeStdout: () => {},
        writeStderr: (v) => { err += v; },
      });
      expect(code).toBe(64);
      expect(err).toContain("--yes");
    });
  });

  test("gain --reset --yes clears the ledger", async () => {
    await withTempDirectory("usable-git-gain-cli-reset-yes-", async (directory) => {
      const stateRoot = join(directory, "state");
      const ledger = createGainLedger({ stateRoot });
      await ledger.append({ ...baseEventInput, repositoryIdentity: "/repo" });
      expect(await ledger.read()).toHaveLength(1);

      let out = "";
      const code = await runGainCli(["--reset", "--yes"], {
        stateRoot,
        repositoryIdentity: "/repo",
        writeStdout: (v) => { out += v; },
        writeStderr: () => {},
      });
      expect(code).toBe(0);
      expect(out).toContain("reset");
      expect(await ledger.read()).toEqual([]);
    });
  });

  test("gain --project filters to current repo only", async () => {
    await withTempDirectory("usable-git-gain-cli-project-", async (directory) => {
      const stateRoot = join(directory, "state");
      const ledger = createGainLedger({ stateRoot });
      await ledger.append({ ...baseEventInput, repositoryIdentity: "/repo/alpha" });
      await ledger.append({ ...baseEventInput, repositoryIdentity: "/repo/beta" });

      let out = "";
      const code = await runGainCli(["--project", "--format", "json"], {
        stateRoot,
        repositoryIdentity: "/repo/alpha",
        writeStdout: (v) => { out += v; },
        writeStderr: () => {},
      });
      expect(code).toBe(0);
      const parsed = JSON.parse(out);
      expect(parsed.totalOperations).toBe(1);
    });
  });
});

import { resolve } from "node:path";

import { createGainLedger, aggregateEvents } from "./ledger.ts";
import {
  formatTextSummary,
  formatTextHistory,
  formatTimeBuckets,
  formatJson,
  formatCsv,
  type ReportFormat,
} from "./report.ts";

export type GainCliOptions = {
  history?: boolean;
  daily?: boolean;
  weekly?: boolean;
  monthly?: boolean;
  all?: boolean;
  format?: ReportFormat;
  project?: boolean;
  reset?: boolean;
  yes?: boolean;
  stateRoot?: string;
  repositoryIdentity?: string;
};

const usage = `Usage:
  usable-git gain [--history] [--daily|--weekly|--monthly|--all]
                  [--format text|json|csv] [--project] [--reset [--yes]]
`;

export const runGainCli = async (
  args: string[] = process.argv.slice(2),
  overrides: { stateRoot?: string; repositoryIdentity?: string; writeStdout?: (v: string) => void; writeStderr?: (v: string) => void } = {},
): Promise<number> => {
  const writeStdout = overrides.writeStdout ?? ((v: string) => process.stdout.write(v));
  const writeStderr = overrides.writeStderr ?? ((v: string) => process.stderr.write(v));

  const options = parseGainArgs(args);
  if (!options) {
    writeStderr(usage);
    return 64;
  }

  const ledger = createGainLedger({ stateRoot: overrides.stateRoot });

  if (options.reset) {
    if (!options.yes) {
      writeStderr("Refusing to reset gain ledger without --yes. Run: usable-git gain --reset --yes\n");
      return 64;
    }
    await ledger.reset();
    writeStdout("Gain ledger reset.\n");
    return 0;
  }

  const events = options.project && overrides.repositoryIdentity
    ? await ledger.readForRepository(overrides.repositoryIdentity)
    : await ledger.read();

  const format = options.format ?? "text";

  if (format === "json") {
    const agg = aggregateEvents(events);
    if (options.history) {
      writeStdout(formatJson({ summary: agg, recent: agg.recent }));
    } else if (options.all) {
      writeStdout(formatJson({ summary: agg, byDay: agg.byDay, byWeek: agg.byWeek, byMonth: agg.byMonth }));
    } else if (options.daily) {
      writeStdout(formatJson({ summary: agg, byDay: agg.byDay }));
    } else if (options.weekly) {
      writeStdout(formatJson({ summary: agg, byWeek: agg.byWeek }));
    } else if (options.monthly) {
      writeStdout(formatJson({ summary: agg, byMonth: agg.byMonth }));
    } else {
      writeStdout(formatJson(agg));
    }
    return 0;
  }

  if (format === "csv") {
    writeStdout(formatCsv(events));
    return 0;
  }

  // text format
  const agg = aggregateEvents(events);
  const parts: string[] = [formatTextSummary(agg)];

  if (options.history) {
    parts.push(formatTextHistory(events));
  }
  if (options.all || options.daily) {
    parts.push(formatTimeBuckets(agg.byDay, "Daily"));
  }
  if (options.all || options.weekly) {
    parts.push(formatTimeBuckets(agg.byWeek, "Weekly"));
  }
  if (options.all || options.monthly) {
    parts.push(formatTimeBuckets(agg.byMonth, "Monthly"));
  }

  writeStdout(parts.join("\n"));
  return 0;
};

const parseGainArgs = (args: string[]): GainCliOptions | null => {
  const options: GainCliOptions = {};
  for (let i = 0; i < args.length; i += 1) {
    const arg = args[i]!;
    switch (arg) {
      case "--history":
        options.history = true;
        break;
      case "--daily":
        options.daily = true;
        break;
      case "--weekly":
        options.weekly = true;
        break;
      case "--monthly":
        options.monthly = true;
        break;
      case "--all":
        options.all = true;
        break;
      case "--project":
        options.project = true;
        break;
      case "--reset":
        options.reset = true;
        break;
      case "--yes":
        options.yes = true;
        break;
      case "--format": {
        const value = args[i + 1];
        if (!value || !["text", "json", "csv"].includes(value)) return null;
        options.format = value as ReportFormat;
        i += 1;
        break;
      }
      case "-h":
      case "--help":
        return null;
      default:
        return null;
    }
  }
  return options;
};

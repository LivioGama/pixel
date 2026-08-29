import type { GainEvent } from "../contracts/v1/gain.ts";
import type { LedgerAggregate, TimeBucket } from "./ledger.ts";

export type ReportFormat = "text" | "json" | "csv";

const formatTokens = (value: number): string => {
  const abs = Math.abs(value);
  if (abs >= 1_000_000) return `${(value / 1_000_000).toFixed(1)}M`;
  if (abs >= 1_000) return `${(value / 1_000).toFixed(1)}K`;
  return value.toFixed(0);
};

const formatBytes = (value: number): string => {
  if (value >= 1_000_000) return `${(value / 1_000_000).toFixed(1)}MB`;
  if (value >= 1_000) return `${(value / 1_000).toFixed(1)}KB`;
  return `${value}B`;
};

const bar = (pct: number, width = 24): string => {
  const filled = Math.round((pct / 100) * width);
  return `${"█".repeat(filled)}${"░".repeat(width - filled)}`;
};

export const formatTextSummary = (agg: LedgerAggregate): string => {
  if (agg.totalOperations === 0) {
    return "usable-git Gain Ledger (empty)\nNo operations recorded yet.\n";
  }
  const lines: string[] = [
    "usable-git Token Savings",
    "════════════════════════════════════════════════════════════",
    "",
    `Total operations:  ${agg.totalOperations}`,
    `Envelope bytes:    ${formatBytes(agg.totalEnvelopeBytes)}`,
    `Raw equivalent:    ${formatBytes(agg.totalRawEquivalentBytes)}`,
    `Tokens saved:      ${formatTokens(agg.totalTokensSaved)} (${agg.avgSavingsPct.toFixed(1)}%)`,
    `Agent ops saved:   ${agg.totalAgentOpsSaved}`,
    `Subprocs saved:    ${agg.totalSubprocessesSaved}`,
    `Efficiency meter:  ${bar(agg.avgSavingsPct)} ${agg.avgSavingsPct.toFixed(1)}%`,
    "",
    "By Operation",
    "──────────────────────────────────────────────────────────────────────",
  ];
  const header = `  #  Operation   Count    Saved    Avg%    Envelope   Raw`;
  lines.push(header);
  lines.push("──────────────────────────────────────────────────────────────────────");
  agg.byOperation.forEach((entry, index) => {
    lines.push(
      `${String(index + 1).padStart(2)}. ${entry.operation.padEnd(12)} ` +
        `${String(entry.count).padStart(6)}  ` +
        `${formatTokens(entry.tokensSaved).padStart(8)}  ` +
        `${entry.avgPct.toFixed(1).padStart(5)}%  ` +
        `${formatBytes(entry.envelopeBytes).padStart(8)}   ` +
        `${formatBytes(entry.rawBytes).padStart(8)}`,
    );
  });
  lines.push("──────────────────────────────────────────────────────────────────────");
  return `${lines.join("\n")}\n`;
};

export const formatTextHistory = (events: GainEvent[]): string => {
  if (events.length === 0) return "No operations recorded yet.\n";
  const lines: string[] = ["Recent Operations", "──────────────────────────────────────"];
  for (const event of events.slice(-20).reverse()) {
    const time = event.timestamp.slice(5, 16).replace("T", " ");
    const saved = formatTokens(event.tokensSaved);
    const pct = event.rawEquivalentBytes > 0
      ? Math.max(0, (1 - event.envelopeBytes / event.rawEquivalentBytes) * 100).toFixed(0)
      : "0";
    const marker = event.tokensSaved > 0 ? "▲" : event.tokensSaved < 0 ? "▼" : "•";
    lines.push(
      `${time} ${marker} ${event.operation.padEnd(10)} ` +
        `${saved.padStart(8)} (${pct}%) [${event.client}/${event.transport}]`,
    );
  }
  return `${lines.join("\n")}\n`;
};

export const formatTimeBuckets = (buckets: TimeBucket[], label: string): string => {
  if (buckets.length === 0) return `No ${label} data yet.\n`;
  const lines: string[] = [`${label} Breakdown`, "────────────────────────────────────────────"];
  for (const bucket of buckets) {
    lines.push(
      `${bucket.bucket}  ops=${String(bucket.count).padStart(5)}  saved=${formatTokens(bucket.tokensSaved).padStart(8)}`,
    );
  }
  return `${lines.join("\n")}\n`;
};

export const formatJson = (data: unknown): string => `${JSON.stringify(data, null, 2)}\n`;

const csvEscape = (value: string | number): string => {
  const str = String(value);
  return /[,"\n]/.test(str) ? `"${str.replace(/"/g, '""')}"` : str;
};

export const formatCsv = (events: GainEvent[]): string => {
  const header = [
    "timestamp",
    "operation",
    "client",
    "transport",
    "resultCode",
    "envelopeBytes",
    "rawEquivalentBytes",
    "agentOpsRaw",
    "agentOpsActual",
    "gitSubprocessesRaw",
    "gitSubprocessesActual",
    "durationMs",
    "tokensSaved",
  ];
  const rows = events.map((e) =>
    [
      e.timestamp,
      e.operation,
      e.client,
      e.transport,
      e.resultCode,
      e.envelopeBytes,
      e.rawEquivalentBytes,
      e.agentOpsRaw,
      e.agentOpsActual,
      e.gitSubprocessesRaw,
      e.gitSubprocessesActual,
      e.durationMs,
      e.tokensSaved,
    ]
      .map(csvEscape)
      .join(","),
  );
  return `${[header.join(","), ...rows].join("\n")}\n`;
};

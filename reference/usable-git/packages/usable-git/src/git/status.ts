export type BranchStatus = {
  oid: string | null;
  head: string | null;
  upstream: string | null;
  ahead: number;
  behind: number;
};

export type StatusChange = {
  path: string;
  originalPath?: string;
  indexStatus: string;
  worktreeStatus: string;
  indexOid?: string;
  kind: "ordinary" | "renamed" | "unmerged" | "untracked" | "ignored";
  conflicted: boolean;
};

export type ParsedStatus = {
  branch: BranchStatus;
  changes: StatusChange[];
};

const defaultBranch = (): BranchStatus => ({
  oid: null,
  head: null,
  upstream: null,
  ahead: 0,
  behind: 0,
});

const ordinary = (record: string): StatusChange | undefined => {
  const match = /^1 ([^ ]{2}) ([^ ]+) ([^ ]+) ([^ ]+) ([^ ]+) ([^ ]+) ([^ ]+) ([\s\S]*)$/.exec(
    record,
  );
  if (!match?.[1] || match[8] === undefined) return undefined;
  return {
    path: match[8],
    indexStatus: match[1][0] ?? ".",
    worktreeStatus: match[1][1] ?? ".",
    indexOid: match[7],
    kind: "ordinary",
    conflicted: match[1].includes("U"),
  };
};

const renamed = (record: string, originalPath: string | undefined): StatusChange | undefined => {
  const match = /^2 ([^ ]{2}) ([^ ]+) ([^ ]+) ([^ ]+) ([^ ]+) ([^ ]+) ([^ ]+) ([^ ]+) ([\s\S]*)$/.exec(
    record,
  );
  if (!match?.[1] || match[9] === undefined) return undefined;
  return {
    path: match[9],
    ...(originalPath === undefined ? {} : { originalPath }),
    indexStatus: match[1][0] ?? ".",
    worktreeStatus: match[1][1] ?? ".",
    indexOid: match[7],
    kind: "renamed",
    conflicted: match[1].includes("U"),
  };
};

const unmerged = (record: string): StatusChange | undefined => {
  const match = /^u ([^ ]{2}) ([^ ]+) ([^ ]+) ([^ ]+) ([^ ]+) ([^ ]+) ([^ ]+) ([^ ]+) ([^ ]+) ([\s\S]*)$/.exec(
    record,
  );
  if (!match?.[1] || match[10] === undefined) return undefined;
  return {
    path: match[10],
    indexStatus: match[1][0] ?? "U",
    worktreeStatus: match[1][1] ?? "U",
    indexOid: match[8],
    kind: "unmerged",
    conflicted: true,
  };
};

export const parsePorcelainV2 = (output: string): ParsedStatus => {
  const branch = defaultBranch();
  const changes: StatusChange[] = [];
  const records = output.split("\0");

  for (let index = 0; index < records.length; index += 1) {
    const record = records[index];
    if (!record) continue;
    if (record.startsWith("# branch.oid ")) branch.oid = record.slice(13) === "(initial)" ? null : record.slice(13);
    else if (record.startsWith("# branch.head ")) {
      const value = record.slice(14);
      branch.head = value === "(detached)" ? null : value;
    } else if (record.startsWith("# branch.upstream ")) branch.upstream = record.slice(18);
    else if (record.startsWith("# branch.ab ")) {
      const match = /^# branch\.ab \+(\d+) -(\d+)$/.exec(record);
      if (match) {
        branch.ahead = Number(match[1]);
        branch.behind = Number(match[2]);
      }
    } else if (record.startsWith("1 ")) {
      const change = ordinary(record);
      if (change) changes.push(change);
    } else if (record.startsWith("2 ")) {
      const change = renamed(record, records[index + 1]);
      index += 1;
      if (change) changes.push(change);
    } else if (record.startsWith("u ")) {
      const change = unmerged(record);
      if (change) changes.push(change);
    } else if (record.startsWith("? ")) {
      changes.push({
        path: record.slice(2),
        indexStatus: "?",
        worktreeStatus: "?",
        kind: "untracked",
        conflicted: false,
      });
    } else if (record.startsWith("! ")) {
      changes.push({
        path: record.slice(2),
        indexStatus: "!",
        worktreeStatus: "!",
        kind: "ignored",
        conflicted: false,
      });
    }
  }

  return { branch, changes };
};

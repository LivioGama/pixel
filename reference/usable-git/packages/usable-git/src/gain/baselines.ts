import type { Operation } from "../contracts/v1.ts";

// Per-operation raw-git equivalent baseline. Conservative estimates derived
// from the benchmark fixtures (small dirty repo, history-20) and the raw-git
// command chains documented in benchmarks/runner.ts. These represent what an
// agent would produce using raw git to achieve the same outcome.
export type RawBaseline = {
  // Estimated bytes of output the raw-git equivalent would produce.
  rawEquivalentBytes: number;
  // Number of agent-facing operations in the raw-git chain.
  agentOpsRaw: number;
  // Number of git subprocesses in the raw-git chain.
  gitSubprocessesRaw: number;
};

export const RAW_BASELINES: Readonly<Record<Operation, RawBaseline>> = {
  // git status --porcelain=v2 -z --branch --untracked-files=all
  // + git rev-list --walk-reflogs --count refs/stash
  inspect: { rawEquivalentBytes: 1841, agentOpsRaw: 2, gitSubprocessesRaw: 2 },
  // git status --porcelain=v2 -z + git diff --staged + git diff
  review: { rawEquivalentBytes: 2200, agentOpsRaw: 3, gitSubprocessesRaw: 3 },
  // git log --format=fuller -20
  history: { rawEquivalentBytes: 9118, agentOpsRaw: 1, gitSubprocessesRaw: 1 },
  // git diff <base>..<target>
  diff: { rawEquivalentBytes: 4000, agentOpsRaw: 1, gitSubprocessesRaw: 1 },
  // git status + git diff -- <file> + git rev-parse HEAD
  // + git commit --only -m <msg> -- <file> + git status
  publish: { rawEquivalentBytes: 3000, agentOpsRaw: 5, gitSubprocessesRaw: 5 },
  // git rev-parse <src> + git push <remote> <src>:<tgt> + git rev-parse <src>
  push: { rawEquivalentBytes: 1500, agentOpsRaw: 3, gitSubprocessesRaw: 3 },
  // publish chain + push chain
  ship: { rawEquivalentBytes: 4500, agentOpsRaw: 7, gitSubprocessesRaw: 8 },
  // git rev-parse HEAD + git branch <name> (or git checkout)
  branch: { rawEquivalentBytes: 800, agentOpsRaw: 2, gitSubprocessesRaw: 2 },
  // git fetch <remote> <branches>
  sync: { rawEquivalentBytes: 1200, agentOpsRaw: 1, gitSubprocessesRaw: 1 },
  // git rev-parse HEAD + git merge --ff-only <oid>
  update: { rawEquivalentBytes: 1000, agentOpsRaw: 2, gitSubprocessesRaw: 2 },
  // Conservative slice of the measured archeology episode (2026-08-27
  // opencode/ship-fast session): 54 tool calls and ~1.8M input tokens across
  // repeated `git log --grep`, per-commit `git show`, and pickaxe guesses to
  // answer one history question. Baselined at a 12-command raw-git chain.
  search: { rawEquivalentBytes: 48_000, agentOpsRaw: 12, gitSubprocessesRaw: 12 },
};

export const baselineFor = (operation: Operation): RawBaseline => RAW_BASELINES[operation]!;

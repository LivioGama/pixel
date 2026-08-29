import { AsyncLocalStorage } from "node:async_hooks";
import { Buffer } from "node:buffer";

export type GitRunResult = {
  stdout: string;
  stderr: string;
  exitCode: number;
  processCount: 1;
  timedOut?: boolean;
  outputLimitExceeded?: boolean;
  stdoutTruncated?: boolean;
  stderrTruncated?: boolean;
};

export type GitSpawnEvent = {
  argv: string[];
  cwd: string;
};

export type GitRunnerOptions = {
  onSpawn?: (event: GitSpawnEvent) => void;
  timeoutMs?: number;
  maxOutputBytes?: number;
};

type GitMetrics = { gitSubprocessCount: number };
const operationMetrics = new AsyncLocalStorage<GitMetrics>();
const failedOperationMetrics = new WeakMap<object, number>();
const defaultTimeoutMs = 120_000;
const defaultMaxOutputBytes = 1_048_576;

const positiveInteger = (value: number | undefined, fallback: number, name: string) => {
  const resolved = value ?? fallback;
  if (!Number.isSafeInteger(resolved) || resolved <= 0) {
    throw new TypeError(`${name} must be a positive integer`);
  }
  return resolved;
};

export const withGitMetrics = async <T>(operation: () => Promise<T>) => {
  const metrics: GitMetrics = { gitSubprocessCount: 0 };
  try {
    const result = await operationMetrics.run(metrics, operation);
    return { result, gitSubprocessCount: metrics.gitSubprocessCount };
  } catch (error) {
    if ((typeof error === "object" && error !== null) || typeof error === "function") {
      failedOperationMetrics.set(error, metrics.gitSubprocessCount);
    }
    throw error;
  }
};

export const gitSubprocessCountForError = (error: unknown) =>
  ((typeof error === "object" && error !== null) || typeof error === "function")
    ? failedOperationMetrics.get(error) ?? 0
    : 0;

export class GitCommandError extends Error {
  readonly argv: string[];
  readonly cwd: string;
  readonly exitCode: number;
  readonly stdout: string;
  readonly stderr: string;
  readonly timedOut: boolean;
  readonly outputLimitExceeded: boolean;

  constructor(event: GitSpawnEvent, result: GitRunResult) {
    super(
      result.timedOut
        ? "git command timed out"
        : result.outputLimitExceeded
          ? "git command exceeded the output limit"
          : result.stderr.trim() || `git exited with status ${result.exitCode}`,
    );
    this.name = "GitCommandError";
    this.argv = event.argv;
    this.cwd = event.cwd;
    this.exitCode = result.exitCode;
    this.stdout = result.stdout;
    this.stderr = result.stderr;
    this.timedOut = result.timedOut ?? false;
    this.outputLimitExceeded = result.outputLimitExceeded ?? false;
  }
}

const redactCredentials = (value: string) => value
  .replace(/([a-z][a-z0-9+.-]*:\/\/)[^\s/@]+:[^\s/@]+@/gi, "$1<redacted>@")
  .replace(/\b(Bearer|Basic)\s+[A-Za-z0-9._~+/=-]+/gi, "$1 <redacted>")
  .replace(/\b(?:gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,})\b/g, "<redacted>")
  .replace(
    /([?&](?:access_token|api_key|auth|key|passwd|password|token)=)[^&\s]+/gi,
    "$1<redacted>",
  );

const boundUtf8 = (value: string, maxBytes: number) => {
  const encoded = Buffer.from(value);
  if (encoded.byteLength <= maxBytes) return { value, truncated: false };
  return { value: encoded.subarray(0, maxBytes).toString("utf8"), truncated: true };
};

const readBounded = async (
  stream: ReadableStream<Uint8Array<ArrayBuffer>>,
  maxBytes: number,
  onLimit: () => void,
) => {
  const reader = stream.getReader();
  const chunks: Uint8Array[] = [];
  let retainedBytes = 0;
  let truncated = false;
  while (true) {
    const { done, value } = await reader.read();
    if (done) break;
    const remaining = maxBytes - retainedBytes;
    if (remaining > 0) {
      const retained = value.byteLength > remaining ? value.subarray(0, remaining) : value;
      chunks.push(retained.slice());
      retainedBytes += retained.byteLength;
    }
    if (value.byteLength > remaining) {
      truncated = true;
      onLimit();
    }
  }
  const captured = Buffer.concat(chunks).toString("utf8");
  const redacted = redactCredentials(
    truncated ? captured.replace(/\S*$/, "<truncated>") : captured,
  );
  const bounded = boundUtf8(redacted, maxBytes);
  return { value: bounded.value, truncated: truncated || bounded.truncated };
};

const gitEnvironment = () => {
  const environment = { ...process.env };
  for (const key of [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_CONFIG",
    "GIT_CONFIG_GLOBAL",
    "GIT_CONFIG_SYSTEM",
    "GIT_CONFIG_NOSYSTEM",
    "GIT_DIFF_OPTS",
    "GIT_EXTERNAL_DIFF",
    "ANTHROPIC_API_KEY",
  ]) {
    delete environment[key];
  }
  for (const key of Object.keys(environment)) {
    if (
      key === "GIT_CONFIG_COUNT" ||
      key.startsWith("GIT_CONFIG_KEY_") ||
      key.startsWith("GIT_CONFIG_VALUE_")
    ) {
      delete environment[key];
    }
  }

  return {
    ...environment,
    LC_ALL: "C",
    LANG: "C",
    PAGER: "cat",
    GIT_PAGER: "cat",
    GIT_EXTERNAL_DIFF: "",
    GIT_OPTIONAL_LOCKS: "0",
    GIT_NO_LAZY_FETCH: "1",
    GIT_TERMINAL_PROMPT: "0",
    GCM_INTERACTIVE: "never",
    GIT_LITERAL_PATHSPECS: "1",
  };
};

export const createGitRunner = (options: GitRunnerOptions = {}) => ({
  run: async (cwd: string, args: string[], input?: Uint8Array): Promise<GitRunResult> => {
    const timeoutMs = positiveInteger(options.timeoutMs, defaultTimeoutMs, "timeoutMs");
    const maxOutputBytes = positiveInteger(
      options.maxOutputBytes,
      defaultMaxOutputBytes,
      "maxOutputBytes",
    );
    const argv = [
      "git",
      "--no-pager",
      "-c",
      "color.ui=false",
      "-c",
      "core.pager=cat",
      "-c",
      "diff.external=",
      ...args,
    ];
    const event = { argv, cwd };
    options.onSpawn?.(event);
    const child = Bun.spawn(argv, {
      cwd,
      detached: true,
      env: gitEnvironment(),
      stdin: input ? Buffer.from(input) : "ignore",
      stdout: "pipe",
      stderr: "pipe",
    });
    const metrics = operationMetrics.getStore();
    if (metrics) metrics.gitSubprocessCount += 1;
    let timedOut = false;
    let outputLimitExceeded = false;
    let terminated = false;
    const terminate = () => {
      if (terminated) return;
      terminated = true;
      if (child.exitCode !== null) return;
      try {
        if (process.platform === "win32") child.kill("SIGKILL");
        else process.kill(-child.pid, "SIGKILL");
      } catch {
        child.kill("SIGKILL");
      }
    };
    const timer = setTimeout(() => {
      timedOut = true;
      terminate();
    }, timeoutMs);
    timer.unref();
    const onOutputLimit = () => {
      outputLimitExceeded = true;
      terminate();
    };
    const [stdout, stderr, rawExitCode] = await Promise.all([
      readBounded(child.stdout, maxOutputBytes, onOutputLimit),
      readBounded(child.stderr, maxOutputBytes, onOutputLimit),
      child.exited,
    ]).finally(() => clearTimeout(timer));
    const exitCode = timedOut ? 124 : outputLimitExceeded && rawExitCode === 0 ? 125 : rawExitCode;
    const result: GitRunResult = {
      stdout: stdout.value,
      stderr: stderr.value,
      exitCode,
      processCount: 1,
      ...(timedOut ? { timedOut: true } : {}),
      ...(outputLimitExceeded ? { outputLimitExceeded: true } : {}),
      ...(stdout.truncated ? { stdoutTruncated: true } : {}),
      ...(stderr.truncated ? { stderrTruncated: true } : {}),
    };

    return result;
  },
  runChecked: async (cwd: string, args: string[], input?: Uint8Array) => {
    const argv = [
      "git",
      "--no-pager",
      "-c",
      "color.ui=false",
      "-c",
      "core.pager=cat",
      "-c",
      "diff.external=",
      ...args,
    ];
    const event = { argv, cwd };
    const runner = createGitRunner(options);
    const result = await runner.run(cwd, args, input);
    if (result.exitCode !== 0 || result.timedOut || result.outputLimitExceeded) {
      throw new GitCommandError(event, result);
    }
    return result;
  },
});

export type GitRunner = ReturnType<typeof createGitRunner>;

export const git = createGitRunner();

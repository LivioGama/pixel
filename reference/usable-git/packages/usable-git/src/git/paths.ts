import { lstat } from "node:fs/promises";
import { isAbsolute, normalize, relative, resolve, sep } from "node:path";
import { git, type GitRunner } from "./runner.ts";

const isOutside = (root: string, candidate: string) => {
  const fromRoot = relative(root, candidate);
  return fromRoot === ".." || fromRoot.startsWith(`..${sep}`) || isAbsolute(fromRoot);
};

const trackedModes = async (root: string, files: string[], runner: GitRunner) => {
  const result = await runner.runChecked(root, ["ls-files", "--stage", "-z", "--", ...files]);
  const modes = new Map<string, string>();
  for (const record of result.stdout.split("\0")) {
    if (!record) continue;
    const match = /^(\d+) [a-f0-9]+ \d+\t([\s\S]+)$/.exec(record);
    if (match?.[1] && match[2]) modes.set(match[2], match[1]);
  }
  const deleted = await runner.runChecked(root, [
    "diff",
    "--cached",
    "--raw",
    "-z",
    "--diff-filter=D",
    "--",
    ...files,
  ]);
  const deletionRecords = deleted.stdout.split("\0");
  for (let index = 0; index < deletionRecords.length - 1; index += 2) {
    const header = deletionRecords[index];
    const path = deletionRecords[index + 1];
    const match = header ? /^:(\d+) 0+ [a-f0-9]+ 0+ D$/.exec(header) : undefined;
    if (match?.[1] && path) modes.set(path, match[1]);
  }
  return modes;
};

export const validateLiteralFiles = async (
  root: string,
  files: string[],
  runner: GitRunner = git,
): Promise<string[]> => {
  const unique = [...new Set(files)];
  if (unique.length === 0) throw new Error("At least one file is required");
  if (unique.length !== files.length) throw new Error("Duplicate file paths are unsupported");

  for (const file of unique) {
    if (
      !file ||
      file === "." ||
      isAbsolute(file) ||
      file.startsWith(":") ||
      /[*?[\]]/.test(file) ||
      normalize(file) !== file ||
      file.split(/[\\/]/).includes("..")
    ) {
      throw new Error(`Invalid literal file path: ${JSON.stringify(file)}`);
    }
    if (isOutside(root, resolve(root, file))) {
      throw new Error(`File escapes repository: ${JSON.stringify(file)}`);
    }
  }

  const modes = await trackedModes(root, unique, runner);
  for (const file of unique) {
    if (modes.get(file) === "160000") {
      throw new Error(`Gitlinks are unsupported: ${JSON.stringify(file)}`);
    }
    try {
      const stats = await lstat(resolve(root, file));
      if (stats.isDirectory()) throw new Error(`Directories are unsupported: ${JSON.stringify(file)}`);
    } catch (error) {
      if (error instanceof Error && "code" in error && error.code === "ENOENT") {
        if (!modes.has(file)) throw new Error(`File does not exist: ${JSON.stringify(file)}`);
      } else {
        throw error;
      }
    }

    if (!modes.has(file)) {
      const ignored = await runner.runChecked(root, [
        "ls-files",
        "--others",
        "--ignored",
        "--exclude-standard",
        "-z",
        "--",
        file,
      ]);
      if (ignored.stdout) throw new Error(`Ignored files are unsupported: ${JSON.stringify(file)}`);
    }
  }

  return unique;
};

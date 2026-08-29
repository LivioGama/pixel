import { resolve } from "node:path";
import { realpath } from "node:fs/promises";
import { git, type GitRunner } from "./runner.ts";

export type RepositoryInfo = {
  root: string;
  gitDir: string;
  commonDir: string;
  isBare: boolean;
};

const oneLine = (value: string) => value.replace(/[\r\n]+$/, "");

export const discoverRepository = async (
  repoPath: string,
  runner: GitRunner = git,
): Promise<RepositoryInfo> => {
  const cwd = await realpath(resolve(repoPath));
  const bareResult = await runner.runChecked(cwd, ["rev-parse", "--is-bare-repository"]);
  const isBare = oneLine(bareResult.stdout) === "true";
  const gitDirResult = await runner.runChecked(cwd, [
    "rev-parse",
    "--path-format=absolute",
    "--git-dir",
  ]);
  const commonDirResult = await runner.runChecked(cwd, [
    "rev-parse",
    "--path-format=absolute",
    "--git-common-dir",
  ]);

  if (isBare) {
    const gitDir = oneLine(gitDirResult.stdout);
    return { root: cwd, gitDir, commonDir: oneLine(commonDirResult.stdout), isBare };
  }

  const rootResult = await runner.runChecked(cwd, [
    "rev-parse",
    "--path-format=absolute",
    "--show-toplevel",
  ]);
  return {
    root: oneLine(rootResult.stdout),
    gitDir: oneLine(gitDirResult.stdout),
    commonDir: oneLine(commonDirResult.stdout),
    isBare,
  };
};

export const requireWorktreeRepository = async (
  repoPath: string,
  runner: GitRunner = git,
) => {
  const repository = await discoverRepository(repoPath, runner);
  if (repository.isBare) {
    throw new Error("Bare repositories are unsupported");
  }
  return repository;
};

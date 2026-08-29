import { mkdtemp, realpath, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

export type TestRepository = {
  path: string;
  run: (...args: string[]) => Promise<string>;
  cleanup: () => Promise<void>;
};

const gitEnvironment = {
  ...process.env,
  GIT_AUTHOR_NAME: "Usable Git Test",
  GIT_AUTHOR_EMAIL: "usable-git@example.test",
  GIT_COMMITTER_NAME: "Usable Git Test",
  GIT_COMMITTER_EMAIL: "usable-git@example.test",
};

export const createRepository = async (): Promise<TestRepository> => {
  const path = await realpath(await mkdtemp(join(tmpdir(), "usable-git-read-")));
  const run = async (...args: string[]) => {
    const process = Bun.spawn(["git", ...args], {
      cwd: path,
      env: gitEnvironment,
      stdout: "pipe",
      stderr: "pipe",
    });
    const [stdout, stderr, exitCode] = await Promise.all([
      new Response(process.stdout).text(),
      new Response(process.stderr).text(),
      process.exited,
    ]);

    if (exitCode !== 0) {
      throw new Error(`git ${args.join(" ")} failed: ${stderr}`);
    }

    return stdout;
  };

  await run("init", "--quiet");

  return {
    path,
    run,
    cleanup: () => rm(path, { recursive: true, force: true }),
  };
};

export const writeFile = async (
  repository: TestRepository,
  relativePath: string,
  contents: string,
) => Bun.write(join(repository.path, relativePath), contents);

export const commitFile = async (
  repository: TestRepository,
  relativePath: string,
  contents: string,
  message: string,
) => {
  await writeFile(repository, relativePath, contents);
  await repository.run("add", "--", relativePath);
  await repository.run("commit", "--quiet", "-m", message);
};

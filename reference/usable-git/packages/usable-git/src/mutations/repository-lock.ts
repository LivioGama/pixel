import { mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { homedir } from "node:os";
import { dirname, join } from "node:path";
import { createHash, randomUUID } from "node:crypto";

interface RepositoryLockOptions {
  stateRoot?: string;
}

interface LockOwner {
  pid: number;
  token: string;
  acquiredAt: string;
  commonDirectory: string;
}

export class RepositoryBusyError extends Error {
  readonly code = "REPO_BUSY";

  constructor(commonDirectory: string) {
    super(`Repository is already locked for mutation: ${commonDirectory}`);
    this.name = "RepositoryBusyError";
  }
}

const getStateRoot = () =>
  process.env.XDG_STATE_HOME
    ? join(process.env.XDG_STATE_HOME, "usable-git")
    : join(homedir(), ".local", "state", "usable-git");

const getLockKey = (commonDirectory: string) =>
  createHash("sha256").update(commonDirectory).digest("hex");

const isProcessAlive = (pid: number) => {
  try {
    process.kill(pid, 0);
    return true;
  } catch (error) {
    return (error as NodeJS.ErrnoException).code !== "ESRCH";
  }
};

const removeStaleLock = async (lockDirectory: string) => {
  try {
    const owner = JSON.parse(
      await readFile(join(lockDirectory, "owner.json"), "utf8"),
    ) as LockOwner;

    if (isProcessAlive(owner.pid)) return false;
    await rm(lockDirectory, { recursive: true, force: true });
    return true;
  } catch {
    return false;
  }
};

export const acquireRepositoryLock = async (
  commonDirectory: string,
  options: RepositoryLockOptions = {},
) => {
  const stateRoot = options.stateRoot ?? getStateRoot();
  const lockDirectory = join(
    stateRoot,
    "locks",
    `${getLockKey(commonDirectory)}.lock`,
  );
  const ownerPath = join(lockDirectory, "owner.json");
  const owner: LockOwner = {
    pid: process.pid,
    token: randomUUID(),
    acquiredAt: new Date().toISOString(),
    commonDirectory,
  };

  await mkdir(dirname(lockDirectory), { recursive: true });

  const acquire = async (allowStaleRecovery: boolean): Promise<void> => {
    try {
      await mkdir(lockDirectory);
      await writeFile(ownerPath, `${JSON.stringify(owner)}\n`, {
        encoding: "utf8",
        flag: "wx",
        mode: 0o600,
      });
    } catch (error) {
      const code = (error as NodeJS.ErrnoException).code;
      if (code !== "EEXIST") throw error;
      if (allowStaleRecovery && (await removeStaleLock(lockDirectory))) {
        await acquire(false);
        return;
      }
      throw new RepositoryBusyError(commonDirectory);
    }
  };

  await acquire(true);
  let released = false;

  return {
    release: async () => {
      if (released) return;
      released = true;

      try {
        const currentOwner = JSON.parse(
          await readFile(ownerPath, "utf8"),
        ) as LockOwner;
        if (currentOwner.token !== owner.token) return;
      } catch {
        return;
      }

      await rm(lockDirectory, { recursive: true, force: true });
    },
  };
};

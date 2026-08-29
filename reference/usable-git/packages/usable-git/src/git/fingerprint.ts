import { createHash } from "node:crypto";
import { lstat, readlink } from "node:fs/promises";
import { resolve } from "node:path";
import type { StatusChange } from "./status.ts";

const hashFile = async (hash: ReturnType<typeof createHash>, path: string) => {
  const reader = Bun.file(path).stream().getReader();
  while (true) {
    const chunk = await reader.read();
    if (chunk.done) break;
    hash.update(chunk.value);
  }
};

export const fingerprintChange = async (root: string, change: StatusChange) => {
  const hash = createHash("sha256");
  hash.update(
    JSON.stringify({
      path: change.path,
      originalPath: change.originalPath ?? null,
      indexStatus: change.indexStatus,
      worktreeStatus: change.worktreeStatus,
      indexOid: change.indexOid ?? null,
      kind: change.kind,
      conflicted: change.conflicted,
    }),
  );
  const path = resolve(root, change.path);
  try {
    const stats = await lstat(path);
    if (stats.isSymbolicLink()) {
      hash.update("\0symlink\0");
      hash.update(await readlink(path));
    } else if (stats.isFile()) {
      hash.update("\0file\0");
      await hashFile(hash, path);
    } else {
      hash.update(`\0mode:${stats.mode}\0`);
    }
  } catch (error) {
    if (error instanceof Error && "code" in error && error.code === "ENOENT") {
      hash.update("\0missing\0");
    } else {
      throw error;
    }
  }
  return hash.digest("hex");
};

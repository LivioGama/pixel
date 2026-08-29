import { createHash, randomUUID } from "node:crypto";
import { open, readFile, rename, rm, stat } from "node:fs/promises";
import { dirname } from "node:path";

export type IndexSnapshot =
  | { exists: false; checksum: null }
  | { exists: true; checksum: string; bytes: Uint8Array; mode: number };

const checksum = (bytes: Uint8Array) =>
  createHash("sha256").update(bytes).digest("hex");

export const indexChecksum = async (indexPath: string): Promise<string | null> => {
  try {
    return checksum(await readFile(indexPath));
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") return null;
    throw error;
  }
};

export const captureIndexSnapshot = async (
  indexPath: string,
): Promise<IndexSnapshot> => {
  try {
    const [bytes, metadata] = await Promise.all([readFile(indexPath), stat(indexPath)]);
    return {
      exists: true,
      checksum: checksum(bytes),
      bytes: new Uint8Array(bytes),
      mode: metadata.mode & 0o777,
    };
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") {
      return { exists: false, checksum: null };
    }
    throw error;
  }
};

const syncDirectory = async (path: string) => {
  const directory = await open(dirname(path), "r");
  try {
    await directory.sync();
  } finally {
    await directory.close();
  }
};

export const restoreIndexIfOwned = async (
  indexPath: string,
  snapshot: IndexSnapshot,
  ownedChecksum: string | null,
) => {
  if ((await indexChecksum(indexPath)) !== ownedChecksum) return false;

  if (!snapshot.exists) {
    await rm(indexPath, { force: true });
    await syncDirectory(indexPath);
    return true;
  }

  const temporaryPath = `${indexPath}.usable-git-${process.pid}-${randomUUID()}`;
  const file = await open(temporaryPath, "wx", snapshot.mode);
  try {
    await file.writeFile(snapshot.bytes);
    await file.sync();
  } finally {
    await file.close();
  }

  try {
    await rename(temporaryPath, indexPath);
    await syncDirectory(indexPath);
  } finally {
    await rm(temporaryPath, { force: true });
  }

  return true;
};

import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";

let candidate: string | undefined;

/** Build once per test process, or verify an explicitly supplied session candidate. */
export const preparePixelCandidate = (): string => {
  if (candidate) return candidate;
  const root = resolve(import.meta.dir, "../../..");
  const supplied = process.env.PIXEL_TEST_BINARY;
  const binary = supplied ? resolve(supplied) : resolve(root, "target/debug/pixel");
  if (!supplied) {
    execFileSync("cargo", ["build", "-p", "pixel-cli"], {
      cwd: root, stdio: "inherit", timeout: 600_000,
    });
  }
  const hash = createHash("sha256").update(readFileSync(binary)).digest("hex");
  if (supplied && hash !== process.env.PIXEL_TEST_BINARY_SHA256) {
    throw new Error("PIXEL_TEST_BINARY requires its matching session-built PIXEL_TEST_BINARY_SHA256");
  }
  console.info(`[sniper test candidate] ${binary} sha256=${hash}`);
  candidate = binary;
  return binary;
};

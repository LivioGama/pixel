import { describe, expect, test } from "bun:test";
import { createHash } from "node:crypto";
import { mkdtemp, mkdir, readFile, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  HomebrewReleaseError,
  prepareHomebrewRelease,
  type HomebrewCommandRunner,
} from "../../../packaging/homebrew/prepare-release.ts";

const artifact = new TextEncoder().encode("immutable release artifact\n");
const sha256 = createHash("sha256").update(artifact).digest("hex");
const release = {
  version: "0.1.0",
  url: "https://github.com/LivioGama/usable-git/releases/download/v0.1.0/usable-git-0.1.0.tar.gz",
  sha256,
};

const fixture = async () => {
  const tapRoot = await mkdtemp(join(tmpdir(), "usable-git-tap-"));
  const formulaPath = join(tapRoot, "Formula", "usable-git.rb");
  await mkdir(join(tapRoot, "Formula"), { recursive: true });
  return { tapRoot, formulaPath };
};

const fetchArtifact = async () => new Response(artifact, { status: 200 });

describe("Homebrew release preparation", () => {
  test("verifies the artifact then runs every release gate without publishing", async () => {
    const { tapRoot, formulaPath } = await fixture();
    const calls: Array<{ command: string; args: string[] }> = [];
    const runner: HomebrewCommandRunner = async (command, args) => {
      calls.push({ command, args });
      return { exitCode: 0, stdout: "ok", stderr: "" };
    };

    const result = await prepareHomebrewRelease({
      ...release,
      tapRoot,
      formulaRef: "liviogama/tap/usable-git",
      fetchArtifact,
      runner,
    });

    expect(result).toEqual({
      formulaPath,
      artifactSha256: sha256,
      verifiedCommands: 3,
    });
    expect(await readFile(formulaPath, "utf8")).toContain(`sha256 "${sha256}"`);
    expect(calls).toEqual([
      {
        command: "brew",
        args: ["audit", "--strict", "--formula", "liviogama/tap/usable-git"],
      },
      {
        command: "brew",
        args: ["install", "--build-from-source", "liviogama/tap/usable-git"],
      },
      {
        command: "brew",
        args: ["test", "liviogama/tap/usable-git"],
      },
    ]);
    expect(calls.flatMap(({ args }) => args)).not.toContain("push");
    expect(calls.flatMap(({ args }) => args)).not.toContain("tag");
  });

  test("preserves the prior formula when artifact checksum verification fails", async () => {
    const { tapRoot, formulaPath } = await fixture();
    await writeFile(formulaPath, "prior formula\n");
    let calls = 0;

    await expect(prepareHomebrewRelease({
      ...release,
      sha256: "1".repeat(64),
      tapRoot,
      formulaRef: "liviogama/tap/usable-git",
      fetchArtifact,
      runner: async () => {
        calls += 1;
        return { exitCode: 0, stdout: "", stderr: "" };
      },
    })).rejects.toThrow("artifact SHA-256 mismatch");

    expect(calls).toBe(0);
    expect(await readFile(formulaPath, "utf8")).toBe("prior formula\n");
  });

  test("rolls back the formula byte-for-byte when a Homebrew gate fails", async () => {
    const { tapRoot, formulaPath } = await fixture();
    const prior = "class Prior < Formula\nend\n";
    await writeFile(formulaPath, prior);
    let call = 0;

    await expect(prepareHomebrewRelease({
      ...release,
      tapRoot,
      formulaRef: "liviogama/tap/usable-git",
      fetchArtifact,
      runner: async () => {
        call += 1;
        return call === 2
          ? { exitCode: 1, stdout: "", stderr: "install exploded" }
          : { exitCode: 0, stdout: "ok", stderr: "" };
      },
    })).rejects.toBeInstanceOf(HomebrewReleaseError);

    expect(call).toBe(2);
    expect(await readFile(formulaPath, "utf8")).toBe(prior);
  });
});

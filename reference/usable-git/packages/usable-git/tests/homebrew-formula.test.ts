import { describe, expect, test } from "bun:test";
import { mkdtemp, readFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  formulaToolNames,
  generateHomebrewFormula,
  writeHomebrewFormula,
} from "../../../packaging/homebrew/generate-formula.ts";
import { operationSchema } from "../src/contracts/v1.ts";

const release = {
  version: "0.1.0",
  url: "https://github.com/LivioGama/usable-git/releases/download/v0.1.0/usable-git-homebrew-0.1.0.tar.gz",
  sha256: "a".repeat(64),
};

describe("Homebrew formula generation", () => {
  test("asserts the exact registered MCP tool surface, derived from the contract", () => {
    // Regression guard: the formula previously hardcoded a five-tool list and
    // would have failed `brew test` against the ten-tool server.
    expect(formulaToolNames).toEqual([...operationSchema.options].sort());
    expect(generateHomebrewFormula(release)).toContain(
      `assert_equal %w[${[...operationSchema.options].sort().join(" ")}], names`,
    );
  });

  test("renders a deterministic complete stable formula", () => {
    const first = generateHomebrewFormula(release);
    const second = generateHomebrewFormula(release);

    expect(first).toBe(second);
    expect(first).toContain("class UsableGit < Formula");
    expect(first).toContain(`url "${release.url}"`);
    expect(first).not.toContain(`version "${release.version}"`);
    expect(first).toContain(`sha256 "${release.sha256}"`);
    expect(first).toContain('license "MIT"');
    expect(first).toContain('depends_on "bun"');
    expect(first).toContain('depends_on "git"');
    expect(first).toContain('system formula_opt_bin("bun")/"bun", "install", "--production", "--frozen-lockfile"');
    expect(first).toContain('libexec.install Dir["*"]');
    expect(first).toContain('export USABLE_GIT_EXECUTABLE_PATH="$0"');
    expect(first).toContain('(bin/"usable-git").chmod 0755');
    expect(first).not.toContain("{{");
    expect(first.endsWith("\n")).toBeTrue();
  });

  test("ships a substantive formula test for every required path", () => {
    const formula = generateHomebrewFormula(release);

    expect(formula).toMatch(/"method"\s+=> "initialize"/);
    expect(formula).toMatch(/"method"\s+=> "tools\/list"/);
    expect(formula).toContain(
      `assert_equal %w[${[...operationSchema.options].sort().join(" ")}]`,
    );
    expect(formula).toContain('"inspect", "--json"');
    // The formula must exercise the current snapshot-token flow. The compact
    // inspect result no longer carries per-file fingerprints on the wire, so a
    // fingerprint fetch here would crash `brew test` at release time.
    expect(formula).toContain('fetch("result").fetch("snapshot")');
    expect(formula).toContain('"--snapshot", snapshot');
    expect(formula).not.toContain('fetch("fingerprint")');
    expect(formula).not.toContain("--expected-fingerprint");
    expect(formula).not.toContain("Digest::SHA256.file");
    expect(formula).not.toContain("shell_output(");
    expect(formula).toContain('"publish", "--json"');
    expect(formula).toContain('File.write repo/"selected.txt", "after\\n"');
    expect(formula).toContain('assert_match " M unrelated.txt"');
    expect(formula).toContain('"push", "--json"');
    expect(formula).toContain('assert_equal pushed.fetch("result").fetch("newTargetOid")');
    expect(formula).toContain('system git, "-C", repo, "fsck", "--strict"');
    expect(formula).toContain('system git, "--git-dir", remote, "fsck", "--strict"');
  });

  test("rejects release metadata that cannot identify a real immutable artifact", () => {
    expect(() => generateHomebrewFormula({ ...release, version: "v0.1.0" })).toThrow(
      "version must be semantic",
    );
    expect(() => generateHomebrewFormula({ ...release, url: "http://example.test/archive.tgz" })).toThrow(
      "url must use HTTPS",
    );
    expect(() => generateHomebrewFormula({ ...release, url: "https://example.test/latest.tgz" })).toThrow(
      "url must contain the version",
    );
    expect(() => generateHomebrewFormula({ ...release, sha256: "placeholder" })).toThrow(
      "sha256 must be 64 lowercase hexadecimal characters",
    );
    expect(() => generateHomebrewFormula({ ...release, sha256: "0".repeat(64) })).toThrow(
      "sha256 cannot be an all-zero placeholder",
    );
  });

  test("atomically writes Formula/usable-git.rb and passes Ruby syntax validation", async () => {
    const root = await mkdtemp(join(tmpdir(), "usable-git-formula-"));
    const path = join(root, "Formula", "usable-git.rb");

    await writeHomebrewFormula({ ...release, outputPath: path });
    expect(await readFile(path, "utf8")).toBe(generateHomebrewFormula(release));

    const ruby = Bun.which("ruby");
    if (!ruby) return;
    const syntax = Bun.spawnSync([ruby, "-c", path], { stdout: "pipe", stderr: "pipe" });
    expect(syntax.exitCode).toBe(0);
    expect(syntax.stdout.toString()).toContain("Syntax OK");
  });
});

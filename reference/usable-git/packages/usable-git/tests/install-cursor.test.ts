import { afterEach, describe, expect, test } from "bun:test";
import { mkdtemp, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import {
  InstallConfigError,
  InstallConflictError,
  installCursor,
} from "@usable-git/install/cursor.ts";

const homes: string[] = [];

const createHome = async () => {
  const home = await mkdtemp(join(tmpdir(), "usable-git-cursor-install-"));
  homes.push(home);
  return home;
};

const configPathFor = (home: string) => join(home, ".cursor", "mcp.json");

const writeConfig = async (home: string, value: unknown) => {
  const configPath = configPathFor(home);
  await mkdir(join(home, ".cursor"), { recursive: true });
  await writeFile(configPath, `${JSON.stringify(value, null, 2)}\n`);
  return configPath;
};

afterEach(async () => {
  await Promise.all(homes.splice(0).map((home) => rm(home, { recursive: true, force: true })));
});

describe("installCursor", () => {
  test("atomically adds the stdio entry while preserving unrelated configuration", async () => {
    const home = await createHome();
    const configPath = await writeConfig(home, {
      theme: "sentinel",
      mcpServers: {
        existing: { command: "/usr/bin/existing", args: ["serve"] },
      },
    });

    const result = await installCursor({
      executablePath: "/opt/homebrew/bin/usable-git",
      force: false,
      home,
    });

    expect(result).toEqual({ client: "cursor", configPath, status: "installed" });
    expect(JSON.parse(await readFile(configPath, "utf8"))).toEqual({
      theme: "sentinel",
      mcpServers: {
        existing: { command: "/usr/bin/existing", args: ["serve"] },
        "usable-git": {
          command: "/opt/homebrew/bin/usable-git",
          args: ["mcp"],
        },
      },
    });
  });

  test("leaves a matching entry byte-for-byte unchanged", async () => {
    const home = await createHome();
    const configPath = await writeConfig(home, {
      sentinel: { keep: true },
      mcpServers: {
        "usable-git": {
          command: "/opt/homebrew/bin/usable-git",
          args: ["mcp"],
        },
      },
    });
    const before = await readFile(configPath, "utf8");

    const result = await installCursor({
      executablePath: "/opt/homebrew/bin/usable-git",
      home,
    });

    expect(result.status).toBe("unchanged");
    expect(await readFile(configPath, "utf8")).toBe(before);
  });

  test("refuses a conflicting entry without touching the file", async () => {
    const home = await createHome();
    const configPath = await writeConfig(home, {
      sentinel: "preserve",
      mcpServers: {
        "usable-git": { command: "/tmp/wrong", args: ["mcp"] },
      },
    });
    const before = await readFile(configPath, "utf8");

    await expect(
      installCursor({ executablePath: "/opt/homebrew/bin/usable-git", home }),
    ).rejects.toBeInstanceOf(InstallConflictError);
    expect(await readFile(configPath, "utf8")).toBe(before);
  });

  test("force replaces only the conflicting usable-git entry", async () => {
    const home = await createHome();
    const configPath = await writeConfig(home, {
      sentinel: ["keep"],
      mcpServers: {
        existing: { url: "https://example.invalid/mcp" },
        "usable-git": { command: "/tmp/wrong", args: ["old"] },
      },
    });

    const result = await installCursor({
      executablePath: "/opt/homebrew/bin/usable-git",
      force: true,
      home,
    });
    const config = JSON.parse(await readFile(configPath, "utf8"));

    expect(result.status).toBe("replaced");
    expect(config.sentinel).toEqual(["keep"]);
    expect(config.mcpServers.existing).toEqual({ url: "https://example.invalid/mcp" });
    expect(config.mcpServers["usable-git"]).toEqual({
      command: "/opt/homebrew/bin/usable-git",
      args: ["mcp"],
    });
  });

  test("preserves malformed configuration and rejects relative executables", async () => {
    const malformedHome = await createHome();
    const malformedPath = configPathFor(malformedHome);
    await mkdir(join(malformedHome, ".cursor"), { recursive: true });
    await writeFile(malformedPath, "{ definitely not json\n");
    const malformedBefore = await readFile(malformedPath, "utf8");

    await expect(
      installCursor({ executablePath: "/opt/homebrew/bin/usable-git", home: malformedHome }),
    ).rejects.toBeInstanceOf(InstallConfigError);
    expect(await readFile(malformedPath, "utf8")).toBe(malformedBefore);

    const relativeHome = await createHome();
    await expect(
      installCursor({ executablePath: "./usable-git", home: relativeHome }),
    ).rejects.toThrow("absolute");
    expect(await Bun.file(configPathFor(relativeHome)).exists()).toBe(false);
  });
});

import { describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  ProvenanceTracker,
  clearDiskMapCache,
  enrichFrames,
  parseEvaluatingChain,
  parseStack,
} from "../src/enrich.ts";

describe("parseStack — V8", () => {
  const v8Stack = [
    "TypeError: Cannot read properties of undefined (reading 'x')",
    "    at useRouter (http://localhost:5173/src/hooks/router.ts:12:9)",
    "    at async loadRoute (http://localhost:5173/src/routes/chat.tsx:88:14)",
    "    at new RouterProvider (http://localhost:5173/node_modules/.vite/deps/chunk-ABC123.js:1042:22)",
    "    at http://localhost:5173/src/main.tsx:5:1",
    "    at processTicksAndRejections (node:internal/process/task_queues:95:5)",
  ].join("\n");

  test("parses named, async, constructor, and bare frames", () => {
    const frames = parseStack(v8Stack);
    expect(frames.length).toBe(5);
    expect(frames[0]).toMatchObject({
      func: "useRouter",
      file: "http://localhost:5173/src/hooks/router.ts",
      line: 12,
      column: 9,
    });
    expect(frames[1].func).toBe("loadRoute");
    expect(frames[2].func).toBe("RouterProvider");
    expect(frames[3].func).toBeUndefined();
    expect(frames[3].file).toBe("http://localhost:5173/src/main.tsx");
    expect(frames[4].file).toBe("node:internal/process/task_queues");
  });
});

describe("parseStack — JSC", () => {
  const jscStack = [
    "useRouter@http://localhost:5173/src/hooks/router.ts:12:9",
    "@http://localhost:5173/src/routes/chat.tsx:88:14",
    "global code@http://localhost:5173/src/main.tsx:5:1",
  ].join("\n");

  test("parses named, anonymous, and global-code frames", () => {
    const frames = parseStack(jscStack);
    expect(frames.length).toBe(3);
    expect(frames[0]).toMatchObject({ func: "useRouter", line: 12, column: 9 });
    expect(frames[1].func).toBeUndefined();
    expect(frames[1].file).toBe("http://localhost:5173/src/routes/chat.tsx");
    expect(frames[2].func).toBe("global code");
  });

  test("empty and headerless input", () => {
    expect(parseStack("")).toEqual([]);
    expect(parseStack(undefined)).toEqual([]);
    expect(parseStack("just a message\nno frames here")).toEqual([]);
  });
});

describe("parseEvaluatingChain", () => {
  test("JSC evaluating chain", () => {
    expect(
      parseEvaluatingChain("undefined is not an object (evaluating 'api.sessions.list')"),
    ).toEqual(["api", "sessions", "list"]);
  });
  test("V8 reading property", () => {
    expect(
      parseEvaluatingChain("Cannot read properties of undefined (reading 'list')"),
    ).toEqual(["list"]);
  });
  test("no match", () => {
    expect(parseEvaluatingChain("plain failure")).toBeUndefined();
  });
});

const makeFixtureTree = () => {
  const root = mkdtempSync(join(tmpdir(), "sniper-prov-"));
  // copy 1: top-level node_modules
  const copy1 = join(root, "node_modules", "@tanstack", "react-router");
  mkdirSync(join(copy1, "dist"), { recursive: true });
  writeFileSync(
    join(copy1, "package.json"),
    JSON.stringify({ name: "@tanstack/react-router", version: "1.130.2" }),
  );
  writeFileSync(join(copy1, "dist", "index.js"), "// copy 1");
  // copy 2: nested under another dependency
  const copy2 = join(root, "node_modules", "some-lib", "node_modules", "@tanstack", "react-router");
  mkdirSync(join(copy2, "dist"), { recursive: true });
  writeFileSync(
    join(copy2, "package.json"),
    JSON.stringify({ name: "@tanstack/react-router", version: "1.128.0" }),
  );
  writeFileSync(join(copy2, "dist", "index.js"), "// copy 2");
  // unrelated singleton package
  const single = join(root, "node_modules", "solo-pkg");
  mkdirSync(single, { recursive: true });
  writeFileSync(join(single, "package.json"), JSON.stringify({ name: "solo-pkg", version: "2.0.0" }));
  writeFileSync(join(single, "index.js"), "// solo");
  return { root, copy1, copy2, single };
};

describe("ProvenanceTracker", () => {
  test("nearest package.json wins, with name/version/realPath", () => {
    const { copy1 } = makeFixtureTree();
    const tracker = new ProvenanceTracker();
    const pkg = tracker.framePackage(join(copy1, "dist", "index.js"));
    expect(pkg?.name).toBe("@tanstack/react-router");
    expect(pkg?.version).toBe("1.130.2");
    expect(pkg?.path).toContain("react-router");
    expect(pkg?.dup_paths).toBeUndefined();
  });

  test("duplicate physical copies attach dup_paths", () => {
    const { copy1, copy2 } = makeFixtureTree();
    const tracker = new ProvenanceTracker();
    tracker.framePackage(join(copy1, "dist", "index.js"));
    const pkg2 = tracker.framePackage(join(copy2, "dist", "index.js"));
    expect(pkg2?.dup_paths?.length).toBe(2);
    expect(tracker.duplicates()["@tanstack/react-router"]?.length).toBe(2);
  });

  test("singleton package has no dup_paths and no duplicates entry", () => {
    const { single } = makeFixtureTree();
    const tracker = new ProvenanceTracker();
    const pkg = tracker.framePackage(join(single, "index.js"));
    expect(pkg?.name).toBe("solo-pkg");
    expect(pkg?.dup_paths).toBeUndefined();
    expect(Object.keys(tracker.duplicates())).toEqual([]);
  });

  test("path outside any package returns undefined", () => {
    const tracker = new ProvenanceTracker();
    expect(tracker.framePackage("/no/such/deep/path/file.js")).toBeUndefined();
  });
});

describe("enrichFrames — disk .map for pre-bundled deps", () => {
  test("maps /node_modules/.vite/deps frames through the sibling .map file", async () => {
    clearDiskMapCache();
    const { root, copy1 } = makeFixtureTree();
    const depsDir = join(root, "node_modules", ".vite", "deps");
    mkdirSync(depsDir, { recursive: true });
    const originalFile = join(copy1, "dist", "index.js");
    writeFileSync(join(depsDir, "chunk-TEST.js"), "export const x = 1;\n");
    // Single-segment map: generated line 1 col 0 → original line 1 col 0 of dist/index.js.
    writeFileSync(
      join(depsDir, "chunk-TEST.js.map"),
      JSON.stringify({ version: 3, sources: [originalFile], mappings: "AAAA", names: [] }),
    );
    const tracker = new ProvenanceTracker();
    const frames = parseStack(
      `    at boom (http://localhost:5173/node_modules/.vite/deps/chunk-TEST.js:1:0)`,
    );
    const server = { config: { root } };
    const [frame] = await enrichFrames(frames, server, tracker);
    expect(frame.mapped_file).toBe(originalFile);
    expect(frame.mapped_line).toBe(1);
    expect(frame.pkg?.name).toBe("@tanstack/react-router");
  });
});

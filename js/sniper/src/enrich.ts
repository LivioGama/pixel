/**
 * Server-side enrichment: parse V8/JSC stack strings into frames, source-map
 * dev-server URLs back to physical files (vite moduleGraph transform maps for
 * /src modules, sibling .map files on disk for pre-bundled deps), and attach
 * package provenance (nearest package.json + duplicate-copy detection).
 */

import { TraceMap, originalPositionFor } from "@jridgewell/trace-mapping";
import { existsSync, readFileSync, realpathSync } from "node:fs";
import { dirname, isAbsolute, join, resolve } from "node:path";
import type { Frame, FramePackage } from "./report.ts";

// ---------------------------------------------------------------------------
// Stack parsing (V8 + JSC)
// ---------------------------------------------------------------------------

// V8:  "    at func (file:line:col)"  |  "    at file:line:col"
//      "    at async func (file:line:col)" | "    at new Foo (file:line:col)"
const V8_WITH_FUNC = /^\s*at\s+(?:async\s+)?(?:new\s+)?(.+?)\s+\((.+?):(\d+):(\d+)\)\s*$/;
const V8_BARE = /^\s*at\s+(?:async\s+)?(.+?):(\d+):(\d+)\s*$/;
// JSC: "func@file:line:col" | "@file:line:col" | "global code@file:line:col"
const JSC = /^(.*?)@(.+?):(\d+):(\d+)\s*$/;

const parseFrameLine = (line: string): Frame | undefined => {
  let m = V8_WITH_FUNC.exec(line);
  if (m) {
    return {
      raw: line.trim(),
      func: m[1],
      file: m[2],
      line: Number(m[3]),
      column: Number(m[4]),
    };
  }
  m = V8_BARE.exec(line);
  if (m) {
    return { raw: line.trim(), file: m[1], line: Number(m[2]), column: Number(m[3]) };
  }
  m = JSC.exec(line);
  if (m) {
    const func = m[1].trim();
    return {
      raw: line.trim(),
      func: func.length > 0 ? func : undefined,
      file: m[2],
      line: Number(m[3]),
      column: Number(m[4]),
    };
  }
  return undefined;
};

/**
 * Parse a raw stack string (V8 or JSC format) into structured frames.
 * Non-frame lines (the message header, "native code", etc.) are skipped.
 */
export const parseStack = (stack: string | undefined | null): Frame[] => {
  if (!stack) return [];
  const frames: Frame[] = [];
  for (const line of stack.split("\n")) {
    if (!line.trim()) continue;
    const frame = parseFrameLine(line);
    if (frame) frames.push(frame);
  }
  return frames;
};

// ---------------------------------------------------------------------------
// Message-class value parsing
// ---------------------------------------------------------------------------

/**
 * JSC-style "undefined is not an object (evaluating 'a.b.c')" → ["a","b","c"].
 * Also matches V8's "Cannot read properties of undefined (reading 'c')".
 */
export const parseEvaluatingChain = (message: string): string[] | undefined => {
  const evaluating = /\(evaluating '([^']+)'\)/.exec(message);
  if (evaluating) {
    const chain = evaluating[1].split(".").map((part) => part.trim()).filter(Boolean);
    if (chain.length > 0) return chain;
  }
  const reading = /\(reading '([^']+)'\)/.exec(message);
  if (reading) return [reading[1]];
  return undefined;
};

// ---------------------------------------------------------------------------
// Package provenance
// ---------------------------------------------------------------------------

interface PackageInfo {
  name: string;
  version?: string;
  realPath: string;
}

/**
 * Tracks name → Set(realPath) across one dev-server run. More than one path
 * for a name is the duplicate-module smoking gun and is attached to every
 * frame of that package as `dup_paths`.
 */
export class ProvenanceTracker {
  private readonly dirCache = new Map<string, PackageInfo | null>();
  private readonly seen = new Map<string, Set<string>>();

  /** Walk up from a physical file path to the nearest package.json. */
  lookup(physicalPath: string): PackageInfo | undefined {
    if (!isAbsolute(physicalPath)) return undefined;
    let dir: string;
    try {
      dir = dirname(physicalPath);
    } catch {
      return undefined;
    }
    const info = this.lookupDir(dir);
    if (!info) return undefined;
    let paths = this.seen.get(info.name);
    if (!paths) {
      paths = new Set();
      this.seen.set(info.name, paths);
    }
    paths.add(info.realPath);
    return info;
  }

  /** FramePackage for a physical path, with dup_paths when >1 copy was seen. */
  framePackage(physicalPath: string): FramePackage | undefined {
    const info = this.lookup(physicalPath);
    if (!info) return undefined;
    const pkg: FramePackage = { name: info.name, path: info.realPath };
    if (info.version) pkg.version = info.version;
    const paths = this.seen.get(info.name);
    if (paths && paths.size > 1) pkg.dup_paths = [...paths].sort();
    return pkg;
  }

  /** All names currently known to exist at more than one physical path. */
  duplicates(): Record<string, string[]> {
    const out: Record<string, string[]> = {};
    for (const [name, paths] of this.seen) {
      if (paths.size > 1) out[name] = [...paths].sort();
    }
    return out;
  }

  /** Feed a package dir directly (e.g. from a .vite/deps/_metadata.json scan). */
  observeDir(dir: string): void {
    const info = this.lookupDir(dir);
    if (!info) return;
    let paths = this.seen.get(info.name);
    if (!paths) {
      paths = new Set();
      this.seen.set(info.name, paths);
    }
    paths.add(info.realPath);
  }

  private lookupDir(startDir: string): PackageInfo | undefined {
    let dir = startDir;
    const visited: string[] = [];
    for (let depth = 0; depth < 40; depth++) {
      const cached = this.dirCache.get(dir);
      if (cached !== undefined) {
        for (const v of visited) this.dirCache.set(v, cached);
        return cached ?? undefined;
      }
      visited.push(dir);
      const manifest = join(dir, "package.json");
      if (existsSync(manifest)) {
        let info: PackageInfo | null = null;
        try {
          const parsed = JSON.parse(readFileSync(manifest, "utf8")) as {
            name?: string;
            version?: string;
          };
          if (parsed.name) {
            let realPath = dir;
            try {
              realPath = realpathSync(dir);
            } catch {
              /* keep symlinked path */
            }
            info = { name: parsed.name, version: parsed.version, realPath };
          }
        } catch {
          info = null;
        }
        for (const v of visited) this.dirCache.set(v, info);
        return info ?? undefined;
      }
      const parent = dirname(dir);
      if (parent === dir) break;
      dir = parent;
    }
    for (const v of visited) this.dirCache.set(v, null);
    return undefined;
  }
}

// ---------------------------------------------------------------------------
// Source mapping
// ---------------------------------------------------------------------------

/** Minimal structural view of the parts of ViteDevServer we touch. */
export interface ModuleGraphLike {
  getModuleByUrl(url: string): Promise<
    | {
        file?: string | null;
        transformResult?: { map?: unknown } | null;
      }
    | undefined
    | null
  >;
}

export interface DevServerLike {
  config: { root: string };
  environments?: { client?: { moduleGraph?: ModuleGraphLike } };
  moduleGraph?: ModuleGraphLike;
}

const moduleGraphOf = (server: DevServerLike): ModuleGraphLike | undefined =>
  server.environments?.client?.moduleGraph ?? server.moduleGraph;

/** `http://localhost:5173/src/main.ts` → `/src/main.ts` (same-origin URLs only). */
const urlPathOf = (file: string): string | undefined => {
  if (file.startsWith("/") && !file.startsWith("//")) return file;
  try {
    const url = new URL(file);
    if (url.protocol === "http:" || url.protocol === "https:") {
      return url.pathname + url.search;
    }
  } catch {
    /* not a URL */
  }
  return undefined;
};

const diskMapCache = new Map<string, TraceMap | null>();

const loadDiskTraceMap = (mapPath: string): TraceMap | undefined => {
  const cached = diskMapCache.get(mapPath);
  if (cached !== undefined) return cached ?? undefined;
  let traceMap: TraceMap | null = null;
  try {
    if (existsSync(mapPath)) {
      traceMap = new TraceMap(JSON.parse(readFileSync(mapPath, "utf8")));
    }
  } catch {
    traceMap = null;
  }
  diskMapCache.set(mapPath, traceMap);
  return traceMap ?? undefined;
};

const applyMapping = (
  frame: Frame,
  traceMap: TraceMap,
  sourceRoot: string,
): void => {
  if (frame.line === undefined) return;
  const pos = originalPositionFor(traceMap, {
    line: frame.line,
    column: frame.column ?? 0,
  });
  if (!pos.source || pos.line == null) return;
  let mappedFile = pos.source;
  // Normalize vite's /@fs/ and relative sources to physical paths.
  if (mappedFile.startsWith("/@fs/")) mappedFile = mappedFile.slice("/@fs".length);
  if (!isAbsolute(mappedFile)) {
    try {
      mappedFile = resolve(sourceRoot, mappedFile.replace(/^\.\//, ""));
    } catch {
      /* keep as-is */
    }
  }
  frame.mapped_file = mappedFile;
  frame.mapped_line = pos.line;
  if (pos.column != null) frame.mapped_column = pos.column;
};

/**
 * Source-map every frame in place, then attach package provenance.
 *
 * - `http://localhost:<port>/src/*` (or bare `/src/*`) frames map through the
 *   vite module graph's transformResult.map;
 * - `/node_modules/.vite/deps/*.js` frames map through the sibling `.map`
 *   file on disk;
 * - already-physical paths skip mapping and go straight to provenance.
 *
 * Never throws — enrichment failures leave the raw frame untouched.
 */
export const enrichFrames = async (
  frames: Frame[],
  server: DevServerLike | undefined,
  provenance: ProvenanceTracker,
): Promise<Frame[]> => {
  const root = server?.config.root ?? process.cwd();
  const graph = server ? moduleGraphOf(server) : undefined;
  for (const frame of frames) {
    try {
      if (!frame.file) continue;
      const urlPath = urlPathOf(frame.file);
      if (urlPath) {
        const cleanPath = urlPath.split("?")[0];
        if (cleanPath.includes("/node_modules/.vite/deps/") && cleanPath.endsWith(".js")) {
          const diskFile = join(root, cleanPath.replace(/^\//, ""));
          const traceMap = loadDiskTraceMap(`${diskFile}.map`);
          if (traceMap) applyMapping(frame, traceMap, dirname(diskFile));
        } else if (graph) {
          const mod = await graph.getModuleByUrl(cleanPath).catch(() => undefined);
          const rawMap = mod?.transformResult?.map;
          if (rawMap) {
            try {
              const traceMap = new TraceMap(rawMap as never);
              // Vite transform maps carry sources relative to the module's
              // own directory (e.g. ["main.ts"] for /src/main.ts).
              applyMapping(frame, traceMap, mod?.file ? dirname(mod.file) : root);
              if (frame.mapped_file === undefined && mod?.file) {
                frame.mapped_file = mod.file;
              }
            } catch {
              /* bad map — leave raw */
            }
          }
          if (frame.mapped_file === undefined && mod?.file) {
            // No transform map (e.g. plain JS served as-is) — still name the file.
            frame.mapped_file = mod.file;
            frame.mapped_line = frame.line;
            frame.mapped_column = frame.column;
          }
        }
      }
      const physical = frame.mapped_file ?? (isAbsolute(frame.file) ? frame.file : undefined);
      if (physical) {
        const pkg = provenance.framePackage(physical);
        if (pkg) frame.pkg = pkg;
      }
    } catch {
      /* never break capture */
    }
  }
  return frames;
};

/** Testing hook: drop the disk .map cache. */
export const clearDiskMapCache = (): void => {
  diskMapCache.clear();
};

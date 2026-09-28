// Pixel extension for Pi — managed by `pixel install --repo`.
// __MANAGED_BEGIN__
// __MANAGED_END__
import { spawnSync } from "node:child_process";
import { appendFileSync, realpathSync } from "node:fs";
import { dirname, isAbsolute, relative, resolve } from "node:path";
import { Type } from "@earendil-works/pi-ai";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

const PIXEL_BIN = __PIXEL_BIN__;
const MAX_OUTPUT = 16000;
const READ_LIMIT = 200;
const ACTIONS = ["scope_task", "list_areas", "search_content", "find_code", "impact", "pack_context", "what_changed", "review_changes", "fetch", "commit", "commit_and_push"] as const;
type Action = (typeof ACTIONS)[number];
const LEGACY_OP: Record<string, string> = {
  "scope-task": "targets", "list-areas": "clusters", "search-content": "search",
  "find-code": "resolve", "pack-context": "context", "what-changed": "changes",
  "review-changes": "review", "fetch": "sync", "commit": "publish",
  "commit-and-push": "ship", "repo-state": "inspect", "commit-history": "history",
  "who-wrote": "provenance",
};

let installed: { version: string; commands: Set<string> } | undefined;
function capabilities() {
  if (installed) return installed;
  const version = spawnSync(PIXEL_BIN, ["--version"], { encoding: "utf8", timeout: 5000 });
  const help = spawnSync(PIXEL_BIN, ["--help"], { encoding: "utf8", timeout: 5000 });
  if (version.status !== 0 || help.status !== 0) {
    throw new Error(`Pixel unavailable at ${PIXEL_BIN}: ${version.error?.message ?? help.error?.message ?? help.stderr?.trim() ?? "failed preflight"}`);
  }
  installed = { version: version.stdout.trim().split("\n")[0], commands: new Set(
    [...help.stdout.matchAll(/^  ([a-z][a-z-]+)\s{2,}/gm)].map((match) => match[1]),
  ) };
  return installed;
}

function run(root: string, args: string[]) {
  const operation = resolveOperation(args[0]);
  const result = spawnSync(PIXEL_BIN, [operation, ...args.slice(1), "--metrics", "off"], {
    cwd: root, encoding: "utf8", timeout: 15000, maxBuffer: 2_000_000,
  });
  if (result.error || result.status !== 0) {
    throw new Error(result.error?.message ?? result.stderr?.trim() ?? `Pixel exited ${result.status}`);
  }
  return result.stdout;
}

function resolveOperation(name: string) {
  const available = capabilities();
  const operation = available.commands.has(name) ? name : LEGACY_OP[name];
  if (!operation || !available.commands.has(operation)) {
    throw new Error(`Pixel ${available.version} lacks operation ${name}`);
  }
  return operation;
}

function parseEvidence(output: string) {
  try { return JSON.parse(output); }
  catch { return output.split("\n").filter(Boolean).slice(0, 40).map((line) => {
    try { return JSON.parse(line); } catch { return line; }
  }); }
}

function rememberPaths(value: unknown, paths: Set<string>, root: string) {
  if (!value || typeof value !== "object") return;
  if (Array.isArray(value)) { value.forEach((item) => rememberPaths(item, paths, root)); return; }
  const object = value as Record<string, unknown>;
  if (typeof object.path === "string" && inRepo(root, object.path)) paths.add(relativeTarget(root, object.path));
  Object.values(object).forEach((item) => rememberPaths(item, paths, root));
}

function health(root: string, action: Action) {
  const status = JSON.parse(run(root, ["status", "--json"]));
  if (!status.index?.base_files) {
    throw new Error("Pixel index unavailable; run `pixel build-index --history .`, then retry");
  }
  if (["list_areas", "find_code", "impact", "pack_context"].includes(action) && !status.graph?.present) {
    throw new Error("Pixel graph unavailable; run `pixel rebuild-graph .`, then retry");
  }
  return { version: capabilities().version, index: status.index, graph: status.graph, facts: status.facts };
}

function audit(root: string, kind: string, reason: string, extra: Record<string, unknown> = {}) {
  try {
    appendFileSync(resolve(root, ".pixel/pi-policy.jsonl"), JSON.stringify({
      time: new Date().toISOString(), kind, reason, ...extra,
    }) + "\n");
  } catch { /* Logging must never loosen or break the policy. */ }
}

function shellQuote(value: string) {
  return `'${value.replaceAll("'", "'\\''")}'`;
}

function inRepo(root: string, path: string) {
  const rel = relative(root, isAbsolute(path) ? path : resolve(root, path));
  return rel === "" || (rel !== ".." && !rel.startsWith("../") && !isAbsolute(rel));
}

function relativeTarget(root: string, path: string) {
  return relative(root, isAbsolute(path) ? path : resolve(root, path));
}

function latestUserText(ctx: any) {
  const entries = ctx.sessionManager.getBranch();
  const last = [...entries].reverse().find((entry: any) => entry.type === "message" && entry.message?.role === "user");
  const content = last?.message?.content;
  if (typeof content === "string") return content;
  return Array.isArray(content)
    ? content.filter((part: any) => part.type === "text").map((part: any) => part.text).join(" ")
    : "";
}

function authorized(action: Action, ctx: any) {
  if (action !== "commit" && action !== "commit_and_push") return true;
  const text = latestUserText(ctx).toLowerCase().trim();
  if (/\b(?:do not|don't|never|without|no)\s+(?:\w+\s+){0,3}(?:commit|push|ship)\b/.test(text)) return false;
  const instruction = /(?:^|[.!?;]\s*)(?:please\s+)?(commit(?:\s+(?:and|&)\s+push)?|ship it|push (?:the|this|my) commit)\b/;
  const politeRequest = /\b(?:please|can you|could you|would you|i authorize you to|i approve you to)\s+(commit(?:\s+(?:and|&)\s+push)?|ship it|push (?:the|this|my) commit)\b/;
  if (/\bcommit messages?\b/.test(text)) return false;
  const request = text.match(instruction)?.[1] ?? text.match(politeRequest)?.[1] ?? "";
  if (action === "commit") return /^(commit|ship it)/.test(request);
  return /^(commit\s+(?:and|&)\s+push|ship it|push)/.test(request);
}

function commandFor(action: Action, p: any): string[][] {
  const target = String(p.query ?? p.goal ?? "").trim();
  const path = p.path ? [String(p.path)] : [];
  if (["scope_task", "search_content", "find_code", "impact", "pack_context"].includes(action)
      && !(p.symbol ?? target)) {
    throw new Error(`${action} requires a goal, query, or symbol`);
  }
  switch (action) {
    case "scope_task": return [["scope-task", String(p.goal ?? target), ...path, "--json"]];
    case "list_areas": return [["list-areas", ...path, "--json"]];
    case "search_content": return [["search-content", target, ...path, "--json", "--limit", "40"]];
    case "find_code": return [["find-code", target, ...path, "--json", "--limit", "5"]];
    case "impact": return [["impact", String(p.symbol ?? target), ...path, "--json"]];
    case "pack_context": return [["pack-context", String(p.symbol ?? target), ...path, "--json", "--budget", "1200"]];
    case "what_changed": return [["what-changed", ...path, "--json"]];
    case "review_changes": return [["review-changes", ...path, "--json"]];
    case "fetch": return [["fetch", String(p.remote ?? "origin"), ...path, "--json"]];
    case "commit": case "commit_and_push": {
      if (!p.message || !Array.isArray(p.files) || p.files.length === 0 || !p.request_id) {
        throw new Error("Commit requires message, explicit files, and request_id");
      }
      const files = p.files.flatMap((file: string) => ["--files", file]);
      const base = ["--message", String(p.message), ...files, "--request-id", String(p.request_id), "--json"];
      return [action === "commit"
        ? ["commit", ...base, ...path]
        : ["commit-and-push", ...base, String(p.remote ?? "origin"), String(p.refspec ?? "HEAD"), ...path]];
    }
  }
}

function simpleTranslation(command: string, root: string, resolvedPaths: Set<string>) {
  const text = command.trim();
  if (/[|;&`$<>\\\n]/.test(text)) return null;
  const match = /^(ls|rg|grep|git|cat)\s*(.*)$/.exec(text);
  if (!match) return null;
  const [, verb, rest] = match;
  if (verb === "cat" && /^[\w./-]+$/.test(rest) && resolvedPaths.has(relativeTarget(root, rest))) {
    return ["search-content", "^", rest, "--json", "--limit", "200"];
  }
  if (verb === "ls" && /^[\w./-]*$/.test(rest)) return ["list-areas", "--json"];
  if ((verb === "rg" || verb === "grep") && /^['"]?[\w.*/:-]+['"]?(?:\s+[\w./-]+)?$/.test(rest)) {
    const [query, path] = rest.split(/\s+/);
    return ["search-content", query.replace(/^['"]|['"]$/g, ""), ...(path ? [path] : []), "--limit", "40"];
  }
  if (verb === "git") {
    if (rest === "status") return ["repo-state", "--json"];
    if (rest === "diff") return ["review-changes", "--json"];
    if (rest === "log") return ["commit-history", "--json"];
    if (rest === "fetch" || /^fetch [\w.-]+$/.test(rest)) return ["fetch", rest.split(" ")[1] ?? "origin", "--json"];
    if (/^blame [\w./-]+$/.test(rest)) return ["who-wrote", rest.slice(6), "--json"];
  }
  return null;
}

function safeCopy(command: string, root: string) {
  const words: string[] = [];
  const token = /\s*(?:'([^']*)'|"([^"]*)"|([^\s'"]+))/y;
  for (let offset = 0; offset < command.length;) {
    token.lastIndex = offset;
    const match = token.exec(command);
    if (!match) return false;
    words.push(match[1] ?? match[2] ?? match[3]);
    offset = token.lastIndex;
  }
  const paths: string[] = [];
  let literalPaths = false;
  for (const word of words.slice(1)) {
    if (/[~*?\[\]{}]/.test(word)) return false;
    if (!literalPaths && word === "--") { literalPaths = true; continue; }
    if (!literalPaths && word.startsWith("-")) {
      if (paths.length === 0 && /^-[RrHLPpfinvXc]+$/.test(word)) continue;
      return false;
    }
    paths.push(word);
  }
  if (paths.length < 2) return false;
  if (!paths.slice(0, -1).some((path) => inRepo(root, path))) return true;
  let destination = resolve(root, paths.at(-1)!);
  while (true) {
    try { return inRepo(realpathSync(root), realpathSync(destination)); }
    catch (error) {
      if ((error as NodeJS.ErrnoException).code !== "ENOENT" || dirname(destination) === destination) return false;
      destination = dirname(destination);
    }
  }
}

function classify(tool: string, input: any, root: string, resolvedPaths: Set<string>): { kind: string; reason: string; translation?: string[]; readLimit?: number } {
  if (tool === "pixel") return { kind: "tool", reason: "structured Pixel" };
  if (["edit", "write", "apply_patch", "todo", "web_search", "web_contents", "web_answer", "bg_wait"].includes(tool)) {
    return { kind: "exception", reason: "edit or non-repository tool" };
  }
  if (tool === "read" || tool === "view_file") {
    const path = String(input.path ?? input.file_path ?? "");
    if (path && !inRepo(root, path)) return { kind: "exception", reason: "outside repository" };
    const target = path ? relativeTarget(root, path) : "";
    if (path && resolvedPaths.has(target) && (!input.limit || Number(input.limit) > READ_LIMIT)) {
      return { kind: "translated", reason: "bounded Pixel-resolved target read", readLimit: READ_LIMIT };
    }
    if (path && resolvedPaths.has(target) && Number(input.limit) > 0 && Number(input.limit) <= READ_LIMIT) {
      return { kind: "exception", reason: "bounded Pixel-resolved target read" };
    }
    return { kind: "blocked", reason: "Use pixel with a task goal, or specify a target and a read limit of at most 200 lines" };
  }
  if (tool === "bash" || tool === "run_command") {
    const command = String(input.command ?? input.cmd ?? "").trim();
    if (/[|;&`$<>\\\n]/.test(command)) {
      return { kind: "blocked", reason: "Shell composition can hide repository reads; call pixel with the full task goal" };
    }
    if (/^pixel(?:-dev)? (build-index|prepare-repo|doctor|status)(?:\s|$)/.test(command)) {
      return { kind: "exception", reason: "explicit Pixel recovery or health check" };
    }
    if (/^pixel(?:-dev)?(?:\s|$)/.test(command)) return { kind: "blocked", reason: "Call the structured pixel tool with an outcome" };
    const translation = simpleTranslation(command, root, resolvedPaths);
    if (translation) return { kind: "translated", reason: translation[0], translation };
    if (/\b(ls|tree|rg|grep|find|fd|cat|head|tail|git\s+(status|diff|log|blame|fetch)|python|python3|node|ruby|perl|awk|sed)\b/.test(command)) {
      return { kind: "blocked", reason: "Ambiguous repository read; call pixel with the full task goal" };
    }
    if (/^cp(?:\s|$)/.test(command) && !safeCopy(command, root)) {
      return { kind: "blocked", reason: "Copying repository content outside the repository bypasses Pixel reads" };
    }
    if (/^(cargo|make|just|npm|pnpm|bun|pytest|go|mkdir|cp|mv|rm|touch|chmod|echo|printf|true|false)(?:\s|$)/.test(command)) {
      return { kind: "exception", reason: "build, test, edit, or execution command" };
    }
    return { kind: "blocked", reason: "Unknown shell capability may read the repository; use pixel or a documented exception" };
  }
  return { kind: "blocked", reason: `No repository-read policy for tool ${tool}; use pixel` };
}

export default function activate(pi: ExtensionAPI) {
  const resolvedPaths = new Set<string>();
  pi.registerTool({
    name: "pixel", label: "Pixel",
    description: "Mandatory repository retrieval and Git interface. Provide the desired outcome as goal; actions are stable across Pixel CLI versions.",
    parameters: Type.Object({
      action: Type.Union(ACTIONS.map((a) => Type.Literal(a))),
      goal: Type.Optional(Type.String()),
      query: Type.Optional(Type.String()),
      path: Type.Optional(Type.String()),
      symbol: Type.Optional(Type.String()),
      remote: Type.Optional(Type.String()),
      refspec: Type.Optional(Type.String()),
      files: Type.Optional(Type.Array(Type.String())),
      message: Type.Optional(Type.String()),
      request_id: Type.Optional(Type.String()),
    }),
    async execute(_id, p, _signal, _update, ctx) {
      const root = ctx.cwd;
      const action = p.action as Action;
      try {
        if (!authorized(action, ctx)) {
          const result = { action, error: `User authorization for ${action} is absent from the current request`, next_action: "Ask the user for explicit authorization" };
          audit(root, "blocked", action, { reason: "authorization absent" });
          return { content: [{ type: "text", text: JSON.stringify(result) }], details: result };
        }
        const index = health(root, action);
        const steps = commandFor(action, p);
        const evidence = steps.map((args) => {
          const output = run(root, args);
          return { operation: args[0], output: parseEvidence(output.slice(0, MAX_OUTPUT)), truncated: output.length > MAX_OUTPUT };
        });
        if (action === "find_code" && /\b(investigate|analy[sz]e|understand)\b/i.test(String(p.goal ?? ""))) {
          const found = evidence[0].output as any;
          const matches = found?.matches;
          if (found?.confidence === "resolved" && Array.isArray(matches) && matches.length === 1 && matches[0].symbol_kind) {
            const match = matches[0];
            const uid = `${match.path}#${match.owner ? `${match.owner}::` : ""}${match.raw}#${match.symbol_kind}`;
            for (const args of [["impact", uid, "--json"], ["pack-context", uid, "--json", "--budget", "1200"]]) {
              const output = run(root, args);
              evidence.push({ operation: args[0], output: parseEvidence(output.slice(0, MAX_OUTPUT)), truncated: output.length > MAX_OUTPUT });
            }
          }
        }
        const truncated = evidence.some((item) => item.truncated);
        evidence.forEach((item) => rememberPaths(item.output, resolvedPaths, root));
        const first = evidence[0].output as any;
        const next_action = truncated ? "Narrow the scope or query"
          : action === "find_code" && first?.confidence !== "resolved" ? "Try search_content with a concrete token"
          : action === "search_content" && Array.isArray(first) && first.length === 0 ? "Broaden the query or check index coverage"
          : undefined;
        const result = { action, evidence, index, truncated, next_action };
        audit(root, "tool", action, { index_health: "present", graph_present: index.graph?.present, facts_fresh: index.facts?.fresh, truncated });
        return { content: [{ type: "text", text: JSON.stringify(result) }], details: result };
      } catch (error) {
        const result = { action, error: String(error), next_action: "Repair Pixel availability or narrow the request; no native fallback" };
        audit(root, "blocked", action, { reason: "Pixel unavailable or operation failed" });
        return { content: [{ type: "text", text: JSON.stringify(result) }], details: result };
      }
    },
  });

  pi.on("tool_call", async (event, ctx) => {
    const decision = classify(event.toolName, event.input, ctx.cwd, resolvedPaths);
    audit(ctx.cwd, decision.kind, decision.reason, { tool: event.toolName, raw_read_covered: ["read", "view_file", "bash", "run_command", "grep", "find", "ls", "glob", "list_dir", "grep_search", "file_search"].includes(event.toolName) });
    if (decision.kind === "blocked") return { block: true, reason: JSON.stringify({ policy: "pixel", redirect: decision.reason }) };
    if (decision.readLimit) event.input.limit = decision.readLimit;
    if (decision.translation) {
      let rewrite;
      try {
        const [name, ...args] = decision.translation;
        rewrite = [shellQuote(PIXEL_BIN), shellQuote(resolveOperation(name)), ...args.map(shellQuote), "--metrics", "off"].join(" ");
      }
      catch (error) {
        audit(ctx.cwd, "blocked", "Pixel unavailable", { tool: event.toolName });
        return { block: true, reason: JSON.stringify({ policy: "pixel", error: String(error), redirect: "Repair Pixel, then retry" }) };
      }
      if ("command" in event.input) event.input.command = rewrite;
      else event.input.cmd = rewrite;
    }
    return undefined;
  });
}

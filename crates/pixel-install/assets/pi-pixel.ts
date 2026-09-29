// Pixel extension for Pi — managed by `pixel install --repo`.
// __MANAGED_BEGIN__
// __MANAGED_END__
import { spawnSync } from "node:child_process";
import { appendFileSync, mkdirSync } from "node:fs";
import { isAbsolute, relative, resolve } from "node:path";
import { Type } from "@earendil-works/pi-ai";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

const PIXEL_BIN = __PIXEL_BIN__;
const MAX_OUTPUT = 16000;
const READ_LIMIT = 200;
const BOOTSTRAP_BUDGET = 2400;
const MIN_PROMPT_LEN = 12;
const EDIT_TOOLS = new Set(["edit", "write", "apply_patch", "edit_file", "replace_file_content", "write_to_file"]);
const ACTIONS = ["scope_task", "list_areas", "search_content", "find_code", "impact", "pack_context", "what_changed", "review_changes", "fetch", "commit", "commit_and_push"] as const;
type Action = (typeof ACTIONS)[number];
type Policy = "advisory" | "enforce" | "off";
const POLICIES = new Set<string>(["advisory", "enforce", "off"]);
const policies = new Map<string, Policy>();

// The environment wins without a spawn; otherwise the layered configuration
// decides (`pixel config policy`, the repository file over the global one),
// read once per project root. A Pixel that cannot answer leaves advisory in
// place: configuration is best effort and native tools stay available.
function policyFor(root: string): Policy {
  if (["0", "false", "off"].includes(process.env.PIXEL_TARGETS_GUARD?.trim().toLowerCase() ?? "")) return "off";
  const override = process.env.PIXEL_POLICY?.trim().toLowerCase();
  if (override) return POLICIES.has(override) ? override as Policy : "advisory";
  const cached = policies.get(root);
  if (cached) return cached;
  let policy: Policy = "advisory";
  try {
    const result = spawnSync(PIXEL_BIN, ["config", "policy", "--json", "--metrics", "off"], { cwd: root, encoding: "utf8", timeout: 5000 });
    const reported = result.status === 0 ? JSON.parse(String(result.stdout)).policy : undefined;
    if (POLICIES.has(reported)) policy = reported;
  } catch { /* Unreadable configuration keeps the advisory default. */ }
  policies.set(root, policy);
  return policy;
}
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
    mkdirSync(resolve(root, ".pixel"), { recursive: true });
    appendFileSync(resolve(root, ".pixel/pi-policy.jsonl"), JSON.stringify({
      time: new Date().toISOString(), kind, reason, ...extra,
    }) + "\n");
  } catch { /* Logging must never loosen or break the policy. */ }
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

function simpleTranslation(command: string, root: string) {
  const text = command.trim();
  if (/[|;&`$<>\\\n]/.test(text)) return null;
  const match = /^(ls|rg|grep|git|cat)(?:\s+|$)(.*)$/.exec(text);
  if (!match) return null;
  const [, verb, rest] = match;
  if (rest.startsWith("-")) return null;
  if ((verb === "cat" || verb === "ls") && rest && !inRepo(root, rest)) return null;
  if (verb === "cat" && /^[\w./-]+$/.test(rest)) {
    return ["search-content", "^", rest, "--json", "--limit", "200"];
  }
  if (verb === "ls" && /^[\w./-]*$/.test(rest)) return ["list-areas", "--json"];
  if ((verb === "rg" || verb === "grep") && /^['"]?[\w.*/:-]+['"]?(?:\s+[\w./-]+)?$/.test(rest)) {
    const [query, path] = rest.split(/\s+/);
    if (path && !inRepo(root, path)) return null;
    return ["search-content", query.replace(/^['"]|['"]$/g, ""), ...(path ? [path] : []), "--limit", "40"];
  }
  if (verb === "git") {
    if (rest === "status") return ["repo-state", "--json"];
    if (rest === "diff") return ["review-changes", "--json"];
    if (rest === "log") return ["commit-history", "--json"];
    if (/^blame [\w./-]+$/.test(rest)) return ["who-wrote", rest.slice(6), "--json"];
  }
  return null;
}

function classify(tool: string, input: any, root: string, resolvedPaths: Set<string>, state: { pixelHealthy?: boolean; pixelCalled: boolean }): { kind: string; reason: string; operation?: string } {
  if (tool === "pixel") return { kind: "tool", reason: "structured Pixel" };
  if (EDIT_TOOLS.has(tool)) {
    // Edits stay allowed when Pixel is unhealthy: the pixel tool already
    // reports the repair path, and gating would deadlock the session.
    if (state.pixelHealthy === true && !state.pixelCalled) {
      return { kind: "blocked", reason: "Call pixel before editing: scope_task with your goal, or pack_context on the target symbol" };
    }
    return { kind: "exception", reason: "edit tool" };
  }
  if (["todo", "web_search", "web_contents", "web_answer", "bg_wait"].includes(tool)) {
    return { kind: "exception", reason: "non-repository tool" };
  }
  if (tool === "read" || tool === "view_file") {
    const path = String(input.path ?? input.file_path ?? "");
    if (path && !inRepo(root, path)) return { kind: "exception", reason: "outside repository" };
    const target = path ? relativeTarget(root, path) : "";
    if (path && resolvedPaths.has(target) && Number(input.limit) > 0 && Number(input.limit) <= READ_LIMIT) {
      return { kind: "exception", reason: "bounded Pixel-resolved target read" };
    }
    return { kind: "blocked", reason: "Use pixel with a task goal, or specify a target and a read limit of at most 200 lines" };
  }
  if (tool === "bash" || tool === "run_command") {
    const command = String(input.command ?? input.cmd ?? "").trim();
    // Only suggest routes for simple commands with known capabilities.
    // Compositions and unsupported syntax retain their shell semantics.
    const translation = simpleTranslation(command, root);
    if (translation) return { kind: "blocked", reason: `Use pixel ${translation[0]} for repository retrieval`, operation: translation[0] };
    return { kind: "exception", reason: "native command or unsupported shell syntax" };
  }
  if (["grep", "find", "ls", "glob", "list_dir", "grep_search", "file_search"].includes(tool)) {
    const path = input.path ?? input.directory ?? input.target_directory;
    if (typeof path === "string" && !inRepo(root, path)) return { kind: "exception", reason: "outside repository" };
    return { kind: "blocked", reason: "Use the pixel tool for repository retrieval" };
  }
  return { kind: "exception", reason: "unknown tool capability" };
}

function pixelText(root: string, args: string[]): string | null {
  try {
    const out = run(root, args);
    const trimmed = out.trim();
    if (!trimmed) return null;
    return trimmed;
  } catch {
    return null;
  }
}

function formatWhatChanged(stdout: string): string | null {
  const j = parseEvidence(stdout) as any;
  if (!j || typeof j !== "object") return null;
  const lines: string[] = [];
  lines.push(`changed_files: ${j.changed_files ?? 0}  risk: ${j.risk ?? "?"}`);
  const syms: any[] = j.symbols ?? [];
  for (const s of syms.slice(0, 15)) {
    const procs = s.processes_total ? `  (${s.processes_total} flows)` : "";
    lines.push(`  ${s.change}  ${s.name}  ${s.path}${procs}`);
  }
  if (syms.length > 15) lines.push(`  …+${syms.length - 15} more`);
  const tests: any[] = j.suggested_tests ?? [];
  if (tests.length) lines.push(`suggested tests: ${tests.slice(0, 10).join(", ")}`);
  if (j.envelope_note) lines.push(`note: ${j.envelope_note}`);
  return lines.length > 1 ? lines.join("\n") : null;
}

export default function activate(pi: ExtensionAPI) {
  const resolvedPaths = new Set<string>();
  const state: { pixelHealthy?: boolean; pixelCalled: boolean } = { pixelCalled: false };
  pi.on("session_start", async () => {
    resolvedPaths.clear();
    state.pixelHealthy = undefined;
    state.pixelCalled = false;
    installed = undefined;
  });
  // Keep a distinct name from the global Pixel extension: Pi 0.87.1 rejects
  // duplicate tool names during startup, before session_start can inspect them.
  pi.registerTool({
    name: "pixel_project", label: "Pixel (project)",
    description: "Repository retrieval and Git interface. Provide the desired outcome as goal; actions are stable across Pixel CLI versions.",
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
          rememberPaths(parseEvidence(output), resolvedPaths, root);
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
              rememberPaths(parseEvidence(output), resolvedPaths, root);
              evidence.push({ operation: args[0], output: parseEvidence(output.slice(0, MAX_OUTPUT)), truncated: output.length > MAX_OUTPUT });
            }
          }
        }
        const truncated = evidence.some((item) => item.truncated);
        const first = evidence[0].output as any;
        const next_action = truncated ? "Narrow the scope or query"
          : action === "find_code" && first?.confidence !== "resolved" ? "Try search_content with a concrete token"
          : action === "search_content" && Array.isArray(first) && first.length === 0 ? "Broaden the query or check index coverage"
          : undefined;
        const result = { action, evidence, index, truncated, next_action };
        state.pixelHealthy = true;
        state.pixelCalled = true;
        audit(root, "tool", action, { index_health: "present", graph_present: index.graph?.present, facts_fresh: index.facts?.fresh, truncated });
        return { content: [{ type: "text", text: JSON.stringify(result) }], details: result };
      } catch (error) {
        state.pixelHealthy = false;
        installed = undefined;
        const result = { action, error: String(error), next_action: "Repair Pixel availability or narrow the request; native tools remain available" };
        audit(root, "unavailable", action, { reason: "Pixel unavailable or operation failed" });
        return { content: [{ type: "text", text: JSON.stringify(result) }], details: result };
      }
    },
  });

  // Keep both the global and project-specific Pixel tools available even when
  // a restored tool selection predates this project extension.
  const activatePixelTool = () => {
    const active = pi.getActiveTools();
    const available = new Set(pi.getAllTools().map((tool) => tool.name));
    const pixelTools = ["pixel", "pixel_project"].filter((name) => available.has(name));
    const missing = pixelTools.filter((name) => !active.includes(name));
    if (missing.length) pi.setActiveTools([...active, ...missing]);
  };
  pi.on("session_start", activatePixelTool);
  pi.on("model_select", activatePixelTool);

  // Task context is independent of optional enforcement. Parse complete
  // evidence before capping the text sent to the model.
  pi.on("before_agent_start", async (event, ctx) => {
    const root = ctx?.cwd ?? process.cwd();
    const prompt = String((event as any).prompt ?? "");
    if (prompt.trim().length < MIN_PROMPT_LEN) return;
    try {
      const index = health(root, "scope_task");
      const scope = run(root, ["scope-task", prompt, "--json", "--no-manifest", "--max-tier", "P1", "--limit", "15"]).trim();
      const repo = run(root, ["repo-state", "--json"]).trim();
      for (const text of [scope, repo]) rememberPaths(parseEvidence(text), resolvedPaths, root);
      state.pixelHealthy = true;
      audit(root, "bootstrap", "scope-task and repo-state injected", { graph_present: index.graph?.present });
      const guidance = policyFor(root) === "enforce"
        ? "Enforcement is enabled for supported native retrieval. Call the pixel tool before editing; compositions and unsupported syntax retain native behavior."
        : "Use these targets as suggestions. Native tools remain available; `pixel config policy enforce` opts into retrieval enforcement, `pixel config policy off` disables policy checks.";
      return {
        message: {
          customType: "pixel-bootstrap", display: false,
          content: `PIXEL TASK CONTEXT (deterministic, from pixel scope-task + repo-state):\n\n${scope.slice(0, BOOTSTRAP_BUDGET)}${scope.length > BOOTSTRAP_BUDGET ? "\n…(truncated)" : ""}\n\n${repo.slice(0, 800)}${repo.length > 800 ? "\n…(truncated)" : ""}\n\n${guidance}`,
        },
      };
    } catch (error) {
      state.pixelHealthy = false;
      installed = undefined;
      audit(root, "bootstrap", "pixel unavailable", { reason: String(error) });
      return {
        message: {
          customType: "pixel-bootstrap", display: false,
          content: `PIXEL UNAVAILABLE: ${String(error)}. Repair Pixel when possible. Native tools remain available.`,
        },
      };
    }
  });

  pi.on("tool_call", async (event, ctx) => {
    const mode = policyFor(ctx.cwd);
    if (mode === "off") return;
    const decision = classify(event.toolName, event.input, ctx.cwd, resolvedPaths, state);
    if (decision.kind === "blocked" && mode === "enforce" && state.pixelHealthy === true) {
      try { if (decision.operation) resolveOperation(decision.operation); }
      catch {
        state.pixelHealthy = false;
        installed = undefined;
        audit(ctx.cwd, "advisory", "Pixel capability unavailable; native tool preserved", { tool: event.toolName });
        return;
      }
      audit(ctx.cwd, "blocked", decision.reason, { tool: event.toolName });
      return { block: true, reason: JSON.stringify({ policy: "pixel", redirect: decision.reason }) };
    }
    audit(ctx.cwd, decision.kind === "blocked" ? "advisory" : decision.kind, decision.reason, { tool: event.toolName });
    return undefined;
  });

  // A tool_result replacement must carry forward the original result. Only
  // successful edits have a post-edit snapshot; failures retain diagnostics.
  pi.on("tool_result", async (event, ctx) => {
    if (!EDIT_TOOLS.has(event.toolName) || event.isError) return;
    const root = ctx?.cwd ?? process.cwd();
    const raw = pixelText(root, ["what-changed", "--json", "--tests"]);
    if (!raw) {
      state.pixelHealthy = false;
      installed = undefined;
      return;
    }
    const compact = formatWhatChanged(raw);
    if (!compact) return;
    return {
      content: [...event.content, { type: "text", text: `PIXEL BLAST RADIUS (pixel what-changed):\n\n${compact}\n\nEdits are unverified until the project's build/tests run.` }],
      details: event.details,
      isError: event.isError,
    };
  });
}

// Executable extension contract: bun scripts/test-pi-policy.mjs
// The real extension handles events; only its host and Pixel process are fixtures.
import assert from "node:assert/strict";
import { chmodSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

const root = mkdtempSync(join(tmpdir(), "pi-policy-"));
const binary = join(root, "pixel");
const trace = join(root, "calls.jsonl");
const settingsPath = join(root, "settings.json");
const editedPath = join(root, "edited.txt");
const originalEnv = [process.env.PIXEL_POLICY, process.env.PIXEL_TARGETS_GUARD];
let passed = 0;
const configure = (settings = {}) => writeFileSync(settingsPath, JSON.stringify(settings));
const calls = () => readFileSync(trace, "utf8").trim().split("\n").filter(Boolean).map(JSON.parse);
const restore = (name, value) => value === undefined ? delete process.env[name] : process.env[name] = value;

try {
  configure();
  writeFileSync(trace, "");
  writeFileSync(editedPath, "before");
  // The bash fence's canonical containment check requires an existing
  // repository tree. Read fixtures stay lexical, so an empty `src/`
  // directory is enough for `ls src`, `rg error src` and `cat src/main.rs`
  // to exercise `arg_reads_repo`.
  mkdirSync(join(root, "src"), { recursive: true });
  writeFileSync(join(root, "src/main.rs"), "fn main() {}\n");
  writeFileSync(binary, `#!${process.execPath}
import { appendFileSync, readFileSync } from "node:fs";
const args = process.argv.slice(2);
const settings = JSON.parse(readFileSync(${JSON.stringify(settingsPath)}, "utf8"));
appendFileSync(${JSON.stringify(trace)}, JSON.stringify(args) + "\\n");
if (settings.fail?.includes(args[0])) { console.error("fixture Pixel unavailable: " + args[0]); process.exit(1); }
const operations = ["status", "scope-task", "repo-state", "find-code", "fetch", "commit", "commit-and-push", "list-areas", "search-content", "impact", "pack-context", "what-changed"];
const box = "warning: diagnostic line\\n\u{1F7E9} pixel " + args[0] + " \u2740 1.0ms\\n  \u2502\\n  \u2514\u2500\u2500\u2500\\n";
if (!args.includes("off") && !["--version", "--help"].includes(args[0])) process.stderr.write(box);
switch (args[0]) {
  case "--version": console.log("pixel 0.6.0"); break;
  case "--help": console.log("Commands:\\n" + operations.filter(op => !settings.missing?.includes(op)).map(op => "  " + op + "  Operation").join("\\n")); break;
  case "status": console.log(JSON.stringify({index:{base_files:1},graph:{present:true},facts:{fresh:true}})); break;
  case "config": console.log(JSON.stringify({policy: settings.policy ?? "advisory", source: "repo"})); break;
  case "scope-task": console.log(JSON.stringify({padding: "x".repeat(settings.scopePadding ?? 0), targets:[{path:"src/main.rs"}]})); break;
  case "repo-state": console.log(JSON.stringify({branch:"fixture"})); break;
  case "find-code": console.log(JSON.stringify(settings.findAmbiguous ? {confidence:"ranked",matches:[{path:"src/a.rs",raw:"main",symbol_kind:"function"},{path:"src/b.rs",raw:"main",symbol_kind:"function"}]} : {padding: "x".repeat(settings.findPadding ?? 0), confidence:"resolved",matches:[{path:"src/found.rs",raw:"main",symbol_kind:"function"}]})); break;
  case "what-changed": console.log(JSON.stringify({changed_files:1,risk:"LOW",symbols:[{change:"modified",name:readFileSync(${JSON.stringify(editedPath)}, "utf8"),path:"src/main.rs"}],suggested_tests:["main_tests"]})); break;
  default: console.log(JSON.stringify({ok:true,op:args[0]}));
}
`);
  chmodSync(binary, 0o755);
  const asset = readFileSync(new URL("../crates/pixel-install/assets/pi-pixel.ts", import.meta.url), "utf8");
  const source = asset.replace("__PIXEL_BIN__", JSON.stringify(binary))
    .replace('import { Type } from "@earendil-works/pi-ai";', 'const Type = new Proxy({}, {get: () => (...args) => args});');
  const extension = join(root, "pixel-guard.ts");
  writeFileSync(extension, source);
  const { default: activate } = await import(pathToFileURL(extension).href);
  let cwd = root;
  const user = (text = "inspect the implementation") => ({
    cwd,
    sessionManager: { getBranch: () => [{ type: "message", message: { role: "user", content: text } }] },
  });
  const host = async (mode, projectRoot = root) => {
    cwd = projectRoot;
    restore("PIXEL_POLICY", mode);
    delete process.env.PIXEL_TARGETS_GUARD;
    // Every host (including fresh sub-projects in the policy loop) needs an
    // existing `src/` so the bash fence's canonical containment check has a
    // directory to canonicalize. The read tool stays lexical.
    mkdirSync(join(projectRoot, "src"), { recursive: true });
    const handlers = new Map();
    let tool;
    // A restored session can come back without the project's tools selected.
    let active = ["bash", "edit", "read"];
    const available = [...active, "pixel", "pixel_project"];
    activate({
      registerTool: (registered) => { tool = registered; },
      on: (name, handler) => handlers.set(name, [...handlers.get(name) ?? [], handler]),
      getActiveTools: () => active,
      getAllTools: () => available.map((name) => ({ name })),
      setActiveTools: (names) => { active = names; },
    });
    const emit = async (name, event, ctx = user()) => {
      let result;
      for (const handler of handlers.get(name) ?? []) {
        const next = await handler(event, ctx);
        if (next) result = next;
        if (next?.block) break;
      }
      return result;
    };
    await emit("session_start", { reason: "startup" });
    assert.ok(active.includes("pixel") && active.includes("pixel_project"), "session start activates both pixel tools");
    return { emit, tool, boot: () => emit("before_agent_start", { prompt: "inspect the implementation" }) };
  };
  const check = async (name, test) => {
    configure();
    await test();
    passed += 1;
    console.log(`ok ${passed} - ${name}`);
  };
  const native = (command, toolName = "bash") => ({ toolName, toolCallId: "shell", input: { command } });
  const edit = () => ({ toolName: "edit", toolCallId: "edit", input: { path: "src/main.rs" } });
  const read = (path = "src/main.rs", limit) => ({ toolName: "read", input: { path, ...(limit === undefined ? {} : { limit }) } });
  const pixelResult = (isError = false, toolName = "pixel") => ({ toolName, toolCallId: "pixel", input: {}, content: [{ type: "text", text: "{}" }], isError });
  const count = (op) => calls().filter(([name]) => name === op).length;

  await check("advisory default and invalid settings preserve every native input", async () => {
    for (const mode of [undefined, "advisory", "invalid"]) {
      const h = await host(mode);
      const boot = await h.boot();
      assert.match(boot.message.content, /Native tools remain available/);
      for (const event of [native("ls src"), native("cat src/main.rs"), read(), edit(), { toolName: "unfamiliar", input: {} }]) {
        const before = structuredClone(event);
        assert.equal(await h.emit("tool_call", event), undefined);
        assert.deepEqual(event, before);
      }
    }
  });

  await check("configuration decides enforcement when the environment is silent", async () => {
    for (const [policy, blocked, guidance] of [["advisory", false, /Native tools remain available/], ["enforce", true, /Enforcement is enabled/], ["off", false, /Native tools remain available/]]) {
      configure({ policy });
      const project = mkdtempSync(join(root, `config-${policy}-`));
      const h = await host(undefined, project);
      assert.match((await h.boot()).message.content, guidance);
      const event = native("ls src");
      assert.equal(Boolean((await h.emit("tool_call", event))?.block), blocked, policy);
      assert.deepEqual(calls().find(([name]) => name === "config")?.slice(1), ["policy", "--json", "--metrics", "off"], policy);
    }
  });

  await check("enforcement blocks supported simple retrieval and gates edits explicitly", async () => {
    const h = await host("enforce");
    await h.boot();
    for (const event of [
      native("ls src"), native("ls -l src"), native("ls -la src"),
      native("rg error src"), native("rg -n error src"), native("rg -m 1 error src"),
      native("cat src/main.rs"), native("head src/main.rs"), native("tail src/main.rs"),
      native("git status"), native("git -C . log"), native("git --no-pager diff"),
      native("find src"), native("find src -name '*.rs'"),
      native("cp src/main.rs /tmp/pixel-leaf-dest"),
      read("src/unknown.rs", 100), edit(),
    ]) {
      const before = structuredClone(event);
      assert.equal((await h.emit("tool_call", event)).block, true, before.input.command ?? before.toolName);
      assert.deepEqual(event, before);
    }
    const result = await h.tool.execute("find", { action: "find_code", goal: "main" }, null, null, user());
    assert.equal(result.details.action, "find_code");
    assert.equal(await h.emit("tool_call", edit()), undefined);
    assert.equal(await h.emit("tool_call", read("src/found.rs", 200)), undefined);
    assert.equal((await h.emit("tool_call", read("src/found.rs", 201))).block, true);
    assert.equal((await h.emit("tool_call", read("src/found.rs"))).block, true);
  });

  await check("enforcement aligns bash reasons with the rust leaf table", async () => {
    const h = await host("enforce");
    await h.boot();
    const why = async (command) => {
      const event = native(command);
      const result = await h.emit("tool_call", event);
      return result ? JSON.parse(result.reason).redirect : undefined;
    };
    assert.equal(await why("cat src/main.rs"), "repository read: use pixel search-content or pixel pack-context <uid>");
    assert.equal(await why("head src/main.rs"), "repository read: use pixel search-content or pixel pack-context <uid>");
    assert.equal(await why("awk '{print}' src/main.rs"), "repository read: use pixel search-content or pixel pack-context <uid>");
    assert.equal(await why("sed 's/a/b/' src/main.rs"), "repository read: use pixel search-content or pixel pack-context <uid>");
    assert.equal(await why("cp src/main.rs /tmp/x"), "repository read: use pixel search-content or pixel pack-context <uid>");
    assert.equal(await why("rg error src"), "repository search: use pixel search-content");
    assert.equal(await why("grep error src"), "repository search: use pixel search-content");
    assert.equal(await why("ls src"), "repository discovery: use pixel list-areas or find-code");
    assert.equal(await why("find src"), "repository discovery: use pixel find-code or list-areas");
    assert.equal(await why("git status"), "repository inspection: use pixel repo-state");
    assert.equal(await why("git diff"), "repository inspection: use pixel review-changes");
    assert.equal(await why("git log"), "repository inspection: use pixel commit-history");
  });

  await check("bash leaf operands honor the credential path regex", async () => {
    const h = await host("enforce");
    await h.boot();
    writeFileSync(join(root, ".env"), "K=v\n");
    for (const command of ["cat .env", "head .env", "rg -n needle .env", "cp .env /tmp/pixel-leaf-dest"]) {
      const event = native(command);
      const result = await h.emit("tool_call", event);
      assert.equal(result.block, true, command);
      assert.equal(JSON.parse(result.reason).redirect, "credential path", command);
      assert.equal(event.input.command, command);
    }
  });

  await check("enforcement preserves compositions, quoted punctuation and unknown capabilities", async () => {
    const h = await host("enforce");
    await h.boot();
    for (const toolName of ["bash", "run_command"]) {
      for (const command of [
        "cargo test | tail -20", "cargo test | rg error", "pixel repo-state --json | jq .branch",
        "cargo test > output.log", "cargo test && rg error output.log",
        "echo 'cat src/main.rs'", "printf 'a|b;$(literal)'", "cat src/a\\ b.rs",
        "git fetch origin", "pixel find-code main", "python3 inspect.py", "lsfoo src",
        "mv src/main.rs /tmp/pixel-move", "cat /tmp/external.txt",
        "lsof", "lsblk", "catapult", "ls /tmp", "grep needle /tmp/external.txt",
        "cd src && ls src", "command ls src", "builtin echo src",
        "find /tmp -name 'x'", "rg -m 1 needle /etc/hosts",
      ]) {
        const event = native(command, toolName);
        assert.equal(await h.emit("tool_call", event), undefined, command);
        assert.equal(event.input.command, command);
      }
    }
    assert.equal(await h.emit("tool_call", { toolName: "unfamiliar", input: {} }), undefined);
    for (const toolName of ["grep", "find", "ls", "glob", "list_dir", "grep_search", "file_search"]) {
      assert.equal(await h.emit("tool_call", { toolName, input: { path: "/tmp/external" } }), undefined);
    }
  });

  await check("off and the legacy opt-out bypass policy classification and audit", async () => {
    for (const optout of [undefined, "0", "false", "OFF"]) {
      const h = await host(optout === undefined ? "off" : "enforce");
      restore("PIXEL_TARGETS_GUARD", optout);
      await h.boot();
      const auditPath = join(root, ".pixel/pi-policy.jsonl");
      const before = readFileSync(auditPath, "utf8");
      for (const event of [native("ls src"), read(), edit()]) assert.equal(await h.emit("tool_call", event), undefined);
      assert.equal(readFileSync(auditPath, "utf8"), before);
    }
  });

  await check("post-edit context follows execution and preserves full native results", async () => {
    const h = await host();
    const before = count("what-changed");
    await h.emit("tool_call", edit());
    assert.equal(count("what-changed"), before);
    writeFileSync(editedPath, "after_edit");
    const event = { ...edit(), content: [{ type: "text", text: "Edit applied: 2 lines" }, { type: "image", data: "fixture", mimeType: "image/png" }], details: { diff: "+ updated", firstChangedLine: 8 }, isError: false };
    const result = await h.emit("tool_result", event);
    assert.deepEqual(result.content.slice(0, 2), event.content);
    assert.match(result.content[2].text, /modified  after_edit  src\/main.rs/);
    assert.deepEqual(result.details, event.details);
    assert.equal(result.isError, false);
    assert.equal(count("what-changed"), before + 1);
  });

  await check("failed edits retain their diagnostics and do not collect impact", async () => {
    const h = await host();
    const before = count("what-changed");
    const event = { ...edit(), content: [{ type: "text", text: "Exact match not found" }], details: { path: "src/main.rs" }, isError: true };
    const original = structuredClone(event);
    assert.equal(await h.emit("tool_result", event), undefined);
    assert.deepEqual(event, original);
    assert.equal(count("what-changed"), before);
  });

  await check("unavailable post-edit context preserves successful native output", async () => {
    const h = await host("enforce");
    await h.boot();
    configure({ fail: ["what-changed"] });
    const event = { ...edit(), content: [{ type: "text", text: "Edit applied" }], details: { diff: "+ changed" }, isError: false };
    assert.equal(await h.emit("tool_result", event), undefined);
    assert.equal(event.content[0].text, "Edit applied");
    assert.equal(await h.emit("tool_call", native("ls src")), undefined);
  });

  await check("bootstrap and structured-tool failures fail open and recover", async () => {
    for (const op of ["status", "scope-task", "repo-state"]) {
      const h = await host("enforce");
      configure({ fail: [op] });
      const boot = await h.boot();
      assert.match(boot.message.content, /PIXEL UNAVAILABLE/);
      assert.equal(await h.emit("tool_call", edit()), undefined);
      assert.equal(await h.emit("tool_call", native("ls src")), undefined);
      configure();
      await h.boot();
      assert.equal((await h.emit("tool_call", edit())).block, true);
      configure({ fail: ["find-code"] });
      const result = await h.tool.execute("find", { action: "find_code", goal: "main" }, null, null, user());
      assert.match(result.details.error, /fixture Pixel unavailable/);
      assert.equal(await h.emit("tool_call", edit()), undefined);
      configure();
    }
  });

  await check("missing route capability fails open without blocking native retrieval", async () => {
    const h = await host("enforce");
    configure({ missing: ["list-areas"] });
    await h.boot();
    assert.equal(await h.emit("tool_call", native("ls src")), undefined);
    assert.equal(await h.emit("tool_call", edit()), undefined);
  });

  await check("complete bootstrap and tool evidence resolve paths before truncation", async () => {
    const h = await host("enforce");
    configure({ scopePadding: 3000, findPadding: 17000 });
    const boot = await h.boot();
    assert.match(boot.message.content, /truncated/);
    assert.equal((await h.emit("tool_call", read("src/main.rs", 200))).block, true, "bootstrap paths alone do not unlock reads");
    assert.equal(await h.emit("tool_result", pixelResult()), undefined);
    assert.equal(await h.emit("tool_call", read("src/main.rs", 200)), undefined);
    const found = await h.tool.execute("find", { action: "find_code", goal: "main" }, null, null, user());
    assert.equal(found.details.truncated, true);
    assert.equal(await h.emit("tool_call", read("src/found.rs", 200)), undefined);
  });

  await check("bounded reads unlock only after a successful pixel result", async () => {
    const h = await host("enforce");
    await h.boot();
    assert.equal((await h.emit("tool_call", read("src/main.rs", 100))).block, true, "blocked before any pixel call");
    await h.emit("tool_result", pixelResult(true));
    assert.equal((await h.emit("tool_call", read("src/main.rs", 100))).block, true, "a failed pixel result does not unlock");
    await h.emit("tool_result", pixelResult(false, "pixel_project"));
    assert.equal(await h.emit("tool_call", read("src/main.rs", 100)), undefined, "allowed after a successful result");
    assert.equal((await h.emit("tool_call", read("src/main.rs", 201))).block, true, "the read limit still applies");
  });

  await check("an unauthorized pixel_project commit is an error and does not count as a call", async () => {
    const h = await host("enforce");
    await h.boot();
    const denied = await h.tool.execute("denied", { action: "commit", files: ["src/main.rs"], message: "x", request_id: "r" }, null, null, user("do not commit"));
    assert.match(denied.content[0].text, /authorization.*absent/);
    assert.equal(denied.isError, true, "a denied commit is an error result");
    const relayed = await h.emit("tool_result", { toolName: "pixel_project", toolCallId: "denied", input: {}, ...denied, isError: false });
    assert.equal(relayed.isError, true, "the error-shaped payload reaches the tool_result session message");
    assert.equal((await h.emit("tool_call", read("src/main.rs", 100))).block, true, "a denied commit does not unlock reads");
  });

  await check("a successful tool result carries the metrics box as its own content item", async () => {
    const h = await host("enforce");
    await h.boot();
    const result = await h.tool.execute("find", { action: "find_code", goal: "main" }, null, null, user());
    assert.equal(result.content.length, 2);
    assert.match(result.content[0].text, /^\{/);
    assert.equal(result.content[1].text, "\u{1F7E9} pixel find-code \u2740 1.0ms\n  \u2502\n  \u2514\u2500\u2500\u2500");
    assert.doesNotMatch(result.content[0].text + result.content[1].text, /warning: diagnostic/);
    const quiet = calls().filter(([name]) => ["status", "scope-task", "repo-state"].includes(name));
    assert.ok(quiet.length > 0 && quiet.every((args) => args.slice(-2).join(" ") === "--metrics off"), "probes stay silent");
    const shown = calls().filter(([name]) => name === "find-code");
    assert.ok(shown.length > 0 && shown.every((args) => !args.includes("off")), "tool runs keep metrics");
  });

  await check("blocked reads say what was wrong and a Pixel call alone unlocks in-repo reads", async () => {
    const h = await host("enforce");
    await h.boot();
    const why = async (event) => JSON.parse((await h.emit("tool_call", event)).reason).redirect;
    const tail = ". Call pixel first, then read with a limit of at most 200 lines";
    assert.equal(await why(read("src/other.rs", 100)), "Read blocked: path not resolved by pixel yet" + tail);
    assert.equal(await why(read("src/other.rs")), "Read blocked: no limit given" + tail);
    assert.equal(await why(read("src/other.rs", 300)), "Read blocked: limit 300 exceeds 200" + tail);
    await h.emit("tool_result", pixelResult());
    assert.equal(await h.emit("tool_call", read("src/never-resolved.rs", 200)), undefined, "global pixel result unlocks any in-repo path");
    assert.equal(await why(read("src/other.rs", 300)), "Read blocked: limit 300 exceeds 200" + tail);
    assert.equal(await why(read(".env", 10)), "Read blocked: credential path" + tail);
    assert.equal(await why(read("@.env", 10)), "Read blocked: credential path" + tail);
    assert.equal(await why(read("keys/id_rsa", 10)), "Read blocked: credential path" + tail);
    assert.equal(await h.emit("tool_call", read("/etc/hosts", 10)), undefined, "outside the repository stays native");
  });

  await check("short prompts and session changes never retain stale edit or path state", async () => {
    const h = await host("enforce");
    assert.equal(await h.emit("before_agent_start", { prompt: "fix" }), undefined);
    assert.equal(await h.emit("tool_call", edit()), undefined);
    await h.tool.execute("find", { action: "find_code", goal: "main" }, null, null, user());
    assert.equal(await h.emit("tool_call", read("src/found.rs", 100)), undefined);
    await h.emit("session_start", { reason: "new" });
    assert.equal(await h.emit("tool_call", edit()), undefined);
    await h.boot();
    assert.equal((await h.emit("tool_call", edit())).block, true);
    assert.equal((await h.emit("tool_call", read("src/found.rs", 100))).block, true);
  });

  await check("structured fetch stays isolated and commit/push authorization survives off", async () => {
    const h = await host("off");
    const fetchBefore = count("fetch");
    const pushBefore = count("commit-and-push");
    const params = { action: "commit_and_push", files: ["src/main.rs"], message: "test", request_id: "fixture" };
    await h.tool.execute("fetch", { action: "fetch", remote: "origin" }, null, null, user("fetch only"));
    assert.equal(count("fetch"), fetchBefore + 1);
    for (const text of ["fetch only", "do not commit and push", "don't commit yet", "commit messages look wrong"]) {
      const result = await h.tool.execute("denied", params, null, null, user(text));
      assert.match(result.content[0].text, /authorization.*absent/);
    }
    assert.equal(count("commit-and-push"), pushBefore);
    await h.tool.execute("allowed", params, null, null, user("commit and push this change"));
    assert.equal(count("commit-and-push"), pushBefore + 1);
  });
  await check("pixel tool results resolve read targets and bare symbols reach pack-context", async () => {
    const h = await host("enforce");
    await h.boot();
    await h.emit("tool_result", {
      toolName: "pixel", isError: false,
      content: [{ type: "text", text: JSON.stringify({ targets: [{ path: "src/other.rs" }] }) }],
    });
    assert.equal(await h.emit("tool_call", read("src/other.rs", 120)), undefined);
    assert.equal((await h.emit("tool_call", read("src/other.rs", 201))).block, true);
    const before = count("pack-context");
    await h.tool.execute("pack", { action: "pack_context", symbol: "main" }, null, null, user());
    const packCalls = calls().filter(([name]) => name === "pack-context");
    assert.equal(packCalls.length, before + 1);
    assert.ok(packCalls.at(-1).includes("src/found.rs#main#function"), JSON.stringify(packCalls.at(-1)));
  });
  await check("ambiguous pack_context target returns find_code candidate uids", async () => {
    configure({ findAmbiguous: true });
    const h = await host("enforce");
    const result = await h.tool.execute("pack", { action: "pack_context", symbol: "main" }, null, null, user());
    const details = JSON.parse(result.content[0].text);
    assert.match(details.next_action, /find_code/);
    assert.match(details.next_action, /src\/a\.rs#main#function/);
    const packCalls = calls().filter(([name]) => name === "pack-context");
    assert.ok(packCalls.at(-1).includes('"main"') || packCalls.at(-1).includes("main"), JSON.stringify(packCalls.at(-1)));
  });

  console.log(`Pi extension: ${passed} event-handler contract groups passed`);
} finally {
  restore("PIXEL_POLICY", originalEnv[0]);
  restore("PIXEL_TARGETS_GUARD", originalEnv[1]);
  rmSync(root, { recursive: true, force: true });
}

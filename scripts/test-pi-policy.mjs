// Focused policy test: node --experimental-strip-types scripts/test-pi-policy.mjs
// Uses a fake Pixel process so every write operation is visible in a trace.
import assert from "node:assert/strict";
import { mkdtempSync, mkdirSync, readFileSync, writeFileSync, chmodSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

const root = mkdtempSync(join(tmpdir(), "pi-policy-"));
mkdirSync(join(root, ".pixel"));
const binary = join(root, "pixel");
const trace = join(root, "calls.jsonl");
writeFileSync(binary, `#!/usr/bin/env node
const fs = require("node:fs");
const args = process.argv.slice(2);
fs.appendFileSync(${JSON.stringify(trace)}, JSON.stringify(args) + "\\n");
switch (args[0]) {
 case "--version": console.log("pixel 0.6.0"); break;
 case "--help": console.log("Commands:\\n  status  Index status\\n  find-code  Find code\\n  fetch  Fetch\\n  commit  Commit\\n  commit-and-push  Ship\\n  list-areas  Areas\\n  search-content  Search\\n  impact  Impact\\n  pack-context  Context"); break;
 case "status": console.log(JSON.stringify({index:{base_files:1},graph:{present:true},facts:{fresh:true}})); break;
 case "find-code": console.log(JSON.stringify({confidence:"resolved",matches:[{path:"src/main.rs",raw:"main",symbol_kind:"function"}]})); break;
 default: console.log(JSON.stringify({ok:true,op:args[0]}));
}
`);
chmodSync(binary, 0o755);

const asset = readFileSync(new URL("../crates/pixel-install/assets/pi-pixel.ts", import.meta.url), "utf8");
const source = asset
  .replace("__PIXEL_BIN__", JSON.stringify(binary))
  .replace('import { Type } from "@earendil-works/pi-ai";',
    'const Type = new Proxy({}, {get: () => (...args) => args});');
const extension = join(root, "pixel-guard.ts");
writeFileSync(extension, source);
const { default: activate } = await import(pathToFileURL(extension).href);
let tool;
let guard;
activate({ registerTool: (value) => { tool = value; }, on: (name, handler) => { assert.equal(name, "tool_call"); guard = handler; } });
const user = (text) => ({ cwd: root, sessionManager: { getBranch: () => [{type:"message", message:{role:"user", content:[{type:"text", text}]}}] } });
const userString = (text) => ({ cwd: root, sessionManager: { getBranch: () => [{type:"message", message:{role:"user", content:text}}] } });
const calls = () => readFileSync(trace, "utf8").trim().split("\n").map(JSON.parse);

const ls = {toolName:"bash", input:{command:"ls src"}};
assert.equal(await guard(ls, user("inspect")), undefined);
assert.match(ls.input.command, /list-areas/);
assert.doesNotMatch(ls.input.command, /^ls /);
const cat = {toolName:"bash", input:{command:"cat src/main.rs"}};
assert.equal((await guard(cat, user("inspect"))).block, true);
assert.equal(cat.input.command, "cat src/main.rs");
const composite = {toolName:"bash", input:{command:"pixel status; cat src/main.rs"}};
assert.equal((await guard(composite, user("inspect"))).block, true);
assert.equal(composite.input.command, "pixel status; cat src/main.rs");
const read = {toolName:"read", input:{path:"src/main.rs"}};
assert.equal((await guard(read, user("inspect"))).block, true);

await tool.execute("1", {action:"find_code", goal:"main"}, null, null, user("find main"));
assert.equal(await guard(read, user("inspect")), undefined);
assert.equal(read.input.limit, 200);
const scopedCat = {toolName:"bash", input:{command:"cat src/main.rs"}};
assert.equal(await guard(scopedCat, user("inspect")), undefined);
assert.match(scopedCat.input.command, /search-content/);

await tool.execute("2", {action:"fetch", remote:"origin"}, null, null, user("fetch only"));
assert.equal(calls().filter((args) => args[0] === "fetch").length, 1);
const denied = await tool.execute("3", {action:"commit_and_push", files:["src/main.rs"], message:"test", request_id:"denied"}, null, null, user("fetch only"));
assert.match(denied.content[0].text, /authorization.*absent/);
assert.equal(calls().filter((args) => args[0] === "commit-and-push").length, 0);
for (const text of ["do not commit and push", "don't commit yet", "commit messages look wrong"]) {
  const result = await tool.execute("no", {action:"commit_and_push", files:["src/main.rs"], message:"test", request_id:"denied"}, null, null, userString(text));
  assert.match(result.content[0].text, /authorization.*absent/, text);
}
assert.equal(calls().filter((args) => args[0] === "commit-and-push").length, 0);
await tool.execute("4", {action:"commit_and_push", files:["src/main.rs"], message:"test", request_id:"allowed"}, null, null, user("commit and push this change"));
assert.equal(calls().filter((args) => args[0] === "commit-and-push").length, 1);
await tool.execute("5", {action:"commit", files:["src/main.rs"], message:"test", request_id:"allowed-2"}, null, null, userString("Please commit this change"));
assert.equal(calls().filter((args) => args[0] === "commit").length, 1);
console.log("Pi policy: translation, pre-execution block, scoped read, fetch isolation, and write authorization passed");

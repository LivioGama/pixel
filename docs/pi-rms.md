# Pixel in Pi RMS

Run `pixel install` to install Pi's short system rule, then `pixel install --repo .`
in each repository. Trust the project in Pi. The project extension at
`.pi/extensions/pixel-guard.ts` registers `pixel` and checks every tool call
before Pi executes it. Reload or a new session loads the same project extension;
model changes do not alter registered tools. `pixel doctor .` checks its location.

| User outcome | Stable action | Pixel CLI operation |
| --- | --- | --- |
| Find likely files for a task | `scope_task` | `scope-task` |
| See repository areas | `list_areas` | `list-areas` |
| Search text | `search_content` | `search-content` |
| Locate code by phrase | `find_code` | `find-code` |
| Inspect symbol effects | `impact` | `impact` |
| Read focused symbol context | `pack_context` | `pack-context` |
| Inspect changed symbols | `what_changed` | `what-changed` |
| Review the working tree | `review_changes` | `review-changes` |
| Fetch remote refs | `fetch` | `fetch` |
| Commit named files | `commit` | `commit` |
| Commit and push named files | `commit_and_push` | `commit-and-push` |

The action names remain stable if Pixel's CLI spelling changes. The extension
reports an explicit error if the installed executable lacks an operation.
Responses include bounded evidence, truncation, index and graph state, and a
next action when results are capped or Pixel is unavailable. The extension
checks `pixel status` before invoking an action. If the index is missing, run
`pixel build-index --history .`. If the graph is missing, run
`pixel rebuild-graph .`. Then retry. There is no native
discovery fallback. History facts can lag; their `fresh` field is returned.

## Native tool policy

Simple `ls`, `rg`, `grep`, and `git status`, `diff`, `log`, `blame`, or `fetch`
shell commands are rewritten to Pixel calls before execution. `git fetch`
rewrites only to `pixel fetch`. Ambiguous commands, pipelines, batches,
indirect reads, and unknown shell capabilities are blocked with a structured
redirect to the `pixel` tool. Pi's `read` tool can read a repository path only
after the Pixel tool has resolved it, with a limit of at most 200 lines. Reads
of unresolved paths are blocked even with a limit. A path outside the
repository is exempt. Edit and write tools, builds, tests, and
execution commands remain available; copying or moving repository content
to an outside path is blocked. `pixel build-index`, `prepare-repo`,
`doctor`, and `status` are direct recovery and health exceptions.

The extension checks Pi tool calls, including read, bash, and named discovery
tools. It cannot intercept Pi's own project context loading before an agent
turn, file access performed *inside* an allowed build or test process, or a
tool process that bypasses Pi's `tool_call` event. The shell allowlist blocks
arbitrary interpreter one-liners and shell metacharacters; it cannot prove a
build script never reads repository files. These are the enforcement limits,
not guarantees of repository sandboxing.

`commit` and `commit_and_push` require explicit user intent in the current
user message, plus named files, a message, and an idempotency request ID.
`commit_and_push` requires intent to push as well. A fetch never grants it.
The extension cannot cryptographically authenticate natural-language intent;
it applies this check at tool execution and reports denials.

Policy decisions are logged in `.pixel/pi-policy.jsonl` with decision kind,
tool, reason, coverage flag, and health or truncation metadata. The log does
not record commands, query strings, file contents, or commit messages. Count
`translated` entries to inspect rewrite frequency. This is an operational
trace, not an audit of reads by allowed child processes.

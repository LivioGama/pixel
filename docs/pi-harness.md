# Pixel in the Pi harness

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
`pixel rebuild-graph .`. Then retry. Native tools remain available under the
default advisory policy. History facts can lag; their `fresh` field is returned.

## Native tool policy

Choose the policy with `pixel config policy`, or for one environment with
`PIXEL_POLICY`:

| Value | Behaviour |
| --- | --- |
| `advisory` (default) | Suggest Pixel retrieval while preserving native tool inputs and execution. |
| `enforce` | Redirect supported simple retrieval commands and apply the scoped read/edit gates described below. |
| `off` | Skip classification, policy logging and read/edit gates. |

`pixel config policy enforce` writes the repository layer
(`<repo>/.pixel/config.yaml`); `--global` writes the machine-wide
`~/.pixel/config.yaml`. The repository file wins over the global one,
`PIXEL_POLICY` overrides both for one environment, and an absent or
unrecognised value selects `advisory`. The extension reads the effective
setting once per project (`pixel config policy --json`) and keeps the
advisory default when Pixel cannot answer. The legacy `PIXEL_TARGETS_GUARD=0`,
`false`, or `off` also disables the policy. These settings do not disable the
structured Pixel tool's write authorization.

In `enforce` mode, supported simple `ls`, `rg`, `grep`, and repository Git
commands receive a redirect to the structured Pixel tool. The extension does
not silently replace a native command with a different operation. Pi's in-repository `read` tool uses
Pixel-resolved paths with a limit of at most 200 lines; outside paths are
exempt. The mode can refuse supported repository retrieval with a redirect.
Shell compositions, redirections, interpreters and unknown capabilities
remain intact when the extension cannot classify them reliably. For example,
`cargo test | tail -20`, `cargo test | rg error`, and
`pixel repo-state --json | jq .branch` keep their original shell semantics.
No leaf is removed or executed separately.

The extension checks Pi tool calls, including read, bash, and named discovery
tools. It cannot intercept Pi's own project context loading before an agent
turn, file access performed *inside* an allowed build or test process, or a
tool process that bypasses Pi's `tool_call` event. The policy is a retrieval
workflow preference, not a repository sandbox. Pi's permissions remain
authoritative. The extension's `classify()` is deterministic local code;
it does not invoke the model-based `pixel classify` command.

## Automatic task context and edit feedback

The extension does not wait for the model to choose Pixel. On a non-trivial
prompt, `before_agent_start` runs `scope-task` and `repo-state` and injects
the bounded result as task context; resolved targets also seed the `read`
exception set. If Pixel is unavailable, the injection carries a repair
instruction. Only `enforce` mode requires a successful structured `pixel`
call before editing while Pixel is healthy. When health is unknown or an
operation fails, the gate opens so recovery remains possible. Policy state
is reset for a new session.

After a successful edit or write, `what-changed` inspects the updated working
tree and adds changed symbols, flows and suggested tests after the original
tool result. Failed edits keep their original error text. Impact feedback
reminds the model that edits remain unverified until builds or tests run;
it describes the working-tree changes, not a proof that the latest edit is
correct. Automatic task context and edit feedback remain available with
`pixel config policy off`.

`commit` and `commit_and_push` require explicit user intent in the current
user message, plus named files, a message, and an idempotency request ID.
`commit_and_push` requires intent to push as well. A fetch never grants it.
The extension cannot cryptographically authenticate natural-language intent;
it applies this check at tool execution and reports denials.

Policy decisions are logged in `.pixel/pi-policy.jsonl` with decision kind,
tool, reason, and health or truncation metadata. The log does
not record commands, query strings, file contents, or commit messages. Count
policy entries to inspect which calls received advice or were blocked. This is an operational
trace, not an audit of reads by allowed child processes.

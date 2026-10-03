# Task contracts and trajectory evaluation

Pixel can track a coding task from its acceptance contract to verified
completion. Existing retrieval commands still return bounded evidence. The task
layer records which evidence is current and which obligations remain unmet.

## Repository requirements

Add executable checks to the repository's existing `.pixel/config.yaml` under
`task` (preserve unrelated settings). For example:

```yaml
task:
  enforcement: enforce
  checks:
    - id: tests
      argv: [cargo, test, --workspace]
      cwd: .
      timeout_ms: 300000
      required: true
  conservative_checks: [tests]
  inputs: []
  outputs: [target]
```

`conservative_checks` explicitly names required checks that cover consumers
missing from structural retrieval. An unavailable index or capped impact answer
never proves that no dependencies exist. Without conservative checks, incomplete
preparation remains unresolved. A repository without executable checks remains
unconfigured; the agent can draft a contract without asking for approval of
every task. `task.enforcement: off` is an operator setting for new tasks in
observation mode. Existing enforced tasks keep their obligations when settings
or evaluation policy change.

The contract combines these requirements with the objective and acceptance
criteria, each mapped to one or more check IDs. Unmapped criteria stay unmet.
An initial automatically created `task-acceptance` criterion starts unmapped.
Repository check IDs cannot be replaced by weaker commands in task files.
Adding checks/mappings strengthens a contract; removing obligations requires
an explicit human revision. The interactive CLI waiver requires a terminal and
typing the exact task ID. It is not a security boundary against a program that
controls the user's terminal or configuration.

## Commands

`task` is the short alias for `task-state`. All commands accept `--json`.
Task mutations accept `--request-id` for idempotent retries; keep the ID and
operation unchanged when retrying. `--expected-revision` detects another writer.

```sh
pixel task begin "Fix the parser regression" --provider pi --session SESSION --json
pixel task contract TASK --json
pixel task contract TASK --file contract.json --json
pixel task contract TASK --definition '<contract JSON>' --json
pixel task prepare TASK --json
pixel task verify TASK --check tests --request-id verify-1 --json
pixel task review TASK --json
pixel task finish TASK --json
pixel task status TASK --json
pixel task events TASK --json
pixel task route TASK --json
pixel task replay TASK --json
pixel task cancel TASK --json
pixel task recover TASK --json
```

Contract files may be JSON or YAML. Inline `--definition` accepts JSON and stays
available when the edit gate prevents creating a contract file. It permits
strengthening only; interactive weakening still requires `--file`.
A minimal complete file is:

```json
{
  "version": 1,
  "objective": "Fix the parser regression",
  "checks": [{"id": "tests", "argv": ["cargo", "test", "--workspace"]}],
  "criteria": [{
    "id": "task-acceptance",
    "description": "Fix the parser regression",
    "checks": ["tests"]
  }],
  "conservative_checks": ["tests"],
  "inputs": [],
  "outputs": ["target"],
  "require_preparation": true,
  "require_review": true
}
```

The deterministic review records the change inventory, conflict/whitespace
validation and explicit `--finding` entries. It does not replace acceptance
checks or establish semantic correctness on its own. `finish` reevaluates source,
contract and executable identities; a previous passing status is insufficient.
The `show --session` and `reset --session` commands retain the separate Claude
context-packet interface. Resetting that packet never deletes completion evidence.

## Captured verification

Verification copies tracked, dirty and untracked source into a private workspace,
including modes and supported internal symlinks. Declared ignored inputs are
copied too. Source identity is independent of the temporary directory name.
Declared outputs cannot overlap tracked source or declared inputs. Absolute or
external symlinks, unsupported submodule capture, oversized source and missing
inputs block verification explicitly. Declare required ignored dependencies as
inputs; do not share mutable source with the live checkout.

Receipts contain source/contract/check identities, outcomes, timing, output byte
counts and output hashes. Raw verification output is not persisted in the task
journal. A private check modifying captured source, including an edit followed
by restoration detected by source metadata, cannot yield a passing receipt.
Interrupted checks stay unknown. `recover` never guesses that an interrupted
process passed or blindly retries a possibly running child.

Checks bind the effective child environment by hash without storing its values.
Changes to that environment invalidate prior receipts. The optional `toolchain`
map pins additional executable names or absolute paths to their SHA-256 file
digests; verification and completion recheck those files. Use it for compilers
or other tools launched by a shell/check driver. Git routing variables are
excluded from private Git operations. Git-aware checks retain captured HEAD,
index and dirty worktree context rather than seeing a synthetic clean commit.

The journal is authoritative; the materialized `task.json` is a rebuildable view.
Legacy version-1 task ledgers load as unverified and need a current contract and
evidence. Old `accepted` or `prepared` labels never mean verified completion.

## Host behavior and measurements

Global installation registers Claude and Codex native hooks; the Pi project
extension handles Pi events. Existing foreign hooks and trust settings remain.
Doctor reports registration and observed invocation separately. Supported edit
hooks deny missing prerequisites. The first fallback edit creates a task but
must be retried after configuration/preparation. A bounded latest session prompt
supplies that fallback contract's objective. Read-only questions do not activate
a coding contract merely because they mention code. Read-only work and task control
remain available. Arbitrary external shell processes and hosts bypassing their
own hooks remain outside enforcement; Pixel's completion status is authoritative.

Pi records branch-local bindings and preserves a task's correction budget across
forks. Cancellation wins. At most three corrective continuations are available,
with an earlier stop after two identical unresolved states. Ordinary tasks have
no implicit total turn limit.

`events` reports observed model tool requests, blocked/retried requests, model
responses, coordinator/classifier calls, durations and available token usage.
Duplicated observations of one real host call count once. Native hooks do not
necessarily see every model request or child; incomplete coverage yields a
lower bound and a null complete count. Estimated savings from the existing
action log remain estimates.

`route` may ask the enabled, already warm local classifier to rank eligible
routes; it never starts a model or falls back to a paid endpoint. The 300 ms
budget and decision cache bound that overhead. `replay` evaluates recorded
inputs with the current pure policy, detects divergences, and never executes
tools or infers outcomes after a changed route.

## Controlled evaluation

`pixel task evaluate suite.json --json` is an explicit experiment, never run
automatically by a hook. It uses the existing `eval/` score and gate conventions
with three arms: `retrieval`, `gates`, and `gates_classifier`. See the controlled
backend documentation in `eval/` for the versioned suite format and fixtures.

Trials fix source, checks, host/model configuration, permissions and environment,
use fresh sessions and disposable private containers, and enforce explicit time
and interaction budgets. Held-out verification runs after the agent exits. Exit
zero, a nonempty answer or the agent's claimed success does not establish success.
Only a configured model gateway can provide egress; containers receive no host
home, Docker socket or deployment credentials.

Quality is evaluated before efficiency. The existing 1.5× baseline-turn ceiling
is a regression threshold, not a shortest-path proof. Empirical route regret is
the interaction gap to the best observed matching successful attempt. Missing
comparators or incomplete telemetry yield unknown regret. Report failure rates,
coverage and optimizer overhead beside any interaction improvement.

# usable-git v1 Product Specification

## Summary

`usable-git` gives coding agents a small, structured Git surface instead of requiring them to compose shell command chains. Version 1 provides safe local inspection, review, history, commit diffing, indexed history search with lifecycle answers, scoped commit creation, single-branch push, combined commit-and-push, branch creation and switching, scoped remote-tracking refresh, and fast-forward branch advance through MCP with a JSON CLI fallback.

## Problem

The existing rule asks agents to think in semantic repository operations, but no executable semantic interface exists. Agents still spend tool calls and context on shell commands, parse unstable text output, and can accidentally stage or publish unrelated user work.

## Goals and non-goals

Goals:

- Make the common inspect, review, history, diff, search, publish, push, ship, branch, sync, and update workflows callable as one structured operation each.
- Keep every response byte agent-relevant: no transport ceremony, no repeated metadata, no fingerprint transcription between calls.
- Preserve unrelated repository state under success, failure, cancellation, contention, and recovery.
- Activate the same contracts in Codex, Claude Code, Cursor Agent, and Devin CLI.
- Produce enough local metadata to prove correctness, adoption, operation-count, token, and latency improvements.

Non-goals for v1:

- Replacing every Git command or exposing arbitrary Git argv.
- Direct object writes or an embedded Git implementation.
- Merge/rebase conflict resolution, stash operations, tags, hunk staging, undo/reset, branch deletion, remote configuration mutation, revision expressions, or reading a file at an arbitrary revision.
- Message-only amend (amending without reselecting files) and file-at-revision reads are known cuts deferred to v1.2.
- AI-generated review findings.
- Publishing telemetry or repository content to a remote service.

## Behavior

### Shared contract

1. Consumers receive exactly eleven semantic operations in three categories:
   - Read-only local: `inspect`, `review`, `history`, `diff`, `search`.
   - Mutating: `publish`, `push`, `ship`, `branch`, `update`.
   - Remote-refreshing, locally non-destructive: `sync`.

2. Every operation accepts an absolute `repoPath`. Relative paths, missing paths, non-repositories, and repository paths the caller cannot read return a structured error without changing repository state.

3. Every response uses a versioned `v1` envelope containing `ok`, the request ID when one applies, warnings when any exist, and exactly one of a typed result or a structured error. Operation name, backend, transport, duration, subprocess counts, and repository identity are not repeated on the wire; local telemetry still records them.

4. Stable error codes distinguish invalid input, invalid repository or path, unsupported repository state, stale expectations, busy repository, nothing to commit, hook/signing/identity/authentication failures, non-fast-forward outcomes, lease rejection, ambiguous network outcome, recovery conflict, invariant violation, an already-existing ref, and scoped Git command failure.

5. The MCP and JSON CLI surfaces accept equivalent inputs and return equivalent envelopes. An agent can fall back from MCP to CLI without changing operation semantics.

6. Read operations never fetch, modify files, modify the index, move refs, change configuration, create a stash, or contact a remote.

7. Mutating operations require explicit scope and optimistic expectations. They fail safely when the repository has changed since inspection; they never silently widen scope to make progress.

8. Detached HEAD (except `branch` create), unresolved conflicts, merge/rebase/cherry-pick/revert sequencer state, bare repositories, sparse or split indexes, and requested submodule mutation are refused before mutation begins.

9. Concurrent mutating requests against the same repository share one repository-level exclusion boundary. A second request receives `busy_repository`; it does not race or wait indefinitely.

10. Repeating a mutating request with the same request ID is idempotent. The caller receives the known prior outcome or an explicit ambiguous/recovery error; a second commit or blind second push is never created. Request IDs are optional: when omitted, the service generates an `auto-` prefixed ID and echoes it in the envelope so the caller can still retry idempotently.

### Inspect

11. `inspect` accepts `repoPath` and an optional list of literal file paths relative to the repository root.

12. A successful `inspect` returns one compact local snapshot: repository root, current branch, HEAD object ID, upstream ref with ahead/behind counts when configured, in-progress operation state when present, stash count when nonzero, configured remotes, and one entry per change. Clean or absent fields are omitted rather than reported as empty.

13. Each changed entry carries its repository-relative path, its porcelain v2 `XY` status pair, and the origin path for renames. Per-file fingerprints are not returned on the wire; `inspect` records them server-side and returns one 12-hex `snapshot` token that later mutations present instead.

14. When files are supplied, only those exact literal paths are returned. `.` and directory, glob, pathspec-magic, and repository-escaping selections are rejected rather than expanded. A file-scoped inspect still records the whole-repository snapshot state behind its token so a scoped view cannot masquerade as a full one. Read operations may report ignored or gitlink state, but mutating those entries remains unsupported.

15. An unborn repository is valid inspection input. The result explicitly reports no HEAD instead of treating the repository as invalid.

16. Clean repositories return empty change collections, not an error.

### Review

17. `review` accepts `repoPath`, optional literal files, an optional pagination cursor, and an optional response byte cap.

18. A successful review keeps staged evidence (`HEAD` to index) separate from unstaged evidence (index to working tree), with per-path statistics and binary markers.

19. Review reports source and destination paths for renames and never drops binary or unusual-filename entries merely because text content is unavailable.

20. Untracked file contents are excluded by default. They are included only when the caller explicitly names those exact files.

21. Large results are deterministically paginated. The same repository snapshot, selection, byte cap, and cursor produce the same next page and cursor. Cursors are short opaque server-held handles, not state-bearing blobs.

22. A cursor tied to a stale or expired server-held page state fails with `stale_state` and instructs the caller to restart pagination; pages from different repository states are never combined silently. Corrupted or cross-operation cursors fail as invalid input.

23. Review returns repository evidence only. It does not invent findings, assign severity, or interpret code quality.

### History

24. `history` accepts `repoPath`, a local ref defaulting to `HEAD`, a limit defaulting to 20 and capped at 100, a `detail` level defaulting to `compact`, and an optional cursor.

25. A successful compact history result is newest-first and includes each commit's abbreviated 12-hex object ID, subject line, author name, commit timestamp, and a merge marker for merge commits. `detail: "full"` restores the forensic shape: full object ID, parent IDs, author and committer identities, complete message, both timestamps, and signature status.

26. History resolves only local refs and objects. Missing or invalid refs return a structured error; the operation never fetches them.

27. Pagination is deterministic and stale cursors fail explicitly.

28. An unborn repository returns an empty history result with explicit unborn-HEAD state.

### Diff

29. `diff` accepts `repoPath` and one target: either two exact object IDs (`{kind: "range", baseOid, targetOid}`) or one commit compared against its first parent, or against the empty tree for a root commit (`{kind: "commit", oid}`). Optional literal files, a cursor, and a byte cap scope the response.

30. Targets are exact object IDs only, abbreviated to at least 12 hex characters. Ref names and revision expressions are rejected; agents obtain object IDs from `inspect`, `history`, or `sync` first.

31. A successful diff mirrors review evidence per path — patch text, binary marker, addition/deletion counts, truncation flag, rename origin — without a staged/unstaged scope, plus the fully resolved base and target object IDs.

32. Diff uses the same deterministic byte-capped pagination and cursor rules as review.

33. Diff is read-only: it never fetches, mutates, or resolves objects it does not already have locally.

### Publish

34. `publish` accepts `repoPath`, a non-empty exact file list, an optional commit message, a mode (`append` by default, or `amend`), an optional request ID, and exactly one of a `snapshot` token from `inspect` or an explicit `expected` state containing expected HEAD (including an explicit unborn value) and a fingerprint for every selected change. Append mode requires a message.

35. Publish commits the complete current contents or deletion of each selected file as one local commit. It does not push.

36. Publish never stages, unstages, commits, edits, deletes, or otherwise changes an unrelated file. Existing unrelated staged entries remain staged and unrelated unstaged/untracked entries remain unchanged.

37. New selected files can be committed, including in an unborn repository, without pulling unrelated staged entries into the commit.

38. Selected paths must be literal repository-relative files. Empty selections, `.`, directories, globs, pathspec magic, ignored files, gitlinks, duplicates, and paths outside the repository are rejected.

39. Publish resolves its expectations — from the snapshot token's server-side record or from the explicit `expected` block — and checks expected HEAD and all selected fingerprints immediately before mutation. Any mismatch, including an unknown or expired snapshot token, returns `stale_state` and creates no commit.

40. Existing Git commit hooks, author identity, signing configuration, and commit-message validation are honored. A hook, identity, or signing failure is returned with a stable error code.

41. If publish fails before Git reports a new commit, the original index is restored only when safe to do so. If another actor changed the index during the operation, publish stops with `recovery_conflict` and does not overwrite that work.

42. Once a new commit is observed, publish never resets or rewrites HEAD as rollback. The result reports the observed commit and any recovery warning.

43. A successful publish returns the new commit ID, committed paths, resulting HEAD/branch state, and enough status metadata to prove unrelated work was preserved. Amend results additionally report the replaced commit ID.

44. A selection with no committable difference returns `nothing_to_commit`; no empty commit is created.

45. Amend mode requires an existing commit — an unborn repository is refused. When the message is omitted, the amended commit reuses the current tip's message. Parents are preserved; amend never rewrites more than the tip. When the amended tip already exists on the configured upstream, the result carries an explicit warning because a following push will require a lease.

### Push

46. `push` accepts `repoPath`, a configured remote name, full source and target branch refs, an optional request ID, expected source object ID, and an explicit mode.

47. Push updates exactly one remote branch. It rejects raw remote URLs, implicit upstreams, short/ambiguous refs, tags, deletes, wildcard or multi-ref refspecs, and unconfigured remotes.

48. Fast-forward mode never forces. A non-fast-forward result returns `non_fast_forward` without retrying as force.

49. Lease mode requires the exact expected target object ID and uses force-with-lease semantics. Blind force and lease values inferred after the request begins are prohibited.

50. Push verifies that the source ref still resolves to the expected source object ID before contacting the remote. A mismatch returns `stale_state`.

51. Authentication, authorization, connection, and server rejection failures are differentiated when Git provides enough evidence.

52. When the connection fails after the remote may have accepted the update, push queries only the explicitly named target ref. It returns confirmed success, confirmed failure, or `network_ambiguous`; it never blindly retries. The ambiguity error carries the effective request ID so the caller can retry idempotently.

53. A successful push returns remote name, source and target refs, old target object ID when known, new target object ID, and push mode.

### Ship

54. `ship` combines publish and push in one call. It accepts the publish request shape — files, message, request ID, and exactly one of `snapshot` or `expected` — plus a configured `remote`, an optional target ref defaulting to the current branch, and an optional push mode defaulting to fast-forward.

55. Ship derives the push source ref and expected source object ID from the freshly created commit server-side. The caller never transcribes an object ID between the commit and the push.

56. A failure in the commit leg returns `ok: false` with that publish error; no push is attempted.

57. A failure in the push leg after a successful commit returns `ok: true`: the result reports the commit and carries `push.ok: false` with the push error code and retry guidance. A commit that exists is never reported as a top-level operation error.

58. A fully successful ship reports the commit ID, branch, committed paths, and the confirmed remote ref update.

### Branch

59. `branch` accepts `repoPath`, an optional request ID, expected HEAD, and one mode: `create` or `switch`, each with a short branch name validated by Git ref rules.

60. Create always branches at the current HEAD and switches to the new branch. Creating from a detached HEAD is allowed. A name that already exists returns `ref_exists`; nothing is moved or reused.

61. Switch refuses to carry any uncommitted tracked change across branches: it returns `unsupported_state` with the dirty paths instead of relying on Git's implicit merge-on-checkout behavior.

62. Branch acquires the repository mutation lock and journals its request like push, so a repeated request ID returns the known outcome.

63. A successful result reports the branch name, its object ID, the previous branch when one existed, and whether the branch was created.

### Sync

64. `sync` is the only v1 operation in the remote-refreshing, locally non-destructive category. It accepts `repoPath`, one configured remote, and an optional list of up to 16 branch names, defaulting to the current branch and its upstream.

65. Sync fetches exactly the named branches through explicit refspecs (`+refs/heads/<branch>:refs/remotes/<remote>/<branch>`) with tags disabled and never prunes. It writes only `refs/remotes/`; the working tree, index, local branches, HEAD, and tags are never touched.

66. A branch absent on the remote is a successful result with a null new object ID, not an error.

67. A fetch failure returns a retryable scoped Git failure. `network_ambiguous` remains reserved for push, where an unacknowledged update can exist remotely.

68. Sync writes no journal and takes no repository mutation lock. Fetching is idempotent and touches no state the lock protects; holding the lock through a slow network fetch would block publish for zero added protection.

69. A successful sync reports each fetched branch's old and new object IDs and whether it moved, plus refreshed ahead/behind counts for the current branch when it tracks the synced remote.

### Update

70. `update` fast-forward-advances the current branch to an exact target object ID the caller observed via `sync` — the local mirror of push's lease design. It accepts `repoPath`, an optional request ID, expected HEAD as an exact object ID, and the target object ID.

71. A current HEAD that no longer matches the expectation returns `stale_state` before any mutation.

72. A target that is not a descendant of HEAD returns `non_fast_forward` with the merge base; divergence is resolved outside usable-git, never by this operation.

73. Before mutating, update compares incoming file changes against dirty working-tree paths. Any overlap returns `unsupported_state` with the conflicting paths; the working tree is never overwritten.

74. Update acquires the repository mutation lock and journals like push. A successful result reports the branch, previous and new object IDs, and the number of commits advanced.

### Search

(Clause numbering is append-only; the search clauses were added after 87.)

88. `search` answers one history question in one call: `repoPath` plus a discriminated target — `{kind: "text", query, scope}` over commit messages, file paths, and diff text (scope `message`, `path`, `diff`, or `all`), or `{kind: "lifecycle", path | token}` with exactly one of `path` or `token`. Optional `limit` (max 50, default 10), `byteCap` (default 32,000), and cursor bound the response.

89. The index is a local SQLite FTS5 database derived entirely from repository data, keyed by the Git common directory, and built lazily inside each call under a fixed time budget: commit metadata and messages first, diff text second. Remaining work is reported as proof — `index.state: "partial"` with pending counts — never as an error, and repeated calls converge to `fresh`.

90. Text hits are ranked across scopes (message above path above diff text) and returned compact: a 12-hex object ID that the existing `diff` operation accepts verbatim, committer date, subject, author, match kind, optional path and `«match»`-wrapped snippet, and the touched-file count.

91. Lifecycle answers "was X dropped": `firstSeen`, `lastChanged`, `removedIn`, `presentAtHead` (verified against HEAD via Git, not the index), and total touches, plus up to five ranked hits as citations.

92. Raw query text never reaches the FTS engine: queries are sanitized into quoted units, so no input can produce a syntax error or operator injection. A query with no searchable characters is `invalid_input`.

93. Search is read-only and closed-world: it never fetches, and it indexes local branch heads plus HEAD only (tags and remote-tracking refs are a documented v1 exclusion). Merge commits, binary files, lockfiles, and generated artifacts are indexed as metadata with their diff text recorded as skipped, never silently dropped. Pagination cursors bind to the indexed tips and go `stale_state` when new history lands between pages.

### Installation, diagnostics, and routing

75. Homebrew is the only v1 distribution channel. The supported install identity is `brew install liviogama/tap/usable-git`.

76. `usable-git install --clients all` registers the same local stdio MCP server for Codex, Claude Code, Cursor Agent, and Devin CLI while preserving every unrelated client configuration entry.

77. Installation is repeatable. Matching existing entries are left valid; conflicting entries fail with a clear explanation unless the caller explicitly supplies `--force`.

78. `usable-git doctor --clients all` checks required runtimes, CLI operation behavior, MCP initialize/list/call behavior, exact tool schemas, client registration, temporary-repository publish and local-bare-remote push, and fresh-session client invocation.

79. Doctor reports each check as pass, fail, or skipped with a reason. It exits non-zero when a required check fails and never reports a client as activated without invoking it in a fresh session.

80. Agent routing prefers MCP, uses the JSON CLI only when MCP is unavailable, and falls back to scoped raw Git only for operations outside the v1 surface. It never falls back from a rejected v1 mutation to a broader raw command that bypasses the rejection.

### Privacy, measurement, and release

81. Telemetry is local and disabled by default. Enabling it is explicit and reversible.

82. Enabled semantic telemetry may retain operation, client, transport, backend, duration, Git subprocess count, result/error code, aggregate counts, component versions, and a salted repository hash. These fields are recorded locally even though the wire envelope no longer repeats them.

83. Semantic telemetry never retains prompts, reasoning, patches, file contents, file names, raw paths, secrets, or command output.

84. Migrating legacy mining data writes a new redacted database and does not delete or overwrite the original database automatically.

85. Reports distinguish semantic adoption, raw fallback, repeated-read elimination, correctness, operation count, estimated Git-related tokens, latency, and client/version distribution.

86. A v1 release is blocked until automated evidence shows: 100% repository correctness and recovery, zero unrelated-work loss/corruption, 100% clean-install activation across Codex, Claude Code, and Devin CLI, at least 95% semantic-tool adoption when applicable, at least 50% fewer agent-facing Git operations, and at least 30% lower Git-related tokens and p95 end-to-end time. The local benchmark policy is three clients with 40 paired trials per scenario/client; Cursor Agent is not required for the v1 gate.

87. Public performance claims cite reproducible raw benchmark artifacts, trial counts, environment/runtime/Git/client versions, commit SHA, median, p95, confidence intervals, and final-state oracles. Historical prototype numbers without those artifacts are labeled historical and unverified.

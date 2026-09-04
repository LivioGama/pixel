//! `pixel hook guard` — mechanical enforcement of the sniper-targets
//! contract, ported from the original working `gitpixel-targets-guard`
//! Python hook (kept as `~/.claude/hooks/gitpixel-targets-guard.pixel-bak.*`
//! on this machine). The hook provides transparent read-only rewrites and
//! non-blocking guidance through the PreToolUse protocol.
//!
//! Contract:
//! 1. SCOPING (ADVISORY) — while `<repo>/.pixel/targets.json` is active
//!    (younger than 24h), reads/greps/edits of repo files OUTSIDE the
//!    target list emit a NON-BLOCKING advisory note and proceed. The
//!    sniper-discovery benchmark (docs/bench/sniper-discovery.md) showed
//!    hard blocking collapses recall (0.60 → 0.19), so the fence advises
//!    instead of denying.
//! 2. MANDATE (ADVISORY) — in a pixel-indexed repo (a `.pixel` dir exists)
//!    with NO active manifest, edits to *existing* files get an advisory
//!    suggesting `pixel targets "<task>"` first; the edit proceeds. An
//!    EXPIRED manifest (>24h) gets an expiry advisory instead of a block.
//! 3. SAFETY (ADVISORY) — destructive git commands get a pixel alternative:
//!    `git reset --hard/--keep`, raw historical file restores
//!    (`git checkout <ref> -- <path>`, `git restore --source`), `git clean -f*`,
//!    `git checkout -f/--force`, `git stash drop/clear`, `git branch -D`, and
//!    `git push --force`. The original command is still allowed to proceed.
//! 4. SUBSTITUTE (ADVISORY) — plain git mutations with an exact pixel
//!    equivalent get the substitute spelled out: `git add` → `pixel publish`,
//!    `git commit` → `pixel publish`, `git push` → `pixel push`,
//!    `git checkout -b`/`git switch -c` → `pixel branch`, and `git rebase` →
//!    `pixel reconcile`. Pixel cannot safely rewrite these because they are
//!    writes, so the original command remains available. Interactive/porcelain
//!    shapes pixel can't cover pass through — see `git_substitute_deny` for
//!    the documented table.
//!    Transcript-store pokes (sqlite3/cat/grep on a known store) get the same
//!    advisory with a `pixel recall` alternative.
//! 5. GLOB — Glob tool calls are deliberately left un-denied: they only
//!    enumerate paths, and the Read/Edit of any result is itself guarded by
//!    the scoping rules above. Blocking enumeration would be pure noise.
//!
//! Rewrites NEVER change command semantics beyond read-only enrichment: a
//! rewrite must never add a write, push, or destructive step the original
//! command didn't have.
//!
//! The hook never blocks ordinary work: advisories exit 0 with a JSON note
//! (systemMessage + additionalContext), no permissionDecision, and transparent
//! read-only rewrites use `updatedInput`. The normal permission flow is
//! untouched.
//! Fails open (exit 0) on any parse error or unexpected shape — a guard
//! that crashes or wedges the session is worse than a guard that misses a
//! case.

use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

const MANIFEST_MAX_AGE_SECS: u64 = 24 * 3600;
const ORIENTATION_ANY: &[&str] = &["CLAUDE.md", "AGENTS.md", "README.md"];
const ORIENTATION_ROOT: &[&str] = &[
    "package.json",
    "Cargo.toml",
    "go.mod",
    "pyproject.toml",
    "tsconfig.json",
    ".gitignore",
];
const READERS: &[&str] = &[
    "cat", "head", "tail", "less", "more", "bat", "nl", "sed", "awk", "rg", "grep", "egrep",
    "fgrep", "find", "strings", "wc",
];

/// Normalize a binary name by stripping any directory path component and
/// known shell wrapper prefixes. This prevents path-prefix evasion attacks
/// where an agent invokes `/usr/bin/grep` or `command grep` to bypass the
/// guard that checks for bare `grep`.
///
/// Examples:
/// - `/usr/bin/grep`  → `grep`
/// - `/bin/cat`       → `cat`
/// - `command grep`   → `grep` (via token[1] promotion in callers)
/// - `builtin grep`   → `grep`
/// - `rtk grep`       → `grep` (rtk is a pass-through wrapper)
///
/// This only strips the path component — callers handle multi-token
/// wrappers (`command`, `builtin`, `rtk`) by removing the wrapper token
/// and calling `normalize_bin` on the next token.
fn normalize_bin(s: &str) -> &str {
    // Strip any directory prefix: /usr/bin/grep → grep
    if let Some(pos) = s.rfind('/') {
        return &s[pos + 1..];
    }
    s
}

/// Shell binaries that accept a `-c`-style flag followed by a script string.
const SHELL_WRAPPERS: &[&str] = &["sh", "bash", "zsh", "dash", "ksh", "fish"];

/// Strip one layer of matching outer quotes.
fn strip_outer_quotes(s: &str) -> &str {
    let b = s.as_bytes();
    if b.len() >= 2
        && ((b[0] == b'"' && b[b.len() - 1] == b'"')
            || (b[0] == b'\'' && b[b.len() - 1] == b'\''))
    {
        return &s[1..s.len() - 1];
    }
    s
}

/// Unwrap a `bash -lc "<script>"` invocation down to `<script>`.
///
/// Without this, `bash -lc "grep -rn foo ."` reads as the binary `bash` and
/// the guard never sees the `grep` inside — a wrapper-evasion hole that
/// affected every harness, not just Codex. It matters most for Codex, which
/// routinely wraps every command as `["bash","-lc", ...]`.
fn unwrap_shell_c(cmd: &str) -> String {
    let trimmed = cmd.trim();
    let mut tokens = trimmed.split_whitespace();
    let Some(first) = tokens.next() else {
        return trimmed.to_string();
    };
    if !SHELL_WRAPPERS.contains(&normalize_bin(first)) {
        return trimmed.to_string();
    }
    let mut cursor = first.len();
    for tok in tokens {
        let Some(idx) = trimmed[cursor..].find(tok) else {
            break;
        };
        cursor += idx + tok.len();
        if !tok.starts_with('-') {
            // A non-flag token before any -c: not a `-c` invocation.
            break;
        }
        // `-c`, `-lc`, `-ic`, `-lic` … all mark the next token as the script.
        if tok.contains('c') {
            let rest = strip_outer_quotes(trimmed[cursor..].trim()).trim();
            return rest.to_string();
        }
    }
    trimmed.to_string()
}

/// Extract a shell command from a tool-input value that may be either a
/// plain string or an array of argv tokens, unwrapping any `sh -c` wrapper.
///
/// Claude/Gemini/Antigravity pass `command` as a string. Codex's `shell`
/// and `unified_exec` tools pass an argv array instead — Codex documents the
/// field as "a string or array of strings". Without the array arm, a payload
/// like `{"command":["bash","-lc","grep -rn foo ."]}` reads as an empty
/// string and the guard silently no-ops, letting raw grep through.
fn command_text(v: &Value) -> String {
    match v {
        Value::String(s) => unwrap_shell_c(s),
        Value::Array(items) => {
            let parts: Vec<&str> = items.iter().filter_map(Value::as_str).collect();
            if parts.is_empty() {
                return String::new();
            }
            // argv form: ["bash", "-lc", "<script>"] → "<script>"
            if SHELL_WRAPPERS.contains(&normalize_bin(parts[0])) {
                if let Some(pos) = parts
                    .iter()
                    .position(|t| t.starts_with('-') && t.contains('c'))
                {
                    if let Some(script) = parts.get(pos + 1) {
                        return unwrap_shell_c(script);
                    }
                }
            }
            unwrap_shell_c(&parts.join(" "))
        }
        _ => String::new(),
    }
}

/// On-disk transcript stores `pixel recall` already ingests into one
/// queryable corpus (`pixel recall search`/`sessions`/`index`). A raw
/// sqlite3/python/cat/grep session digging through one of these by hand —
/// exactly what happened before this advisory existed, recovering a
/// quota-blocked Devin session's task via manual `sqlite3` + `python3`
/// archaeology — is real, non-destructive work that should not be hard
/// blocked (same recall-regression lesson as the sniper fence:
/// docs/bench/sniper-discovery.md), but deserves a pointer to the
/// deterministic replacement.
const TRANSCRIPT_STORE_MARKERS: &[&str] = &[
    ".local/share/devin/cli/sessions.db",
    ".local/share/devin/cli/transcripts",
    // NB: `.config/devin` is NOT a transcript store — it holds config.json,
    // mcp_config.json and skills/, i.e. the very files `pixel install` writes.
    // Listing it here made the guard deny reads of pixel's own installed
    // Devin hook config and point the agent at `pixel recall` instead, which
    // cannot answer a config question. The real store is the
    // `.local/share/devin/cli/...` pair above.
    ".claude/projects",
    ".cursor/chats",
    ".codex/sessions",
    ".gemini/tmp",
    ".local/share/opencode",
    ".zcode/cli/db",
];
/// Tools capable of digging through a transcript store's raw records
/// (queries a sqlite DB, or runs a script over JSON/JSONL). Deliberately
/// narrower than `READERS`: a bare `cat`/`grep` on a transcript path is
/// still flagged via `READERS` below, but `python3`/`node`/`jq` only count
/// as archaeology when paired with a known store path — otherwise every
/// unrelated script invocation would be flagged.
const ARCHAEOLOGY_TOOLS: &[&str] = &["sqlite3", "python3", "python ", "node ", "jq "];

/// The transcript-store marker `cmd` touches with a tool capable of
/// reading it, or `None` when the command doesn't match — the common
/// case, checked first for speed.
fn transcript_store_hit(cmd: &str) -> Option<&'static str> {
    let store = TRANSCRIPT_STORE_MARKERS.iter().find(|m| cmd.contains(**m))?;
    let digs_in = ARCHAEOLOGY_TOOLS.iter().any(|t| cmd.contains(t))
        || READERS.iter().any(|r| cmd.contains(r));
    if digs_in {
        Some(store)
    } else {
        None
    }
}

/// Advisory (non-blocking) lines for a transcript-store poke.
fn transcript_archaeology_advisory_lines(store: &str) -> Vec<String> {
    vec![
        format!("Advisory: this command reads `{store}` — a transcript store `pixel recall` already indexes."),
        "`pixel recall sessions --agent <devin|codex|claude|cursor|gemini|opencode|zcode>` lists sessions by title/cwd/turn-count in one call.".into(),
        "`pixel recall search \"<phrase>\" --agent <agent> --session <name>` pulls the exact turn text — no manual sqlite3/python needed.".into(),
        "Run `pixel recall index` first if this store hasn't been ingested yet.".into(),
    ]
}

/// One scoped task inside the manifest. v2 manifests carry several of
/// these (concurrent agents each scope their own task); the legacy v1
/// shape maps to exactly one.
struct TaskEntry {
    task: String,
    files: Vec<(String, String)>, // (path, tier)
}

struct Manifest {
    root: PathBuf,
    tasks: Vec<TaskEntry>,
}

/// Entry point for `pixel hook guard`. Reads the PreToolUse hook payload
/// from stdin. Never returns an `Err` that would surface as exit 1 — every
/// failure path is a deliberate exit 0 (allow, optionally with advice).
pub fn run() -> ! {
    if let Ok(kill) = std::env::var("PIXEL_TARGETS_GUARD") {
        if matches!(kill.as_str(), "0" | "false" | "off") {
            std::process::exit(0);
        }
    }

    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() || input.trim().is_empty() {
        std::process::exit(0);
    }
    let Ok(payload) = serde_json::from_str::<Value>(&input) else {
        std::process::exit(0);
    };
    if !payload.is_object() {
        std::process::exit(0);
    }

    let event = payload
        .get("hook_event_name")
        .and_then(Value::as_str)
        .unwrap_or("");
    if !is_guard_event(&payload, event) {
        std::process::exit(0);
    }

    let tool = payload.get("tool_name").and_then(Value::as_str).unwrap_or("");
    let cwd = payload
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let tool_input = payload.get("tool_input").cloned().unwrap_or(Value::Null);
    let Some(tool_input) = tool_input.as_object() else {
        std::process::exit(0);
    };

    let raw_path = tool_input
        .get("file_path")
        .or_else(|| tool_input.get("path"))
        // Antigravity: view_file uses AbsolutePath; replace_file_content/write_to_file use TargetFile
        .or_else(|| tool_input.get("AbsolutePath"))
        .or_else(|| tool_input.get("TargetFile"))
        // Cursor composer tools: target_file, filePath
        .or_else(|| tool_input.get("target_file"))
        .or_else(|| tool_input.get("filePath"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let anchor = resolve(raw_path, &cwd).unwrap_or_else(|| canonical(&cwd));

    let idx_root = find_up(&anchor, ".pixel");
    let manifest_root = find_up(&anchor, &Path::new(".pixel").join("targets.json"));
    let (manifest, manifest_expired) = match manifest_root.as_deref().map(load_manifest_state) {
        Some(ManifestState::Active(m)) => (Some(m), false),
        Some(ManifestState::Expired) => (None, true),
        _ => (None, false),
    };

    if tool == "Bash" || tool == "exec" || tool == "bash" || tool == "run_shell_command"
        || tool == "execute" || tool == "Shell"
        // Antigravity's bash tool
        || tool == "run_command"
        // Codex's real shell tools. `shell` and `unified_exec` are the names
        // Codex 0.146.1 actually emits; without them a Codex session runs
        // `grep`/`rg`/`find` completely unguarded. `local_shell` is the
        // OpenAI Responses API tool type for the same capability.
        || tool == "shell" || tool == "unified_exec" || tool == "local_shell"
    {
        let cmd_value = tool_input
            .get("command")
            // Antigravity: run_command uses "CommandLine"
            .or_else(|| tool_input.get("CommandLine"))
            // Some harnesses use "cmd"
            .or_else(|| tool_input.get("cmd"))
            // Codex unified_exec passes argv under "input"
            .or_else(|| tool_input.get("input"))
            .cloned()
            .unwrap_or(Value::Null);
        // Codex passes argv as an array; everyone else passes a string.
        let cmd_owned = command_text(&cmd_value);
        let cmd = cmd_owned.as_str();
        // SAFETY TIER FIRST: destructive git + git substitute + transcript store
        // advisories. These run before rewrite attempts so a safe read-only
        // rewrite never hides a more important mutation warning.
        if let Some(lines) = bash_deny_lines(cmd, idx_root.as_deref()) {
            advise(&non_blocking_advisory_lines(&lines));
        }
        if let Some(lines) = git_mutation_substitute_lines(cmd, idx_root.as_deref(), &cwd) {
            advise(&non_blocking_advisory_lines(&lines));
        }
        if let Some(store) = transcript_store_hit(cmd) {
            advise(&transcript_archaeology_advisory_lines(store));
        }
        // REWRITE TIER: try transparent bash → pixel rewrite BEFORE any advisory.
        // Advisories (scoping, transcript) call advise() which exits, precluding
        // the rewrite. By checking rewrite first, we ensure the rewrite takes
        // priority over the advisory — the rewrite IS the resolution.
        if idx_root.is_some() {
            if let Some(rewritten) = try_rewrite_bash(cmd, &cwd) {
                // Read-only search rewrites are semantically equivalent, so
                // transparently replace the input and let the normal tool
                // permission flow continue.
                allow_rewrite(&rewritten);
            }
        }

        // ADVISORY TIER (only if no rewrite applied): scoping advisory
        check_bash_advisories(cmd, &cwd, idx_root.as_deref(), manifest.as_ref());
        std::process::exit(0);
    }

    match tool {
        "Read" | "Grep" | "Glob"
        | "read" | "grep" | "find_file_by_name" | "glob" | "notebook_read"
        | "read_file" | "search" | "find" | "ls"
        // Antigravity: view_file (read), grep_search (grep), find_by_name (find), list_dir (ls)
        | "view_file" | "grep_search" | "find_by_name" | "list_dir"
        // Cursor composer: file_search
        | "file_search" => {
            // In indexed repos, recommend pixel search for Grep tool calls.
            // The hook cannot change the tool type (Grep→Bash), so this is an
            // advisory and the original Grep call proceeds.
            if idx_root.is_some() && is_grep_tool(tool, &tool_input) {
                let pattern = tool_input
                    .get("pattern")
                    .and_then(Value::as_str)
                    .or_else(|| tool_input.get("query").and_then(Value::as_str))
                    // Antigravity grep_search / Cursor file_search use "Query"
                    .or_else(|| tool_input.get("Query").and_then(Value::as_str))
                    .unwrap_or("");
                if !pattern.is_empty() {
                    advise(&grep_redirect_advisory_lines(&pattern, &cwd, &tool_input));
                }
            }
            // RETRIEVAL ADVISORY — in an indexed repo with NO active manifest,
            // suggest `pixel targets` first while allowing retrieval to proceed.
            // Read is allowed through (reading a known file is not retrieval),
            // but gets an advisory in indexed repos with no manifest if the
            // file is a source file — suggesting `pixel targets` first.
            // `PIXEL_GUARD_RETRIEVAL=0` disables this tier.
            if idx_root.is_some() && manifest.is_none() && !manifest_expired && is_retrieval_tool(tool) {
                if !env_flag_off("PIXEL_GUARD_RETRIEVAL") {
                    retrieval_guard_advisory(&cwd, idx_root.as_deref().unwrap());
                }
            }
            // Retrieval-first advisory for Read of source files: in an indexed
            // repo with no active manifest, suggest `pixel targets` before
            // reading source files. Advisory only — the read proceeds. This
            // catches the "massive token waste via redundant reads" failure
            // mode where agents read entire files instead of using pixel search.
            if idx_root.is_some() && manifest.is_none() && !manifest_expired && is_read_tool(tool) {
                if let Some(p) = resolve(raw_path, &cwd) {
                    if p.is_file() && is_source_file(&p) && !is_exempt(&p, idx_root.as_deref().unwrap()) {
                        if !env_flag_off("PIXEL_GUARD_READ") {
                            read_scoping_advisory(&p, idx_root.as_deref().unwrap());
                        }
                    }
                }
            }
            if let Some(m) = &manifest {
                let p = resolve(raw_path, &cwd).unwrap_or_else(|| canonical(&cwd));
                if !allowed(&p, m) {
                    scoping_advisory(&p, m);
                }
            }
        }
        "Edit" | "MultiEdit" | "NotebookEdit" | "Write"
        | "edit" | "write" | "notebook_edit"
        | "apply_patch" | "write_file"
        // Antigravity: replace_file_content (edit), write_to_file (write), edit_file (edit)
        | "replace_file_content" | "write_to_file" | "edit_file" => {
            let Some(p) = resolve(raw_path, &cwd) else {
                std::process::exit(0);
            };
            let exists = p.is_file();
            // write_to_file / Write / write / write_file create new files — always allowed
            if (tool == "Write" || tool == "write" || tool == "write_file" || tool == "write_to_file") && !exists {
                std::process::exit(0); // creating a new file is always allowed
            }
            if let Some(m) = &manifest {
                if exists && !allowed(&p, m) {
                    scoping_advisory(&p, m);
                }
                std::process::exit(0);
            }
            // MANDATE ADVISORY — indexed repo, no active manifest: suggest
            // `pixel targets` before edits to existing files, but proceed.
            if let Some(root) = &idx_root {
                if exists && !is_exempt(&p, root) {
                    if manifest_expired {
                        expired_manifest_advisory(root);
                    }
                    if env_flag_off("PIXEL_GUARD_EDIT") {
                        mandate_advisory(&p, root);
                    } else {
                        edit_guard_advisory(&p, root);
                    }
                }
            } else if exists {
                // Unindexed directory: suggest indexing so pixel's scoped
                // retrieval works. Advisory only — the edit proceeds.
                // Pixel works in ANY directory, not just git repos — the
                // index is a `.pixel/` dir, independent of `.git/`.
                if let Some(git_root) = find_up(&anchor, ".git") {
                    suggest_index_advisory(&git_root, true);
                } else {
                    // Non-git directory: still suggest indexing.
                    suggest_index_advisory(&canonical(&cwd), false);
                }
            }
        }
        _ => {}
    }
    std::process::exit(0);
}

/// Accept Claude Code's/Codex's/Devin's/zcode's `PreToolUse`, Gemini's
/// `BeforeTool`, and Cursor's `preToolUse` hook events. Cursor's payload
/// carries no `hook_event_name` field at all (verified against the
/// installed `cursor-agent` bundle: the `preToolUse` handler builds its
/// hook-script stdin from exactly `{conversation_id, generation_id, model,
/// tool_name, tool_input, tool_use_id, cwd}` — no event-name key) because
/// pixel is only ever wired into Cursor's `preToolUse` array, so the event
/// is already implicit from which array invoked us. Treat the payload
/// shape itself (`tool_name` + `tool_input` present, no explicit event
/// name) as an implicit PreToolUse.
fn is_guard_event(payload: &Value, event: &str) -> bool {
    if event == "PreToolUse" || event == "BeforeTool" {
        return true;
    }
    event.is_empty() && payload.get("tool_name").is_some() && payload.get("tool_input").is_some()
}

fn canonical(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

fn resolve(raw: &str, base: &Path) -> Option<PathBuf> {
    if raw.is_empty() {
        return None;
    }
    let p = Path::new(raw);
    let joined = if p.is_absolute() { p.to_path_buf() } else { base.join(p) };
    Some(std::fs::canonicalize(&joined).unwrap_or(joined))
}

/// Walk upward from `start` (or its parent, if `start` is a file) looking
/// for `rel` (a file or directory). Returns the directory containing it.
fn find_up(start: &Path, rel: impl AsRef<Path>) -> Option<PathBuf> {
    let rel = rel.as_ref();
    let mut dir = if start.is_file() {
        start.parent()?.to_path_buf()
    } else {
        start.to_path_buf()
    };
    loop {
        if dir.join(rel).exists() {
            return Some(dir);
        }
        match dir.parent() {
            Some(parent) if parent != dir => dir = parent.to_path_buf(),
            _ => return None,
        }
    }
}

/// Outcome of reading `.pixel/targets.json`: distinguishes "no usable
/// manifest because everything hit the 24h TTL" (worth an advisory note)
/// from "no manifest at all / unreadable" (silent).
enum ManifestState {
    Absent,
    Expired,
    Active(Manifest),
}

/// Read the enforcement manifest, accepting BOTH shapes:
/// - v2 (multi-task): `{version: 2, tasks: [{id, task, created_unix, targets: [...]}]}`
/// - legacy (v1/singleton): `{task, created_unix, files: [...]}`
/// Expired tasks (older than the 24h TTL) are dropped individually; a
/// manifest whose tasks have all expired reports `Expired`.
fn load_manifest_state(root: &Path) -> ManifestState {
    let Ok(text) = std::fs::read_to_string(root.join(".pixel").join("targets.json")) else {
        return ManifestState::Absent;
    };
    let Ok(m) = serde_json::from_str::<Value>(&text) else {
        return ManifestState::Absent;
    };
    let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH) else {
        return ManifestState::Absent;
    };
    let now = now.as_secs();
    let mut saw_expired = false;
    let tasks: Vec<TaskEntry> = if m.get("version").and_then(Value::as_u64) == Some(2) {
        let Some(raw_tasks) = m.get("tasks").and_then(Value::as_array) else {
            return ManifestState::Absent;
        };
        raw_tasks
            .iter()
            .filter(|t| {
                let created = t.get("created_unix").and_then(Value::as_u64).unwrap_or(0);
                let fresh = now.saturating_sub(created) <= MANIFEST_MAX_AGE_SECS;
                if !fresh {
                    saw_expired = true;
                }
                fresh
            })
            .filter_map(|t| {
                Some(TaskEntry {
                    task: t.get("task").and_then(Value::as_str).unwrap_or("?").to_string(),
                    files: parse_manifest_files(t.get("targets")?.as_array()?),
                })
            })
            .collect()
    } else {
        let created_unix = m.get("created_unix").and_then(Value::as_u64).unwrap_or(0);
        if now.saturating_sub(created_unix) > MANIFEST_MAX_AGE_SECS {
            return ManifestState::Expired;
        }
        let Some(files) = m.get("files").and_then(Value::as_array) else {
            return ManifestState::Absent;
        };
        vec![TaskEntry {
            task: m.get("task").and_then(Value::as_str).unwrap_or("?").to_string(),
            files: parse_manifest_files(files),
        }]
    };
    if tasks.is_empty() {
        return if saw_expired {
            ManifestState::Expired
        } else {
            ManifestState::Absent
        };
    }
    ManifestState::Active(Manifest { root: root.to_path_buf(), tasks })
}

/// Compatibility shim over `load_manifest_state` for tests that only care
/// about an active manifest.
#[cfg(test)]
fn load_manifest(root: &Path) -> Option<Manifest> {
    match load_manifest_state(root) {
        ManifestState::Active(m) => Some(m),
        _ => None,
    }
}

fn parse_manifest_files(raw: &[Value]) -> Vec<(String, String)> {
    raw.iter()
        .filter_map(|f| {
            let path = f.get("path")?.as_str()?.to_string();
            let tier = f.get("tier").and_then(Value::as_str).unwrap_or("").to_string();
            Some((path, tier))
        })
        .collect()
}

/// Iterator over every (path, tier) across all active tasks.
fn all_files(m: &Manifest) -> impl Iterator<Item = &(String, String)> {
    m.tasks.iter().flat_map(|t| t.files.iter())
}

fn rel_of<'a>(abs: &Path, root: &Path) -> String {
    abs.strip_prefix(root)
        .map(|r| r.to_string_lossy().into_owned())
        .unwrap_or_else(|_| abs.to_string_lossy().into_owned())
}

/// Scoping verdict for one absolute path while a manifest is active.
fn allowed(abs: &Path, m: &Manifest) -> bool {
    if abs != m.root && !abs.starts_with(&m.root) {
        return true; // outside the scoped repo entirely
    }
    let rel = rel_of(abs, &m.root);
    if rel == ".pixel" || rel.starts_with(".pixel/") {
        return true;
    }
    let target_paths: HashSet<&str> = all_files(m).map(|(p, _)| p.as_str()).collect();
    if target_paths.contains(rel.as_str()) {
        return true;
    }
    if abs.is_dir() {
        if rel.is_empty() || rel == "." {
            return true;
        }
        let prefix = format!("{rel}/");
        return target_paths.iter().any(|t| t.starts_with(&prefix));
    }
    let basename = abs.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if ORIENTATION_ANY.contains(&basename) {
        return true;
    }
    if ORIENTATION_ROOT.contains(&rel.as_str()) {
        return true;
    }
    false
}

fn is_exempt(abs: &Path, idx_root: &Path) -> bool {
    if abs != idx_root && !abs.starts_with(idx_root) {
        return true; // outside the indexed repo
    }
    let rel = rel_of(abs, idx_root);
    if rel.starts_with(".pixel/") {
        return true;
    }
    let basename = abs.file_name().and_then(|n| n.to_str()).unwrap_or("");
    ORIENTATION_ANY.contains(&basename) || ORIENTATION_ROOT.contains(&rel.as_str())
}

/// Build the NON-BLOCKING advisory response JSON. Deliberately carries NO
/// `permissionDecision`: the tool call proceeds through the normal
/// permission flow; the note is surfaced to the user (`systemMessage`) and
/// offered to the model (`additionalContext`).
fn advisory_json(note: &str) -> Value {
    serde_json::json!({
        "systemMessage": note,
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "additionalContext": note
        }
    })
}

/// Emit a non-blocking advisory and allow the tool call (exit 0).
fn advise(lines: &[String]) -> ! {
    print!("{}", advisory_json(&lines.join("\n")));
    std::process::exit(0);
}

/// Convert a legacy corrective message into a non-blocking advisory while
/// preserving its useful alternative and explanation. The hook must never
/// force a retry merely because Pixel has a preferred operation.
fn non_blocking_advisory_lines(lines: &[String]) -> Vec<String> {
    let mut out = lines.to_vec();
    if let Some(first) = out.first_mut() {
        *first = first.replacen("BLOCKED", "pixel-guard advisory", 1);
    }
    out.push("Proceeding with the original command or tool call.".into());
    out
}

/// Truncate a task string for display (char-safe, appends an ellipsis).
fn short_task(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let cut: String = s.chars().take(max_chars).collect();
    format!("{cut}…")
}

/// Advisory note for a read/edit outside the active targets manifest.
/// Non-blocking by design: the sniper-discovery benchmark showed hard
/// scoping denies collapse task recall, so the fence informs instead.
fn scoping_advisory_lines(abs: &Path, m: &Manifest) -> Vec<String> {
    let rel = rel_of(abs, &m.root);
    let total: usize = m.tasks.iter().map(|t| t.files.len()).sum();
    let mut lines = vec![format!(
        "pixel-targets-guard advisory: '{rel}' is outside the active targets manifest ({} task(s), {total} file(s)):",
        m.tasks.len()
    )];
    lines.extend(
        m.tasks
            .iter()
            .map(|t| format!("  - '{}'", short_task(&t.task, 70))),
    );
    lines.push(
        "Proceeding. If scope has drifted, re-run `pixel targets \"<refined task>\"`".into(),
    );
    lines.push(
        "to refresh your task's list, or `pixel targets --clear` to end scoping.".into(),
    );
    lines
}

fn scoping_advisory(abs: &Path, m: &Manifest) -> ! {
    advise(&scoping_advisory_lines(abs, m));
}

/// Advisory note for an edit in an indexed repo with no active manifest.
fn mandate_advisory_lines(abs: &Path, idx_root: &Path) -> Vec<String> {
    let rel = rel_of(abs, idx_root);
    vec![
        "pixel-targets-guard advisory: no sniper target list is active for this repo.".into(),
        format!("Proceeding with this edit ({rel}), but scoping first is recommended:"),
        "  pixel targets \"<one-line task description>\" .".into(),
        "That returns the closed P0/P1/P2 file list and activates .pixel/targets.json.".into(),
        "Ending a task: pixel targets --clear".into(),
    ]
}

fn mandate_advisory(abs: &Path, idx_root: &Path) -> ! {
    advise(&mandate_advisory_lines(abs, idx_root));
}

/// True for tools that perform codebase retrieval (Grep, Glob, find_file_by_name).
/// Read is NOT a retrieval tool — reading a known file path is consumption,
/// not search. `search` is included (some agents use it for code search).
/// Antigravity: grep_search, find_by_name, list_dir, file_search are all retrieval.
fn is_retrieval_tool(tool: &str) -> bool {
    matches!(
        tool,
        "Grep" | "grep" | "Glob" | "glob" | "find_file_by_name" | "search"
        | "find" | "ls"
        // Antigravity/Gemini retrieval tools
        | "grep_search" | "find_by_name" | "list_dir" | "file_search"
    )
}

/// True for tools that read file contents (Read, read_file, view_file, etc).
/// Used for the retrieval-first scoping advisory — reading a source file in
/// an indexed repo with no manifest gets a non-blocking suggestion to run
/// `pixel targets` first.
fn is_read_tool(tool: &str) -> bool {
    matches!(
        tool,
        "Read" | "read" | "read_file" | "notebook_read"
        | "view_file"  // Antigravity
    )
}

/// True if the file extension suggests source code (not config/docs/prose).
/// Used to limit the read-scoping advisory to source files — reading a
/// README or package.json is always legitimate.
fn is_source_file(p: &Path) -> bool {
    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
    matches!(
        ext,
        "rs" | "ts" | "tsx" | "js" | "jsx" | "py" | "go" | "java" | "c" | "cpp"
        | "h" | "hpp" | "cs" | "rb" | "swift" | "kt" | "scala" | "clj" | "ex"
        | "exs" | "erl" | "hs" | "ml" | "fs" | "nim" | "zig" | "v" | "lua"
        | "php" | "pl" | "r" | "dart" | "elm" | "julia" | "lisp" | "sch"
    )
}

/// Advisory note for reading a source file in an indexed repo with no active
/// manifest. Non-blocking — the read proceeds. Suggests `pixel targets` first
/// to scope the work, and `pixel search --context` as a cheaper alternative
/// to reading the entire file.
fn read_scoping_advisory_lines(abs: &Path, idx_root: &Path) -> Vec<String> {
    let rel = rel_of(abs, idx_root);
    vec![
        format!("pixel-guard advisory: reading source file '{rel}' in an indexed repo with no active targets manifest."),
        "Consider scoping first to identify the relevant files:".into(),
        format!("  pixel targets \"<one-line task description>\" {}", idx_root.display()),
        "Or use `pixel search '<pattern>' --context 5` to get the relevant code with surrounding context — no full-file Read needed.".into(),
        "Proceeding with this read.".into(),
    ]
}

fn read_scoping_advisory(abs: &Path, idx_root: &Path) -> ! {
    advise(&read_scoping_advisory_lines(abs, idx_root));
}

/// Check if an env var is explicitly set to "0"/"false"/"off" (kill-switch
/// pattern, mirroring the top-level PIXEL_TARGETS_GUARD check).
fn env_flag_off(name: &str) -> bool {
    std::env::var(name)
        .map(|v| matches!(v.as_str(), "0" | "false" | "off"))
        .unwrap_or(false)
}

/// Advisory for Grep/Glob/find in an indexed repo with no active manifest.
/// Tells the agent to run `pixel targets` first while allowing retrieval.
fn retrieval_guard_advisory(_cwd: &Path, idx_root: &Path) -> ! {
    let root = idx_root.display().to_string();
    advise(&[
        "pixel-guard advisory: code search happened before retrieval scoping.".into(),
        "In an indexed directory, consider running `pixel targets` before searching the codebase.".into(),
        format!("  pixel targets \"<one-line task description>\" {root}"),
        "That returns the P0/P1/P2 file list in <50ms. Work P0 first, then P1.".into(),
        "After scoping, use `pixel search` / `pixel resolve` for code search — not grep/glob.".into(),
        "Proceeding with the original retrieval call.".into(),
    ]);
}

/// Advisory for edits to existing files in an indexed repo with no active
/// manifest. Suggests scoping before editing, but never blocks the edit.
fn edit_guard_advisory(abs: &Path, idx_root: &Path) -> ! {
    let rel = rel_of(abs, idx_root);
    let root = idx_root.display().to_string();
    advise(&[
        format!("pixel-guard advisory: editing {rel} before retrieval scoping."),
        "In an indexed directory, consider running `pixel targets` before editing existing files.".into(),
        format!("  pixel targets \"<one-line task description>\" {root}"),
        "That returns the P0/P1/P2 file list. If this file is in the list, it is a useful scope check.".into(),
        "Proceeding with the original edit.".into(),
    ]);
}

/// Advisory note when the targets manifest exists but every task in it has
/// exceeded the 24h TTL.
fn expired_manifest_advisory_lines(idx_root: &Path) -> Vec<String> {
    vec![
        format!(
            "pixel-targets-guard advisory: the targets manifest in {} has expired (24h TTL).",
            idx_root.join(".pixel").join("targets.json").display()
        ),
        "Proceeding unscoped. If you are still working a scoped task, re-run".into(),
        "  pixel targets \"<one-line task description>\" .".into(),
    ]
}

fn expired_manifest_advisory(idx_root: &Path) -> ! {
    advise(&expired_manifest_advisory_lines(idx_root));
}

/// Advisory for edits in a directory that pixel hasn't indexed yet: suggest
/// indexing so scoped retrieval works, then proceed. Pixel works in ANY
/// directory — not just git repos. The `is_git` flag adjusts the message.
fn suggest_index_advisory(dir: &Path, is_git: bool) -> ! {
    let repo_phrase = if is_git { "git repo" } else { "directory" };
    advise(&[
        format!("pixel-targets-guard advisory: this {} has not been indexed by pixel yet.", repo_phrase),
        "Proceeding. To enable pixel's scoped retrieval (one-time, takes seconds):".into(),
        format!("  pixel index {}", dir.display()),
        "Then scope tasks with: pixel targets \"<one-line task description>\" .".into(),
        "Pixel works in any directory — not just git repos. The index is a .pixel/ dir.".into(),
    ]);
}

/// Bash-command checks stay conservative around substitutions and heredocs.
/// Any safety or search recommendation generated here is converted to a
/// non-blocking advisory before it reaches the hook protocol.
/// Advisory-only check for Bash commands — called AFTER rewrite attempt
/// in run() so that rewrites take priority over advisories. This contains
/// the scoping advisory plus advisories for common grep/search bypass patterns
/// (sed, awk, perl, python, find, ls, cat).
fn check_bash_advisories(cmd: &str, cwd: &Path, idx_root: Option<&Path>, manifest: Option<&Manifest>) {
    // Strip leading `cd X &&` before pattern matching — the same stripping
    // that try_rewrite_bash does. strip_cd_prefix returns (effective_cwd, effective_cmd).
    let (effective_cwd, effective_cmd) = strip_cd_prefix(cmd, cwd);
    // Skip complex commands — heredocs, command substitution are left alone.
    if effective_cmd.contains("<<") || effective_cmd.contains("$(") || effective_cmd.contains('`') {
        return;
    }
    // Bypass-pattern advisories for indexed repos
    if let Some(root) = idx_root {
        if let Some(lines) = bypass_advisory_lines(effective_cmd, &effective_cwd, root) {
            advise(&non_blocking_advisory_lines(&lines));
        }
    }
    if let Some(m) = manifest {
        if let Some(first_file) = single_reader_target(effective_cmd, &effective_cwd) {
            if !allowed(&first_file, m) {
                scoping_advisory(&first_file, m);
            }
        }
    }
}

/// Advisory messages for common grep/search bypass patterns that should use pixel
/// instead. Returns Some(lines) if the command matches a known bypass pattern,
/// or None if it's a legitimate use case.
fn bypass_advisory_lines(cmd: &str, cwd: &Path, root: &Path) -> Option<Vec<String>> {
    let mut tokens = simple_tokenize(cmd);
    if tokens.is_empty() {
        return None;
    }
    // Strip shell wrapper prefixes (rtk, command, builtin) so `command grep`
    // and `rtk grep` are properly intercepted. This closes the wrapper-prefix
    // evasion where an agent invokes `command grep` to bypass a guard that
    // only checks for bare `grep`.
    while matches!(tokens.first().map(String::as_str), Some("rtk") | Some("command") | Some("builtin")) {
        tokens.remove(0);
        if tokens.is_empty() {
            return None;
        }
    }
    // Normalize the binary name: strip directory prefix so `/usr/bin/grep`,
    // `/bin/cat`, `/usr/local/bin/rg` etc. all match their base names.
    // Agents evade the guard by using absolute paths -- this closes that bypass.
    let bin_raw = tokens[0].as_str();
    let bin = normalize_bin(bin_raw);
    match bin {

        // sed as search: sed -n '/pattern/p' file
        "sed" if tokens.len() >= 3 && tokens.contains(&"-n".to_string()) => Some(vec![
            "BLOCKED by pixel-guard: sed used as a search tool — use pixel search instead.".to_string(),
            format!("  pixel search '<pattern>' {} --context 5", root.display()),
            "sed -n '/p' prints matching lines; pixel search returns them with context.".to_string(),
        ]),
        // awk as search: awk '/pattern/' file
        "awk" if tokens.len() >= 3 && tokens.iter().any(|t| t.starts_with('/') && t.ends_with('/')) => Some(vec![
            "BLOCKED by pixel-guard: awk used as a search tool — use pixel search instead.".to_string(),
            format!("  pixel search '<pattern>' {} --context 5", root.display()),
            "awk '/pattern/' prints matching lines; pixel search returns them with context.".to_string(),
        ]),
        // perl one-liner search: perl -ne 'print if /pattern/' file
        "perl" if tokens.len() >= 3 && tokens.iter().any(|t| t.contains("/")) => Some(vec![
            "BLOCKED by pixel-guard: perl used as a search tool — use pixel search instead.".to_string(),
            format!("  pixel search '<pattern>' {} --context 5", root.display()),
            "perl -ne 'print if /x/' prints matching lines; pixel search returns them with context.".to_string(),
        ]),
        // python3 -c search: python3 -c "...open(f)...search..."
        "python3" if tokens.len() >= 4 && tokens.contains(&"-c".to_string()) => Some(vec![
            "BLOCKED by pixel-guard: python3 used as a search tool — use pixel search instead.".to_string(),
            format!("  pixel search '<pattern>' {} --context 5", root.display()),
            "Python one-liners for code search bypass the deterministic index; use pixel.".to_string(),
        ]),
        // python (alias) - same
        "python" if tokens.len() >= 4 && tokens.contains(&"-c".to_string()) => Some(vec![
            "BLOCKED by pixel-guard: python used as a search tool — use pixel search instead.".to_string(),
            format!("  pixel search '<pattern>' {} --context 5", root.display()),
            "Python one-liners for code search bypass the deterministic index; use pixel.".to_string(),
        ]),
        // node -e search
        "node" if tokens.len() >= 4 && tokens.contains(&"-e".to_string()) => Some(vec![
            "BLOCKED by pixel-guard: node used as a search tool — use pixel search instead.".to_string(),
            format!("  pixel search '<pattern>' {} --context 5", root.display()),
            "Node.js one-liners for code search bypass the deterministic index; use pixel.".to_string(),
        ]),
        // ruby -e search
        "ruby" if tokens.len() >= 4 && tokens.contains(&"-e".to_string()) => Some(vec![
            "BLOCKED by pixel-guard: ruby used as a search tool — use pixel search instead.".to_string(),
            format!("  pixel search '<pattern>' {} --context 5", root.display()),
            "Ruby one-liners for code search bypass the deterministic index; use pixel.".to_string(),
        ]),
        // lua -e search
        "lua" if tokens.len() >= 4 && tokens.contains(&"-e".to_string()) => Some(vec![
            "BLOCKED by pixel-guard: lua used as a search tool — use pixel search instead.".to_string(),
            format!("  pixel search '<pattern>' {} --context 5", root.display()),
            "Lua one-liners for code search bypass the deterministic index; use pixel.".to_string(),
        ]),
        // ag (the silver searcher) - alternative to grep
        "ag" if tokens.len() >= 2 => Some(vec![
            "BLOCKED by pixel-guard: ag (silver searcher) used for code search — use pixel search instead.".to_string(),
            format!("  pixel search '<pattern>' {} --context 5", root.display()),
            "ag is a grep alternative; pixel search provides deterministic retrieval from the index.".to_string(),
        ]),
        // ack - alternative to grep
        "ack" if tokens.len() >= 2 => Some(vec![
            "BLOCKED by pixel-guard: ack used for code search — use pixel search instead.".to_string(),
            format!("  pixel search '<pattern>' {} --context 5", root.display()),
            "ack is a grep alternative; pixel search provides deterministic retrieval from the index.".to_string(),
        ]),
        // egrep - extended grep
        "egrep" if tokens.len() >= 2 => Some(vec![
            "BLOCKED by pixel-guard: egrep used for code search — use pixel search instead.".to_string(),
            format!("  pixel search '<pattern>' {} --context 5", root.display()),
            "egrep is grep with extended regex; pixel search handles all regex patterns.".to_string(),
        ]),
        // fgrep - fixed-string grep
        "fgrep" if tokens.len() >= 2 => Some(vec![
            "BLOCKED by pixel-guard: fgrep used for code search — use pixel search instead.".to_string(),
            format!("  pixel search '<pattern>' {} --context 5", root.display()),
            "fgrep is grep for fixed strings; pixel search handles literal patterns too.".to_string(),
        ]),
        // find -exec grep: find ... -exec grep ... {} +
        "find" if tokens.len() >= 5 && tokens.contains(&"-exec".to_string()) => {
            // Only block when the exec chain contains grep/rg/ag/ack
            if tokens.iter().any(|t| t == "grep" || t == "rg" || t == "ag" || t == "ack") {
                Some(vec![
                    "BLOCKED by pixel-guard: find -exec grep nests grep inside find — use pixel search directly.".to_string(),
                    format!("  pixel search '<pattern>' {} --context 5", root.display()),
                    "find -exec grep adds indirection; pixel search is the deterministic path.".to_string(),
                ])
            } else {
                None
            }
        }
        // find -name (file discovery): find ... -name "*.rs"
        "find" if tokens.contains(&"-name".to_string()) => Some(vec![
            "BLOCKED by pixel-guard: find -name for file discovery — use pixel search or pixel targets.".to_string(),
            format!("  pixel search '<pattern>' {} --context 5  # for content search", root.display()),
            format!("  pixel targets \"<task>\" {}  # for file scoping", root.display()),
            "find -name patterns locate files by name; pixel search finds content, pixel targets scopes files.".to_string(),
        ]),
        // xargs grep: find ... | xargs grep
        "xargs" if tokens.len() >= 2 && tokens.iter().any(|t| t == "grep" || t == "rg" || t == "ag" || t == "ack") => Some(vec![
            "BLOCKED by pixel-guard: xargs grep pattern — use pixel search directly.".to_string(),
            format!("  pixel search '<pattern>' {} --context 5", root.display()),
            "xargs grep adds pipeline indirection; pixel search is the deterministic path.".to_string(),
        ]),
        // ls of source dir: ls crates/pixel-graph/src/
        "ls" if tokens.len() >= 2 => {
            // Check if the target is a directory that looks like source code
            if let Some(path) = tokens.get(1) {
                let resolved = resolve(path, cwd)?;
                if resolved.is_dir() {
                    // Heuristic: directory name suggests source code
                    let dir_name = resolved.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    if dir_name == "src" || dir_name == "lib" || dir_name == "test" || dir_name == "tests" || dir_name == "include" {
                        return Some(vec![
                            "BLOCKED by pixel-guard: ls of source directory — use pixel search or pixel targets.".to_string(),
                            format!("  pixel search '<pattern>' {} --context 5  # for content search", root.display()),
                            format!("  pixel targets \"<task>\" {}  # for file scoping", root.display()),
                            "ls lists files; pixel search finds content deterministically.".to_string(),
                        ]);
                    }
                }
            }
            None
        }
        // cat of source file: cat crates/pixel-graph/src/extract.rs
        "cat" if tokens.len() == 2 => {
            if let Some(path) = tokens.get(1) {
                let resolved = resolve(path, cwd)?;
                if resolved.is_file() {
                    let ext = resolved.extension().and_then(|e| e.to_str()).unwrap_or("");
                    if matches!(ext, "rs" | "ts" | "tsx" | "js" | "py" | "go" | "java" | "c" | "cpp" | "h" | "hpp" | "cs") {
                        return Some(vec![
                            "BLOCKED by pixel-guard: cat of source file — use pixel resolve or pixel search.".to_string(),
                            format!("  pixel resolve '<symbol>' {}  # jump to definition", root.display()),
                            format!("  pixel search '<pattern>' {} --context 5  # find in file", root.display()),
                            "Read tool is for known files; pixel handles code navigation.".to_string(),
                        ]);
                    }
                }
            }
            None
        }
        // head/tail/more/less used as file readers
        "head" | "tail" => {
            // Find the file argument (skip flags like -20, -n 20)
            let file_arg = tokens.iter().skip(1).find(|t| !t.starts_with('-'));
            if let Some(path) = file_arg {
                let resolved = resolve(path, cwd)?;
                if resolved.is_file() {
                    let ext = resolved.extension().and_then(|e| e.to_str()).unwrap_or("");
                    if matches!(ext, "rs" | "ts" | "tsx" | "js" | "py" | "go" | "java" | "c" | "cpp" | "h" | "hpp" | "cs") {
                        return Some(vec![
                            format!("BLOCKED by pixel-guard: {} of source file — use pixel search --context or Read.", bin),
                            format!("  pixel search '<pattern>' {} --context 10  # with more lines", root.display()),
                            "Read tool for known files; pixel search for content discovery.".to_string(),
                        ]);
                    }
                }
            }
            None
        }
        _ => None,
    }
}

/// Safety-advisory tier for Bash commands: destructive git operations. Returns
/// recommendation lines, or None when the command is not recognized.
/// Deliberately has NO substitution/heredoc bail — see the guard documentation.
fn bash_deny_lines(cmd: &str, idx_root: Option<&Path>) -> Option<Vec<String>> {
    let root = idx_root?;
    if !cmd.contains("git") {
        return None;
    }
    for (sub, args) in git_invocations(cmd) {
        if let Some(lines) = destructive_git_deny(&sub, &args, root) {
            return Some(lines);
        }
    }
    None
}

/// Split a shell command into pipeline/sequence segments and extract every
/// `git <subcommand> <args…>` invocation as owned tokens. Uses the guard's
/// quote-aware segmenting tokenizer — not a full shell parser, but robust to
/// flag ordering, to substitution-wrapped arguments (a `$(…)` chunk becomes
/// ordinary tokens that simply never match a destructive flag), and to
/// separators inside quoted arguments (a multi-line `--message "…git add…"`
/// never opens a phantom `git` segment).
fn git_invocations(cmd: &str) -> Vec<(String, Vec<String>)> {
    let mut out = Vec::new();
    for tokens in tokenize_segments(cmd) {
        let Some(git_pos) = tokens.iter().position(|t| t == "git") else {
            continue;
        };
        let mut rest = tokens[git_pos + 1..].iter();
        let mut sub = None;
        while let Some(t) = rest.next() {
            if t == "-C" || t == "-c" {
                let _ = rest.next(); // skip the global flag's value
                continue;
            }
            if t.starts_with('-') {
                continue; // other global flags (--no-pager, --git-dir=…)
            }
            sub = Some(t.clone());
            break;
        }
        if let Some(sub) = sub {
            out.push((sub, rest.cloned().collect()));
        }
    }
    out
}

/// True for a combined short-flag cluster containing `c` (e.g. `-fd`
/// contains 'f', `-Df` contains 'D'). Long flags (`--force`) don't match.
fn short_cluster_has(token: &str, c: char) -> bool {
    token.len() >= 2
        && token.starts_with('-')
        && !token.starts_with("--")
        && token[1..].chars().all(|ch| ch.is_ascii_alphanumeric())
        && token[1..].contains(c)
}

/// True when `ref` looks like a branch name — not a relative ref
/// (`HEAD`, `HEAD~N`, `HEAD^`) and not a raw OID (40/64 hex chars).
/// Used to distinguish `git reset --hard <branch>` (repoint, no data
/// loss) from `git reset --hard HEAD~3` (real data loss).
fn is_branch_like(ref_str: &str) -> bool {
    if ref_str.is_empty() {
        return false;
    }
    // Relative refs — HEAD, HEAD~N, HEAD^, HEAD@{N}
    if ref_str == "HEAD" || ref_str.starts_with("HEAD~") || ref_str.starts_with("HEAD^") || ref_str.starts_with("HEAD@") {
        return false;
    }
    // Raw OID — 40 (SHA-1) or 64 (SHA-256) hex chars
    let trimmed = ref_str.trim();
    if (trimmed.len() == 40 || trimmed.len() == 64) && trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
        return false;
    }
    // Short OID — 7+ hex chars (git accepts abbreviated SHAs)
    if trimmed.len() >= 7 && trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
        return false;
    }
    true
}

/// Read the current branch name from `.git/HEAD` (the `ref: refs/heads/X`
/// line). Returns `None` if detached or unreadable.
fn current_branch(root: &Path) -> Option<String> {
    let head = std::fs::read_to_string(root.join(".git").join("HEAD")).ok()?;
    let head = head.trim();
    head.strip_prefix("ref: refs/heads/").map(|s| s.to_string())
}

/// Deny verdict for one parsed `git <sub> <args>` invocation. Flag-order
/// robust: matching is on tokens, not raw substrings.
fn destructive_git_deny(sub: &str, args: &[String], root: &Path) -> Option<Vec<String>> {
    let has = |flag: &str| args.iter().any(|a| a == flag);
    let cluster = |c: char| args.iter().any(|a| short_cluster_has(a, c));
    match sub {
        "reset" if has("--hard") || has("--keep") => {
            let target = args.iter().find(|a| !a.starts_with('-'));
            match target {
                // `git reset --hard <branch>` — repointing current branch at
                // another branch. Not data loss; the right alternative is
                // `git checkout -B` which does the same without the destructive
                // connotation. Suggest it directly so the agent doesn't
                // trial-and-error its way past the block.
                Some(t) if is_branch_like(t) => {
                    let current = current_branch(root).unwrap_or_else(|| "<branch>".into());
                    Some(vec![
                        "BLOCKED by pixel-targets-guard: `git reset --hard` to a branch ref repoints the current branch.".into(),
                        "Use `git checkout -B` instead — same effect, non-destructive semantics:".into(),
                        format!("  git checkout -B {current} {t}"),
                        "(The destructive tier still guards `reset --hard HEAD~N` and raw OIDs — those are real data loss.)".into(),
                    ])
                }
                // `git reset --hard HEAD~N` / raw OID / HEAD — actual data
                // loss. Keep the rescue suggestion.
                _ => Some(vec![
                    "BLOCKED by pixel-targets-guard: `git reset --hard/--keep` destroys in-progress work.".into(),
                    "\"It was working before\" is a rescue problem — use the surgical planner:".into(),
                    "  pixel rescue \"<what broke>\" .            # plan: versions + recommended last-good".into(),
                    "  pixel rescue --apply <oid> --file <path>  # gated restore (working tree only)".into(),
                    "Dirty files: add --merge (3-way, keeps your edits) or --stash-first.".into(),
                ]),
            }
        }
        // `--ours`/`--theirs` select a side of an unmerged path — the
        // idiomatic conflict-resolution form (`git checkout --theirs -- f`).
        // Git itself errors on non-conflicted paths, so exempting them never
        // opens a historical-restore path.
        "checkout" if has("--") && !has("--ours") && !has("--theirs") => {
            Some(raw_restore_deny())
        }
        "checkout" if has("--force") || cluster('f') => Some(vec![
            "BLOCKED by pixel-targets-guard: `git checkout -f/--force` discards in-progress work.".into(),
            "Use the surgical planner instead:".into(),
            "  pixel rescue \"<what broke>\" .            # plan: versions + recommended last-good".into(),
            "  pixel rescue --apply <oid> --file <path> [--merge|--stash-first]".into(),
        ]),
        "restore" if args.iter().any(|a| a == "--source" || a.starts_with("--source=")) => {
            Some(raw_restore_deny())
        }
        "clean" if has("--force") || cluster('f') => Some(vec![
            "BLOCKED by pixel-targets-guard: `git clean -f` permanently deletes untracked files.".into(),
            "If something went missing, recover it instead of deleting more:".into(),
            "  pixel excavate --phrase \"<what you're looking for>\"  # history/stash/reflog search".into(),
            "  pixel rescue \"<what broke>\" .".into(),
        ]),
        // First NON-FLAG argument, so `git stash -q drop` doesn't slip past.
        "stash" if args.iter().find(|a| !a.starts_with('-')).is_some_and(|a| a == "drop" || a == "clear") => Some(vec![
            "BLOCKED by pixel-targets-guard: `git stash drop/clear` permanently discards stashed work.".into(),
            "Stashed code is recoverable history — use:".into(),
            "  pixel excavate --phrase \"<what you're looking for>\"  # searches stash + reflog too".into(),
        ]),
        "branch" if has("-D") || cluster('D') || (has("--delete") && (has("--force") || cluster('f'))) => {
            Some(vec![
                "BLOCKED by pixel-targets-guard: `git branch -D` force-deletes unmerged work.".into(),
                "If the branch's code matters, recover it deliberately:".into(),
                "  pixel excavate --phrase \"<what you're looking for>\"".into(),
                "  pixel rescue \"<what broke>\" .".into(),
            ])
        }
        // `--force-with-lease` (and `--force-if-includes`) are the safe
        // forms pixel's own ops use — only bare `--force`/`-f` is denied.
        "push" if has("--force") || cluster('f') => Some(vec![
            "BLOCKED by pixel-targets-guard: `git push --force` can destroy remote history.".into(),
            "Use pixel's gated mutation ops instead:".into(),
            format!("  pixel push --request-id <id> {}", shell_quote(&root.display().to_string())),
            format!("  pixel ship --files <f>... --message \"<msg>\" --request-id <id> {}", shell_quote(&root.display().to_string())),
            "(pixel push uses --force-with-lease semantics only where safe.)".into(),
        ]),
        // `git merge` used to integrate a branch is denied outright: the
        // doctrine forbids merge commits without exception, and
        // `reconcile` is the deterministic replacement. `--abort` /
        // `--continue` / `--quit` are merge-state *exits*, not
        // integrations — denying those would strand an agent mid-conflict
        // with no way out, so they pass through.
        "merge"
            if !args.iter().any(|a| {
                a == "--abort" || a == "--continue" || a == "--quit"
            }) =>
        {
            Some(vec![
                "BLOCKED by pixel-targets-guard: `git merge` creates a merge commit — forbidden without exception.".into(),
                "Branch integration is deterministic reconciliation:".into(),
                format!(
                    "  pixel reconcile {} --strategy rebase-if-clean",
                    shell_quote(&root.display().to_string())
                ),
                "It proves a clean rebase via merge-tree before touching the worktree and".into(),
                "reports structured conflicts when they exist. (`git merge --abort/--continue`".into(),
                "are not blocked — they exit an in-progress merge.)".into(),
            ])
        }
        _ => None,
    }
}

fn raw_restore_deny() -> Vec<String> {
    vec![
        "BLOCKED by pixel-targets-guard: raw historical file restore can clobber in-progress work.".into(),
        "Use the surgical planner instead:".into(),
        "  pixel rescue \"<what broke>\" .            # plan: versions + recommended last-good".into(),
        "  pixel rescue --apply <oid> --file <path> [--merge|--stash-first]".into(),
    ]
}

// ---------------------------------------------------------------------------
// SUBSTITUTE tier — plain git mutations with an exact pixel equivalent are
// matched with the substitute command spelled out. This is advisory, NEVER
// a rewrite: per the invariant at the top of this file a rewrite must never
// add a write step, and every pixel mutation op writes (journal, snapshot
// token). The original command remains available to the agent.
// ---------------------------------------------------------------------------

/// Substitute recommendation for a full Bash command. Only fires in indexed
/// repos, mirroring `bash_deny_lines`.
fn git_mutation_substitute_lines(cmd: &str, idx_root: Option<&Path>, cwd: &Path) -> Option<Vec<String>> {
    // The idx_root is found from the hook payload's cwd, but the actual
    // command may cd to a different directory first (e.g. `cd /repo && git
    // rebase`). Try the idx_root first, then extract a cd/-C target from the
    // command as a fallback for conflict-state checking.
    let root = idx_root?;
    if !cmd.contains("git") {
        return None;
    }
    for (sub, args) in git_invocations(cmd) {
        if let Some(lines) = git_substitute_deny(&sub, &args, root) {
            // Check if a cd target or git -C path has a reconcile conflict
            // state file — if so, allow the rebase as an escape hatch.
            if sub == "rebase" {
                let alt_root = extract_cd_target(cmd, cwd).or_else(|| extract_git_c_path(&args, cwd));
                if let Some(alt) = alt_root {
                    if alt != root && reconcile_conflict_pending(&alt) {
                        return None;
                    }
                }
            }
            return Some(lines);
        }
    }
    None
}

/// Extract the target of a `cd <path>` in the command string, resolved
/// against cwd. Returns None if no cd is found or the path doesn't exist.
fn extract_cd_target(cmd: &str, cwd: &Path) -> Option<PathBuf> {
    // Match `cd <path>` possibly followed by `&&` or `;`
    let cd_idx = cmd.find("cd ")?;
    let rest = &cmd[cd_idx + 3..];
    let end = rest.find(|c: char| c == '&' || c == ';').unwrap_or(rest.len());
    let path = rest[..end].trim().trim_matches(|c: char| c == '"' || c == '\'');
    if path.is_empty() {
        return None;
    }
    let p = Path::new(path);
    let resolved = if p.is_absolute() { p.to_path_buf() } else { cwd.join(p) };
    resolved.canonicalize().ok().filter(|p| p.is_dir())
}

/// Extract the path from `git -C <path>` args, resolved against cwd.
fn extract_git_c_path(args: &[String], cwd: &Path) -> Option<PathBuf> {
    let c_idx = args.iter().position(|a| a == "-C")?;
    let path = args.get(c_idx + 1)?;
    let p = Path::new(path);
    let resolved = if p.is_absolute() { p.to_path_buf() } else { cwd.join(p) };
    resolved.canonicalize().ok().filter(|p| p.is_dir())
}

/// Per-invocation SUBSTITUTE verdict for `git <sub> <args>`.
///
/// Pass-through table — shapes pixel can NOT cover, deliberately allowed:
///
/// | command shape                                   | why it passes through                          |
/// |-------------------------------------------------|------------------------------------------------|
/// | `git commit --interactive` / `-p`/`--patch`     | interactive hunk staging, no pixel equivalent  |
/// | `git commit --fixup=` / `--squash=`             | targets an interactive-rebase workflow         |
/// | `git push --tags/--delete/-d/--mirror/--all/--prune` | no pixel refspec equivalent               |
/// | `git push -o/--push-option`                     | server options pixel push doesn't forward      |
/// | `git rebase -i/--interactive`                   | interactive todo editing                       |
/// | `git rebase --continue/--abort/--skip/--quit/--edit-todo` | rebase-state exits — denying strands the agent mid-conflict |
/// | `git rebase --onto/--exec/-x/--autosquash/--root` | not expressible as `pixel reconcile`         |
/// | `git checkout -B` / plain `git checkout <ref>`  | force-reset / plain switch (destructive tier already covers `-f`/`--`) |
/// | `git switch` without `-c`/`--create`            | plain branch switch, not a mutation            |
/// | `git add -p`/`--patch`/`-i`/`--interactive`     | interactive hunk staging, no pixel equivalent  |
/// | `git add` during active sequencer (cherry-pick/rebase/merge/revert) | conflict-resolution staging; `--continue` commits, not `pixel publish` |
/// | `git commit` during active sequencer                | concludes the sequencer's own commit (a merge commit needs both parents) — `pixel publish` writes a plain commit and would corrupt the graph |
/// Detect an active git sequencer state (cherry-pick, rebase, merge, or
/// revert) by looking for the marker files git writes into the git
/// directory. When any is present, `git add` is conflict-resolution staging
/// and `git commit` is the sequencer's own conclusion — `pixel publish`
/// (a plain single-parent commit) cannot substitute for either.
///
/// Resolves the git directory from `root/.git`, handling both the common
/// directory case and the worktree file-pointer case (`gitdir: <path>`).
/// Returns `false` on any resolution uncertainty — fail-closed for the
/// substitute deny, so an unknown layout keeps the existing guard behavior.
fn sequencer_in_progress(root: &Path) -> bool {
    let dot_git = root.join(".git");
    let git_dir = if dot_git.is_dir() {
        dot_git
    } else if dot_git.is_file() {
        // Worktree: `.git` is a file containing `gitdir: <path>`.
        let Ok(text) = std::fs::read_to_string(&dot_git) else {
            return false;
        };
        let Some(line) = text.lines().find(|l| l.starts_with("gitdir:")) else {
            return false;
        };
        let pointed = PathBuf::from(line.trim_start_matches("gitdir:").trim());
        // A relative `gitdir:` pointer is relative to the directory holding
        // the `.git` file — resolving it against the process cwd instead
        // would silently return false (fail-closed into a wrong deny).
        if pointed.is_absolute() { pointed } else { root.join(pointed) }
    } else {
        return false; // no .git — not a repo root we can reason about
    };
    // CHERRY_PICK_HEAD / MERGE_HEAD / REVERT_HEAD → cherry-pick, merge, or
    // revert in progress. rebase-merge/ or rebase-apply/ → rebase (or
    // `git am`) in progress.
    git_dir.join("CHERRY_PICK_HEAD").is_file()
        || git_dir.join("MERGE_HEAD").is_file()
        || git_dir.join("REVERT_HEAD").is_file()
        || git_dir.join("rebase-merge").is_dir()
        || git_dir.join("rebase-apply").is_dir()
}

/// Check if `pixel reconcile` has reported a conflict that requires manual
/// resolution. When true, the guard allows `git rebase` as an escape hatch —
/// `pixel reconcile` itself reported "manual resolution required", so the
/// deterministic path is exhausted and raw git is the only way forward.
fn reconcile_conflict_pending(root: &Path) -> bool {
    root.join(".pixel").join("reconcile-conflict.json").is_file()
}

/// Run `git status --porcelain` in `root` and return the list of modified
/// (tracked) file paths. Used to auto-populate the `pixel publish --files`
/// recommendation for `git add .` with the actual files.
/// Returns None on spawn failure; empty vec if no modified files.
fn git_status_porcelain_files(root: &Path) -> Option<Vec<String>> {
    let output = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let files: Vec<String> = text
        .lines()
        .filter_map(|line| {
            // Format: "XY <path>" where XY is 2 status chars.
            // Only include modified/added/staged files (not untracked ??).
            if line.len() < 4 {
                return None;
            }
            let status = &line[..2];
            // Skip untracked files (??) -- git add . would stage them, but
            // pixel publish expects tracked files. Untracked files need to
            // be explicitly listed by the agent.
            if status == "??" {
                return None;
            }
            let path = line[3..].trim();
            if path.is_empty() {
                return None;
            }
            // Handle rename: "R  old -> new" -- take the new path
            if let Some(arrow) = path.find(" -> ") {
                return Some(path[arrow + 4..].to_string());
            }
            Some(path.to_string())
        })
        .collect();
    Some(files)
}

fn git_substitute_deny(sub: &str, args: &[String], root: &Path) -> Option<Vec<String>> {
    let root_q = shell_quote(&root.display().to_string());
    match sub {
        "add" => {
            // Interactive hunk staging — no pixel equivalent, pass through.
            if args.iter().any(|a| {
                a == "-p" || a == "--patch" || a == "-i" || a == "--interactive"
            }) {
                return None;
            }
            // Conflict-resolution staging during an active sequencer
            // (cherry-pick / rebase / merge): `git add` here stages resolved
            // files WITHOUT committing — the sequencer's own `--continue`
            // creates the commit. `pixel publish` cannot substitute because it
            // commits in one step, which would either conflict with the
            // sequencer state or produce a stray commit outside the sequencer's
            // replay. Pass through so the agent can resolve and continue.
            if sequencer_in_progress(root) {
                return None;
            }
            // Collect pathspecs (non-flag tokens). Flags that consume a
            // value (-A/--all/-u/--update are self-contained; -N/--intent-to-add
            // too) don't take a following pathspec, but we don't model every
            // value-consuming flag — the common shapes (`git add <files>`,
            // `git add .`, `git add -A`) are covered.
            let all_variant = args.iter().any(|a| {
                a == "." || a == "-A" || a == "--all" || a == "-u" || a == "--update"
            });
            let pathspecs: Vec<&String> = args
                .iter()
                .filter(|a| !a.starts_with('-') && a.as_str() != ".")
                .collect();
            let mut lines = vec![
                "BLOCKED [PIXEL_SUBSTITUTE] by pixel-guard: raw `git add` stages files outside pixel's journaled mutation surface.".into(),
                "`pixel publish` stages AND commits in one step — use it instead:".into(),
            ];
            if !pathspecs.is_empty() {
                let files = pathspecs
                    .iter()
                    .map(|f| format!("--files {}", shell_quote(f)))
                    .collect::<Vec<_>>()
                    .join(" ");
                lines.push(format!(
                    "  pixel publish {files} --message \"<msg>\" --request-id <id> {root_q}"
                ));
            } else if all_variant {
                lines.push(format!(
                    "  pixel publish --files <f1> [--files <f2> …] --message \"<msg>\" --request-id <id> {root_q}"
                ));
                lines.push(
                    "List each modified tracked file as its own --files flag (run `pixel changes .` to see them).".into(),
                );
            } else {
                // Deny-with-answer: query git status --porcelain to auto-populate
                // the actual modified files, so the agent doesn't burn a full LLM
                // turn guessing what to stage. Falls back to the generic message
                // on any failure.
                if let Some(files) = git_status_porcelain_files(root) {
                    if !files.is_empty() {
                        let files_str = files
                            .iter()
                            .map(|f| format!("--files {}", shell_quote(f)))
                            .collect::<Vec<_>>()
                            .join(" ");
                        lines.push(format!(
                            "  pixel publish {files_str} --message \"<msg>\" --request-id <id> {root_q}"
                        ));
                        lines.push(
                            "(Auto-populated from git status --porcelain -- adjust if needed.)".into(),
                        );
                        return Some(lines);
                    }
                }
                lines.push(format!(
                    "  pixel publish --files <file> [--files <file2> …] --message \"<msg>\" --request-id <id> {root_q}"
                ));
            }
            Some(lines)
        }
        "commit" => {
            let c = parse_commit_args(args);
            if c.interactive {
                return None; // pass-through: interactive staging
            }
            // Concluding an in-progress sequencer (merge / cherry-pick /
            // revert / rebase): `git commit` here finishes what the sequencer
            // started — for a merge it writes the merge commit with BOTH
            // parents recorded from MERGE_HEAD. `pixel publish` cannot
            // substitute: it creates a plain single-parent commit, silently
            // losing the merge parent. Same rule as the `add` arm above.
            if sequencer_in_progress(root) {
                return None;
            }
            let msg = c
                .message
                .as_deref()
                .map(shell_quote)
                .unwrap_or_else(|| "\"<msg>\"".to_string());
            let files = if c.files.is_empty() {
                "--files <file> [--files <file2> …]".to_string()
            } else {
                c.files
                    .iter()
                    .map(|f| format!("--files {}", shell_quote(f)))
                    .collect::<Vec<_>>()
                    .join(" ")
            };
            let amend = if c.amend { "--amend " } else { "" };
            let what = if c.amend {
                "`git commit --amend`"
            } else {
                "`git commit`"
            };
            let mut lines = vec![
                format!("BLOCKED [PIXEL_SUBSTITUTE] by pixel-guard: raw {what} bypasses pixel's snapshot-gated, journaled mutation surface."),
                "Run the exact equivalent instead (--files repeated once per file):".into(),
                format!("  pixel publish {amend}{files} --message {msg} --request-id <id> {root_q}"),
            ];
            if c.all {
                lines.push(
                    "(-a detected: list each modified tracked file as its own --files flag.)".into(),
                );
            }
            Some(lines)
        }
        // Plain pushes — INCLUDING `--force-with-lease`, which the
        // destructive tier deliberately allows but pixel push covers with
        // the same lease semantics. Bare `--force`/`-f` never reaches
        // here (destructive tier runs first).
        "push" => {
            const PUSH_PASS: &[&str] = &[
                "--tags", "--delete", "-d", "--mirror", "--all", "--prune", "--branches",
            ];
            if args.iter().any(|a| {
                PUSH_PASS.contains(&a.as_str())
                    || a == "-o"
                    || a == "--push-option"
                    || a.starts_with("--push-option=")
            }) {
                return None; // pass-through: no pixel refspec equivalent
            }
            let words: Vec<&String> = args.iter().filter(|a| !a.starts_with('-')).collect();
            let remote = words
                .first()
                .map(|s| shell_quote(s))
                .unwrap_or_else(|| "<remote>".to_string());
            let refspec = words
                .get(1)
                .map(|s| shell_quote(s))
                .unwrap_or_else(|| "<refspec>".to_string());
            Some(vec![
                "BLOCKED [PIXEL_SUBSTITUTE] by pixel-guard: raw `git push` bypasses pixel's snapshot-gated, journaled mutation surface.".into(),
                "Run the exact equivalent instead:".into(),
                format!("  pixel push {remote} {refspec} --request-id <id> {root_q}"),
            ])
        }
        "checkout" => {
            let pos = args.iter().position(|a| a == "-b")?;
            let name = args
                .get(pos + 1)
                .map(|s| shell_quote(s))
                .unwrap_or_else(|| "<name>".to_string());
            Some(branch_substitute_lines("`git checkout -b`", &name, &root_q))
        }
        "switch" => {
            let pos = args.iter().position(|a| a == "-c" || a == "--create")?;
            let name = args
                .get(pos + 1)
                .map(|s| shell_quote(s))
                .unwrap_or_else(|| "<name>".to_string());
            Some(branch_substitute_lines("`git switch -c`", &name, &root_q))
        }
        "rebase" => {
            const REBASE_PASS: &[&str] = &[
                "-i", "--interactive", "--continue", "--abort", "--skip", "--quit",
                "--edit-todo", "--onto", "--exec", "-x", "--autosquash", "--root",
            ];
            if args.iter().any(|a| REBASE_PASS.contains(&a.as_str())) {
                return None; // pass-through: interactive / state exit / not reconcile-expressible
            }
            // Escape hatch: if `pixel reconcile` already reported a conflict
            // (state file exists), allow the rebase so the agent can resolve
            // manually. The guard already allows `git rebase --continue` etc.
            // via REBASE_PASS, but the initial `git rebase origin/main` that
            // starts the rebase is blocked here. When reconcile says "manual
            // resolution required", this is the only path forward.
            if reconcile_conflict_pending(root) {
                return None;
            }
            Some(vec![
                "BLOCKED [PIXEL_SUBSTITUTE] by pixel-guard: raw `git rebase` is replaced by deterministic reconciliation.".into(),
                "Run the exact equivalent instead:".into(),
                format!("  pixel reconcile {root_q} --strategy rebase-if-clean --push auto"),
                "It proves a clean rebase via merge-tree before touching the worktree and reports structured conflicts when they exist.".into(),
                "If reconcile already reported a conflict, use `pixel reconcile --into` or resolve the conflict markers manually.".into(),
            ])
        }
        _ => None,
    }
}

fn branch_substitute_lines(what: &str, name_q: &str, root_q: &str) -> Vec<String> {
    vec![
        format!("BLOCKED [PIXEL_SUBSTITUTE] by pixel-guard: raw {what} bypasses pixel's journaled branch op."),
        "Run the exact equivalent instead (creates AND checks out the branch):".into(),
        format!("  pixel branch {name_q} --request-id <id> {root_q}"),
    ]
}

/// Parsed shape of `git commit` arguments, enough to enrich the
/// `pixel publish` substitute suggestion.
#[derive(Default)]
struct CommitArgs {
    message: Option<String>,
    all: bool,
    amend: bool,
    interactive: bool,
    files: Vec<String>,
}

/// Commit flags that consume a following value token (so the value must
/// not be mistaken for a pathspec).
const COMMIT_VALUE_FLAGS: &[&str] = &[
    "-m", "--message", "-C", "-c", "--fixup", "--squash", "-F", "--file",
    "--author", "--date", "-t", "--template", "--trailer",
];

fn parse_commit_args(args: &[String]) -> CommitArgs {
    let mut out = CommitArgs::default();
    let mut i = 0;
    while i < args.len() {
        let t = args[i].as_str();
        if t == "--amend" {
            out.amend = true;
        } else if t == "-a" || t == "--all" {
            out.all = true;
        } else if t == "--interactive" || t == "--patch" || t == "--fixup" || t == "--squash"
            || t.starts_with("--fixup=") || t.starts_with("--squash=")
        {
            // --fixup/--squash target an interactive-rebase workflow.
            out.interactive = true;
        } else if t == "-m" || t == "--message" {
            out.message = args.get(i + 1).cloned();
            i += 2;
            continue;
        } else if let Some(v) = t.strip_prefix("--message=") {
            out.message = Some(v.to_string());
        } else if t.starts_with("--") {
            if COMMIT_VALUE_FLAGS.contains(&t) {
                i += 2; // long flag + its value
                continue;
            }
            // other long flags (self-contained or --flag=value)
        } else if t.starts_with('-') && t.len() > 1 {
            let body = &t[1..];
            if body.chars().all(|c| c.is_ascii_alphabetic()) {
                // short flag or cluster: -am, -sm, -p …
                if body.contains('a') {
                    out.all = true;
                }
                if body.contains('p') {
                    out.interactive = true;
                }
                if body.ends_with('m') {
                    // -m (possibly clustered) consumes the next token
                    out.message = args.get(i + 1).cloned();
                    i += 2;
                    continue;
                }
                if COMMIT_VALUE_FLAGS.contains(&t) {
                    i += 2; // e.g. -C <commit>, -F <file>
                    continue;
                }
            } else if let Some(v) = t.strip_prefix("-m") {
                // attached form: -m<msg>
                out.message = Some(v.to_string());
            }
        } else {
            out.files.push(t.to_string()); // pathspec
        }
        i += 1;
    }
    out
}

/// If `cmd`'s first pipeline segment is a known reader command with exactly
/// one existing-file argument, resolve and return it. Bails (returns
/// `None`) on anything containing command substitution, backticks,
/// heredocs, or loop keywords — those are too complex to reason about
/// conservatively, so they're simply not checked (fail open).
fn single_reader_target(cmd: &str, cwd: &Path) -> Option<PathBuf> {
    if cmd.contains("$(") || cmd.contains('`') || cmd.contains("<<") {
        return None;
    }
    if ["xargs", "for ", "while "].iter().any(|kw| cmd.contains(kw)) {
        return None;
    }
    let first_segment = cmd
        .split([';', '|'])
        .next()?
        .split("&&")
        .next()?
        .trim();
    let tokens = simple_tokenize(first_segment);
    let (mut tokens, eff_cwd) = if tokens.first().map(String::as_str) == Some("cd") {
        let rest_after_cd = cmd.splitn(2, "&&").nth(1)?.trim();
        let new_cwd = resolve(tokens.get(1)?, cwd)?;
        let rest_tokens = simple_tokenize(rest_after_cd.split([';', '|']).next()?.trim());
        (rest_tokens, new_cwd)
    } else {
        (tokens, cwd.to_path_buf())
    };
    if tokens.first().map(String::as_str) == Some("rtk") {
        tokens.remove(0);
    }
    let cmd_name = tokens.first()?.as_str();
    if !READERS.contains(&cmd_name) && cmd_name != "read" {
        return None;
    }
    let mut args: Vec<&str> = tokens[1..]
        .iter()
        .map(String::as_str)
        .filter(|a| !a.starts_with('-'))
        .collect();
    if matches!(cmd_name, "sed" | "awk") && !args.is_empty() {
        args.remove(0); // the sed/awk program itself, not a file
    }
    let files: Vec<PathBuf> = args
        .iter()
        .filter_map(|a| resolve(a, &eff_cwd))
        .filter(|p| p.is_file())
        .collect();
    if files.len() == 1 {
        Some(files.into_iter().next().unwrap())
    } else {
        None
    }
}

/// Minimal whitespace tokenizer honoring single/double quotes. Not a full
/// shell parser — sufficient for the conservative reader-file detection
/// above, matching the original hook's own scope.
fn simple_tokenize(s: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    for c in s.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => current.push(c),
            None if c == '\'' || c == '"' => quote = Some(c),
            None if c.is_whitespace() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            None => current.push(c),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Quote-aware segmentation + tokenization: split a command into
/// pipeline/sequence segments on UNQUOTED `;`, `|`, `&`, and newlines
/// (`&&`/`||` fall out of the single-char rule), tokenizing each segment
/// with the same quote rules as `simple_tokenize`. Quote state is tracked
/// BEFORE splitting — the raw-string pre-split this replaced cut through
/// quoted arguments, so a multi-line `pixel publish --message "…git add…"`
/// produced a phantom `git add` segment and denied its own substitute.
fn tokenize_segments(s: &str) -> Vec<Vec<String>> {
    let mut segments = Vec::new();
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    for c in s.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => current.push(c),
            None if c == '\'' || c == '"' => quote = Some(c),
            None if c == ';' || c == '|' || c == '&' || c == '\n' => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
                if !tokens.is_empty() {
                    segments.push(std::mem::take(&mut tokens));
                }
            }
            None if c.is_whitespace() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            None => current.push(c),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    if !tokens.is_empty() {
        segments.push(tokens);
    }
    segments
}

// ---------------------------------------------------------------------------
// Command rewriting — transparent upgrade of grep/rg/git to pixel equivalents.
// Modeled on RTK's rewrite approach: the hook returns updatedInput JSON and
// the agent receives pixel's enriched output without knowing the command was
// rewritten. Only fires in indexed repos (.pixel/ exists).
// ---------------------------------------------------------------------------

/// Bash rewrites remain transparent and are limited to semantically equivalent
/// read-only operations. Commands that cannot be safely rewritten continue
/// through the original tool path with an advisory.
/// Emit a PreToolUse "allow" response with a rewritten Bash command. The
/// agent receives pixel's output instead of the original tool's output.
fn allow_rewrite(new_command: &str) -> ! {
    // Deliberately NO permissionDecision:"allow": the rewritten command must
    // still go through normal permission evaluation, so the agent sees and
    // approves the pixel command it is about to run.
    let resp = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "updatedInput": {
                "command": new_command
            }
        }
    });
    print!("{}", resp);
    std::process::exit(0);
}

/// Check if a tool call is a Grep-style search (has a pattern/query field).
fn is_grep_tool(tool: &str, input: &serde_json::Map<String, Value>) -> bool {
    // Claude Code's Grep tool has "pattern"; Devin's grep has "pattern";
    // some agents use "query". Read/Glob don't have pattern fields.
    // Antigravity's grep_search uses "Query"; file_search uses "Query".
    if !matches!(tool, "Grep" | "grep" | "search" | "grep_search" | "file_search") {
        return false;
    }
    input.get("pattern").is_some()
        || input.get("query").is_some()
        || input.get("Query").is_some()
}

/// Build an advisory for a Grep tool call redirecting to `pixel search` —
/// but only when the search is actually equivalent. If the Grep tool
/// carries fields Pixel search can't express (glob/type/output_mode), we
/// Build a non-blocking advisory for a Grep-style tool call. The hook cannot
/// change the tool type from Grep to Bash, so the original call always
/// proceeds; this helper only explains the equivalent Pixel command when one
/// exists.
fn grep_redirect_advisory_lines(
    pattern: &str,
    cwd: &Path,
    input: &serde_json::Map<String, Value>,
) -> Vec<String> {
    // Context flags are expressible; glob/type/output_mode are not.
    let mut flags = Vec::new();
    for f in ["-A", "-B", "-C"] {
        if input.contains_key(f) {
            flags.push(f.to_string());
        }
    }
    if input.contains_key("glob") || input.contains_key("type") || input.contains_key("output_mode") {
        return vec![
            "pixel-guard advisory: this Grep call includes filters that Pixel search cannot preserve exactly.".into(),
            "Proceeding with the original Grep call; use Pixel search when those filters are not needed.".into(),
        ];
    }
    let root = find_up(cwd, ".pixel")
        .map(|r| r.display().to_string())
        .unwrap_or_else(|| ".".to_string());
    let Some(cmd) = search_can_replace(pattern, &flags, &root) else {
        return vec![
            "pixel-guard advisory: this Grep query cannot be represented exactly by Pixel search.".into(),
            "Proceeding with the original Grep call.".into(),
        ];
    };
    vec![
        "pixel-guard advisory: Grep cannot be transparently rewired because the hook cannot change the tool type.".into(),
        format!("Equivalent Bash command if useful: {cmd}"),
        "Proceeding with the original Grep call.".into(),
    ]
}

/// Try to rewrite a Bash command to a pixel equivalent. Returns the new
/// command string if a rewrite applies, or None to let the original pass.
fn try_rewrite_bash(cmd: &str, cwd: &Path) -> Option<String> {
    let trimmed = cmd.trim();

    // Skip complex commands -- heredocs, command substitution are left alone.
    if trimmed.contains("<<")
        || trimmed.contains("$(")
        || trimmed.contains('`')
    {
        return None;
    }

    // Strip a leading `cd <dir> &&` prefix -- agents commonly generate
    // `cd /path && grep ...`. The cd changes the cwd for the grep, so we
    // resolve the new cwd and pass it to the grep rewriter. The rest of
    // the command (after &&) is what we actually rewrite.
    let (effective_cwd, body) = strip_cd_prefix(trimmed, cwd);

    let root_dir = find_up(&effective_cwd, ".pixel").unwrap_or_else(|| effective_cwd.clone());
    let root = root_dir.display().to_string();

    // --- Compound command handling: split on ; and && ---
    // Agents generate commands like `grep ...; echo "---"` or
    // `grep ... && git status`. Instead of bailing on these, split on
    // unquoted ; and &&, try to rewrite each segment, and reassemble.
    // If ANY segment rewrites, return the full reassembled command.
    // If NO segment rewrites, fall through to return None.
    if has_unquoted_semicolon_or_amp(body) {
        return try_rewrite_compound(body, &effective_cwd, &root_dir, trimmed);
    }

    // After stripping cd, check for remaining control operators (&, ;, >, <)
    // that we can't handle. Pipes (|) are handled below.
    // (The ; and && case is already handled above by try_rewrite_compound.)
    if has_unquoted_control(body) {
        return None;
    }

    // --- rg / grep -> pixel search ---
    // Handle pipelines: if the command is `grep ... | grep -v ... | sort`,
    // try to rewrite the FIRST segment (before the first `|`). If the first
    // segment is a grep/rg that can be replaced by `pixel search`, rewrite
    // just that segment and keep the rest of the pipeline intact. This is
    // the common pattern agents generate: `grep -rln "pattern" ... | grep -v
    // node_modules | sort | wc -l`.
    if let Some(pipe_idx) = first_unquoted_pipe(body) {
        let first_segment = body[..pipe_idx].trim();
        let rest = &body[pipe_idx + 1..];
        if let Some(rewritten) = try_rewrite_grep(first_segment, &effective_cwd, &root_dir) {
            // Re-attach the cd prefix if we stripped one, so the rewritten
            // command still runs in the right directory for the pipeline
            // filters that follow.
            if body.len() != trimmed.len() {
                let cd_prefix = &trimmed[..trimmed.len() - body.len()];
                return Some(format!("{cd_prefix}{rewritten} |{rest}"));
            }
            return Some(format!("{rewritten} |{rest}"));
        }
        // First segment isn't a grep -- don't touch the pipeline.
        return None;
    }

    if let Some(rewritten) = try_rewrite_grep(body, &effective_cwd, &root_dir) {
        if body.len() != trimmed.len() {
            let cd_prefix = &trimmed[..trimmed.len() - body.len()];
            return Some(format!("{cd_prefix}{rewritten}"));
        }
        return Some(rewritten);
    }

    // --- git log with search intent -> pixel excavate ---
    if let Some(rewritten) = try_rewrite_git_archaeology(body, &root) {
        if body.len() != trimmed.len() {
            let cd_prefix = &trimmed[..trimmed.len() - body.len()];
            return Some(format!("{cd_prefix}{rewritten}"));
        }
        return Some(rewritten);
    }

    None
}

/// Check for unquoted `;` or `&&` (compound command separators) in the body.
/// This is a lighter check than `has_unquoted_control` -- it only looks for
/// the separators we can handle via `try_rewrite_compound`.
fn has_unquoted_semicolon_or_amp(cmd: &str) -> bool {
    let mut quote: Option<char> = None;
    let mut prev_amp = false;
    for c in cmd.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '\'' || c == '"' => quote = Some(c),
            None if c == ';' => return true,
            None if c == '&' => {
                if prev_amp {
                    return true; // `&&`
                }
                prev_amp = true;
                continue;
            }
            None => {}
        }
        prev_amp = false;
    }
    false
}

/// Split a compound command on unquoted `;` and `&&`, try to rewrite each
/// segment, and reassemble. Returns the full reassembled command if ANY
/// segment was rewritten, or None if no segment matched.
fn try_rewrite_compound(
    body: &str,
    effective_cwd: &Path,
    root_dir: &Path,
    trimmed: &str,
) -> Option<String> {
    let segments = split_compound(body);
    if segments.len() <= 1 {
        return None; // not actually compound -- fall through to normal handling
    }
    let root = root_dir.display().to_string();
    let mut any_rewritten = false;
    let mut rewritten_segments: Vec<String> = Vec::with_capacity(segments.len());
    for (seg, sep) in segments {
        let seg_trimmed = seg.trim();
        if seg_trimmed.is_empty() {
            rewritten_segments.push(seg);
            if !sep.is_empty() {
                let last = rewritten_segments.last_mut().unwrap();
                last.push_str(&sep);
            }
            continue;
        }
        // Try pipeline-aware rewrite for this segment
        let rewritten = if let Some(pipe_idx) = first_unquoted_pipe(seg_trimmed) {
            let first_segment = seg_trimmed[..pipe_idx].trim();
            let rest = &seg_trimmed[pipe_idx + 1..];
            if let Some(rw) = try_rewrite_grep(first_segment, effective_cwd, root_dir) {
                Some(format!("{rw} |{rest}"))
            } else {
                try_rewrite_grep(seg_trimmed, effective_cwd, root_dir)
                    .or_else(|| try_rewrite_git_archaeology(seg_trimmed, &root))
            }
        } else {
            try_rewrite_grep(seg_trimmed, effective_cwd, root_dir)
                .or_else(|| try_rewrite_git_archaeology(seg_trimmed, &root))
        };
        match rewritten {
            Some(rw) => {
                any_rewritten = true;
                rewritten_segments.push(rw);
            }
            None => {
                rewritten_segments.push(seg);
            }
        }
        // Re-attach separator with proper spacing
        if !sep.is_empty() {
            let last = rewritten_segments.last_mut().unwrap();
            last.push(' ');
            last.push_str(&sep);
            last.push(' ');
        }
    }
    if !any_rewritten {
        return None;
    }
    let result = rewritten_segments.join("");
    // Re-attach the cd prefix if we stripped one
    if body.len() != trimmed.len() {
        let cd_prefix = &trimmed[..trimmed.len() - body.len()];
        return Some(format!("{cd_prefix}{result}"));
    }
    Some(result)
}

/// Split a command on unquoted `;` and `&&`, returning (segment, separator)
/// pairs. The separator is the text that followed the segment (`;`, `&&`,
/// or empty for the last segment). Quote-aware -- separators inside quotes
/// are not split.
fn split_compound(s: &str) -> Vec<(String, String)> {
    let mut result = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match quote {
            Some(q) if c == q => {
                quote = None;
                current.push(c);
            }
            Some(_) => current.push(c),
            None if c == '\'' || c == '"' => {
                quote = Some(c);
                current.push(c);
            }
            None if c == ';' => {
                result.push((current.clone(), ";".to_string()));
                current.clear();
            }
            None if c == '&' && i + 1 < chars.len() && chars[i + 1] == '&' => {
                result.push((current.clone(), "&&".to_string()));
                current.clear();
                i += 1; // skip the second &
            }
            None => current.push(c),
        }
        i += 1;
    }
    if !current.is_empty() || result.is_empty() {
        result.push((current, String::new()));
    }
    result
}

/// Strip a leading `cd <dir> && ` prefix from a command, returning the
/// effective cwd (original cwd + cd target) and the remaining body. If
/// there's no cd prefix, returns (original_cwd, original_cmd).
fn strip_cd_prefix<'a>(cmd: &'a str, cwd: &Path) -> (PathBuf, &'a str) {
    let trimmed = cmd.trim();
    if !trimmed.starts_with("cd ") {
        return (cwd.to_path_buf(), cmd);
    }
    // Find the first unquoted `&&` after the cd.
    let rest_after_cd = &trimmed[3..];
    let amp_idx = match find_unquoted_double_amp(rest_after_cd) {
        Some(i) => i,
        None => return (cwd.to_path_buf(), cmd),
    };
    let dir_str = rest_after_cd[..amp_idx].trim();
    // Strip quotes from the directory.
    let dir_str = dir_str
        .trim_matches(|c| c == '\'' || c == '"')
        .trim();
    let new_cwd = if dir_str.starts_with('/') {
        PathBuf::from(dir_str)
    } else {
        cwd.join(dir_str)
    };
    let body = rest_after_cd[amp_idx + 2..].trim_start();
    // Return the body with a reference into the original string.
    // Find where body starts in the original cmd.
    let body_offset = cmd.len() - body.len();
    let body_ref = &cmd[body_offset..];
    (new_cwd, body_ref)
}

/// Find the byte index of the first unquoted `&&` in the string.
fn find_unquoted_double_amp(s: &str) -> Option<usize> {
    let mut quote: Option<char> = None;
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i + 1 < chars.len() {
        let c = chars[i];
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '\'' || c == '"' => quote = Some(c),
            None if c == '&' && chars[i + 1] == '&' => {
                return Some(s.char_indices().nth(i).map(|(idx, _)| idx).unwrap_or(0));
            }
            None => {}
        }
        i += 1;
    }
    None
}

/// Flags that consume a following value (or an attached `=value`), so they
/// must be skipped when locating the search pattern.
const VALUE_FLAGS: &[&str] = &[
    "-A", "-B", "-C", "-m", "-g", "-t", "-f", "--include", "--exclude",
    "--glob", "--type", "-d", "--max-depth",
];

/// Value-consuming flags that also change the match count in ways `pixel
/// search` can't reproduce. Their presence makes a rewrite non-equivalent,
/// so the command falls through to the original. File-filter flags
/// (`--include`/`--exclude`/`--glob`/`--type`) are NOT here — we drop them
/// and search a superset (see `search_can_replace`).
const SCOPE_FLAGS: &[&str] = &[
    "-m",
];

/// Check for unquoted control operators EXCEPT pipe (`|`) and redirects
/// (`>`, `<`). Pipes are handled separately by [`first_unquoted_pipe`].
/// Redirects (`2>/dev/null`, `> out.txt`) are common in grep commands and
/// don't change the command structure — the guard can safely rewrite the
/// grep part and leave the redirect in place. `&&` is handled by
/// [`strip_cd_prefix`] which strips a leading `cd X &&` before this check.
fn has_unquoted_control(cmd: &str) -> bool {
    let mut quote: Option<char> = None;
    let mut prev_amp = false;
    for c in cmd.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '\'' || c == '"' => quote = Some(c),
            None if c == '&' => {
                // Single `&` (background) is control; `&&` is handled by
                // strip_cd_prefix for the leading cd case. A `&&` in the
                // middle of the body (after cd strip) IS control.
                if prev_amp {
                    return true; // `&&` in the body
                }
                prev_amp = true;
                continue;
            }
            None if c == ';' || c == '\n' => return true,
            None => {}
        }
        prev_amp = false;
    }
    false
}

/// Find the byte index of the first unquoted pipe (`|`) in the command, or
/// None if there are no unquoted pipes. Used to split a pipeline into
/// segments so the first grep/rg segment can be rewritten to `pixel search`
/// while keeping the rest of the pipe intact.
fn first_unquoted_pipe(cmd: &str) -> Option<usize> {
    let mut quote: Option<char> = None;
    let mut prev_was_pipe = false;
    for (i, c) in cmd.char_indices() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '\'' || c == '"' => quote = Some(c),
            None if c == '|' => {
                // Skip `||` (logical OR) — only split on a single `|` pipe.
                if prev_was_pipe {
                    prev_was_pipe = false;
                    continue;
                }
                // Look ahead: is the next char also `|`? Then it's `||`.
                if cmd[i + 1..].starts_with('|') {
                    prev_was_pipe = true;
                    continue;
                }
                return Some(i);
            }
            None => {}
        }
        prev_was_pipe = false;
    }
    None
}

/// Single-quote `s` for shell interpolation, leaving it bare when it is
/// already shell-safe (so common roots like `/repo` stay readable).
fn shell_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    if s.chars().all(|c| {
        c.is_ascii_alphanumeric()
            || matches!(c, '/' | '.' | '_' | '-' | ':' | '=' | '+' | '@' | '~')
    }) {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Parse a grep/rg command into (pattern, path-scope args, unsupported
/// flags). Returns None if the command isn't a grep-style search.
/// Splits combined short flags (e.g. `-rl` → `-r` `-l`) to catch
/// unsupported flags that would otherwise slip through.
fn parse_grep(cmd: &str) -> Option<(String, Vec<String>, Vec<String>)> {
    let mut tokens = simple_tokenize(cmd);
    if tokens.is_empty() {
        return None;
    }
    // Strip shell wrapper prefixes (rtk, command, builtin) so `command grep`
    // and `builtin grep` are properly intercepted, matching bypass_advisory_lines.
    while matches!(tokens.first().map(String::as_str), Some("rtk") | Some("command") | Some("builtin")) {
        tokens.remove(0);
        if tokens.is_empty() {
            return None;
        }
    }
    // Normalize the binary name: strip path prefix so `/usr/bin/grep`,
    // `/bin/grep`, `/usr/local/bin/rg` etc. all match their base names.
    // Agents evade the guard by using absolute paths — this closes that bypass.
    let bin_raw = tokens[0].as_str();
    let bin = std::path::Path::new(bin_raw)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(bin_raw);
    if !matches!(bin, "rg" | "grep" | "egrep" | "fgrep") {
        return None;
    }
    // Expand combined short flags: -rl → -r -l, -rc → -r -c, -rn → -r -n
    let mut expanded: Vec<String> = Vec::new();
    for t in tokens {
        if t.starts_with('-') && !t.starts_with("--") && t.len() > 2 {
            // This is a combined short flag like -rl, -rc, -rn
            let first = &t[0..2]; // -r
            for c in t[2..].chars() {
                expanded.push(format!("{}{}", first, c));
            }
        } else {
            expanded.push(t);
        }
    }
    tokens = expanded;

    let unsupported_flags = [
        "-l", "--files-with-matches", "-c", "--count", "-v", "--invert",
        "-o", "--only-matching",
    ];
    let mut unsupported: Vec<String> = tokens[1..]
        .iter()
        .filter(|t| unsupported_flags.contains(&t.as_str()))
        .cloned()
        .collect();
    // Locate the pattern, skipping value-consuming flags and their values.
    let mut i = 1;
    let mut pattern: Option<String> = None;
    let mut pattern_idx = 0;
    while i < tokens.len() {
        let t = &tokens[i];
        if t == "-e" {
            pattern = tokens.get(i + 1).cloned();
            pattern_idx = i + 1;
            break;
        }
        if let Some(p) = t.strip_prefix("--regexp=") {
            pattern = Some(p.to_string());
            pattern_idx = i;
            break;
        }
        if t.starts_with('-') {
            if t.starts_with("--") && t.contains('=') {
                let base = t.split('=').next().unwrap_or(t);
                if SCOPE_FLAGS.contains(&base) {
                    unsupported.push(base.to_string());
                }
                i += 1; // self-contained --flag=value
                continue;
            }
            if VALUE_FLAGS.contains(&t.as_str()) {
                if SCOPE_FLAGS.contains(&t.as_str()) {
                    unsupported.push(t.clone());
                }
                i += 2; // flag + its value
                continue;
            }
            if t.len() > 2 && !t.starts_with("--") {
                let flag = &t[..2];
                if VALUE_FLAGS.contains(&flag) {
                    if SCOPE_FLAGS.contains(&flag) {
                        unsupported.push(flag.to_string());
                    }
                    i += 1; // short flag with attached value, e.g. -A5
                    continue;
                }
            }
            i += 1;
            continue;
        }
        pattern = Some(t.clone());
        pattern_idx = i;
        break;
    }
    let pattern = pattern?;
    // Collect paths AFTER the pattern, skipping value-consuming flags AND
    // their values (e.g. `-A 25` — `25` is not a path). Without this,
    // `grep -n "x" -A 25 file.rs` sees paths=["25", "file.rs"] (len 2) and
    // try_rewrite_grep bails on multi-path — the grep never gets rewritten
    // to pixel search.
    let mut paths: Vec<String> = Vec::new();
    let mut j = pattern_idx + 1;
    while j < tokens.len() {
        let t = &tokens[j];
        if t.starts_with('-') {
            // Self-contained --flag=value
            if t.starts_with("--") && t.contains('=') {
                j += 1;
                continue;
            }
            // Value-consuming flag: skip flag + its value
            if VALUE_FLAGS.contains(&t.as_str()) {
                j += 2;
                continue;
            }
            // Short flag with attached value, e.g. -A5
            if t.len() > 2 && !t.starts_with("--") {
                let flag = &t[..2];
                if VALUE_FLAGS.contains(&flag) {
                    j += 1;
                    continue;
                }
            }
            // Regular flag — skip just the flag
            j += 1;
            continue;
        }
        // Non-flag token — it's a path
        paths.push(t.clone());
        j += 1;
    }
    Some((pattern, paths, unsupported))
}

/// Shared equivalence predicate: can a grep-style search be transparently
/// replaced by `pixel search`? Returns the pixel command (root already
/// interpolated) if equivalent, or None if it can't be expressed. pixel
/// search is regex-based, so any pattern is expressible; only
/// output-modifying flags we can't honor fall through.
///
/// `--include`/`--exclude`/`--glob`/`--type` are file-filter flags that
/// `pixel search` doesn't support yet. We rewrite anyway and DROP them —
/// `pixel search` searches all code files (a superset of `--include`), and
/// the downstream pipeline (`| grep -v ...`) usually filters the rest.
/// This is a deliberate superset rewrite: more results, but never fewer,
/// and the agent can refine.
fn search_can_replace(pattern: &str, flags: &[String], root: &str) -> Option<String> {
    // Flags that change OUTPUT semantics in ways we can't represent.
    // File-filter flags (--include/--exclude/--glob/--type) are NOT here —
    // we drop them and search a superset.
    let unsupported_flags = [
        "-l", "--files-with-matches", "-c", "--count", "-v", "--invert",
        "-o", "--only-matching",
        "-m", "--max-count",
    ];
    if flags.iter().any(|f| unsupported_flags.contains(&f.as_str())) {
        return None;
    }
    let escaped = pattern.replace('\'', "'\\''");
    Some(format!(
        "pixel search '{}' {} --context 5",
        escaped,
        shell_quote(root)
    ))
}

/// Rewrite `rg PATTERN` / `grep PATTERN` → `pixel search PATTERN --context 5`
///
/// A single explicit path argument is preserved as the pixel search scope,
/// but only when it actually exists (file or directory) and lives inside
/// the indexed repo — rewriting a grep of `/etc/hosts` (or a typo'd path)
/// into a pixel search would silently change semantics. Multiple paths
/// can't be expressed as one pixel root, so they fall through unrewritten.
fn try_rewrite_grep(cmd: &str, cwd: &Path, root: &Path) -> Option<String> {
    // Strip trailing redirects (2>/dev/null, >file, <file) — they don't
    // change the search semantics, just I/O. The rewritten pixel command
    // doesn't need them (pixel search doesn't write to stderr in a way
    // that needs suppressing). Keep the redirect in the output so the
    // agent's intent is preserved.
    let (cmd_clean, redirect_suffix) = strip_redirects(cmd);
    let (pattern, paths, unsupported) = parse_grep(&cmd_clean)?;
    let scope = match paths.len() {
        0 => root.display().to_string(),
        1 => {
            let token = paths.into_iter().next().unwrap();
            let resolved = resolve(&token, cwd)?;
            if !resolved.is_file() && !resolved.is_dir() {
                return None;
            }
            let canon_root = canonical(root);
            if resolved != canon_root && !resolved.starts_with(&canon_root) {
                return None;
            }
            token
        }
        _ => return None,
    };
    let rewritten = search_can_replace(&pattern, &unsupported, &scope)?;
    if redirect_suffix.is_empty() {
        Some(rewritten)
    } else {
        Some(format!("{rewritten} {redirect_suffix}"))
    }
}

/// Strip trailing I/O redirects from a command segment. Returns (clean_cmd,
/// redirect_suffix). Handles `2>/dev/null`, `>file`, `2>file`, `<file`,
/// `&>file`, `1>file`. Only strips from the end — redirects in the middle
/// of a pipeline are handled by the pipe splitter before this runs.
fn strip_redirects(cmd: &str) -> (String, String) {
    let tokens = simple_tokenize(cmd);
    if tokens.is_empty() {
        return (cmd.to_string(), String::new());
    }
    // Scan from the end for redirect tokens. A redirect token is one that
    // starts with a digit followed by `>`, or starts with `>`, `<`, or `&>`.
    // The token may be attached to the filename (e.g. `2>/dev/null`) or
    // separate (e.g. `2>` `/dev/null`).
    let mut redirect_start = tokens.len();
    let mut i = tokens.len();
    while i > 0 {
        i -= 1;
        let t = &tokens[i];
        // `2>/dev/null` or `>file` or `&>file` — single token with redirect+target
        if t.starts_with("2>") || t.starts_with("1>") || t.starts_with("&>")
            || t.starts_with('>') || t.starts_with('<')
        {
            redirect_start = i;
            continue;
        }
        // `2>` or `>` or `<` as a separate token — consumes the next token as filename
        if (t == "2>" || t == "1>" || t == "&>" || t == ">" || t == "<")
            && i + 1 < tokens.len()
        {
            redirect_start = i;
            continue;
        }
        // Non-redirect token — stop scanning
        break;
    }
    if redirect_start == tokens.len() {
        return (cmd.to_string(), String::new());
    }
    let clean = tokens[..redirect_start].join(" ");
    let redirect = tokens[redirect_start..].join(" ");
    (clean, redirect)
}

/// Rewrite `git log` archaeology to `pixel excavate` — but ONLY when the
/// pixel command is an exact equivalent. The original command must carry
/// nothing but ONE search term (`-S <term>` / `-Sterm` / `-G <term>` /
/// `-Gterm` / `--grep=<term>`) and optionally ONE pathspec after `--`.
/// Anything the rewrite can't represent — `--author`, `-n`/counts, rev
/// ranges, display flags, bare revs, multiple pathspecs — falls through to
/// the original command unchanged (fail open: a non-equivalent substitute
/// is worse than no guard).
fn try_rewrite_git_archaeology(cmd: &str, root: &str) -> Option<String> {
    let mut tokens = simple_tokenize(cmd);
    // Strip shell wrapper prefixes (rtk, command, builtin) — same as parse_grep.
    while matches!(tokens.first().map(String::as_str), Some("rtk") | Some("command") | Some("builtin")) {
        tokens.remove(0);
    }
    if tokens.len() < 3 || tokens[0] != "git" || tokens[1] != "log" {
        return None;
    }
    let mut phrase: Option<String> = None;
    let mut pathspecs: Vec<String> = Vec::new();
    let mut after_dashdash = false;
    let mut i = 2;
    while i < tokens.len() {
        let t = &tokens[i];
        if after_dashdash {
            pathspecs.push(t.clone());
            i += 1;
            continue;
        }
        if t == "--" {
            after_dashdash = true;
            i += 1;
            continue;
        }
        if let Some(p) = t.strip_prefix("--grep=") {
            if phrase.is_some() || p.is_empty() {
                return None;
            }
            phrase = Some(p.to_string());
            i += 1;
            continue;
        }
        if t == "-S" || t == "-G" {
            if phrase.is_some() {
                return None;
            }
            phrase = Some(tokens.get(i + 1)?.clone());
            i += 2;
            continue;
        }
        if let Some(p) = t.strip_prefix("-S").or_else(|| t.strip_prefix("-G")) {
            if phrase.is_some() || p.is_empty() {
                return None;
            }
            phrase = Some(p.to_string());
            i += 1;
            continue;
        }
        // Any other flag, rev, or range makes the rewrite non-equivalent.
        return None;
    }
    let phrase = phrase?;
    if pathspecs.len() > 1 {
        return None;
    }
    let escaped = phrase.replace('\'', "'\\''");
    let mut out = format!("pixel excavate --phrase '{}' {}", escaped, shell_quote(root));
    if let Some(p) = pathspecs.first() {
        out.push_str(&format!(" --file {}", shell_quote(p)));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Create a unique scratch dir (with a `src/` subdir) acting as the
    /// indexed repo root for path-validation tests. Returns the
    /// canonicalized root so `starts_with` comparisons are stable on
    /// platforms where the temp dir is a symlink (macOS).
    fn scratch_repo(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("pixel-guard-{}-{}", name, std::process::id()));
        std::fs::create_dir_all(root.join("src")).unwrap();
        canonical(&root)
    }

    #[test]
    fn rewrite_rg_simple_pattern() {
        let cmd = "rg GUARD_MATCHER";
        let rewritten = try_rewrite_grep(cmd, Path::new("/repo"), Path::new("/repo"));
        assert_eq!(
            rewritten,
            Some("pixel search 'GUARD_MATCHER' /repo --context 5".to_string())
        );
    }

    #[test]
    fn rewrite_grep_simple_pattern() {
        let repo = scratch_repo("dot-path");
        let cmd = "grep -rn GUARD_MATCHER .";
        let rewritten = try_rewrite_grep(cmd, &repo, &repo);
        assert_eq!(
            rewritten,
            Some("pixel search 'GUARD_MATCHER' . --context 5".to_string())
        );
    }

    #[test]
    fn rewrite_regex_pattern() {
        // pixel search is regex-based, so regex patterns are expressible.
        let cmd = "rg \"foo.*bar\"";
        let rewritten = try_rewrite_grep(cmd, Path::new("/repo"), Path::new("/repo"));
        assert_eq!(
            rewritten,
            Some("pixel search 'foo.*bar' /repo --context 5".to_string())
        );
    }

    #[test]
    fn no_rewrite_unsupported_flags() {
        let cmd = "rg -l GUARD_MATCHER";
        let rewritten = try_rewrite_grep(cmd, Path::new("/repo"), Path::new("/repo"));
        assert!(rewritten.is_none(), "-l flag should not be rewritten");
    }

    #[test]
    fn no_rewrite_non_grep() {
        let cmd = "ls -la";
        let rewritten = try_rewrite_grep(cmd, Path::new("/repo"), Path::new("/repo"));
        assert!(rewritten.is_none());
    }

    #[test]
    fn rewrite_git_log_grep() {
        let cmd = "git log --grep=register_mcp";
        let rewritten = try_rewrite_git_archaeology(cmd, "/repo");
        assert_eq!(
            rewritten,
            Some("pixel excavate --phrase 'register_mcp' /repo".to_string())
        );
    }

    #[test]
    fn no_rewrite_git_log_without_search() {
        let cmd = "git log --oneline -10";
        let rewritten = try_rewrite_git_archaeology(cmd, "/repo");
        assert!(rewritten.is_none(), "plain git log should not be rewritten");
    }

    #[test]
    fn rewrite_git_log_s_with_single_pathspec() {
        let rewritten = try_rewrite_git_archaeology("git log -S term -- src/", "/repo");
        assert_eq!(
            rewritten,
            Some("pixel excavate --phrase 'term' /repo --file src/".to_string())
        );
        // Bare -S with no pathspec keeps the plain form.
        assert_eq!(
            try_rewrite_git_archaeology("git log -S term", "/repo"),
            Some("pixel excavate --phrase 'term' /repo".to_string())
        );
        // Attached form -Sterm.
        assert_eq!(
            try_rewrite_git_archaeology("git log -Sterm", "/repo"),
            Some("pixel excavate --phrase 'term' /repo".to_string())
        );
    }

    #[test]
    fn no_rewrite_git_log_unrepresentable() {
        // Anything the excavate rewrite can't represent must fall through
        // to the original command (fail open), never a lossy substitute.
        for cmd in [
            "git log -S term --author=bob",
            "git log -S term -n 5",
            "git log -S term main..dev",
            "git log -S term v1.0",
            "git log -S term --oneline",
            "git log -S term -- src/ lib/",
            "git log -S term src/",
            "git log -S term -G other",
        ] {
            assert!(
                try_rewrite_git_archaeology(cmd, "/repo").is_none(),
                "`{cmd}` is not exactly representable and must not be rewritten"
            );
        }
    }

    #[test]
    fn advisory_json_is_non_blocking() {
        let v = advisory_json("note text");
        assert_eq!(v["systemMessage"], "note text");
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PreToolUse");
        assert_eq!(v["hookSpecificOutput"]["additionalContext"], "note text");
        assert!(
            v["hookSpecificOutput"].get("permissionDecision").is_none(),
            "advisory must not carry a permissionDecision (neither deny nor auto-allow)"
        );
        assert!(v.get("decision").is_none());
    }

    #[test]
    fn scoping_outside_manifest_is_advisory_not_deny() {
        let repo = scratch_repo("advisory-scope");
        let a = repo.join("src").join("a.rs");
        let c = repo.join("src").join("c.rs");
        for f in [&a, &c] {
            std::fs::write(f, "x").unwrap();
        }
        write_manifest(
            &repo,
            &serde_json::json!({
                "version": 2,
                "tasks": [
                    {"id": "t", "task": "the task", "created_unix": now_unix(),
                     "targets": [{"path": "src/a.rs", "tier": "P0"}]},
                ],
            })
            .to_string(),
        );
        let m = load_manifest(&repo).unwrap();
        assert!(!allowed(&c, &m), "c.rs is outside the manifest");
        let msg = scoping_advisory_lines(&c, &m).join("\n");
        assert!(msg.contains("advisory"), "must be phrased as advisory: {msg}");
        assert!(msg.contains("src/c.rs"), "must name the file: {msg}");
        assert!(msg.contains("pixel targets"), "must suggest re-scoping: {msg}");
        assert!(!msg.contains("BLOCKED"), "must not read as a deny: {msg}");
        assert!(!msg.contains("PIXEL_TARGETS_GUARD"), "no bypass ad: {msg}");
    }

    #[test]
    fn mandate_and_index_advisories_are_non_blocking_text() {
        let repo = scratch_repo("advisory-mandate");
        let f = repo.join("src").join("a.rs");
        std::fs::write(&f, "x").unwrap();
        let msg = mandate_advisory_lines(&f, &repo).join("\n");
        assert!(msg.contains("advisory") && !msg.contains("BLOCKED"), "{msg}");
        assert!(msg.contains("pixel targets"), "{msg}");
        let msg = expired_manifest_advisory_lines(&repo).join("\n");
        assert!(msg.contains("expired") && !msg.contains("BLOCKED"), "{msg}");
        assert!(!msg.contains("PIXEL_TARGETS_GUARD"), "{msg}");
    }

    #[test]
    fn manifest_all_expired_reports_expired_state() {
        let repo = scratch_repo("expired-state");
        write_manifest(
            &repo,
            &serde_json::json!({
                "version": 2,
                "tasks": [
                    {"id": "old", "task": "stale", "created_unix": now_unix() - MANIFEST_MAX_AGE_SECS - 10,
                     "targets": [{"path": "src/a.rs", "tier": "P0"}]},
                ],
            })
            .to_string(),
        );
        assert!(matches!(load_manifest_state(&repo), ManifestState::Expired));
        let missing = scratch_repo("expired-state-missing");
        assert!(matches!(load_manifest_state(&missing), ManifestState::Absent));
    }

    #[test]
    fn git_pull_passes_through_and_is_not_rewritten() {
        // Raw `git pull` is no longer blocked; it must not be transparently
        // rewritten either.
        let repo = Path::new("/repo");
        assert!(bash_deny_lines("git pull", Some(repo)).is_none());
        assert!(bash_deny_lines("git pull upstream main", Some(repo)).is_none());
        assert!(bash_deny_lines("git pull --rebase origin main", Some(repo)).is_none());
        assert!(try_rewrite_bash("git pull", repo).is_none());
        assert!(try_rewrite_bash("git pull upstream main", repo).is_none());
    }

    #[test]
    fn no_deny_git_status() {
        let repo = Path::new("/repo");
        assert!(bash_deny_lines("git status", Some(repo)).is_none());
        assert!(try_rewrite_bash("git status", Path::new("/tmp")).is_none());
    }

    #[test]
    fn substituted_destructive_command_still_denied() {
        // The substitution bail must NOT let destructive commands through:
        // denies run before (and independent of) the conservative skip.
        let repo = Path::new("/repo");
        assert!(
            bash_deny_lines("git reset --hard $(git rev-parse HEAD~1)", Some(repo)).is_some(),
            "substitution must not bypass the destructive deny"
        );
        assert!(
            bash_deny_lines("git clean -fd `git rev-parse --show-toplevel`", Some(repo)).is_some()
        );
    }

    #[test]
    fn destructive_set_expanded() {
        let repo = Path::new("/repo");
        let denied = [
            "git reset --hard",
            "git reset --keep HEAD~2",
            "git clean -f",
            "git clean -fd",
            "git clean -fdx",
            "git clean -df",
            "git clean --force",
            "git checkout -f main",
            "git checkout --force main",
            "git checkout HEAD~1 -- src/lib.rs",
            "git restore --source HEAD~1 src/lib.rs",
            "git restore --source=HEAD~1 src/lib.rs",
            "git stash drop",
            "git stash clear",
            "git stash -q drop",
            "git checkout -- src/lib.rs",
            "git branch -D feature",
            "git push --force",
            "git push -f origin main",
        ];
        for cmd in denied {
            assert!(
                bash_deny_lines(cmd, Some(repo)).is_some(),
                "`{cmd}` must be denied"
            );
        }
        // NOT destructive-denied. Some of these (pushes, checkout -b) are
        // deliberately picked up by the SUBSTITUTE tier instead — asserted
        // in the substitute_* tests below — but they must never carry the
        // destructive tier's verdict.
        let not_destructive = [
            "git push --force-with-lease",
            "git push --force-with-lease=main origin main",
            "git push --force-if-includes --force-with-lease",
            "git push origin main",
            "git clean -n",
            "git checkout main",
            "git checkout -b feature",
            "git stash",
            "git stash list",
            "git stash pop",
            "git branch -d merged",
            "git branch --list",
            "git reset --soft HEAD~1",
            "git restore --staged src/lib.rs",
            // Conflict-side selection: idiomatic resolution commands, not
            // historical restores — git errors on non-conflicted paths.
            "git checkout --theirs -- src/lib.rs",
            "git checkout --ours -- src/lib.rs",
            "git checkout --theirs src/lib.rs",
            "git stash push -m 'drop'",
        ];
        for cmd in not_destructive {
            assert!(
                bash_deny_lines(cmd, Some(repo)).is_none(),
                "`{cmd}` must not be destructive-denied"
            );
        }
    }

    #[test]
    fn destructive_deny_robust_to_flag_order_and_segments() {
        let repo = Path::new("/repo");
        assert!(bash_deny_lines("git -C /repo reset --hard", Some(repo)).is_some());
        assert!(bash_deny_lines("git clean -d -f", Some(repo)).is_some());
        assert!(
            bash_deny_lines("git status && git reset --hard HEAD~1", Some(repo)).is_some(),
            "destructive segment in a compound command must be denied"
        );
    }

    #[test]
    fn reset_hard_branch_suggests_checkout_b() {
        // `git reset --hard <branch>` should suggest `git checkout -B`
        // instead of `pixel rescue` — it's a repoint, not data loss.
        let repo = scratch_repo("reset-branch");
        let lines = bash_deny_lines("git reset --hard history-rewrite", Some(&repo))
            .expect("branch-targeted reset --hard must still be denied");
        let msg = lines.join("\n");
        assert!(msg.contains("git checkout -B"), "should suggest checkout -B: {msg}");
        assert!(!msg.contains("pixel rescue"), "should NOT suggest rescue for branch repoint: {msg}");
    }

    #[test]
    fn reset_hard_head_tilde_still_suggests_rescue() {
        // `git reset --hard HEAD~N` is real data loss — keep rescue suggestion.
        let repo = Path::new("/repo");
        let lines = bash_deny_lines("git reset --hard HEAD~3", Some(repo))
            .expect("HEAD~N reset must be denied");
        let msg = lines.join("\n");
        assert!(msg.contains("pixel rescue"), "should suggest rescue for HEAD~N: {msg}");
        assert!(!msg.contains("git checkout -B"), "should NOT suggest checkout -B for HEAD~N: {msg}");
    }

    #[test]
    fn reset_hard_raw_oid_still_suggests_rescue() {
        // `git reset --hard <oid>` is real data loss — keep rescue suggestion.
        let repo = Path::new("/repo");
        let lines = bash_deny_lines("git reset --hard abc123def456789", Some(repo))
            .expect("raw OID reset must be denied");
        let msg = lines.join("\n");
        assert!(msg.contains("pixel rescue"), "should suggest rescue for raw OID: {msg}");
    }

    #[test]
    fn reset_hard_head_alone_still_suggests_rescue() {
        // `git reset --hard HEAD` discards working tree changes — rescue.
        let repo = Path::new("/repo");
        let lines = bash_deny_lines("git reset --hard HEAD", Some(repo))
            .expect("bare HEAD reset must be denied");
        let msg = lines.join("\n");
        assert!(msg.contains("pixel rescue"), "should suggest rescue for bare HEAD: {msg}");
    }

    #[test]
    fn is_branch_like_classification() {
        // Branch names
        assert!(is_branch_like("main"));
        assert!(is_branch_like("feature/rewrite"));
        assert!(is_branch_like("history-rewrite"));
        assert!(is_branch_like("v1.2.3"));
        // Relative refs — NOT branch-like
        assert!(!is_branch_like("HEAD"));
        assert!(!is_branch_like("HEAD~1"));
        assert!(!is_branch_like("HEAD~3"));
        assert!(!is_branch_like("HEAD^"));
        assert!(!is_branch_like("HEAD@{1}"));
        // Raw OIDs — NOT branch-like
        assert!(!is_branch_like("abc123def4567890123456789012345678901234567")); // 40 hex
        assert!(!is_branch_like("abc1234")); // 7 hex (short OID)
        assert!(!is_branch_like(""));
    }

    #[test]
    fn quoted_destructive_text_not_denied() {
        // A destructive command mentioned inside a quoted argument is data,
        // not an executed command — the tokenizer folds it into one token.
        let repo = Path::new("/repo");
        assert!(
            bash_deny_lines("git commit -m 'do not git reset --hard here'", Some(repo)).is_none()
        );
        // Separators INSIDE quotes must not split the argument into a
        // phantom segment (the raw-string pre-split bug): a semicolon or
        // newline in a commit message is still data.
        assert!(
            bash_deny_lines("git commit -m 'step 1; git reset --hard later'", Some(repo)).is_none()
        );
        assert!(
            bash_deny_lines("pixel publish --message \"cleanup | git clean -fd equivalent\" .", Some(repo)).is_none()
        );
        assert!(
            git_mutation_substitute_lines(
                "pixel publish --files a.rs --message \"fix(guard): pass git add through\ngit add now allowed mid-sequencer\" --request-id x .",
                Some(repo),
                Path::new("/repo")
            )
            .is_none(),
            "a multi-line --message mentioning `git add` must not deny pixel's own substitute"
        );
        // …but a genuinely unquoted chained invocation is still caught.
        assert!(
            bash_deny_lines("pixel search 'x' . && git reset --hard", Some(repo)).is_some()
        );
    }

    #[test]
    fn no_deny_outside_indexed_repo() {
        assert!(bash_deny_lines("git reset --hard", None).is_none());
    }

    #[test]
    fn advisory_messages_never_advertise_bypass() {
        let repo = Path::new("/repo");
        for cmd in [
            "git reset --hard",
            "git clean -fd",
            "git push --force",
            "git stash drop",
        ] {
            let msg = non_blocking_advisory_lines(&bash_deny_lines(cmd, Some(repo)).unwrap()).join("\n");
            assert!(
                !msg.contains("PIXEL_TARGETS_GUARD"),
                "advisory for `{cmd}` must not advertise the kill switch: {msg}"
            );
            assert!(!msg.contains("BLOCKED"), "must be non-blocking: {msg}");
            assert!(msg.contains("Proceeding"), "must allow the original command: {msg}");
        }
        let mut input = serde_json::Map::new();
        input.insert("pattern".to_string(), Value::String("foo".to_string()));
        let grep_msg = grep_redirect_advisory_lines("foo", Path::new("/tmp"), &input).join("\n");
        assert!(!grep_msg.contains("PIXEL_TARGETS_GUARD"));
        assert!(!grep_msg.contains("BLOCKED"));
    }

    #[test]
    fn is_grep_tool_detects_pattern() {
        let mut input = serde_json::Map::new();
        input.insert("pattern".to_string(), Value::String("foo".to_string()));
        assert!(is_grep_tool("Grep", &input));
        assert!(!is_grep_tool("Bash", &input));
    }

    #[test]
    fn is_grep_tool_no_pattern_field() {
        let input = serde_json::Map::new();
        assert!(!is_grep_tool("Grep", &input));
    }

    #[test]
    fn rewrite_first_grep_segment_in_pipeline() {
        // Pipelines: the first grep/rg segment is rewritten to pixel search,
        // the rest of the pipe is preserved. This is the common agent pattern:
        // `grep -rln "pattern" ... | grep -v node_modules | sort | wc -l`
        let rewritten = try_rewrite_bash("rg foo | head -5", Path::new("/tmp"));
        assert!(rewritten.is_some(), "first grep segment in a pipeline should be rewritten");
        let cmd = rewritten.unwrap();
        assert!(cmd.starts_with("pixel search 'foo'"), "cmd was: {cmd}");
        assert!(cmd.contains("| head -5"), "rest of pipe must be preserved, cmd was: {cmd}");
    }

    #[test]
    fn pipeline_with_non_grep_first_segment_not_rewritten() {
        // If the first segment isn't grep/rg, don't touch the pipeline.
        let rewritten = try_rewrite_bash("cat foo.txt | grep bar", Path::new("/tmp"));
        assert!(rewritten.is_none(), "non-grep first segment must not be rewritten");
    }

    #[test]
    fn logical_or_not_treated_as_pipe() {
        // `||` is logical OR, not a pipe — must not be split.
        let rewritten = try_rewrite_bash("rg foo || echo failed", Path::new("/tmp"));
        assert!(rewritten.is_none(), "|| must not be treated as a pipe");
    }

    #[test]
    fn reject_control_flow_rewrite() {
        // Redirects (> <) are handled by strip_redirects, not control flow.
        // In a non-indexed directory, no rewrite applies.
        let non_idx = Path::new("/tmp/pixel-guard-no-idx-test-xyz");
        assert!(try_rewrite_bash("rg foo > out.txt", non_idx).is_none());
    }

    #[test]
    fn compound_command_rewrites_grep_segment() {
        // In an indexed dir, `rg foo && echo hi` should rewrite the rg segment
        // and keep the `&& echo hi` suffix. This is the new compound command
        // handling that replaces the old bail-on-control-flow behavior.
        let idx = scratch_repo("compound-idx");
        std::fs::create_dir_all(idx.join(".pixel")).unwrap();
        let rewritten = try_rewrite_bash("rg foo && echo hi", &idx);
        assert!(rewritten.is_some(), "compound command should rewrite grep segment");
        let rw = rewritten.unwrap();
        assert!(rw.contains("pixel search"), "should contain pixel search");
        assert!(rw.contains("echo hi"), "should preserve the echo suffix");
    }

    #[test]
    fn compound_command_semicolon_rewrites_grep_segment() {
        let idx = scratch_repo("compound-semi");
        std::fs::create_dir_all(idx.join(".pixel")).unwrap();
        let rewritten = try_rewrite_bash("rg foo; echo hi", &idx);
        assert!(rewritten.is_some(), "compound command with ; should rewrite grep segment");
        let rw = rewritten.unwrap();
        assert!(rw.contains("pixel search"), "should contain pixel search");
        assert!(rw.contains("echo hi"), "should preserve the echo suffix");
    }

    #[test]
    fn value_flag_skips_pattern() {
        // -A 5 consumes "5"; the pattern is "foo", not "5".
        let rewritten = try_rewrite_grep("grep -A 5 foo", Path::new("/repo"), Path::new("/repo"));
        assert_eq!(
            rewritten,
            Some("pixel search 'foo' /repo --context 5".to_string())
        );
    }

    #[test]
    fn regexp_equals_pattern() {
        let rewritten = try_rewrite_grep("grep --regexp=foo", Path::new("/repo"), Path::new("/repo"));
        assert_eq!(
            rewritten,
            Some("pixel search 'foo' /repo --context 5".to_string())
        );
    }

    #[test]
    fn rtk_grep_and_rg_rewritten() {
        let rewritten = try_rewrite_grep("rtk grep 'foo'", Path::new("/repo"), Path::new("/repo"));
        assert_eq!(
            rewritten,
            Some("pixel search 'foo' /repo --context 5".to_string())
        );

        let rewritten = try_rewrite_grep("rtk rg foo", Path::new("/repo"), Path::new("/repo"));
        assert_eq!(
            rewritten,
            Some("pixel search 'foo' /repo --context 5".to_string())
        );
    }

    #[test]
    fn path_prefixed_grep_rewritten() {
        // Agents evade guard by using /usr/bin/grep — parse_grep must normalize.
        let parsed = parse_grep("/usr/bin/grep -rn foo .");
        assert!(
            parsed.is_some(),
            "/usr/bin/grep should be recognized as grep after path normalization"
        );
        let (pat, _, unsupported) = parsed.unwrap();
        assert_eq!(pat, "foo");
        assert!(unsupported.is_empty());

        // /usr/local/bin/rg should also work
        let parsed = parse_grep("/usr/local/bin/rg bar .");
        assert!(
            parsed.is_some(),
            "/usr/local/bin/rg should be recognized as rg after path normalization"
        );
        let (pat, _, _) = parsed.unwrap();
        assert_eq!(pat, "bar");

        // /bin/grep should also normalize
        let parsed = parse_grep("/bin/grep -rn baz .");
        assert!(
            parsed.is_some(),
            "/bin/grep should be recognized as grep after path normalization"
        );
    }

    #[test]
    fn value_flag_after_pattern_does_not_eat_path() {
        // `grep -n "x" -A 25 file.rs` — the `25` after `-A` is NOT a path.
        // Without the fix, parse_grep collected paths=["25", "file.rs"] (len
        // 2) and try_rewrite_grep bailed on multi-path.
        let repo = scratch_repo("value-flag-path");
        let file = repo.join("src").join("file.rs");
        std::fs::write(&file, "fn x() {}\n").unwrap();
        let cmd = "grep -n \"fn x\" -A 25 src/file.rs";
        let rewritten = try_rewrite_grep(cmd, &repo, &repo);
        assert_eq!(
            rewritten,
            Some("pixel search 'fn x' src/file.rs --context 5".to_string())
        );
    }

    #[test]
    fn scope_flag_not_rewritten() {
        // -m changes the match count; pixel search can't honor it → no rewrite.
        let repo = Path::new("/repo");
        assert!(try_rewrite_grep("grep -m 5 foo", repo, repo).is_none());
    }

    #[test]
    fn file_filter_flags_rewritten_as_superset() {
        // --include/--glob/--type are file-filter flags that pixel search
        // doesn't support yet. We rewrite anyway and DROP them — pixel search
        // searches all code files (a superset), and downstream pipeline
        // filters handle the rest. The pattern must be correctly identified.
        let repo = Path::new("/repo");
        let rewritten = try_rewrite_grep("grep --include=*.rs foo", repo, repo);
        assert!(rewritten.is_some(), "--include should be rewritten as superset");
        assert!(rewritten.unwrap().contains("'foo'"), "pattern must be foo");

        let rewritten = try_rewrite_grep("grep --glob '*.rs' foo", repo, repo);
        assert!(rewritten.is_some(), "--glob should be rewritten as superset");

        // `rg --type rust foo` must not misparse "rust" as the pattern.
        let rewritten = try_rewrite_grep("rg --type rust foo", repo, repo);
        assert!(rewritten.is_some(), "--type should be rewritten as superset");
        assert!(rewritten.unwrap().contains("'foo'"), "pattern must be foo, not rust");

        let rewritten = try_rewrite_grep("rg -t rust foo", repo, repo);
        assert!(rewritten.is_some(), "-t should be rewritten as superset");
        assert!(rewritten.unwrap().contains("'foo'"), "pattern must be foo, not rust");
    }

    #[test]
    fn preserves_path_scope() {
        let repo = scratch_repo("path-scope");
        let rewritten = try_rewrite_grep("rg foo src/", &repo, &repo);
        assert_eq!(
            rewritten,
            Some("pixel search 'foo' src/ --context 5".to_string())
        );
    }

    #[test]
    fn nonexistent_path_not_rewritten() {
        let repo = scratch_repo("no-such-path");
        assert!(
            try_rewrite_grep("rg foo no/such/dir", &repo, &repo).is_none(),
            "a path that doesn't exist must not be silently rescoped"
        );
    }

    #[test]
    fn path_outside_repo_not_rewritten() {
        let repo = scratch_repo("outside");
        let outside = std::env::temp_dir();
        let cmd = format!("rg foo {}", outside.display());
        assert!(
            try_rewrite_grep(&cmd, &repo, &repo).is_none(),
            "a path outside the indexed repo must not be rewritten"
        );
    }

    #[test]
    fn multiple_paths_not_rewritten() {
        let repo = scratch_repo("multi-path");
        let rewritten = try_rewrite_grep("rg foo src/ lib/", &repo, &repo);
        assert!(rewritten.is_none(), "multiple roots can't be expressed");
    }

    #[test]
    fn quotes_root_with_space() {
        let rewritten = try_rewrite_grep("rg foo", Path::new("/my repo"), Path::new("/my repo"));
        assert_eq!(
            rewritten,
            Some("pixel search 'foo' '/my repo' --context 5".to_string())
        );
    }

    #[test]
    fn accepts_before_tool_event() {
        let empty = serde_json::json!({});
        assert!(is_guard_event(&empty, "PreToolUse"));
        assert!(is_guard_event(&empty, "BeforeTool"));
        assert!(!is_guard_event(&empty, "PostToolUse"));
    }

    #[test]
    fn accepts_cursor_shaped_payload_with_no_event_name() {
        // Cursor's preToolUse hook sends no `hook_event_name` at all —
        // verified against the installed cursor-agent bundle. The payload
        // shape itself (tool_name + tool_input, no event key) must count
        // as an implicit PreToolUse.
        let cursor_shaped = serde_json::json!({
            "tool_name": "Shell",
            "tool_input": {"command": "ls"},
            "cwd": "/tmp"
        });
        assert!(is_guard_event(&cursor_shaped, ""));
        // A payload with neither an event name nor the tool_name/tool_input
        // shape must NOT be treated as a guard event.
        let unrelated = serde_json::json!({"foo": "bar"});
        assert!(!is_guard_event(&unrelated, ""));
    }

    #[test]
    fn destructive_commands_are_not_transparently_rewritten() {
        // Safety advisories run before the rewrite path; a destructive git
        // command must remain the original command, not become a Pixel write.
        let rewritten = try_rewrite_bash("git reset --hard HEAD", Path::new("/tmp"));
        assert!(rewritten.is_none());
    }

    #[test]
    fn scoping_sees_grep_file_before_rewrite() {
        // Ordering guarantee: in run(), check_bash (which applies the
        // manifest scoping via single_reader_target) executes BEFORE any
        // rewrite attempt. This test proves the scoping detector still
        // extracts the file from exactly the kind of grep command the
        // rewriter would otherwise transform — so a manifest-blocked file
        // read via grep is blocked by scoping_block, never rewritten.
        let repo = scratch_repo("scope-order");
        let file = repo.join("src").join("secret.rs");
        std::fs::write(&file, "x").unwrap();
        let cmd = format!("grep foo {}", file.display());
        let detected = single_reader_target(&cmd, &repo);
        assert_eq!(detected, Some(canonical(&file)));
    }

    fn now_unix() -> u64 {
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
    }

    /// Write `text` as `<root>/.pixel/targets.json`.
    fn write_manifest(root: &Path, text: &str) {
        let dir = root.join(".pixel");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("targets.json"), text).unwrap();
    }

    #[test]
    fn manifest_v2_union_allows_file_from_either_task() {
        let repo = scratch_repo("v2-union");
        let a = repo.join("src").join("a.rs");
        let b = repo.join("src").join("b.rs");
        let c = repo.join("src").join("c.rs");
        for f in [&a, &b, &c] {
            std::fs::write(f, "x").unwrap();
        }
        let now = now_unix();
        write_manifest(
            &repo,
            &serde_json::json!({
                "version": 2,
                "tasks": [
                    {"id": "aaa", "task": "task A", "created_unix": now,
                     "targets": [{"path": "src/a.rs", "tier": "P0"}]},
                    {"id": "bbb", "task": "task B", "created_unix": now,
                     "targets": [{"path": "src/b.rs", "tier": "P0"}]},
                ],
            })
            .to_string(),
        );
        let m = load_manifest(&repo).expect("v2 manifest must load");
        assert_eq!(m.tasks.len(), 2);
        assert!(allowed(&a, &m), "file in task A must be allowed");
        assert!(
            allowed(&b, &m),
            "file listed only in task B must be allowed while task A is also active"
        );
        assert!(!allowed(&c, &m), "file in no task must be blocked");
    }

    #[test]
    fn manifest_v2_expired_task_dropped() {
        let repo = scratch_repo("v2-expiry");
        let a = repo.join("src").join("a.rs");
        let b = repo.join("src").join("b.rs");
        for f in [&a, &b] {
            std::fs::write(f, "x").unwrap();
        }
        let now = now_unix();
        write_manifest(
            &repo,
            &serde_json::json!({
                "version": 2,
                "tasks": [
                    {"id": "old", "task": "stale", "created_unix": now - MANIFEST_MAX_AGE_SECS - 10,
                     "targets": [{"path": "src/a.rs", "tier": "P0"}]},
                    {"id": "new", "task": "fresh", "created_unix": now,
                     "targets": [{"path": "src/b.rs", "tier": "P0"}]},
                ],
            })
            .to_string(),
        );
        let m = load_manifest(&repo).expect("fresh task keeps manifest alive");
        assert_eq!(m.tasks.len(), 1, "expired task must be dropped");
        assert!(!allowed(&a, &m), "expired task's file must not be allowed");
        assert!(allowed(&b, &m));
    }

    #[test]
    fn manifest_v2_all_expired_is_no_manifest() {
        let repo = scratch_repo("v2-all-expired");
        let now = now_unix();
        write_manifest(
            &repo,
            &serde_json::json!({
                "version": 2,
                "tasks": [
                    {"id": "old", "task": "stale", "created_unix": now - MANIFEST_MAX_AGE_SECS - 10,
                     "targets": [{"path": "src/a.rs", "tier": "P0"}]},
                ],
            })
            .to_string(),
        );
        assert!(load_manifest(&repo).is_none());
    }

    #[test]
    fn manifest_legacy_shape_still_read() {
        let repo = scratch_repo("legacy-shape");
        let a = repo.join("src").join("a.rs");
        let c = repo.join("src").join("c.rs");
        for f in [&a, &c] {
            std::fs::write(f, "x").unwrap();
        }
        write_manifest(
            &repo,
            &serde_json::json!({
                "version": 1,
                "task": "legacy task",
                "created_unix": now_unix(),
                "files": [{"path": "src/a.rs", "tier": "P0"}],
            })
            .to_string(),
        );
        let m = load_manifest(&repo).expect("legacy manifest must load");
        assert_eq!(m.tasks.len(), 1);
        assert_eq!(m.tasks[0].task, "legacy task");
        assert!(allowed(&a, &m));
        assert!(!allowed(&c, &m));
    }

    // --- SUBSTITUTE tier -------------------------------------------------

    fn sub(cmd: &str) -> Option<Vec<String>> {
        git_mutation_substitute_lines(cmd, Some(Path::new("/repo")), Path::new("/repo"))
    }

    /// Every substitute candidate carries the useful explanation and exact
    /// Pixel alternative. The hook converts the candidate to an advisory
    /// before emitting it.
    fn assert_substitute_contract(cmd: &str, substitute_fragment: &str) -> String {
        let msg = sub(cmd)
            .unwrap_or_else(|| panic!("`{cmd}` must be substitute-denied"))
            .join("\n");
        assert!(msg.contains("BLOCKED [PIXEL_SUBSTITUTE]"), "reason code missing for `{cmd}`: {msg}");
        assert!(
            msg.contains(substitute_fragment),
            "substitute for `{cmd}` must contain `{substitute_fragment}`: {msg}"
        );
        assert!(
            !msg.contains("PIXEL_GUARD_RAW_GIT=1"),
            "human-override env var must NOT be advertised in deny for `{cmd}`: {msg}"
        );
        msg
    }

    #[test]
    fn substitute_commit_with_message_parsed() {
        let msg = assert_substitute_contract(
            "git commit -m 'fix the parser'",
            "pixel publish",
        );
        assert!(msg.contains("--message 'fix the parser'"), "parsed -m must enrich the suggestion: {msg}");
        assert!(msg.contains("--request-id <id>"), "{msg}");
        // --message form and -am cluster parse too.
        let msg = assert_substitute_contract("git commit --message 'x y'", "pixel publish");
        assert!(msg.contains("--message 'x y'"), "{msg}");
        // A single safe word stays bare through shell_quote.
        let msg = assert_substitute_contract("git commit -am 'both words here'", "pixel publish");
        assert!(msg.contains("--message 'both words here'"), "{msg}");
        assert!(msg.contains("-a detected"), "-a must enrich the suggestion: {msg}");
    }

    #[test]
    fn substitute_commit_without_message_uses_placeholder() {
        let msg = assert_substitute_contract("git commit", "pixel publish");
        assert!(msg.contains("--message \"<msg>\""), "placeholder expected: {msg}");
        assert!(msg.contains("--files <file>"), "files placeholder expected: {msg}");
    }

    #[test]
    fn substitute_commit_pathspecs_become_files_flags() {
        let msg = assert_substitute_contract(
            "git commit -m fix src/a.rs src/b.rs",
            "pixel publish",
        );
        assert!(
            msg.contains("--files src/a.rs --files src/b.rs"),
            "each pathspec must be its own --files: {msg}"
        );
    }

    #[test]
    fn substitute_commit_amend_suggests_publish_amend() {
        let msg = assert_substitute_contract("git commit --amend -m better", "pixel publish --amend");
        assert!(msg.contains("--message better"), "{msg}");
    }

    #[test]
    fn substitute_push_plain_and_with_lease() {
        let msg = assert_substitute_contract("git push origin main", "pixel push origin main --request-id <id>");
        assert!(msg.contains("/repo"), "{msg}");
        // --force-with-lease is allowed by the destructive tier but IS
        // substitute-denied — pixel push carries the same lease semantics.
        assert!(bash_deny_lines("git push --force-with-lease origin main", Some(Path::new("/repo"))).is_none());
        assert_substitute_contract(
            "git push --force-with-lease origin main",
            "pixel push origin main --request-id <id>",
        );
        // No remote/refspec → placeholders.
        let msg = assert_substitute_contract("git push", "pixel push <remote> <refspec>");
        assert!(msg.contains("--request-id <id>"), "{msg}");
    }

    #[test]
    fn substitute_branch_creation() {
        assert_substitute_contract("git checkout -b feature/x", "pixel branch feature/x --request-id <id>");
        assert_substitute_contract("git switch -c feature/y", "pixel branch feature/y --request-id <id>");
        assert_substitute_contract("git switch --create feature/z", "pixel branch feature/z --request-id <id>");
    }

    #[test]
    fn substitute_rebase_suggests_reconcile() {
        let msg = assert_substitute_contract(
            "git rebase main",
            "pixel reconcile /repo --strategy rebase-if-clean --push auto",
        );
        assert!(msg.contains("merge-tree"), "{msg}");
        assert_substitute_contract("git rebase", "pixel reconcile /repo");
    }

    #[test]
    fn substitute_pass_throughs() {
        // Interactive/porcelain shapes pixel can't cover must NOT be denied.
        for cmd in [
            "git rebase -i HEAD~3",
            "git rebase --interactive main",
            "git rebase --continue",
            "git rebase --abort",
            "git rebase --skip",
            "git rebase --onto main topic feature",
            "git commit --interactive",
            "git commit -p",
            "git commit --patch",
            "git commit --fixup=abc123",
            "git push --tags",
            "git push origin --delete old-branch",
            "git push -d origin old-branch",
            "git push --mirror backup",
            "git push --all origin",
            "git checkout main",
            "git checkout -B feature",
            "git switch main",
            "git status",
            "git log --oneline",
            "git add -p",
            "git add --patch",
            "git add -i",
            "git add --interactive",
        ] {
            assert!(sub(cmd).is_none(), "`{cmd}` must pass through the substitute tier");
        }
    }

    #[test]
    fn substitute_add_with_pathspecs() {
        let msg = assert_substitute_contract(
            "git add src/a.rs src/b.rs",
            "pixel publish",
        );
        assert!(
            msg.contains("--files src/a.rs --files src/b.rs"),
            "each pathspec must be its own --files: {msg}"
        );
    }

    #[test]
    fn substitute_add_dot_suggests_enumerate() {
        let msg = assert_substitute_contract("git add .", "pixel publish");
        assert!(
            msg.contains("List each modified tracked file"),
            "`git add .` must suggest enumerating files: {msg}"
        );
    }

    #[test]
    fn substitute_add_all_variant() {
        for cmd in ["git add -A", "git add --all", "git add -u", "git add --update"] {
            let msg = assert_substitute_contract(cmd, "pixel publish");
            assert!(
                msg.contains("List each modified tracked file"),
                "`{cmd}` must suggest enumerating files: {msg}"
            );
        }
    }

    #[test]
    fn substitute_only_in_indexed_repo() {
        assert!(git_mutation_substitute_lines("git commit -m x", None, Path::new("/repo")).is_none());
    }

    #[test]
    fn substitute_advisory_keeps_suggestion_and_allows_original() {
        let lines = sub("git commit -m x").unwrap();
        let advisory = non_blocking_advisory_lines(&lines).join("\n");
        assert!(!advisory.contains("BLOCKED"), "must not read as a deny: {advisory}");
        assert!(advisory.contains("pixel-guard advisory"), "{advisory}");
        assert!(advisory.contains("pixel publish"), "suggestion must survive the downgrade: {advisory}");
        assert!(advisory.contains("Proceeding"), "the original git command must remain available: {advisory}");
    }

    #[test]
    fn substitute_runs_after_destructive_tier() {
        // Bare --force stays a destructive deny; it must never fall to the
        // softer substitute wording (check_bash consults bash_deny_lines
        // first, and the substitute tier's push arm can't even see it
        // in practice — but assert the destructive verdict directly).
        let repo = Path::new("/repo");
        let msg = bash_deny_lines("git push --force origin main", Some(repo)).unwrap().join("\n");
        assert!(msg.contains("destroy remote history"), "{msg}");
    }

    // --- transcript escalation ------------------------------------------

    #[test]
    fn zcode_store_is_flagged() {
        let store = transcript_store_hit("sqlite3 ~/.zcode/cli/db/db.sqlite 'select 1'");
        assert_eq!(store, Some(".zcode/cli/db"));
        let msg = transcript_archaeology_advisory_lines(store.unwrap()).join("\n");
        assert!(msg.contains("Advisory") && !msg.contains("BLOCKED"), "{msg}");
        assert!(msg.contains("pixel recall"), "{msg}");
    }

    #[test]
    fn unrelated_commands_hit_no_store() {
        assert!(transcript_store_hit("cargo test -p pixel").is_none());
        // A store path with no reading tool is not archaeology.
        assert!(transcript_store_hit("ls ~/.zcode/cli/db").is_none());
        assert!(transcript_store_hit("echo .zcode/cli/db").is_none());
    }

    #[test]
    fn grep_tool_gets_advisory_when_transparent_rewrite_is_unavailable() {
        // A Grep tool call carrying fields pixel search can't express
        // (glob/type/output_mode) must be allowed through with a non-blocking
        // advisory rather than a non-equivalent redirect.
        for field in ["glob", "type", "output_mode"] {
            let mut input = serde_json::Map::new();
            input.insert("pattern".to_string(), Value::String("foo".to_string()));
            input.insert(field.to_string(), Value::String("x".to_string()));
            let msg = grep_redirect_advisory_lines("foo", Path::new("/tmp"), &input).join("\n");
            assert!(!msg.contains("BLOCKED"), "Grep with `{field}` must not be blocked: {msg}");
            assert!(msg.contains("Proceeding"), "Grep with `{field}` must proceed: {msg}");
        }
    }

    #[test]
    fn grep_tool_gets_equivalent_command_as_advisory() {
        let repo = scratch_repo("grep-advisory");
        std::fs::create_dir_all(repo.join(".pixel")).unwrap();
        let mut input = serde_json::Map::new();
        input.insert("pattern".to_string(), Value::String("foo".to_string()));
        let msg = grep_redirect_advisory_lines("foo", &repo, &input).join("\n");
        assert!(!msg.contains("BLOCKED"), "equivalent Grep must not be blocked: {msg}");
        assert!(msg.contains("pixel search"), "advisory should show the Pixel equivalent: {msg}");
        assert!(msg.contains("Proceeding with the original Grep call"), "{msg}");
    }

    // --- sequencer pass-through for `git add` ---------------------------

    /// Create a real git repo in a temp dir and return its root path.
    fn real_repo(name: &str) -> PathBuf {
        let root = std::env::temp_dir()
            .join(format!("pixel-guard-seq-{}-{}", name, std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::process::Command::new("git")
            .arg("init").arg("-q").arg("-b").arg("main").arg(&root)
            .status().unwrap();
        std::process::Command::new("git")
            .arg("-C").arg(&root)
            .args(["config", "user.email", "t@t"])
            .status().unwrap();
        std::process::Command::new("git")
            .arg("-C").arg(&root)
            .args(["config", "user.name", "t"])
            .status().unwrap();
        std::fs::write(root.join("a.txt"), b"a").unwrap();
        std::process::Command::new("git")
            .arg("-C").arg(&root).args(["add", "."])
            .status().unwrap();
        std::process::Command::new("git")
            .arg("-C").arg(&root).args(["commit", "-qm", "init"])
            .status().unwrap();
        canonical(&root)
    }

    #[test]
    fn sequencer_in_progress_false_on_clean_repo() {
        let root = real_repo("clean");
        assert!(
            !sequencer_in_progress(&root),
            "clean repo must not report sequencer in progress"
        );
    }

    #[test]
    fn sequencer_in_progress_true_for_cherry_pick() {
        let root = real_repo("cherrypick");
        std::fs::write(root.join(".git").join("CHERRY_PICK_HEAD"), b"abc123\n").unwrap();
        assert!(
            sequencer_in_progress(&root),
            "CHERRY_PICK_HEAD must signal sequencer in progress"
        );
    }

    #[test]
    fn sequencer_in_progress_true_for_rebase() {
        let root = real_repo("rebase");
        std::fs::create_dir_all(root.join(".git").join("rebase-merge")).unwrap();
        assert!(
            sequencer_in_progress(&root),
            "rebase-merge/ dir must signal sequencer in progress"
        );
    }

    #[test]
    fn sequencer_in_progress_true_for_merge() {
        let root = real_repo("merge");
        std::fs::write(root.join(".git").join("MERGE_HEAD"), b"def456\n").unwrap();
        assert!(
            sequencer_in_progress(&root),
            "MERGE_HEAD must signal sequencer in progress"
        );
    }

    #[test]
    fn git_add_passes_through_during_cherry_pick() {
        // Regression: during cherry-pick/rebase/merge conflict resolution,
        // `git add` stages resolved files WITHOUT committing — the
        // sequencer's `--continue` creates the commit. `pixel publish`
        // commits in one step and cannot substitute. The guard must pass
        // `git add` through when a sequencer is active.
        let root = real_repo("add-cherrypick");
        std::fs::write(root.join(".git").join("CHERRY_PICK_HEAD"), b"abc123\n").unwrap();
        assert!(
            git_mutation_substitute_lines("git add src/foo.rs", Some(&root), &root).is_none(),
            "`git add` during cherry-pick must pass through, not be substitute-denied"
        );
    }

    #[test]
    fn git_add_still_denied_without_sequencer() {
        // No sequencer active → normal substitute deny applies.
        let root = real_repo("add-noseq");
        assert!(
            git_mutation_substitute_lines("git add src/foo.rs", Some(&root), &root).is_some(),
            "`git add` without active sequencer must still be substitute-denied"
        );
    }

    #[test]
    fn sequencer_in_progress_true_for_revert() {
        let root = real_repo("revert");
        std::fs::write(root.join(".git").join("REVERT_HEAD"), b"abc123\n").unwrap();
        assert!(
            sequencer_in_progress(&root),
            "REVERT_HEAD must signal sequencer in progress"
        );
    }

    #[test]
    fn sequencer_in_progress_true_for_rebase_apply() {
        // `git rebase --apply` and `git am` conflicts use rebase-apply/.
        let root = real_repo("rebase-apply");
        std::fs::create_dir_all(root.join(".git").join("rebase-apply")).unwrap();
        assert!(
            sequencer_in_progress(&root),
            "rebase-apply/ dir must signal sequencer in progress"
        );
    }

    #[test]
    fn git_commit_passes_through_during_merge() {
        // Regression: concluding a conflicted merge is `git add` (already
        // passed through) then `git commit` — which writes the merge commit
        // with BOTH parents from MERGE_HEAD. The old deny pointed at
        // `pixel publish`, whose plain single-parent commit would silently
        // corrupt the merge graph.
        let root = real_repo("commit-merge");
        std::fs::write(root.join(".git").join("MERGE_HEAD"), b"def456\n").unwrap();
        assert!(
            git_mutation_substitute_lines("git commit -m 'resolve merge'", Some(&root), &root).is_none(),
            "`git commit` during merge must pass through, not be substitute-denied"
        );
    }

    #[test]
    fn git_commit_passes_through_during_cherry_pick() {
        let root = real_repo("commit-cherrypick");
        std::fs::write(root.join(".git").join("CHERRY_PICK_HEAD"), b"abc123\n").unwrap();
        assert!(
            git_mutation_substitute_lines("git commit", Some(&root), &root).is_none(),
            "`git commit` during cherry-pick must pass through"
        );
    }

    #[test]
    fn git_commit_still_denied_without_sequencer() {
        let root = real_repo("commit-noseq");
        assert!(
            git_mutation_substitute_lines("git commit -m 'plain'", Some(&root), &root).is_some(),
            "`git commit` without active sequencer must still be substitute-denied"
        );
    }

    #[test]
    fn sequencer_in_progress_resolves_relative_worktree_gitdir() {
        // A `.git` FILE with a relative `gitdir:` pointer resolves against
        // the directory containing the file, not the process cwd.
        let root = real_repo("relative-gitdir");
        let real_git = root.join(".git");
        let moved = root.join("actual-git-dir");
        std::fs::rename(&real_git, &moved).unwrap();
        std::fs::write(&real_git, b"gitdir: actual-git-dir\n").unwrap();
        assert!(!sequencer_in_progress(&root), "clean state via pointer");
        std::fs::write(moved.join("MERGE_HEAD"), b"def456\n").unwrap();
        assert!(
            sequencer_in_progress(&root),
            "relative gitdir pointer must resolve against the worktree root"
        );
    }
}

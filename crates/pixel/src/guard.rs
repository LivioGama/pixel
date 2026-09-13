//! `pixel hook guard` — provider-aware, exact-subset search routing.
//! Explicit providers preserve unsupported calls silently. Without a
//! provider, legacy non-blocking task-scoping guidance remains available.
//!
//! Legacy advisory contract (without `--provider`):
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
//! Rewrites preserve native search bytes and status within a narrow
//! literal-file subset; they do not substitute enriched Pixel output.
//! Uncovered execution shapes fall back to the original native command.
//!
//! The hook never blocks ordinary work: advisories exit 0 with a JSON note
//! (systemMessage + additionalContext), no permissionDecision, and transparent
//! read-only rewrites use `updatedInput`. Claude/Devin retain their normal
//! permission flow. Codex requires an explicit `allow` for the user-approved
//! literal-file rewrite subset only; credential-shaped paths are excluded.
//! Fails open (exit 0) on any parse error or unexpected shape — a guard
//! that crashes or wedges the session is worse than a guard that misses a
//! case.

use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

const COMPOSED_MAX_INPUT: usize = 1024 * 1024;
const COMPOSED_MAX_OUTPUT: usize = 1024 * 1024;
const COMPOSED_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Provider {
    Claude,
    Codex,
    Devin,
}

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
        && ((b[0] == b'"' && b[b.len() - 1] == b'"') || (b[0] == b'\'' && b[b.len() - 1] == b'\''))
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
            if SHELL_WRAPPERS.contains(&normalize_bin(parts[0]))
                && let Some(pos) = parts
                    .iter()
                    .position(|t| t.starts_with('-') && t.contains('c'))
                && let Some(script) = parts.get(pos + 1)
            {
                return unwrap_shell_c(script);
            }
            unwrap_shell_c(&parts.join(" "))
        }
        _ => String::new(),
    }
}

/// Preserve a provider's command representation while replacing only the
/// script executed by a known shell wrapper. Arbitrary argv arrays stay on the
/// native path: converting them into `sh -c` would change quoting semantics.
fn rewritten_command_value(original: &Value, rewritten: String) -> Option<Value> {
    match original {
        Value::String(_) => Some(Value::String(rewritten)),
        Value::Array(items) => {
            let parts: Vec<&str> = items.iter().map(Value::as_str).collect::<Option<_>>()?;
            if parts.is_empty() || !SHELL_WRAPPERS.contains(&normalize_bin(parts[0])) {
                return None;
            }
            let script_index = parts
                .iter()
                .position(|token| token.starts_with('-') && token.contains('c'))?
                + 1;
            if script_index >= items.len() {
                return None;
            }
            let mut updated = items.clone();
            updated[script_index] = Value::String(rewritten);
            Some(Value::Array(updated))
        }
        _ => None,
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
    let store = TRANSCRIPT_STORE_MARKERS
        .iter()
        .find(|m| cmd.contains(**m))?;
    let digs_in = ARCHAEOLOGY_TOOLS.iter().any(|t| cmd.contains(t))
        || READERS.iter().any(|r| cmd.contains(r));
    if digs_in { Some(store) } else { None }
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

/// Provider adapters only change the command field. Timeouts, cwd, metadata
/// and future provider arguments survive untouched. Codex requires allow
/// alongside updatedInput; that authorization is restricted to this exact
/// read-only compatibility subset, never applied to fallback calls.
fn rewrite_json(provider: Provider, updated_input: Value) -> Value {
    let mut output = serde_json::json!({
        "hookEventName": "PreToolUse",
        "updatedInput": updated_input,
    });
    if provider == Provider::Codex {
        output["permissionDecision"] = Value::String("allow".into());
        output["permissionDecisionReason"] =
            Value::String("Pixel compatibility routing: single-file literal read only.".into());
    }
    serde_json::json!({"hookSpecificOutput": output})
}

fn provider_rewrite(provider: Provider, payload: &Value) -> Option<Value> {
    if !is_guard_event(
        payload,
        payload
            .get("hook_event_name")
            .and_then(Value::as_str)
            .unwrap_or(""),
    ) {
        return None;
    }
    let tool = payload.get("tool_name")?.as_str()?;
    let shell = match provider {
        Provider::Claude => tool == "Bash",
        Provider::Codex => matches!(tool, "Bash" | "shell" | "unified_exec" | "local_shell"),
        Provider::Devin => tool == "exec" || tool == "Bash",
    };
    if !shell {
        return None;
    }
    let input = payload.get("tool_input")?.as_object()?;
    // An execution-specific environment is not the hook's environment.
    // In particular, rg config can request a preprocessor: never authorize
    // that native fallback using only the apparent read-only argv shape.
    if input.contains_key("env") || input.contains_key("environment") {
        return None;
    }
    let original_command = input.get("command")?;
    let command = command_text(original_command);
    if command.is_empty() {
        return None;
    }
    let base = payload.get("cwd").and_then(Value::as_str).map_or_else(
        || std::env::current_dir().unwrap_or_default(),
        PathBuf::from,
    );
    let cwd = input
        .get("workdir")
        .or_else(|| input.get("cwd"))
        .and_then(Value::as_str)
        .map(|p| base.join(p))
        .unwrap_or(base);
    let rewritten = crate::search_compat::rewrite(&command, &cwd)?;
    let rewritten_command = rewritten_command_value(original_command, rewritten)?;
    let mut updated = Value::Object(input.clone());
    updated["command"] = rewritten_command;
    Some(rewrite_json(provider, updated))
}

fn run_provider_guard(provider: Provider, delegate_rtk: bool, raw: &str) -> ! {
    let Ok(payload) = serde_json::from_str::<Value>(raw) else {
        std::process::exit(0);
    };
    if let Some(response) = provider_rewrite(provider, &payload) {
        print!("{response}");
        std::process::exit(0);
    }
    if delegate_rtk && provider == Provider::Claude {
        delegate_rtk_hook(raw);
    }
    // Ordinary commands must not receive a fresh context note on every
    // tool call. No response means no rewrite or permission override.
    std::process::exit(0);
}

/// A foreign Codex `PreToolUse` command retained at install time.  This is
/// deliberately a *command snapshot*, rather than a pointer back to
/// `hooks.json`: a later edit to the live configuration cannot turn Pixel into
/// an executor for an arbitrary command.
#[derive(Debug)]
struct ComposedForeignHook {
    matcher: String,
    command: String,
}

fn load_composed_backup(path: &Path) -> Option<Vec<ComposedForeignHook>> {
    // A replacement symlink would make the fixed hook command execute a
    // different file than the installer sealed. Refuse it rather than follow.
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > COMPOSED_MAX_INPUT as u64 {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return None;
        }
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take((COMPOSED_MAX_INPUT + 1) as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > COMPOSED_MAX_INPUT {
        return None;
    }
    let envelope: Value = serde_json::from_slice(&bytes).ok()?;
    if envelope.get("version").and_then(Value::as_u64) != Some(1)
        || envelope.get("provider").and_then(Value::as_str) != Some("codex")
    {
        return None;
    }
    let groups = envelope.get("pre_tool_use")?.as_array()?;
    let mut result = Vec::new();
    for group in groups {
        let matcher = group.get("matcher").and_then(Value::as_str).unwrap_or("");
        // Codex treats an omitted/empty matcher as a catch-all. `*` is also
        // accepted by existing configs even though it is not a Rust regex.
        if !matches!(matcher, "" | "*" | ".*") && regex::Regex::new(matcher).is_err() {
            return None;
        }
        let hooks = group.get("hooks")?.as_array()?;
        for hook in hooks {
            if hook.get("type").and_then(Value::as_str) != Some("command") {
                return None;
            }
            let command = hook.get("command").and_then(Value::as_str)?;
            // Shell snippets legitimately contain newlines; only NUL cannot
            // be represented as a process argument and is rejected here.
            if command.is_empty() || command.bytes().any(|byte| byte == 0) {
                return None;
            }
            // A Pixel command in the backup would recurse; installers must
            // remove Pixel before snapshotting, and this is a second boundary.
            if command.contains(" pixel hook ") || command.starts_with("pixel hook ") {
                return None;
            }
            result.push(ComposedForeignHook {
                matcher: matcher.to_owned(),
                command: command.to_owned(),
            });
        }
    }
    Some(result)
}

fn composed_matches(matcher: &str, tool: &str) -> bool {
    matches!(matcher, "" | "*" | ".*")
        || regex::Regex::new(matcher).is_ok_and(|regex| regex.is_match(tool))
}

fn run_foreign_command(command: &str, raw: &[u8], cwd: &Path) -> Option<Value> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let mut child = Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let stdout = child.stdout.take()?;
    let stderr = child.stderr.take()?;
    let (out_tx, out_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let read = stdout
            .take((COMPOSED_MAX_OUTPUT + 1) as u64)
            .read_to_end(&mut bytes);
        let _ = out_tx.send(read.map(|_| bytes));
    });
    let (err_tx, err_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let read = stderr
            .take((COMPOSED_MAX_OUTPUT + 1) as u64)
            .read_to_end(&mut bytes);
        let _ = err_tx.send(read.map(|_| bytes));
    });
    if let Some(mut stdin) = child.stdin.take() {
        let raw = raw.to_vec();
        // The command receives exactly the bytes Codex delivered, not a
        // reserialized JSON value with whitespace/key ordering changed.
        std::thread::spawn(move || {
            let _ = stdin.write_all(&raw);
        });
    }
    let deadline = std::time::Instant::now() + COMPOSED_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(_)) | Err(_) => return None,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    let stdout = out_rx.recv_timeout(remaining).ok()?.ok()?;
    let stderr = err_rx.recv_timeout(remaining).ok()?.ok()?;
    if stdout.len() > COMPOSED_MAX_OUTPUT || stderr.len() > COMPOSED_MAX_OUTPUT {
        return None;
    }
    if stdout.is_empty() {
        return Some(Value::Null);
    }
    serde_json::from_slice(&stdout).ok()
}

fn foreign_hook_output(value: Value) -> Option<Value> {
    // Empty stdout is the standard observer result. Any other response must
    // be a single Codex-shaped PreToolUse response; arbitrary JSON is not
    // safely mergeable and disables Pixel rewriting for this invocation.
    if value.is_null() {
        return Some(value);
    }
    let specific = value.get("hookSpecificOutput")?.as_object()?;
    (specific.get("hookEventName").and_then(Value::as_str) == Some("PreToolUse")).then_some(value)
}

fn has_foreign_mutation(value: &Value) -> bool {
    let Some(specific) = value.get("hookSpecificOutput") else {
        return false;
    };
    specific.get("updatedInput").is_some()
        || specific.get("permissionDecision").is_some()
        || value.get("permissionDecision").is_some()
}

fn foreign_denial(value: &Value) -> bool {
    value
        .get("hookSpecificOutput")
        .and_then(|specific| specific.get("permissionDecision"))
        .and_then(Value::as_str)
        == Some("deny")
        || value.get("permissionDecision").and_then(Value::as_str) == Some("deny")
}

fn foreign_context(value: &Value) -> Option<&str> {
    value
        .get("hookSpecificOutput")
        .and_then(|specific| specific.get("additionalContext"))
        .and_then(Value::as_str)
}

fn compose_context(mut response: Value, contexts: &[String]) -> Value {
    if contexts.is_empty() {
        return response;
    }
    let context = contexts.join("\n");
    response["hookSpecificOutput"]["additionalContext"] = Value::String(context);
    response
}

/// Execute the install-time snapshot of Codex foreign PreToolUse commands,
/// then apply Pixel's transparent literal-read rewrite only when no foreign
/// handler returned a denial or input/permission mutation. Every failure is a
/// fail-open native execution with no Pixel rewrite.
pub fn run_composed_codex(backup: &Path) -> ! {
    let mut raw = Vec::new();
    if std::io::stdin()
        .take((COMPOSED_MAX_INPUT + 1) as u64)
        .read_to_end(&mut raw)
        .is_err()
        || raw.is_empty()
        || raw.len() > COMPOSED_MAX_INPUT
    {
        std::process::exit(0);
    }
    let Ok(payload) = serde_json::from_slice::<Value>(&raw) else {
        std::process::exit(0);
    };
    if !is_guard_event(
        &payload,
        payload
            .get("hook_event_name")
            .and_then(Value::as_str)
            .unwrap_or(""),
    ) {
        std::process::exit(0);
    }
    let Some(tool) = payload.get("tool_name").and_then(Value::as_str) else {
        std::process::exit(0);
    };
    let Some(cwd) = payload
        .get("cwd")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .filter(|path| path.is_dir())
    else {
        std::process::exit(0);
    };
    let Some(hooks) = load_composed_backup(backup) else {
        std::process::exit(0);
    };

    let mut contexts = Vec::new();
    let mut terminal_foreign = None;
    let mut incomplete_foreign = false;
    for hook in hooks
        .into_iter()
        .filter(|hook| composed_matches(&hook.matcher, tool))
    {
        let Some(foreign) =
            run_foreign_command(&hook.command, &raw, &cwd).and_then(foreign_hook_output)
        else {
            // An unavailable, malformed or unbounded foreign handler means we
            // cannot prove its semantics; preserve native Codex behavior.
            incomplete_foreign = true;
            continue;
        };
        if foreign.is_null() {
            continue;
        }
        if foreign_denial(&foreign) || has_foreign_mutation(&foreign) {
            // Codex normally invokes independent handlers concurrently. Keep
            // dispatching the remaining install-time commands for their side
            // effects, but retain the first authoritative response because
            // only one response can be returned from this composed runtime.
            if terminal_foreign.is_none() {
                terminal_foreign = Some(foreign);
            }
            continue;
        }
        if let Some(context) = foreign_context(&foreign) {
            contexts.push(context.to_owned());
        } else {
            // A valid but unknown response shape cannot be merged losslessly.
            incomplete_foreign = true;
        }
    }
    if let Some(foreign) = terminal_foreign {
        // Never place a Pixel rewrite after foreign authority. Returning this
        // one valid response preserves the first foreign decision.
        print!("{foreign}");
        std::process::exit(0);
    }
    if incomplete_foreign {
        std::process::exit(0);
    }
    if let Some(response) = provider_rewrite(Provider::Codex, &payload) {
        print!("{}", compose_context(response, &contexts));
    } else if !contexts.is_empty() {
        print!("{}", compose_context(advisory_json(""), &contexts));
    }
    std::process::exit(0);
}

/// The installer enables this only after adopting the exact existing RTK
/// registration. There is one response writer, never two competing hooks.
fn delegate_rtk_hook(raw: &str) -> ! {
    use std::io::Write;
    use std::process::{Command, Stdio};
    const MAX_OUTPUT: u64 = 1024 * 1024;
    let Ok(mut child) = Command::new("rtk")
        .args(["hook", "claude"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    else {
        print!(
            "{}",
            advisory_json("pixel routing: RTK delegate unavailable; original call proceeds.")
        );
        std::process::exit(0);
    };
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let (out_tx, out_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let read = stdout.take(MAX_OUTPUT + 1).read_to_end(&mut bytes);
        let _ = out_tx.send(read.map(|_| bytes));
    });
    let (err_tx, err_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let read = stderr.take(MAX_OUTPUT + 1).read_to_end(&mut bytes);
        let _ = err_tx.send(read.map(|_| bytes));
    });
    if let Some(mut stdin) = child.stdin.take() {
        let input = raw.to_owned();
        std::thread::spawn(move || {
            let _ = stdin.write_all(input.as_bytes());
        });
    }
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    // No thread joins: a descendant retaining a pipe must not outlive the
    // hook deadline. Oversize/partial output is discarded, never serialized
    // as if it were RTK's complete response.
    let stdout = out_rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()));
    let stderr = err_rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()));
    if let (Some(status), Ok(Ok(stdout)), Ok(Ok(stderr))) = (status, stdout, stderr)
        && stdout.len() as u64 <= MAX_OUTPUT
        && stderr.len() as u64 <= MAX_OUTPUT
    {
        let _ = std::io::stdout().write_all(&stdout);
        let _ = std::io::stderr().write_all(&stderr);
        std::process::exit(status.code().unwrap_or(1));
    }
    print!(
        "{}",
        advisory_json(
            "pixel routing: RTK delegate timed out or returned incomplete output; original call proceeds."
        )
    );
    std::process::exit(0);
}

/// Entry point for `pixel hook guard`. Reads the PreToolUse hook payload
/// from stdin. Never returns an `Err` that would surface as exit 1 — every
/// failure path is a deliberate exit 0 (allow, optionally with advice).
pub fn run(provider: Option<Provider>, delegate_rtk: bool) -> ! {
    if let Ok(kill) = std::env::var("PIXEL_TARGETS_GUARD")
        && matches!(kill.as_str(), "0" | "false" | "off")
    {
        std::process::exit(0);
    }

    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() || input.trim().is_empty() {
        std::process::exit(0);
    }
    if let Some(provider) = provider {
        run_provider_guard(provider, delegate_rtk, &input);
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

    let tool = payload
        .get("tool_name")
        .and_then(Value::as_str)
        .unwrap_or("");
    let cwd = payload.get("cwd").and_then(Value::as_str).map_or_else(
        || std::env::current_dir().unwrap_or_default(),
        PathBuf::from,
    );
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

    // POST-TOOL-USE blast-radius: after an Edit/Write/apply_patch, deliver
    // the dependants of what was just edited without being asked (P0·3).
    // This is the only one of the nine tracks that turns an *ignored doctrine rule*
    // into a *delivered fact*: the PreToolUse doctrine says "run pixel impact
    // before editing a symbol", butthe bench shows agents don't. Here the
    // dependants arrive after the edit, unsolicited.
    if event == "PostToolUse" {
        post_tool_use_blast_radius(&anchor, idx_root.as_deref(), tool);
        std::process::exit(0);
    }

    let manifest_root = find_up(&anchor, Path::new(".pixel").join("targets.json"));
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
        if idx_root.is_some()
            && let Some(original) = tool_input.get("command").and_then(Value::as_str)
            && let Some(rewritten) = crate::search_compat::rewrite(original, &cwd)
        {
            // Read-only search rewrites are semantically equivalent, so
            // transparently replace the input and let the normal tool
            // permission flow continue.
            let mut updated = Value::Object(tool_input.clone());
            updated["command"] = Value::String(rewritten);
            print!("{}", rewrite_json(Provider::Claude, updated));
            std::process::exit(0);
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
            if idx_root.is_some() && is_grep_tool(tool, tool_input) {
                let pattern = tool_input
                    .get("pattern")
                    .and_then(Value::as_str)
                    .or_else(|| tool_input.get("query").and_then(Value::as_str))
                    // Antigravity grep_search / Cursor file_search use "Query"
                    .or_else(|| tool_input.get("Query").and_then(Value::as_str))
                    .unwrap_or("");
                if !pattern.is_empty() {
                    advise(&grep_redirect_advisory_lines(pattern, &cwd, tool_input));
                }
            }
            // RETRIEVAL ADVISORY — in an indexed repo with NO active manifest,
            // suggest `pixel targets` first while allowing retrieval to proceed.
            // Read is allowed through (reading a known file is not retrieval),
            // but gets an advisory in indexed repos with no manifest if the
            // file is a source file — suggesting `pixel targets` first.
            // `PIXEL_GUARD_RETRIEVAL=0` disables this tier.
            if idx_root.is_some() && manifest.is_none() && !manifest_expired && is_retrieval_tool(tool)
                && !env_flag_off("PIXEL_GUARD_RETRIEVAL") {
                    retrieval_guard_advisory(&cwd, idx_root.as_deref().unwrap());
                }
            // Retrieval-first advisory for Read of source files: in an indexed
            // repo with no active manifest, suggest `pixel targets` before
            // reading source files. Advisory only — the read proceeds. This
            // catches the "massive token waste via redundant reads" failure
            // mode where agents read entire files instead of using pixel search.
            if idx_root.is_some() && manifest.is_none() && !manifest_expired && is_read_tool(tool)
                && let Some(p) = resolve(raw_path, &cwd)
                    && p.is_file() && is_source_file(&p) && !is_exempt(&p, idx_root.as_deref().unwrap())
                        && !env_flag_off("PIXEL_GUARD_READ") {
                            read_scoping_advisory(&p, idx_root.as_deref().unwrap());
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
    if event == "PreToolUse" || event == "BeforeTool" || event == "PostToolUse" {
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
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    };
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

/// After an Edit/Write/apply_patch, report bounded references from the existing
/// graph snapshot, distinguishing same-file and cross-file dependants. This is
/// advisory evidence, not proof of breakage or current-source freshness. A graph
/// miss or unknown edited path remains a silent allow; no refresh is triggered.
///
/// Entry point for the `pixel hook post-tool-use` hook: the already-written
/// PostToolUse blast-radius hook invoked by `pixel hook post-tool-use` .
/// Unlike [`run`] which infers the event from the payload, this *forces* the event
/// to `PostToolUse` — PostToolUse hook files are per-event,so `hook_event_name`
/// is often absent from their payload. Reads stdin, resolves the edited path +
/// index, and emits a NON-BLOCKING blast-radius advisory; a graph/index miss
/// is a silent allow (exit 0, never a denial).
pub fn run_post_tool_use(provider: Option<Provider>) -> ! {
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() || input.trim().is_empty() {
        std::process::exit(0);
    }
    // No run_provider_guard call here: that path emits *PreToolUse*
    // permission rewrites and never returns, which would make an installed
    // `post-tool-use --provider claude` hook exit before the blast radius
    // runs. The provider only qualifies the payload's runtime; the path
    // keys are already normalized below (file_path/path/TargetFile/
    // AbsolutePath/target_file/filePath) and the advisory emitted by
    // `post_tool_use_advisory` is already the Claude hook contract.
    let _ = provider;
    let Ok(payload) = serde_json::from_str::<Value>(&input) else {
        std::process::exit(0);
    };
    let tool = payload
        .get("tool_name")
        .and_then(Value::as_str)
        .unwrap_or("");
    let cwd = payload.get("cwd").and_then(Value::as_str).map_or_else(
        || std::env::current_dir().unwrap_or_default(),
        PathBuf::from,
    );
    let tool_input = payload.get("tool_input").cloned().unwrap_or(Value::Null);
    let Some(tool_input) = tool_input.as_object() else {
        std::process::exit(0);
    };
    let raw_path = tool_input
        .get("file_path")
        .or_else(|| tool_input.get("path"))
        .or_else(|| tool_input.get("TargetFile"))
        .or_else(|| tool_input.get("AbsolutePath"))
        .or_else(|| tool_input.get("target_file"))
        .or_else(|| tool_input.get("filePath"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let anchor = resolve(raw_path, &cwd).unwrap_or_else(|| canonical(&cwd));
    let idx_root = find_up(&anchor, ".pixel");
    post_tool_use_blast_radius(&anchor, idx_root.as_deref(), tool);
    std::process::exit(0);
}

fn post_tool_use_blast_radius(abs: &Path, idx_root: Option<&Path>, tool: &str) {
    use pixel_graph::GraphStore;
    let is_edit = matches!(
        tool,
        "Edit"
            | "MultiEdit"
            | "NotebookEdit"
            | "Write"
            | "edit"
            | "write"
            | "notebook_edit"
            | "apply_patch"
            | "write_file"
            | "replace_file_content"
            | "write_to_file"
            | "edit_file"
    );
    if !is_edit {
        return;
    }
    let Some(root) = idx_root.map(PathBuf::from) else {
        return;
    };
    let db = root.join(".pixel").join("graph.db");
    if !db.exists() {
        return;
    }
    let Ok(mut store) = GraphStore::open(&db) else {
        return;
    };
    let rel = rel_of(abs, &root);
    if rel.is_empty() || rel.starts_with(".pixel/") {
        return;
    }
    let Ok(Some(file_row)) = store.file_by_path(&rel) else {
        return;
    };
    if let Some(note) = post_edit_snapshot_note(&mut store, file_row.id, &rel) {
        print!("{}", post_tool_use_advisory(&note));
    }
}

/// Read the existing graph in one transaction; never refresh or read source in
/// the post-edit path. Counts concern indexed references, not proven breakages.
fn post_edit_snapshot_note(
    store: &mut pixel_graph::GraphStore,
    file_id: i64,
    rel: &str,
) -> Option<String> {
    const PATH_LIMIT: i64 = 8;
    const PATH_CHARS: usize = 120;
    let tx = store.conn_mut().transaction().ok()?;
    let (symbols, cross_file, same_file, files, unresolved): (i64, i64, i64, i64, i64) = tx
        .query_row(
            "WITH incoming AS (
            SELECT DISTINCT src.id, src.file_id
            FROM symbols dst JOIN edges e ON e.dst_id = dst.id
            JOIN symbols src ON src.id = e.src_id WHERE dst.file_id = ?1
        ) SELECT
            (SELECT COUNT(*) FROM symbols WHERE file_id = ?1),
            (SELECT COUNT(*) FROM incoming WHERE file_id != ?1),
            (SELECT COUNT(*) FROM incoming WHERE file_id = ?1),
            (SELECT COUNT(DISTINCT file_id) FROM incoming WHERE file_id != ?1),
            (SELECT COUNT(*) FROM unresolved_calls WHERE name IN
                (SELECT DISTINCT name FROM symbols WHERE file_id = ?1))",
            [file_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .ok()?;
    if cross_file == 0 && same_file == 0 && unresolved == 0 {
        return None;
    }
    let mut paths = tx
        .prepare(
            "SELECT DISTINCT f.path FROM symbols dst
         JOIN edges e ON e.dst_id = dst.id JOIN symbols src ON src.id = e.src_id
         JOIN files f ON f.id = src.file_id
         WHERE dst.file_id = ?1 AND src.file_id != ?1 ORDER BY f.path LIMIT ?2",
        )
        .ok()?;
    let rows = paths
        .query_map([file_id, PATH_LIMIT], |row| row.get::<_, String>(0))
        .ok()?;
    let mut paths_capped = files > PATH_LIMIT;
    let mut rendered_paths = Vec::new();
    for path in rows {
        let path = path.ok()?;
        paths_capped |= path.chars().count() > PATH_CHARS;
        // Quote control characters and newlines: repository names are data.
        rendered_paths.push(serde_json::to_string(&short_task(&path, PATH_CHARS)).ok()?);
    }
    let paths = if rendered_paths.is_empty() {
        "none indexed".to_string()
    } else {
        rendered_paths.join(", ")
    };
    let edited = serde_json::to_string(&short_task(rel, PATH_CHARS)).ok()?;
    Some(format!(
        "just edited {edited}: stored graph records {cross_file} cross-file referencing symbols in {files} files and {same_file} same-file referencing symbols for its {symbols} symbols. \
         Dependent paths: {paths}. paths_capped={paths_capped}; lower_bound={} within the stored snapshot (unresolved same-name calls: {unresolved}). \
         Freshness unchecked: this graph snapshot may predate the edit; no refresh or source read was performed. \
         References may be approximate; inspect these dependants and test before trusting the change.",
        unresolved > 0 || paths_capped,
    ))
}

/// PostToolUse advisory: surface the blast-radius note to the model without a
/// permission decision (the edit already happened).
fn post_tool_use_advisory(note: &str) -> serde_json::Value {
    serde_json::json!({
        "systemMessage": note,
        "hookSpecificOutput": {
            "hookEventName": "PostToolUse",
            "additionalContext": note
        }
    })
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
///   Expired tasks (older than the 24h TTL) are dropped individually; a
///   manifest whose tasks have all expired reports `Expired`.
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
                    task: t
                        .get("task")
                        .and_then(Value::as_str)
                        .unwrap_or("?")
                        .to_string(),
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
            task: m
                .get("task")
                .and_then(Value::as_str)
                .unwrap_or("?")
                .to_string(),
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
    ManifestState::Active(Manifest {
        root: root.to_path_buf(),
        tasks,
    })
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
            let tier = f
                .get("tier")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            Some((path, tier))
        })
        .collect()
}

/// Iterator over every (path, tier) across all active tasks.
fn all_files(m: &Manifest) -> impl Iterator<Item = &(String, String)> {
    m.tasks.iter().flat_map(|t| t.files.iter())
}

fn rel_of(abs: &Path, root: &Path) -> String {
    abs.strip_prefix(root).map_or_else(
        |_| abs.to_string_lossy().into_owned(),
        |r| r.to_string_lossy().into_owned(),
    )
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
    lines
        .push("Proceeding. If scope has drifted, re-run `pixel targets \"<refined task>\"`".into());
    lines.push("to refresh your task's list, or `pixel targets --clear` to end scoping.".into());
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
        "Read" | "read" | "read_file" | "notebook_read" | "view_file" // Antigravity
    )
}

/// True if the file extension suggests source code (not config/docs/prose).
/// Used to limit the read-scoping advisory to source files — reading a
/// README or package.json is always legitimate.
fn is_source_file(p: &Path) -> bool {
    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
    matches!(
        ext,
        "rs" | "ts"
            | "tsx"
            | "js"
            | "jsx"
            | "py"
            | "go"
            | "java"
            | "c"
            | "cpp"
            | "h"
            | "hpp"
            | "cs"
            | "rb"
            | "swift"
            | "kt"
            | "scala"
            | "clj"
            | "ex"
            | "exs"
            | "erl"
            | "hs"
            | "ml"
            | "fs"
            | "nim"
            | "zig"
            | "v"
            | "lua"
            | "php"
            | "pl"
            | "r"
            | "dart"
            | "elm"
            | "julia"
            | "lisp"
            | "sch"
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
    std::env::var(name).is_ok_and(|v| matches!(v.as_str(), "0" | "false" | "off"))
}

/// Advisory for Grep/Glob/find in an indexed repo with no active manifest.
/// Tells the agent to run `pixel targets` first while allowing retrieval.
fn retrieval_guard_advisory(_cwd: &Path, idx_root: &Path) -> ! {
    let root = idx_root.display().to_string();
    advise(&[
        "pixel-guard advisory: code search happened before retrieval scoping.".into(),
        "In an indexed directory, consider running `pixel targets` before searching the codebase."
            .into(),
        format!("  pixel targets \"<one-line task description>\" {root}"),
        "That returns the P0/P1/P2 file list in <50ms. Work P0 first, then P1.".into(),
        "After scoping, use `pixel search` / `pixel resolve` for code search — not grep/glob."
            .into(),
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
        format!(
            "pixel-targets-guard advisory: this {repo_phrase} has not been indexed by pixel yet."
        ),
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
fn check_bash_advisories(
    cmd: &str,
    cwd: &Path,
    idx_root: Option<&Path>,
    manifest: Option<&Manifest>,
) {
    // Strip leading `cd X &&` before pattern matching — the same stripping
    // that try_rewrite_bash does. strip_cd_prefix returns (effective_cwd, effective_cmd).
    let (effective_cwd, effective_cmd) = strip_cd_prefix(cmd, cwd);
    // Skip complex commands — heredocs, command substitution are left alone.
    if effective_cmd.contains("<<") || effective_cmd.contains("$(") || effective_cmd.contains('`') {
        return;
    }
    // Bypass-pattern advisories for indexed repos
    if let Some(root) = idx_root
        && let Some(lines) = bypass_advisory_lines(effective_cmd, &effective_cwd, root)
    {
        advise(&non_blocking_advisory_lines(&lines));
    }
    if let Some(m) = manifest
        && let Some(first_file) = single_reader_target(effective_cmd, &effective_cwd)
        && !allowed(&first_file, m)
    {
        scoping_advisory(&first_file, m);
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
    while matches!(
        tokens.first().map(String::as_str),
        Some("rtk") | Some("command") | Some("builtin")
    ) {
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
    if ref_str == "HEAD"
        || ref_str.starts_with("HEAD~")
        || ref_str.starts_with("HEAD^")
        || ref_str.starts_with("HEAD@")
    {
        return false;
    }
    // Raw OID — 40 (SHA-1) or 64 (SHA-256) hex chars
    let trimmed = ref_str.trim();
    if (trimmed.len() == 40 || trimmed.len() == 64)
        && trimmed.chars().all(|c| c.is_ascii_hexdigit())
    {
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
    head.strip_prefix("ref: refs/heads/")
        .map(ToString::to_string)
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
        "BLOCKED by pixel-targets-guard: raw historical file restore can clobber in-progress work."
            .into(),
        "Use the surgical planner instead:".into(),
        "  pixel rescue \"<what broke>\" .            # plan: versions + recommended last-good"
            .into(),
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
fn git_mutation_substitute_lines(
    cmd: &str,
    idx_root: Option<&Path>,
    cwd: &Path,
) -> Option<Vec<String>> {
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
                let alt_root =
                    extract_cd_target(cmd, cwd).or_else(|| extract_git_c_path(&args, cwd));
                if let Some(alt) = alt_root
                    && alt != root
                    && reconcile_conflict_pending(&alt)
                {
                    return None;
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
    let end = rest.find(['&', ';']).unwrap_or(rest.len());
    let path = rest[..end]
        .trim()
        .trim_matches(|c: char| c == '"' || c == '\'');
    if path.is_empty() {
        return None;
    }
    let p = Path::new(path);
    let resolved = if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    };
    resolved.canonicalize().ok().filter(|p| p.is_dir())
}

/// Extract the path from `git -C <path>` args, resolved against cwd.
fn extract_git_c_path(args: &[String], cwd: &Path) -> Option<PathBuf> {
    let c_idx = args.iter().position(|a| a == "-C")?;
    let path = args.get(c_idx + 1)?;
    let p = Path::new(path);
    let resolved = if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    };
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
        if pointed.is_absolute() {
            pointed
        } else {
            root.join(pointed)
        }
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
    root.join(".pixel")
        .join("reconcile-conflict.json")
        .is_file()
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
            if args
                .iter()
                .any(|a| a == "-p" || a == "--patch" || a == "-i" || a == "--interactive")
            {
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
            let all_variant = args
                .iter()
                .any(|a| a == "." || a == "-A" || a == "--all" || a == "-u" || a == "--update");
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
                if let Some(files) = git_status_porcelain_files(root)
                    && !files.is_empty()
                {
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
                .map_or_else(|| "\"<msg>\"".to_string(), shell_quote);
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
                format!(
                    "BLOCKED [PIXEL_SUBSTITUTE] by pixel-guard: raw {what} bypasses pixel's snapshot-gated, journaled mutation surface."
                ),
                "Run the exact equivalent instead (--files repeated once per file):".into(),
                format!(
                    "  pixel publish {amend}{files} --message {msg} --request-id <id> {root_q}"
                ),
            ];
            if c.all {
                lines.push(
                    "(-a detected: list each modified tracked file as its own --files flag.)"
                        .into(),
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
                "--tags",
                "--delete",
                "-d",
                "--mirror",
                "--all",
                "--prune",
                "--branches",
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
                .map_or_else(|| "<remote>".to_string(), |s| shell_quote(s));
            let refspec = words
                .get(1)
                .map_or_else(|| "<refspec>".to_string(), |s| shell_quote(s));
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
                .map_or_else(|| "<name>".to_string(), |s| shell_quote(s));
            Some(branch_substitute_lines("`git checkout -b`", &name, &root_q))
        }
        "switch" => {
            let pos = args.iter().position(|a| a == "-c" || a == "--create")?;
            let name = args
                .get(pos + 1)
                .map_or_else(|| "<name>".to_string(), |s| shell_quote(s));
            Some(branch_substitute_lines("`git switch -c`", &name, &root_q))
        }
        "rebase" => {
            const REBASE_PASS: &[&str] = &[
                "-i",
                "--interactive",
                "--continue",
                "--abort",
                "--skip",
                "--quit",
                "--edit-todo",
                "--onto",
                "--exec",
                "-x",
                "--autosquash",
                "--root",
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
        format!(
            "BLOCKED [PIXEL_SUBSTITUTE] by pixel-guard: raw {what} bypasses pixel's journaled branch op."
        ),
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
    "-m",
    "--message",
    "-C",
    "-c",
    "--fixup",
    "--squash",
    "-F",
    "--file",
    "--author",
    "--date",
    "-t",
    "--template",
    "--trailer",
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
        } else if t == "--interactive"
            || t == "--patch"
            || t == "--fixup"
            || t == "--squash"
            || t.starts_with("--fixup=")
            || t.starts_with("--squash=")
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
    if ["xargs", "for ", "while "]
        .iter()
        .any(|kw| cmd.contains(kw))
    {
        return None;
    }
    let first_segment = cmd.split([';', '|']).next()?.split("&&").next()?.trim();
    let tokens = simple_tokenize(first_segment);
    let (mut tokens, eff_cwd) = if tokens.first().map(String::as_str) == Some("cd") {
        let rest_after_cd = cmd.split_once("&&")?.1.trim();
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

/// Check if a tool call is a Grep-style search (has a pattern/query field).
fn is_grep_tool(tool: &str, input: &serde_json::Map<String, Value>) -> bool {
    // Claude Code's Grep tool has "pattern"; Devin's grep has "pattern";
    // some agents use "query". Read/Glob don't have pattern fields.
    // Antigravity's grep_search uses "Query"; file_search uses "Query".
    if !matches!(
        tool,
        "Grep" | "grep" | "search" | "grep_search" | "file_search"
    ) {
        return false;
    }
    input.get("pattern").is_some() || input.get("query").is_some() || input.get("Query").is_some()
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
    if input.contains_key("glob") || input.contains_key("type") || input.contains_key("output_mode")
    {
        return vec![
            "pixel-guard advisory: this Grep call includes filters that Pixel search cannot preserve exactly.".into(),
            "Proceeding with the original Grep call; use Pixel search when those filters are not needed.".into(),
        ];
    }
    let root = find_up(cwd, ".pixel").map_or_else(|| ".".to_string(), |r| r.display().to_string());
    let Some(cmd) = search_can_replace(pattern, &flags, &root) else {
        return vec![
            "pixel-guard advisory: this Grep query cannot be represented exactly by Pixel search."
                .into(),
            "Proceeding with the original Grep call.".into(),
        ];
    };
    vec![
        "pixel-guard advisory: Grep cannot be transparently rewired because the hook cannot change the tool type.".into(),
        format!("Equivalent Bash command if useful: {cmd}"),
        "Proceeding with the original Grep call.".into(),
    ]
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
    let dir_str = dir_str.trim_matches(|c| c == '\'' || c == '"').trim();
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

/// Find the byte index of the first `&&` outside single or double quotes.
fn find_unquoted_double_amp(s: &str) -> Option<usize> {
    let mut quote: Option<char> = None;
    let mut prev_amp_at: Option<usize> = None;
    for (idx, c) in s.char_indices() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '\'' || c == '"' => quote = Some(c),
            None if c == '&' => {
                if let Some(first) = prev_amp_at {
                    return Some(first);
                }
                prev_amp_at = Some(idx);
                continue;
            }
            None => {}
        }
        prev_amp_at = None;
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
        "-l",
        "--files-with-matches",
        "-c",
        "--count",
        "-v",
        "--invert",
        "-o",
        "--only-matching",
        "-m",
        "--max-count",
    ];
    if flags
        .iter()
        .any(|f| unsupported_flags.contains(&f.as_str()))
    {
        return None;
    }
    let escaped = pattern.replace('\'', "'\\''");
    Some(format!(
        "pixel search '{}' {} --context 5",
        escaped,
        shell_quote(root)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Create a unique scratch dir (with a `src/` subdir) acting as the
    /// indexed repo root for path-validation tests. Returns the
    /// canonicalized root so `starts_with` comparisons are stable on
    /// platforms where the temp dir is a symlink (macOS).
    fn scratch_repo(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("pixel-guard-{}-{}", name, std::process::id()));
        std::fs::create_dir_all(root.join("src")).unwrap();
        canonical(&root)
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
        assert!(
            msg.contains("advisory"),
            "must be phrased as advisory: {msg}"
        );
        assert!(msg.contains("src/c.rs"), "must name the file: {msg}");
        assert!(
            msg.contains("pixel targets"),
            "must suggest re-scoping: {msg}"
        );
        assert!(!msg.contains("BLOCKED"), "must not read as a deny: {msg}");
        assert!(!msg.contains("PIXEL_TARGETS_GUARD"), "no bypass ad: {msg}");
    }

    #[test]
    fn mandate_and_index_advisories_are_non_blocking_text() {
        let repo = scratch_repo("advisory-mandate");
        let f = repo.join("src").join("a.rs");
        std::fs::write(&f, "x").unwrap();
        let msg = mandate_advisory_lines(&f, &repo).join("\n");
        assert!(
            msg.contains("advisory") && !msg.contains("BLOCKED"),
            "{msg}"
        );
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
        assert!(matches!(
            load_manifest_state(&missing),
            ManifestState::Absent
        ));
    }

    #[test]
    fn git_pull_passes_through_and_is_not_rewritten() {
        // Raw `git pull` is no longer blocked; it must not be transparently
        // rewritten either.
        let repo = Path::new("/repo");
        assert!(bash_deny_lines("git pull", Some(repo)).is_none());
        assert!(bash_deny_lines("git pull upstream main", Some(repo)).is_none());
        assert!(bash_deny_lines("git pull --rebase origin main", Some(repo)).is_none());
    }

    #[test]
    fn no_deny_git_status() {
        let repo = Path::new("/repo");
        assert!(bash_deny_lines("git status", Some(repo)).is_none());
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
        assert!(
            msg.contains("git checkout -B"),
            "should suggest checkout -B: {msg}"
        );
        assert!(
            !msg.contains("pixel rescue"),
            "should NOT suggest rescue for branch repoint: {msg}"
        );
    }

    #[test]
    fn reset_hard_head_tilde_still_suggests_rescue() {
        // `git reset --hard HEAD~N` is real data loss — keep rescue suggestion.
        let repo = Path::new("/repo");
        let lines = bash_deny_lines("git reset --hard HEAD~3", Some(repo))
            .expect("HEAD~N reset must be denied");
        let msg = lines.join("\n");
        assert!(
            msg.contains("pixel rescue"),
            "should suggest rescue for HEAD~N: {msg}"
        );
        assert!(
            !msg.contains("git checkout -B"),
            "should NOT suggest checkout -B for HEAD~N: {msg}"
        );
    }

    #[test]
    fn reset_hard_raw_oid_still_suggests_rescue() {
        // `git reset --hard <oid>` is real data loss — keep rescue suggestion.
        let repo = Path::new("/repo");
        let lines = bash_deny_lines("git reset --hard abc123def456789", Some(repo))
            .expect("raw OID reset must be denied");
        let msg = lines.join("\n");
        assert!(
            msg.contains("pixel rescue"),
            "should suggest rescue for raw OID: {msg}"
        );
    }

    #[test]
    fn reset_hard_head_alone_still_suggests_rescue() {
        // `git reset --hard HEAD` discards working tree changes — rescue.
        let repo = Path::new("/repo");
        let lines = bash_deny_lines("git reset --hard HEAD", Some(repo))
            .expect("bare HEAD reset must be denied");
        let msg = lines.join("\n");
        assert!(
            msg.contains("pixel rescue"),
            "should suggest rescue for bare HEAD: {msg}"
        );
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
        assert!(!is_branch_like(
            "abc123def4567890123456789012345678901234567"
        )); // 40 hex
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
            bash_deny_lines(
                "pixel publish --message \"cleanup | git clean -fd equivalent\" .",
                Some(repo)
            )
            .is_none()
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
        assert!(bash_deny_lines("pixel search 'x' . && git reset --hard", Some(repo)).is_some());
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
            let msg =
                non_blocking_advisory_lines(&bash_deny_lines(cmd, Some(repo)).unwrap()).join("\n");
            assert!(
                !msg.contains("PIXEL_TARGETS_GUARD"),
                "advisory for `{cmd}` must not advertise the kill switch: {msg}"
            );
            assert!(!msg.contains("BLOCKED"), "must be non-blocking: {msg}");
            assert!(
                msg.contains("Proceeding"),
                "must allow the original command: {msg}"
            );
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
    fn accepts_before_tool_event_and_post_tool_use() {
        let empty = serde_json::json!({});
        assert!(is_guard_event(&empty, "PreToolUse"));
        assert!(is_guard_event(&empty, "BeforeTool"));
        // PostToolUse (blast-radius hook) is now a guard event too.
        assert!(is_guard_event(
            &serde_json::json!({"tool_name": "Edit", "tool_input": {}}),
            "PostToolUse"
        ));
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
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
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
        assert!(
            msg.contains("BLOCKED [PIXEL_SUBSTITUTE]"),
            "reason code missing for `{cmd}`: {msg}"
        );
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
        let msg = assert_substitute_contract("git commit -m 'fix the parser'", "pixel publish");
        assert!(
            msg.contains("--message 'fix the parser'"),
            "parsed -m must enrich the suggestion: {msg}"
        );
        assert!(msg.contains("--request-id <id>"), "{msg}");
        // --message form and -am cluster parse too.
        let msg = assert_substitute_contract("git commit --message 'x y'", "pixel publish");
        assert!(msg.contains("--message 'x y'"), "{msg}");
        // A single safe word stays bare through shell_quote.
        let msg = assert_substitute_contract("git commit -am 'both words here'", "pixel publish");
        assert!(msg.contains("--message 'both words here'"), "{msg}");
        assert!(
            msg.contains("-a detected"),
            "-a must enrich the suggestion: {msg}"
        );
    }

    #[test]
    fn substitute_commit_without_message_uses_placeholder() {
        let msg = assert_substitute_contract("git commit", "pixel publish");
        assert!(
            msg.contains("--message \"<msg>\""),
            "placeholder expected: {msg}"
        );
        assert!(
            msg.contains("--files <file>"),
            "files placeholder expected: {msg}"
        );
    }

    #[test]
    fn substitute_commit_pathspecs_become_files_flags() {
        let msg =
            assert_substitute_contract("git commit -m fix src/a.rs src/b.rs", "pixel publish");
        assert!(
            msg.contains("--files src/a.rs --files src/b.rs"),
            "each pathspec must be its own --files: {msg}"
        );
    }

    #[test]
    fn substitute_commit_amend_suggests_publish_amend() {
        let msg =
            assert_substitute_contract("git commit --amend -m better", "pixel publish --amend");
        assert!(msg.contains("--message better"), "{msg}");
    }

    #[test]
    fn substitute_push_plain_and_with_lease() {
        let msg = assert_substitute_contract(
            "git push origin main",
            "pixel push origin main --request-id <id>",
        );
        assert!(msg.contains("/repo"), "{msg}");
        // --force-with-lease is allowed by the destructive tier but IS
        // substitute-denied — pixel push carries the same lease semantics.
        assert!(
            bash_deny_lines(
                "git push --force-with-lease origin main",
                Some(Path::new("/repo"))
            )
            .is_none()
        );
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
        assert_substitute_contract(
            "git checkout -b feature/x",
            "pixel branch feature/x --request-id <id>",
        );
        assert_substitute_contract(
            "git switch -c feature/y",
            "pixel branch feature/y --request-id <id>",
        );
        assert_substitute_contract(
            "git switch --create feature/z",
            "pixel branch feature/z --request-id <id>",
        );
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
            assert!(
                sub(cmd).is_none(),
                "`{cmd}` must pass through the substitute tier"
            );
        }
    }

    #[test]
    fn substitute_add_with_pathspecs() {
        let msg = assert_substitute_contract("git add src/a.rs src/b.rs", "pixel publish");
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
        for cmd in [
            "git add -A",
            "git add --all",
            "git add -u",
            "git add --update",
        ] {
            let msg = assert_substitute_contract(cmd, "pixel publish");
            assert!(
                msg.contains("List each modified tracked file"),
                "`{cmd}` must suggest enumerating files: {msg}"
            );
        }
    }

    #[test]
    fn substitute_only_in_indexed_repo() {
        assert!(
            git_mutation_substitute_lines("git commit -m x", None, Path::new("/repo")).is_none()
        );
    }

    #[test]
    fn substitute_advisory_keeps_suggestion_and_allows_original() {
        let lines = sub("git commit -m x").unwrap();
        let advisory = non_blocking_advisory_lines(&lines).join("\n");
        assert!(
            !advisory.contains("BLOCKED"),
            "must not read as a deny: {advisory}"
        );
        assert!(advisory.contains("pixel-guard advisory"), "{advisory}");
        assert!(
            advisory.contains("pixel publish"),
            "suggestion must survive the downgrade: {advisory}"
        );
        assert!(
            advisory.contains("Proceeding"),
            "the original git command must remain available: {advisory}"
        );
    }

    #[test]
    fn substitute_runs_after_destructive_tier() {
        // Bare --force stays a destructive deny; it must never fall to the
        // softer substitute wording (check_bash consults bash_deny_lines
        // first, and the substitute tier's push arm can't even see it
        // in practice — but assert the destructive verdict directly).
        let repo = Path::new("/repo");
        let msg = bash_deny_lines("git push --force origin main", Some(repo))
            .unwrap()
            .join("\n");
        assert!(msg.contains("destroy remote history"), "{msg}");
    }

    // --- transcript escalation ------------------------------------------

    #[test]
    fn zcode_store_is_flagged() {
        let store = transcript_store_hit("sqlite3 ~/.zcode/cli/db/db.sqlite 'select 1'");
        assert_eq!(store, Some(".zcode/cli/db"));
        let msg = transcript_archaeology_advisory_lines(store.unwrap()).join("\n");
        assert!(
            msg.contains("Advisory") && !msg.contains("BLOCKED"),
            "{msg}"
        );
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
            assert!(
                !msg.contains("BLOCKED"),
                "Grep with `{field}` must not be blocked: {msg}"
            );
            assert!(
                msg.contains("Proceeding"),
                "Grep with `{field}` must proceed: {msg}"
            );
        }
    }

    #[test]
    fn grep_tool_gets_equivalent_command_as_advisory() {
        let repo = scratch_repo("grep-advisory");
        std::fs::create_dir_all(repo.join(".pixel")).unwrap();
        let mut input = serde_json::Map::new();
        input.insert("pattern".to_string(), Value::String("foo".to_string()));
        let msg = grep_redirect_advisory_lines("foo", &repo, &input).join("\n");
        assert!(
            !msg.contains("BLOCKED"),
            "equivalent Grep must not be blocked: {msg}"
        );
        assert!(
            msg.contains("pixel search"),
            "advisory should show the Pixel equivalent: {msg}"
        );
        assert!(
            msg.contains("Proceeding with the original Grep call"),
            "{msg}"
        );
    }

    // --- sequencer pass-through for `git add` ---------------------------

    /// Create a real git repo in a temp dir and return its root path.
    fn real_repo(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("pixel-guard-seq-{}-{}", name, std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::process::Command::new("git")
            .arg("init")
            .arg("-q")
            .arg("-b")
            .arg("main")
            .arg(&root)
            .status()
            .unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["config", "user.email", "t@t"])
            .status()
            .unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["config", "user.name", "t"])
            .status()
            .unwrap();
        std::fs::write(root.join("a.txt"), b"a").unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["add", "."])
            .status()
            .unwrap();
        std::process::Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["commit", "-qm", "init"])
            .status()
            .unwrap();
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
            git_mutation_substitute_lines("git commit -m 'resolve merge'", Some(&root), &root)
                .is_none(),
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

    /// The kill switches (`PIXEL_DAEMON_AUTO_START=0`, `PIXEL_GUARD_*=off`)
    /// fire only on an explicit off value: unset and any other value keep
    /// the feature on.
    #[test]
    fn env_flag_off_fires_only_on_an_explicit_off_value() {
        let name = format!("PIXEL_TEST_FLAG_{}_{}", std::process::id(), line!());
        assert!(!env_flag_off(&name), "unset");
        for (value, expected) in [
            ("0", true),
            ("false", true),
            ("off", true),
            ("1", false),
            ("", false),
            ("no", false),
        ] {
            // SAFETY: the variable name is unique to this test (pid + line),
            // so no other thread in the process reads or writes it.
            unsafe {
                std::env::set_var(&name, value);
            }
            assert_eq!(env_flag_off(&name), expected, "{value:?}");
        }
        // SAFETY: as above.
        unsafe {
            std::env::remove_var(&name);
        }
    }

    /// A composed hook runs for a tool when its matcher is the catch-all or
    /// a regex matching the tool name; anything else (or a broken regex)
    /// leaves the hook out.
    #[test]
    fn composed_matches_accepts_catch_alls_and_matching_regexes_only() {
        for catch_all in ["", "*", ".*"] {
            assert!(composed_matches(catch_all, "Bash"), "{catch_all:?}");
        }
        assert!(composed_matches("Bash", "Bash"));
        assert!(composed_matches("Bash|Edit", "Edit"));
        assert!(!composed_matches("Edit", "Bash"));
        assert!(
            !composed_matches("(", "Bash"),
            "an invalid regex never matches"
        );
    }

    /// The branch name in a deny message comes from `.git/HEAD`; a detached
    /// HEAD or a missing file yields no name rather than a wrong one.
    #[test]
    fn current_branch_reads_the_symbolic_head_only() {
        let root = scratch_repo("current-branch");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        assert_eq!(current_branch(&root), None, "no HEAD file");
        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/feature/x\n").unwrap();
        assert_eq!(current_branch(&root).as_deref(), Some("feature/x"));
        std::fs::write(
            root.join(".git/HEAD"),
            "0123456789abcdef0123456789abcdef01234567\n",
        )
        .unwrap();
        assert_eq!(current_branch(&root), None, "detached HEAD");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The `cd <dir> && <body>` splitter must find the `&&` that separates
    /// the directory from the body, and only that one: an `&&` inside quotes
    /// belongs to the argument, a lone `&` is a background job.
    #[test]
    fn find_unquoted_double_amp_reports_the_first_separator_outside_quotes() {
        assert_eq!(find_unquoted_double_amp(""), None);
        assert_eq!(find_unquoted_double_amp("cargo test"), None);
        assert_eq!(
            find_unquoted_double_amp("a & b"),
            None,
            "a lone `&` is not a separator"
        );
        assert_eq!(find_unquoted_double_amp("&& b"), Some(0));
        assert_eq!(find_unquoted_double_amp("a && b"), Some(2));
        assert_eq!(
            find_unquoted_double_amp("a && b && c"),
            Some(2),
            "first, not last"
        );
        assert_eq!(find_unquoted_double_amp("'a && b' && c"), Some(9));
        assert_eq!(find_unquoted_double_amp("\"x&&y\" && z"), Some(7));
        assert_eq!(find_unquoted_double_amp("'unterminated && quote"), None);
        assert_eq!(
            find_unquoted_double_amp("a &x& b"),
            None,
            "the ampersands must be adjacent"
        );
        // Byte index, not char index: `é` is two bytes.
        assert_eq!(find_unquoted_double_amp("é && x"), Some(3));
        assert_eq!(&"é && x"[3..5], "&&");
    }

    #[test]
    fn strip_cd_prefix_uses_the_unquoted_separator() {
        let cwd = Path::new("/repo");
        let (dir, body) = strip_cd_prefix("cd /tmp/x && cargo test", cwd);
        assert_eq!((dir.as_path(), body), (Path::new("/tmp/x"), "cargo test"));
        let (dir, body) = strip_cd_prefix("cd 'a && b' && ls", cwd);
        assert_eq!((dir.as_path(), body), (Path::new("/repo/a && b"), "ls"));
        let (dir, body) = strip_cd_prefix("cd sub", cwd);
        assert_eq!((dir.as_path(), body), (Path::new("/repo"), "cd sub"));
    }
}

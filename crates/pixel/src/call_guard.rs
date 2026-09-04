//! Call history + circuit breaker for pixel CLI invocations.
//!
//! Prevents infinite loops and unbounded memory growth from agents that
//! repeatedly call pixel commands without making progress. The classic
//! failure modes (from research on LLM agent infinite loops):
//!
//! 1. **Hard loop** — same `pixel search "foo"` called 5 times with
//!    identical args. No dedupe → wasted turns.
//! 2. **Soft loop** — `pixel search "foo"`, then `pixel search "Foo"`,
//!    then `pixel search "FOO"` — minimal arg changes, no new signal.
//! 3. **Retry storm** — `pixel targets` fails or returns empty, agent
//!    retries with slightly different task descriptions 4-5 times.
//! 4. **Context growth** — `pixel context` called on every symbol in
//!    the manifest, burning context budget without bound.
//!
//! The call history is persisted to `.pixel/calls.json` so it survives
//! across invocations (each `pixel` call is a separate process). The
//! circuit breaker fires when repeated calls exceed thresholds, returning
//! a structured guidance message instead of executing the command.
//!
//! Fails open: if the call log can't be read/written, the command
//! proceeds normally. A guard that blocks work due to a filesystem error
//! is worse than no guard.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

/// Call log entry — one per pixel invocation.
#[derive(Debug, Clone)]
struct CallEntry {
    command: String,
    args_hash: String,
    timestamp: u64,
}

/// How long to keep call history (in seconds). Calls older than this
/// are pruned on each write. 10 minutes is enough to detect loops
/// within a single agent turn cycle without accumulating stale data
/// across long sessions.
const CALL_HISTORY_TTL_SECS: u64 = 600;

/// Hard loop threshold: if the same (command, args_hash) has this many
/// PRIOR calls within the TTL window, the circuit breaker fires on the
/// next call. 2 prior + this call = 3 total identical calls = hard loop.
const HARD_LOOP_THRESHOLD: usize = 2;

/// Soft loop threshold: if this many PRIOR calls to the same command
/// (any args) exist within the TTL window, the circuit breaker fires.
/// 5 prior + this call = 6 total calls to `search` in 10 minutes.
const SOFT_LOOP_THRESHOLD: usize = 5;

/// Commands subject to the circuit breaker. `targets` is excluded
/// because re-running targets with a different task description is
/// legitimate (task evolution). `index`, `daemon`, `install`, `doctor`
/// are infrastructure commands, not retrieval.
const GUARDED_COMMANDS: &[&str] = &["search", "resolve", "context", "impact", "changes"];

/// Stable hash for call args (FNV-1a 64, hex, first 12 chars — same
/// scheme as `targets_task_id`). This is NOT a cryptographic hash; it
/// just needs to be deterministic so identical args produce identical
/// hashes.
fn args_hash(args: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in args.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")[..12].to_string()
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Load the call history from `.pixel/calls.json`. Returns an empty
/// vec on any error (missing file, corrupt JSON, etc.) — fails open.
fn load_calls(calls_path: &Path) -> Vec<CallEntry> {
    let Ok(text) = std::fs::read_to_string(calls_path) else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    v.get("calls")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|e| {
                    Some(CallEntry {
                        command: e.get("command")?.as_str()?.to_string(),
                        args_hash: e.get("args_hash")?.as_str()?.to_string(),
                        timestamp: e.get("timestamp")?.as_u64()?,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Save the call history to `.pixel/calls.json` (atomic tmp + rename).
/// Fails silently — a write error should not block the command.
fn save_calls(calls_path: &Path, calls: &[CallEntry]) {
    let arr: Vec<Value> = calls
        .iter()
        .map(|c| {
            serde_json::json!({
                "command": c.command,
                "args_hash": c.args_hash,
                "timestamp": c.timestamp,
            })
        })
        .collect();
    let body = serde_json::json!({ "calls": arr });
    if let Some(parent) = calls_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = calls_path.with_extension("json.tmp");
    if std::fs::write(&tmp, serde_json::to_vec_pretty(&body).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(&tmp, calls_path);
    }
}

/// Find the `.pixel` directory for a given path (walks up like the
/// guard does). Returns the path to `calls.json` inside it.
fn calls_path_for(anchor: &Path) -> Option<PathBuf> {
    let mut dir = anchor
        .canonicalize()
        .unwrap_or_else(|_| anchor.to_path_buf());
    loop {
        let candidate = dir.join(".pixel");
        if candidate.is_dir() {
            return Some(candidate.join("calls.json"));
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Result of the circuit breaker check.
pub enum CallGuardResult {
    /// Command may proceed normally.
    Allow,
    /// Circuit breaker fired — return this message to the agent
    /// instead of executing the command.
    Block(String),
}

/// Check if a pixel command should be allowed to proceed, and record
/// the call in history. Call this at the start of each guarded command
/// handler. If it returns `Block`, print the message and exit — do NOT
/// execute the command.
///
/// `command` is the subcommand name ("search", "resolve", etc.).
/// `args` is the stringified arguments (pattern + paths for search,
/// phrase + paths for resolve, etc.).
/// `cwd` is the current working directory (used to find `.pixel/`).
pub fn check_and_record(command: &str, args: &str, cwd: &Path) -> CallGuardResult {
    if !GUARDED_COMMANDS.contains(&command) {
        return CallGuardResult::Allow;
    }

    let Some(calls_path) = calls_path_for(cwd) else {
        // Not in a pixel-indexed directory — no call tracking, allow.
        return CallGuardResult::Allow;
    };

    let now = now_unix();
    let mut calls = load_calls(&calls_path);

    // Prune expired entries.
    calls.retain(|c| now.saturating_sub(c.timestamp) <= CALL_HISTORY_TTL_SECS);

    let ah = args_hash(args);

    // Check for hard loop: same (command, args_hash) appearing
    // HARD_LOOP_THRESHOLD times.
    let hard_count = calls
        .iter()
        .filter(|c| c.command == command && c.args_hash == ah)
        .count();
    if hard_count >= HARD_LOOP_THRESHOLD {
        let msg = format!(
            "CIRCUIT BREAKER: `pixel {command}` called {hard_count} times with the same arguments in the last 10 minutes.\n\
             This is a hard loop — the result will not change. Stop calling `pixel {command}` with these args.\n\
             \n\
             What to do instead:\n\
             - `pixel targets \"<natural language description>\" .` — re-scope with a description, not a guessed identifier\n\
             - Read the file directly if you know the path\n\
             - `web_search` if this is an external/dependency symbol\n\
             - Ask the user — the term may not exist in this codebase\n\
             \n\
             To reset the call history: rm .pixel/calls.json"
        );
        // Record this call too (so the count is visible if the agent
        // somehow retries), then save.
        calls.push(CallEntry {
            command: command.to_string(),
            args_hash: ah,
            timestamp: now,
        });
        save_calls(&calls_path, &calls);
        return CallGuardResult::Block(msg);
    }

    // Check for soft loop: same command (any args) appearing
    // SOFT_LOOP_THRESHOLD times.
    let soft_count = calls.iter().filter(|c| c.command == command).count();
    if soft_count >= SOFT_LOOP_THRESHOLD {
        let msg = format!(
            "CIRCUIT BREAKER: `pixel {command}` called {soft_count} times in the last 10 minutes (with varying arguments).\n\
             This is a soft loop — you're calling `{command}` too many times without making progress.\n\
             \n\
             What to do instead:\n\
             - You've already searched for this concept. Read the files from the manifest instead of searching more.\n\
             - If `pixel {command}` hasn't found it by now, the term likely doesn't exist in the indexed source.\n\
             - Switch to `pixel targets \"<natural language description>\" .` for concept-level retrieval\n\
             - Ask the user if you're stuck\n\
             \n\
             To reset the call history: rm .pixel/calls.json"
        );
        calls.push(CallEntry {
            command: command.to_string(),
            args_hash: ah,
            timestamp: now,
        });
        save_calls(&calls_path, &calls);
        return CallGuardResult::Block(msg);
    }

    // Record the call and proceed.
    calls.push(CallEntry {
        command: command.to_string(),
        args_hash: ah,
        timestamp: now,
    });
    save_calls(&calls_path, &calls);
    CallGuardResult::Allow
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("pixel-call-guard-test-{}-{n}", std::process::id()));
        std::fs::create_dir_all(dir.join(".pixel")).unwrap();
        dir
    }

    #[test]
    fn allows_first_call() {
        let dir = temp_dir();
        match check_and_record("search", "foo .", &dir) {
            CallGuardResult::Allow => {}
            CallGuardResult::Block(msg) => panic!("first call should be allowed: {msg}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn blocks_hard_loop() {
        let dir = temp_dir();
        // Call 3 times with same args (threshold is 3).
        check_and_record("search", "foo .", &dir);
        check_and_record("search", "foo .", &dir);
        match check_and_record("search", "foo .", &dir) {
            CallGuardResult::Block(msg) => {
                assert!(msg.contains("hard loop"), "must mention hard loop: {msg}");
            }
            CallGuardResult::Allow => panic!("third identical call must be blocked"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn blocks_soft_loop() {
        let dir = temp_dir();
        // Call 5 times with different args (threshold is 5).
        for i in 0..5 {
            check_and_record("search", &format!("query{i} ."), &dir);
        }
        match check_and_record("search", "another-query .", &dir) {
            CallGuardResult::Block(msg) => {
                assert!(msg.contains("soft loop"), "must mention soft loop: {msg}");
            }
            CallGuardResult::Allow => panic!("6th call must be blocked (soft loop)"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn does_not_block_different_commands() {
        let dir = temp_dir();
        // 4 searches + 4 resolves — neither hits the soft threshold.
        for i in 0..4 {
            check_and_record("search", &format!("q{i} ."), &dir);
            check_and_record("resolve", &format!("p{i} ."), &dir);
        }
        match check_and_record("search", "another .", &dir) {
            CallGuardResult::Allow => {}
            CallGuardResult::Block(msg) => {
                panic!("5th search with mixed commands should be allowed: {msg}")
            }
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unguarded_commands_always_allowed() {
        let dir = temp_dir();
        // `targets` is not in GUARDED_COMMANDS — unlimited calls.
        for _ in 0..20 {
            match check_and_record("targets", "fix the bug .", &dir) {
                CallGuardResult::Allow => {}
                CallGuardResult::Block(msg) => panic!("targets should not be guarded: {msg}"),
            }
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fails_open_without_pixel_dir() {
        let dir = std::env::temp_dir().join(format!("pixel-no-dotdir-{}", now_unix()));
        std::fs::create_dir_all(&dir).unwrap();
        match check_and_record("search", "foo .", &dir) {
            CallGuardResult::Allow => {}
            CallGuardResult::Block(msg) => panic!("must fail open without .pixel/: {msg}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn prunes_expired_entries() {
        let dir = temp_dir();
        let calls_path = dir.join(".pixel").join("calls.json");

        // Write a call from 20 minutes ago (past TTL).
        let old = serde_json::json!({
            "calls": [{
                "command": "search",
                "args_hash": args_hash("old ."),
                "timestamp": now_unix() - 1200,
            }]
        });
        std::fs::write(&calls_path, old.to_string()).unwrap();

        // A new call should NOT trigger the hard loop (old entry pruned).
        match check_and_record("search", "old .", &dir) {
            CallGuardResult::Allow => {}
            CallGuardResult::Block(msg) => panic!("expired entry must be pruned: {msg}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}

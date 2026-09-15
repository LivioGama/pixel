//! Advisory call history for read-only pixel CLI invocations.
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
//! emits a warning when repeated calls exceed thresholds. Counts are not
//! proof of a loop or unchanged repository state: retrieval always executes.
//!
//! Fails open: if the call log can't be read/written, the command
//! proceeds normally. A guard that blocks work due to a filesystem error
//! is worse than no guard.
//!
//! **Caller scoping.** The loop patterns above describe ONE agent going in
//! circles. A fan-out harness is the opposite: N sibling subagents each make a
//! single legitimate call, concurrently, in the same repo. Counted repo-wide
//! those are indistinguishable, and the breaker fires on agent 6 of 18 — real
//! work blocked by a loop detector. So each caller may declare an identity via
//! `PIXEL_SESSION_ID` (or `PIXEL_AGENT_ID`); counting is then scoped to that
//! identity and siblings never charge each other's budget. With no identity
//! declared, warnings use repo-wide counting. Neither case blocks retrieval.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

/// Call log entry — one per pixel invocation.
#[derive(Debug, Clone)]
struct CallEntry {
    command: String,
    args_hash: String,
    timestamp: u64,
    /// Caller identity from `PIXEL_SESSION_ID`; empty for unscoped callers
    /// and for entries written by older pixel versions.
    session: String,
}

/// How long to keep call history (in seconds). Calls older than this
/// are pruned on each write. 10 minutes is enough to detect loops
/// within a single agent turn cycle without accumulating stale data
/// across long sessions.
const CALL_HISTORY_TTL_SECS: u64 = 600;
/// Best-effort diagnostics, not an audit ledger. Bound memory/disk use even
/// during a high-volume fan-out; lost concurrent samples affect warnings only.
const MAX_CALL_HISTORY: usize = 2048;

/// Warn after this many prior identical calls; do not infer unchanged results.
const HARD_LOOP_THRESHOLD: usize = 2;

/// Warn after this many prior calls to the same command, regardless of args.
const SOFT_LOOP_THRESHOLD: usize = 5;

/// Commands subject to the circuit breaker. `targets` is excluded
/// because re-running targets with a different task description is
/// legitimate (task evolution). `index`, `daemon`, `install`, `doctor`
/// are infrastructure commands, not retrieval.
const GUARDED_COMMANDS: &[&str] = &[
    "search-content",
    "find-code",
    "pack-context",
    "impact",
    "what-changed",
];

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

/// Caller identity, or `""` when the caller did not declare one.
///
/// Parallel subagents in a fan-out set this to something unique per agent so
/// the breaker measures *that agent's* looping rather than the whole fleet's
/// combined traffic. Capped and sanitised because it lands in a JSON file.
fn session_id() -> String {
    let raw = std::env::var("PIXEL_SESSION_ID")
        .or_else(|_| std::env::var("PIXEL_AGENT_ID"))
        .unwrap_or_default();
    raw.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '#' | '~'))
        .take(64)
        .collect()
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
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
                        // Missing on entries written before caller scoping
                        // existed — treated as the unscoped caller.
                        session: e
                            .get("session")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
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
        .skip(calls.len().saturating_sub(MAX_CALL_HISTORY))
        .map(|c| {
            serde_json::json!({
                "command": c.command,
                "args_hash": c.args_hash,
                "timestamp": c.timestamp,
                "session": c.session,
            })
        })
        .collect();
    let body = serde_json::json!({ "calls": arr });
    if let Some(parent) = calls_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = calls_path.with_extension(format!("json.{}.tmp", std::process::id()));
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

/// Result of the advisory check. Both variants permit command execution.
pub enum CallGuardResult {
    /// Command may proceed normally.
    Allow,
    /// Print the diagnostic on stderr and continue the command.
    Warn(String),
}

/// Check if a pixel command should be allowed to proceed, and record
/// the call in history. Call this at the start of each guarded command
/// handler. If it returns `Warn`, print the message and still execute.
///
/// `command` is the subcommand name ("search-content", "find-code", etc.).
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
    let session = session_id();

    // Only this caller's own history counts toward its loop budget. Sibling
    // subagents in a fan-out each declare a distinct session, so one agent's
    // legitimate single call is never charged to another's budget. Callers
    // that declare nothing share the unscoped bucket, which is the pre-existing
    // repo-wide behaviour.
    let mine = |c: &&CallEntry| c.session == session;

    // Check for hard loop: same (command, args_hash) appearing
    // HARD_LOOP_THRESHOLD times.
    let hard_count = calls
        .iter()
        .filter(mine)
        .filter(|c| c.command == command && c.args_hash == ah)
        .count();
    if hard_count >= HARD_LOOP_THRESHOLD {
        let msg = format!(
            "note: `pixel {command}` has {hard_count} prior calls with identical arguments in 10 minutes; repository state may have changed. Continuing retrieval."
        );
        // Record this call too (so the count is visible if the agent
        // somehow retries), then save.
        calls.push(CallEntry {
            command: command.to_string(),
            args_hash: ah,
            timestamp: now,
            session,
        });
        save_calls(&calls_path, &calls);
        return CallGuardResult::Warn(msg);
    }

    // Check for soft loop: same command (any args) appearing
    // SOFT_LOOP_THRESHOLD times.
    let soft_count = calls
        .iter()
        .filter(mine)
        .filter(|c| c.command == command)
        .count();
    if soft_count >= SOFT_LOOP_THRESHOLD {
        let msg = format!(
            "note: `pixel {command}` has {soft_count} prior calls in 10 minutes; counts alone do not establish a loop. Continuing retrieval."
        );
        calls.push(CallEntry {
            command: command.to_string(),
            args_hash: ah,
            timestamp: now,
            session,
        });
        save_calls(&calls_path, &calls);
        return CallGuardResult::Warn(msg);
    }

    // Record the call and proceed.
    calls.push(CallEntry {
        command: command.to_string(),
        args_hash: ah,
        timestamp: now,
        session,
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

    /// Env mutation is process-global, so the session tests are serialised
    /// behind one lock and always restore the previous value.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_session<T>(id: Option<&str>, f: impl FnOnce() -> T) -> T {
        let _g = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let prev = std::env::var("PIXEL_SESSION_ID").ok();
        // ENV_LOCK is held for the whole call, so no other test reads or
        // writes PIXEL_SESSION_ID meanwhile; the previous value is restored
        // before the lock is released.
        match id {
            // SAFETY: under ENV_LOCK (see above).
            Some(v) => unsafe { std::env::set_var("PIXEL_SESSION_ID", v) },
            // SAFETY: under ENV_LOCK (see above).
            None => unsafe { std::env::remove_var("PIXEL_SESSION_ID") },
        }
        let out = f();
        match prev {
            // SAFETY: under ENV_LOCK (see above).
            Some(v) => unsafe { std::env::set_var("PIXEL_SESSION_ID", v) },
            // SAFETY: under ENV_LOCK (see above).
            None => unsafe { std::env::remove_var("PIXEL_SESSION_ID") },
        }
        out
    }

    fn seed_history(dir: &Path, entries: &[(&str, &str)]) {
        let now = now_unix();
        let calls: Vec<Value> = entries
            .iter()
            .map(|(command, args)| {
                serde_json::json!({
                    "command": command,
                    "args_hash": args_hash(args),
                    "timestamp": now,
                    "session": "",
                })
            })
            .collect();
        std::fs::write(
            dir.join(".pixel").join("calls.json"),
            serde_json::json!({ "calls": calls }).to_string(),
        )
        .unwrap();
    }

    #[test]
    fn history_under_a_command_name_counts_toward_the_current_one() {
        // Two identical `search-content` calls logged, then the same query:
        // that is the third identical call, a hard loop, not a first call.
        let warning = |result: &CallGuardResult| match result {
            CallGuardResult::Allow => None,
            CallGuardResult::Warn(msg) => Some(msg.clone()),
        };
        let dir = temp_dir();
        seed_history(
            &dir,
            &[("search-content", "foo ."), ("search-content", "foo .")],
        );
        let hard = with_session(None, || check_and_record("search-content", "foo .", &dir));
        let hard = warning(&hard);
        assert!(
            hard.as_deref()
                .is_some_and(|msg| msg.contains("identical arguments")),
            "name history must count: {hard:?}"
        );

        let dir2 = temp_dir();
        seed_history(
            &dir2,
            &[
                ("find-code", "a"),
                ("find-code", "b"),
                ("find-code", "c"),
                ("find-code", "d"),
                ("find-code", "e"),
            ],
        );
        let soft = with_session(None, || check_and_record("find-code", "f", &dir2));
        let soft = warning(&soft);
        assert!(
            soft.as_deref()
                .is_some_and(|msg| msg.contains("prior calls in 10 minutes")),
            "name history counts toward the soft threshold: {soft:?}"
        );

        // A new name passed in is guarded and recorded under the same name.
        let dir3 = temp_dir();
        let first = with_session(None, || check_and_record("pack-context", "uid .", &dir3));
        assert_eq!(warning(&first), None);
        let saved = load_calls(&dir3.join(".pixel").join("calls.json"));
        assert_eq!(saved.len(), 1, "a guarded name is still guarded");
        assert_eq!(saved[0].command, "pack-context");
        for d in [dir, dir2, dir3] {
            std::fs::remove_dir_all(&d).ok();
        }
    }

    #[test]
    fn allows_first_call() {
        let dir = temp_dir();
        with_session(None, || {
            match check_and_record("search-content", "foo .", &dir) {
                CallGuardResult::Allow => {}
                CallGuardResult::Warn(msg) => panic!("first call should be allowed: {msg}"),
            }
        });
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn warns_on_repeated_arguments() {
        let dir = temp_dir();
        // Call 3 times with same args (threshold is 3). Held under the env
        // lock: counting is scoped by PIXEL_SESSION_ID, and env is global, so
        // a concurrent scoped test would otherwise move these calls into a
        // different bucket.
        with_session(None, || {
            check_and_record("search-content", "foo .", &dir);
            check_and_record("search-content", "foo .", &dir);
            match check_and_record("search-content", "foo .", &dir) {
                CallGuardResult::Warn(msg) => {
                    assert!(
                        msg.contains("identical arguments"),
                        "must explain repeated arguments: {msg}"
                    );
                }
                CallGuardResult::Allow => panic!("third identical call must emit a warning"),
            }
        });
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn warns_on_frequent_retrieval() {
        let dir = temp_dir();
        with_session(None, || {
            // Call 5 times with different args (threshold is 5).
            for i in 0..5 {
                check_and_record("search-content", &format!("query{i} ."), &dir);
            }
            match check_and_record("search-content", "another-query .", &dir) {
                CallGuardResult::Warn(msg) => {
                    assert!(
                        msg.contains("counts alone"),
                        "must qualify the inference: {msg}"
                    );
                }
                CallGuardResult::Allow => panic!("6th call must emit a warning (soft loop)"),
            }
        });
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn does_not_block_different_commands() {
        let dir = temp_dir();
        with_session(None, || {
            // 4 searches + 4 resolves — neither hits the soft threshold.
            for i in 0..4 {
                check_and_record("search-content", &format!("q{i} ."), &dir);
                check_and_record("find-code", &format!("p{i} ."), &dir);
            }
            match check_and_record("search-content", "another .", &dir) {
                CallGuardResult::Allow => {}
                CallGuardResult::Warn(msg) => {
                    panic!("5th search with mixed commands should be allowed: {msg}")
                }
            }
        });
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unguarded_commands_always_allowed() {
        let dir = temp_dir();
        with_session(None, || {
            // `targets` is not in GUARDED_COMMANDS — unlimited calls.
            for _ in 0..20 {
                match check_and_record("scope-task", "fix the bug .", &dir) {
                    CallGuardResult::Allow => {}
                    CallGuardResult::Warn(msg) => panic!("targets should not be guarded: {msg}"),
                }
            }
        });
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fails_open_without_pixel_dir() {
        let dir = std::env::temp_dir().join(format!("pixel-no-dotdir-{}", now_unix()));
        std::fs::create_dir_all(&dir).unwrap();
        with_session(None, || {
            match check_and_record("search-content", "foo .", &dir) {
                CallGuardResult::Allow => {}
                CallGuardResult::Warn(msg) => panic!("must fail open without .pixel/: {msg}"),
            }
        });
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parallel_sessions_do_not_charge_each_other() {
        // The fan-out case: 18 sibling subagents each make ONE impact call in
        // the same repo. Repo-wide counting used to block everyone after the
        // 5th; scoped counting must let all 18 through.
        let dir = temp_dir();
        for i in 0..18 {
            let allowed = with_session(Some(&format!("agent-{i}")), || {
                matches!(
                    check_and_record("impact", &format!("symbol{i} ."), &dir),
                    CallGuardResult::Allow
                )
            });
            assert!(allowed, "sibling subagent {i} must not be blocked");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_single_session_still_trips_the_soft_loop() {
        // Scoping must not disable the guard: one agent looping is still caught.
        let dir = temp_dir();
        with_session(Some("looper"), || {
            for i in 0..SOFT_LOOP_THRESHOLD {
                check_and_record("impact", &format!("q{i} ."), &dir);
            }
            match check_and_record("impact", "another .", &dir) {
                CallGuardResult::Warn(msg) => assert!(msg.contains("counts alone"), "{msg}"),
                CallGuardResult::Allow => panic!("one session looping must still emit a warning"),
            }
        });
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_single_session_still_trips_the_hard_loop() {
        let dir = temp_dir();
        with_session(Some("looper2"), || {
            check_and_record("impact", "same .", &dir);
            check_and_record("impact", "same .", &dir);
            match check_and_record("impact", "same .", &dir) {
                CallGuardResult::Warn(msg) => assert!(msg.contains("identical arguments"), "{msg}"),
                CallGuardResult::Allow => panic!("identical repeats must still emit a warning"),
            }
        });
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unscoped_callers_keep_the_old_repo_wide_behaviour() {
        let dir = temp_dir();
        with_session(None, || {
            for i in 0..SOFT_LOOP_THRESHOLD {
                check_and_record("impact", &format!("q{i} ."), &dir);
            }
            match check_and_record("impact", "another .", &dir) {
                CallGuardResult::Warn(msg) => assert!(msg.contains("counts alone"), "{msg}"),
                CallGuardResult::Allow => panic!("unscoped warnings must remain caller-scoped"),
            }
        });
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn legacy_entries_without_session_belong_to_the_unscoped_bucket() {
        // calls.json written by an older pixel has no `session` key; those
        // entries must keep counting for unscoped callers and must NOT count
        // against a newly-scoped subagent.
        let dir = temp_dir();
        let calls_path = dir.join(".pixel").join("calls.json");
        let now = now_unix();
        let legacy: Vec<Value> = (0..SOFT_LOOP_THRESHOLD)
            .map(|i| {
                serde_json::json!({
                    "command": "impact",
                    "args_hash": args_hash(&format!("legacy{i} .")),
                    "timestamp": now,
                })
            })
            .collect();
        std::fs::write(
            &calls_path,
            serde_json::json!({ "calls": legacy }).to_string(),
        )
        .unwrap();

        let scoped_allowed = with_session(Some("fresh-agent"), || {
            matches!(
                check_and_record("impact", "mine .", &dir),
                CallGuardResult::Allow
            )
        });
        assert!(
            scoped_allowed,
            "legacy traffic must not block a new session"
        );

        let unscoped_warned = with_session(None, || {
            matches!(
                check_and_record("impact", "theirs .", &dir),
                CallGuardResult::Warn(_)
            )
        });
        assert!(unscoped_warned, "legacy traffic still counts unscoped");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn session_id_is_sanitised() {
        with_session(Some("agent/../../etc  \"evil\""), || {
            let id = session_id();
            assert!(!id.contains('/'), "path separators must be stripped: {id}");
            assert!(!id.contains('"'), "quotes must be stripped: {id}");
        });
    }

    #[test]
    fn diagnostics_are_bounded_and_never_claim_unchanged_results() {
        let dir = temp_dir();
        let path = dir.join(".pixel/calls.json");
        let calls: Vec<_> = (0..MAX_CALL_HISTORY + 100)
            .map(|i| CallEntry {
                command: "search-content".into(),
                args_hash: args_hash(&i.to_string()),
                timestamp: now_unix(),
                session: String::new(),
            })
            .collect();
        save_calls(&path, &calls);
        assert_eq!(load_calls(&path).len(), MAX_CALL_HISTORY);
        with_session(None, || {
            match check_and_record("search-content", "new query", &dir) {
                CallGuardResult::Warn(message) => {
                    assert!(message.contains("Continuing retrieval"));
                    assert!(!message.contains("result will not change"));
                    assert!(!message.contains("rm "));
                }
                CallGuardResult::Allow => panic!("frequent retrieval should warn"),
            }
        });
        assert_eq!(load_calls(&path).len(), MAX_CALL_HISTORY);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn prunes_expired_entries() {
        let dir = temp_dir();
        let calls_path = dir.join(".pixel").join("calls.json");

        // Write a call from 20 minutes ago (past TTL).
        let old = serde_json::json!({
            "calls": [{
                "command": "search-content",
                "args_hash": args_hash("old ."),
                "timestamp": now_unix() - 1200,
            }]
        });
        std::fs::write(&calls_path, old.to_string()).unwrap();

        // A new call should NOT trigger the hard loop (old entry pruned).
        with_session(None, || {
            match check_and_record("search-content", "old .", &dir) {
                CallGuardResult::Allow => {}
                CallGuardResult::Warn(msg) => panic!("expired entry must be pruned: {msg}"),
            }
        });
        std::fs::remove_dir_all(&dir).ok();
    }
}

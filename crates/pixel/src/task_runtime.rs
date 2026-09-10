//! Durable, bounded task packets for Claude Code hook delivery.
//!
//! This store is deliberately separate from `.pixel/targets.json`: targets is
//! an advisory cross-provider manifest, while this file records the active
//! Claude task for a particular hook session. Corrupt or unavailable state is
//! treated as absent so hook callers can always fail open.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

const STORE_VERSION: u8 = 1;
const MAX_SESSIONS: usize = 16;
const TTL_SECS: u64 = 24 * 60 * 60;
const MAX_SESSION_ID_BYTES: usize = 128;
const MAX_TASK_CHARS: usize = 1024;
const MAX_TARGETS: usize = 8;
const MAX_PATH_CHARS: usize = 512;
const MAX_EVIDENCE_CHARS: usize = 180;
const MIN_RENDER_BUDGET: usize = 256;
const LEDGER_VERSION: u8 = 1;
const MAX_PROVIDER_BYTES: usize = 32;
const MAX_EVENTS: usize = 256;
const MAX_TRANSITION_BYTES: usize = 64;

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct TaskTarget {
    pub(crate) path: String,
    pub(crate) tier: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) line: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) evidence: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct EvidenceSnapshot {
    pub(crate) revision: u64,
    pub(crate) reason: String,
    pub(crate) created_unix: u64,
    pub(crate) head_oid: String,
    pub(crate) targets: Vec<TaskTarget>,
    pub(crate) impact: String,
}

/// The factual packet injected into a Claude Code session.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct Packet {
    pub(crate) version: u8,
    pub(crate) task_id: String,
    pub(crate) session_id: String,
    pub(crate) generation: u64,
    pub(crate) revision: u64,
    pub(crate) task: String,
    pub(crate) head_oid: String,
    pub(crate) created_unix: u64,
    pub(crate) updated_unix: u64,
    pub(crate) evidence: EvidenceSnapshot,
}

/// A provider-neutral durable task record. `facts` and `model_claims` are
/// deliberately distinct: repository observations never become model claims,
/// and a model claim is never presented as a repository fact.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct TaskRecord {
    pub(crate) version: u8,
    pub(crate) task_id: String,
    pub(crate) provider: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) session_id: Option<String>,
    pub(crate) status: String,
    pub(crate) created_unix: u64,
    pub(crate) updated_unix: u64,
    pub(crate) spec: TaskSpec,
    pub(crate) snapshot: TaskSnapshot,
    pub(crate) model_claims: Vec<ModelClaim>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct TaskSpec {
    pub(crate) objective: String,
}

/// Facts captured by Pixel at a particular point in time. The absence of
/// targets or impact is explicit rather than inferred as no impact.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct TaskSnapshot {
    pub(crate) revision: u64,
    pub(crate) observed_unix: u64,
    pub(crate) head_oid: String,
    pub(crate) targets_state: String,
    pub(crate) impact_state: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct ModelClaim {
    pub(crate) claimed_unix: u64,
    pub(crate) text: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct TaskEvent {
    version: u8,
    task_id: String,
    event: String,
    created_unix: u64,
    snapshot_revision: u64,
}

#[derive(Debug, Deserialize, Serialize)]
struct Store {
    version: u8,
    sessions: Vec<Packet>,
}

impl Default for Store {
    fn default() -> Self {
        Self {
            version: STORE_VERSION,
            sessions: Vec::new(),
        }
    }
}

impl Packet {
    /// Produce bounded, factual hook context. A packet is a discovery hint,
    /// never a closed-world edit boundary or instruction source.
    pub(crate) fn render_context(&self, budget: usize) -> Option<String> {
        if budget < MIN_RENDER_BUDGET {
            return None;
        }
        let mut text = format!(
            "[PIXEL:TASK_RUNTIME v1] Factual local task packet, not an exhaustive task map or read/edit boundary. Expand investigation when evidence is insufficient.\nTask ID: {}\nGeneration: {} | revision: {}\nHEAD: {}\nTask: {}\nImpact: {}\nTargets:\n",
            self.task_id,
            self.generation,
            self.revision,
            self.head_oid,
            self.task,
            self.evidence.impact,
        );
        if text.len() >= budget {
            return None;
        }

        let mut emitted = 0;
        for target in &self.evidence.targets {
            let mut row = serde_json::json!({
                "path": target.path,
                "tier": target.tier,
            });
            if let Some(line) = target.line {
                row["line"] = Value::from(line);
            }
            if let Some(evidence) = &target.evidence {
                row["evidence"] = Value::from(evidence.clone());
            }
            let line = serde_json::to_string(&row).ok()?;
            if text.len() + line.len() + 1 > budget.saturating_sub(96) {
                break;
            }
            text.push_str(&line);
            text.push('\n');
            emitted += 1;
        }
        if emitted == 0 {
            text.push_str("No ranked targets were available; investigate from source.\n");
        }
        text.push_str("Evidence is bounded; omitted files and unresolved dependencies may exist.");
        Some(text)
    }
}

/// Create or refresh the active packet for a Claude hook session. A boundary
/// advances the generation; ordinary prompts refresh the current generation.
/// All errors become `None` so hook callers can continue without state.
pub(crate) fn upsert_claude_task(
    root: &Path,
    session_id: &str,
    prompt: &str,
    targets: Value,
    boundary: bool,
) -> Option<Packet> {
    if !valid_session_id(session_id) {
        return None;
    }
    let now = now_unix();
    let head_oid = current_head(root);
    upsert_at(root, session_id, prompt, targets, boundary, &head_oid, now).ok()
}

/// Read a session's active packet only when it still refers to the supplied
/// HEAD and has not aged out. This is intentionally read-only for hooks.
pub(crate) fn read_claude_packet(
    root: &Path,
    session_id: &str,
    head: &str,
    now: u64,
) -> Option<Packet> {
    if !valid_session_id(session_id) {
        return None;
    }
    let store = load_store(&store_path(root));
    store.sessions.into_iter().find(|packet| {
        packet.session_id == session_id
            && packet.head_oid == head
            && !expired(packet.updated_unix, now)
    })
}

/// JSON inspection entry point for the CLI. Invalid/corrupt stores read as
/// empty; filesystem errors while locating a root remain actionable to CLI.
pub(crate) fn show(path: &Path, session: &str) -> Result<Option<Value>, String> {
    let root = crate::discover_root(path)?;
    let head = current_head(&root);
    let packet = read_claude_packet(&root, session, &head, now_unix());
    packet
        .map(|entry| serde_json::to_value(entry).map_err(|e| e.to_string()))
        .transpose()
}

/// Remove exactly one Claude session packet. Unlike hook operations, CLI
/// callers receive an error when a requested state mutation cannot publish.
pub(crate) fn reset(path: &Path, session: &str) -> Result<bool, String> {
    if !valid_session_id(session) {
        return Err("invalid Claude session id".to_string());
    }
    let root = crate::discover_root(path)?;
    let state_path = store_path(&root);
    let mut store = load_store(&state_path);
    let before = store.sessions.len();
    store.sessions.retain(|packet| packet.session_id != session);
    if store.sessions.len() == before {
        return Ok(false);
    }
    save_store(&state_path, &store)?;
    Ok(true)
}

/// Start a provider-neutral task ledger. The initial snapshot intentionally
/// records only observations Pixel can make without asking a model to infer
/// scope; targets and impact stay explicitly uncollected until a later phase.
pub(crate) fn begin(
    root: &Path,
    objective: &str,
    provider: &str,
    session_id: Option<&str>,
) -> Result<TaskRecord, String> {
    create_task(root, objective, provider, session_id, "begun")
}

/// Atomically enough for a hard handoff: this returns an accepted task only
/// after the immutable task record and its acceptance event have both been
/// published. A caller that receives an error must fail open rather than claim
/// the foreground handoff happened.
pub(crate) fn accept_task(
    root: &Path,
    objective: &str,
    provider: &str,
    session_id: Option<&str>,
) -> Result<TaskRecord, String> {
    create_task(root, objective, provider, session_id, "accepted")
}

fn create_task(
    root: &Path,
    objective: &str,
    provider: &str,
    session_id: Option<&str>,
    status: &str,
) -> Result<TaskRecord, String> {
    let objective = objective.trim();
    if objective.is_empty() {
        return Err("task objective must not be empty".to_string());
    }
    if !valid_provider(provider) {
        return Err("invalid task provider".to_string());
    }
    if session_id.is_some_and(|session| !valid_session_id(session)) {
        return Err("invalid task session id".to_string());
    }

    let now = now_unix();
    let task_id = next_task_id(root, now)?;
    let record = TaskRecord {
        version: LEDGER_VERSION,
        task_id: task_id.clone(),
        provider: provider.to_string(),
        session_id: session_id.map(str::to_string),
        status: status.to_string(),
        created_unix: now,
        updated_unix: now,
        spec: TaskSpec {
            objective: truncate(objective, MAX_TASK_CHARS),
        },
        snapshot: TaskSnapshot {
            revision: 1,
            observed_unix: now,
            head_oid: current_head(root),
            targets_state: "not_collected".to_string(),
            impact_state: "not_collected".to_string(),
        },
        model_claims: Vec::new(),
    };
    save_task(root, &record)?;
    append_event(root, &record, status)?;
    Ok(record)
}

/// Refresh the factual repository snapshot for an existing task. It does not
/// manufacture targets or impact: those require their own deterministic Pixel
/// operations and are represented as absent here until collected.
pub(crate) fn prepare(root: &Path, task_id: &str) -> Result<Option<TaskRecord>, String> {
    let mut record = match load_task(root, task_id) {
        Some(record) => record,
        None => return Ok(None),
    };
    let now = now_unix();
    record.status = "prepared".to_string();
    record.updated_unix = now;
    record.snapshot.revision = record.snapshot.revision.saturating_add(1);
    record.snapshot.observed_unix = now;
    record.snapshot.head_oid = current_head(root);
    save_task(root, &record)?;
    append_event(root, &record, "prepared")?;
    Ok(Some(record))
}

/// Persist one scheduler-owned lifecycle transition. The returned record is
/// available only after both the atomic record replacement and append-only
/// factual event have been published. Model prose cannot enter this path.
pub(crate) fn transition(
    root: &Path,
    task_id: &str,
    status: &str,
    event: &str,
) -> Result<Option<TaskRecord>, String> {
    if !valid_transition(status) || !valid_transition(event) {
        return Err("invalid task transition".to_string());
    }
    let mut record = match load_task(root, task_id) {
        Some(record) => record,
        None => return Ok(None),
    };
    let now = now_unix();
    record.status = status.to_string();
    record.updated_unix = now;
    save_task(root, &record)?;
    append_event(root, &record, event)?;
    Ok(Some(record))
}

/// Read a durable task record. Bad ids, partial JSON, and future formats are
/// treated as absent so callers can safely fall back without trusting corrupt
/// state.
pub(crate) fn status(root: &Path, task_id: &str) -> Option<TaskRecord> {
    load_task(root, task_id)
}

/// Read bounded, parseable events. A torn/corrupt event line is ignored rather
/// than being promoted into a fabricated event.
pub(crate) fn events(root: &Path, task_id: &str) -> Vec<Value> {
    if !valid_task_id(task_id) {
        return Vec::new();
    }
    fs::read_to_string(events_path(root, task_id))
        .ok()
        .map(|raw| {
            raw.lines()
                .filter_map(|line| serde_json::from_str::<TaskEvent>(line).ok())
                .take(MAX_EVENTS)
                .filter_map(|event| serde_json::to_value(event).ok())
                .collect()
        })
        .unwrap_or_default()
}

fn upsert_at(
    root: &Path,
    session_id: &str,
    prompt: &str,
    targets: Value,
    boundary: bool,
    head_oid: &str,
    now: u64,
) -> Result<Packet, String> {
    let state_path = store_path(root);
    let mut store = load_store(&state_path);
    store
        .sessions
        .retain(|packet| !expired(packet.updated_unix, now));

    let prior = store
        .sessions
        .iter()
        .position(|packet| packet.session_id == session_id)
        .map(|index| store.sessions.remove(index));
    let generation = match &prior {
        Some(packet) if boundary => packet.generation.saturating_add(1),
        Some(packet) => packet.generation,
        None => 1,
    };
    let revision = prior
        .as_ref()
        .filter(|_| !boundary)
        .map(|packet| packet.revision.saturating_add(1))
        .unwrap_or(1);
    let packet = Packet {
        version: STORE_VERSION,
        task_id: format!("claude:{session_id}:{generation}"),
        session_id: session_id.to_string(),
        generation,
        revision,
        task: truncate(prompt.trim(), MAX_TASK_CHARS),
        head_oid: head_oid.to_string(),
        created_unix: prior
            .as_ref()
            .filter(|_| !boundary)
            .map(|packet| packet.created_unix)
            .unwrap_or(now),
        updated_unix: now,
        evidence: EvidenceSnapshot {
            revision,
            reason: if boundary {
                "task_boundary".to_string()
            } else if prior.is_some() {
                "prompt_refresh".to_string()
            } else {
                "initial_prompt".to_string()
            },
            created_unix: now,
            head_oid: head_oid.to_string(),
            targets: extract_targets(&targets),
            impact: "deferred_no_symbol".to_string(),
        },
    };
    store.sessions.push(packet.clone());
    store.sessions.sort_by_key(|entry| entry.updated_unix);
    if store.sessions.len() > MAX_SESSIONS {
        let overflow = store.sessions.len() - MAX_SESSIONS;
        store.sessions.drain(0..overflow);
    }
    save_store(&state_path, &store)?;
    Ok(packet)
}

fn store_path(root: &Path) -> PathBuf {
    root.join(".pixel").join("task-runtime.json")
}

fn tasks_root(root: &Path) -> PathBuf {
    root.join(".pixel").join("tasks")
}

fn task_dir(root: &Path, task_id: &str) -> PathBuf {
    tasks_root(root).join(task_id)
}

fn task_path(root: &Path, task_id: &str) -> PathBuf {
    task_dir(root, task_id).join("task.json")
}

fn events_path(root: &Path, task_id: &str) -> PathBuf {
    task_dir(root, task_id).join("events.jsonl")
}

fn next_task_id(root: &Path, now: u64) -> Result<String, String> {
    for _ in 0..1024 {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let candidate = format!("task-{now}-{sequence}");
        if !task_path(root, &candidate).exists() {
            return Ok(candidate);
        }
    }
    Err("unable to allocate a unique task id".to_string())
}

fn load_task(root: &Path, task_id: &str) -> Option<TaskRecord> {
    if !valid_task_id(task_id) {
        return None;
    }
    fs::read_to_string(task_path(root, task_id))
        .ok()
        .and_then(|raw| serde_json::from_str::<TaskRecord>(&raw).ok())
        .filter(|record| record.version == LEDGER_VERSION && record.task_id == task_id)
}

fn save_task(root: &Path, record: &TaskRecord) -> Result<(), String> {
    let path = task_path(root, &record.task_id);
    save_json_atomic(&path, record)
}

fn append_event(root: &Path, record: &TaskRecord, event: &str) -> Result<(), String> {
    let path = events_path(root, &record.task_id);
    let parent = path
        .parent()
        .ok_or_else(|| format!("task event path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    let entry = TaskEvent {
        version: LEDGER_VERSION,
        task_id: record.task_id.clone(),
        event: event.to_string(),
        created_unix: record.updated_unix,
        snapshot_revision: record.snapshot.revision,
    };
    let mut body = serde_json::to_vec(&entry).map_err(|e| e.to_string())?;
    body.push(b'\n');
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("open {}: {e}", path.display()))?;
    file.write_all(&body)
        .map_err(|e| format!("append {}: {e}", path.display()))
}

fn load_store(path: &Path) -> Store {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<Store>(&raw).ok())
        .filter(|store| store.version == STORE_VERSION)
        .unwrap_or_default()
}

fn save_store(path: &Path, store: &Store) -> Result<(), String> {
    save_json_atomic(path, store)
}

fn save_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("task runtime path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temp = parent.join(format!(
        ".pixel-task.{}.{}.tmp",
        std::process::id(),
        sequence
    ));
    let body = serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?;
    fs::write(&temp, body).map_err(|e| format!("write {}: {e}", temp.display()))?;
    fs::rename(&temp, path).map_err(|e| format!("publish {}: {e}", path.display()))
}

fn extract_targets(data: &Value) -> Vec<TaskTarget> {
    data.get("targets")
        .and_then(Value::as_array)
        .map(|targets| {
            targets
                .iter()
                .filter_map(|target| {
                    let path = target.get("path")?.as_str()?;
                    if path.is_empty() {
                        return None;
                    }
                    let tier = match target.get("tier").and_then(Value::as_str) {
                        Some("P0") => "P0",
                        Some("P2") => "P2",
                        _ => "P1",
                    };
                    let evidence = target
                        .get("evidence")
                        .and_then(Value::as_array)
                        .and_then(|items| items.first())
                        .and_then(|item| item.get("text"))
                        .and_then(Value::as_str)
                        .map(|text| truncate(text, MAX_EVIDENCE_CHARS));
                    let line = target
                        .get("evidence")
                        .and_then(Value::as_array)
                        .and_then(|items| items.first())
                        .and_then(|item| item.get("line"))
                        .and_then(Value::as_u64);
                    Some(TaskTarget {
                        path: truncate(path, MAX_PATH_CHARS),
                        tier: tier.to_string(),
                        line,
                        evidence,
                    })
                })
                .take(MAX_TARGETS)
                .collect()
        })
        .unwrap_or_default()
}

fn current_head(root: &Path) -> String {
    pixel_git::GitRunner::new(root)
        .rev_parse_head()
        .unwrap_or_else(|| "unavailable".to_string())
}

fn valid_session_id(session: &str) -> bool {
    !session.is_empty()
        && session.len() <= MAX_SESSION_ID_BYTES
        && session
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn valid_provider(provider: &str) -> bool {
    !provider.is_empty()
        && provider.len() <= MAX_PROVIDER_BYTES
        && provider
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn valid_task_id(task_id: &str) -> bool {
    !task_id.is_empty()
        && task_id.len() <= 128
        && task_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn valid_transition(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_TRANSITION_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn expired(updated_unix: u64, now: u64) -> bool {
    now.saturating_sub(updated_unix) > TTL_SECS
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn truncate(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "pixel-task-runtime-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join(".pixel")).unwrap();
        root
    }

    fn targets(path: &str) -> Value {
        serde_json::json!({"targets":[{
            "path": path,
            "tier":"P0",
            "evidence":[{"line":7,"text":"relevant implementation evidence"}]
        }]})
    }

    #[test]
    fn starts_then_refreshes_same_generation_with_bounded_evidence() {
        let root = root("refresh");
        let first = upsert_at(
            &root,
            "session-1",
            " first task ",
            targets("src/a.rs"),
            false,
            "abc",
            100,
        )
        .unwrap();
        let refreshed = upsert_at(
            &root,
            "session-1",
            "second prompt",
            targets("src/b.rs"),
            false,
            "abc",
            101,
        )
        .unwrap();

        assert_eq!(first.task_id, "claude:session-1:1");
        assert_eq!(refreshed.generation, 1);
        assert_eq!(refreshed.revision, 2);
        assert_eq!(refreshed.created_unix, 100);
        assert_eq!(refreshed.evidence.reason, "prompt_refresh");
        assert_eq!(refreshed.evidence.targets[0].path, "src/b.rs");
        assert_eq!(
            read_claude_packet(&root, "session-1", "abc", 101),
            Some(refreshed)
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn boundary_starts_new_generation_and_revision_one() {
        let root = root("boundary");
        upsert_at(
            &root,
            "session-1",
            "first",
            targets("src/a.rs"),
            false,
            "abc",
            100,
        )
        .unwrap();
        let next = upsert_at(
            &root,
            "session-1",
            "new task",
            targets("src/b.rs"),
            true,
            "abc",
            101,
        )
        .unwrap();

        assert_eq!(next.task_id, "claude:session-1:2");
        assert_eq!(next.generation, 2);
        assert_eq!(next.revision, 1);
        assert_eq!(next.created_unix, 101);
        assert_eq!(next.evidence.reason, "task_boundary");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn mismatched_head_and_expired_packet_do_not_restore() {
        let root = root("freshness");
        upsert_at(
            &root,
            "session-1",
            "task",
            targets("src/a.rs"),
            false,
            "abc",
            100,
        )
        .unwrap();

        assert!(read_claude_packet(&root, "session-1", "def", 101).is_none());
        assert!(read_claude_packet(&root, "session-1", "abc", 100 + TTL_SECS + 1).is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cap_evicts_oldest_session_and_invalid_or_corrupt_state_fails_open() {
        let root = root("cap");
        for index in 0..=MAX_SESSIONS {
            upsert_at(
                &root,
                &format!("session-{index}"),
                "task",
                targets("src/a.rs"),
                false,
                "abc",
                100 + index as u64,
            )
            .unwrap();
        }
        let store = load_store(&store_path(&root));
        assert_eq!(store.sessions.len(), MAX_SESSIONS);
        assert!(
            store
                .sessions
                .iter()
                .all(|packet| packet.session_id != "session-0")
        );

        std::fs::write(store_path(&root), "not json").unwrap();
        assert!(read_claude_packet(&root, "session-1", "abc", 200).is_none());
        assert!(
            upsert_claude_task(&root, "bad/session", "task", targets("src/a.rs"), false).is_none()
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rendered_packet_is_bounded_and_says_evidence_is_not_a_boundary() {
        let root = root("render");
        let packet = upsert_at(
            &root,
            "session-1",
            "task",
            targets("src/a.rs"),
            false,
            "abc",
            100,
        )
        .unwrap();
        let rendered = packet.render_context(600).unwrap();

        assert!(rendered.len() <= 600);
        assert!(rendered.contains("[PIXEL:TASK_RUNTIME v1]"));
        assert!(rendered.contains("not an exhaustive task map"));
        assert!(packet.render_context(100).is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ledger_keeps_facts_separate_from_claims_and_replays_events() {
        let root = root("ledger");
        let begun = begin(&root, "add task ledger", "claude", Some("session-1")).unwrap();

        assert_eq!(begun.status, "begun");
        assert_eq!(begun.model_claims, Vec::<ModelClaim>::new());
        assert_eq!(begun.snapshot.targets_state, "not_collected");
        assert_eq!(status(&root, &begun.task_id), Some(begun.clone()));
        assert_eq!(events(&root, &begun.task_id).len(), 1);

        let prepared = prepare(&root, &begun.task_id).unwrap().unwrap();
        assert_eq!(prepared.status, "prepared");
        assert_eq!(prepared.snapshot.revision, 2);
        let events = events(&root, &begun.task_id);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["event"], "begun");
        assert_eq!(events[1]["event"], "prepared");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn corrupt_task_and_event_state_fail_open() {
        let root = root("corrupt-ledger");
        let record = begin(&root, "task", "claude", None).unwrap();
        std::fs::write(task_path(&root, &record.task_id), "not json").unwrap();
        std::fs::write(events_path(&root, &record.task_id), "not json\n").unwrap();

        assert!(status(&root, &record.task_id).is_none());
        assert!(events(&root, &record.task_id).is_empty());
        assert!(prepare(&root, &record.task_id).unwrap().is_none());
        assert!(begin(&root, "task", "bad/provider", None).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn acceptance_returns_only_after_durable_acceptance_event() {
        let root = root("accept");
        let accepted = accept_task(&root, "run controller", "claude", Some("session-1")).unwrap();

        assert_eq!(accepted.status, "accepted");
        assert_eq!(status(&root, &accepted.task_id), Some(accepted.clone()));
        assert_eq!(events(&root, &accepted.task_id)[0]["event"], "accepted");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn transition_updates_status_and_appends_a_factual_event() {
        let root = root("transition");
        let begun = begin(&root, "run scheduler", "claude", None).unwrap();
        let running = transition(&root, &begun.task_id, "running", "worker_started")
            .unwrap()
            .unwrap();

        assert_eq!(running.status, "running");
        assert_eq!(events(&root, &begun.task_id)[1]["event"], "worker_started");
        assert!(
            transition(&root, "missing", "running", "worker_started")
                .unwrap()
                .is_none()
        );
        assert!(transition(&root, &begun.task_id, "not valid", "worker_started").is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}

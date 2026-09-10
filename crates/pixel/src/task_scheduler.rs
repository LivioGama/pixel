//! Deterministic lifecycle control for one isolated Claude task worker.
//!
//! The scheduler does not plan, race, or infer ownership. It can only launch a
//! task already durably accepted by Pixel into a registered sandbox, and it
//! records process facts independently from model output.

use std::fs;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

const WORKER_VERSION: u8 = 1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WorkerConfig {
    pub(crate) executable: PathBuf,
    pub(crate) model: Option<String>,
    pub(crate) max_turns: Option<u32>,
    pub(crate) max_budget_usd: Option<String>,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            executable: PathBuf::from("claude"),
            model: None,
            max_turns: None,
            max_budget_usd: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct WorkerRecord {
    pub(crate) version: u8,
    pub(crate) task_id: String,
    pub(crate) candidate_id: String,
    pub(crate) pid: u32,
    pub(crate) process_group: i32,
    pub(crate) started_unix: u64,
    pub(crate) updated_unix: u64,
    pub(crate) state: WorkerState,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WorkerState {
    Running,
    Exited,
    Failed,
    Stopped,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct WorkerStatus {
    pub(crate) record: WorkerRecord,
    pub(crate) alive: bool,
}

/// Launch one worker only after both task acceptance and sandbox registration
/// have been verified. The spawned child is made leader of its own process
/// group, allowing stop to terminate the worker and descendants together.
pub(crate) fn start(
    root: &Path,
    task_id: &str,
    candidate_id: &str,
    config: &WorkerConfig,
) -> Result<WorkerRecord, String> {
    let task = crate::task_runtime::status(root, task_id)
        .ok_or_else(|| "task is absent or corrupt".to_string())?;
    if task.provider != "claude" {
        return Err("worker start supports only Claude tasks".to_string());
    }
    if task.status != "accepted" {
        return Err(format!(
            "task must be accepted before worker start (status: {})",
            task.status
        ));
    }
    let candidate = crate::task_sandbox::load(root, task_id, candidate_id)?
        .ok_or_else(|| "candidate sandbox is absent".to_string())?;
    if candidate.task_id != task_id || !candidate.sandbox_root.is_dir() {
        return Err("candidate sandbox is invalid".to_string());
    }

    if let Some(existing) = load(root, task_id, candidate_id)?
        && existing.state == WorkerState::Running && process_alive(existing.pid) {
            return Err(format!("worker is already running (pid {})", existing.pid));
        }

    let record = spawn_candidate(
        root,
        task_id,
        candidate_id,
        &candidate,
        &task.spec.objective,
        config,
    )?;
    if let Err(error) =
        crate::task_runtime::transition(root, task_id, "worker_running", "worker_started").and_then(
            |record| record.ok_or_else(|| "task disappeared before worker start".to_string()),
        )
    {
        let _ = terminate_group(record.process_group, record.pid);
        let _ = fs::remove_file(worker_path(root, task_id, candidate_id));
        return Err(error);
    }
    Ok(record)
}

/// Start a pre-registered race group. Pixel does not invent candidates: every
/// id must already resolve to a sandbox created with explicit ownership.
pub(crate) fn start_race(
    root: &Path,
    task_id: &str,
    candidate_ids: &[String],
    config: &WorkerConfig,
) -> Result<RaceStart, String> {
    let task = crate::task_runtime::status(root, task_id)
        .ok_or_else(|| "task is absent or corrupt".to_string())?;
    if task.provider != "claude" || task.status != "accepted" {
        return Err("race requires an accepted Claude task".to_string());
    }
    if candidate_ids.len() < 2 {
        return Err("race requires at least two pre-registered candidates".to_string());
    }
    let mut started = Vec::new();
    let mut failures = Vec::new();
    for candidate_id in candidate_ids {
        let candidate = match crate::task_sandbox::load(root, task_id, candidate_id) {
            Ok(Some(candidate)) => candidate,
            Ok(None) => {
                failures.push(format!("{candidate_id}:sandbox_absent"));
                continue;
            }
            Err(error) => {
                failures.push(format!("{candidate_id}:{error}"));
                continue;
            }
        };
        match spawn_candidate(
            root,
            task_id,
            candidate_id,
            &candidate,
            &task.spec.objective,
            config,
        ) {
            Ok(record) => started.push(record),
            Err(error) => failures.push(format!("{candidate_id}:{error}")),
        }
    }
    if started.is_empty() {
        let _ = crate::task_runtime::transition(root, task_id, "race_failed", "race_failed");
        return Ok(RaceStart { started, failures });
    }
    let event = if failures.is_empty() {
        "race_started"
    } else {
        "race_started_partial"
    };
    crate::task_runtime::transition(root, task_id, "race_running", event)?
        .ok_or_else(|| "task disappeared before race start".to_string())?;
    Ok(RaceStart { started, failures })
}

/// Poll one race group and promote only the first completed eligible candidate.
/// A completion message is not evidence: `task_sandbox::inspect` verifies the
/// actual worktree diff and ownership before promotion.
pub(crate) fn poll_race(
    root: &Path,
    task_id: &str,
    candidate_ids: &[String],
) -> Result<RacePoll, String> {
    let mut terminal = Vec::new();
    let mut running = 0usize;
    for candidate_id in candidate_ids {
        let Some(status) = status(root, task_id, candidate_id)? else {
            continue;
        };
        if status.alive {
            running += 1;
            continue;
        }
        if matches!(
            status.record.state,
            WorkerState::Exited | WorkerState::Failed | WorkerState::Stopped
        ) {
            terminal.push(candidate_id.clone());
        }
    }
    for candidate_id in &terminal {
        let Some(candidate) = crate::task_sandbox::load(root, task_id, candidate_id)? else {
            continue;
        };
        let inspection = crate::task_sandbox::inspect(&candidate)?;
        if inspection.verdict == crate::task_sandbox::CandidateVerdict::Eligible {
            let promoted = crate::task_sandbox::promote(&candidate)?;
            if promoted.verdict == crate::task_sandbox::CandidateVerdict::Promoted {
                for loser in candidate_ids.iter().filter(|id| *id != candidate_id) {
                    stop(root, task_id, loser)?;
                }
                crate::task_runtime::transition(root, task_id, "race_won", "race_won")?;
                return Ok(RacePoll {
                    winner: Some(candidate_id.clone()),
                    running,
                    terminal,
                    no_winner: false,
                });
            }
        }
    }
    let all_terminal = running == 0
        && candidate_ids.iter().all(|candidate_id| {
            load(root, task_id, candidate_id)
                .ok()
                .flatten()
                .is_some_and(|record| record.state != WorkerState::Running)
        });
    if all_terminal {
        crate::task_runtime::transition(root, task_id, "race_no_winner", "race_no_winner")?;
    }
    Ok(RacePoll {
        winner: None,
        running,
        terminal,
        no_winner: all_terminal,
    })
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct RaceStart {
    pub(crate) started: Vec<WorkerRecord>,
    pub(crate) failures: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct RacePoll {
    pub(crate) winner: Option<String>,
    pub(crate) running: usize,
    pub(crate) terminal: Vec<String>,
    pub(crate) no_winner: bool,
}

fn spawn_candidate(
    root: &Path,
    task_id: &str,
    candidate_id: &str,
    candidate: &crate::task_sandbox::Candidate,
    objective: &str,
    config: &WorkerConfig,
) -> Result<WorkerRecord, String> {
    if let Some(existing) = load(root, task_id, candidate_id)?
        && existing.state == WorkerState::Running && process_alive(existing.pid) {
            return Err(format!("worker is already running (pid {})", existing.pid));
        }
    let launch = crate::claude_controller::ClaudeWorkerLaunch {
        task_id,
        worktree_id: candidate_id,
        worktree: &candidate.sandbox_root,
        objective,
    };
    let options = crate::claude_controller::ClaudeWorkerOptions {
        executable: &config.executable,
        model: config.model.as_deref(),
        max_turns: config.max_turns,
        max_budget_usd: config.max_budget_usd.as_deref(),
    };
    let mut command =
        crate::claude_controller::build_worker_command_with_options(&launch, &options)?;
    // This scheduler can be called by a JSON-producing hook. Worker output is
    // deliberately discarded until Pixel has a dedicated log sink; inheriting
    // hook stdout/stderr would corrupt the provider protocol.
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    configure_process_group(&mut command)?;
    let child = command
        .spawn()
        .map_err(|error| format!("spawn Claude worker: {error}"))?;
    let now = now_unix();
    let record = WorkerRecord {
        version: WORKER_VERSION,
        task_id: task_id.to_string(),
        candidate_id: candidate_id.to_string(),
        pid: child.id(),
        process_group: child.id() as i32,
        started_unix: now,
        updated_unix: now,
        state: WorkerState::Running,
    };
    if let Err(error) = save(root, &record) {
        let _ = terminate_group(record.process_group, record.pid);
        let _ = fs::remove_file(worker_path(root, task_id, candidate_id));
        return Err(error);
    }
    Ok(record)
}

/// Return current process truth. A dead running PID is factually transitioned
/// to `exited`; exit status cannot be fabricated without a dedicated reaper.
pub(crate) fn status(
    root: &Path,
    task_id: &str,
    candidate_id: &str,
) -> Result<Option<WorkerStatus>, String> {
    let mut record = match load(root, task_id, candidate_id)? {
        Some(record) => record,
        None => return Ok(None),
    };
    let alive = record.state == WorkerState::Running && process_alive(record.pid);
    if record.state == WorkerState::Running && !alive {
        record.state = WorkerState::Exited;
        record.updated_unix = now_unix();
        save(root, &record)?;
        let _ = crate::task_runtime::transition(root, task_id, "worker_exited", "worker_exited");
    }
    Ok(Some(WorkerStatus { record, alive }))
}

/// Send SIGTERM to the dedicated worker process group. This is safe to repeat;
/// no PID is killed unless it is still the recorded running task worker.
pub(crate) fn stop(
    root: &Path,
    task_id: &str,
    candidate_id: &str,
) -> Result<Option<WorkerRecord>, String> {
    let mut record = match load(root, task_id, candidate_id)? {
        Some(record) => record,
        None => return Ok(None),
    };
    if record.state == WorkerState::Running && process_alive(record.pid) {
        terminate_group(record.process_group, record.pid)?;
    }
    record.state = WorkerState::Stopped;
    record.updated_unix = now_unix();
    save(root, &record)?;
    let _ = crate::task_runtime::transition(root, task_id, "worker_stopped", "worker_stopped");
    Ok(Some(record))
}

pub(crate) fn load(
    root: &Path,
    task_id: &str,
    candidate_id: &str,
) -> Result<Option<WorkerRecord>, String> {
    let path = worker_path(root, task_id, candidate_id);
    if !path.exists() {
        return Ok(None);
    }
    fs::read_to_string(&path)
        .map_err(|error| format!("read {}: {error}", path.display()))
        .and_then(|body| {
            serde_json::from_str::<WorkerRecord>(&body).map_err(|error| error.to_string())
        })
        .map(|record| {
            (record.version == WORKER_VERSION
                && record.task_id == task_id
                && record.candidate_id == candidate_id)
                .then_some(record)
        })
}

fn save(root: &Path, record: &WorkerRecord) -> Result<(), String> {
    let path = worker_path(root, &record.task_id, &record.candidate_id);
    let parent = path
        .parent()
        .ok_or_else(|| "worker path has no parent".to_string())?;
    fs::create_dir_all(parent).map_err(|error| format!("create {}: {error}", parent.display()))?;
    let temp = parent.join(format!(".worker.{}.{}.tmp", std::process::id(), record.pid));
    let body = serde_json::to_vec_pretty(record).map_err(|error| error.to_string())?;
    fs::write(&temp, body).map_err(|error| format!("write {}: {error}", temp.display()))?;
    fs::rename(&temp, &path).map_err(|error| format!("publish {}: {error}", path.display()))
}

fn worker_path(root: &Path, task_id: &str, candidate_id: &str) -> PathBuf {
    root.join(".pixel")
        .join("tasks")
        .join(task_id)
        .join("workers")
        .join(format!("{candidate_id}.json"))
}

fn configure_process_group(command: &mut Command) -> Result<(), String> {
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        });
    }
    Ok(())
}

fn process_alive(pid: u32) -> bool {
    unsafe {
        if libc::kill(pid as i32, 0) == 0 {
            true
        } else {
            io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
        }
    }
}

fn terminate_group(process_group: i32, pid: u32) -> Result<(), String> {
    let result = unsafe { libc::kill(-process_group, libc::SIGTERM) };
    if result == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    let fallback = unsafe { libc::kill(pid as i32, libc::SIGTERM) };
    if fallback == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(format!(
            "terminate worker {pid}: {}",
            io::Error::last_os_error()
        ))
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn fixture(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("pixel-worker-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("owned.txt"), "before\n").unwrap();
        let run = |args: &[&str]| {
            let status = Command::new("git")
                .current_dir(&root)
                .args(args)
                .status()
                .unwrap();
            assert!(status.success());
        };
        run(&["init"]);
        run(&["config", "user.email", "pixel@example.test"]);
        run(&["config", "user.name", "Pixel"]);
        run(&["add", "."]);
        run(&["commit", "-m", "fixture"]);
        root
    }

    #[test]
    fn fake_worker_has_real_start_status_stop_lifecycle() {
        let root = fixture("lifecycle");
        let accepted =
            crate::task_runtime::accept_task(&root, "change owned", "claude", None).unwrap();
        let candidate = crate::task_sandbox::create(
            &root,
            &accepted.task_id,
            "candidate1",
            ["owned.txt".to_string()],
        )
        .unwrap();
        let fake = root.join("fake-claude");
        fs::write(
            &fake,
            "#!/bin/sh\ntrap 'exit 0' TERM\nwhile :; do sleep 1; done\n",
        )
        .unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        let config = WorkerConfig {
            executable: fake,
            model: Some("sonnet".to_string()),
            max_turns: Some(3),
            max_budget_usd: Some("1.25".to_string()),
        };

        let started = start(&root, &accepted.task_id, &candidate.candidate_id, &config).unwrap();
        assert_eq!(started.state, WorkerState::Running);
        assert!(
            status(&root, &accepted.task_id, &candidate.candidate_id)
                .unwrap()
                .unwrap()
                .alive
        );
        let stopped = stop(&root, &accepted.task_id, &candidate.candidate_id)
            .unwrap()
            .unwrap();
        assert_eq!(stopped.state, WorkerState::Stopped);
        assert_eq!(
            crate::task_runtime::status(&root, &accepted.task_id)
                .unwrap()
                .status,
            "worker_stopped"
        );
        let _ = crate::task_sandbox::cleanup(&root, &accepted.task_id, &candidate.candidate_id);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn race_promotes_first_eligible_candidate_and_cancels_loser() {
        let root = fixture("race-winner");
        let accepted =
            crate::task_runtime::accept_task(&root, "race change", "claude", None).unwrap();
        let winner = crate::task_sandbox::create(
            &root,
            &accepted.task_id,
            "winner",
            ["owned.txt".to_string()],
        )
        .unwrap();
        let loser = crate::task_sandbox::create(
            &root,
            &accepted.task_id,
            "loser",
            ["owned.txt".to_string()],
        )
        .unwrap();
        let fake = root.join("fake-race-claude");
        fs::write(&fake, "#!/bin/sh\nif [ \"$PIXEL_WORKTREE_ID\" = winner ]; then printf 'after\\n' > owned.txt; fi\ntrap 'exit 0' TERM\nwhile :; do sleep 1; done\n").unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        let config = WorkerConfig {
            executable: fake,
            ..WorkerConfig::default()
        };
        let ids = vec![winner.candidate_id.clone(), loser.candidate_id.clone()];

        let started = start_race(&root, &accepted.task_id, &ids, &config).unwrap();
        assert_eq!(started.started.len(), 2);
        // The fixture writes before entering its loop. Marking the winner
        // stopped simulates a reaped finished child within this in-process test.
        std::thread::sleep(std::time::Duration::from_millis(250));
        stop(&root, &accepted.task_id, &winner.candidate_id).unwrap();
        let outcome = poll_race(&root, &accepted.task_id, &ids).unwrap();
        // Keep the fixture cleanup deterministic even when an assertion below
        // exposes a race regression before the production cancellation check.
        let _ = stop(&root, &accepted.task_id, &loser.candidate_id);
        assert_eq!(outcome.winner.as_deref(), Some("winner"));
        assert_eq!(
            fs::read_to_string(root.join("owned.txt")).unwrap(),
            "after\n"
        );
        assert_eq!(
            load(&root, &accepted.task_id, &loser.candidate_id)
                .unwrap()
                .unwrap()
                .state,
            WorkerState::Stopped
        );
        let _ = crate::task_sandbox::cleanup(&root, &accepted.task_id, &winner.candidate_id);
        let _ = crate::task_sandbox::cleanup(&root, &accepted.task_id, &loser.candidate_id);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn race_records_no_winner_when_completed_candidates_have_no_diff() {
        let root = fixture("race-empty");
        let accepted =
            crate::task_runtime::accept_task(&root, "race no change", "claude", None).unwrap();
        let first = crate::task_sandbox::create(
            &root,
            &accepted.task_id,
            "first",
            ["owned.txt".to_string()],
        )
        .unwrap();
        let second = crate::task_sandbox::create(
            &root,
            &accepted.task_id,
            "second",
            ["owned.txt".to_string()],
        )
        .unwrap();
        let fake = root.join("fake-noop-claude");
        fs::write(
            &fake,
            "#!/bin/sh\ntrap 'exit 0' TERM\nwhile :; do sleep 1; done\n",
        )
        .unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        let config = WorkerConfig {
            executable: fake,
            ..WorkerConfig::default()
        };
        let ids = vec![first.candidate_id.clone(), second.candidate_id.clone()];
        start_race(&root, &accepted.task_id, &ids, &config).unwrap();
        stop(&root, &accepted.task_id, &first.candidate_id).unwrap();
        stop(&root, &accepted.task_id, &second.candidate_id).unwrap();
        let outcome = poll_race(&root, &accepted.task_id, &ids).unwrap();
        assert!(outcome.winner.is_none());
        assert!(outcome.no_winner);
        assert_eq!(
            crate::task_runtime::status(&root, &accepted.task_id)
                .unwrap()
                .status,
            "race_no_winner"
        );
        let _ = crate::task_sandbox::cleanup(&root, &accepted.task_id, &first.candidate_id);
        let _ = crate::task_sandbox::cleanup(&root, &accepted.task_id, &second.candidate_id);
        let _ = fs::remove_dir_all(root);
    }
}

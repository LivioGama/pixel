//! Provider adapter contracts for isolated Claude Code task workers.
//!
//! This module intentionally contains no process spawning and no hook wiring.
//! The ledger owns durable acceptance; the hook/controller integration may only
//! hand off after that acceptance succeeds. Keeping the decision and argv
//! construction pure makes failure fail open to the foreground provider.

use std::path::{Path, PathBuf};
use std::process::Command;

/// A request for durable ownership by the task ledger.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TaskAcceptanceRequest {
    pub(crate) provider: &'static str,
    pub(crate) session_id: String,
    pub(crate) objective: String,
    pub(crate) repository: PathBuf,
}

/// The only acceptance result that permits a foreground handoff.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TaskAcceptance {
    pub(crate) task_id: String,
    pub(crate) status: String,
}

/// Durable task acceptance boundary. Implementations must not report
/// `accepted` until the task record is transactionally visible to the ledger.
pub(crate) trait TaskAcceptor {
    fn accept(&self, request: &TaskAcceptanceRequest) -> Result<TaskAcceptance, String>;
}

/// Production bridge to Pixel's durable task ledger.
pub(crate) struct LedgerAcceptor;

impl TaskAcceptor for LedgerAcceptor {
    fn accept(&self, request: &TaskAcceptanceRequest) -> Result<TaskAcceptance, String> {
        let record = crate::task_runtime::accept_task(
            &request.repository,
            &request.objective,
            request.provider,
            Some(&request.session_id),
        )?;
        Ok(TaskAcceptance {
            task_id: record.task_id,
            status: record.status,
        })
    }
}

/// The hook's deterministic choice after asking the durable ledger.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum HandoffDecision {
    /// The normal provider handles the prompt; no background write was started.
    Foreground { reason: String },
    /// A ledger-owned task is durable and can be handed to the controller.
    Handoff { task_id: String, status: String },
}

/// Convert ledger evidence into an unambiguous handoff decision.
///
/// A bad/empty acceptance is deliberately treated exactly like a rejected or
/// unavailable ledger: the foreground prompt proceeds and no duplicate worker
/// is started.
pub(crate) fn decide_handoff(
    acceptor: &dyn TaskAcceptor,
    request: &TaskAcceptanceRequest,
) -> HandoffDecision {
    match acceptor.accept(request) {
        Ok(accepted) if !accepted.task_id.trim().is_empty() && accepted.status == "accepted" => {
            HandoffDecision::Handoff {
                task_id: accepted.task_id,
                status: accepted.status,
            }
        }
        Ok(accepted) => HandoffDecision::Foreground {
            reason: format!("ledger_not_accepted:{}", accepted.status),
        },
        Err(_) => HandoffDecision::Foreground {
            // Error details are intentionally not injected into hook output.
            reason: "ledger_unavailable".to_string(),
        },
    }
}

/// Immutable inputs required to construct one isolated Claude worker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ClaudeWorkerLaunch<'a> {
    pub(crate) task_id: &'a str,
    pub(crate) worktree_id: &'a str,
    pub(crate) worktree: &'a Path,
    pub(crate) objective: &'a str,
}

/// Limits selected by Pixel's deterministic scheduler. They map directly to
/// documented print-mode Claude CLI flags and are omitted when not configured.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ClaudeWorkerOptions<'a> {
    pub(crate) executable: &'a Path,
    pub(crate) model: Option<&'a str>,
    pub(crate) max_turns: Option<u32>,
    pub(crate) max_budget_usd: Option<&'a str>,
    /// Path to a file whose contents are appended to Claude's default system
    /// prompt via `--append-system-prompt-file`. Non-destructive: Claude's
    /// safety guidelines and tool descriptions remain intact. Used to inject
    /// Pixel retrieval rules so workers are Pixel-aware without relying on
    /// CLAUDE.md discovery.
    pub(crate) system_prompt_file: Option<&'a Path>,
    /// Path to a file appended to the system prompt of every sub-agent the
    /// worker spawns, via `--append-subagent-system-prompt-file`. Sub-agents
    /// do not inherit `system_prompt_file`, so without this they only see the
    /// repository's CLAUDE.md and their own agent body. The worker always runs
    /// in print mode, which is the only mode Claude Code honours the flag in.
    pub(crate) subagent_prompt_file: Option<&'a Path>,
}

/// Construct a worker command with explicit scheduler-provided limits. This
/// supports a fake executable in tests without changing production's OAuth
/// backed Claude invocation.
pub(crate) fn build_worker_command_with_options(
    launch: &ClaudeWorkerLaunch<'_>,
    options: &ClaudeWorkerOptions<'_>,
) -> Result<Command, String> {
    validate_launch(launch)?;
    if options.executable.as_os_str().is_empty() {
        return Err("Claude worker executable must not be empty".to_string());
    }
    if options.model.is_some_and(|model| model.trim().is_empty()) {
        return Err("Claude worker model must not be empty".to_string());
    }
    if options
        .max_budget_usd
        .is_some_and(|budget| budget.trim().is_empty())
    {
        return Err("Claude worker max budget must not be empty".to_string());
    }

    let mut command = Command::new(options.executable);
    command
        .current_dir(launch.worktree)
        .env("PIXEL_TASK_ID", launch.task_id)
        .env("PIXEL_WORKTREE_ID", launch.worktree_id)
        .arg("--print")
        .arg("--output-format")
        .arg("stream-json")
        .arg("--include-hook-events")
        .arg("--allow-dangerously-skip-permissions")
        .arg("--permission-mode")
        .arg("bypassPermissions")
        .arg("--permission-prompts")
        .arg("none")
        .arg("--no-chrome")
        .arg("--name")
        .arg(format!("pixel:{}", launch.task_id));
    if let Some(model) = options.model {
        command.arg("--model").arg(model);
    }
    if let Some(max_turns) = options.max_turns {
        command.arg("--max-turns").arg(max_turns.to_string());
    }
    if let Some(max_budget_usd) = options.max_budget_usd {
        command.arg("--max-budget-usd").arg(max_budget_usd);
    }
    if let Some(prompt_file) = options.system_prompt_file {
        if prompt_file.as_os_str().is_empty() {
            return Err("Claude worker system prompt file must not be empty".to_string());
        }
        command.arg("--append-system-prompt-file").arg(prompt_file);
    }
    if let Some(prompt_file) = options.subagent_prompt_file {
        if prompt_file.as_os_str().is_empty() {
            return Err("Claude worker subagent prompt file must not be empty".to_string());
        }
        command
            .arg("--append-subagent-system-prompt-file")
            .arg(prompt_file);
    }
    command.arg(worker_prompt(launch));
    Ok(command)
}

fn validate_launch(launch: &ClaudeWorkerLaunch<'_>) -> Result<(), String> {
    for (field, value) in [
        ("task id", launch.task_id),
        ("worktree id", launch.worktree_id),
        ("objective", launch.objective),
    ] {
        if value.trim().is_empty() {
            return Err(format!("Claude worker {field} must not be empty"));
        }
    }
    if !launch.worktree.is_absolute() {
        return Err("Claude worker worktree must be absolute".to_string());
    }
    Ok(())
}

fn worker_prompt(launch: &ClaudeWorkerLaunch<'_>) -> String {
    format!(
        "You are an isolated Pixel task worker.\nTask ID: {}\nWorktree ID: {}\n\
Work only inside the current worktree. Do not publish, push, or modify the primary checkout.\n\
Objective:\n{}",
        launch.task_id, launch.worktree_id, launch.objective
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Accepted;

    impl TaskAcceptor for Accepted {
        fn accept(&self, _: &TaskAcceptanceRequest) -> Result<TaskAcceptance, String> {
            Ok(TaskAcceptance {
                task_id: "task-42".to_string(),
                status: "accepted".to_string(),
            })
        }
    }

    struct Rejected;

    impl TaskAcceptor for Rejected {
        fn accept(&self, _: &TaskAcceptanceRequest) -> Result<TaskAcceptance, String> {
            Ok(TaskAcceptance {
                task_id: String::new(),
                status: "rejected_overlap".to_string(),
            })
        }
    }

    fn request() -> TaskAcceptanceRequest {
        TaskAcceptanceRequest {
            provider: "claude",
            session_id: "session-1".to_string(),
            objective: "Change greeting behavior".to_string(),
            repository: PathBuf::from("/repo"),
        }
    }

    #[test]
    fn only_durable_accepted_task_can_handoff() {
        assert_eq!(
            decide_handoff(&Accepted, &request()),
            HandoffDecision::Handoff {
                task_id: "task-42".to_string(),
                status: "accepted".to_string(),
            }
        );
        assert_eq!(
            decide_handoff(&Rejected, &request()),
            HandoffDecision::Foreground {
                reason: "ledger_not_accepted:rejected_overlap".to_string(),
            }
        );
    }

    #[test]
    fn worker_command_uses_normal_oauth_stream_json_mode() {
        let worktree = Path::new("/tmp/pixel-task-42");
        let command = build_worker_command_with_options(
            &ClaudeWorkerLaunch {
                task_id: "task-42",
                worktree_id: "worktree-7",
                worktree,
                objective: "Change greeting behavior",
            },
            &ClaudeWorkerOptions {
                executable: Path::new("claude"),
                model: None,
                max_turns: None,
                max_budget_usd: None,
                system_prompt_file: None,
                subagent_prompt_file: None,
            },
        )
        .unwrap();
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        let env: Vec<_> = command
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect();

        assert_eq!(command.get_program(), "claude");
        assert_eq!(command.get_current_dir(), Some(worktree));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--output-format", "stream-json"])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--permission-mode", "bypassPermissions"])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--permission-prompts", "none"])
        );
        assert!(args.iter().any(|arg| arg == "--print"));
        assert!(args.iter().any(|arg| arg == "--include-hook-events"));
        assert!(
            args.iter()
                .any(|arg| arg == "--allow-dangerously-skip-permissions")
        );
        assert!(args.iter().any(|arg| arg == "--no-chrome"));
        assert!(!args.iter().any(|arg| arg == "--bare" || arg == "--chrome"));
        assert!(args.last().unwrap().contains("Task ID: task-42"));
        assert!(args.last().unwrap().contains("Worktree ID: worktree-7"));
        assert!(env.contains(&("PIXEL_TASK_ID".to_string(), Some("task-42".to_string()))));
        assert!(env.contains(&(
            "PIXEL_WORKTREE_ID".to_string(),
            Some("worktree-7".to_string())
        )));
    }

    #[test]
    fn system_prompt_file_injects_append_flag() {
        let prompt_file = Path::new("/tmp/pixel-worker-rules.md");
        let command = build_worker_command_with_options(
            &ClaudeWorkerLaunch {
                task_id: "task-42",
                worktree_id: "worktree-7",
                worktree: Path::new("/tmp/pixel-task-42"),
                objective: "Change greeting behavior",
            },
            &ClaudeWorkerOptions {
                executable: Path::new("claude"),
                model: None,
                max_turns: None,
                max_budget_usd: None,
                system_prompt_file: Some(prompt_file),
                subagent_prompt_file: None,
            },
        )
        .unwrap();
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--append-system-prompt-file", "/tmp/pixel-worker-rules.md"]),
            "expected --append-system-prompt-file flag, args were: {args:?}"
        );
    }

    #[test]
    fn no_system_prompt_file_omits_append_flag() {
        let command = build_worker_command_with_options(
            &ClaudeWorkerLaunch {
                task_id: "task-42",
                worktree_id: "worktree-7",
                worktree: Path::new("/tmp/pixel-task-42"),
                objective: "Change greeting behavior",
            },
            &ClaudeWorkerOptions {
                executable: Path::new("claude"),
                model: None,
                max_turns: None,
                max_budget_usd: None,
                system_prompt_file: None,
                subagent_prompt_file: None,
            },
        )
        .unwrap();
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(
            !args.iter().any(|arg| arg == "--append-system-prompt-file"),
            "flag must be absent when system_prompt_file is None"
        );
    }

    #[test]
    fn subagent_prompt_file_injects_append_subagent_flag() {
        let prompt_file = Path::new("/tmp/pixel-subagent-rules.md");
        let command = build_worker_command_with_options(
            &ClaudeWorkerLaunch {
                task_id: "task-42",
                worktree_id: "worktree-7",
                worktree: Path::new("/tmp/pixel-task-42"),
                objective: "Change greeting behavior",
            },
            &ClaudeWorkerOptions {
                executable: Path::new("claude"),
                model: None,
                max_turns: None,
                max_budget_usd: None,
                system_prompt_file: None,
                subagent_prompt_file: Some(prompt_file),
            },
        )
        .unwrap();
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(
            args.windows(2).any(|pair| pair
                == [
                    "--append-subagent-system-prompt-file",
                    "/tmp/pixel-subagent-rules.md"
                ]),
            "expected --append-subagent-system-prompt-file flag, args were: {args:?}"
        );
        assert!(
            !args.iter().any(|arg| arg == "--append-system-prompt-file"),
            "the sub-agent file must not leak into the worker's own system prompt: {args:?}"
        );
    }

    #[test]
    fn no_subagent_prompt_file_omits_append_subagent_flag() {
        let command = build_worker_command_with_options(
            &ClaudeWorkerLaunch {
                task_id: "task-42",
                worktree_id: "worktree-7",
                worktree: Path::new("/tmp/pixel-task-42"),
                objective: "Change greeting behavior",
            },
            &ClaudeWorkerOptions {
                executable: Path::new("claude"),
                model: None,
                max_turns: None,
                max_budget_usd: None,
                system_prompt_file: Some(Path::new("/tmp/pixel-worker-rules.md")),
                subagent_prompt_file: None,
            },
        )
        .unwrap();
        let args: Vec<_> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(
            !args
                .iter()
                .any(|arg| arg == "--append-subagent-system-prompt-file"),
            "flag must be absent when subagent_prompt_file is None: {args:?}"
        );
    }

    #[test]
    fn empty_subagent_prompt_file_path_is_rejected() {
        let result = build_worker_command_with_options(
            &ClaudeWorkerLaunch {
                task_id: "task-42",
                worktree_id: "worktree-7",
                worktree: Path::new("/tmp/pixel-task-42"),
                objective: "Change greeting behavior",
            },
            &ClaudeWorkerOptions {
                executable: Path::new("claude"),
                model: None,
                max_turns: None,
                max_budget_usd: None,
                system_prompt_file: None,
                subagent_prompt_file: Some(Path::new("")),
            },
        );
        assert!(
            result.is_err(),
            "empty subagent_prompt_file must be rejected"
        );
    }

    #[test]
    fn empty_system_prompt_file_path_is_rejected() {
        let result = build_worker_command_with_options(
            &ClaudeWorkerLaunch {
                task_id: "task-42",
                worktree_id: "worktree-7",
                worktree: Path::new("/tmp/pixel-task-42"),
                objective: "Change greeting behavior",
            },
            &ClaudeWorkerOptions {
                executable: Path::new("claude"),
                model: None,
                max_turns: None,
                max_budget_usd: None,
                system_prompt_file: Some(Path::new("")),
                subagent_prompt_file: None,
            },
        );
        assert!(result.is_err(), "empty system_prompt_file must be rejected");
    }

    #[test]
    fn command_rejects_missing_identity_or_relative_worktree() {
        let opts = ClaudeWorkerOptions {
            executable: Path::new("claude"),
            model: None,
            max_turns: None,
            max_budget_usd: None,
            system_prompt_file: None,
            subagent_prompt_file: None,
        };
        let relative = ClaudeWorkerLaunch {
            task_id: "task-42",
            worktree_id: "worktree-7",
            worktree: Path::new("relative"),
            objective: "objective",
        };
        assert!(build_worker_command_with_options(&relative, &opts).is_err());
        let missing = ClaudeWorkerLaunch {
            task_id: "",
            ..ClaudeWorkerLaunch {
                task_id: "task-42",
                worktree_id: "worktree-7",
                worktree: Path::new("/tmp/worktree"),
                objective: "objective",
            }
        };
        assert!(build_worker_command_with_options(&missing, &opts).is_err());
    }

    #[test]
    fn ledger_adapter_returns_handoff_only_after_durable_acceptance() {
        let root = std::env::temp_dir().join(format!(
            "pixel-controller-ledger-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let mut request = request();
        request.repository = root.clone();

        let HandoffDecision::Handoff { task_id, status } =
            decide_handoff(&LedgerAcceptor, &request)
        else {
            panic!("durable ledger acceptance must hand off");
        };
        assert_eq!(status, "accepted");
        assert_eq!(
            crate::task_runtime::status(&root, &task_id)
                .expect("accepted record must be visible")
                .status,
            "accepted"
        );
        assert!(
            crate::task_runtime::events(&root, &task_id)
                .iter()
                .any(|event| event["event"] == "accepted")
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}

//! `pixel doctor` — checks install state, binary path, daemon health, and
//! index/graph/facts freshness, reporting green/yellow/red per check.
//!
//! Every check is listed in [`CHECKS`] with the command that repairs it, so a
//! caller can select checks by id (`--only`, `--skip`) and act on a finding
//! without parsing its prose. [`repair_plan`] folds the flagged checks into
//! one run of each distinct command, which `pixel doctor --fix` executes.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::InstallError;
use crate::config;
use crate::install;

pub type Result<T> = std::result::Result<T, InstallError>;

/// The five mandatory scenarios the rule text and the SessionStart usage
/// string must agree on. One name per scenario (the guard-verb that anchors
/// it): targets (sniper scoping — mandatory first call, advisory fence),
/// resolve (phrase → code), rescue (history recovery, includes excavate),
/// reconcile (branch sync), impact (blast radius, includes changes).
pub const MANDATORY_SCENARIOS: &[&str] = &[
    "scope-task",
    "find-code",
    "plan-rollback",
    "sync-branch",
    "impact",
];

/// Per-check status for the doctor report, ordered from healthy to broken.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Green,
    Yellow,
    Red,
}

impl fmt::Display for CheckStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Green => "green",
            Self::Yellow => "yellow",
            Self::Red => "red",
        })
    }
}

/// One check the doctor knows: its stable id and the command that repairs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CheckSpec {
    pub id: &'static str,
    /// Shell command that repairs a yellow or red outcome, `{root}` standing
    /// for the quoted repository root; `None` when no command can.
    pub fix: Option<&'static str>,
}

/// A catalogue entry; keeps [`CHECKS`] one line per check.
const fn entry(id: &'static str, fix: Option<&'static str>) -> CheckSpec {
    CheckSpec { id, fix }
}

/// Command that rewrites everything `pixel install` deploys in the home.
const FIX_INSTALL: Option<&str> = Some("pixel install");
/// Command that rewrites the repo-local enforcement files.
const FIX_REPO_INSTALL: Option<&str> = Some("pixel install --repo {root}");
/// Command that builds the text index and the graph, and warms the daemon.
const FIX_PREPARE: Option<&str> = Some("pixel prepare-repo {root}");

/// Every check, in the order a run reports them. Ids are stable: scripts
/// select them with `--only`/`--skip`, and a check missing from this list
/// panics when it runs.
pub const CHECKS: &[CheckSpec] = &[
    entry("binary.path", None),
    entry("binary.executable", None),
    entry("install.agent-prompt", FIX_INSTALL),
    entry("install.subagent-prompt", FIX_INSTALL),
    entry("install.pi-prompt", FIX_INSTALL),
    entry("install.codex-config", FIX_INSTALL),
    entry("install.codex-metrics-hook", FIX_INSTALL),
    entry("install.opencode-agents-md", FIX_INSTALL),
    entry("install.antigravity", FIX_INSTALL),
    entry("install.claude-hooks", FIX_INSTALL),
    // The removal command names the orphaned file, so the outcome carries it.
    entry("install.rtk-backup", None),
    entry("install.legacy-wrappers", FIX_INSTALL),
    entry("rule.parity", FIX_INSTALL),
    entry("rule.scenarios", FIX_INSTALL),
    entry("repo.codex-config", FIX_REPO_INSTALL),
    entry("repo.codex-hooks", FIX_REPO_INSTALL),
    entry("repo.devin-hooks", FIX_REPO_INSTALL),
    entry("repo.claude-hooks", FIX_REPO_INSTALL),
    entry("repo.pi-guard", FIX_REPO_INSTALL),
    entry("daemon.health", Some("pixel daemon start {root}")),
    entry(
        "daemon.epistemics",
        Some("pixel daemon stop {root} && pixel daemon start {root}"),
    ),
    entry("index.freshness", FIX_PREPARE),
    entry("graph.freshness", FIX_PREPARE),
    entry(
        "facts.freshness",
        Some("pixel build-index --history {root}"),
    ),
];

/// One doctor check.
#[derive(Debug, Clone, Serialize)]
pub struct DoctorCheck {
    pub id: String,
    pub status: CheckStatus,
    pub required: bool,
    pub duration_ms: u64,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
    /// Shell command that repairs this check; only on a yellow or red one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
    /// The argv after `pixel` of each command in `fix`, when `fix` is the
    /// catalogue command `--fix` may run by itself; `None` for a command only
    /// one outcome names (a path to remove), which stays the user's call.
    #[serde(skip)]
    pub repair: Option<Vec<Vec<String>>>,
}

/// The full doctor report.
#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    pub version: String,
    pub ok: bool,
    pub executable_path: String,
    pub home: String,
    pub checks: Vec<DoctorCheck>,
    pub summary: DoctorSummary,
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorSummary {
    pub green: usize,
    pub yellow: usize,
    pub red: usize,
    /// Checks left out by `only`/`skip`.
    pub skipped: usize,
}

impl DoctorReport {
    /// Whether any check sits at or above `threshold`: the CLI exits 1 then.
    #[must_use]
    pub fn fails(&self, threshold: CheckStatus) -> bool {
        self.checks.iter().any(|c| c.status >= threshold)
    }
}

/// The terminal form: one tally line, then every yellow or red check, red
/// first, each with its `fix:` line when a command repairs it.
impl fmt::Display for DoctorReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = &self.summary;
        write!(f, "pixel doctor: ran {} check(s)", self.checks.len())?;
        if s.skipped > 0 {
            write!(f, ", skipped {}", s.skipped)?;
        }
        writeln!(
            f,
            " — {} green, {} yellow, {} red",
            s.green, s.yellow, s.red
        )?;
        let mut flagged: Vec<&DoctorCheck> = self
            .checks
            .iter()
            .filter(|c| c.status != CheckStatus::Green)
            .collect();
        // Stable: checks of one status keep their run order.
        flagged.sort_by_key(|c| std::cmp::Reverse(c.status));
        for c in flagged {
            let message = c.reason.as_deref().unwrap_or(&c.summary);
            writeln!(f, "  [{}] {}: {}", c.status, c.id, one_line(message))?;
            if let Some(fix) = &c.fix {
                writeln!(f, "    fix: {fix}")?;
            }
        }
        Ok(())
    }
}

/// The terminal form of `pixel doctor --list`: one line per check, its id
/// padded to a column, then its repair command or `-` when none exists.
#[must_use]
pub fn render_catalogue(checks: &[CheckSpec]) -> String {
    let width = checks.iter().map(|c| c.id.len()).max().unwrap_or(0);
    checks
        .iter()
        .map(|c| format!("{:<width$}  {}\n", c.id, c.fix.unwrap_or("-")))
        .collect()
}

/// Options controlling a doctor run.
#[derive(Debug, Clone, Default)]
pub struct DoctorOptions {
    /// Path to the pixel binary to check. Defaults to the current exe.
    pub executable_path: Option<PathBuf>,
    /// Home directory. Defaults to `$HOME`.
    pub home: Option<PathBuf>,
    /// Repo root to check index/graph/facts freshness for. If None, only
    /// install-state checks run.
    pub repo_root: Option<PathBuf>,
    /// Shell whose wrapper block should be checked, as a `$SHELL`-style value.
    /// Defaults to `$SHELL`. Must match what `pixel install` was given, or the
    /// check looks at the wrong profile.
    pub shell: Option<String>,
    /// The `claude` executable whose version decides which wrapper block is
    /// expected (see `InstallOptions::claude_executable`). Defaults to the
    /// first `claude` on PATH.
    pub claude_executable: Option<PathBuf>,
    /// Dry-run parser for one `pixel …` argv (including the leading
    /// "pixel"), supplied by the CLI binary from its real clap definition.
    /// When present, the `rule.parity` check parses every pixel command
    /// line found in the installed rule text against it — documented
    /// syntax the binary rejects goes red. When None (library callers
    /// without access to the CLI parser), the parity check is skipped.
    #[allow(clippy::type_complexity)]
    pub syntax_validator: Option<fn(&[String]) -> std::result::Result<(), String>>,
    /// Check ids to run; empty runs every check. Ids come from [`CHECKS`].
    pub only: Vec<String>,
    /// Check ids to leave out.
    pub skip: Vec<String>,
}

/// Run `pixel doctor`.
///
/// # Errors
///
/// Fails before any check runs when `only` or `skip` names an id missing
/// from [`CHECKS`] or both name the same id, when no home directory
/// resolves, or when the current executable cannot be located.
pub fn doctor(options: &DoctorOptions) -> Result<DoctorReport> {
    validate_selection(&options.only, &options.skip)?;
    let home = options
        .home
        .clone()
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .ok_or(InstallError::NoHome)?;
    let executable_path = match &options.executable_path {
        Some(p) => p.clone(),
        None => std::env::current_exe().map_err(InstallError::CurrentExe)?,
    };
    let exe = executable_path
        .canonicalize()
        .unwrap_or_else(|_| executable_path.clone());

    let mut runner = Runner {
        only: &options.only,
        skip: &options.skip,
        root: options.repo_root.as_deref(),
        shell: options.shell.as_deref(),
        checks: Vec::new(),
        skipped: 0,
    };

    runner.check(
        "binary.path",
        || -> std::result::Result<DoctorCheckDetail, String> {
            if !exe.is_file() {
                return Err(format!("binary not found at {}", exe.display()));
            }
            Ok(DoctorCheckDetail {
                summary: format!("binary present at {}", exe.display()),
                detail: Some(serde_json::json!({ "path": exe.display().to_string() })),
            })
        },
    );

    runner.check(
        "binary.executable",
        || -> std::result::Result<DoctorCheckDetail, String> {
            let out = Command::new(&exe).arg("--version").output();
            match out {
                Ok(o) if o.status.success() => Ok(DoctorCheckDetail {
                    summary: format!(
                        "binary runs ({} bytes stdout)",
                        String::from_utf8_lossy(&o.stdout).trim().len()
                    ),
                    detail: None,
                }),
                Ok(o) => Err(format!(
                    "binary exited {}: {}",
                    o.status,
                    capped(
                        &one_line(&String::from_utf8_lossy(&o.stderr)),
                        STDERR_EXCERPT_CHARS
                    )
                )),
                Err(e) => Err(format!("failed to run binary: {e}")),
            }
        },
    );

    runner.check(
        "install.agent-prompt",
        || -> std::result::Result<DoctorCheckDetail, String> {
            let path = home.join(".local/share/pixel/agent-prompt.md");
            if !path.is_file() {
                return Err("agent-prompt.md not deployed — run `pixel install`".into());
            }
            let content = fs::read_to_string(&path).map_err(|e| e.to_string())?;
            // Byte equality with the bundled asset, like the sub-agent
            // prompt: a deployed copy that still carries the headline
            // sections but has diverged on the command map, the call shapes
            // or the mutation warning teaches agents syntax this binary may
            // no longer accept, and `rule.parity` does not cover it.
            if content != install::AGENT_PROMPT_ASSET {
                return Err("agent-prompt.md is stale — run `pixel install` to update".into());
            }
            Ok(DoctorCheckDetail {
                summary: format!("agent-prompt.md deployed ({} bytes)", content.len()),
                detail: Some(serde_json::json!({ "path": path.display().to_string() })),
            })
        },
    );

    runner.check(
        "install.subagent-prompt",
        || -> std::result::Result<DoctorCheckDetail, String> {
            let path = home
                .join(".local/share/pixel")
                .join(install::SUBAGENT_PROMPT_FILE);
            if !path.is_file() {
                return Err(format!(
                    "{} not deployed — run `pixel install`",
                    install::SUBAGENT_PROMPT_FILE
                ));
            }
            let content = fs::read_to_string(&path).map_err(|e| e.to_string())?;
            // Byte equality with the bundled asset: the wrapper hands this
            // file to every print-mode sub-agent, so a stale copy silently
            // teaches them syntax this binary may no longer accept.
            if content != install::SUBAGENT_PROMPT_ASSET {
                return Err(format!(
                    "{} is stale — run `pixel install` to update",
                    install::SUBAGENT_PROMPT_FILE
                ));
            }
            Ok(DoctorCheckDetail {
                summary: format!(
                    "{} deployed ({} bytes)",
                    install::SUBAGENT_PROMPT_FILE,
                    content.len()
                ),
                detail: Some(serde_json::json!({ "path": path.display().to_string() })),
            })
        },
    );

    runner.check(
        "install.pi-prompt",
        || -> std::result::Result<DoctorCheckDetail, String> {
            let path = home.join(install::PI_PROMPT_REL);
            if !path.is_file() {
                return Err(format!(
                    "{} not deployed — run `pixel install`",
                    path.display()
                ));
            }
            let content = fs::read_to_string(&path).map_err(|e| e.to_string())?;
            // The managed block, not the whole file: instructions the user
            // keeps outside the markers are theirs, but a missing, stale or
            // unterminated block is exactly what `pixel install` rewrites.
            if content != config::apply_managed_markers(&content, install::AGENT_PROMPT_ASSET) {
                return Err(format!(
                    "{} is stale — run `pixel install` to update",
                    path.display()
                ));
            }
            Ok(DoctorCheckDetail {
                summary: format!(
                    "APPEND_SYSTEM.md carries the agent prompt ({} bytes)",
                    content.len()
                ),
                detail: Some(serde_json::json!({ "path": path.display().to_string() })),
            })
        },
    );

    let codex_home = crate::codex_config::codex_home(&home, options.home.is_some());
    runner.check(
        "install.codex-config",
        || -> std::result::Result<DoctorCheckDetail, String> {
            let (summary, detail) = crate::codex_config::check_developer_instructions(&codex_home)?;
            Ok(DoctorCheckDetail {
                summary,
                detail: Some(detail),
            })
        },
    );
    runner.check(
        "install.codex-metrics-hook",
        || -> std::result::Result<DoctorCheckDetail, String> {
            let (summary, detail) = crate::codex_config::check_metrics_hook(&codex_home)?;
            Ok(DoctorCheckDetail {
                summary,
                detail: Some(detail),
            })
        },
    );

    runner.check(
        "install.opencode-agents-md",
        || -> std::result::Result<DoctorCheckDetail, String> {
            let (summary, detail) = crate::opencode_config::check_opencode(
                &crate::opencode_config::opencode_config_dir(&home, options.home.is_some()),
            )?;
            Ok(DoctorCheckDetail {
                summary,
                detail: Some(detail),
            })
        },
    );

    let exe_for_antigravity = exe.clone();
    let home_for_antigravity = home.clone();
    runner.check(
        "install.antigravity",
        move || -> std::result::Result<DoctorCheckDetail, String> {
            let (summary, detail) = crate::antigravity::check_antigravity_install(
                &home_for_antigravity,
                &exe_for_antigravity,
            )?;
            Ok(DoctorCheckDetail {
                summary,
                detail: Some(detail),
            })
        },
    );

    // The doctrine now reaches every Claude process through the lifecycle
    // hooks in ~/.claude/settings.json — SessionStart injects the deployed
    // agent prompt itself. Verify the whole lifecycle contract: matchers,
    // commands and the executable this binary's install would write, not
    // just a `pixel` substring.
    runner.check(
        "install.claude-hooks",
        || -> std::result::Result<DoctorCheckDetail, String> {
            let path = home.join(".claude/settings.json");
            if !path.is_file() {
                return Err(format!(
                    "{} not found — run `pixel install`",
                    path.display()
                ));
            }
            let value = install::read_settings(&path).map_err(|e| e.to_string())?;
            let hooks = value
                .get("hooks")
                .and_then(serde_json::Value::as_object)
                .ok_or_else(|| format!("no hooks object in {}", path.display()))?;
            let has = |event: &str, verb: &str| {
                hooks
                    .get(event)
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(|groups| {
                        groups.iter().any(|group| {
                            group
                                .get("hooks")
                                .and_then(serde_json::Value::as_array)
                                .is_some_and(|inner| {
                                    inner.iter().any(|hook| {
                                        hook.get("command")
                                            .and_then(serde_json::Value::as_str)
                                            .is_some_and(|c| {
                                                c.contains(&format!("run-hook {verb}"))
                                                    && c.contains("pixel")
                                            })
                                    })
                                })
                        })
                    })
            };
            // Claude runs post-compaction through SessionStart with matcher
            // "compact" — verify the verb AND its matcher, not just presence.
            let has_compact = hooks
                .get("SessionStart")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|groups| {
                    groups.iter().any(|group| {
                        group.get("matcher").and_then(serde_json::Value::as_str) == Some("compact")
                            && group
                                .get("hooks")
                                .and_then(serde_json::Value::as_array)
                                .is_some_and(|inner| {
                                    inner.iter().any(|hook| {
                                        hook.get("command")
                                            .and_then(serde_json::Value::as_str)
                                            .is_some_and(|c| {
                                                c.contains("run-hook post-compaction")
                                                    && c.contains("pixel")
                                            })
                                    })
                                })
                    })
                });
            let mut missing = Vec::new();
            if !has("SessionStart", "session-start") {
                missing.push("SessionStart→session-start");
            }
            if !has("UserPromptSubmit", "prompt-submit") {
                missing.push("UserPromptSubmit→prompt-submit");
            }
            if !has_compact {
                missing.push("SessionStart(compact)→post-compaction");
            }
            if !missing.is_empty() {
                return Err(format!(
                    "missing pixel lifecycle hooks in {}: {} — run `pixel install`",
                    path.display(),
                    missing.join(", ")
                ));
            }
            let stacked = crate::routing::stacked_pixel_hooks(&value, &exe);
            if !stacked.is_empty() {
                return Err(format!(
                    "pixel hooks registered more than once in {}: {} — run `pixel install`",
                    path.display(),
                    stacked.join(", ")
                ));
            }
            Ok(DoctorCheckDetail {
                summary: format!("claude lifecycle hooks configured in {}", path.display()),
                detail: Some(serde_json::json!({ "path": path.display().to_string() })),
            })
        },
    );

    // A global RTK backup that no guard delegates to is never applied again;
    // say so rather than leave a file that looks like a live registration.
    runner.record("install.rtk-backup", || {
        Ok(rtk_backup_check(crate::routing::orphan_rtk_backup(
            &home, &exe,
        )))
    });

    // Legacy `claude()` shell wrappers are harmful now: a surviving block
    // double-injects the prompt on every wrapped launch. Any pixel-managed
    // block in ANY candidate profile (the resolved shell's or a stray left
    // by an install that ran under the wrong $SHELL) is red.
    let shell_override = options.shell.clone();
    runner.check(
        "install.legacy-wrappers",
        || -> std::result::Result<DoctorCheckDetail, String> {
            let shell = install::resolve_shell(shell_override.as_deref());
            let (_, resolved) = install::shell_profile_for(&shell, &home);
            let mut profiles = vec![resolved.clone()];
            profiles.extend(
                install::stray_wrapper_profiles(&home, &resolved)
                    .into_iter()
                    .map(|(_, path)| path),
            );
            let mut blocks = Vec::new();
            for profile in &profiles {
                if fs::read_to_string(profile)
                    .ok()
                    .and_then(|content| install::extract_managed_block(&content))
                    .is_some()
                {
                    blocks.push(profile.display().to_string());
                }
            }
            if !blocks.is_empty() {
                return Err(format!(
                    "stale pixel shell wrapper in {} — run `pixel install` to remove it (the SessionStart hook now injects the prompt; the wrapper double-injects)",
                    blocks.join(", ")
                ));
            }
            Ok(DoctorCheckDetail {
                summary: "no legacy shell wrappers — SessionStart hook injects the prompt".into(),
                detail: Some(serde_json::json!({ "profiles_checked": profiles
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>() })),
            })
        },
    );

    // Rule-vs-binary parity: every `pixel …` command line documented in the
    // INSTALLED rule text must dry-run parse against the binary's real clap
    // definition. Drift between documented CLI syntax and the binary was the
    // largest defect category found — this makes it a red doctor check
    // instead of a silent lie agents follow into parse errors.
    if let Some(validator) = options.syntax_validator {
        let home_for_rule = home.clone();
        // The prompt `pixel install` deploys always carries command lines, so
        // a rule text without any is not something a reinstall repairs.
        runner.record("rule.parity", move || {
            let Some((source, rule_text)) = installed_rule_text(&home_for_rule) else {
                return Ok((CheckStatus::Yellow, DoctorCheckDetail {
                    summary: "no installed rule text found (agent-prompt.md not deployed, no managed block or rule file) — run `pixel install`; parity not checked".into(),
                    detail: None,
                }, Remedy::Catalogue));
            };
            let commands = extract_rule_commands(&rule_text);
            if commands.is_empty() {
                return Ok((CheckStatus::Yellow, DoctorCheckDetail {
                    summary: format!(
                        "installed rule text at {} contains no `pixel …` command lines — parity not checked",
                        source.display()
                    ),
                    detail: None,
                }, Remedy::Manual));
            }
            let mut parsed_ok = 0usize;
            let mut unparsed: Vec<String> = Vec::new();
            let mut failures: Vec<String> = Vec::new();
            for line in &commands {
                match normalize_rule_command(line) {
                    None => unparsed.push(line.clone()),
                    Some(argv) => match validator(&argv) {
                        Ok(()) => parsed_ok += 1,
                        Err(e) => failures.push(format!("`{line}` → {e}")),
                    },
                }
            }
            let detail = Some(serde_json::json!({
                "source": source.display().to_string(),
                "command_lines": commands.len(),
                "parsed_ok": parsed_ok,
                "unparsed": unparsed,
                "failures": failures,
            }));
            if !failures.is_empty() {
                return Err(format!(
                    "{} documented command line(s) rejected by the CLI parser: {}",
                    failures.len(),
                    failures.join("; ")
                ));
            }
            Ok((CheckStatus::Green, DoctorCheckDetail {
                summary: format!(
                    "{parsed_ok}/{} documented pixel command lines parse against the CLI ({} unparsed placeholder line(s) skipped)",
                    commands.len(),
                    unparsed.len()
                ),
                detail,
            }, Remedy::Catalogue))
        });
    }

    // Scenario-count consistency: the installed rule text and the
    // SessionStart usage string must agree on the FIVE mandatory scenarios
    // (targets/resolve/rescue/reconcile/impact). A scenario the rule
    // mandates but the injected session never hears about — or vice versa —
    // is exactly the drift class this doctor exists to catch.
    {
        let home_for_rule = home.clone();
        runner.check_status("rule.scenarios", move || {
            let Some((source, rule_text)) = installed_rule_text(&home_for_rule) else {
                return Ok((
                    CheckStatus::Yellow,
                    DoctorCheckDetail {
                        summary: "no installed rule text found (agent-prompt.md not deployed, no managed block or rule file) — run `pixel install`; scenario consistency not checked"
                            .into(),
                        detail: None,
                    },
                ));
            };
            let mismatches = scenario_mismatches(&rule_text, pixel_proto::op::SESSION_USAGE);
            if !mismatches.is_empty() {
                return Err(format!(
                    "scenario drift between installed rule text ({}) and session usage string: {}",
                    source.display(),
                    mismatches.join("; ")
                ));
            }
            Ok((
                CheckStatus::Green,
                DoctorCheckDetail {
                    summary: format!(
                        "rule text and session usage agree on all {} mandatory scenarios",
                        MANDATORY_SCENARIOS.len()
                    ),
                    detail: Some(serde_json::json!({
                        "scenarios": MANDATORY_SCENARIOS,
                        "source": source.display().to_string(),
                    })),
                },
            ))
        });
    }

    if let Some(root) = &options.repo_root {
        // Repo-local enforcement (`pixel install --repo <path>`). A check is
        // red only when the file carries evidence of a Pixel install (a
        // Pixel entry, marker or sidecar) and that install is broken. An
        // absent file, or one the project keeps for itself with nothing of
        // Pixel's in it, is informational green: a repo where repo-install
        // never ran is a valid state, not a broken one.
        runner.check_status("repo.codex-config", || {
            let codex_dir = root.join(".codex");
            if !crate::codex_config::carries_pixel_block(&codex_dir)? {
                return Ok((
                    CheckStatus::Green,
                    DoctorCheckDetail {
                        summary: "no pixel block in .codex/config.toml — repo-local codex instructions not installed"
                            .into(),
                        detail: None,
                    },
                ));
            }
            let (summary, detail) = crate::codex_config::check_developer_instructions(&codex_dir)?;
            Ok((
                CheckStatus::Green,
                DoctorCheckDetail {
                    summary,
                    detail: Some(detail),
                },
            ))
        });

        runner.check_status("repo.codex-hooks", || {
            let hooks_path = root.join(".codex").join(crate::codex_config::HOOKS_FILE);
            let sidecar = root.join(".codex").join(crate::routing::CODEX_COMPOSED_BACKUP);
            match (hooks_path.is_file(), sidecar.is_file()) {
                (false, false) => Ok((
                    CheckStatus::Green,
                    DoctorCheckDetail {
                        summary: "no .codex/hooks.json — repo-local composed guard not installed".into(),
                        detail: None,
                    },
                )),
                (true, false) => {
                    let value = install::read_settings(&hooks_path).map_err(|e| e.to_string())?;
                    if !crate::routing::has_pixel_hook(&value, &exe) {
                        return Ok((
                            CheckStatus::Green,
                            DoctorCheckDetail {
                                summary: "no pixel hook in .codex/hooks.json — repo-local composed guard not installed".into(),
                                detail: None,
                            },
                        ));
                    }
                    Err(format!(
                        "{} carries pixel hooks without their composed-guard backup {} — run `pixel install --repo`",
                        hooks_path.display(),
                        sidecar.display()
                    ))
                }
                (false, true) => Err(format!(
                    "composed-guard backup {} exists but {} is missing — run `pixel install --repo`",
                    sidecar.display(),
                    hooks_path.display()
                )),
                (true, true) => {
                    let value = install::read_settings(&hooks_path).map_err(|e| e.to_string())?;
                    let groups = value
                        .get("hooks")
                        .and_then(|h| h.get("PreToolUse"))
                        .and_then(serde_json::Value::as_array);
                    let managed = groups.is_some_and(|groups| {
                        groups.len() == 1
                            && groups[0]
                                .get("hooks")
                                .and_then(serde_json::Value::as_array)
                                .is_some_and(|hooks| {
                                    hooks.iter().any(|hook| {
                                        hook.get("command")
                                            .and_then(serde_json::Value::as_str)
                                            .is_some_and(|c| {
                                                c.contains(
                                                    "run-hook composed-guard --provider codex --backup ",
                                                )
                                            })
                                    })
                                })
                    });
                    if !managed {
                        return Err(format!(
                            "PreToolUse in {} is not the managed composed-guard group — run `pixel install --repo`",
                            hooks_path.display()
                        ));
                    }
                    Ok((
                        CheckStatus::Green,
                        DoctorCheckDetail {
                            summary: format!(
                                "composed codex guard configured in {} (backup={})",
                                hooks_path.display(),
                                sidecar.display()
                            ),
                            detail: Some(serde_json::json!({
                                "hooks": hooks_path.display().to_string(),
                                "backup": sidecar.display().to_string(),
                            })),
                        },
                    ))
                }
            }
        });

        runner.check_status("repo.devin-hooks", || {
            let path = root.join(crate::routing::DEVIN_LOCAL_CONFIG);
            let value = if path.is_file() {
                install::read_settings(&path).map_err(|e| e.to_string())?
            } else {
                serde_json::Value::Null
            };
            if !crate::routing::has_pixel_hook(&value, &exe) {
                return Ok((
                    CheckStatus::Green,
                    DoctorCheckDetail {
                        summary: format!(
                            "no pixel hook in {} — repo-local devin guard not installed",
                            crate::routing::DEVIN_LOCAL_CONFIG
                        ),
                        detail: None,
                    },
                ));
            }
            if !crate::routing::has_pixel_guard(&value, "run-hook guard --provider devin", &exe) {
                return Err(format!(
                    "pixel hooks in {} but no pixel guard PreToolUse entry — run `pixel install --repo`",
                    path.display()
                ));
            }
            Ok((
                CheckStatus::Green,
                DoctorCheckDetail {
                    summary: format!("devin guard registered in {}", path.display()),
                    detail: Some(serde_json::json!({ "path": path.display().to_string() })),
                },
            ))
        });

        runner.check_status("repo.claude-hooks", || {
            // The shared settings.json is committed: a pixel guard there
            // runs this machine's binary path on every teammate's clone.
            let shared = root.join(crate::routing::CLAUDE_SHARED_SETTINGS);
            if shared.is_file() {
                let value = install::read_settings(&shared).map_err(|e| e.to_string())?;
                if crate::routing::has_pixel_guard(&value, "run-hook guard", &exe) {
                    return Err(format!(
                        "pixel guard in the shared {} names this machine's binary — run `pixel install --repo` to move it to {}",
                        shared.display(),
                        crate::routing::CLAUDE_LOCAL_SETTINGS
                    ));
                }
            }
            let path = root.join(crate::routing::CLAUDE_LOCAL_SETTINGS);
            let value = if path.is_file() {
                install::read_settings(&path).map_err(|e| e.to_string())?
            } else {
                serde_json::Value::Null
            };
            let rtk_backup = root.join(crate::routing::RTK_BACKUP);
            if !crate::routing::has_pixel_hook(&value, &exe) && !rtk_backup.is_file() {
                return Ok((
                    CheckStatus::Green,
                    DoctorCheckDetail {
                        summary: format!(
                            "no pixel hook in {} — repo-local claude guard not installed",
                            crate::routing::CLAUDE_LOCAL_SETTINGS
                        ),
                        detail: None,
                    },
                ));
            }
            if !crate::routing::has_pixel_guard(&value, "run-hook guard --provider claude", &exe) {
                return Err(format!(
                    "pixel install evidence (hook or {}) but no pixel guard PreToolUse entry in {} — run `pixel install --repo`",
                    rtk_backup.display(),
                    path.display()
                ));
            }
            // Claude Code merges the shared and global settings into the
            // same session: a shell rewriter there races the guard.
            let global = crate::routing::Provider::Claude.path(&home);
            let mut others = vec![&shared];
            if !crate::routing::same_file(&shared, &global) {
                others.push(&global);
            }
            let mut rivals = Vec::new();
            for other in others {
                let (groups, _) = crate::routing::global_pre_tool_use(other);
                for command in
                    crate::routing::hook_commands(&crate::routing::blocking_claude_groups(&groups))
                {
                    rivals.push(format!("{command} in {}", other.display()));
                }
            }
            if !rivals.is_empty() {
                return Ok((
                    CheckStatus::Yellow,
                    DoctorCheckDetail {
                        summary: format!(
                            "claude guard in {} runs beside another shell rewriter ({}) — run `pixel install --repo {}` to hold the guard back",
                            path.display(),
                            rivals.join(", "),
                            crate::routing::quoted_executable(root)
                        ),
                        detail: Some(serde_json::json!({ "path": path.display().to_string() })),
                    },
                ));
            }
            Ok((
                CheckStatus::Green,
                DoctorCheckDetail {
                    summary: format!("claude guard registered in {}", path.display()),
                    detail: Some(serde_json::json!({ "path": path.display().to_string() })),
                },
            ))
        });

        runner.check_status("repo.pi-guard", || {
            pi_guard_check(root, crate::pi_project::guard_state(root))
        });

        runner.check(
            "daemon.health",
            || -> std::result::Result<DoctorCheckDetail, String> {
                let sock = pixel_daemon::daemon::socket_path(root);
                if !sock.exists() {
                    return Err(format!("no daemon socket at {}", sock.display()));
                }
                Ok(DoctorCheckDetail {
                    summary: format!("daemon socket present at {}", sock.display()),
                    detail: Some(serde_json::json!({ "socket": sock.display().to_string() })),
                })
            },
        );

        // Epistemics-presence probe: when a daemon answers, one retrieval op
        // should carry an `epistemics` object in its response. Warning-only
        // (Yellow), never red — the envelope is landing concurrently and a
        // daemon built from an older binary is a staleness note, not a
        // broken install.
        // Only an answering daemon without the envelope needs the restart;
        // no daemon at all is `daemon.health`'s finding and carries its fix.
        runner.record("daemon.epistemics", || {
            let sock = pixel_daemon::daemon::socket_path(root);
            if !sock.exists() {
                return Ok((CheckStatus::Yellow, DoctorCheckDetail {
                    summary: "no daemon running — epistemics probe skipped".into(),
                    detail: None,
                }, Remedy::Manual));
            }
            match probe_daemon_epistemics(&sock) {
                Ok(true) => Ok((CheckStatus::Green, DoctorCheckDetail {
                    summary: "daemon retrieval response carries an epistemics object".into(),
                    detail: None,
                }, Remedy::Catalogue)),
                Ok(false) => Ok((CheckStatus::Yellow, DoctorCheckDetail {
                    summary: "daemon retrieval response has NO epistemics object — daemon may predate the epistemics envelope; restart it".into(),
                    detail: None,
                }, Remedy::Catalogue)),
                Err(e) => Ok((CheckStatus::Yellow, DoctorCheckDetail {
                    summary: format!("epistemics probe inconclusive: {e}"),
                    detail: None,
                }, Remedy::Manual)),
            }
        });

        runner.check(
            "index.freshness",
            || -> std::result::Result<DoctorCheckDetail, String> {
                let shard = root
                    .join(pixel_index::index::SHARD_DIR)
                    .join(pixel_index::index::SHARD_FILE);
                if !shard.is_file() {
                    return Err("index not built".into());
                }
                let mtime = fs::metadata(&shard)
                    .map_err(|e| e.to_string())?
                    .modified()
                    .map_err(|e| e.to_string())?;
                let age = age_secs(mtime);
                Ok(DoctorCheckDetail {
                    summary: format!("index present ({age}s old)"),
                    detail: Some(serde_json::json!({ "age_secs": age })),
                })
            },
        );

        runner.check(
            "graph.freshness",
            || -> std::result::Result<DoctorCheckDetail, String> {
                let db = root
                    .join(pixel_index::index::SHARD_DIR)
                    .join(pixel_daemon::api::GRAPH_DB_FILE);
                if !db.is_file() {
                    return Err("graph not built".into());
                }
                let mtime = fs::metadata(&db)
                    .map_err(|e| e.to_string())?
                    .modified()
                    .map_err(|e| e.to_string())?;
                let age = age_secs(mtime);
                Ok(DoctorCheckDetail {
                    summary: format!("graph present ({age}s old)"),
                    detail: Some(serde_json::json!({ "age_secs": age })),
                })
            },
        );

        runner.check_status("facts.freshness", || -> std::result::Result<(CheckStatus, DoctorCheckDetail), String> {
            let store = pixel_facts::FactsStore::open(root).map_err(|e| e.to_string())?;
            let state = store.index_state();
            // Red: schema version mismatch — the db was written by a different
            // build and must be rebuilt before it can be trusted.
            if state.schema_version != pixel_facts::store::FACTS_SCHEMA_VERSION {
                return Err(format!(
                    "facts schema version mismatch: on-disk {} != expected {} (rebuild required)",
                    state.schema_version,
                    pixel_facts::store::FACTS_SCHEMA_VERSION
                ));
            }
            // Counter-based dead/poisoned detection: mtime and diff_state
            // alone lie (the historical poisoned DB had every commit marked
            // INDEXED with empty hunk text), so measure the actual text and
            // gram rows.
            let count = |sql: &str| -> i64 {
                store.conn().query_row(sql, [], |r| r.get(0)).unwrap_or(0)
            };
            let hunks_with_text = count(
                "SELECT count(*) FROM hunks WHERE length(added) > 0 OR length(removed) > 0",
            );
            let diff_grams = count("SELECT count(*) FROM diff_grams");
            let repo_commits = pixel_git::GitRunner::new(root)
                .rev_list_count_all()
                .unwrap_or(0);
            if let Some(reason) =
                facts_dead_reason(state.commits_indexed, repo_commits, diff_grams)
            {
                return Err(reason);
            }
            let detail = Some(serde_json::json!({
                "phase": state.phase,
                "commits_indexed": state.commits_indexed,
                "total_commits": repo_commits.max(state.total_commits),
                "diff_indexed_pct": state.diff_indexed_pct,
                "hunks_with_text": hunks_with_text,
                "diff_grams": diff_grams,
                "fresh": state.fresh,
                "schema_version": state.schema_version,
            }));
            if !state.fresh {
                // Yellow: stale — ingest has not caught up to the current refs.
                return Ok((CheckStatus::Yellow, DoctorCheckDetail {
                    summary: format!(
                        "facts db present but stale (phase {}, {} commits, {:.0}% diff coverage)",
                        state.phase,
                        state.commits_indexed,
                        state.diff_indexed_pct * 100.0
                    ),
                    detail,
                }));
            }
            Ok((CheckStatus::Green, DoctorCheckDetail {
                summary: format!(
                    "facts db fresh ({} commits, {:.0}% diff coverage, {} hunks with text, {} grams)",
                    state.commits_indexed,
                    state.diff_indexed_pct * 100.0,
                    hunks_with_text,
                    diff_grams
                ),
                detail,
            }))
        });
    }

    let Runner {
        checks, skipped, ..
    } = runner;
    let green = checks
        .iter()
        .filter(|c| c.status == CheckStatus::Green)
        .count();
    let yellow = checks
        .iter()
        .filter(|c| c.status == CheckStatus::Yellow)
        .count();
    let red = checks
        .iter()
        .filter(|c| c.status == CheckStatus::Red)
        .count();
    let ok = red == 0;

    Ok(DoctorReport {
        version: "v1".into(),
        ok,
        executable_path: exe.display().to_string(),
        home: home.display().to_string(),
        checks,
        summary: DoctorSummary {
            green,
            yellow,
            red,
            skipped,
        },
    })
}

/// `repo.pi-guard`: where the repository's pi guard stands. A guard left in
/// `.pi/agent/` by an older release is yellow, since pi never loads it there;
/// a foreign file at the guard's path fails the check.
fn pi_guard_check(
    root: &Path,
    state: crate::pi_project::GuardState,
) -> std::result::Result<(CheckStatus, DoctorCheckDetail), String> {
    use crate::pi_project::GuardState;
    Ok(match state {
        GuardState::Absent => (
            CheckStatus::Green,
            DoctorCheckDetail {
                summary: format!(
                    "no {} — repo-local pi guard not installed",
                    crate::pi_project::EXTENSION
                ),
                detail: None,
            },
        ),
        GuardState::Installed(path) => (
            CheckStatus::Green,
            DoctorCheckDetail {
                summary: format!(
                    "pi guard extension installed at {} (loads once pi trusts the project)",
                    path.display()
                ),
                detail: Some(serde_json::json!({ "path": path.display().to_string() })),
            },
        ),
        GuardState::Foreign(path) => {
            return Err(format!(
                "{} is not a pixel-managed guard extension — move it aside, then run `pixel install --repo {}`",
                path.display(),
                crate::routing::quoted_executable(root)
            ));
        }
        GuardState::Legacy(path) => (
            CheckStatus::Yellow,
            DoctorCheckDetail {
                summary: format!(
                    "pi guard at {}, which pi never loads in a project — run `pixel install --repo {}` to move it to {}",
                    path.display(),
                    crate::routing::quoted_executable(root),
                    crate::pi_project::EXTENSION
                ),
                detail: Some(serde_json::json!({ "path": path.display().to_string() })),
            },
        ),
    })
}
/// `install.rtk-backup`: yellow when `orphan` names a global RTK backup no
/// pixel guard delegates to, with the command that removes it.
fn rtk_backup_check(orphan: Option<PathBuf>) -> (CheckStatus, DoctorCheckDetail, Remedy) {
    match orphan {
        None => (
            CheckStatus::Green,
            DoctorCheckDetail {
                summary: "no orphaned RTK backup".into(),
                detail: None,
            },
            Remedy::Catalogue,
        ),
        Some(path) => {
            let remove = format!("rm {}", crate::routing::quoted_executable(&path));
            (
                CheckStatus::Yellow,
                DoctorCheckDetail {
                    summary: format!(
                        "{} holds an RTK hook no pixel guard delegates to; pixel never applies it — remove it: {remove}",
                        path.display(),
                    ),
                    detail: Some(serde_json::json!({ "path": path.display().to_string() })),
                },
                Remedy::Command(remove),
            )
        }
    }
}

struct DoctorCheckDetail {
    summary: String,
    detail: Option<serde_json::Value>,
}

/// Which repair command a yellow or red outcome carries.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Remedy {
    /// The check's command in [`CHECKS`].
    Catalogue,
    /// A command only this outcome can name, such as a path to remove.
    Command(String),
    /// No command: nothing to repair, or another check carries the fix.
    Manual,
}

/// Longest stderr excerpt a check reason quotes from a child process.
const STDERR_EXCERPT_CHARS: usize = 256;

/// Runs the checks the selection keeps and records each outcome.
struct Runner<'a> {
    only: &'a [String],
    skip: &'a [String],
    root: Option<&'a Path>,
    /// The `--shell` override, handed on to the `pixel install` a fix runs.
    shell: Option<&'a str>,
    checks: Vec<DoctorCheck>,
    skipped: usize,
}

impl Runner<'_> {
    /// Run a check that is green on `Ok` and red on `Err`.
    fn check(
        &mut self,
        id: &'static str,
        run: impl FnOnce() -> std::result::Result<DoctorCheckDetail, String>,
    ) {
        self.record(id, || {
            run().map(|d| (CheckStatus::Green, d, Remedy::Catalogue))
        });
    }

    /// Like `check`, but the closure may also report a non-fatal `Yellow`
    /// status (e.g. a stale-but-valid index) in addition to `Green`/`Red`.
    fn check_status(
        &mut self,
        id: &'static str,
        run: impl FnOnce() -> std::result::Result<(CheckStatus, DoctorCheckDetail), String>,
    ) {
        self.record(id, || {
            run().map(|(status, d)| (status, d, Remedy::Catalogue))
        });
    }

    /// Run `id` unless the selection leaves it out, timing it and attaching
    /// its repair command.
    fn record(
        &mut self,
        id: &'static str,
        run: impl FnOnce() -> std::result::Result<(CheckStatus, DoctorCheckDetail, Remedy), String>,
    ) {
        let spec = spec(id);
        if !selected(self.only, self.skip, id) {
            self.skipped += 1;
            return;
        }
        let started = Instant::now();
        let outcome = run();
        let duration_ms = started.elapsed().as_millis() as u64;
        let check = match outcome {
            Ok((status, d, remedy)) => DoctorCheck {
                id: id.into(),
                status,
                required: true,
                duration_ms,
                summary: d.summary,
                reason: None,
                detail: d.detail,
                repair: repair_for(spec, status, &remedy, self.root, self.shell),
                fix: fix_for(spec, status, remedy, self.root, self.shell),
            },
            Err(reason) => DoctorCheck {
                id: id.into(),
                status: CheckStatus::Red,
                required: true,
                duration_ms,
                summary: "check failed".into(),
                reason: Some(reason),
                detail: None,
                repair: repair_for(
                    spec,
                    CheckStatus::Red,
                    &Remedy::Catalogue,
                    self.root,
                    self.shell,
                ),
                fix: fix_for(
                    spec,
                    CheckStatus::Red,
                    Remedy::Catalogue,
                    self.root,
                    self.shell,
                ),
            },
        };
        self.checks.push(check);
    }
}

/// The catalogue entry for `id`.
///
/// # Panics
///
/// When `id` is missing from [`CHECKS`]: a check the catalogue does not list
/// could be neither selected nor repaired, which is a bug in this module.
fn spec(id: &str) -> &'static CheckSpec {
    CHECKS
        .iter()
        .find(|spec| spec.id == id)
        .unwrap_or_else(|| panic!("doctor check `{id}` is missing from CHECKS"))
}

/// Whether `id` runs: listed by `only` (or `only` is empty) and not by `skip`.
fn selected(only: &[String], skip: &[String], id: &str) -> bool {
    (only.is_empty() || only.iter().any(|o| o == id)) && !skip.iter().any(|s| s == id)
}

/// Refuse a selection that names an unknown check, or one both kept and
/// left out: either would silently run fewer checks than the caller meant.
fn validate_selection(only: &[String], skip: &[String]) -> Result<()> {
    if let Some(id) = only
        .iter()
        .chain(skip)
        .find(|id| !CHECKS.iter().any(|spec| spec.id == id.as_str()))
    {
        return Err(InstallError::UnknownDoctorCheck(id.clone()));
    }
    if let Some(id) = only.iter().find(|id| skip.contains(id)) {
        return Err(InstallError::ConflictingDoctorSelection(id.clone()));
    }
    Ok(())
}

/// The repair command a check reports: none on green, otherwise what the
/// outcome names, with `{root}` in a catalogue command replaced by the quoted
/// repository root (`.` when there is none) and `shell` handed to a
/// home-wide `pixel install`.
fn fix_for(
    spec: &CheckSpec,
    status: CheckStatus,
    remedy: Remedy,
    root: Option<&Path>,
    shell: Option<&str>,
) -> Option<String> {
    if status == CheckStatus::Green {
        return None;
    }
    match remedy {
        Remedy::Catalogue => spec.fix.map(|template| {
            let root = root.map_or_else(|| ".".to_owned(), crate::routing::quoted_executable);
            let shell = shell.map(shell_word);
            catalogue_steps(template, &root, shell.as_deref())
                .iter()
                .map(|argv| format!("pixel {}", argv.join(" ")))
                .collect::<Vec<_>>()
                .join(" && ")
        }),
        Remedy::Command(command) => Some(command),
        Remedy::Manual => None,
    }
}

/// The argv `--fix` runs for a check: the same commands as [`fix_for`], with
/// the root unquoted, and only for a flagged check whose outcome kept the
/// catalogue command.
fn repair_for(
    spec: &CheckSpec,
    status: CheckStatus,
    remedy: &Remedy,
    root: Option<&Path>,
    shell: Option<&str>,
) -> Option<Vec<Vec<String>>> {
    if status == CheckStatus::Green || *remedy != Remedy::Catalogue {
        return None;
    }
    let root = root.map_or_else(|| ".".to_owned(), |r| r.to_string_lossy().into_owned());
    spec.fix
        .map(|template| catalogue_steps(template, &root, shell))
}

/// The argv after `pixel` of each `&&`-joined command in `template`, with
/// `{root}` replaced by `root`. The home-wide `pixel install` also gets
/// `--shell <shell>` when one was given, so the repair reads the profile the
/// check read.
fn catalogue_steps(template: &str, root: &str, shell: Option<&str>) -> Vec<Vec<String>> {
    template
        .split(" && ")
        .map(|command| {
            let mut argv: Vec<String> = command
                .split_whitespace()
                .skip(1)
                .map(|word| {
                    if word == "{root}" {
                        root.to_owned()
                    } else {
                        word.to_owned()
                    }
                })
                .collect();
            if let Some(shell) = shell
                && argv == ["install"]
            {
                argv.extend(["--shell".to_owned(), shell.to_owned()]);
            }
            argv
        })
        .collect()
}

/// `shell` as one shell word: bare when it holds only path-safe characters,
/// single-quoted otherwise.
fn shell_word(shell: &str) -> String {
    if !shell.is_empty()
        && shell
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-".contains(c))
    {
        shell.to_owned()
    } else {
        crate::routing::quoted_executable(Path::new(shell))
    }
}

/// One command `pixel doctor --fix` runs, and the flagged checks it repairs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Repair {
    /// The command as the report prints it in `fix`.
    pub command: String,
    /// The argv after `pixel` of each `&&`-joined step.
    #[serde(skip)]
    pub steps: Vec<Vec<String>>,
    /// Ids of the flagged checks this command repairs, in report order.
    pub checks: Vec<String>,
}

/// The repairs `--fix` runs for `report`: one per distinct catalogue command
/// among the flagged checks, in catalogue order (home install, repo install,
/// daemon, index), so a command shared by twelve checks runs once. A check
/// whose fix only its outcome can name (a file to remove) or that has none is
/// left out and keeps its `fix:` line.
#[must_use]
pub fn repair_plan(report: &DoctorReport) -> Vec<Repair> {
    let mut plan: Vec<Repair> = Vec::new();
    for check in &report.checks {
        let (Some(steps), Some(command)) = (&check.repair, &check.fix) else {
            continue;
        };
        if let Some(repair) = plan.iter_mut().find(|r| r.steps == *steps) {
            repair.checks.push(check.id.clone());
        } else {
            plan.push(Repair {
                command: command.clone(),
                steps: steps.clone(),
                checks: vec![check.id.clone()],
            });
        }
    }
    plan
}

/// Run `repair`'s steps in order with `exe` (the pixel binary), stopping at
/// the first that fails, as `&&` would. Output is captured: stdout belongs to
/// the report, and a failure quotes the step's stderr.
///
/// # Errors
///
/// The failing step, its exit status and an excerpt of its stderr, or why it
/// could not start.
pub fn run_repair(exe: &Path, repair: &Repair) -> std::result::Result<(), String> {
    for argv in &repair.steps {
        let step = format!("pixel {}", argv.join(" "));
        let out = Command::new(exe)
            .args(argv)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("`{step}` could not start: {e}"))?;
        if !out.status.success() {
            let stderr = capped(
                &one_line(&String::from_utf8_lossy(&out.stderr)),
                STDERR_EXCERPT_CHARS,
            );
            let excerpt = if stderr.is_empty() {
                String::new()
            } else {
                format!(": {stderr}")
            };
            return Err(format!("`{step}` failed ({}){excerpt}", out.status));
        }
    }
    Ok(())
}

/// How a repair ended, judged against the report taken after every repair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RepairStatus {
    /// It succeeded and every check it targeted is green now.
    Fixed,
    /// It succeeded, yet a check it targeted is still yellow or red.
    NotConverged,
    /// A step exited non-zero or could not start.
    Failed,
}

impl fmt::Display for RepairStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Fixed => "fixed",
            Self::NotConverged => "not converged",
            Self::Failed => "failed",
        })
    }
}

/// A repair `--fix` ran and what the checks said afterwards.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RepairOutcome {
    pub command: String,
    pub checks: Vec<String>,
    pub status: RepairStatus,
    /// Targeted checks still yellow or red (or no longer reported) after
    /// every repair ran.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub still_flagged: Vec<String>,
    /// Why the repair failed, when it did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Judge `repair` from its run and the report `after` it. A command that
/// exits 0 is not taken at its word: it is `fixed` only when the checks it
/// targeted re-run green.
#[must_use]
pub fn judge_repair(
    repair: Repair,
    run: std::result::Result<(), String>,
    after: &DoctorReport,
) -> RepairOutcome {
    let still_flagged: Vec<String> = repair
        .checks
        .iter()
        .filter(|id| {
            after
                .checks
                .iter()
                .find(|c| c.id == **id)
                .is_none_or(|c| c.status != CheckStatus::Green)
        })
        .cloned()
        .collect();
    let (status, error) = match run {
        Err(error) => (RepairStatus::Failed, Some(error)),
        Ok(()) if still_flagged.is_empty() => (RepairStatus::Fixed, None),
        Ok(()) => (RepairStatus::NotConverged, None),
    };
    RepairOutcome {
        command: repair.command,
        checks: repair.checks,
        status,
        still_flagged,
        error,
    }
}

/// The terminal form of a `--fix` run: a tally line, then one line per
/// repair with the checks it targeted, and what is left when it did not end
/// `fixed`.
#[must_use]
pub fn render_repairs(outcomes: &[RepairOutcome]) -> String {
    if outcomes.is_empty() {
        return "pixel doctor --fix: nothing to repair automatically\n".to_owned();
    }
    let count = |status| outcomes.iter().filter(|o| o.status == status).count();
    let mut text = format!(
        "pixel doctor --fix: ran {} repair(s) — {} fixed, {} not converged, {} failed\n",
        outcomes.len(),
        count(RepairStatus::Fixed),
        count(RepairStatus::NotConverged),
        count(RepairStatus::Failed),
    );
    for outcome in outcomes {
        text.push_str(&format!(
            "  [{}] {} ({})\n",
            outcome.status,
            outcome.command,
            outcome.checks.join(", ")
        ));
        if let Some(error) = &outcome.error {
            text.push_str(&format!("    error: {error}\n"));
        }
        if !outcome.still_flagged.is_empty() {
            text.push_str(&format!(
                "    still flagged: {}\n",
                outcome.still_flagged.join(", ")
            ));
        }
    }
    text
}

/// `text` on one line: whitespace runs and control characters (a child's
/// newlines, a terminal escape) collapse to single spaces.
fn one_line(text: &str) -> String {
    text.split(|c: char| c.is_whitespace() || c.is_control())
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// `text` cut to `max_chars` characters, an ellipsis marking the cut.
fn capped(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let mut cut: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

/// The dead/poisoned-DB predicate for `facts.freshness`, factored out so it
/// is unit-testable without a real repo:
/// - a repo with commits but an empty facts db is DEAD (never ingested, or a
///   just-wiped poisoned db that nothing has re-ingested yet);
/// - indexed commits with ZERO diff-gram postings is the poisoned signature
///   (the historical bug stored every hunk with empty added/removed text, so
///   `diff_grams` had no rows and excavate/search returned nothing forever
///   while diff_state claimed INDEXED).
///
/// Returns `Some(reason)` when the check must go RED.
pub fn facts_dead_reason(
    commits_indexed: u64,
    repo_commits: u64,
    diff_grams: i64,
) -> Option<String> {
    if commits_indexed == 0 && repo_commits > 0 {
        return Some(format!(
            "facts db has 0 commits indexed but the repo has {repo_commits} — \
             history queries will return nothing; run `pixel build-index --history`"
        ));
    }
    if commits_indexed > 0 && diff_grams == 0 {
        return Some(format!(
            "facts db poisoned: {commits_indexed} commits indexed but 0 diff-gram \
             postings — diff text was never stored; delete .pixel/history.db or \
             re-run `pixel build-index --history`"
        ));
    }
    None
}

fn age_secs(mtime: SystemTime) -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let m = mtime.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    now.saturating_sub(m)
}

/// Path of the prompt `pixel install` deploys and the shell wrappers inject
/// (`--append-system-prompt-file`): the rule text agents actually read.
fn deployed_agent_prompt(home: &Path) -> PathBuf {
    home.join(".local/share/pixel/agent-prompt.md")
}

/// Locate the installed pixel rule text, in the order agents receive it:
/// the deployed `~/.local/share/pixel/agent-prompt.md` (0.2.x installs write
/// nothing else), else the managed block inside the first CLAUDE.md/AGENTS.md
/// that carries one (installs before 0.2.0), else the canonical rule source
/// at `~/.agent-config/rules/pixel.md`. Returns the source path and the text.
fn installed_rule_text(home: &Path) -> Option<(PathBuf, String)> {
    let prompt = deployed_agent_prompt(home);
    if let Ok(text) = fs::read_to_string(&prompt) {
        return Some((prompt, text));
    }
    for path in config::find_agent_configs(home) {
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        if let Some(start) = content.find(config::MANAGED_BEGIN) {
            let body = &content[start + config::MANAGED_BEGIN.len()..];
            let block = match body.find(config::MANAGED_END) {
                Some(end) => &body[..end],
                None => body,
            };
            return Some((path, block.to_string()));
        }
    }
    let rules = home.join(config::PIXEL_RULES_REL);
    fs::read_to_string(&rules).ok().map(|text| (rules, text))
}

/// Extract every `pixel …` command line from the fenced code blocks of a
/// rule document. Trailing `# comments` are stripped; prose and non-pixel
/// lines are ignored.
pub fn extract_rule_commands(rule_text: &str) -> Vec<String> {
    let mut in_fence = false;
    let mut out = Vec::new();
    for line in rule_text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if !in_fence {
            continue;
        }
        // Strip a trailing shell comment (` # …`) — rule examples annotate
        // commands this way.
        let code = match trimmed.find(" #") {
            Some(i) => trimmed[..i].trim_end(),
            None => trimmed,
        };
        if code.starts_with("pixel ") {
            out.push(code.to_string());
        }
    }
    out
}

/// Normalize one documented `pixel …` line into a parseable argv:
/// - `[…]` optional groups are UNWRAPPED (their flags get tested too);
/// - `a|b|c` alternations pick the first alternative;
/// - `<placeholder>` tokens (quoted or bare) become a dummy value;
/// - bare `N` becomes `3` (numeric flag placeholders);
/// - `/path/to/repo` becomes `.`;
/// - a trailing `...` variadic marker is dropped.
///
/// Returns `None` when the line contains syntax this normalizer cannot
/// handle — the caller reports such lines as "unparsed" instead of silently
/// passing them.
pub fn normalize_rule_command(line: &str) -> Option<Vec<String>> {
    // Unwrap bracketed optional groups: brackets may span several
    // whitespace-separated tokens, so strip the characters up front.
    let unbracketed: String = line.chars().filter(|c| *c != '[' && *c != ']').collect();

    // Tokenize, honoring double quotes.
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    for c in unbracketed.chars() {
        match c {
            '"' => in_quotes = !in_quotes,
            c if c.is_whitespace() && !in_quotes => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if in_quotes {
        return None; // unbalanced quotes — can't normalize
    }
    if !current.is_empty() {
        tokens.push(current);
    }

    let mut argv = Vec::with_capacity(tokens.len());
    for token in tokens {
        // Drop a trailing variadic marker (`<f>...` → `<f>`).
        let token = token.strip_suffix("...").unwrap_or(&token).to_string();
        // Placeholder → dummy value. A quoted multi-word placeholder is one
        // token by now (`<what broke, in the user's words>`).
        let token = if token.starts_with('<') && token.ends_with('>') {
            "x".to_string()
        } else {
            token
        };
        // Alternation outside placeholders: pick the first alternative
        // (`report|rebase-if-clean` → `report`, `--merge|--stash-first` →
        // `--merge`).
        let token = match token.split('|').next() {
            Some(first) if first.len() < token.len() => first.to_string(),
            _ => token,
        };
        // Well-known placeholder spellings.
        let token = match token.as_str() {
            "/path/to/repo" => ".".to_string(),
            "N" => "3".to_string(),
            _ => token,
        };
        // Anything still carrying placeholder syntax is beyond this
        // normalizer.
        if token.contains('<') || token.contains('>') || token.contains('…') {
            return None;
        }
        argv.push(token);
    }
    if argv.first().map(String::as_str) != Some("pixel") {
        return None;
    }
    Some(argv)
}

/// Compare the installed rule text and the session usage string on the
/// mandatory scenarios. Returns one message per drift found (empty = agree).
pub fn scenario_mismatches(rule_text: &str, session_usage: &str) -> Vec<String> {
    let mut out = Vec::new();
    for scenario in MANDATORY_SCENARIOS {
        // A rule text deployed before the command rename names the scenario
        // by its old name (`pixel targets`); that still runs, so it counts.
        let in_rule = rule_text.contains(&format!("pixel {scenario}"))
            || pixel_proto::commands::former_name(scenario)
                .is_some_and(|old| rule_text.contains(&format!("pixel {old}")));
        let in_usage = session_usage.contains(scenario);
        match (in_rule, in_usage) {
            (true, false) => out.push(format!(
                "'{scenario}' is mandated by the rule text but missing from the session usage string"
            )),
            (false, true) => out.push(format!(
                "'{scenario}' is in the session usage string but the rule text never mentions `pixel {scenario}`"
            )),
            (false, false) => out.push(format!(
                "'{scenario}' is missing from BOTH the rule text and the session usage string"
            )),
            (true, true) => {}
        }
    }
    out
}

/// One NDJSON retrieval round trip against a running daemon socket, checking
/// whether the response carries an `epistemics` object (envelope- or
/// data-level). Short timeouts — this is a health probe, not a query.
fn probe_daemon_epistemics(sock: &Path) -> std::result::Result<bool, String> {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    let mut stream = UnixStream::connect(sock).map_err(|e| format!("connect: {e}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .map_err(|e| e.to_string())?;
    let req = serde_json::json!({
        "op": "search",
        "pattern": "fn ",
        "json": true,
        "limit": 1,
    });
    let mut line = req.to_string();
    line.push('\n');
    stream
        .write_all(line.as_bytes())
        .map_err(|e| format!("write: {e}"))?;
    stream.flush().map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(stream);
    let mut buf = String::new();
    reader
        .read_line(&mut buf)
        .map_err(|e| format!("read: {e}"))?;
    let resp: serde_json::Value = serde_json::from_str(&buf).map_err(|e| format!("parse: {e}"))?;
    let has = resp.get("epistemics").is_some()
        || resp
            .get("data")
            .is_some_and(|d| d.get("epistemics").is_some());
    Ok(has)
}

/// Re-export the daemon socket-path helper for the CLI.
pub use pixel_daemon::daemon::socket_path as daemon_socket_path;

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::facts_dead_reason;
    use super::{
        CHECKS, CheckSpec, CheckStatus, DoctorCheck, DoctorReport, DoctorSummary, Remedy, Repair,
        RepairOutcome, RepairStatus, age_secs, capped, catalogue_steps, extract_rule_commands,
        fix_for, judge_repair, normalize_rule_command, one_line, probe_daemon_epistemics,
        render_catalogue, render_repairs, repair_for, repair_plan, rtk_backup_check, run_repair,
        scenario_mismatches, selected, shell_word, spec, validate_selection,
    };
    use crate::InstallError;

    fn finding(
        id: &str,
        status: CheckStatus,
        reason: Option<&str>,
        fix: Option<&str>,
    ) -> DoctorCheck {
        DoctorCheck {
            id: id.into(),
            status,
            required: true,
            duration_ms: 0,
            summary: format!("{id} summary"),
            reason: reason.map(Into::into),
            detail: None,
            fix: fix.map(Into::into),
            repair: None,
        }
    }

    /// A flagged check carrying the catalogue repair `steps`.
    fn repairable(id: &str, status: CheckStatus, fix: &str, steps: &[&[&str]]) -> DoctorCheck {
        DoctorCheck {
            repair: Some(argv(steps)),
            ..finding(id, status, None, Some(fix))
        }
    }

    fn argv(steps: &[&[&str]]) -> Vec<Vec<String>> {
        steps.iter().map(|step| ids(step)).collect()
    }

    fn report(checks: Vec<DoctorCheck>, skipped: usize) -> DoctorReport {
        let count = |status| checks.iter().filter(|c| c.status == status).count();
        let summary = DoctorSummary {
            green: count(CheckStatus::Green),
            yellow: count(CheckStatus::Yellow),
            red: count(CheckStatus::Red),
            skipped,
        };
        DoctorReport {
            version: "v1".into(),
            ok: summary.red == 0,
            executable_path: "/bin/pixel".into(),
            home: "/home".into(),
            checks,
            summary,
        }
    }

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(ToString::to_string).collect()
    }

    /// The exit code gates CI and the agent loop: a check exactly at the
    /// threshold must fail it, a milder one must not.
    #[test]
    fn fails_should_trip_at_the_threshold_and_not_below() {
        let green = report(vec![finding("a", CheckStatus::Green, None, None)], 0);
        let yellow = report(
            vec![
                finding("a", CheckStatus::Green, None, None),
                finding("b", CheckStatus::Yellow, None, None),
            ],
            0,
        );
        let red = report(
            vec![finding("c", CheckStatus::Red, Some("broken"), None)],
            0,
        );
        assert!(!green.fails(CheckStatus::Yellow));
        assert!(!green.fails(CheckStatus::Red));
        assert!(yellow.fails(CheckStatus::Yellow));
        assert!(
            !yellow.fails(CheckStatus::Red),
            "yellow alone passes the default gate"
        );
        assert!(red.fails(CheckStatus::Red));
        assert!(red.fails(CheckStatus::Yellow));
    }

    #[test]
    fn check_status_should_display_its_serialized_name() {
        for status in [CheckStatus::Green, CheckStatus::Yellow, CheckStatus::Red] {
            assert_eq!(
                serde_json::to_value(status).unwrap(),
                serde_json::Value::String(status.to_string())
            );
        }
        assert_eq!(CheckStatus::Yellow.to_string(), "yellow");
    }

    /// The terminal form lists what needs attention, red first, each with the
    /// command that repairs it, and hides the green checks behind the tally.
    #[test]
    fn report_display_should_list_red_before_yellow_with_fix_lines() {
        let text = report(
            vec![
                finding("binary.path", CheckStatus::Green, None, None),
                finding(
                    "facts.freshness",
                    CheckStatus::Yellow,
                    None,
                    Some("pixel build-index --history '/r'"),
                ),
                finding(
                    "install.agent-prompt",
                    CheckStatus::Red,
                    Some("stale\n  prompt"),
                    Some("pixel install"),
                ),
                finding("daemon.epistemics", CheckStatus::Yellow, None, None),
            ],
            2,
        )
        .to_string();
        let expected = [
            "pixel doctor: ran 4 check(s), skipped 2 — 1 green, 2 yellow, 1 red",
            "  [red] install.agent-prompt: stale prompt",
            "    fix: pixel install",
            "  [yellow] facts.freshness: facts.freshness summary",
            "    fix: pixel build-index --history '/r'",
            "  [yellow] daemon.epistemics: daemon.epistemics summary",
            "",
        ]
        .join("\n");
        assert_eq!(text, expected);
    }

    #[test]
    fn report_display_should_omit_the_skip_count_when_nothing_was_skipped() {
        let text = report(
            vec![finding("binary.path", CheckStatus::Green, None, None)],
            0,
        )
        .to_string();
        assert_eq!(
            text,
            "pixel doctor: ran 1 check(s) — 1 green, 0 yellow, 0 red\n"
        );
    }

    #[test]
    fn render_catalogue_should_align_ids_and_mark_checks_without_a_fix() {
        let text = render_catalogue(&[
            CheckSpec {
                id: "binary.path",
                fix: None,
            },
            CheckSpec {
                id: "index.freshness",
                fix: Some("pixel prepare-repo {root}"),
            },
        ]);
        assert_eq!(
            text,
            [
                "binary.path      -",
                "index.freshness  pixel prepare-repo {root}",
                ""
            ]
            .join("\n")
        );
    }

    /// `--only`/`--skip` address checks by id, so two entries sharing one
    /// would make a selection ambiguous.
    #[test]
    fn checks_should_have_unique_ids() {
        let mut seen = std::collections::HashSet::new();
        for check in CHECKS {
            assert!(seen.insert(check.id), "duplicate id {}", check.id);
        }
    }

    #[test]
    fn spec_should_return_the_catalogue_entry() {
        assert_eq!(
            spec("facts.freshness").fix,
            Some("pixel build-index --history {root}")
        );
    }

    #[test]
    #[should_panic(expected = "doctor check `nope` is missing from CHECKS")]
    fn spec_should_panic_on_an_uncatalogued_id() {
        let _ = spec("nope");
    }

    #[test]
    fn selected_should_keep_only_listed_ids_and_drop_skipped_ones() {
        assert!(
            selected(&[], &[], "binary.path"),
            "no selection runs everything"
        );
        assert!(selected(&ids(&["binary.path"]), &[], "binary.path"));
        assert!(!selected(&ids(&["binary.path"]), &[], "daemon.health"));
        assert!(!selected(&[], &ids(&["daemon.health"]), "daemon.health"));
        assert!(selected(&[], &ids(&["daemon.health"]), "binary.path"));
    }

    #[test]
    fn validate_selection_should_refuse_unknown_and_conflicting_ids() {
        assert!(validate_selection(&ids(&["binary.path"]), &ids(&["daemon.health"])).is_ok());
        assert!(matches!(
            validate_selection(&ids(&["binary.path", "nope"]), &[]),
            Err(InstallError::UnknownDoctorCheck(id)) if id == "nope"
        ));
        assert!(matches!(
            validate_selection(&[], &ids(&["typo"])),
            Err(InstallError::UnknownDoctorCheck(id)) if id == "typo"
        ));
        assert!(matches!(
            validate_selection(&ids(&["binary.path"]), &ids(&["binary.path"])),
            Err(InstallError::ConflictingDoctorSelection(id)) if id == "binary.path"
        ));
    }

    /// A fix is a command an agent may run as is: never on a healthy check,
    /// and always pointed at the repository the report is about.
    #[test]
    fn fix_for_should_name_a_command_only_for_a_check_that_needs_one() {
        let repo = CheckSpec {
            id: "repo.pi-guard",
            fix: Some("pixel install --repo {root}"),
        };
        let root = Path::new("/tmp/it's here");
        assert_eq!(
            fix_for(
                &repo,
                CheckStatus::Green,
                Remedy::Catalogue,
                Some(root),
                None
            ),
            None
        );
        assert_eq!(
            fix_for(&repo, CheckStatus::Red, Remedy::Catalogue, Some(root), None).as_deref(),
            Some("pixel install --repo '/tmp/it'\\''s here'")
        );
        assert_eq!(
            fix_for(&repo, CheckStatus::Yellow, Remedy::Catalogue, None, None).as_deref(),
            Some("pixel install --repo .")
        );
        assert_eq!(
            fix_for(
                &repo,
                CheckStatus::Yellow,
                Remedy::Command("rm x".into()),
                Some(root),
                None
            )
            .as_deref(),
            Some("rm x")
        );
        assert_eq!(
            fix_for(&repo, CheckStatus::Red, Remedy::Manual, Some(root), None),
            None
        );
        let bare = CheckSpec {
            id: "binary.path",
            fix: None,
        };
        assert_eq!(
            fix_for(&bare, CheckStatus::Red, Remedy::Catalogue, Some(root), None),
            None
        );
    }

    /// `--fix` runs a catalogue command by dropping its leading word, so a
    /// template that did not start with `pixel` would run the wrong program.
    #[test]
    fn every_catalogue_command_should_be_a_chain_of_pixel_invocations() {
        for check in CHECKS {
            for command in check.fix.iter().flat_map(|fix| fix.split(" && ")) {
                assert!(command.starts_with("pixel "), "{}: {command}", check.id);
            }
        }
    }

    #[test]
    fn catalogue_steps_should_split_chains_and_substitute_the_root() {
        assert_eq!(
            catalogue_steps(
                "pixel daemon stop {root} && pixel daemon start {root}",
                "/r",
                None
            ),
            argv(&[&["daemon", "stop", "/r"], &["daemon", "start", "/r"]])
        );
    }

    /// The home install reads the shell profile, so the repair must target
    /// the shell the check was told about; the repo install and every other
    /// command take no `--shell`.
    #[test]
    fn catalogue_steps_should_hand_the_shell_to_the_home_install_only() {
        assert_eq!(
            catalogue_steps("pixel install", "/r", Some("fish")),
            argv(&[&["install", "--shell", "fish"]])
        );
        assert_eq!(
            catalogue_steps("pixel install", "/r", None),
            argv(&[&["install"]])
        );
        assert_eq!(
            catalogue_steps("pixel install --repo {root}", "/r", Some("fish")),
            argv(&[&["install", "--repo", "/r"]])
        );
    }

    #[test]
    fn fix_for_should_print_the_shell_the_repair_will_use() {
        let install = spec("install.agent-prompt");
        assert_eq!(
            fix_for(
                install,
                CheckStatus::Red,
                Remedy::Catalogue,
                None,
                Some("fish")
            )
            .as_deref(),
            Some("pixel install --shell fish")
        );
        assert_eq!(
            fix_for(
                install,
                CheckStatus::Red,
                Remedy::Catalogue,
                None,
                Some("my sh")
            )
            .as_deref(),
            Some("pixel install --shell 'my sh'")
        );
        assert_eq!(
            fix_for(
                spec("daemon.epistemics"),
                CheckStatus::Yellow,
                Remedy::Catalogue,
                Some(Path::new("/r")),
                Some("fish")
            )
            .as_deref(),
            Some("pixel daemon stop '/r' && pixel daemon start '/r'")
        );
    }

    #[test]
    fn shell_word_should_quote_only_what_a_shell_would_split() {
        assert_eq!(shell_word("fish"), "fish");
        assert_eq!(
            shell_word("/opt/homebrew/bin/fish-3.7_x"),
            "/opt/homebrew/bin/fish-3.7_x"
        );
        assert_eq!(shell_word("a b"), "'a b'");
        assert_eq!(shell_word(""), "''");
    }

    /// `--fix` runs only the catalogue command of a flagged check, with the
    /// root as one raw argument (no shell quoting: nothing parses it again).
    #[test]
    fn repair_for_should_carry_argv_only_for_a_flagged_catalogue_fix() {
        let repo = spec("repo.pi-guard");
        let root = Path::new("/tmp/it's here");
        assert_eq!(
            repair_for(
                repo,
                CheckStatus::Yellow,
                &Remedy::Catalogue,
                Some(root),
                None
            ),
            Some(argv(&[&["install", "--repo", "/tmp/it's here"]]))
        );
        assert_eq!(
            repair_for(repo, CheckStatus::Red, &Remedy::Catalogue, None, None),
            Some(argv(&[&["install", "--repo", "."]]))
        );
        assert_eq!(
            repair_for(
                repo,
                CheckStatus::Green,
                &Remedy::Catalogue,
                Some(root),
                None
            ),
            None
        );
        assert_eq!(
            repair_for(
                repo,
                CheckStatus::Red,
                &Remedy::Command("rm x".into()),
                Some(root),
                None
            ),
            None,
            "a command one outcome names stays the user's call"
        );
        assert_eq!(
            repair_for(repo, CheckStatus::Red, &Remedy::Manual, Some(root), None),
            None
        );
        assert_eq!(
            repair_for(
                spec("binary.path"),
                CheckStatus::Red,
                &Remedy::Catalogue,
                Some(root),
                None
            ),
            None
        );
    }

    /// Twelve checks share `pixel install`: the plan runs it once, lists
    /// every check it repairs, and keeps the report's order.
    #[test]
    fn repair_plan_should_run_each_command_once_in_report_order() {
        let install: &[&[&str]] = &[&["install"]];
        let prepare: &[&[&str]] = &[&["prepare-repo", "/r"]];
        let plan = repair_plan(&report(
            vec![
                finding("binary.path", CheckStatus::Green, None, None),
                repairable(
                    "install.agent-prompt",
                    CheckStatus::Red,
                    "pixel install",
                    install,
                ),
                finding(
                    "install.rtk-backup",
                    CheckStatus::Yellow,
                    None,
                    Some("rm '/h/rtk.json'"),
                ),
                repairable("rule.parity", CheckStatus::Yellow, "pixel install", install),
                repairable(
                    "index.freshness",
                    CheckStatus::Yellow,
                    "pixel prepare-repo '/r'",
                    prepare,
                ),
                repairable(
                    "graph.freshness",
                    CheckStatus::Red,
                    "pixel prepare-repo '/r'",
                    prepare,
                ),
            ],
            0,
        ));
        assert_eq!(
            plan,
            vec![
                Repair {
                    command: "pixel install".into(),
                    steps: argv(install),
                    checks: ids(&["install.agent-prompt", "rule.parity"]),
                },
                Repair {
                    command: "pixel prepare-repo '/r'".into(),
                    steps: argv(prepare),
                    checks: ids(&["index.freshness", "graph.freshness"]),
                },
            ]
        );
    }

    fn sh_repair(steps: &[&[&str]]) -> Repair {
        Repair {
            command: "c".into(),
            steps: argv(steps),
            checks: vec![],
        }
    }

    #[test]
    fn run_repair_should_run_every_step_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log");
        let append = |word: &str| format!("echo {word} >> '{}'", log.display());
        let (first, second) = (append("one"), append("two"));
        let repair = sh_repair(&[&["-c", &first], &["-c", &second]]);
        assert_eq!(run_repair(Path::new("/bin/sh"), &repair), Ok(()));
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "one\ntwo\n");
    }

    /// Like `&&`: once a step fails the rest of the repair does not run, and
    /// the error names the step and quotes its stderr.
    #[test]
    fn run_repair_should_stop_at_the_first_failing_step() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("marker");
        let touch = format!("touch '{}'", marker.display());
        let repair = sh_repair(&[&["-c", "echo boom >&2; exit 3"], &["-c", &touch]]);
        let error = run_repair(Path::new("/bin/sh"), &repair).unwrap_err();
        assert_eq!(
            error,
            "`pixel -c echo boom >&2; exit 3` failed (exit status: 3): boom"
        );
        assert!(!marker.exists(), "the step after the failure ran");
        let silent = run_repair(Path::new("/bin/sh"), &sh_repair(&[&["-c", "exit 4"]]));
        assert_eq!(
            silent,
            Err("`pixel -c exit 4` failed (exit status: 4)".into())
        );
    }

    #[test]
    fn run_repair_should_report_a_binary_that_cannot_start() {
        let error =
            run_repair(Path::new("/nonexistent/pixel"), &sh_repair(&[&["install"]])).unwrap_err();
        assert!(
            error.starts_with("`pixel install` could not start: "),
            "{error}"
        );
    }

    /// A command that exits 0 is not proof: the verdict comes from the checks
    /// re-run after every repair.
    #[test]
    fn judge_repair_should_trust_the_rerun_checks_over_the_exit_code() {
        let after = report(
            vec![
                finding("install.agent-prompt", CheckStatus::Green, None, None),
                finding("index.freshness", CheckStatus::Green, None, None),
                finding("graph.freshness", CheckStatus::Yellow, None, None),
            ],
            0,
        );
        let repair = |checks: &[&str]| Repair {
            command: "pixel x".into(),
            steps: vec![],
            checks: ids(checks),
        };
        let fixed = judge_repair(repair(&["install.agent-prompt"]), Ok(()), &after);
        assert_eq!(
            (fixed.status, fixed.still_flagged),
            (RepairStatus::Fixed, vec![])
        );
        let stuck = judge_repair(
            repair(&["index.freshness", "graph.freshness"]),
            Ok(()),
            &after,
        );
        assert_eq!(
            (stuck.status, stuck.still_flagged, stuck.error),
            (RepairStatus::NotConverged, ids(&["graph.freshness"]), None)
        );
        let vanished = judge_repair(repair(&["daemon.health"]), Ok(()), &after);
        assert_eq!(
            (vanished.status, vanished.still_flagged),
            (RepairStatus::NotConverged, ids(&["daemon.health"]))
        );
        let failed = judge_repair(
            repair(&["install.agent-prompt"]),
            Err("boom".into()),
            &after,
        );
        assert_eq!(
            (failed.status, failed.error.as_deref()),
            (RepairStatus::Failed, Some("boom"))
        );
    }

    #[test]
    fn render_repairs_should_tally_and_detail_every_outcome() {
        let outcome =
            |status, checks: &[&str], still: &[&str], error: Option<&str>| RepairOutcome {
                command: format!("pixel {status}"),
                checks: ids(checks),
                status,
                still_flagged: ids(still),
                error: error.map(Into::into),
            };
        let text = render_repairs(&[
            outcome(RepairStatus::Fixed, &["a", "b"], &[], None),
            outcome(RepairStatus::NotConverged, &["c", "d"], &["d"], None),
            outcome(
                RepairStatus::Failed,
                &["e"],
                &["e"],
                Some("`pixel e` failed"),
            ),
        ]);
        let expected = [
            "pixel doctor --fix: ran 3 repair(s) — 1 fixed, 1 not converged, 1 failed",
            "  [fixed] pixel fixed (a, b)",
            "  [not converged] pixel not converged (c, d)",
            "    still flagged: d",
            "  [failed] pixel failed (e)",
            "    error: `pixel e` failed",
            "    still flagged: e",
            "",
        ]
        .join("\n");
        assert_eq!(text, expected);
        assert_eq!(
            render_repairs(&[]),
            "pixel doctor --fix: nothing to repair automatically\n"
        );
    }

    #[test]
    fn repair_status_should_serialize_in_snake_case() {
        assert_eq!(
            serde_json::to_value(RepairStatus::NotConverged).unwrap(),
            "not_converged"
        );
    }

    #[test]
    fn rtk_backup_check_should_carry_the_removal_command_as_its_fix() {
        let (status, _, remedy) = rtk_backup_check(None);
        assert_eq!((status, remedy), (CheckStatus::Green, Remedy::Catalogue));
        let (status, detail, remedy) = rtk_backup_check(Some(PathBuf::from("/h/.claude/rtk.json")));
        assert_eq!(status, CheckStatus::Yellow);
        assert_eq!(remedy, Remedy::Command("rm '/h/.claude/rtk.json'".into()));
        assert!(
            detail
                .summary
                .ends_with("remove it: rm '/h/.claude/rtk.json'"),
            "{}",
            detail.summary
        );
    }

    #[test]
    fn one_line_should_fold_newlines_and_control_characters_into_single_spaces() {
        assert_eq!(
            one_line("  error:\n\tbad\x1b[31m  thing \r\n"),
            "error: bad [31m thing"
        );
        assert_eq!(one_line("plain"), "plain");
    }

    #[test]
    fn capped_should_keep_text_at_the_limit_and_cut_beyond_it() {
        assert_eq!(capped("abcd", 4), "abcd", "exactly at the cap is kept");
        assert_eq!(capped("abcde", 4), "abc…");
        assert_eq!(
            capped("éééé", 3).chars().count(),
            3,
            "counts characters, not bytes"
        );
    }

    #[test]
    fn age_secs_is_the_seconds_since_the_mtime() {
        let ninety_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(90);
        let age = age_secs(ninety_ago);
        assert!((90..=91).contains(&age), "{age}");
        assert_eq!(
            age_secs(std::time::SystemTime::now() + std::time::Duration::from_secs(60)),
            0
        );
    }

    /// The daemon health probe reads one NDJSON answer and looks for the
    /// epistemics object at either level; a daemon that answers without one
    /// is reported unhealthy, not as an error.
    #[test]
    fn probe_daemon_epistemics_reads_one_answer_from_the_socket() {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixListener;
        let dir = std::env::temp_dir().join(format!("pixel-probe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (answer, expected) in [
            (
                r#"{"ok":true,"op":"search","epistemics":{"basis":"x"}}"#,
                true,
            ),
            (
                r#"{"ok":true,"op":"search","data":{"epistemics":{}}}"#,
                true,
            ),
            (r#"{"ok":true,"op":"search","data":{}}"#, false),
        ] {
            let sock = dir.join("daemon.sock");
            let _ = std::fs::remove_file(&sock);
            let listener = UnixListener::bind(&sock).unwrap();
            // Poll instead of blocking: a probe that never connects must
            // leave a failed assertion, not a hung test.
            listener.set_nonblocking(true).unwrap();
            let sent = answer.to_string();
            let server = std::thread::spawn(move || {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                let stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            if std::time::Instant::now() > deadline {
                                return;
                            }
                            std::thread::sleep(std::time::Duration::from_millis(10));
                        }
                        Err(e) => panic!("accept: {e}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request = String::new();
                reader.read_line(&mut request).unwrap();
                let req: serde_json::Value = serde_json::from_str(&request).unwrap();
                assert_eq!(req["op"], "search");
                let mut stream = stream;
                writeln!(stream, "{sent}").unwrap();
            });
            assert_eq!(probe_daemon_epistemics(&sock), Ok(expected), "{answer}");
            server.join().unwrap();
        }
        let err = probe_daemon_epistemics(&dir.join("absent.sock")).unwrap_err();
        assert!(err.starts_with("connect: "), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -- rule-vs-binary parity: extraction + normalization ------------------

    const SAMPLE_RULE: &str = r#"
## Scenario 1

```bash
# Deleted or currently-nonexistent code: search all history, stash, and reflog
pixel dig-history --phrase "<what you're looking for>" [--path <path>] [--json]

pixel plan-rollback "<what broke, in the user's words>" /path/to/repo [--json]

pixel plan-rollback --apply <oid> --file <path> /path/to/repo [--merge|--stash-first|--allow-dirty]
```

Prose mentioning `pixel doctor` inline must NOT be extracted.

```bash
pixel scope-task --clear /path/to/repo   # when the task ends
pixel sync-branch /path/to/repo [--strategy report|rebase-if-clean] [--push auto|never]
git clone https://example.com/repo.git
```
"#;

    #[test]
    fn extracts_only_fenced_pixel_lines_and_strips_comments() {
        let commands = extract_rule_commands(SAMPLE_RULE);
        assert_eq!(
            commands,
            vec![
                "pixel dig-history --phrase \"<what you're looking for>\" [--path <path>] [--json]",
                "pixel plan-rollback \"<what broke, in the user's words>\" /path/to/repo [--json]",
                "pixel plan-rollback --apply <oid> --file <path> /path/to/repo [--merge|--stash-first|--allow-dirty]",
                "pixel scope-task --clear /path/to/repo",
                "pixel sync-branch /path/to/repo [--strategy report|rebase-if-clean] [--push auto|never]",
            ],
            "must extract exactly the fenced pixel lines, comment-stripped, no inline prose"
        );
    }

    #[test]
    fn normalizes_placeholders_brackets_and_alternations() {
        assert_eq!(
            normalize_rule_command(
                "pixel find-code \"<phrase>\" /path/to/repo [--json] [--limit N]"
            ),
            Some(vec![
                "pixel".into(),
                "find-code".into(),
                "x".into(),
                ".".into(),
                "--json".into(),
                "--limit".into(),
                "3".into(),
            ])
        );
        assert_eq!(
            normalize_rule_command(
                "pixel sync-branch /path/to/repo [--strategy report|rebase-if-clean] [--push auto|never]"
            ),
            Some(vec![
                "pixel".into(),
                "sync-branch".into(),
                ".".into(),
                "--strategy".into(),
                "report".into(),
                "--push".into(),
                "auto".into(),
            ])
        );
        assert_eq!(
            normalize_rule_command(
                "pixel commit --files <f>... --message \"<msg>\" --request-id <id> /path/to/repo"
            ),
            Some(vec![
                "pixel".into(),
                "commit".into(),
                "--files".into(),
                "x".into(),
                "--message".into(),
                "x".into(),
                "--request-id".into(),
                "x".into(),
                ".".into(),
            ])
        );
        // Bracketed flag alternation picks the first flag.
        assert_eq!(
            normalize_rule_command(
                "pixel plan-rollback --apply <oid> --file <path> /path/to/repo [--merge|--stash-first|--allow-dirty]"
            )
            .as_deref()
            .and_then(|v| v.last().cloned()),
            Some("--merge".to_string())
        );
    }

    #[test]
    fn unnormalizable_lines_are_reported_not_silently_passed() {
        // Unbalanced quotes.
        assert_eq!(
            normalize_rule_command("pixel search-content \"unclosed"),
            None
        );
        // Ellipsis placeholder syntax the normalizer doesn't understand.
        assert_eq!(normalize_rule_command("pixel search-content a…b"), None);
        // Not a pixel line at all.
        assert_eq!(normalize_rule_command("git status"), None);
    }

    // -- scenario consistency ------------------------------------------------

    #[test]
    fn scenario_agreement_is_empty_when_both_sides_name_all_five() {
        let rule = "use pixel scope-task first, pixel find-code for phrases, \
                    pixel plan-rollback for history, pixel sync-branch for sync, \
                    pixel impact before edits";
        assert!(
            scenario_mismatches(rule, pixel_proto::op::SESSION_USAGE).is_empty(),
            "all five scenarios present on both sides must produce zero mismatches"
        );
    }

    #[test]
    fn scenarios_named_by_their_pre_rename_names_still_agree() {
        // Every install before the rename deployed this vocabulary.
        let old_rule = "use pixel targets first, pixel resolve for phrases, \
                        pixel rescue for history, pixel reconcile for sync, \
                        pixel impact before edits";
        assert!(
            scenario_mismatches(old_rule, pixel_proto::op::SESSION_USAGE).is_empty(),
            "old command names in the rule text must satisfy the scenarios"
        );
        let mixed =
            "pixel scope-task, pixel resolve, pixel plan-rollback, pixel reconcile, pixel impact";
        assert!(scenario_mismatches(mixed, pixel_proto::op::SESSION_USAGE).is_empty());
        // A name that was never a scenario does not stand in for one.
        let wrong = "pixel targets, pixel resolve, pixel rescue, pixel sync, pixel impact";
        let drift = scenario_mismatches(wrong, pixel_proto::op::SESSION_USAGE);
        assert_eq!(drift.len(), 1, "{drift:?}");
        assert!(drift[0].contains("'sync-branch'"), "{drift:?}");
    }

    #[test]
    fn scenario_drift_is_flagged_per_missing_side() {
        let rule_without_impact =
            "pixel scope-task, pixel find-code, pixel plan-rollback, pixel sync-branch";
        let usage_without_impact =
            "scope-task find-code plan-rollback sync-branch — four scenarios only";
        // Rule lacks impact → usage-only drift message.
        let drift = scenario_mismatches(rule_without_impact, pixel_proto::op::SESSION_USAGE);
        assert_eq!(
            drift.len(),
            1,
            "exactly the impact scenario drifts: {drift:?}"
        );
        assert!(drift[0].contains("impact"));
        // Usage lacks impact while the rule mandates it → red-worthy drift.
        let rule_full =
            "pixel scope-task pixel find-code pixel plan-rollback pixel sync-branch pixel impact";
        let drift = scenario_mismatches(rule_full, usage_without_impact);
        assert_eq!(drift.len(), 1, "{drift:?}");
        assert!(drift[0].contains("missing from the session usage string"));
    }

    #[test]
    fn live_session_usage_and_live_rule_source_agree_when_rule_readable() {
        // The real parity gate runs inside `pixel doctor` against the
        // installed text; here we only pin that the SESSION_USAGE constant
        // itself names every mandatory scenario.
        for scenario in super::MANDATORY_SCENARIOS {
            assert!(
                pixel_proto::op::SESSION_USAGE.contains(scenario),
                "SESSION_USAGE must name '{scenario}'"
            );
        }
    }

    #[test]
    fn poisoned_db_signature_is_red() {
        // The real-world poisoned DB: 11 commits marked indexed, 323 hunks all
        // with empty text, therefore 0 diff_grams rows.
        let reason = facts_dead_reason(11, 21, 0);
        assert!(
            reason.as_deref().unwrap_or("").contains("poisoned"),
            "indexed commits with zero grams must be flagged poisoned, got {reason:?}"
        );
    }

    #[test]
    fn empty_db_in_nonempty_repo_is_red() {
        let reason = facts_dead_reason(0, 21, 0);
        assert!(
            reason.is_some(),
            "0 indexed commits while the repo has commits must be RED"
        );
    }

    #[test]
    fn healthy_and_trivially_empty_cases_are_not_red() {
        assert_eq!(facts_dead_reason(21, 21, 50_000), None, "healthy db");
        assert_eq!(facts_dead_reason(0, 0, 0), None, "empty repo, empty db");
    }
}

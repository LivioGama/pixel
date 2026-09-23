//! Idempotent provider integration: safe shell routing and lifecycle context.
//! Unknown overlapping hooks are preserved rather than double-rewritten.
//! Configuration alone is not proof that an agent has executed the hooks.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::InstallError;
use crate::config;

pub type Result<T> = std::result::Result<T, InstallError>;

/// Per-check status for the install report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Green,
    Yellow,
    Red,
}

/// One install action and its outcome.
#[derive(Debug, Clone, Serialize)]
pub struct InstallStep {
    pub id: String,
    pub status: CheckStatus,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// The full install report.
#[derive(Debug, Clone, Serialize)]
pub struct InstallReport {
    pub version: String,
    pub ok: bool,
    pub executable_path: String,
    pub home: String,
    /// True if this report describes a dry run: every step below reflects
    /// what WOULD happen, but no filesystem write occurred.
    pub dry_run: bool,
    pub steps: Vec<InstallStep>,
    pub summary: InstallSummary,
}

#[derive(Debug, Clone, Serialize)]
pub struct InstallSummary {
    pub green: usize,
    pub yellow: usize,
    pub red: usize,
}

/// Options controlling an install run.
#[derive(Debug, Clone, Default)]
pub struct InstallOptions {
    /// Path to the pixel binary the installed hooks point at. Defaults to
    /// the current exe.
    pub executable_path: Option<PathBuf>,
    /// Home directory. Defaults to `$HOME`.
    pub home: Option<PathBuf>,
    /// Shell whose profile is checked for a legacy `claude()` wrapper block
    /// to remove, as a `$SHELL`-style value (`fish`, `/bin/bash`, …).
    /// Defaults to the account's login shell, then `$SHELL`.
    pub shell: Option<String>,
    /// Deprecated and ignored: the retired shell wrapper needed a `claude
    /// --version` probe to decide whether `--append-subagent-system-prompt-file`
    /// was safe. The SessionStart hook injects the prompt now, so no Claude
    /// probe happens.
    pub claude_executable: Option<PathBuf>,
    /// If true, compute and report every step's outcome exactly as a real
    /// run would, but perform no filesystem writes: no settings.json edits,
    /// no hook files, no agent-config rewrites, no backups, no directory
    /// creation. Safe to run against a real `$HOME` to preview an install.
    pub dry_run: bool,
    /// Repository root to install project-local enforcement into
    /// (`pixel install --repo <path>`). When set, ONLY repo-local steps run:
    /// `.codex/config.toml` + `.codex/hooks.json`, `.devin/config.local.json`,
    /// `.claude/settings.local.json` (PreToolUse guard), and
    /// `.pi/extensions/pixel-guard.ts` ([`REPO_ARTIFACTS`]) — none of the
    /// global prompt/lifecycle-hook deploys.
    pub repo: Option<PathBuf>,
}

/// First executable file named `name` on PATH.
#[cfg(test)]
pub(crate) fn find_on_path(name: &str) -> Option<PathBuf> {
    find_in_paths(name, &std::env::var_os("PATH")?)
}

/// First executable file named `name` in the PATH-style list `path`.
#[cfg(test)]
fn find_in_paths(name: &str, path: &std::ffi::OsStr) -> Option<PathBuf> {
    std::env::split_paths(path).find_map(|dir| {
        let candidate = dir.join(name);
        if !candidate.is_file() {
            return None;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let executable = candidate
                .metadata()
                .is_ok_and(|meta| meta.permissions().mode() & 0o111 != 0);
            executable.then_some(candidate)
        }
        #[cfg(not(unix))]
        {
            Some(candidate)
        }
    })
}

/// Run `pixel install`. Idempotent: safe to re-run.
///
/// The install deploys the agent system prompt, registers the Claude
/// lifecycle hooks in `~/.claude/settings.json` (the SessionStart hook
/// injects that prompt into EVERY Claude process — the retired `claude()`
/// shell wrapper only ever fired for human login shells and is removed),
/// and sets up the Codex `developer_instructions` config key so every
/// invocation includes the Pixel retrieval protocol —
/// and, when OpenCode is present, a managed block in its global
/// `~/.config/opencode/AGENTS.md`. Codex also gets the metrics relay: its
/// tool results never surface stderr, so a PostToolUse entry re-emits the
/// finalized 🟩 line as context. No managed blocks in the home-level
/// CLAUDE.md/AGENTS.md files; PreToolUse enforcement is repo-local
/// (`pixel install --repo`).
pub fn install(options: &InstallOptions) -> Result<InstallReport> {
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

    let dry_run = options.dry_run;
    if let Some(repo) = &options.repo {
        return install_project(repo, &home, &exe, dry_run);
    }
    let codex_home = crate::codex_config::codex_home(&home, options.home.is_some());
    let mut steps = vec![
        deploy_agent_prompt(&home, dry_run)?,
        // The doctrine reaches EVERY Claude process through the lifecycle
        // hooks in ~/.claude/settings.json: SessionStart injects the agent
        // prompt itself, so direct `claude` launches (cmux, agents, cron)
        // get it without going through a shell function. No PreToolUse here —
        // enforcement is repo-local (`pixel install --repo`).
        crate::routing::install_at_scoped(
            &home,
            &crate::routing::Provider::Claude.path(&home),
            &exe,
            crate::routing::Provider::Claude,
            crate::routing::HookScope::LifecycleOnly,
            &[],
            dry_run,
        )?,
        // The zsh `claude()` wrapper only ever fired for human login shells
        // and would now double-inject alongside SessionStart — strip it.
        remove_legacy_wrappers(&home, options.shell.as_deref(), dry_run)?,
        crate::codex_config::install_developer_instructions(&codex_home, dry_run)?,
        crate::codex_config::install_metrics_hook(&codex_home, &exe, dry_run)?,
    ];
    let opencode_dir = crate::opencode_config::opencode_config_dir(&home, options.home.is_some());
    if opencode_dir.is_dir() {
        steps.push(crate::opencode_config::install_opencode(
            &opencode_dir,
            &home,
            dry_run,
        )?);
    }
    if crate::antigravity::antigravity_config_dir(&home).is_dir() {
        steps.push(crate::antigravity::deploy_plugin_assets(
            &home, &exe, dry_run,
        )?);
        steps.push(crate::antigravity::enable_plugin_in_config(&home, dry_run)?);
        steps.push(crate::antigravity::install_global_hooks(
            &home, &exe, dry_run,
        )?);
    }

    let green = steps
        .iter()
        .filter(|s| s.status == CheckStatus::Green)
        .count();
    let yellow = steps
        .iter()
        .filter(|s| s.status == CheckStatus::Yellow)
        .count();
    let red = steps
        .iter()
        .filter(|s| s.status == CheckStatus::Red)
        .count();
    let ok = red == 0;

    Ok(InstallReport {
        version: "v1".into(),
        ok,
        executable_path: exe.display().to_string(),
        home: home.display().to_string(),
        dry_run,
        steps,
        summary: InstallSummary { green, yellow, red },
    })
}

/// Repo-local install (`pixel install --repo <path>`): project-scoped agent
/// enforcement instead of the global prompt deploy. Writes:
///   - `<repo>/.codex/config.toml` — the same `developer_instructions`
///     managed block the global install writes (Codex merges a project-local
///     config.toml over the global one);
///   - `<repo>/.codex/hooks.json` — the composed-guard PreToolUse group plus
///     its `pixel-composed-guard-backup.json` sidecar, which snapshots any
///     pre-existing project hooks so the composed runtime can replay them;
///   - `<repo>/.devin/config.local.json` — a pixel `run-hook guard --provider
///     devin` PreToolUse group merged alongside any foreign entries;
///   - `<repo>/.claude/settings.local.json` — a pixel `run-hook guard
///     --provider claude` PreToolUse group merged alongside any foreign
///     entries (the personal project settings: the command names this
///     machine's binary, so the shared `settings.json` never carries it);
///   - `<repo>/.pi/extensions/pixel-guard.ts` — pi's guard extension
///     ([`crate::pi_project`]).
///
/// Every one of those files is then listed in the clone's `info/exclude`
/// ([`crate::repo_git`]). Codex has no personal project file, so a
/// `.codex/hooks.json` the repository tracks is left alone: the composed
/// guard would put this machine's path into a file every clone runs.
fn install_project(repo: &Path, home: &Path, exe: &Path, dry_run: bool) -> Result<InstallReport> {
    let codex_dir = repo.join(".codex");
    let codex_hooks = codex_dir.join(crate::codex_config::HOOKS_FILE);
    let codex_step = if crate::repo_git::is_tracked(repo, CODEX_PROJECT_HOOKS) {
        InstallStep {
            id: "hooks.codex".into(),
            status: CheckStatus::Yellow,
            summary: dry_run_summary(
                dry_run,
                "codex project guard not installed: .codex/hooks.json is tracked by git, and the guard would carry this machine's pixel path into every clone",
            ),
            detail: Some(codex_hooks.display().to_string()),
        }
    } else {
        crate::routing::install_project_codex_at(home, &codex_hooks, exe, dry_run)?
    };
    let steps = vec![
        crate::routing::install_project_claude_at(repo, home, exe, dry_run)?,
        crate::codex_config::install_developer_instructions(&codex_dir, dry_run)?,
        codex_step,
        crate::routing::install_project_devin_at(repo, exe, dry_run)?,
        crate::pi_project::install(repo, exe, dry_run)?,
        exclude_project_artifacts(repo, dry_run)?,
    ];

    let green = steps
        .iter()
        .filter(|s| s.status == CheckStatus::Green)
        .count();
    let yellow = steps
        .iter()
        .filter(|s| s.status == CheckStatus::Yellow)
        .count();
    let red = steps
        .iter()
        .filter(|s| s.status == CheckStatus::Red)
        .count();

    Ok(InstallReport {
        version: "v1".into(),
        ok: red == 0,
        executable_path: exe.display().to_string(),
        home: repo.display().to_string(),
        dry_run,
        steps,
        summary: InstallSummary { green, yellow, red },
    })
}

/// The Codex project hook file, relative to the repository.
const CODEX_PROJECT_HOOKS: &str = ".codex/hooks.json";

/// A file `pixel install --repo` may write, relative to the repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepoArtifact {
    /// Path relative to the repository root.
    pub path: &'static str,
    /// Whether the file names this machine's pixel binary, so the install
    /// lists it in the clone's `info/exclude` to keep it out of commits.
    pub machine_local: bool,
}

/// Every file `pixel install --repo` may write. The per-project list in
/// `website/content/docs.md` and the `--repo` help name exactly these paths,
/// which `docs_drift::` checks.
pub const REPO_ARTIFACTS: &[RepoArtifact] = &[
    RepoArtifact {
        path: crate::routing::CLAUDE_LOCAL_SETTINGS,
        machine_local: true,
    },
    RepoArtifact {
        path: crate::routing::RTK_BACKUP,
        machine_local: true,
    },
    RepoArtifact {
        path: ".codex/config.toml",
        machine_local: false,
    },
    RepoArtifact {
        path: CODEX_PROJECT_HOOKS,
        machine_local: true,
    },
    RepoArtifact {
        path: ".codex/pixel-composed-guard-backup.json",
        machine_local: true,
    },
    RepoArtifact {
        path: crate::routing::DEVIN_LOCAL_CONFIG,
        machine_local: true,
    },
    RepoArtifact {
        path: crate::pi_project::EXTENSION,
        machine_local: true,
    },
];

/// The [`REPO_ARTIFACTS`] that name this machine's pixel binary.
fn machine_local_artifacts() -> Vec<&'static str> {
    REPO_ARTIFACTS
        .iter()
        .filter(|artifact| artifact.machine_local)
        .map(|artifact| artifact.path)
        .collect()
}

/// List the machine-local [`REPO_ARTIFACTS`] in the clone's `info/exclude`, so a
/// `git add -A` cannot publish a hook that points at this machine's binary.
fn exclude_project_artifacts(repo: &Path, dry_run: bool) -> Result<InstallStep> {
    let added = crate::repo_git::exclude_locally(repo, &machine_local_artifacts(), dry_run)?;
    Ok(InstallStep {
        id: "repo.git-exclude".into(),
        status: CheckStatus::Green,
        summary: dry_run_summary(
            dry_run,
            &format!(
                "{} machine-local path(s) added to the clone's info/exclude",
                added.len()
            ),
        ),
        detail: (!added.is_empty()).then(|| added.join(" ")),
    })
}

/// File name, under `~/.local/share/pixel/`, of the short prompt appended to
/// Claude Code sub-agents (`--append-subagent-system-prompt-file`, print mode
/// only). Kept apart from `agent-prompt.md` because a sub-agent gets neither
/// the session's `--append-system-prompt-file` nor its history, and because a
/// long prompt loses to a long agent body: this one stays under 2 KB.
pub(crate) const SUBAGENT_PROMPT_FILE: &str = "subagent-prompt.md";

/// The agent prompt as bundled in the binary.
pub(crate) const AGENT_PROMPT_ASSET: &str = include_str!("../assets/pixel-agent-prompt.md");

/// The sub-agent prompt as bundled in the binary.
pub(crate) const SUBAGENT_PROMPT_ASSET: &str = include_str!("../assets/pixel-subagent-prompt.md");

/// Pi's system-prompt file, relative to home. Pi reads it automatically — no
/// flag, no hook — and a user may already keep instructions in it, so pixel
/// owns only the managed block inside it.
pub(crate) const PI_PROMPT_REL: &str = ".pi/agent/APPEND_SYSTEM.md";

/// The prompt files a `pixel install` deployed under `home` that no longer
/// match the copies bundled in this binary, by file name. Claude's
/// SessionStart hook and the sub-agent flag read these files as they are, so
/// after an upgrade every agent keeps the old command map until the install
/// is rerun. A file that was never deployed is not listed: an install that
/// never happened is `pixel doctor`'s report, not an upgrade's.
pub fn stale_prompts(home: &Path) -> Vec<&'static str> {
    let dir = home.join(".local/share/pixel");
    [
        ("agent-prompt.md", AGENT_PROMPT_ASSET),
        (SUBAGENT_PROMPT_FILE, SUBAGENT_PROMPT_ASSET),
    ]
    .into_iter()
    .filter(|(name, asset)| {
        fs::read_to_string(dir.join(name)).is_ok_and(|deployed| deployed != *asset)
    })
    .map(|(name, _)| name)
    .collect()
}

/// Copy the bundled Pixel agent system prompt to `~/.local/share/pixel/agent-prompt.md`,
/// the sub-agent prompt to `~/.local/share/pixel/subagent-prompt.md`, and the
/// prompt into Pi's system-prompt file (pi reads it automatically, no flag needed).
/// The prompt instructs agents to use `pixel search-content`/`pixel find-code`/`pixel impact`
/// instead of `grep`/`rg` for code discovery in indexed repositories.
fn deploy_agent_prompt(home: &Path, dry_run: bool) -> Result<InstallStep> {
    let dest_dir = home.join(".local/share/pixel");
    let dest = dest_dir.join("agent-prompt.md");
    let subagent_dest = dest_dir.join(SUBAGENT_PROMPT_FILE);
    let pi_dest = home.join(PI_PROMPT_REL);
    if dry_run {
        return Ok(InstallStep {
            id: "agent-prompt".into(),
            status: CheckStatus::Green,
            summary: format!("would deploy agent-prompt.md and {SUBAGENT_PROMPT_FILE}"),
            detail: Some(format!(
                "dest={} subagent={} pi={}",
                dest.display(),
                subagent_dest.display(),
                pi_dest.display()
            )),
        });
    }
    fs::create_dir_all(&dest_dir)?;
    let needs_write = write_if_changed(&dest, AGENT_PROMPT_ASSET)?;
    let subagent_written = write_if_changed(&subagent_dest, SUBAGENT_PROMPT_ASSET)?;
    let pi_written = write_pi_prompt(&pi_dest)?;
    Ok(InstallStep {
        id: "agent-prompt".into(),
        status: CheckStatus::Green,
        summary: format!(
            "{} agent-prompt.md, {} {SUBAGENT_PROMPT_FILE}{}",
            if needs_write { "deployed" } else { "verified" },
            if subagent_written {
                "deployed"
            } else {
                "verified"
            },
            if pi_written {
                ", updated Pi's APPEND_SYSTEM.md"
            } else {
                ""
            }
        ),
        detail: Some(format!(
            "path={} subagent={} pi={}",
            dest.display(),
            subagent_dest.display(),
            pi_dest.display()
        )),
    })
}

/// Write `content` to `path` unless the file already holds exactly it.
/// Returns whether a write happened.
fn write_if_changed(path: &Path, content: &str) -> Result<bool> {
    let needs_write = match fs::read_to_string(path) {
        Ok(existing) => existing != content,
        Err(_) => true,
    };
    if needs_write {
        fs::write(path, content)?;
    }
    Ok(needs_write)
}

/// Put the bundled prompt inside the managed markers in Pi's system-prompt
/// file, backing the previous bytes up first. Pi reads that file
/// automatically and a user may already keep instructions in it: everything
/// outside the markers survives, and a failed write is an error rather than a
/// silently green step (the prompts under `~/.local/share/pixel/` are pixel's
/// own files; this one is not).
fn write_pi_prompt(path: &Path) -> Result<bool> {
    let existing = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };
    let wanted = managed_pi_content(&existing, AGENT_PROMPT_ASSET);
    if wanted == existing {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    write_atomically(path, &wanted)?;
    Ok(true)
}

/// The content Pi's system-prompt file should hold, given `existing`.
///
/// A copy of the prompt that is not inside the markers yet — what an install
/// before the markers wrote verbatim, or a hand copy — is wrapped in place
/// instead of appended a second time, and text the user keeps around it
/// survives. Anything else follows the Markdown agent-config rules
/// ([`config::apply_managed_markers`]).
fn managed_pi_content(existing: &str, asset: &str) -> String {
    if !existing.contains(config::MANAGED_BEGIN) && existing.contains(asset) {
        let begin = config::MANAGED_BEGIN;
        let end = config::MANAGED_END;
        let block = format!("{begin}\n{asset}\n{end}\n");
        return existing.replacen(asset, &block, 1);
    }
    config::apply_managed_markers(existing, asset)
}

/// Replace `path` with `content` in one step: the bytes already there are
/// backed up first, the new bytes go to a sibling temp file, and that file is
/// renamed over the target. A crash mid-write leaves the old profile intact
/// instead of a half-written one — a shell profile is read by every
/// interactive shell, and it is the user's file.
pub(crate) fn write_atomically(path: &Path, content: &str) -> Result<()> {
    config::backup_if_changing(path, content.as_bytes())?;
    let tmp = path.with_extension("pixel-tmp");
    fs::write(&tmp, content)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod pi_prompt_content_tests {
    use super::{AGENT_PROMPT_ASSET, managed_pi_content, write_pi_prompt};
    use crate::config::{MANAGED_BEGIN, MANAGED_END};

    #[test]
    fn a_prompt_file_written_by_an_older_install_is_wrapped_in_place_not_duplicated() {
        let wrapped = managed_pi_content(AGENT_PROMPT_ASSET, AGENT_PROMPT_ASSET);
        assert!(wrapped.starts_with(MANAGED_BEGIN), "{wrapped}");
        assert!(wrapped.trim_end().ends_with(MANAGED_END), "{wrapped}");
        assert_eq!(
            wrapped.matches(AGENT_PROMPT_ASSET).count(),
            1,
            "the prompt must appear once, not once outside the markers and once inside"
        );
    }

    #[test]
    fn user_text_around_a_stale_copy_is_kept() {
        let existing = format!("My own pi note.\n{AGENT_PROMPT_ASSET}");
        let wrapped = managed_pi_content(&existing, AGENT_PROMPT_ASSET);
        assert!(wrapped.starts_with("My own pi note.\n"), "{wrapped}");
        assert_eq!(wrapped.matches(AGENT_PROMPT_ASSET).count(), 1);
    }

    #[test]
    fn a_file_pixel_never_wrote_keeps_its_text_and_gets_one_block() {
        let existing = "answer in French.\n";
        let wanted = managed_pi_content(existing, AGENT_PROMPT_ASSET);
        assert!(wanted.starts_with(existing), "{wanted}");
        assert!(wanted.contains(MANAGED_BEGIN), "{wanted}");
        assert_eq!(
            wanted.matches(MANAGED_BEGIN).count(),
            1,
            "one block, not one per install"
        );
        assert_eq!(
            managed_pi_content(&wanted, AGENT_PROMPT_ASSET),
            wanted,
            "a managed file is left alone on the next install"
        );
    }

    #[cfg(unix)]
    #[test]
    fn write_pi_prompt_fails_when_path_is_unreadable() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("pixel-write-pi-{:x}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pi.md");
        fs::write(&path, b"user text").unwrap();
        // Remove read permission but keep write permission.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o200)).unwrap();

        // An unreadable file must be an error, never "no file".
        let result = write_pi_prompt(&path);
        assert!(
            result.is_err(),
            "expected error for unreadable path, got {result:?}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_whose_bytes_are_not_utf8_is_not_a_missing_file() {
        use std::fs;

        let dir =
            std::env::temp_dir().join(format!("pixel-write-pi-non-utf8-{:x}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pi.md");
        // The file is there but cannot be read as text: `read_to_string` fails
        // with InvalidData while the byte-wise backup and the atomic write
        // would both succeed, so "I could not read it" must not pass for "there
        // is no file" — that would replace bytes the user owns.
        let user_bytes = b"\xff\xfeanswer in French\n";
        fs::write(&path, user_bytes).unwrap();

        let result = write_pi_prompt(&path);

        assert!(
            result.is_err(),
            "a prompt pixel cannot read as text must be an error, got {result:?}"
        );
        assert_eq!(
            fs::read(&path).unwrap(),
            user_bytes.as_slice(),
            "the user's bytes must survive a failed read"
        );
        let leftovers: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("pixel-bak") || name.contains("pixel-tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "a failed read must leave no backup and no temp file: {leftovers:?}"
        );

        let _ = fs::remove_dir_all(&dir);
    }
}

/// Which shell dialect the managed wrapper block is written in.
///
/// fish is not a POSIX shell: `claude() { ...; }` is a syntax error there and
/// `$@` does not exist, so supporting it means generating a different block —
/// not just writing the same block somewhere else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellKind {
    /// bash, zsh, and anything else that accepts POSIX function syntax.
    Posix,
    /// fish.
    Fish,
}

impl ShellKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ShellKind::Posix => "posix",
            ShellKind::Fish => "fish",
        }
    }
}

/// File name of the fish drop-in. pixel creates this file, owns all of it, and
/// deletes it on uninstall.
pub(crate) const FISH_DROPIN: &str = "pixel.fish";

/// The shell whose profile is checked for legacy wrappers: the caller's
/// override, else the
/// account's login shell, else `$SHELL`.
///
/// The wrappers are a `claude` function a human runs from an interactive
/// shell, so the shell that matters is the login shell. `$SHELL` is not a
/// reliable witness of it: a coding agent's command tool (Claude Code's
/// runs under `/bin/zsh` on a fish machine), `env -i` or cron report their
/// own. The account database is asked first; `$SHELL` is the fallback when
/// it cannot be read, and the override is for the case where both are
/// wrong.
pub(crate) fn resolve_shell(shell_override: Option<&str>) -> String {
    resolve_shell_from(
        shell_override,
        account_login_shell(),
        std::env::var("SHELL").ok(),
    )
}

/// The resolution order behind [`resolve_shell`], with every source passed
/// in. An empty source counts as absent.
pub(crate) fn resolve_shell_from(
    shell_override: Option<&str>,
    account: Option<String>,
    env_shell: Option<String>,
) -> String {
    let present = |s: Option<String>| s.filter(|v| !v.trim().is_empty());
    shell_override
        .map(ToString::to_string)
        .or_else(|| present(account))
        .or_else(|| present(env_shell))
        .unwrap_or_default()
}

/// The login shell recorded for the current account: Directory Services on
/// macOS (`dscl . -read /Users/<user> UserShell`), the passwd database
/// elsewhere (`getent passwd <user>`, then `/etc/passwd`). `None` when the
/// user name is unknown or nothing answers.
#[cfg_attr(test, mutants::skip)] // process spawns and /etc reads over the tested parsers
fn account_login_shell() -> Option<String> {
    let user = std::env::var("USER")
        .ok()
        .filter(|u| !u.is_empty())
        .or_else(|| {
            let out = std::process::Command::new("id").arg("-un").output().ok()?;
            out.status
                .success()
                .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
                .filter(|u| !u.is_empty())
        })?;
    if cfg!(target_os = "macos") {
        let out = std::process::Command::new("dscl")
            .args([".", "-read", &format!("/Users/{user}"), "UserShell"])
            .output()
            .ok()?;
        return out
            .status
            .success()
            .then(|| parse_dscl_user_shell(&String::from_utf8_lossy(&out.stdout)))
            .flatten();
    }
    let getent = std::process::Command::new("getent")
        .args(["passwd", &user])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).into_owned());
    let passwd = match getent {
        Some(text) => text,
        None => fs::read_to_string("/etc/passwd").ok()?,
    };
    parse_passwd_shell(&passwd, &user)
}

/// The shell in `dscl` output: the value after `UserShell:`.
pub(crate) fn parse_dscl_user_shell(output: &str) -> Option<String> {
    output
        .lines()
        .find_map(|line| line.strip_prefix("UserShell:"))
        .map(str::trim)
        .filter(|shell| !shell.is_empty())
        .map(ToString::to_string)
}

/// The shell of `user` in passwd text: the seventh field of the line whose
/// first field is exactly `user`.
pub(crate) fn parse_passwd_shell(passwd: &str, user: &str) -> Option<String> {
    passwd
        .lines()
        .map(|line| line.split(':').collect::<Vec<_>>())
        .find(|fields| fields.first() == Some(&user))
        .and_then(|fields| fields.get(6).map(|s| s.trim().to_string()))
        .filter(|shell| !shell.is_empty())
}

#[cfg(test)]
mod shell_resolution_tests {
    use super::{parse_dscl_user_shell, parse_passwd_shell, resolve_shell_from};

    /// The order is override, account, `$SHELL`: an agent's tool shell in
    /// `$SHELL` must lose to the account's login shell, and an empty value
    /// at any level must not shadow the next one.
    #[test]
    fn override_beats_account_beats_env_and_empty_values_are_skipped() {
        let fish = || Some("/opt/homebrew/bin/fish".to_string());
        let zsh = || Some("/bin/zsh".to_string());
        assert_eq!(resolve_shell_from(Some("bash"), fish(), zsh()), "bash");
        assert_eq!(
            resolve_shell_from(None, fish(), zsh()),
            "/opt/homebrew/bin/fish"
        );
        assert_eq!(resolve_shell_from(None, None, zsh()), "/bin/zsh");
        assert_eq!(
            resolve_shell_from(None, Some(" ".into()), zsh()),
            "/bin/zsh"
        );
        assert_eq!(resolve_shell_from(None, None, Some(String::new())), "");
        assert_eq!(resolve_shell_from(None, None, None), "");
    }

    #[test]
    fn dscl_output_yields_the_user_shell_line_only() {
        assert_eq!(
            parse_dscl_user_shell("UserShell: /opt/homebrew/bin/fish\n"),
            Some("/opt/homebrew/bin/fish".to_string())
        );
        assert_eq!(
            parse_dscl_user_shell(
                "RecordName: navid\nUserShell:\t/bin/zsh\nNFSHomeDirectory: /Users/navid\n"
            ),
            Some("/bin/zsh".to_string())
        );
        assert_eq!(parse_dscl_user_shell("UserShell:\n"), None);
        assert_eq!(parse_dscl_user_shell("No such key: UserShell\n"), None);
        assert_eq!(parse_dscl_user_shell(""), None);
    }

    #[test]
    fn passwd_text_yields_the_seventh_field_of_the_exact_user() {
        let passwd = "root:x:0:0:root:/root:/bin/bash\n\
                      navid:x:501:20:Navid:/home/navid:/usr/bin/fish\n\
                      navidx:x:502:20::/home/navidx:/bin/sh\n";
        assert_eq!(
            parse_passwd_shell(passwd, "navid"),
            Some("/usr/bin/fish".to_string())
        );
        assert_eq!(
            parse_passwd_shell(passwd, "navidx"),
            Some("/bin/sh".to_string()),
            "exact name, not prefix"
        );
        assert_eq!(parse_passwd_shell(passwd, "nobody"), None);
        assert_eq!(parse_passwd_shell("short:x:1:1\n", "short"), None);
        assert_eq!(
            parse_passwd_shell("empty:x:1:1::/home/empty:\n", "empty"),
            None
        );
    }
}

/// Classify a `$SHELL` value by its executable name. Matching the file name
/// rather than a substring of the whole path keeps a path like
/// `/home/fisher/bin/zsh` out of the fish branch.
pub(crate) fn shell_kind_from(shell: &str) -> ShellKind {
    let name = shell.rsplit('/').next().unwrap_or(shell);
    if name.eq_ignore_ascii_case("fish") {
        ShellKind::Fish
    } else {
        ShellKind::Posix
    }
}

/// fish's config root: `$XDG_CONFIG_HOME/fish` when that variable points inside
/// the home being installed into, otherwise fish's default `~/.config/fish`.
/// The containment test keeps an ambient `XDG_CONFIG_HOME` from redirecting an
/// install that was explicitly aimed at another home (`--home`, tests).
fn fish_config_dir(home: &Path) -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        let dir = PathBuf::from(&xdg);
        if !xdg.is_empty() && dir.starts_with(home) {
            return dir.join("fish");
        }
    }
    home.join(".config").join("fish")
}

/// Where the managed block lives for `shell`, and which dialect it must be
/// written in: `~/.bashrc` for bash, `~/.config/fish/conf.d/pixel.fish` for
/// fish, `~/.zshrc` otherwise.
///
/// fish reads neither `~/.zshrc` nor `~/.bashrc`. Its `conf.d/` directory is
/// sourced automatically for every session, so the block gets its own file
/// there rather than being appended to the user's `config.fish`.
pub(crate) fn shell_profile_for(shell: &str, home: &Path) -> (ShellKind, PathBuf) {
    match shell_kind_from(shell) {
        ShellKind::Fish => (
            ShellKind::Fish,
            fish_config_dir(home).join("conf.d").join(FISH_DROPIN),
        ),
        ShellKind::Posix
            if shell
                .rsplit('/')
                .next()
                .is_some_and(|name| name.eq_ignore_ascii_case("bash")) =>
        {
            (ShellKind::Posix, home.join(".bashrc"))
        }
        ShellKind::Posix => (ShellKind::Posix, home.join(".zshrc")),
    }
}

/// The profiles of the other shells `pixel install` knows, with their
/// shell name, that hold a pixel-managed block: the residue of an install
/// that targeted the wrong shell (an agent's `$SHELL` on a fish machine).
/// `resolved_profile` is the one the current shell loads and is skipped.
pub(crate) fn stray_wrapper_profiles(
    home: &Path,
    resolved_profile: &Path,
) -> Vec<(&'static str, PathBuf)> {
    let candidates = [
        ("zsh", home.join(".zshrc")),
        ("bash", home.join(".bashrc")),
        (
            "fish",
            fish_config_dir(home).join("conf.d").join(FISH_DROPIN),
        ),
    ];
    candidates
        .into_iter()
        .filter(|(_, profile)| profile != resolved_profile)
        .filter(|(_, profile)| {
            fs::read_to_string(profile).is_ok_and(|text| extract_managed_block(&text).is_some())
        })
        .collect()
}

pub(crate) const PIXEL_MANAGED_BEGIN: &str = "# >>> pixel-managed >>>";
pub(crate) const PIXEL_MANAGED_END: &str = "# <<< pixel-managed <<<";

/// Return the pixel-managed block found in `content`, markers included.
pub(crate) fn extract_managed_block(content: &str) -> Option<String> {
    let mut out: Vec<&str> = Vec::new();
    let mut in_block = false;
    for line in content.lines() {
        if line.trim_start().starts_with(PIXEL_MANAGED_BEGIN) {
            in_block = true;
        }
        if in_block {
            out.push(line.trim_end());
            if line.trim_start().starts_with(PIXEL_MANAGED_END) {
                return Some(out.join("\n"));
            }
        }
    }
    None
}

/// Strip an existing pixel-managed block from a file's content.
///
/// An unterminated block is refused instead of swallowing the rest of the
/// file: the caller reports it and writes nothing. "Everything from the begin
/// marker to EOF is ours" is what used to delete the end of a user's profile.
fn strip_shell_wrappers(content: &str) -> Result<String> {
    let begin = PIXEL_MANAGED_BEGIN;
    let end = PIXEL_MANAGED_END;
    let mut out = String::new();
    let mut skipping = false;
    for line in content.lines() {
        if line.trim_start().starts_with(begin) {
            skipping = true;
            continue;
        }
        if skipping && line.trim_start().starts_with(end) {
            skipping = false;
            continue;
        }
        if !skipping {
            out.push_str(line);
            out.push('\n');
        }
    }
    if skipping {
        return Err(InstallError::UnterminatedManagedBlock);
    }
    // Remove trailing blank lines left by the stripped block.
    while out.ends_with("\n\n") {
        out.pop();
    }
    Ok(out)
}

#[cfg(test)]
mod shell_wrapper_strip_tests {
    use super::{PIXEL_MANAGED_BEGIN, PIXEL_MANAGED_END, strip_shell_wrappers};
    use crate::InstallError;

    #[test]
    fn a_terminated_block_is_stripped_and_the_surrounding_lines_kept() {
        let profile = format!(
            "alias first='one'\n{PIXEL_MANAGED_BEGIN}\nclaude() {{ :; }}\n{PIXEL_MANAGED_END}\nalias last='two'\n"
        );
        let cleaned = strip_shell_wrappers(&profile).expect("a closed block is strippable");
        assert_eq!(cleaned, "alias first='one'\nalias last='two'\n");
        assert!(
            !cleaned.contains(PIXEL_MANAGED_BEGIN),
            "the block must be gone"
        );
    }

    #[test]
    fn a_begin_without_an_end_is_refused_not_stripped_to_eof() {
        let broken = format!("{PIXEL_MANAGED_BEGIN}\nalias keep='me'\n");
        assert!(
            matches!(
                strip_shell_wrappers(&broken),
                Err(InstallError::UnterminatedManagedBlock)
            ),
            "an unterminated block must be refused: the lines after it are the user's"
        );
    }

    #[test]
    fn a_stray_end_marker_without_a_begin_is_kept_as_user_content() {
        let profile = format!("alias mine='kept'\n{PIXEL_MANAGED_END}\n");
        assert_eq!(strip_shell_wrappers(&profile).unwrap(), profile);
    }
}

/// Remove the legacy `claude()` shell-wrapper block — from the resolved
/// shell's profile AND from any other candidate profile an earlier install
/// wrote to. The wrapper only ever fired for human login shells; now that
/// the SessionStart hook injects the prompt into every Claude process, a
/// surviving wrapper double-injects on every wrapped launch.
fn remove_legacy_wrappers(
    home: &Path,
    shell_override: Option<&str>,
    dry_run: bool,
) -> Result<InstallStep> {
    let shell = resolve_shell(shell_override);
    let (_, resolved) = shell_profile_for(&shell, home);
    let mut candidates: Vec<(ShellKind, PathBuf)> =
        vec![(shell_kind_from(&shell), resolved.clone())];
    for (_, path) in stray_wrapper_profiles(home, &resolved) {
        let kind = if path.file_name().is_some_and(|n| n == FISH_DROPIN) {
            ShellKind::Fish
        } else {
            ShellKind::Posix
        };
        candidates.push((kind, path));
    }
    let mut removed: Vec<String> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    for (kind, profile) in &candidates {
        let Ok(existing) = fs::read_to_string(profile) else {
            continue;
        };
        let cleaned = match strip_shell_wrappers(&existing) {
            Ok(cleaned) => cleaned,
            Err(e) => {
                return Ok(InstallStep {
                    id: "shell-wrappers".into(),
                    status: CheckStatus::Red,
                    summary: format!("{} in {} — profile not touched", e, profile.display()),
                    detail: Some(format!("profile={}", profile.display())),
                });
            }
        };
        if cleaned == existing {
            skipped.push(profile.display().to_string());
            continue;
        }
        removed.push(profile.display().to_string());
        if dry_run {
            continue;
        }
        if *kind == ShellKind::Fish && cleaned.trim().is_empty() {
            let _ = config::backup_if_changing(profile, cleaned.as_bytes())?;
            fs::remove_file(profile)?;
        } else {
            write_atomically(profile, &cleaned)?;
        }
    }
    Ok(InstallStep {
        id: "shell-wrappers".into(),
        status: CheckStatus::Green,
        summary: dry_run_summary(
            dry_run,
            &if removed.is_empty() {
                "no legacy shell wrappers found — nothing to remove".to_string()
            } else {
                format!("removed legacy shell wrappers from {}", removed.join(", "))
            },
        ),
        detail: Some(format!("removed={removed:?} clean={skipped:?}")),
    })
}

/// Remove the pixel-managed shell wrapper block from the user's shell profile.
pub(crate) fn remove_shell_wrappers(
    home: &Path,
    shell_override: Option<&str>,
    dry_run: bool,
) -> Result<InstallStep> {
    let shell = resolve_shell(shell_override);
    let (kind, profile) = shell_profile_for(&shell, home);
    let detail = Some(format!(
        "profile={} shell={}",
        profile.display(),
        kind.as_str()
    ));
    let existing = match fs::read_to_string(&profile) {
        Ok(s) => s,
        Err(_) => {
            return Ok(InstallStep {
                id: "shell-wrappers".into(),
                status: CheckStatus::Green,
                summary: dry_run_summary(dry_run, "no shell profile — skipping"),
                detail,
            });
        }
    };
    let cleaned = match strip_shell_wrappers(&existing) {
        Ok(cleaned) => cleaned,
        Err(e) => {
            return Ok(InstallStep {
                id: "shell-wrappers".into(),
                status: CheckStatus::Red,
                summary: format!("{e} — profile not touched"),
                detail,
            });
        }
    };
    if cleaned == existing {
        return Ok(InstallStep {
            id: "shell-wrappers".into(),
            status: CheckStatus::Green,
            summary: dry_run_summary(dry_run, "no shell wrappers found — skipping"),
            detail,
        });
    }
    if !dry_run {
        // The fish drop-in is a file pixel created and owns end to end: once
        // the block is stripped there is nothing left in it worth keeping. A
        // user's own .zshrc/.bashrc is only ever edited in place.
        if kind == ShellKind::Fish && cleaned.trim().is_empty() {
            let _ = config::backup_if_changing(&profile, cleaned.as_bytes())?;
            fs::remove_file(&profile)?;
        } else {
            write_atomically(&profile, &cleaned)?;
        }
    }
    Ok(InstallStep {
        id: "shell-wrappers".into(),
        status: CheckStatus::Green,
        summary: dry_run_summary(dry_run, "removed shell wrappers"),
        detail,
    })
}

pub(crate) fn read_settings(path: &Path) -> Result<serde_json::Value> {
    match fs::read_to_string(path) {
        Ok(s) => Ok(serde_json::from_str(&s)?),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(serde_json::json!({})),
        Err(e) => Err(e.into()),
    }
}

/// Serialize and write `value` to `path`, backing up any pre-existing,
/// content-differing file first. In dry-run mode, performs no write, no
/// backup, and no directory creation, and always returns `Ok(None)`.
pub(crate) fn write_settings(
    path: &Path,
    value: &serde_json::Value,
    dry_run: bool,
) -> Result<Option<PathBuf>> {
    let serialized = format!("{}\n", serde_json::to_string_pretty(value)?);
    if dry_run {
        return Ok(None);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let backup_path = config::backup_if_changing(path, serialized.as_bytes())?;
    fs::write(path, serialized)?;
    Ok(backup_path)
}

pub(crate) fn dry_run_summary(dry_run: bool, summary: &str) -> String {
    if dry_run {
        format!("[dry-run] would report: {summary}")
    } else {
        summary.to_string()
    }
}

pub(crate) fn with_backup_note(detail: String, backup_path: Option<PathBuf>) -> String {
    match backup_path {
        Some(p) => format!("{detail} (backup={})", p.display()),
        None => detail,
    }
}

// ---------------------------------------------------------------------------
// rollout / migration (clean cut, no shims)
// ---------------------------------------------------------------------------

/// Outcome of a `pixel migrate` run.
#[derive(Debug, Clone, Serialize)]
pub struct MigrateReport {
    pub version: String,
    pub ok: bool,
    pub repo_root: String,
    /// True if a `.gitpixel/` directory was found and deleted.
    pub old_state_removed: bool,
    /// Compatibility field: false because this command does not rebuild indexes.
    pub new_state_rebuilt: bool,
    /// True once `.pixel/` exists; existing contents are preserved.
    pub new_state_directory_prepared: bool,
}

/// Remove legacy `.gitpixel/` state and prepare the current `.pixel/` directory.
///
/// Existing `.pixel/` contents are preserved. Missing indexes are built lazily
/// on first use, not by this command. (The old gain-ledger
/// carry-over was removed together with the gain module: an unmeasured
/// token-savings ledger was exactly the kind of claim-without-measurement
/// the doctrine now forbids.)
pub fn migrate(repo_root: &Path) -> Result<MigrateReport> {
    let old_dir = repo_root.join(".gitpixel");
    let new_dir = repo_root.join(".pixel");

    // Delete the old state directory.
    let old_state_removed = if old_dir.exists() {
        fs::remove_dir_all(&old_dir)?;
        true
    } else {
        false
    };

    // Preparing a directory is not proof that any index has been rebuilt.
    fs::create_dir_all(&new_dir)?;

    Ok(MigrateReport {
        version: "v1".into(),
        ok: true,
        repo_root: repo_root.display().to_string(),
        old_state_removed,
        new_state_rebuilt: false,
        new_state_directory_prepared: true,
    })
}

#[cfg(test)]
mod stale_prompt_tests {
    use super::*;

    fn deploy(home: &Path, name: &str, content: &str) {
        let dir = home.join(".local/share/pixel");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(name), content).unwrap();
    }

    #[test]
    fn nothing_deployed_is_not_stale() {
        let home = tempfile::tempdir().unwrap();
        assert!(stale_prompts(home.path()).is_empty());
    }

    #[test]
    fn prompts_matching_this_binary_are_not_stale() {
        let home = tempfile::tempdir().unwrap();
        deploy(home.path(), "agent-prompt.md", AGENT_PROMPT_ASSET);
        deploy(home.path(), SUBAGENT_PROMPT_FILE, SUBAGENT_PROMPT_ASSET);
        assert!(stale_prompts(home.path()).is_empty());
    }

    /// An older release's prompt, the case an upgrade leaves behind: each
    /// file is judged on its own, against its own bundled copy.
    #[test]
    fn a_prompt_from_another_release_is_named() {
        let home = tempfile::tempdir().unwrap();
        deploy(home.path(), "agent-prompt.md", "# Pixel, an older prompt\n");
        deploy(home.path(), SUBAGENT_PROMPT_FILE, SUBAGENT_PROMPT_ASSET);
        assert_eq!(stale_prompts(home.path()), vec!["agent-prompt.md"]);
        deploy(home.path(), SUBAGENT_PROMPT_FILE, AGENT_PROMPT_ASSET);
        assert_eq!(
            stale_prompts(home.path()),
            vec!["agent-prompt.md", SUBAGENT_PROMPT_FILE]
        );
    }
}

#[cfg(test)]
mod migration_tests {
    use super::*;

    #[test]
    fn migrate_preserves_current_state_and_reports_no_rebuild() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path();
        fs::create_dir_all(root.join(".pixel")).unwrap();
        fs::create_dir_all(root.join(".gitpixel")).unwrap();
        let state = root.join(".pixel/user-state.json");
        fs::write(&state, b"{\"preserve\":true}").unwrap();
        let first = migrate(root).unwrap();
        assert!(first.old_state_removed);
        assert!(first.new_state_directory_prepared);
        assert!(
            !first.new_state_rebuilt,
            "preparing a directory is not rebuilding an index"
        );
        assert_eq!(fs::read(&state).unwrap(), b"{\"preserve\":true}");
        let second = migrate(root).unwrap();
        assert!(!second.old_state_removed);
        assert!(second.new_state_directory_prepared);
        assert!(!second.new_state_rebuilt);
        assert_eq!(fs::read(&state).unwrap(), b"{\"preserve\":true}");
    }
}

#[cfg(test)]
mod path_tests {
    use super::{find_in_paths, find_on_path};

    #[test]
    fn find_in_paths_returns_the_first_executable_file_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("pixel-find-path-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let first = dir.join("first");
        let second = dir.join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        // `tool` is a plain file in `first` and an executable in `second`.
        std::fs::write(first.join("tool"), b"#!/bin/sh\n").unwrap();
        std::fs::write(second.join("tool"), b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(second.join("tool"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        std::fs::create_dir_all(first.join("dirtool")).unwrap();
        let path = std::env::join_paths([&first, &second]).unwrap();
        assert_eq!(find_in_paths("tool", &path), Some(second.join("tool")));
        assert_eq!(
            find_in_paths("dirtool", &path),
            None,
            "a directory is not a binary"
        );
        assert_eq!(find_in_paths("absent", &path), None);
        let only_first = std::env::join_paths([&first]).unwrap();
        assert_eq!(find_in_paths("tool", &only_first), None, "not executable");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn find_on_path_reads_the_process_path() {
        let sh = find_on_path("sh").expect("sh is on every unix PATH");
        assert!(sh.ends_with("sh"), "{}", sh.display());
        assert!(sh.is_absolute(), "{}", sh.display());
        assert_eq!(find_on_path("pixel-definitely-not-installed-xyz"), None);
    }
}

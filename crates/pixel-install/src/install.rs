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
    /// If true, compute and report every step's outcome exactly as a real
    /// run would, but perform no filesystem writes: no settings.json edits,
    /// no hook files, no agent-config rewrites, no backups, no directory
    /// creation. Safe to run against a real `$HOME` to preview an install.
    pub dry_run: bool,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
struct InstalledAgents {
    claude: bool,
}

#[allow(dead_code)]
fn command_available(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        let candidate = dir.join(name);
        if !candidate.is_file() {
            return false;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            candidate
                .metadata()
                .map(|meta| meta.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        }
        #[cfg(not(unix))]
        {
            true
        }
    })
}

#[allow(dead_code)]
fn installed_agents(home: &Path) -> InstalledAgents {
    InstalledAgents {
        claude: command_available("claude")
            || home.join(".claude/settings.json").is_file()
            || home.join(config::CLAUDE_HOOKS_DIR).is_dir(),
    }
}

#[allow(dead_code)]
pub(crate) fn claude_installed(home: &Path) -> bool {
    installed_agents(home).claude
}

/// Run `pixel install`. Idempotent: safe to re-run.
///
/// The install is deliberately minimal: it deploys the agent system prompt
/// and sets up shell wrappers so every `claude`/`codex` invocation includes
/// the Pixel retrieval protocol. No hooks, no managed blocks in CLAUDE.md/
/// AGENTS.md, no provider-specific routing — the system prompt is the single
/// enforcement mechanism.
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
    let steps = vec![
        deploy_agent_prompt(&home, dry_run)?,
        install_shell_wrappers(&home, dry_run)?,
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

/// Copy the bundled Pixel agent system prompt to `~/.local/share/pixel/agent-prompt.md`
/// and to `~/.pi/agent/APPEND_SYSTEM.md` (Pi reads this automatically, no flag needed).
/// The prompt instructs agents to use `pixel search`/`pixel resolve`/`pixel impact`
/// instead of `grep`/`rg` for code discovery in indexed repositories.
fn deploy_agent_prompt(home: &Path, dry_run: bool) -> Result<InstallStep> {
    let dest_dir = home.join(".local/share/pixel");
    let dest = dest_dir.join("agent-prompt.md");
    let pi_dest = home.join(".pi/agent/APPEND_SYSTEM.md");
    if dry_run {
        return Ok(InstallStep {
            id: "agent-prompt".into(),
            status: CheckStatus::Green,
            summary: "would deploy agent-prompt.md".into(),
            detail: Some(format!("dest={}", dest.display())),
        });
    }
    fs::create_dir_all(&dest_dir)?;
    // The asset is embedded at compile time so the installed binary is self-contained.
    const ASSET: &str = include_str!("../assets/pixel-agent-prompt.md");
    let needs_write = match fs::read_to_string(&dest) {
        Ok(existing) => existing != ASSET,
        Err(_) => true,
    };
    if needs_write {
        fs::write(&dest, ASSET)?;
    }
    // Deploy to Pi's APPEND_SYSTEM.md so pi reads it automatically.
    if let Some(pi_parent) = pi_dest.parent() {
        let _ = fs::create_dir_all(pi_parent);
        let _ = fs::write(&pi_dest, ASSET);
    }
    Ok(InstallStep {
        id: "agent-prompt".into(),
        status: CheckStatus::Green,
        summary: format!(
            "{} agent-prompt.md",
            if needs_write { "deployed" } else { "verified" }
        ),
        detail: Some(format!("path={} pi={}", dest.display(), pi_dest.display())),
    })
}

/// Detect the user's shell profile path (~/.zshrc on macOS, ~/.bashrc on Linux).
pub(crate) fn shell_profile_path(home: &Path) -> PathBuf {
    let shell = std::env::var("SHELL").unwrap_or_default();
    if shell.contains("bash") {
        home.join(".bashrc")
    } else {
        home.join(".zshrc")
    }
}

pub(crate) const PIXEL_MANAGED_BEGIN: &str = "# >>> pixel-managed >>>";
pub(crate) const PIXEL_MANAGED_END: &str = "# <<< pixel-managed <<<";

/// Build the managed shell-wrapper block. Uses shell functions (not aliases)
/// because functions handle subcommands correctly (`codex exec ...` works).
fn shell_wrapper_block(prompt_path: &str) -> String {
    format!(
        "{begin}\n\
         # Pixel agent system prompt — added by `pixel install`\n\
         # Remove with `pixel uninstall`\n\
         claude() {{ command claude --append-system-prompt-file \"{prompt}\" \"$@\"; }}\n\
         codex() {{ command codex -c \"model_instructions_file=\\\"{prompt}\\\"\" \"$@\"; }}\n\
         {end}",
        begin = PIXEL_MANAGED_BEGIN,
        end = PIXEL_MANAGED_END,
        prompt = prompt_path,
    )
}

/// Strip an existing pixel-managed block from a file's content.
fn strip_shell_wrappers(content: &str) -> String {
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
    // Remove trailing blank lines left by the stripped block.
    while out.ends_with("\n\n") {
        out.pop();
    }
    out
}

/// Install shell wrappers for `claude` and `codex` in the user's shell profile
/// so every invocation automatically includes the Pixel system prompt.
fn install_shell_wrappers(home: &Path, dry_run: bool) -> Result<InstallStep> {
    let profile = shell_profile_path(home);
    let prompt_path = "$HOME/.local/share/pixel/agent-prompt.md";
    let block = shell_wrapper_block(prompt_path);
    if dry_run {
        return Ok(InstallStep {
            id: "shell-wrappers".into(),
            status: CheckStatus::Green,
            summary: format!("would write shell wrappers to {}", profile.display()),
            detail: Some(format!("profile={}", profile.display())),
        });
    }
    let existing = fs::read_to_string(&profile).unwrap_or_default();
    let cleaned = strip_shell_wrappers(&existing);
    let had_old_block = cleaned != existing;
    let mut new_content = cleaned;
    if !new_content.ends_with('\n') && !new_content.is_empty() {
        new_content.push('\n');
    }
    if !new_content.is_empty() {
        new_content.push('\n');
    }
    new_content.push_str(&block);
    new_content.push('\n');
    // Always write — the block may need refreshing even if old content was clean.
    fs::write(&profile, &new_content)?;
    let _ = had_old_block; // tracked for summary accuracy
    Ok(InstallStep {
        id: "shell-wrappers".into(),
        status: CheckStatus::Green,
        summary: format!(
            "{} shell wrappers in {}",
            if had_old_block { "updated" } else { "installed" },
            profile.display()
        ),
        detail: Some(format!("profile={}", profile.display())),
    })
}

/// Remove the pixel-managed shell wrapper block from the user's shell profile.
pub(crate) fn remove_shell_wrappers(home: &Path, dry_run: bool) -> Result<InstallStep> {
    let profile = shell_profile_path(home);
    let existing = match fs::read_to_string(&profile) {
        Ok(s) => s,
        Err(_) => {
            return Ok(InstallStep {
                id: "shell-wrappers".into(),
                status: CheckStatus::Green,
                summary: dry_run_summary(dry_run, "no shell profile — skipping"),
                detail: Some(format!("profile={}", profile.display())),
            });
        }
    };
    let cleaned = strip_shell_wrappers(&existing);
    if cleaned == existing {
        return Ok(InstallStep {
            id: "shell-wrappers".into(),
            status: CheckStatus::Green,
            summary: dry_run_summary(dry_run, "no shell wrappers found — skipping"),
            detail: Some(format!("profile={}", profile.display())),
        });
    }
    if !dry_run {
        fs::write(&profile, &cleaned)?;
    }
    Ok(InstallStep {
        id: "shell-wrappers".into(),
        status: CheckStatus::Green,
        summary: dry_run_summary(dry_run, "removed shell wrappers"),
        detail: Some(format!("profile={}", profile.display())),
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
    /// True if `.pixel/` was rebuilt fresh.
    pub new_state_rebuilt: bool,
}

/// Migrate a repo from the old `.gitpixel/` state to a fresh `.pixel/` state.
///
/// Deletes `.gitpixel/` and rebuilds `.pixel/` fresh. No state migration —
/// every index is a cache and is rebuilt on first use. (The old gain-ledger
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

    // Rebuild `.pixel/` fresh (the index/graph/facts are caches; the daemon
    // and CLI rebuild them on first use).
    fs::create_dir_all(&new_dir)?;
    let new_state_rebuilt = true;

    Ok(MigrateReport {
        version: "v1".into(),
        ok: true,
        repo_root: repo_root.display().to_string(),
        old_state_removed,
        new_state_rebuilt,
    })
}

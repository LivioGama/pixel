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
    // 1. Clean up any remnants from previous hook-based installs.
    // 2. Deploy the Pixel agent system prompt.
    // 3. Install shell wrappers so claude/codex always use the system prompt.
    let steps = vec![
        scrub_deprecated(&home, dry_run)?,
        remove_existing_guard_hooks(&home, dry_run)?,
        strip_old_managed_blocks(&home, dry_run)?,
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

/// Strip old pixel-managed blocks from all agent-config files (CLAUDE.md,
/// AGENTS.md, etc.). The system prompt replaces these — managed blocks are
/// no longer written by install, but old ones from previous installs must
/// be cleaned up.
fn strip_old_managed_blocks(home: &Path, dry_run: bool) -> Result<InstallStep> {
    let targets = config::find_agent_configs(home);
    let mut stripped = 0usize;
    for path in &targets {
        let original = match fs::read_to_string(path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let cleaned = config::strip_managed_block(&original);
        if cleaned != original {
            stripped += 1;
            if !dry_run {
                fs::write(path, &cleaned)?;
            }
        }
    }
    Ok(InstallStep {
        id: "strip-old-blocks".into(),
        status: CheckStatus::Green,
        summary: format!(
            "{} {} agent-config managed block(s)",
            if dry_run { "would strip" } else { "stripped" },
            stripped
        ),
        detail: Some(format!("files_checked={}", targets.len())),
    })
}

fn scrub_deprecated(home: &Path, dry_run: bool) -> Result<InstallStep> {
    let settings = home.join(".claude").join("settings.json");
    // The deprecated usable-git/gitpixel/sniper MCP server entries are
    // retired unconditionally — pixel replaces them via Bash + the guard
    // hook, not via MCP. The guard-hook command rewrite is unrelated to MCP
    // registration and always proceeds.
    let outcome = config::scrub_settings_json(&settings, dry_run)?;
    // Claude Code keeps GLOBAL MCP registrations in ~/.claude.json (top-level
    // `mcpServers`), not in ~/.claude/settings.json — a retired server
    // registered there survived every install scrub and kept failing to
    // connect at session start. Same file shape, so the same scrubber runs
    // on both.
    let global_config = home.join(".claude.json");
    let global_outcome = config::scrub_settings_json(&global_config, dry_run)?;
    // Retired-tool RULE files are scrubbed alongside the MCP entries. An
    // MCP registration and a Markdown rule are two different ways to keep
    // offering a retired tool; removing only the first leaves Devin,
    // Cline, and Cursor still advertising `usable-git`/`gitpixel` to the
    // model as rules it may load.
    let rule_files = config::scrub_deprecated_rule_files(home, dry_run)?;
    let removed = outcome.mcp_servers_removed
        + global_outcome.mcp_servers_removed
        + outcome.guard_hooks_removed
        + rule_files.len();
    let summary = format!(
        "removed {removed} deprecated MCP/hook/rule entr{}",
        if removed == 1 { "y" } else { "ies" }
    );
    let mut detail = format!(
        "mcp_servers_removed={} global_mcp_servers_removed={} guard_hooks_removed={} rule_files_removed={}",
        outcome.mcp_servers_removed,
        global_outcome.mcp_servers_removed,
        outcome.guard_hooks_removed,
        rule_files.len()
    );
    if !rule_files.is_empty() {
        detail.push_str(&format!(
            " rule_files=[{}]",
            rule_files
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(",")
        ));
    }
    Ok(InstallStep {
        id: "mcp.deprecated".into(),
        status: CheckStatus::Green,
        summary: dry_run_summary(dry_run, &summary),
        detail: Some(with_backup_note(detail, outcome.backup_path)),
    })
}

fn remove_existing_guard_hooks(home: &Path, dry_run: bool) -> Result<InstallStep> {
    let mut changed_files = 0usize;
    let mut changed_events = 0usize;
    let mut removed_scripts = 0usize;

    let nested_files = [
        home.join(".claude/settings.json"),
        home.join(config::DEVIN_CONFIG_DIR)
            .join(config::DEVIN_CONFIG_FILE),
        home.join(config::CODEX_HOOKS_FILE),
        home.join(config::GEMINI_SETTINGS_FILE),
    ];
    for path in nested_files {
        let changed = remove_guard_from_settings_file(&path, &["hooks"], false, dry_run)?;
        if changed {
            changed_files += 1;
            changed_events += 1;
        }
    }

    let zcode = home.join(config::ZCODE_CONFIG_FILE);
    if remove_guard_from_settings_file(&zcode, &["hooks", "events"], false, dry_run)? {
        changed_files += 1;
        changed_events += 1;
    }

    let cursor = home.join(config::CURSOR_HOOKS_FILE);
    if remove_guard_from_settings_file(&cursor, &["hooks"], true, dry_run)? {
        changed_files += 1;
        changed_events += 1;
    }

    for root in project_hook_search_roots(home) {
        let path = root.join(".codex/hooks.json");
        if remove_guard_from_settings_file(&path, &["hooks"], false, dry_run)? {
            changed_files += 1;
            changed_events += 1;
        }
    }

    // The old hook scripts are Pixel-owned and are no longer referenced by
    // any supported integration — the system prompt replaces all hooks.
    // Remove them as part of the migration so doctor reports a clean install.
    for name in [
        config::GUARD_HOOK,
        config::OLD_GUARD_HOOK,
        config::SESSION_START_HOOK,
        config::PROMPT_SUBMIT_HOOK,
        config::POST_COMPACTION_HOOK,
    ] {
        let path = home.join(config::CLAUDE_HOOKS_DIR).join(name);
        if path.is_file() {
            removed_scripts += 1;
            if !dry_run {
                fs::remove_file(path)?;
            }
        }
    }

    let summary = if changed_files == 0 {
        "rewire-first mode: no Pixel blocking guard entries found".to_string()
    } else {
        format!(
            "rewire-first mode: removed Pixel blocking guard from {changed_files} config file(s)"
        )
    };
    Ok(InstallStep {
        id: "hook.guard.cleanup".into(),
        status: CheckStatus::Green,
        summary: dry_run_summary(dry_run, &summary),
        detail: Some(format!(
            "changed_event_groups={changed_events} removed_scripts={removed_scripts}"
        )),
    })
}

fn remove_guard_from_settings_file(
    path: &Path,
    hooks_path: &[&str],
    flat_schema: bool,
    dry_run: bool,
) -> Result<bool> {
    if !path.is_file() {
        return Ok(false);
    }
    let mut value = read_settings(path)?;
    let changed = {
        let Some(mut current) = value.as_object_mut() else {
            return Err(InstallError::Config(config::ConfigError::InvalidSettings {
                path: path.to_path_buf(),
                reason: "settings root is not an object".into(),
            }));
        };
        for key in hooks_path {
            let Some(next) = current
                .get_mut(*key)
                .and_then(serde_json::Value::as_object_mut)
            else {
                return Ok(false);
            };
            current = next;
        }
        if flat_schema {
            config::remove_flat_guard_hook_entries(current) > 0
        } else {
            config::remove_guard_hook_entries(current) > 0
        }
    };
    if changed && !dry_run {
        write_settings(path, &value, false)?;
    }
    Ok(changed)
}

/// Copy the bundled Pixel agent system prompt to `~/.local/share/pixel/agent-prompt.md`.
/// This file is injected into Claude workers via `--append-system-prompt-file` and
/// can be used with Codex via `model_instructions_file`. The prompt instructs
/// agents to use `pixel search`/`pixel resolve`/`pixel impact` instead of
/// `grep`/`rg` for code discovery in indexed repositories.
fn deploy_agent_prompt(home: &Path, dry_run: bool) -> Result<InstallStep> {
    let dest_dir = home.join(".local/share/pixel");
    let dest = dest_dir.join("agent-prompt.md");
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
    Ok(InstallStep {
        id: "agent-prompt".into(),
        status: CheckStatus::Green,
        summary: format!(
            "{} agent-prompt.md",
            if needs_write { "deployed" } else { "verified" }
        ),
        detail: Some(format!("path={}", dest.display())),
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

/// Directories commonly holding project checkouts, searched one level deep
/// for a project-local `.codex/hooks.json` that could SHADOW the global one
/// installed above. Empirically verified (2026-08-30, `codex exec` against
/// a real ship-fast checkout carrying only a `worktree-path-guard.sh`
/// PreToolUse entry): Codex does NOT merge global and project-level
/// `hooks.json` — a project's own file completely replaces the global
/// PreToolUse array for every Codex session in that project. A destructive
/// `git branch -D` ran to completion, unblocked, proving the global guard
/// never fired. Any project with a pre-existing `.codex/hooks.json`
/// (installed by cmux/orca's worktree-path-guard, observed here in
/// ship-fast, omni, liza, execution-engine, and others) silently drops all
/// of pixel's Codex-side enforcement.
fn project_hook_search_roots(home: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for parent in ["Documents", "Desktop"] {
        let Ok(entries) = fs::read_dir(home.join(parent)) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                roots.push(path);
            }
        }
    }
    roots
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

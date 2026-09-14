//! `pixel uninstall` — the inverse of `pixel install`.
//!
//! Removes every trace pixel install wrote:
//!   - managed blocks from CLAUDE.md / AGENTS.md / .zcode/AGENTS.md /
//!     .pi/agent/AGENTS.md
//!   - pixel run-hook entries from Claude, Devin, Codex, Gemini, zcode, Cursor,
//!     and project-level .codex/hooks.json settings files
//!   - pixel run-hook scripts from ~/.claude/hooks/
//!   - the pi guard extension (~/.pi/agent/extensions/pixel-guard.ts)
//!   - the pixel block from Pi's ~/.pi/agent/APPEND_SYSTEM.md (the rest of
//!     that shared file is the user's and is kept)
//!   - the pixel rule source file (~/.agent-config/rules/pixel.md)
//!   - the pixel binary (~/.local/bin/pixel by default)
//!
//! Idempotent: safe to re-run. Each step reports what was removed (or that
//! nothing was found). Backups are written before every destructive write,
//! same as install.

use std::fs;
use std::path::{Path, PathBuf};

use crate::InstallError;
use crate::config;
use crate::install::{self, CheckStatus, InstallReport, InstallStep, InstallSummary};
use crate::routing;

pub type Result<T> = std::result::Result<T, InstallError>;

/// Options controlling an uninstall run.
#[derive(Debug, Clone, Default)]
pub struct UninstallOptions {
    /// Home directory. Defaults to `$HOME`.
    pub home: Option<PathBuf>,
    /// Path to the pixel binary to remove. Defaults to `~/.local/bin/pixel`.
    pub binary_path: Option<PathBuf>,
    /// Shell whose wrapper block should be removed, as a `$SHELL`-style value.
    /// Defaults to `$SHELL`.
    pub shell: Option<String>,
    /// If true, compute and report every step's outcome exactly as a real
    /// run would, but perform no filesystem writes.
    pub dry_run: bool,
    /// Remove only the shell wrapper block of `shell` and leave every other
    /// artifact in place: the way out of a block written for a shell that
    /// never loads it (`pixel doctor` names the file) without losing the
    /// install that works.
    pub wrappers_only: bool,
}

/// Markers that identify pixel-authored hook entries in any settings file.
/// Each corresponds to a hook script filename installed by `pixel install`.
const PIXEL_HOOK_MARKERS: &[&str] = &[
    config::GUARD_HOOK,
    config::SESSION_START_HOOK,
    config::PROMPT_SUBMIT_HOOK,
    config::POST_COMPACTION_HOOK,
    // Also clean up the old guard hook from pre-rename installs.
    config::OLD_GUARD_HOOK,
];

/// Run `pixel uninstall`. Idempotent: safe to re-run.
pub fn uninstall(options: &UninstallOptions) -> Result<InstallReport> {
    let home = options
        .home
        .clone()
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .ok_or(InstallError::NoHome)?;
    let binary_path = options
        .binary_path
        .clone()
        .unwrap_or_else(|| home.join(".local").join("bin").join("pixel"));

    let dry_run = options.dry_run;
    if options.wrappers_only {
        let step = install::remove_shell_wrappers(&home, options.shell.as_deref(), dry_run)?;
        let summary = InstallSummary {
            green: usize::from(step.status == CheckStatus::Green),
            yellow: usize::from(step.status == CheckStatus::Yellow),
            red: usize::from(step.status == CheckStatus::Red),
        };
        let ok = summary.red == 0;
        return Ok(InstallReport {
            version: "v1".into(),
            ok,
            executable_path: binary_path.display().to_string(),
            home: home.display().to_string(),
            dry_run,
            steps: vec![step],
            summary,
        });
    }
    let steps = vec![
        // 1. Remove shell wrappers from the shell's profile (~/.zshrc,
        //    ~/.bashrc, or fish's ~/.config/fish/conf.d/pixel.fish).
        install::remove_shell_wrappers(&home, options.shell.as_deref(), dry_run)?,
        // 2. Strip managed blocks from all agent-config Markdown files.
        strip_agent_configs(&home, dry_run)?,
        // 3. Remove pixel run-hook entries from Claude settings.json + delete hook
        //    scripts from ~/.claude/hooks/.
        remove_claude_hooks(&home, dry_run)?,
        // 4. Remove pixel run-hook entries from every other tool's settings file.
        remove_devin_hooks(&home, dry_run)?,
        remove_codex_hooks(&home, dry_run)?,
        remove_gemini_hooks(&home, dry_run)?,
        remove_zcode_hooks(&home, dry_run)?,
        remove_cursor_hooks(&home, dry_run)?,
        remove_pi_extension(&home, dry_run)?,
        // 5. Remove pixel hooks from project-level .codex/hooks.json files.
        remove_project_codex_hooks(&home, dry_run)?,
        // 6. Remove the pixel rule source file.
        remove_rule_source(&home, dry_run)?,
        // 7. Remove the pixel agent system prompt.
        remove_agent_prompt(&home, dry_run)?,
        // 7b. Take the pixel block out of Codex's developer_instructions.
        crate::codex_config::remove_developer_instructions(
            &crate::codex_config::codex_home(&home, options.home.is_some()),
            dry_run,
        )?,
        // 8. Remove the pixel binary.
        remove_binary(&binary_path, dry_run)?,
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
        executable_path: binary_path.display().to_string(),
        home: home.display().to_string(),
        dry_run,
        steps,
        summary: InstallSummary { green, yellow, red },
    })
}

// -------------------------------------------------------------------------
// Step 1: strip managed blocks from agent-config Markdown files
// -------------------------------------------------------------------------

fn strip_agent_configs(home: &Path, dry_run: bool) -> Result<InstallStep> {
    let targets = config::find_agent_configs(home);
    // Also strip managed blocks from zcode and pi AGENTS.md files.
    let mut all_targets = targets;
    let zcode_agents = home.join(".zcode").join("AGENTS.md");
    if zcode_agents.is_file() {
        all_targets.push(zcode_agents);
    }
    let pi_agents = home.join(config::PI_CONFIG_DIR).join("AGENTS.md");
    if pi_agents.is_file() {
        all_targets.push(pi_agents);
    }

    let mut stripped = 0usize;
    let mut skipped = 0usize;
    let mut backups: Vec<String> = Vec::new();

    for path in &all_targets {
        let original = match fs::read_to_string(path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        if !original.contains(config::MANAGED_BEGIN) {
            skipped += 1;
            continue;
        }
        let cleaned = config::strip_managed_block(&original);
        if dry_run {
            stripped += 1;
            continue;
        }
        let bk = config::backup_if_changing(path, cleaned.as_bytes())?;
        fs::write(path, &cleaned)?;
        if bk.is_some() {
            backups.push(path.display().to_string());
        }
        stripped += 1;
    }

    let summary =
        format!("stripped managed block from {stripped} file(s) ({skipped} already clean)");
    let detail = if backups.is_empty() {
        None
    } else {
        Some(format!("files=[{}]", backups.join(",")))
    };
    Ok(InstallStep {
        id: "agent-config".into(),
        status: CheckStatus::Green,
        summary: install::dry_run_summary(dry_run, &summary),
        detail,
    })
}

// -------------------------------------------------------------------------
// Step 2: remove Claude hooks (settings.json entries + hook scripts)
// -------------------------------------------------------------------------

fn remove_claude_hooks(home: &Path, dry_run: bool) -> Result<InstallStep> {
    let settings = home.join(".claude").join("settings.json");
    let mut removed_entries = 0usize;
    let mut backup_path = None;

    if settings.is_file() {
        let mut value = install::read_settings(&settings)?;
        if let Some(hooks) = value
            .get_mut("hooks")
            .and_then(serde_json::Value::as_object_mut)
        {
            let saved = if routing::has_delegate(hooks) {
                let saved = routing::load_rtk_backup(home)?;
                if saved.is_empty() {
                    return Err(InstallError::InvalidSettings {
                        path: home.join(routing::RTK_BACKUP),
                        reason: "RTK delegate backup missing; refusing to lose its registration"
                            .into(),
                    });
                }
                saved
            } else {
                Vec::new()
            };
            let before = hooks.clone();
            routing::remove_pixel_hooks(hooks);
            if !saved.is_empty() {
                routing::restore_rtk(hooks, &saved);
            }
            removed_entries += usize::from(*hooks != before);
            // Remove pixel entries from every event. Events that pixel
            // registered under: PreToolUse, SessionStart, UserPromptSubmit,
            // PostCompaction. But iterate ALL event keys — a user may have
            // moved things around, and we want to be thorough.
            let event_keys: Vec<String> = hooks.keys().cloned().collect();
            for event in event_keys {
                if let Some(existing) = hooks.get(&event) {
                    let mut filtered = existing.clone();
                    for marker in PIXEL_HOOK_MARKERS {
                        filtered = config::remove_hook_entries(&filtered, marker);
                    }
                    // Also handle flat-schema entries (Cursor-style command
                    // at top level) just in case.
                    for marker in PIXEL_HOOK_MARKERS {
                        filtered = config::remove_flat_hook_entries(&filtered, marker);
                    }
                    if filtered.as_array().is_some_and(std::vec::Vec::is_empty) {
                        hooks.remove(&event);
                        removed_entries += 1;
                    } else if filtered != *existing {
                        hooks.insert(event, filtered);
                        removed_entries += 1;
                    }
                }
            }
            // If the hooks object is now empty, remove it entirely.
            if hooks.is_empty()
                && let Some(obj) = value.as_object_mut()
            {
                obj.remove("hooks");
            }
        }
        if !dry_run && removed_entries > 0 {
            backup_path = install::write_settings(&settings, &value, dry_run)?;
        }
        if !dry_run && home.join(routing::RTK_BACKUP).is_file() {
            fs::remove_file(home.join(routing::RTK_BACKUP))?;
        }
    }

    // Delete hook scripts from ~/.claude/hooks/.
    let hooks_dir = home.join(config::CLAUDE_HOOKS_DIR);
    let hook_files = [
        config::GUARD_HOOK,
        config::SESSION_START_HOOK,
        config::PROMPT_SUBMIT_HOOK,
        config::POST_COMPACTION_HOOK,
        config::OLD_GUARD_HOOK,
    ];
    let mut scripts_removed = 0usize;
    for name in &hook_files {
        let path = hooks_dir.join(name);
        if !path.is_file() {
            continue;
        }
        if dry_run {
            scripts_removed += 1;
            continue;
        }
        // Back up before deleting.
        let current = fs::read(&path).unwrap_or_default();
        let _ = config::backup_if_changing(&path, &{
            let mut s = current.clone();
            s.push(0);
            s
        });
        let _ = fs::remove_file(&path);
        scripts_removed += 1;
    }

    let summary = format!(
        "removed {removed_entries} Claude hook event(s), deleted {scripts_removed} hook script(s)"
    );
    Ok(InstallStep {
        id: "hooks.claude".into(),
        status: CheckStatus::Green,
        summary: install::dry_run_summary(dry_run, &summary),
        detail: Some(install::with_backup_note(
            format!("settings={}", settings.display()),
            backup_path,
        )),
    })
}

// -------------------------------------------------------------------------
// Step 3a: remove Devin hooks
// -------------------------------------------------------------------------

fn remove_devin_hooks(home: &Path, dry_run: bool) -> Result<InstallStep> {
    let config_path = home
        .join(config::DEVIN_CONFIG_DIR)
        .join(config::DEVIN_CONFIG_FILE);
    let (removed, backup_path) = remove_pixel_hooks_from_settings(&config_path, dry_run)?;
    let summary = format!("removed {removed} Devin hook entry/entries");
    Ok(InstallStep {
        id: "hooks.devin".into(),
        status: CheckStatus::Green,
        summary: install::dry_run_summary(dry_run, &summary),
        detail: Some(install::with_backup_note(
            format!("config={}", config_path.display()),
            backup_path,
        )),
    })
}

// -------------------------------------------------------------------------
// Step 3b: remove Codex hooks
// -------------------------------------------------------------------------

fn remove_codex_hooks(home: &Path, dry_run: bool) -> Result<InstallStep> {
    let config_path = home.join(config::CODEX_HOOKS_FILE);
    let (removed, backup_path) = remove_pixel_hooks_from_settings(&config_path, dry_run)?;
    let summary = format!("removed {removed} Codex hook entry/entries");
    Ok(InstallStep {
        id: "hooks.codex".into(),
        status: CheckStatus::Green,
        summary: install::dry_run_summary(dry_run, &summary),
        detail: Some(install::with_backup_note(
            format!("config={}", config_path.display()),
            backup_path,
        )),
    })
}

// -------------------------------------------------------------------------
// Step 3c: remove Gemini hooks
// -------------------------------------------------------------------------

fn remove_gemini_hooks(home: &Path, dry_run: bool) -> Result<InstallStep> {
    let config_path = home.join(config::GEMINI_SETTINGS_FILE);
    let (removed, backup_path) = remove_pixel_hooks_from_settings(&config_path, dry_run)?;
    let summary = format!("removed {removed} Gemini hook entry/entries");
    Ok(InstallStep {
        id: "hooks.gemini".into(),
        status: CheckStatus::Green,
        summary: install::dry_run_summary(dry_run, &summary),
        detail: Some(install::with_backup_note(
            format!("config={}", config_path.display()),
            backup_path,
        )),
    })
}

// -------------------------------------------------------------------------
// Step 3d: remove zcode hooks + AGENTS.md managed block
// -------------------------------------------------------------------------

fn remove_zcode_hooks(home: &Path, dry_run: bool) -> Result<InstallStep> {
    let config_path = home.join(config::ZCODE_CONFIG_FILE);
    if !config_path.is_file() {
        return Ok(InstallStep {
            id: "hooks.zcode".into(),
            status: CheckStatus::Green,
            summary: install::dry_run_summary(dry_run, "no zcode config — skipping"),
            detail: None,
        });
    }
    let mut value = install::read_settings(&config_path)?;
    let mut removed = 0usize;
    // zcode nests hooks under `hooks.events.<Event>`.
    if let Some(hooks_root) = value
        .get_mut("hooks")
        .and_then(serde_json::Value::as_object_mut)
    {
        if let Some(events) = hooks_root
            .get_mut("events")
            .and_then(serde_json::Value::as_object_mut)
        {
            let event_keys: Vec<String> = events.keys().cloned().collect();
            for event in event_keys {
                if let Some(existing) = events.get(&event) {
                    let mut filtered = existing.clone();
                    for marker in PIXEL_HOOK_MARKERS {
                        filtered = config::remove_hook_entries(&filtered, marker);
                    }
                    if filtered.as_array().is_some_and(std::vec::Vec::is_empty) {
                        events.remove(&event);
                        removed += 1;
                    } else if filtered != *events.get(&event).unwrap() {
                        events.insert(event, filtered);
                        removed += 1;
                    }
                }
            }
            if events.is_empty() {
                hooks_root.remove("events");
            }
        }
        if hooks_root.is_empty()
            && let Some(obj) = value.as_object_mut()
        {
            obj.remove("hooks");
        }
    }
    let mut backup_path = None;
    if !dry_run && removed > 0 {
        backup_path = install::write_settings(&config_path, &value, dry_run)?;
    }

    // Also strip the managed block from ~/.zcode/AGENTS.md.
    let agents_md = home.join(".zcode").join("AGENTS.md");
    let mut agents_stripped = false;
    if agents_md.is_file() {
        let original = fs::read_to_string(&agents_md).unwrap_or_default();
        if original.contains(config::MANAGED_BEGIN) {
            let cleaned = config::strip_managed_block(&original);
            if !dry_run {
                let bk = config::backup_if_changing(&agents_md, cleaned.as_bytes())?;
                fs::write(&agents_md, &cleaned)?;
                backup_path = backup_path.or(bk);
            }
            agents_stripped = true;
        }
    }

    let summary = format!(
        "removed {removed} zcode hook entry/entries{}",
        if agents_stripped {
            " + stripped AGENTS.md managed block"
        } else {
            ""
        }
    );
    Ok(InstallStep {
        id: "hooks.zcode".into(),
        status: CheckStatus::Green,
        summary: install::dry_run_summary(dry_run, &summary),
        detail: Some(install::with_backup_note(
            format!("config={}", config_path.display()),
            backup_path,
        )),
    })
}

// -------------------------------------------------------------------------
// Step 3e: remove Cursor hooks (flat schema)
// -------------------------------------------------------------------------

fn remove_cursor_hooks(home: &Path, dry_run: bool) -> Result<InstallStep> {
    let config_path = home.join(config::CURSOR_HOOKS_FILE);
    if !config_path.is_file() {
        return Ok(InstallStep {
            id: "hooks.cursor".into(),
            status: CheckStatus::Green,
            summary: install::dry_run_summary(dry_run, "no Cursor hooks.json — skipping"),
            detail: None,
        });
    }
    let mut value = install::read_settings(&config_path)?;
    let mut removed = 0usize;
    if let Some(hooks) = value
        .get_mut("hooks")
        .and_then(serde_json::Value::as_object_mut)
    {
        let event_keys: Vec<String> = hooks.keys().cloned().collect();
        for event in event_keys {
            if let Some(existing) = hooks.get(&event) {
                let mut filtered = existing.clone();
                for marker in PIXEL_HOOK_MARKERS {
                    filtered = config::remove_flat_hook_entries(&filtered, marker);
                }
                if filtered.as_array().is_some_and(std::vec::Vec::is_empty) {
                    hooks.remove(&event);
                    removed += 1;
                } else if filtered != *hooks.get(&event).unwrap() {
                    hooks.insert(event, filtered);
                    removed += 1;
                }
            }
        }
        if hooks.is_empty()
            && let Some(obj) = value.as_object_mut()
        {
            obj.remove("hooks");
        }
    }
    let mut backup_path = None;
    if !dry_run && removed > 0 {
        backup_path = install::write_settings(&config_path, &value, dry_run)?;
    }
    let summary = format!("removed {removed} Cursor hook entry/entries");
    Ok(InstallStep {
        id: "hooks.cursor".into(),
        status: CheckStatus::Green,
        summary: install::dry_run_summary(dry_run, &summary),
        detail: Some(install::with_backup_note(
            format!("config={}", config_path.display()),
            backup_path,
        )),
    })
}

// -------------------------------------------------------------------------
// Step 3f: remove pi guard extension + AGENTS.md managed block
// -------------------------------------------------------------------------

fn remove_pi_extension(home: &Path, dry_run: bool) -> Result<InstallStep> {
    let config_dir = home.join(config::PI_CONFIG_DIR);
    if !config_dir.is_dir() {
        return Ok(InstallStep {
            id: "hooks.pi".into(),
            status: CheckStatus::Green,
            summary: install::dry_run_summary(dry_run, "no pi config dir — skipping"),
            detail: None,
        });
    }
    let ext_file = config_dir.join("extensions").join("pixel-guard.ts");
    let mut ext_removed = false;
    if ext_file.is_file() {
        if !dry_run {
            let current = fs::read(&ext_file).unwrap_or_default();
            let _ = config::backup_if_changing(&ext_file, &{
                let mut s = current.clone();
                s.push(0);
                s
            });
            let _ = fs::remove_file(&ext_file);
        }
        ext_removed = true;
    }

    // Strip managed block from ~/.pi/agent/AGENTS.md.
    let agents_md = config_dir.join("AGENTS.md");
    let mut agents_stripped = false;
    if agents_md.is_file() {
        let original = fs::read_to_string(&agents_md).unwrap_or_default();
        if original.contains(config::MANAGED_BEGIN) {
            let cleaned = config::strip_managed_block(&original);
            if !dry_run {
                let _ = config::backup_if_changing(&agents_md, cleaned.as_bytes())?;
                fs::write(&agents_md, &cleaned)?;
            }
            agents_stripped = true;
        }
    }

    let summary = format!(
        "{}{}",
        if ext_removed {
            "removed pi guard extension"
        } else {
            "no pi extension found"
        },
        if agents_stripped {
            " + stripped AGENTS.md managed block"
        } else {
            ""
        },
    );
    Ok(InstallStep {
        id: "hooks.pi".into(),
        status: CheckStatus::Green,
        summary: install::dry_run_summary(dry_run, &summary),
        detail: Some(format!("ext={}", ext_file.display())),
    })
}

// -------------------------------------------------------------------------
// Step 4: remove pixel hooks from project-level .codex/hooks.json files
// -------------------------------------------------------------------------

fn remove_project_codex_hooks(home: &Path, dry_run: bool) -> Result<InstallStep> {
    let mut patched = Vec::new();
    let mut conflicts = Vec::new();
    for root in project_hook_search_roots(home) {
        let config_path = root.join(".codex").join("hooks.json");
        if !config_path.is_file() {
            continue;
        }
        match restore_project_codex_composed_guard(&config_path, dry_run)? {
            ComposedGuardRestore::Restored => {
                patched.push(config_path.display().to_string());
                continue;
            }
            // A snapshot exists but its schema or the managed guard has been
            // edited.  Do not fall through to generic Pixel-hook removal: it
            // would delete the only record needed to recover the user's
            // original guard configuration.
            ComposedGuardRestore::Conflict => {
                conflicts.push(config_path.display().to_string());
                continue;
            }
            ComposedGuardRestore::NotManaged => {}
        }
        let (removed, _) = remove_pixel_hooks_from_settings(&config_path, dry_run)?;
        if removed > 0 {
            patched.push(config_path.display().to_string());
        }
    }
    let summary = if patched.is_empty() && conflicts.is_empty() {
        "no project-level .codex/hooks.json needed patching".to_string()
    } else {
        format!(
            "patched {} project-level .codex/hooks.json file(s); {} composed guard conflict(s) preserved",
            patched.len(),
            conflicts.len()
        )
    };
    Ok(InstallStep {
        id: "hooks.codex_project_shadow".into(),
        status: if conflicts.is_empty() {
            CheckStatus::Green
        } else {
            CheckStatus::Yellow
        },
        summary: install::dry_run_summary(dry_run, &summary),
        detail: if patched.is_empty() && conflicts.is_empty() {
            None
        } else {
            let mut detail = Vec::new();
            if !patched.is_empty() {
                detail.push(format!("restored=[{}]", patched.join(", ")));
            }
            if !conflicts.is_empty() {
                // Paths explain the actionable conflict without exposing the
                // snapshot payload, which may contain arbitrary user hooks.
                detail.push(format!("preserved_conflicts=[{}]", conflicts.join(", ")));
            }
            Some(detail.join("; "))
        },
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ComposedGuardRestore {
    /// No sidecar belongs to this project; generic Pixel-hook cleanup may run.
    NotManaged,
    /// Exact original `PreToolUse` was restored and the sidecar was removed.
    Restored,
    /// A sidecar exists, but the current managed value was changed or the
    /// sidecar is invalid.  Preserve both files for manual reconciliation.
    Conflict,
}

/// Restore a project-local Codex guard adoption without ever overwriting a
/// user change made after install.
///
/// New sidecars carry `managed_pre_tool_use`, so comparison is byte-for-byte
/// at the JSON value layer.  Early compositions did not record that field; a
/// deliberately strict legacy signature permits their recovery only when the
/// current array is exactly one unfiltered composed-guard command pointing at
/// this project's own sidecar.
fn restore_project_codex_composed_guard(
    config_path: &Path,
    dry_run: bool,
) -> Result<ComposedGuardRestore> {
    let Some(codex_dir) = config_path.parent() else {
        return Ok(ComposedGuardRestore::NotManaged);
    };
    let sidecar = codex_dir.join(routing::CODEX_COMPOSED_BACKUP);
    if !sidecar.is_file() {
        return Ok(ComposedGuardRestore::NotManaged);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if fs::metadata(&sidecar)?.permissions().mode() & 0o077 != 0 {
            return Ok(ComposedGuardRestore::Conflict);
        }
    }

    let snapshot: serde_json::Value = match fs::read_to_string(&sidecar)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
    {
        Some(value) => value,
        None => return Ok(ComposedGuardRestore::Conflict),
    };
    if snapshot.get("version").and_then(serde_json::Value::as_u64) != Some(1)
        || snapshot.get("provider").and_then(serde_json::Value::as_str) != Some("codex")
    {
        return Ok(ComposedGuardRestore::Conflict);
    }
    let Some(original_pre) = snapshot
        .get("pre_tool_use")
        .and_then(serde_json::Value::as_array)
    else {
        return Ok(ComposedGuardRestore::Conflict);
    };

    let mut settings = install::read_settings(config_path)?;
    let Some(current_pre) = settings
        .get("hooks")
        .and_then(serde_json::Value::as_object)
        .and_then(|hooks| hooks.get("PreToolUse"))
        .and_then(serde_json::Value::as_array)
    else {
        return Ok(ComposedGuardRestore::Conflict);
    };

    let unchanged = snapshot
        .get("managed_pre_tool_use")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|managed| managed == current_pre)
        || (snapshot.get("managed_pre_tool_use").is_none()
            && legacy_composed_guard_signature(current_pre, &sidecar));
    if !unchanged {
        return Ok(ComposedGuardRestore::Conflict);
    }

    if !dry_run {
        let hooks = settings
            .get_mut("hooks")
            .and_then(serde_json::Value::as_object_mut)
            // `current_pre` above proves this cannot fail unless an internal
            // mutation happened between reads, which it cannot in this value.
            .expect("validated hooks object");
        hooks.insert(
            "PreToolUse".into(),
            serde_json::Value::Array(original_pre.clone()),
        );
        install::write_settings(config_path, &settings, false)?;
        // Delete only after the restored settings are durable.  If deletion
        // fails, retain the sidecar rather than risk silently losing recovery
        // data; a subsequent uninstall will report a conflict instead of
        // overwriting user state.
        fs::remove_file(&sidecar)?;
    }
    Ok(ComposedGuardRestore::Restored)
}

fn legacy_composed_guard_signature(current_pre: &[serde_json::Value], sidecar: &Path) -> bool {
    let [group] = current_pre else {
        return false;
    };
    let Some(group) = group.as_object() else {
        return false;
    };
    // An unfiltered group has no matcher and no opaque fields that could be
    // meaningful to Codex.  `hooks` must contain exactly one command hook.
    if group.len() != 1 || !group.contains_key("hooks") {
        return false;
    }
    let Some(hooks) = group.get("hooks").and_then(serde_json::Value::as_array) else {
        return false;
    };
    let [hook] = hooks.as_slice() else {
        return false;
    };
    if hook.get("type").and_then(serde_json::Value::as_str) != Some("command") {
        return false;
    }
    let Some(command) = hook.get("command").and_then(serde_json::Value::as_str) else {
        return false;
    };
    let escaped_sidecar = sidecar.to_string_lossy().replace('\'', "'\\''");
    // Installs since the command rename write `run-hook`; 0.2.x wrote `hook`.
    ["run-hook", "hook"].iter().any(|verb| {
        command.contains(&format!(
            " {verb} composed-guard --provider codex --backup "
        ))
    }) && command.ends_with(&format!("'{escaped_sidecar}'"))
}

/// Directories commonly holding project checkouts — mirrors the same logic
/// in install.rs.
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

// -------------------------------------------------------------------------
// Step 5: remove the pixel rule source file
// -------------------------------------------------------------------------

fn remove_rule_source(home: &Path, dry_run: bool) -> Result<InstallStep> {
    let path = home.join(config::PIXEL_RULES_REL);
    if !path.is_file() {
        return Ok(InstallStep {
            id: "rule.source".into(),
            status: CheckStatus::Green,
            summary: install::dry_run_summary(dry_run, "no rule source file — skipping"),
            detail: None,
        });
    }
    if !dry_run {
        let current = fs::read(&path).unwrap_or_default();
        let _ = config::backup_if_changing(&path, &{
            let mut s = current.clone();
            s.push(0);
            s
        });
        let _ = fs::remove_file(&path);
    }
    Ok(InstallStep {
        id: "rule.source".into(),
        status: CheckStatus::Green,
        summary: install::dry_run_summary(dry_run, "removed pixel rule source file"),
        detail: Some(format!("path={}", path.display())),
    })
}

// -------------------------------------------------------------------------
// Step 6: remove the pixel agent system prompt
// -------------------------------------------------------------------------

fn remove_agent_prompt(home: &Path, dry_run: bool) -> Result<InstallStep> {
    let path = home.join(".local/share/pixel/agent-prompt.md");
    let subagent_path = home
        .join(".local/share/pixel")
        .join(install::SUBAGENT_PROMPT_FILE);
    let pi_path = home.join(install::PI_PROMPT_REL);
    let existed = path.is_file();
    if !existed && !subagent_path.is_file() && !pi_path.is_file() {
        return Ok(InstallStep {
            id: "agent-prompt".into(),
            status: CheckStatus::Green,
            summary: install::dry_run_summary(dry_run, "no agent-prompt file — skipping"),
            detail: None,
        });
    }
    // Pi's system-prompt file is shared: pixel owns the managed block inside
    // it, not the file. Whatever the user keeps outside the markers survives,
    // and the file is deleted only when the block was all it held.
    let pi_original = match fs::read_to_string(&pi_path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };
    let pi_cleaned = pi_original
        .contains(config::MANAGED_BEGIN)
        .then(|| config::strip_managed_block(&pi_original));
    let pi_removed = pi_cleaned
        .as_deref()
        .is_some_and(|cleaned| cleaned.trim().is_empty());
    if !dry_run {
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&subagent_path);
        if let Some(cleaned) = pi_cleaned.as_deref() {
            if cleaned.trim().is_empty() {
                let _ = config::backup_if_changing(&pi_path, cleaned.as_bytes())?;
                fs::remove_file(&pi_path)?;
            } else if cleaned != pi_original {
                let _ = config::backup_if_changing(&pi_path, cleaned.as_bytes())?;
                fs::write(&pi_path, cleaned)?;
            }
        }
    }
    let summary = if pi_removed {
        "removed agent-prompt.md, subagent-prompt.md and the pi prompt file"
    } else if pi_cleaned.is_some() {
        "removed agent-prompt.md and subagent-prompt.md, kept the text around the pixel block in APPEND_SYSTEM.md"
    } else {
        "removed agent-prompt.md and subagent-prompt.md"
    };
    Ok(InstallStep {
        id: "agent-prompt".into(),
        status: CheckStatus::Green,
        summary: install::dry_run_summary(dry_run, summary),
        detail: Some(format!(
            "path={} subagent={} pi={}",
            path.display(),
            subagent_path.display(),
            pi_path.display()
        )),
    })
}

// -------------------------------------------------------------------------
// Step 7: remove the pixel binary
// -------------------------------------------------------------------------

fn remove_binary(binary_path: &Path, dry_run: bool) -> Result<InstallStep> {
    if !binary_path.is_file() {
        return Ok(InstallStep {
            id: "binary".into(),
            status: CheckStatus::Green,
            summary: install::dry_run_summary(dry_run, "no binary found — skipping"),
            detail: Some(format!("path={}", binary_path.display())),
        });
    }
    if !dry_run {
        let _ = fs::remove_file(binary_path);
    }
    Ok(InstallStep {
        id: "binary".into(),
        status: CheckStatus::Green,
        summary: install::dry_run_summary(dry_run, "removed pixel binary"),
        detail: Some(format!("path={}", binary_path.display())),
    })
}

// -------------------------------------------------------------------------
// Shared helper: remove pixel run-hook entries from a settings file with a
// top-level `hooks` object (Claude/Devin/Codex/Gemini schema).
// -------------------------------------------------------------------------

fn remove_pixel_hooks_from_settings(
    config_path: &Path,
    dry_run: bool,
) -> Result<(usize, Option<PathBuf>)> {
    if !config_path.is_file() {
        return Ok((0, None));
    }
    let mut value = install::read_settings(config_path)?;
    let mut removed = 0usize;
    if let Some(hooks) = value
        .get_mut("hooks")
        .and_then(serde_json::Value::as_object_mut)
    {
        let before = hooks.clone();
        routing::remove_pixel_hooks(hooks);
        removed += usize::from(*hooks != before);
        let event_keys: Vec<String> = hooks.keys().cloned().collect();
        for event in event_keys {
            if let Some(existing) = hooks.get(&event) {
                let mut filtered = existing.clone();
                for marker in PIXEL_HOOK_MARKERS {
                    filtered = config::remove_hook_entries(&filtered, marker);
                }
                // Also handle flat-schema entries.
                for marker in PIXEL_HOOK_MARKERS {
                    filtered = config::remove_flat_hook_entries(&filtered, marker);
                }
                let changed = filtered != *hooks.get(&event).unwrap();
                if filtered.as_array().is_some_and(std::vec::Vec::is_empty) {
                    hooks.remove(&event);
                    removed += 1;
                } else if changed {
                    hooks.insert(event, filtered);
                    removed += 1;
                }
            }
        }
        if hooks.is_empty()
            && let Some(obj) = value.as_object_mut()
        {
            obj.remove("hooks");
        }
    }
    let mut backup_path = None;
    if !dry_run && removed > 0 {
        backup_path = install::write_settings(config_path, &value, dry_run)?;
    }
    Ok((removed, backup_path))
}

#[cfg(test)]
mod routing_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn routing_uninstall_restores_rtk_fragment_and_preserves_later_user_changes() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        let path = home.join(".claude/settings.json");
        let rtk =
            json!({"matcher":"Bash","hooks":[{"type":"command","command":"rtk hook claude"}]});
        let foreign = json!({"matcher":"startup","hooks":[{"type":"command","command":"keep-security-check"}]});
        install::write_settings(
            &path,
            &json!({"hooks":{"PreToolUse":[rtk.clone()],"SessionStart":[foreign.clone()]}}),
            false,
        )
        .unwrap();
        routing::install_provider(
            home,
            Path::new("/tmp/pixel"),
            routing::Provider::Claude,
            false,
        )
        .unwrap();
        assert!(home.join(routing::RTK_BACKUP).is_file());
        let mut installed = install::read_settings(&path).unwrap();
        installed["later_user_change"] = json!(true);
        install::write_settings(&path, &installed, false).unwrap();
        remove_claude_hooks(home, true).unwrap();
        assert_eq!(install::read_settings(&path).unwrap(), installed);
        remove_claude_hooks(home, false).unwrap();
        let restored = install::read_settings(&path).unwrap();
        assert_eq!(restored["hooks"]["PreToolUse"], json!([rtk]));
        assert_eq!(restored["hooks"]["SessionStart"], json!([foreign]));
        assert_eq!(restored["later_user_change"], true);
        assert!(!home.join(routing::RTK_BACKUP).exists());
        remove_claude_hooks(home, false).unwrap();
        assert_eq!(install::read_settings(&path).unwrap(), restored);
    }

    #[test]
    fn routing_uninstall_removes_direct_codex_and_devin_commands() {
        for provider in [routing::Provider::Codex, routing::Provider::Devin] {
            let home = tempfile::tempdir().unwrap();
            routing::install_provider(home.path(), Path::new("/tmp/pixel"), provider, false)
                .unwrap();
            let path = provider.path(home.path());
            assert!(remove_pixel_hooks_from_settings(&path, false).unwrap().0 > 0);
            assert_eq!(install::read_settings(&path).unwrap(), json!({}));
        }
    }

    /// A mixed event keeps its foreign entries after the pixel ones go, an
    /// event holding only pixel entries disappears, an untouched event is
    /// neither rewritten nor counted.
    #[test]
    fn remove_pixel_hooks_from_settings_rewrites_only_the_events_that_changed() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("settings.json");
        let lint = json!({"matcher":"Bash","hooks":[{"type":"command","command":"lint"}]});
        let guard = json!({"matcher":"Bash","hooks":[{"type":"command","command":format!("sh {}", config::GUARD_HOOK)}]});
        let start = json!({"hooks":[{"type":"command","command":format!("pixel run-hook {}", config::SESSION_START_HOOK)}]});
        let stop = json!({"hooks":[{"type":"command","command":"say done"}]});
        install::write_settings(
            &path,
            &json!({"hooks":{
                "PreToolUse":[guard, lint.clone()],
                "SessionStart":[start],
                "Stop":[stop.clone()],
            }, "theme":"dark"}),
            false,
        )
        .unwrap();
        let (removed, backup) = remove_pixel_hooks_from_settings(&path, false).unwrap();
        assert_eq!(removed, 2, "PreToolUse rewritten, SessionStart dropped");
        assert!(backup.is_some());
        let after = install::read_settings(&path).unwrap();
        assert_eq!(after["hooks"]["PreToolUse"], json!([lint]));
        assert!(after["hooks"].get("SessionStart").is_none(), "{after}");
        assert_eq!(after["hooks"]["Stop"], json!([stop]));
        assert_eq!(after["theme"], "dark");
        let (again, _) = remove_pixel_hooks_from_settings(&path, false).unwrap();
        assert_eq!(again, 0, "nothing left to remove");
        assert_eq!(
            remove_pixel_hooks_from_settings(&home.path().join("absent.json"), false).unwrap(),
            (0, None)
        );
    }

    #[test]
    fn legacy_composed_guard_signature_accepts_both_hook_verbs() {
        let sidecar = Path::new("/p/.codex/pixel-composed.json");
        let group = |command: &str| vec![json!({"hooks":[{"type":"command","command": command}]})];
        for verb in ["run-hook", "hook"] {
            let command = format!(
                "'/tmp/pixel' {verb} composed-guard --provider codex --backup '/p/.codex/pixel-composed.json'"
            );
            assert!(
                legacy_composed_guard_signature(&group(&command), sidecar),
                "`{verb}` composed-guard entry is Pixel's: {command}"
            );
        }
        assert!(!legacy_composed_guard_signature(
            &group(
                "'/tmp/pixel' run-hook guard --provider codex --backup '/p/.codex/pixel-composed.json'"
            ),
            sidecar
        ));
        assert!(!legacy_composed_guard_signature(
            &group("'/tmp/pixel' run-hook composed-guard --provider codex --backup '/other.json'"),
            sidecar
        ));
    }

    #[test]
    fn project_composed_guard_uninstall_restores_exact_snapshot() {
        let home = tempfile::tempdir().unwrap();
        let codex = home.path().join("Documents/project/.codex");
        let path = codex.join("hooks.json");
        let original =
            json!([{"matcher":"Bash","hooks":[{"type":"command","command":"keep-guard"}]}]);
        let managed = json!([{"hooks":[{"type":"command","command":"'/tmp/pixel' hook composed-guard --provider codex --backup '/unused'"}]}]);
        install::write_settings(
            &path,
            &json!({"hooks":{"PreToolUse":managed.clone()}}),
            false,
        )
        .unwrap();
        install::write_settings(
            &codex.join(routing::CODEX_COMPOSED_BACKUP),
            &json!({
                "version": 1,
                "provider": "codex",
                "pre_tool_use": original.clone(),
                "managed_pre_tool_use": managed,
            }),
            false,
        )
        .unwrap();
        make_private(&codex.join(routing::CODEX_COMPOSED_BACKUP));

        let step = remove_project_codex_hooks(home.path(), false).unwrap();
        assert_eq!(step.status, CheckStatus::Green);
        assert_eq!(
            install::read_settings(&path).unwrap()["hooks"]["PreToolUse"],
            original
        );
        assert!(!codex.join(routing::CODEX_COMPOSED_BACKUP).exists());
    }

    #[test]
    fn project_composed_guard_uninstall_preserves_changed_managed_value() {
        let home = tempfile::tempdir().unwrap();
        let codex = home.path().join("Documents/project/.codex");
        let path = codex.join("hooks.json");
        let managed = json!([{"hooks":[{"type":"command","command":"'/tmp/pixel' hook composed-guard --provider codex --backup '/unused'"}]}]);
        let changed = json!([
            {"hooks":[{"type":"command","command":"'/tmp/pixel' hook composed-guard --provider codex --backup '/unused'"}]},
            {"matcher":"Bash","hooks":[{"type":"command","command":"user-change"}]}
        ]);
        install::write_settings(
            &path,
            &json!({"hooks":{"PreToolUse":changed.clone()}}),
            false,
        )
        .unwrap();
        install::write_settings(
            &codex.join(routing::CODEX_COMPOSED_BACKUP),
            &json!({
                "version": 1,
                "provider": "codex",
                "pre_tool_use": [],
                "managed_pre_tool_use": managed,
            }),
            false,
        )
        .unwrap();
        make_private(&codex.join(routing::CODEX_COMPOSED_BACKUP));

        let step = remove_project_codex_hooks(home.path(), false).unwrap();
        assert_eq!(step.status, CheckStatus::Yellow);
        assert_eq!(
            install::read_settings(&path).unwrap()["hooks"]["PreToolUse"],
            changed
        );
        assert!(codex.join(routing::CODEX_COMPOSED_BACKUP).is_file());
    }

    fn make_private(path: &Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        #[cfg(not(unix))]
        let _ = path;
    }
}

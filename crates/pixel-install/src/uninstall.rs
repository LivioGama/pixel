//! `pixel uninstall` — the inverse of `pixel install`.
//!
//! Removes every trace pixel install wrote:
//!   - managed blocks from CLAUDE.md / AGENTS.md / .zcode/AGENTS.md /
//!     .pi/agent/AGENTS.md
//!   - pixel hook entries from Claude, Devin, Codex, Gemini, zcode, Cursor,
//!     and project-level .codex/hooks.json settings files
//!   - pixel hook scripts from ~/.claude/hooks/
//!   - the pi guard extension (~/.pi/agent/extensions/pixel-guard.ts)
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

pub type Result<T> = std::result::Result<T, InstallError>;

/// Options controlling an uninstall run.
#[derive(Debug, Clone)]
#[derive(Default)]
pub struct UninstallOptions {
    /// Home directory. Defaults to `$HOME`.
    pub home: Option<PathBuf>,
    /// Path to the pixel binary to remove. Defaults to `~/.local/bin/pixel`.
    pub binary_path: Option<PathBuf>,
    /// If true, compute and report every step's outcome exactly as a real
    /// run would, but perform no filesystem writes.
    pub dry_run: bool,
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
    let steps = vec![
        // 1. Strip managed blocks from all agent-config Markdown files.
        strip_agent_configs(&home, dry_run)?,
        // 2. Remove pixel hook entries from Claude settings.json + delete hook
        //    scripts from ~/.claude/hooks/.
        remove_claude_hooks(&home, dry_run)?,
        // 3. Remove pixel hook entries from every other tool's settings file.
        remove_devin_hooks(&home, dry_run)?,
        remove_codex_hooks(&home, dry_run)?,
        remove_gemini_hooks(&home, dry_run)?,
        remove_zcode_hooks(&home, dry_run)?,
        remove_cursor_hooks(&home, dry_run)?,
        remove_pi_extension(&home, dry_run)?,
        // 4. Remove pixel hooks from project-level .codex/hooks.json files.
        remove_project_codex_hooks(&home, dry_run)?,
        // 5. Remove the pixel rule source file.
        remove_rule_source(&home, dry_run)?,
        // 6. Remove the pixel binary.
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
                    if filtered.as_array().is_some_and(|a| a.is_empty()) {
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
                && let Some(obj) = value.as_object_mut() {
                    obj.remove("hooks");
                }
        }
        if !dry_run && removed_entries > 0 {
            backup_path = install::write_settings(&settings, &value, dry_run)?;
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
                    if filtered.as_array().is_some_and(|a| a.is_empty()) {
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
            && let Some(obj) = value.as_object_mut() {
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
                if filtered.as_array().is_some_and(|a| a.is_empty()) {
                    hooks.remove(&event);
                    removed += 1;
                } else if filtered != *hooks.get(&event).unwrap() {
                    hooks.insert(event, filtered);
                    removed += 1;
                }
            }
        }
        if hooks.is_empty()
            && let Some(obj) = value.as_object_mut() {
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
    for root in project_hook_search_roots(home) {
        let config_path = root.join(".codex").join("hooks.json");
        if !config_path.is_file() {
            continue;
        }
        let (removed, _) = remove_pixel_hooks_from_settings(&config_path, dry_run)?;
        if removed > 0 {
            patched.push(config_path.display().to_string());
        }
    }
    let summary = if patched.is_empty() {
        "no project-level .codex/hooks.json needed patching".to_string()
    } else {
        format!(
            "patched {} project-level .codex/hooks.json file(s)",
            patched.len()
        )
    };
    Ok(InstallStep {
        id: "hooks.codex_project_shadow".into(),
        status: CheckStatus::Green,
        summary: install::dry_run_summary(dry_run, &summary),
        detail: if patched.is_empty() {
            None
        } else {
            Some(patched.join(", "))
        },
    })
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
// Step 6: remove the pixel binary
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
// Shared helper: remove pixel hook entries from a settings file with a
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
                if filtered.as_array().is_some_and(|a| a.is_empty()) {
                    hooks.remove(&event);
                    removed += 1;
                } else if changed {
                    hooks.insert(event, filtered);
                    removed += 1;
                }
            }
        }
        if hooks.is_empty()
            && let Some(obj) = value.as_object_mut() {
                obj.remove("hooks");
            }
    }
    let mut backup_path = None;
    if !dry_run && removed > 0 {
        backup_path = install::write_settings(config_path, &value, dry_run)?;
    }
    Ok((removed, backup_path))
}

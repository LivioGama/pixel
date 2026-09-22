//! OpenCode integration through `~/.config/opencode/AGENTS.md`.
//!
//! AGENTS.md is the one mechanism both OpenCode generations honour for
//! global instructions: v1 loads it in the global slot and v2 — where the
//! `instructions` config field is accepted but never resolved — loads only
//! AGENTS.md files. The deployed prompt is embedded between the managed
//! markers, exactly like Pi's `APPEND_SYSTEM.md`, so `pixel install`
//! refreshes it in place while text the user keeps outside the markers
//! survives.
//!
//! One shadow to preserve: on v1 a *new* global AGENTS.md replaces the
//! `~/.claude/CLAUDE.md` fallback (the first matching file wins the global
//! slot). When install creates the file where none existed and a
//! `~/.claude/CLAUDE.md` is present, the file is seeded with that content —
//! the winning file then carries everything the shadowed one had, plus the
//! pixel block. On v2 there is no fallback to shadow, and the copied rules
//! are simply the rules the user wanted anyway.
//!
//! The same step sweeps two stale artifacts out of `opencode.json`:
//! `instructions` entries naming the deployed prompt (written by earlier
//! installs — dead config on v2 and a second copy of the prompt on v1) and
//! `plugin`/`plugins` entries whose `pixel.mjs` target no longer exists —
//! a load failure on every launch, left behind by manual attempts to wire
//! the repo's `.opencode/plugins/pixel.mjs` into a global config.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::config;
use crate::install::{self, CheckStatus, InstallStep, Result};

/// `AGENTS.md`, relative to the OpenCode config directory.
pub const AGENTS_MD_FILE: &str = "AGENTS.md";

/// `opencode.json`, relative to the OpenCode config directory — sweep-only:
/// the mechanism lives in AGENTS.md, so an unreadable config never blocks
/// the block.
pub const OPENCODE_CONFIG_FILE: &str = "opencode.json";

/// Path tail identifying the deployed prompt inside a leftover
/// `instructions` entry from the earlier mechanism.
const PROMPT_PATH_TAIL: &str = ".local/share/pixel/agent-prompt.md";

/// File name of the plugin the repo ships for project-local use; a global
/// `plugin` entry pointing at one that does not exist is a guaranteed load
/// failure.
const PIXEL_PLUGIN_FILE: &str = "pixel.mjs";

/// The OpenCode config directory: `$XDG_CONFIG_HOME/opencode` on a real
/// install (the variable OpenCode itself honours), `<home>/.config/opencode`
/// for tests and explicit `--home` overrides.
pub(crate) fn opencode_config_dir(home: &Path, home_was_explicit: bool) -> PathBuf {
    resolve_config_dir(home, home_was_explicit, std::env::var_os("XDG_CONFIG_HOME"))
}

/// Pure resolution behind [`opencode_config_dir`], so tests pin every arm
/// without mutating the process environment.
fn resolve_config_dir(
    home: &Path,
    home_was_explicit: bool,
    xdg_config_home: Option<std::ffi::OsString>,
) -> PathBuf {
    if !home_was_explicit
        && let Some(dir) = xdg_config_home
        && !dir.is_empty()
    {
        return PathBuf::from(dir).join("opencode");
    }
    home.join(".config").join("opencode")
}

fn agents_md_path(config_dir: &Path) -> PathBuf {
    config_dir.join(AGENTS_MD_FILE)
}

fn config_path(config_dir: &Path) -> PathBuf {
    config_dir.join(OPENCODE_CONFIG_FILE)
}

/// True when an `instructions` entry names the deployed Pixel prompt,
/// wherever it was installed from. Suffix-matched, not substring: a
/// `.../agent-prompt.md.bak` backup is the user's file, not our entry.
fn is_pixel_instruction(entry: &str) -> bool {
    entry
        .replace('\\', "/")
        .trim_end()
        .ends_with(PROMPT_PATH_TAIL)
}

/// True when a `plugin`/`plugins` entry names `pixel.mjs` and resolves to a
/// path that does not exist — `~/` against `home`, relative paths against
/// the config directory, like OpenCode. An entry that resolves to a real
/// file is left alone even if it is ours: v2 warns on file plugins but
/// still loads them.
fn is_stale_pixel_plugin(entry: &str, config_dir: &Path, home: &Path) -> bool {
    let entry = entry.trim();
    if entry.rsplit('/').next() != Some(PIXEL_PLUGIN_FILE) {
        return false;
    }
    let path = if let Some(rest) = entry.strip_prefix("~/") {
        home.join(rest)
    } else if entry.starts_with('~') {
        // `~other/...` — cannot resolve without a user database; leave it.
        return false;
    } else {
        let p = Path::new(entry);
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            config_dir.join(p)
        }
    };
    !path.exists()
}

/// `pixel install` step: put the bundled prompt inside the managed markers
/// of `AGENTS.md`, seeding a new file with `~/.claude/CLAUDE.md` when one
/// exists so a v1 install shadows nothing, then sweep stale pixel entries
/// out of `opencode.json`.
pub(crate) fn install_opencode(
    config_dir: &Path,
    home: &Path,
    dry_run: bool,
) -> Result<InstallStep> {
    let agents = agents_md_path(config_dir);
    let json = config_path(config_dir);
    let detail = Some(format!(
        "agents={} config={}",
        agents.display(),
        json.display()
    ));
    let step = |status, summary: String| InstallStep {
        id: "opencode-agents-md".into(),
        status,
        summary,
        detail: detail.clone(),
    };

    // The managed block ----------------------------------------------------
    // Only NotFound means "no file": an unreadable AGENTS.md is an error to
    // surface, not a file to replace wholesale.
    let existing = match fs::read_to_string(&agents) {
        Ok(content) => Some(content),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    let seeded = match &existing {
        Some(content) => content.clone(),
        // A new global AGENTS.md wins the slot ~/.claude/CLAUDE.md fills on
        // v1 — carry that content into it so nothing is shadowed.
        None => match fs::read_to_string(home.join(".claude/CLAUDE.md")) {
            Ok(content) => content,
            Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e.into()),
        },
    };
    let wanted = config::apply_managed_markers(&seeded, install::AGENT_PROMPT_ASSET);
    if existing.as_deref() == Some(wanted.as_str()) {
        // Fall through to the sweep even when the block is already current.
    } else if dry_run {
        return Ok(step(
            CheckStatus::Green,
            install::dry_run_summary(true, "would update AGENTS.md managed block"),
        ));
    } else {
        fs::create_dir_all(config_dir)?;
        install::write_atomically(&agents, &wanted)?;
    }

    // The sweeps -----------------------------------------------------------
    let (removed_instructions, removed_plugins, sweep_note) = sweep_config(&json, home, dry_run);
    let mut summary = format!(
        "{} pixel block in {}",
        if wanted == seeded {
            "verified"
        } else {
            "installed"
        },
        agents.display()
    );
    if removed_instructions + removed_plugins > 0 {
        summary.push_str(&format!(
            "; swept {removed_instructions} instruction + {removed_plugins} plugin entr{}",
            if removed_instructions + removed_plugins == 1 {
                "y"
            } else {
                "ies"
            }
        ));
    }
    if let Some(note) = sweep_note {
        summary.push_str(&format!("; {note}"));
    }
    Ok(step(CheckStatus::Green, summary))
}

/// Remove pixel entries from `opencode.json`'s `instructions` and
/// `plugin`/`plugins` arrays, writing the file only when something came
/// out. Returns (instructions removed, plugins removed, note when the file
/// could not be swept).
fn sweep_config(path: &Path, home: &Path, dry_run: bool) -> (usize, usize, Option<String>) {
    let config_dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut config: Value = match fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(v) if v.is_object() => v,
            _ => {
                return (
                    0,
                    0,
                    Some(format!(
                        "{} left untouched (not strict JSON)",
                        path.display()
                    )),
                );
            }
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => return (0, 0, None),
        Err(e) => {
            return (
                0,
                0,
                Some(format!("{} left untouched ({e})", path.display())),
            );
        }
    };
    let object = config.as_object_mut().expect("filtered to objects above");
    let mut removed_instructions = 0;
    if let Some(slot) = object.get_mut("instructions").and_then(Value::as_array_mut) {
        let before = slot.len();
        slot.retain(|v| !v.as_str().is_some_and(is_pixel_instruction));
        removed_instructions = before - slot.len();
        if slot.is_empty() {
            object.remove("instructions");
        }
    }
    let mut removed_plugins = 0;
    for key in ["plugin", "plugins"] {
        if let Some(slot) = object.get_mut(key).and_then(Value::as_array_mut) {
            let before = slot.len();
            slot.retain(|v| {
                !v.as_str()
                    .is_some_and(|s| is_stale_pixel_plugin(s, config_dir, home))
            });
            removed_plugins += before - slot.len();
        }
    }
    if removed_instructions + removed_plugins > 0
        && !dry_run
        && let Err(e) = install::write_settings(path, &config, false)
    {
        return (
            0,
            0,
            Some(format!("sweep of {} failed: {e}", path.display())),
        );
    }
    (removed_instructions, removed_plugins, None)
}

/// `pixel uninstall` step: take the managed block out of `AGENTS.md` (the
/// file goes when nothing else was in it), and drop the earlier mechanism's
/// `instructions` entries.
pub(crate) fn remove_opencode(
    config_dir: &Path,
    home: &Path,
    dry_run: bool,
) -> Result<InstallStep> {
    let agents = agents_md_path(config_dir);
    let step = |status, summary: String| InstallStep {
        id: "opencode-agents-md".into(),
        status,
        summary,
        detail: Some(format!("agents={}", agents.display())),
    };
    let mut removed = Vec::new();
    if agents.is_file() {
        let content = fs::read_to_string(&agents)?;
        let stripped = config::strip_managed_block(&content);
        if stripped != content {
            if dry_run {
                return Ok(step(
                    CheckStatus::Green,
                    install::dry_run_summary(true, "would strip the AGENTS.md managed block"),
                ));
            }
            if stripped.trim().is_empty() {
                fs::remove_file(&agents)?;
                removed.push(format!("{} (block was the whole file)", agents.display()));
            } else {
                install::write_atomically(&agents, &stripped)?;
                removed.push(format!("block from {}", agents.display()));
            }
        }
    }
    let (swept, plugins, _) = sweep_config(&config_path(config_dir), home, dry_run);
    if swept + plugins > 0 {
        removed.push(format!("{swept} instruction + {plugins} plugin entries"));
    }
    let summary = if removed.is_empty() {
        "no pixel OpenCode config — nothing to remove".to_string()
    } else {
        format!("removed {}", removed.join(" and "))
    };
    Ok(step(CheckStatus::Green, summary))
}

/// `pixel doctor` check: `AGENTS.md` carries the current managed block.
/// Green-skips when OpenCode has no config directory — the install step is
/// gated the same way.
pub(crate) fn check_opencode(config_dir: &Path) -> std::result::Result<(String, Value), String> {
    let agents = agents_md_path(config_dir);
    let detail = serde_json::json!({ "path": agents.display().to_string() });
    if !config_dir.is_dir() {
        return Ok((
            "OpenCode config directory not present — skipping".into(),
            detail,
        ));
    }
    let content = fs::read_to_string(&agents)
        .map_err(|_| format!("no {} — run `pixel install`", agents.display()))?;
    if content != config::apply_managed_markers(&content, install::AGENT_PROMPT_ASSET) {
        return Err(format!(
            "{} is stale — run `pixel install` to update",
            agents.display()
        ));
    }
    Ok((
        format!(
            "AGENTS.md carries the agent prompt ({} bytes)",
            content.len()
        ),
        detail,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unique temp dir that removes itself when the test ends — `scratch`
    /// without this left every fixture under `/tmp` forever.
    struct Scratch(PathBuf);

    impl std::ops::Deref for Scratch {
        type Target = Path;

        fn deref(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn scratch(name: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!(
            "pixel-opencode-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }

    fn config_dir(home: &Path) -> PathBuf {
        opencode_config_dir(home, true)
    }

    #[test]
    fn install_creates_a_block_only_agents_md_and_verifies_on_rerun() {
        let home = scratch("fresh");
        let dir = config_dir(&home);
        fs::create_dir_all(&dir).unwrap();
        let step = install_opencode(&dir, &home, false).unwrap();
        assert_eq!(step.status, CheckStatus::Green);
        let content = fs::read_to_string(dir.join(AGENTS_MD_FILE)).unwrap();
        assert!(content.contains(config::MANAGED_BEGIN));
        assert!(content.contains(install::AGENT_PROMPT_ASSET));
        // Re-run: the block is verified, not duplicated.
        let step = install_opencode(&dir, &home, false).unwrap();
        assert!(step.summary.contains("verified"), "{}", step.summary);
        assert_eq!(
            fs::read_to_string(dir.join(AGENTS_MD_FILE))
                .unwrap()
                .matches(config::MANAGED_BEGIN)
                .count(),
            1
        );
    }

    #[test]
    fn a_new_agents_md_seeds_the_claude_md_it_would_shadow() {
        let home = scratch("shadow");
        let dir = config_dir(&home);
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(home.join(".claude")).unwrap();
        fs::write(home.join(".claude/CLAUDE.md"), "my global rules\n").unwrap();
        install_opencode(&dir, &home, false).unwrap();
        let content = fs::read_to_string(dir.join(AGENTS_MD_FILE)).unwrap();
        assert!(content.contains("my global rules"), "{content}");
        assert!(content.contains(install::AGENT_PROMPT_ASSET));
        assert!(
            content.find("my global rules").unwrap() < content.find(config::MANAGED_BEGIN).unwrap()
        );
    }

    #[test]
    fn an_existing_agents_md_keeps_user_content_and_refreshes_the_block() {
        let home = scratch("existing");
        let dir = config_dir(&home);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(AGENTS_MD_FILE),
            format!(
                "before\n\n{}\nold prompt\n{}\nafter\n",
                config::MANAGED_BEGIN,
                config::MANAGED_END
            ),
        )
        .unwrap();
        install_opencode(&dir, &home, false).unwrap();
        let content = fs::read_to_string(dir.join(AGENTS_MD_FILE)).unwrap();
        assert!(content.contains("before") && content.contains("after"));
        assert!(content.contains(install::AGENT_PROMPT_ASSET));
        assert!(!content.contains("old prompt"));
    }

    #[test]
    fn install_sweeps_stale_instructions_and_plugin_entries() {
        let home = scratch("sweep");
        let dir = config_dir(&home);
        fs::create_dir_all(&dir).unwrap();
        let real_plugin = dir.join("exists/pixel.mjs");
        fs::create_dir_all(real_plugin.parent().unwrap()).unwrap();
        fs::write(&real_plugin, "// exists\n").unwrap();
        // Missing config entirely: no sweep, and no "untouched" note either.
        let step = install_opencode(&dir, &home, false).unwrap();
        assert!(!step.summary.contains("untouched"), "{}", step.summary);
        fs::write(
            dir.join(OPENCODE_CONFIG_FILE),
            serde_json::to_string_pretty(&serde_json::json!({
                "model": "m",
                "instructions": [
                    "CONTRIBUTING.md",
                    format!("{}/.local/share/pixel/agent-prompt.md", home.display()),
                    // Contains the tail but does not end with it: a backup
                    // the user keeps, not our entry.
                    format!("{}/.local/share/pixel/agent-prompt.md.bak", home.display())
                ],
                "plugin": [
                    "~/missing/pixel.mjs",
                    "./plugins/caveman/plugin.js",
                    real_plugin.display().to_string()
                ],
                "plugins": ["other/plugin.ts", "./gone/pixel.mjs"]
            }))
            .unwrap(),
        )
        .unwrap();
        let step = install_opencode(&dir, &home, false).unwrap();
        assert!(
            step.summary
                .contains("swept 1 instruction + 2 plugin entries"),
            "exact per-kind counts land in the summary: {}",
            step.summary
        );
        let config: Value =
            serde_json::from_str(&fs::read_to_string(dir.join(OPENCODE_CONFIG_FILE)).unwrap())
                .unwrap();
        assert_eq!(
            config["instructions"],
            serde_json::json!([
                "CONTRIBUTING.md",
                format!("{}/.local/share/pixel/agent-prompt.md.bak", home.display())
            ])
        );
        let plugin = config["plugin"].as_array().unwrap();
        assert_eq!(plugin.len(), 2, "{plugin:?}");
        assert!(
            plugin
                .iter()
                .any(|v| v.as_str() == Some("./plugins/caveman/plugin.js"))
        );
        assert!(
            plugin.iter().any(|v| v.as_str() == real_plugin.to_str()),
            "an existing pixel.mjs is left alone: {plugin:?}"
        );
        assert_eq!(config["plugins"], serde_json::json!(["other/plugin.ts"]));
        assert_eq!(config["model"], "m");
    }

    #[test]
    fn an_unparseable_config_does_not_block_the_agents_md() {
        let home = scratch("unparseable");
        let dir = config_dir(&home);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(OPENCODE_CONFIG_FILE), "{ // jsonc\n}").unwrap();
        let step = install_opencode(&dir, &home, false).unwrap();
        assert_eq!(step.status, CheckStatus::Green, "{}", step.summary);
        assert!(step.summary.contains("untouched"), "{}", step.summary);
        assert!(dir.join(AGENTS_MD_FILE).is_file());
        assert_eq!(
            fs::read_to_string(dir.join(OPENCODE_CONFIG_FILE)).unwrap(),
            "{ // jsonc\n}"
        );
        // Valid JSON of the wrong shape is skipped the same way.
        fs::write(dir.join(OPENCODE_CONFIG_FILE), "[1]").unwrap();
        let step = install_opencode(&dir, &home, false).unwrap();
        assert_eq!(step.status, CheckStatus::Green, "{}", step.summary);
        assert!(step.summary.contains("untouched"), "{}", step.summary);
    }

    #[test]
    fn an_unreadable_config_does_not_block_the_agents_md() {
        let home = scratch("unreadable-cfg");
        let dir = config_dir(&home);
        fs::create_dir_all(&dir).unwrap();
        // A directory where a file is expected fails the read on every
        // platform and for every user — including root, which a 0o000 mode
        // would not stop.
        fs::create_dir(dir.join(OPENCODE_CONFIG_FILE)).unwrap();
        // Not "absent": the sweep reports the file untouched rather than
        // silently treating it as empty.
        let step = install_opencode(&dir, &home, false).unwrap();
        assert_eq!(step.status, CheckStatus::Green, "{}", step.summary);
        assert!(step.summary.contains("untouched"), "{}", step.summary);
        assert!(dir.join(AGENTS_MD_FILE).is_file());
    }

    #[test]
    fn an_unreadable_agents_md_is_an_error_not_a_replace() {
        let home = scratch("unreadable-agents");
        let dir = config_dir(&home);
        fs::create_dir_all(&dir).unwrap();
        // Non-UTF-8 bytes fail read_to_string without depending on
        // permissions — and the write path would happily replace the file,
        // which is exactly what must not happen.
        let agents = dir.join(AGENTS_MD_FILE);
        fs::write(&agents, b"\xff\xfe\x00").unwrap();
        assert!(install_opencode(&dir, &home, false).is_err());
        assert_eq!(fs::read(&agents).unwrap(), b"\xff\xfe\x00");
    }

    #[test]
    fn an_unreadable_claude_seed_is_an_error_not_an_empty_file() {
        let home = scratch("unreadable-seed");
        let dir = config_dir(&home);
        fs::create_dir_all(&dir).unwrap();
        let claude_dir = home.join(".claude");
        fs::create_dir_all(&claude_dir).unwrap();
        fs::create_dir(claude_dir.join("CLAUDE.md")).unwrap();
        // Same rule as AGENTS.md: a seed that exists but cannot be read is
        // surfaced, never silently replaced with an empty file.
        assert!(install_opencode(&dir, &home, false).is_err());
    }

    #[test]
    fn a_config_with_nothing_to_sweep_is_not_rewritten() {
        let home = scratch("clean");
        let dir = config_dir(&home);
        fs::create_dir_all(&dir).unwrap();
        // Deliberately non-pretty formatting: a rewrite would reflow it.
        fs::write(
            dir.join(OPENCODE_CONFIG_FILE),
            "{\"model\":\"m\",\"plugin\":[\"x.js\"]}",
        )
        .unwrap();
        let step = install_opencode(&dir, &home, false).unwrap();
        assert!(!step.summary.contains("swept"), "{}", step.summary);
        assert_eq!(
            fs::read_to_string(dir.join(OPENCODE_CONFIG_FILE)).unwrap(),
            "{\"model\":\"m\",\"plugin\":[\"x.js\"]}"
        );
    }

    #[test]
    fn sweep_counts_each_kind_independently() {
        let home = scratch("sweep-one-kind");
        let dir = config_dir(&home);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(OPENCODE_CONFIG_FILE),
            serde_json::to_string_pretty(&serde_json::json!({
                "instructions": [
                    format!("{}/.local/share/pixel/agent-prompt.md", home.display())
                ]
            }))
            .unwrap(),
        )
        .unwrap();
        let step = install_opencode(&dir, &home, false).unwrap();
        assert!(
            step.summary
                .contains("swept 1 instruction + 0 plugin entry"),
            "a single swept entry stays singular: {}",
            step.summary
        );
    }

    #[test]
    fn dry_run_writes_nothing() {
        let home = scratch("dry-run");
        let dir = config_dir(&home);
        fs::create_dir_all(&dir).unwrap();
        let step = install_opencode(&dir, &home, true).unwrap();
        assert_eq!(step.status, CheckStatus::Green);
        assert!(step.summary.contains("dry-run"));
        assert!(!dir.join(AGENTS_MD_FILE).exists());
    }

    #[test]
    fn remove_strips_the_block_and_deletes_a_block_only_file() {
        let home = scratch("remove");
        let dir = config_dir(&home);
        fs::create_dir_all(&dir).unwrap();
        install_opencode(&dir, &home, false).unwrap();
        // File holding only the block: removed entirely.
        let step = remove_opencode(&dir, &home, false).unwrap();
        assert_eq!(step.status, CheckStatus::Green, "{}", step.summary);
        assert!(!dir.join(AGENTS_MD_FILE).exists());
        // File holding user text + block: text survives.
        fs::write(
            dir.join(AGENTS_MD_FILE),
            format!(
                "mine\n{}\nprompt\n{}\n",
                config::MANAGED_BEGIN,
                config::MANAGED_END
            ),
        )
        .unwrap();
        let step = remove_opencode(&dir, &home, false).unwrap();
        assert_eq!(step.status, CheckStatus::Green, "{}", step.summary);
        assert_eq!(
            fs::read_to_string(dir.join(AGENTS_MD_FILE)).unwrap(),
            "mine\n"
        );
        // Nothing left: green no-op.
        let step = remove_opencode(&dir, &home, false).unwrap();
        assert!(
            step.summary.contains("nothing to remove"),
            "{}",
            step.summary
        );
    }

    #[test]
    fn remove_also_drops_leftover_instructions_entries() {
        let home = scratch("remove-instructions");
        let dir = config_dir(&home);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(OPENCODE_CONFIG_FILE),
            serde_json::to_string_pretty(&serde_json::json!({
                "instructions": [
                    format!("{}/.local/share/pixel/agent-prompt.md", home.display())
                ]
            }))
            .unwrap(),
        )
        .unwrap();
        let step = remove_opencode(&dir, &home, false).unwrap();
        assert!(
            step.summary.contains("1 instruction + 0 plugin entries"),
            "{}",
            step.summary
        );
        let config: Value =
            serde_json::from_str(&fs::read_to_string(dir.join(OPENCODE_CONFIG_FILE)).unwrap())
                .unwrap();
        assert!(config.get("instructions").is_none(), "{config}");

        // Plugin-only removal reports its own count.
        fs::write(
            dir.join(OPENCODE_CONFIG_FILE),
            serde_json::to_string_pretty(&serde_json::json!({
                "plugin": ["~/nowhere/pixel.mjs"]
            }))
            .unwrap(),
        )
        .unwrap();
        let step = remove_opencode(&dir, &home, false).unwrap();
        assert!(
            step.summary.contains("0 instruction + 1 plugin entries"),
            "{}",
            step.summary
        );
    }

    #[test]
    fn config_dir_honours_xdg_only_on_a_real_install() {
        let home = Path::new("/home/u");
        let xdg = Some(std::ffi::OsString::from("/xdg"));
        assert_eq!(
            resolve_config_dir(home, false, xdg.clone()),
            PathBuf::from("/xdg/opencode")
        );
        // An explicit --home is a test fixture, not the user's machine:
        // XDG must not leak into it.
        assert_eq!(
            resolve_config_dir(home, true, xdg),
            home.join(".config/opencode")
        );
        assert_eq!(
            resolve_config_dir(home, false, None),
            home.join(".config/opencode")
        );
        // An empty XDG variable is unset, not a root-relative path.
        assert_eq!(
            resolve_config_dir(home, false, Some(std::ffi::OsString::new())),
            home.join(".config/opencode")
        );
    }

    #[test]
    fn check_skips_absent_opencode_and_verifies_the_block() {
        let missing = Path::new("/definitely/not/here/opencode");
        let (summary, _) = check_opencode(missing).unwrap();
        assert!(summary.contains("skipping"), "{summary}");

        let home = scratch("check");
        let dir = config_dir(&home);
        fs::create_dir_all(&dir).unwrap();
        assert!(check_opencode(&dir).is_err());
        install_opencode(&dir, &home, false).unwrap();
        let (summary, _) = check_opencode(&dir).unwrap();
        assert!(summary.contains("agent prompt"), "{summary}");
        // A stale block is reported, not greened.
        let agents = dir.join(AGENTS_MD_FILE);
        let content = fs::read_to_string(&agents).unwrap();
        fs::write(&agents, content.replace("pixel", "stale")).unwrap();
        assert!(check_opencode(&dir).is_err());
    }
}

//! OpenCode integration through `~/.config/opencode/opencode.json`.
//!
//! OpenCode merges the `instructions` array of its global config into every
//! session's context, alongside whatever AGENTS.md files apply. Adding the
//! deployed `agent-prompt.md` path there is the OpenCode analogue of Codex's
//! `developer_instructions`: the Pixel retrieval protocol reaches every
//! session without a plugin, a wrapper, or a forked system prompt.
//!
//! Why `instructions` and not `~/.config/opencode/AGENTS.md`: in OpenCode's
//! precedence the global AGENTS.md *replaces* the `~/.claude/CLAUDE.md`
//! fallback — creating it would hide every global rule a migrating Claude
//! Code user already keeps. `instructions` is additive and shadows nothing.
//!
//! The file is rewritten only through `serde_json`: any other top-level key
//! (model, provider, mcp, tui…) round-trips untouched, a parse failure is a
//! red step rather than a rewrite — OpenCode tolerates JSONC comments that
//! this crate cannot round-trip — and a pre-existing `opencode.jsonc` is
//! left alone, since OpenCode loads `opencode.json` alongside it.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::install::{self, CheckStatus, InstallStep, Result};

/// `opencode.json`, relative to the OpenCode config directory.
pub const OPENCODE_CONFIG_FILE: &str = "opencode.json";

/// `instructions` — the merged instruction-file list.
pub const INSTRUCTIONS_KEY: &str = "instructions";

/// Path tail identifying the deployed prompt inside an `instructions`
/// entry. Matching on the tail — not the full path — lets uninstall and
/// doctor recognise entries written under a different home directory.
const PROMPT_PATH_TAIL: &str = ".local/share/pixel/agent-prompt.md";

/// The OpenCode config directory: `$XDG_CONFIG_HOME/opencode` on a real
/// install (the variable OpenCode itself honours), `<home>/.config/opencode`
/// for tests and explicit `--home` overrides.
pub(crate) fn opencode_config_dir(home: &Path, home_was_explicit: bool) -> PathBuf {
    if !home_was_explicit
        && let Some(dir) = std::env::var_os("XDG_CONFIG_HOME")
        && !dir.is_empty()
    {
        return PathBuf::from(dir).join("opencode");
    }
    home.join(".config").join("opencode")
}

fn config_path(config_dir: &Path) -> PathBuf {
    config_dir.join(OPENCODE_CONFIG_FILE)
}

/// True when an `instructions` entry names the deployed Pixel prompt,
/// wherever it was installed from.
fn is_pixel_instruction(entry: &str) -> bool {
    entry.replace('\\', "/").contains(PROMPT_PATH_TAIL)
}

/// Read the config file. `Ok(None)` when absent; `Err` when present but not
/// parseable as strict JSON or not an object — both are user-owned states
/// the installer must refuse to rewrite.
fn read_config(path: &Path) -> std::result::Result<Option<Value>, String> {
    match fs::read_to_string(path) {
        Ok(text) => {
            let value: Value = serde_json::from_str(&text).map_err(|e| {
                format!(
                    "{} does not parse as strict JSON: {e} (comments? \
                     opencode.jsonc is honoured alongside, so add the entry \
                     there) — not touched",
                    path.display()
                )
            })?;
            if value.is_object() {
                Ok(Some(value))
            } else {
                Err(format!(
                    "{} is not a JSON object — not touched",
                    path.display()
                ))
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

/// The `instructions` array as a mutable slot inside `config`, or `Err`
/// when the key exists but is not an array — a malformed shape the
/// installer must not silently replace.
fn instructions_slot(config: &mut Value) -> std::result::Result<&mut Vec<Value>, String> {
    let object = config
        .as_object_mut()
        .expect("read_config rejects non-objects");
    let entry = object
        .entry(INSTRUCTIONS_KEY)
        .or_insert_with(|| Value::Array(Vec::new()));
    entry
        .as_array_mut()
        .ok_or_else(|| format!("`{INSTRUCTIONS_KEY}` is not an array — not touched"))
}

/// `pixel install` step: register the deployed agent prompt in OpenCode's
/// global `instructions`, idempotently alongside whatever the user already
/// instructs.
pub(crate) fn install_instructions(
    config_dir: &Path,
    prompt_path: &Path,
    dry_run: bool,
) -> Result<InstallStep> {
    let path = config_path(config_dir);
    let detail = Some(format!(
        "path={} key={INSTRUCTIONS_KEY} tail={PROMPT_PATH_TAIL}",
        path.display()
    ));
    let step = |status, summary: String| InstallStep {
        id: "opencode-instructions".into(),
        status,
        summary,
        detail: detail.clone(),
    };
    let mut config = match read_config(&path) {
        Ok(c) => c.unwrap_or_else(|| serde_json::json!({})),
        Err(e) => return Ok(step(CheckStatus::Red, e)),
    };
    let prompt = prompt_path.display().to_string();
    let already = {
        let slot = match instructions_slot(&mut config) {
            Ok(s) => s,
            Err(e) => return Ok(step(CheckStatus::Red, format!("{}: {e}", path.display()))),
        };
        if slot
            .iter()
            .any(|v| v.as_str().is_some_and(is_pixel_instruction))
        {
            true
        } else {
            slot.push(Value::String(prompt));
            false
        }
    };
    if already {
        return Ok(step(
            CheckStatus::Green,
            format!("verified pixel instructions in {}", path.display()),
        ));
    }
    let summary = format!(
        "{} pixel agent prompt in {}",
        if path.is_file() {
            "updated"
        } else {
            "installed"
        },
        path.display()
    );
    if dry_run {
        return Ok(step(
            CheckStatus::Green,
            install::dry_run_summary(true, &summary),
        ));
    }
    let backup = install::write_settings(&path, &config, false)?;
    Ok(step(
        CheckStatus::Green,
        install::with_backup_note(summary, backup),
    ))
}

/// `pixel uninstall` step: drop every `instructions` entry that names the
/// deployed prompt; the key itself is removed once it holds nothing.
pub(crate) fn remove_instructions(config_dir: &Path, dry_run: bool) -> Result<InstallStep> {
    let path = config_path(config_dir);
    let detail = Some(format!(
        "path={} key={INSTRUCTIONS_KEY} tail={PROMPT_PATH_TAIL}",
        path.display()
    ));
    let step = |status, summary: String| InstallStep {
        id: "opencode-instructions".into(),
        status,
        summary,
        detail: detail.clone(),
    };
    let mut config = match read_config(&path) {
        Ok(Some(c)) => c,
        Ok(None) => {
            return Ok(step(
                CheckStatus::Green,
                format!("no {} — nothing to remove", path.display()),
            ));
        }
        Err(e) => return Ok(step(CheckStatus::Red, e)),
    };
    let removed = {
        let slot = match instructions_slot(&mut config) {
            Ok(s) => s,
            Err(e) => return Ok(step(CheckStatus::Red, format!("{}: {e}", path.display()))),
        };
        let before = slot.len();
        slot.retain(|v| !v.as_str().is_some_and(is_pixel_instruction));
        let removed = before - slot.len();
        if slot.is_empty()
            && let Some(object) = config.as_object_mut()
        {
            object.remove(INSTRUCTIONS_KEY);
        }
        removed
    };
    if removed == 0 {
        return Ok(step(
            CheckStatus::Green,
            format!(
                "no pixel instruction in {} — nothing to remove",
                path.display()
            ),
        ));
    }
    let summary = format!(
        "removed {removed} pixel instruction entr{} from {}",
        if removed == 1 { "y" } else { "ies" },
        path.display()
    );
    if dry_run {
        return Ok(step(
            CheckStatus::Green,
            install::dry_run_summary(true, &summary),
        ));
    }
    let backup = install::write_settings(&path, &config, false)?;
    Ok(step(
        CheckStatus::Green,
        install::with_backup_note(summary, backup),
    ))
}

/// `pixel doctor` check: the deployed prompt is registered in the global
/// `instructions`. Green-skips when OpenCode has no config directory — the
/// install step is gated the same way.
pub(crate) fn check_instructions(
    config_dir: &Path,
) -> std::result::Result<(String, Value), String> {
    let path = config_path(config_dir);
    let detail = serde_json::json!({
        "path": path.display().to_string(),
        "key": INSTRUCTIONS_KEY,
        "tail": PROMPT_PATH_TAIL,
    });
    if !config_dir.is_dir() {
        return Ok((
            "OpenCode config directory not present — skipping".into(),
            detail,
        ));
    }
    let registered = match read_config(&path)? {
        Some(config) => config
            .get(INSTRUCTIONS_KEY)
            .and_then(Value::as_array)
            .is_some_and(|slot| {
                slot.iter()
                    .any(|v| v.as_str().is_some_and(is_pixel_instruction))
            }),
        None => false,
    };
    if !registered {
        return Err(format!(
            "no pixel instruction in {} — run `pixel install`",
            path.display()
        ));
    }
    Ok((
        format!("pixel agent prompt registered in {}", path.display()),
        detail,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "pixel-opencode-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn prompt(home: &Path) -> PathBuf {
        home.join(".local/share/pixel/agent-prompt.md")
    }

    #[test]
    fn install_creates_instructions_and_verifies_on_rerun() {
        let dir = scratch("install");
        let prompt = prompt(&dir);
        let step = install_instructions(&dir, &prompt, false).unwrap();
        assert_eq!(step.status, CheckStatus::Green);
        let config: Value =
            serde_json::from_str(&fs::read_to_string(dir.join(OPENCODE_CONFIG_FILE)).unwrap())
                .unwrap();
        assert_eq!(
            config[INSTRUCTIONS_KEY],
            serde_json::json!([prompt.display().to_string()])
        );
        let step = install_instructions(&dir, &prompt, false).unwrap();
        assert!(step.summary.contains("verified"), "{}", step.summary);
        // A second run must not duplicate the entry.
        let config: Value =
            serde_json::from_str(&fs::read_to_string(dir.join(OPENCODE_CONFIG_FILE)).unwrap())
                .unwrap();
        assert_eq!(config[INSTRUCTIONS_KEY].as_array().unwrap().len(), 1);
    }

    #[test]
    fn install_preserves_foreign_keys_and_instructions() {
        let dir = scratch("preserve");
        fs::write(
            dir.join(OPENCODE_CONFIG_FILE),
            serde_json::to_string_pretty(&serde_json::json!({
                "$schema": "https://opencode.ai/config.json",
                "model": "anthropic/claude-sonnet-4-5",
                "instructions": ["CONTRIBUTING.md", "docs/*.md"],
                "mcp": {"jira": {"type": "remote", "url": "https://x"}}
            }))
            .unwrap(),
        )
        .unwrap();
        install_instructions(&dir, &prompt(&dir), false).unwrap();
        let config: Value =
            serde_json::from_str(&fs::read_to_string(dir.join(OPENCODE_CONFIG_FILE)).unwrap())
                .unwrap();
        let slot = config[INSTRUCTIONS_KEY].as_array().unwrap();
        assert_eq!(slot.len(), 3);
        assert_eq!(slot[0], "CONTRIBUTING.md");
        assert_eq!(slot[1], "docs/*.md");
        assert!(slot[2].as_str().unwrap().contains(PROMPT_PATH_TAIL));
        assert_eq!(config["model"], "anthropic/claude-sonnet-4-5");
        assert!(config["mcp"]["jira"].is_object());
    }

    #[test]
    fn install_refuses_unparseable_and_malformed_configs() {
        let dir = scratch("refuse");
        // JSONC comments parse for OpenCode but not for serde_json.
        fs::write(dir.join(OPENCODE_CONFIG_FILE), "{ // mine\n\"model\": 1\n}").unwrap();
        let step = install_instructions(&dir, &prompt(&dir), false).unwrap();
        assert_eq!(step.status, CheckStatus::Red);
        // Non-object root.
        fs::write(dir.join(OPENCODE_CONFIG_FILE), "[1,2]").unwrap();
        let step = install_instructions(&dir, &prompt(&dir), false).unwrap();
        assert_eq!(step.status, CheckStatus::Red);
        // `instructions` of the wrong shape.
        fs::write(
            dir.join(OPENCODE_CONFIG_FILE),
            r#"{"instructions": "AGENTS.md"}"#,
        )
        .unwrap();
        let step = install_instructions(&dir, &prompt(&dir), false).unwrap();
        assert_eq!(step.status, CheckStatus::Red, "{}", step.summary);
        assert_eq!(
            fs::read_to_string(dir.join(OPENCODE_CONFIG_FILE)).unwrap(),
            r#"{"instructions": "AGENTS.md"}"#
        );
    }

    #[cfg(unix)]
    #[test]
    fn install_refuses_a_config_it_cannot_read() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("unreadable");
        let path = dir.join(OPENCODE_CONFIG_FILE);
        fs::write(&path, "{}").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        // Permission denied is not "absent": the step must go red and leave
        // the file alone, not treat the config as empty and overwrite it.
        let step = install_instructions(&dir, &prompt(&dir), false).unwrap();
        assert_eq!(step.status, CheckStatus::Red, "{}", step.summary);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "{}");
    }

    #[test]
    fn dry_run_writes_nothing() {
        let dir = scratch("dry-run");
        let step = install_instructions(&dir, &prompt(&dir), true).unwrap();
        assert_eq!(step.status, CheckStatus::Green);
        assert!(step.summary.contains("dry-run"));
        assert!(!dir.join(OPENCODE_CONFIG_FILE).exists());
    }

    #[test]
    fn remove_strips_only_pixel_entries_and_drops_empty_key() {
        let dir = scratch("remove");
        install_instructions(&dir, &prompt(&dir), false).unwrap();
        let other_home = Path::new("/other/home");
        // An entry written from another home must also come out.
        let path = dir.join(OPENCODE_CONFIG_FILE);
        let mut config: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        config[INSTRUCTIONS_KEY]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!(prompt(other_home).display().to_string()));
        config[INSTRUCTIONS_KEY]
            .as_array_mut()
            .unwrap()
            .insert(0, serde_json::json!("mine.md"));
        fs::write(&path, serde_json::to_string_pretty(&config).unwrap()).unwrap();

        let step = remove_instructions(&dir, false).unwrap();
        assert_eq!(step.status, CheckStatus::Green, "{}", step.summary);
        let config: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(config[INSTRUCTIONS_KEY], serde_json::json!(["mine.md"]));

        // Removing again leaves the user's entry and is a no-op.
        let step = remove_instructions(&dir, false).unwrap();
        assert!(step.summary.contains("nothing to remove"));
        let config: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(config[INSTRUCTIONS_KEY], serde_json::json!(["mine.md"]));

        // When ours was the only entry the key itself goes.
        fs::write(
            &path,
            serde_json::to_string_pretty(&serde_json::json!({
                "instructions": [prompt(&dir).display().to_string()],
                "model": "m"
            }))
            .unwrap(),
        )
        .unwrap();
        remove_instructions(&dir, false).unwrap();
        let config: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(config.get(INSTRUCTIONS_KEY).is_none(), "{config}");
        assert_eq!(config["model"], "m");
    }

    #[test]
    fn config_dir_honours_xdg_only_on_a_real_install() {
        let home = Path::new("/home/u");
        let saved = std::env::var_os("XDG_CONFIG_HOME");
        // SAFETY: the only reader of XDG_CONFIG_HOME in this test binary is
        // opencode_config_dir, exercised serially inside this block; the
        // variable is restored on exit either way.
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", "/xdg");
            assert_eq!(
                opencode_config_dir(home, false),
                PathBuf::from("/xdg/opencode")
            );
            // An explicit --home is a test fixture, not the user's machine:
            // XDG must not leak into it.
            assert_eq!(
                opencode_config_dir(home, true),
                home.join(".config/opencode")
            );
            std::env::remove_var("XDG_CONFIG_HOME");
            assert_eq!(
                opencode_config_dir(home, false),
                home.join(".config/opencode")
            );
            match saved {
                Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
        }
    }

    #[test]
    fn check_skips_absent_opencode_and_verifies_the_entry() {
        let missing = Path::new("/definitely/not/here/opencode");
        let (summary, _) = check_instructions(missing).unwrap();
        assert!(summary.contains("skipping"), "{summary}");

        let dir = scratch("check");
        assert!(check_instructions(&dir).is_err());
        install_instructions(&dir, &prompt(&dir), false).unwrap();
        let (summary, _) = check_instructions(&dir).unwrap();
        assert!(summary.contains("registered"), "{summary}");
    }
}

//! Antigravity integration module.
//!
//! Deploys the Pixel plugin to `~/.gemini/config/plugins/pixel/`,
//! ensures the plugin is enabled in `~/.gemini/config/config.json`,
//! and installs the `pixel-guard` hooks in `~/.gemini/config/hooks.json`.

use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};

use crate::install::{CheckStatus, InstallStep, Result};

pub(crate) const AGENT_PROMPT_ASSET: &str = include_str!("../assets/pixel-agent-prompt.md");

pub(crate) fn antigravity_config_dir(home: &Path) -> PathBuf {
    home.join(".gemini/config")
}

pub(crate) fn plugin_dir(home: &Path) -> PathBuf {
    antigravity_config_dir(home).join("plugins/pixel")
}

pub(crate) fn hooks_path(home: &Path) -> PathBuf {
    antigravity_config_dir(home).join("hooks.json")
}

pub(crate) fn config_path(home: &Path) -> PathBuf {
    antigravity_config_dir(home).join("config.json")
}

/// Deploy plugin assets: `plugin.json`, `rules/AGENTS.md`, `skills/pixel/SKILL.md`, `hooks.json`.
pub fn deploy_plugin_assets(home: &Path, exe: &Path, dry_run: bool) -> Result<InstallStep> {
    let p_dir = plugin_dir(home);
    if dry_run {
        return Ok(InstallStep {
            id: "install.antigravity-plugin".into(),
            status: CheckStatus::Green,
            summary: format!("would deploy antigravity plugin to {}", p_dir.display()),
            detail: None,
        });
    }

    fs::create_dir_all(&p_dir)?;
    let rules_dir = p_dir.join("rules");
    fs::create_dir_all(&rules_dir)?;
    let skills_dir = p_dir.join("skills/pixel");
    fs::create_dir_all(&skills_dir)?;

    let plugin_manifest = json!({
        "name": "pixel",
        "displayName": "Pixel Code Intelligence",
        "description": "Deterministic AST code retrieval and code-graph navigation layer for Antigravity.",
        "version": env!("CARGO_PKG_VERSION"),
        "managedBy": "pixel"
    });
    fs::write(
        p_dir.join("plugin.json"),
        serde_json::to_string_pretty(&plugin_manifest)? + "\n",
    )?;

    // rules/AGENTS.md
    fs::write(rules_dir.join("AGENTS.md"), AGENT_PROMPT_ASSET)?;

    // skills/pixel/SKILL.md
    let skill_content = format!(
        "---\nname: pixel\ndescription: >-\n  Deterministic code retrieval: indexed search, concept resolve, impact\n  analysis, caller/callee tracing, task targets, plan generation, and git\n  history archaeology via the `pixel` CLI.\n---\n\n{AGENT_PROMPT_ASSET}"
    );
    fs::write(skills_dir.join("SKILL.md"), skill_content)?;

    // Plugin hooks.json
    let guard_cmd = format!("'{}' run-hook guard --provider antigravity", exe.display());
    let plugin_hooks = json!({
        "pixel-guard": {
            "enabled": true,
            "PreToolUse": [
                {
                    "matcher": "run_command|grep_search|find_by_name|view_file",
                    "hooks": [
                        {
                            "type": "command",
                            "command": guard_cmd,
                            "timeout": 10
                        }
                    ]
                }
            ],
            "PreInvocation": [
                {
                    "type": "command",
                    "command": guard_cmd,
                    "timeout": 10
                }
            ]
        }
    });
    fs::write(
        p_dir.join("hooks.json"),
        serde_json::to_string_pretty(&plugin_hooks)? + "\n",
    )?;

    Ok(InstallStep {
        id: "install.antigravity-plugin".into(),
        status: CheckStatus::Green,
        summary: format!("deployed antigravity plugin to {}", p_dir.display()),
        detail: Some(format!("path={}", p_dir.display())),
    })
}

/// Enable pixel plugin in `~/.gemini/config/config.json`.
pub fn enable_plugin_in_config(home: &Path, dry_run: bool) -> Result<InstallStep> {
    let cfg_file = config_path(home);
    if dry_run {
        return Ok(InstallStep {
            id: "install.antigravity-config".into(),
            status: CheckStatus::Green,
            summary: format!("would enable pixel plugin in {}", cfg_file.display()),
            detail: None,
        });
    }

    let mut root_val: Value = if cfg_file.is_file() {
        let text = fs::read_to_string(&cfg_file)?;
        serde_json::from_str(&text).unwrap_or_else(|_| json!({}))
    } else {
        if let Some(p) = cfg_file.parent() {
            fs::create_dir_all(p)?;
        }
        json!({})
    };

    let plugins_obj = root_val.as_object_mut().and_then(|obj| {
        if !obj.contains_key("plugins") {
            obj.insert("plugins".into(), json!({}));
        }
        obj.get_mut("plugins").and_then(Value::as_object_mut)
    });

    if let Some(plugins) = plugins_obj {
        plugins.insert("pixel".into(), json!({ "enabled": true }));
    }

    fs::write(&cfg_file, serde_json::to_string_pretty(&root_val)? + "\n")?;

    Ok(InstallStep {
        id: "install.antigravity-config".into(),
        status: CheckStatus::Green,
        summary: "enabled pixel plugin in config.json".into(),
        detail: Some(format!("path={}", cfg_file.display())),
    })
}

/// Add or update pixel-guard in `~/.gemini/config/hooks.json`.
pub fn install_global_hooks(home: &Path, exe: &Path, dry_run: bool) -> Result<InstallStep> {
    let h_path = hooks_path(home);
    let guard_cmd = format!("'{}' run-hook guard --provider antigravity", exe.display());

    if dry_run {
        return Ok(InstallStep {
            id: "install.antigravity-hooks".into(),
            status: CheckStatus::Green,
            summary: format!("would configure pixel-guard in {}", h_path.display()),
            detail: None,
        });
    }

    let mut root_val: Value = if h_path.is_file() {
        let text = fs::read_to_string(&h_path)?;
        serde_json::from_str(&text).unwrap_or_else(|_| json!({}))
    } else {
        if let Some(p) = h_path.parent() {
            fs::create_dir_all(p)?;
        }
        json!({})
    };

    let root_map =
        root_val
            .as_object_mut()
            .ok_or_else(|| crate::InstallError::InvalidSettings {
                path: h_path.clone(),
                reason: "hooks.json root is not an object".into(),
            })?;

    let pixel_guard_spec = json!({
        "enabled": true,
        "PreToolUse": [
            {
                "matcher": "run_command|grep_search|find_by_name|view_file",
                "hooks": [
                    {
                        "type": "command",
                        "command": guard_cmd,
                        "timeout": 10
                    }
                ]
            }
        ],
        "PreInvocation": [
            {
                "type": "command",
                "command": guard_cmd,
                "timeout": 10
            }
        ]
    });

    root_map.insert("pixel-guard".into(), pixel_guard_spec);

    fs::write(&h_path, serde_json::to_string_pretty(&root_val)? + "\n")?;

    Ok(InstallStep {
        id: "install.antigravity-hooks".into(),
        status: CheckStatus::Green,
        summary: "configured pixel-guard in hooks.json".into(),
        detail: Some(format!("path={}", h_path.display())),
    })
}

/// Check antigravity installation status for `pixel doctor`.
pub fn check_antigravity_install(
    home: &Path,
    _exe: &Path,
) -> std::result::Result<(String, Value), String> {
    let p_dir = plugin_dir(home);
    let h_path = hooks_path(home);
    let cfg_path = config_path(home);

    if !antigravity_config_dir(home).is_dir() {
        return Ok((
            "Antigravity config directory not present (~/.gemini/config) — skipping".into(),
            json!({}),
        ));
    }

    let mut missing = Vec::new();

    if !p_dir.join("plugin.json").is_file() {
        missing.push("plugin.json");
    }
    if !p_dir.join("rules/AGENTS.md").is_file() {
        missing.push("rules/AGENTS.md");
    }
    if !p_dir.join("skills/pixel/SKILL.md").is_file() {
        missing.push("skills/pixel/SKILL.md");
    }

    let plugin_enabled = if cfg_path.is_file() {
        fs::read_to_string(&cfg_path).ok().and_then(|text| {
            let v: Value = serde_json::from_str(&text).ok()?;
            v.get("plugins")?.get("pixel")?.get("enabled")?.as_bool()
        }) == Some(true)
    } else {
        false
    };
    if !plugin_enabled {
        missing.push("config.json (pixel plugin enabled)");
    }

    let hooks_installed = if h_path.is_file() {
        fs::read_to_string(&h_path).ok().and_then(|text| {
            let v: Value = serde_json::from_str(&text).ok()?;
            let guard = v.get("pixel-guard")?;
            let pre_tool = guard.get("PreToolUse")?.as_array()?;
            let first_group = pre_tool.first()?;
            let cmd = first_group
                .get("hooks")?
                .as_array()?
                .first()?
                .get("command")?
                .as_str()?;
            Some(cmd.contains("run-hook guard --provider antigravity"))
        }) == Some(true)
    } else {
        false
    };
    if !hooks_installed {
        missing.push("hooks.json (pixel-guard configured)");
    }

    if !missing.is_empty() {
        return Err(format!(
            "Antigravity integration incomplete: missing {} — run `pixel install`",
            missing.join(", ")
        ));
    }

    Ok((
        "Antigravity plugin, hooks, and configuration active".into(),
        json!({
            "plugin_dir": p_dir.display().to_string(),
            "hooks_path": h_path.display().to_string(),
            "config_path": cfg_path.display().to_string(),
        }),
    ))
}

/// Remove Antigravity integration during `pixel uninstall`.
pub fn remove_antigravity(home: &Path, dry_run: bool) -> Result<InstallStep> {
    let p_dir = plugin_dir(home);
    let h_path = hooks_path(home);
    let cfg_path = config_path(home);

    if dry_run {
        return Ok(InstallStep {
            id: "uninstall.antigravity".into(),
            status: CheckStatus::Green,
            summary: "would remove Antigravity plugin and hooks".into(),
            detail: None,
        });
    }

    let mut removed_items = Vec::new();

    if p_dir.is_dir() {
        let _ = fs::remove_dir_all(&p_dir);
        removed_items.push("plugin directory");
    }

    if h_path.is_file()
        && let Ok(text) = fs::read_to_string(&h_path)
            && let Ok(mut v) = serde_json::from_str::<Value>(&text)
            && let Some(obj) = v.as_object_mut()
                && obj.remove("pixel-guard").is_some() {
                    let _ = fs::write(
                        &h_path,
                        serde_json::to_string_pretty(&v).unwrap_or_default() + "\n",
                    );
                    removed_items.push("hooks.json entry");
                }

    if cfg_path.is_file()
        && let Ok(text) = fs::read_to_string(&cfg_path)
            && let Ok(mut v) = serde_json::from_str::<Value>(&text)
            && let Some(plugins) = v.get_mut("plugins").and_then(Value::as_object_mut)
                && plugins.remove("pixel").is_some() {
                    let _ = fs::write(
                        &cfg_path,
                        serde_json::to_string_pretty(&v).unwrap_or_default() + "\n",
                    );
                    removed_items.push("config.json entry");
                }

    let summary = if removed_items.is_empty() {
        "no Antigravity integration found to remove".into()
    } else {
        format!("removed Antigravity: {}", removed_items.join(", "))
    };

    Ok(InstallStep {
        id: "uninstall.antigravity".into(),
        status: CheckStatus::Green,
        summary,
        detail: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_antigravity_deploy_and_check() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        let exe = PathBuf::from("/usr/local/bin/pixel");

        // Before directory exists -> doctor skips cleanly
        let (summary, _) = check_antigravity_install(home, &exe).unwrap();
        assert!(summary.contains("skipping"));

        // Create ~/.gemini/config
        fs::create_dir_all(antigravity_config_dir(home)).unwrap();

        // Doctor should fail because nothing is installed yet
        let err = check_antigravity_install(home, &exe).unwrap_err();
        assert!(err.contains("Antigravity integration incomplete"));

        // Deploy plugin
        let step1 = deploy_plugin_assets(home, &exe, false).unwrap();
        assert_eq!(step1.status, CheckStatus::Green);
        assert!(plugin_dir(home).join("plugin.json").is_file());
        assert!(plugin_dir(home).join("rules/AGENTS.md").is_file());
        assert!(plugin_dir(home).join("skills/pixel/SKILL.md").is_file());
        assert!(plugin_dir(home).join("hooks.json").is_file());

        // Enable in config
        let step2 = enable_plugin_in_config(home, false).unwrap();
        assert_eq!(step2.status, CheckStatus::Green);

        // Install hooks
        let step3 = install_global_hooks(home, &exe, false).unwrap();
        assert_eq!(step3.status, CheckStatus::Green);

        // Doctor check should now succeed
        let (summary, detail) = check_antigravity_install(home, &exe).unwrap();
        assert!(summary.contains("Antigravity plugin, hooks, and configuration active"));
        assert!(detail.get("plugin_dir").is_some());

        // Uninstall
        let step4 = remove_antigravity(home, false).unwrap();
        assert_eq!(step4.status, CheckStatus::Green);
        assert!(!plugin_dir(home).exists());

        // Verify hooks removed
        let h_text = fs::read_to_string(hooks_path(home)).unwrap();
        let h_val: Value = serde_json::from_str(&h_text).unwrap();
        assert!(h_val.get("pixel-guard").is_none());
    }
}

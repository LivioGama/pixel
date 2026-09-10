//! Provider-specific hook configuration. Installation is not proof that a
//! harness has fired the hooks; doctor reports that boundary separately.

use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::{Map, Value, json};

use crate::{InstallError, config, install};

pub(crate) const RTK_BACKUP: &str = ".claude/pixel-rtk-hooks.json";
/// Project-local snapshot of Codex `PreToolUse` groups adopted by the composed
/// guard.  It deliberately lives next to the project hook config so a runtime
/// never has to discover or execute the currently mutable hook configuration.
pub(crate) const CODEX_COMPOSED_BACKUP: &str = "pixel-composed-guard-backup.json";
#[allow(dead_code)]
const CODEX_COMPOSED_BACKUP_VERSION: u64 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Provider {
    Claude,
    Codex,
    Devin,
}

impl Provider {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Devin => "devin",
        }
    }

    pub(crate) fn path(self, home: &Path) -> PathBuf {
        home.join(match self {
            Self::Claude => ".claude/settings.json",
            Self::Codex => config::CODEX_HOOKS_FILE,
            Self::Devin => ".config/devin/config.json",
        })
    }

    pub(crate) fn shell(self) -> &'static str {
        if self == Self::Devin { "exec" } else { "Bash" }
    }

    /// Codex emits several names for shell-shaped tools. Keep the matcher
    /// provider-specific instead of relying on a Claude-only `Bash` matcher.
    fn shell_matcher(self) -> &'static str {
        match self {
            Self::Codex => "Bash|shell|unified_exec|local_shell",
            _ => self.shell(),
        }
    }

    pub(crate) fn compact(self) -> &'static str {
        if self == Self::Devin {
            "PostCompaction"
        } else {
            "SessionStart"
        }
    }
}

pub(crate) fn quoted_executable(exe: &Path) -> String {
    format!("'{}'", exe.to_string_lossy().replace('\'', "'\\''"))
}

/// Recognize our executable commands and legacy script names, not arbitrary
/// commands merely containing a lifecycle verb.
pub(crate) fn is_pixel_hook(command: &str) -> bool {
    fn executable_name(executable: &str) -> Option<String> {
        let unquoted = if let Some(inner) = executable
            .strip_prefix('\'')
            .and_then(|s| s.strip_suffix('\''))
        {
            inner.replace("'\\''", "'")
        } else {
            if executable.chars().any(char::is_whitespace) {
                return None;
            }
            executable.to_string()
        };
        Path::new(&unquoted)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
    }
    if executable_name(command).is_some_and(|name| {
        [
            config::GUARD_HOOK,
            config::OLD_GUARD_HOOK,
            config::SESSION_START_HOOK,
            config::PROMPT_SUBMIT_HOOK,
            config::POST_COMPACTION_HOOK,
        ]
        .contains(&name.as_str())
    }) {
        return true;
    }
    command
        .rsplit_once(" hook ")
        .is_some_and(|(executable, verb)| {
            executable_name(executable).as_deref() == Some("pixel")
                && [
                    "guard",
                    "guard --provider claude",
                    "guard --provider codex",
                    "guard --provider devin",
                    "guard --provider claude --delegate-rtk",
                    "composed-guard --provider codex",
                    "session-start",
                    "prompt-submit",
                    "prompt-submit --provider claude",
                    "post-compaction",
                    "post-compaction --provider claude",
                ]
                .contains(&verb)
                || (verb.starts_with("composed-guard --provider codex --backup ")
                    && verb
                        .strip_prefix("composed-guard --provider codex --backup ")
                        .is_some_and(|backup| !backup.is_empty()))
        })
}

/// Preserve outer matchers and co-located foreign/security hooks.
pub(crate) fn remove_pixel_hooks(hooks: &mut Map<String, Value>) {
    hooks.retain(|_, groups| {
        let Some(groups) = groups.as_array_mut() else {
            return true;
        };
        groups.retain_mut(|group| {
            let Some(inner) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
                return true;
            };
            inner.retain(|hook| {
                !hook
                    .get("command")
                    .and_then(Value::as_str)
                    .is_some_and(is_pixel_hook)
            });
            !inner.is_empty()
        });
        !groups.is_empty()
    });
}

fn anchored_literal(name: &str) -> Option<&str> {
    let inner = name
        .strip_prefix('^')?
        .strip_suffix('$')
        .unwrap_or(name.strip_prefix('^')?);
    (!inner.is_empty()
        && inner
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')))
    .then_some(inner)
}

fn shell_overlap(group: &Value, provider: Provider) -> bool {
    let matcher = group.get("matcher").and_then(Value::as_str).unwrap_or("");
    let names: Vec<&str> = matcher.split('|').collect();
    let matches_shell = names.iter().copied().any(|name| match provider {
        Provider::Codex => matches!(name, "Bash" | "shell" | "unified_exec" | "local_shell"),
        _ => name == provider.shell(),
    });
    if matches_shell || matches!(matcher, "" | "*" | ".*") {
        return true;
    }
    // A matcher made solely of anchored literal tool-name prefixes is provably
    // disjoint when none can match a current shell tool. This matters for
    // project-local Codex hooks: they shadow global hooks even when they only
    // guard `apply_patch` or a specific MCP edit tool.
    if names.iter().all(|name| anchored_literal(name).is_some()) {
        return names.iter().copied().any(|name| match provider {
            Provider::Codex => matches!(
                anchored_literal(name),
                Some("Bash" | "shell" | "unified_exec" | "local_shell")
            ),
            _ => anchored_literal(name) == Some(provider.shell()),
        });
    }
    // Unknown regexes may match the shell too. Only exact known non-shell
    // alternatives can be ruled out without guessing another engine's regex.
    !matcher.split('|').all(|name| {
        [
            "Read",
            "Grep",
            "Glob",
            "Edit",
            "Write",
            "MultiEdit",
            "NotebookEdit",
            "read",
            "grep",
            "glob",
            "edit",
            "write",
            "apply_patch",
        ]
        .contains(&name)
    })
}

/// CMUX's generated Codex pre-tool feed is an observer: it consumes stdin and
/// returns `{}` when no surface is attached. It has no `updatedInput` or
/// permission decision, so it cannot compete with Pixel's compatibility
/// rewrite. This exact, single-command shape is intentionally the only
/// overlap that may coexist; every unknown command remains a routing blocker.
fn passive_cmux_codex_feed(group: &Value, provider: Provider) -> bool {
    if provider != Provider::Codex
        || !group.get("matcher").and_then(Value::as_str).unwrap_or("").is_empty()
    {
        return false;
    }
    let Some(hooks) = group.get("hooks").and_then(Value::as_array) else {
        return false;
    };
    hooks.len() == 1
        && hooks[0].get("type").and_then(Value::as_str) == Some("command")
        && hooks[0]
            .get("command")
            .and_then(Value::as_str)
            .is_some_and(|command| {
                command.ends_with("/cmux-codex-hook-persistent-feed-PreToolUse.sh")
            })
}

/// Orca's generated wrapper is an observer only while its provider-specific
/// target script is absent: its explicit `else` branch consumes stdin and
/// emits no hook response. If Orca is later installed, this returns false on
/// the next install/doctor pass and Pixel defers rather than racing a newly
/// active hook.
fn inactive_orca_observer(group: &Value, provider: Provider) -> bool {
    if !matches!(provider, Provider::Claude | Provider::Codex)
        || group.get("matcher").and_then(Value::as_str).unwrap_or("")
            != if provider == Provider::Claude {
                "*"
            } else {
                ""
            }
    {
        return false;
    }
    let Some(hooks) = group.get("hooks").and_then(Value::as_array) else {
        return false;
    };
    let Some(command) = (hooks.len() == 1)
        .then(|| hooks[0].get("command").and_then(Value::as_str))
        .flatten()
    else {
        return false;
    };
    let Some(path) = command
        .strip_prefix("if [ -f '")
        .and_then(|rest| rest.split_once("' "))
        .map(|(path, _)| path)
    else {
        return false;
    };
    path.ends_with(&format!("/.orca/agent-hooks/{}-hook.sh", provider.name()))
        && command.contains("else { command -p cat")
        && !Path::new(path).is_file()
}

fn exact_rtk(group: &Value) -> bool {
    group == &json!({"matcher":"Bash","hooks":[{"type":"command","command":"rtk hook claude"}]})
}

/// Vibe Island's generated Claude bridge is a passive observer for the
/// compatible Bash subset. The exact wrapper exits successfully when the
/// bridge is absent; when present, its current PreToolUse probe produces no
/// response, so it cannot compete with Pixel's `updatedInput` rewrite. Keep
/// this deliberately narrow: any additional handler or a different command is
/// an unknown mutator and must still block routing.
fn passive_vibe_claude_bridge(group: &Value, provider: Provider) -> bool {
    if provider != Provider::Claude || group.get("matcher").and_then(Value::as_str) != Some("*") {
        return false;
    }
    let Some(hooks) = group.get("hooks").and_then(Value::as_array) else {
        return false;
    };
    hooks.len() == 1
        && hooks[0].get("type").and_then(Value::as_str) == Some("command")
        && hooks[0]
            .get("command")
            .and_then(Value::as_str)
            .is_some_and(|command| {
                command.contains(".vibe-island/bin/vibe-island-bridge")
                    && command.contains("--source claude")
                    && command.contains("&&")
                    && command.ends_with("exit 0'")
            })
}

/// GitNexus's generated hook augments Claude context only. Its implementation
/// writes `hookSpecificOutput.additionalContext`; it never returns
/// `updatedInput` or a permission decision. Its graph context can therefore be
/// merged with Pixel's deterministic compatibility rewrite.
fn passive_gitnexus_claude_hook(group: &Value, provider: Provider) -> bool {
    if provider != Provider::Claude
        || group.get("matcher").and_then(Value::as_str) != Some("Grep|Glob|Bash")
    {
        return false;
    }
    let Some(hooks) = group.get("hooks").and_then(Value::as_array) else {
        return false;
    };
    hooks.len() == 1
        && hooks[0].get("type").and_then(Value::as_str) == Some("command")
        && hooks[0]
            .get("command")
            .and_then(Value::as_str)
            .is_some_and(|command| command.ends_with("/.claude/hooks/gitnexus/gitnexus-hook.cjs\""))
}

fn hook_group(command: String, matcher: Option<&str>) -> Value {
    let mut group =
        json!({"hooks":[{"type":"command","command":command,"timeout":config::HOOK_TIMEOUT}]});
    if let Some(matcher) = matcher {
        group["matcher"] = matcher.into();
    }
    group
}

pub(crate) fn has_delegate(hooks: &Map<String, Value>) -> bool {
    hooks
        .get("PreToolUse")
        .and_then(Value::as_array)
        .is_some_and(|groups| {
            groups.iter().any(|group| {
                group
                    .get("hooks")
                    .and_then(Value::as_array)
                    .is_some_and(|inner| {
                        inner.iter().any(|hook| {
                            hook.get("command")
                                .and_then(Value::as_str)
                                .is_some_and(|command| {
                                    is_pixel_hook(command) && command.contains(" --delegate-rtk")
                                })
                        })
                    })
            })
        })
}

pub(crate) fn restore_rtk(hooks: &mut Map<String, Value>, saved: &[Value]) {
    let groups = hooks.entry("PreToolUse").or_insert_with(|| json!([]));
    if let Some(groups) = groups.as_array_mut() {
        for group in saved {
            if !groups.contains(group) {
                groups.push(group.clone());
            }
        }
    }
}

pub(crate) fn load_rtk_backup(home: &Path) -> crate::Result<Vec<Value>> {
    let path = home.join(RTK_BACKUP);
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let saved: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(path)?)?;
    if !saved.iter().all(exact_rtk) {
        return Err(InstallError::InvalidSettings {
            path: home.join(RTK_BACKUP),
            reason: "unrecognized RTK backup; refusing to restore commands".into(),
        });
    }
    Ok(saved)
}

/// Apply the pure configuration transform and return (routing enabled, RTK
/// fragments adopted). Unknown overlapping hooks remain untouched.
fn configure(
    value: &mut Value,
    provider: Provider,
    exe: &Path,
    saved: &[Value],
) -> Result<(bool, Vec<Value>), String> {
    let root = value
        .as_object_mut()
        .ok_or("settings root is not an object")?;
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or("hooks is not an object")?;
    let delegated = has_delegate(hooks);
    if delegated && saved.is_empty() {
        return Err("RTK delegate backup missing; refusing to lose its registration".into());
    }
    remove_pixel_hooks(hooks);
    if delegated {
        restore_rtk(hooks, saved);
    }
    let pre = hooks
        .entry("PreToolUse")
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .ok_or("PreToolUse is not an array")?;
    let blocked = pre.iter().any(|group| {
        !(!shell_overlap(group, provider)
            || passive_vibe_claude_bridge(group, provider)
            || passive_gitnexus_claude_hook(group, provider)
            || passive_cmux_codex_feed(group, provider)
            || inactive_orca_observer(group, provider)
            || provider == Provider::Claude && exact_rtk(group))
    });
    let mut adopted = Vec::new();
    if !blocked {
        if provider == Provider::Claude {
            pre.retain(|group| {
                if exact_rtk(group) {
                    adopted.push(group.clone());
                    false
                } else {
                    true
                }
            });
        }
        let delegate = if adopted.is_empty() {
            ""
        } else {
            " --delegate-rtk"
        };
        pre.push(hook_group(
            format!(
                "{} hook guard --provider {}{delegate}",
                quoted_executable(exe),
                provider.name()
            ),
            Some(provider.shell_matcher()),
        ));
    }
    for (event, verb, matcher) in [
        ("SessionStart", "session-start", None),
        ("UserPromptSubmit", "prompt-submit", None),
        (
            provider.compact(),
            "post-compaction",
            if provider == Provider::Devin {
                None
            } else {
                Some("compact")
            },
        ),
    ] {
        let groups = hooks
            .entry(event)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .ok_or_else(|| format!("{event} is not an array"))?;
        // Claude's task runtime is session-scoped. Make that provider choice
        // explicit at the lifecycle boundary without changing Codex/Devin's
        // established hook command shape.
        let provider_arg = if provider == Provider::Claude
            && matches!(verb, "prompt-submit" | "post-compaction")
        {
            " --provider claude"
        } else {
            ""
        };
        groups.push(hook_group(
            format!("{} hook {verb}{provider_arg}", quoted_executable(exe)),
            matcher,
        ));
    }
    Ok((!blocked, adopted))
}

#[allow(dead_code)]
pub(crate) fn install_provider(
    home: &Path,
    exe: &Path,
    provider: Provider,
    dry_run: bool,
) -> crate::Result<install::InstallStep> {
    install_at(home, &provider.path(home), exe, provider, dry_run)
}

/// Verify the installed transform, including matchers, executable, shell
/// overlap and saved delegation. Merely finding a command string is not proof.
pub(crate) fn configuration_status(
    home: &Path,
    exe: &Path,
    provider: Provider,
) -> crate::Result<bool> {
    let path = provider.path(home);
    let original = install::read_settings(&path)?;
    let mut expected = original.clone();
    let saved = if provider == Provider::Claude {
        load_rtk_backup(home)?
    } else {
        Vec::new()
    };
    let (enabled, _) = configure(&mut expected, provider, exe, &saved).map_err(|reason| {
        InstallError::InvalidSettings {
            path: path.clone(),
            reason,
        }
    })?;
    if expected != original {
        return Err(InstallError::InvalidSettings {
            path,
            reason: "provider hook configuration differs from the supported event/matcher/command contract; run pixel install".into(),
        });
    }
    Ok(enabled)
}

#[allow(dead_code)]
fn composed_backup_path(config_path: &Path) -> Result<PathBuf, String> {
    config_path
        .parent()
        .map(|parent| parent.join(CODEX_COMPOSED_BACKUP))
        .ok_or_else(|| "Codex hook configuration has no parent directory".into())
}

#[allow(dead_code)]
fn composed_codex_group(exe: &Path, backup: &Path) -> Value {
    hook_group(
        format!(
            "{} hook composed-guard --provider codex --backup {}",
            quoted_executable(exe),
            quoted_executable(backup)
        ),
        None,
    )
}

#[allow(dead_code)]
fn composed_backup(groups: Vec<Value>, managed_pre_tool_use: Value) -> Value {
    json!({
        "version": CODEX_COMPOSED_BACKUP_VERSION,
        "provider": "codex",
        "pre_tool_use": groups,
        "managed_pre_tool_use": managed_pre_tool_use,
    })
}

#[allow(dead_code)]
fn read_composed_backup(path: &Path) -> crate::Result<Value> {
    let value: Value = serde_json::from_str(&fs::read_to_string(path)?)?;
    let valid_header = value.get("version").and_then(Value::as_u64)
        == Some(CODEX_COMPOSED_BACKUP_VERSION)
        && value.get("provider").and_then(Value::as_str) == Some("codex");
    let groups = value.get("pre_tool_use").and_then(Value::as_array);
    let managed = value.get("managed_pre_tool_use").and_then(Value::as_array);
    if !valid_header || groups.is_none() || managed.is_none() {
        return Err(InstallError::InvalidSettings {
            path: path.into(),
            reason: "unrecognized composed Codex backup; refusing to execute or overwrite it"
                .into(),
        });
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if fs::metadata(path)?.permissions().mode() & 0o077 != 0 {
            return Err(InstallError::InvalidSettings {
                path: path.into(),
                reason: "composed Codex backup must be mode 0600".into(),
            });
        }
    }
    Ok(value)
}

/// Persist the immutable runtime input before installing the command that can
/// consume it. `persist` is an atomic same-directory rename; write mode is
/// tightened before the file becomes visible.
#[allow(dead_code)]
fn write_composed_backup(
    path: &Path,
    groups: &[Value],
    managed_pre_tool_use: Value,
    dry_run: bool,
) -> crate::Result<()> {
    if dry_run {
        return Ok(());
    }
    let parent = path.parent().ok_or_else(|| InstallError::InvalidSettings {
        path: path.into(),
        reason: "composed Codex backup has no parent directory".into(),
    })?;
    fs::create_dir_all(parent)?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_nanos();
    let temporary_path = parent.join(format!(
        ".pixel-composed-guard-{stamp}-{}.tmp",
        std::process::id()
    ));
    let mut temporary = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary_path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    temporary.write_all(
        format!(
            "{}\n",
            serde_json::to_string_pretty(&composed_backup(groups.to_vec(), managed_pre_tool_use))?
        )
        .as_bytes(),
    )?;
    temporary.flush()?;
    drop(temporary);
    fs::rename(&temporary_path, path)?;
    Ok(())
}

/// Adopt every existing project-local Codex PreToolUse group behind a single
/// deterministic Pixel entrypoint. Reinstalls only accept the exact managed
/// shape: a user edit to PreToolUse is a hard refusal, never a silent snapshot
/// refresh that could grant Pixel authority over a newly added command.
#[allow(dead_code)]
pub(crate) fn install_project_codex_at(
    _home: &Path,
    path: &Path,
    exe: &Path,
    dry_run: bool,
) -> crate::Result<install::InstallStep> {
    let backup_path =
        composed_backup_path(path).map_err(|reason| InstallError::InvalidSettings {
            path: path.into(),
            reason,
        })?;
    let mut value = install::read_settings(path)?;
    let expected_group = composed_codex_group(exe, &backup_path);
    let backup_exists = backup_path.is_file();

    if backup_exists {
        // Validate before changing the config. This also proves the runtime
        // input was created by this installer and remains private.
        let stored = read_composed_backup(&backup_path)?;
        let existing = value
            .get("hooks")
            .and_then(Value::as_object)
            .and_then(|hooks| hooks.get("PreToolUse"))
            .and_then(Value::as_array)
            .ok_or_else(|| InstallError::InvalidSettings {
                path: path.into(),
                reason: "composed Codex install lost its PreToolUse group; refusing to overwrite user changes".into(),
            })?;
        if existing.as_slice() != [expected_group.clone()] {
            return Err(InstallError::InvalidSettings {
                path: path.into(),
                reason: "composed Codex PreToolUse diverged from its managed contract; refusing to overwrite user changes".into(),
            });
        }
        if stored["managed_pre_tool_use"] != json!([expected_group.clone()]) {
            return Err(InstallError::InvalidSettings {
                path: backup_path.clone(),
                reason: "composed Codex backup managed contract diverged; refusing to execute or overwrite it".into(),
            });
        }
    }

    // `configure` owns lifecycle cleanup/installation. Capture the original
    // PreToolUse groups *after* stale Pixel registrations are removed, then
    // replace that event only with the composed guard.
    let mut snapshot_source = value.clone();
    if let Some(hooks) = snapshot_source
        .get_mut("hooks")
        .and_then(Value::as_object_mut)
    {
        remove_pixel_hooks(hooks);
    }
    let snapshot = snapshot_source
        .get("hooks")
        .and_then(Value::as_object)
        .and_then(|hooks| hooks.get("PreToolUse"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let saved = Vec::new();
    configure(&mut value, Provider::Codex, exe, &saved).map_err(|reason| {
        InstallError::InvalidSettings {
            path: path.into(),
            reason,
        }
    })?;
    let hooks = value
        .get_mut("hooks")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| InstallError::InvalidSettings {
            path: path.into(),
            reason: "hooks is not an object".into(),
        })?;
    hooks.insert("PreToolUse".into(), json!([expected_group.clone()]));

    if !backup_exists {
        // Sidecar first: config publication cannot expose a command that lacks
        // its approved, atomically-written input.
        write_composed_backup(
            &backup_path,
            &snapshot,
            json!([expected_group.clone()]),
            dry_run,
        )?;
    }
    let backup = install::write_settings(path, &value, dry_run)?;
    Ok(install::InstallStep {
        id: "hooks.codex".into(),
        status: install::CheckStatus::Green,
        summary: install::dry_run_summary(
            dry_run,
            "codex lifecycle + composed shell routing configured (live unverified)",
        ),
        detail: Some(install::with_backup_note(
            format!(
                "{} (composed backup={})",
                path.display(),
                backup_path.display()
            ),
            backup,
        )),
    })
}

#[allow(dead_code)]
pub(crate) fn install_at(
    home: &Path,
    path: &Path,
    exe: &Path,
    provider: Provider,
    dry_run: bool,
) -> crate::Result<install::InstallStep> {
    let mut value = install::read_settings(path)?;
    let saved = if provider == Provider::Claude {
        load_rtk_backup(home)?
    } else {
        Vec::new()
    };
    let (enabled, adopted) = configure(&mut value, provider, exe, &saved).map_err(|reason| {
        InstallError::InvalidSettings {
            path: path.into(),
            reason,
        }
    })?;
    // Persist only the adopted fragment, never restore a whole settings file
    // over later user edits. Save before changing its active registration.
    if !adopted.is_empty() {
        install::write_settings(&home.join(RTK_BACKUP), &json!(adopted), dry_run)?;
    }
    let backup = install::write_settings(path, &value, dry_run)?;
    Ok(install::InstallStep {
        id: format!("hooks.{}", provider.name()),
        status: if enabled {
            install::CheckStatus::Green
        } else {
            install::CheckStatus::Yellow
        },
        summary: install::dry_run_summary(
            dry_run,
            &format!(
                "{} lifecycle configured; shell routing {} (live unverified)",
                provider.name(),
                if enabled {
                    "configured"
                } else {
                    "not installed: unknown overlapping hook"
                }
            ),
        ),
        detail: Some(install::with_backup_note(
            path.display().to_string(),
            backup,
        )),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routing_ownership_rejects_other_executables_and_command_mentions() {
        assert!(is_pixel_hook(
            "'/tmp/Pixel tools/pixel' hook guard --provider claude"
        ));
        assert!(is_pixel_hook("'/tmp/Pixel'\\''s/pixel' hook prompt-submit"));
        assert!(is_pixel_hook(
            "'/tmp/Pixel'\\''s/pixel' hook prompt-submit --provider claude"
        ));
        assert!(is_pixel_hook(
            "'/tmp/Pixel'\\''s/pixel' hook post-compaction --provider claude"
        ));
        for foreign in [
            "other-pixel hook guard",
            "echo /tmp/pixel hook session-start",
            "echo ~/.claude/hooks/pixel-prompt-submit",
            "echo 'pixel hook guard'",
            "'/tmp/pixel' hook session-start && security-check",
            "'/tmp/pixel' hook guard --provider claude; security-check",
            "'/tmp/pixel' hook prompt-submit > user-log",
        ] {
            assert!(!is_pixel_hook(foreign), "{foreign}");
        }
    }

    #[test]
    fn routing_provider_transform_preserves_foreign_hooks_and_lifecycle_contracts() {
        for provider in [Provider::Claude, Provider::Codex, Provider::Devin] {
            let foreign = json!({"matcher":"SessionStart","hooks":[{"type":"command","command":"keep-security-check"}]});
            let mut value = json!({"hooks":{"SessionStart":[foreign.clone(), {"hooks":[{"command":"/tmp/pixel hook session-start"}]}], "PostCompaction":[{"hooks":[{"command":"~/.claude/hooks/pixel-post-compaction"}]}]},"unrelated":true});
            let (enabled, _) = configure(
                &mut value,
                provider,
                Path::new("/tmp/Pixel tools/pixel"),
                &[],
            )
            .unwrap();
            assert!(enabled);
            assert_eq!(value["unrelated"], true);
            assert_eq!(value["hooks"]["SessionStart"][0], foreign);
            assert!(value["hooks"]["SessionStart"][1].get("matcher").is_none());
            assert_eq!(
                value["hooks"]["PreToolUse"][0]["matcher"],
                provider.shell_matcher()
            );
            assert!(value["hooks"][provider.compact()].is_array());
            let prompt = value["hooks"]["UserPromptSubmit"].as_array().unwrap();
            let prompt_command = prompt.last().unwrap()["hooks"][0]["command"]
                .as_str()
                .unwrap();
            if provider == Provider::Claude {
                assert!(prompt_command.ends_with("hook prompt-submit --provider claude"));
            } else {
                assert!(prompt_command.ends_with("hook prompt-submit"));
            }
            if provider != Provider::Devin {
                assert_eq!(value["hooks"]["SessionStart"][2]["matcher"], "compact");
                assert!(value["hooks"].get("PostCompact").is_none());
                let compact_command = value["hooks"]["SessionStart"][2]["hooks"][0]["command"]
                    .as_str()
                    .unwrap();
                if provider == Provider::Claude {
                    assert!(compact_command.ends_with("hook post-compaction --provider claude"));
                } else {
                    assert!(compact_command.ends_with("hook post-compaction"));
                }
            }
            let once = value.clone();
            configure(
                &mut value,
                provider,
                Path::new("/tmp/Pixel tools/pixel"),
                &[],
            )
            .unwrap();
            assert_eq!(value, once);
        }
    }

    #[test]
    fn routing_rtk_adoption_round_trips_without_discarding_user_hooks() {
        let rtk =
            json!({"matcher":"Bash","hooks":[{"type":"command","command":"rtk hook claude"}]});
        let mut value = json!({"hooks":{"PreToolUse":[rtk.clone()]}});
        let (enabled, adopted) =
            configure(&mut value, Provider::Claude, Path::new("/tmp/pixel"), &[]).unwrap();
        assert!(enabled);
        assert_eq!(adopted, vec![rtk.clone()]);
        assert!(has_delegate(value["hooks"].as_object().unwrap()));
        let once = value.clone();
        configure(
            &mut value,
            Provider::Claude,
            Path::new("/tmp/pixel"),
            &adopted,
        )
        .unwrap();
        assert_eq!(value, once);
        let hooks = value["hooks"].as_object_mut().unwrap();
        remove_pixel_hooks(hooks);
        restore_rtk(hooks, &adopted);
        assert_eq!(hooks["PreToolUse"], json!([rtk]));
    }

    #[test]
    fn routing_unknown_overlap_is_preserved_without_a_second_mutator() {
        let security =
            json!({"matcher":"Bash","hooks":[{"type":"command","command":"keep-security-check"}]});
        let mut value = json!({"hooks":{"PreToolUse":[security.clone()]}});
        let (enabled, adopted) =
            configure(&mut value, Provider::Claude, Path::new("/tmp/pixel"), &[]).unwrap();
        assert!(!enabled);
        assert!(adopted.is_empty());
        assert_eq!(value["hooks"]["PreToolUse"], json!([security]));
    }

    #[test]
    fn routing_codex_cmux_observer_can_coexist_with_the_single_rewriter() {
        let cmux = json!({
            "hooks":[{"type":"command","command":"/Users/test/.cmux/hooks/cmux-codex-hook-persistent-feed-PreToolUse.sh"}]
        });
        let mut value = json!({"hooks":{"PreToolUse":[cmux.clone()]}});
        let (enabled, adopted) =
            configure(&mut value, Provider::Codex, Path::new("/tmp/pixel"), &[]).unwrap();
        assert!(enabled);
        assert!(adopted.is_empty());
        assert_eq!(value["hooks"]["PreToolUse"][0], cmux);
        assert_eq!(
            value["hooks"]["PreToolUse"][1]["matcher"],
            "Bash|shell|unified_exec|local_shell"
        );
        assert!(
            value["hooks"]["PreToolUse"][1]["hooks"][0]["command"]
                .as_str()
                .unwrap()
                .ends_with("hook guard --provider codex")
        );
    }

    #[test]
    fn routing_claude_vibe_observer_can_coexist_with_pixel_and_adopted_rtk() {
        let vibe = json!({
            "matcher":"*",
            "hooks":[{"type":"command","command":"/bin/sh -c '[ -x \"$HOME/.vibe-island/bin/vibe-island-bridge\" ] && \"$HOME/.vibe-island/bin/vibe-island-bridge\" --source claude; exit 0'"}]
        });
        let rtk =
            json!({"matcher":"Bash","hooks":[{"type":"command","command":"rtk hook claude"}]});
        let mut value = json!({"hooks":{"PreToolUse":[vibe.clone(), rtk]}});
        let (enabled, adopted) =
            configure(&mut value, Provider::Claude, Path::new("/tmp/pixel"), &[]).unwrap();
        assert!(enabled);
        assert_eq!(adopted.len(), 1);
        assert_eq!(value["hooks"]["PreToolUse"][0], vibe);
        assert_eq!(value["hooks"]["PreToolUse"][1]["matcher"], "Bash");
        assert!(
            value["hooks"]["PreToolUse"][1]["hooks"][0]["command"]
                .as_str()
                .unwrap()
                .ends_with("hook guard --provider claude --delegate-rtk")
        );
    }

    #[test]
    fn routing_claude_gitnexus_context_hook_can_coexist_with_pixel() {
        let gitnexus = json!({
            "matcher":"Grep|Glob|Bash",
            "hooks":[{"type":"command","command":"node \"/Users/test/.claude/hooks/gitnexus/gitnexus-hook.cjs\""}]
        });
        let mut value = json!({"hooks":{"PreToolUse":[gitnexus.clone()]}});
        let (enabled, adopted) =
            configure(&mut value, Provider::Claude, Path::new("/tmp/pixel"), &[]).unwrap();
        assert!(enabled);
        assert!(adopted.is_empty());
        assert_eq!(value["hooks"]["PreToolUse"][0], gitnexus);
        assert_eq!(value["hooks"]["PreToolUse"][1]["matcher"], "Bash");
    }

    #[test]
    fn routing_codex_inactive_orca_wrapper_can_coexist_but_an_active_one_blocks() {
        let missing = std::env::temp_dir()
            .join(format!("pixel-routing-orca-missing-{}", std::process::id()))
            .join(".orca/agent-hooks/codex-hook.sh");
        let command = format!(
            "if [ -f '{}' ] && [ -r '{}' ] && [ -x '{}' ]; then /bin/sh '{}'; else {{ command -p cat 2>/dev/null || cat; }} >/dev/null 2>&1 || :; fi",
            missing.display(),
            missing.display(),
            missing.display(),
            missing.display()
        );
        let orca = json!({"hooks":[{"type":"command","command":command}]});
        let mut value = json!({"hooks":{"PreToolUse":[orca.clone()]}});
        let (enabled, _) =
            configure(&mut value, Provider::Codex, Path::new("/tmp/pixel"), &[]).unwrap();
        assert!(enabled);

        std::fs::create_dir_all(missing.parent().unwrap()).unwrap();
        std::fs::write(&missing, "#!/bin/sh\n").unwrap();
        let mut active = json!({"hooks":{"PreToolUse":[orca]}});
        let (enabled, _) =
            configure(&mut active, Provider::Codex, Path::new("/tmp/pixel"), &[]).unwrap();
        assert!(!enabled);
        let _ = std::fs::remove_file(missing);
    }

    #[test]
    fn routing_claude_inactive_orca_wrapper_can_coexist() {
        let missing = std::env::temp_dir()
            .join(format!(
                "pixel-routing-orca-claude-missing-{}",
                std::process::id()
            ))
            .join(".orca/agent-hooks/claude-hook.sh");
        let command = format!(
            "if [ -f '{}' ] && [ -r '{}' ] && [ -x '{}' ]; then /bin/sh '{}'; else {{ command -p cat 2>/dev/null || cat; }} >/dev/null 2>&1 || :; fi",
            missing.display(),
            missing.display(),
            missing.display(),
            missing.display()
        );
        let orca = json!({"matcher":"*","hooks":[{"type":"command","command":command}]});
        let mut value = json!({"hooks":{"PreToolUse":[orca.clone()]}});
        let (enabled, _) =
            configure(&mut value, Provider::Claude, Path::new("/tmp/pixel"), &[]).unwrap();
        assert!(enabled);
        assert_eq!(value["hooks"]["PreToolUse"][0], orca);
    }

    #[test]
    fn routing_codex_anchored_non_shell_matcher_is_disjoint_and_kept() {
        let worktree_guard = json!({
            "matcher":"^apply_patch$|^mcp__filesystem__|^mcp__morph-mcp__edit_file$",
            "hooks":[{"type":"command","command":"keep-worktree-path-guard"}]
        });
        let mut value = json!({"hooks":{"PreToolUse":[worktree_guard.clone()]}});
        let (enabled, _) =
            configure(&mut value, Provider::Codex, Path::new("/tmp/pixel"), &[]).unwrap();
        assert!(enabled);
        assert_eq!(value["hooks"]["PreToolUse"][0], worktree_guard);
        assert_eq!(
            value["hooks"]["PreToolUse"][1]["matcher"],
            "Bash|shell|unified_exec|local_shell"
        );
    }

    #[test]
    fn project_codex_composition_snapshots_guards_and_refuses_divergence() {
        use std::os::unix::fs::PermissionsExt;

        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("Documents/guarded/.codex/hooks.json");
        let original = json!([
            {"matcher":"Bash","hooks":[{"type":"command","command":"deny-unsafe-shell"}]},
            {"matcher":"*","hooks":[{"type":"command","command":"audit-all-tools"}]}
        ]);
        install::write_settings(
            &path,
            &json!({"hooks":{"PreToolUse":original.clone()}, "keep":true}),
            false,
        )
        .unwrap();

        install_project_codex_at(home.path(), &path, Path::new("/tmp/pixel"), false).unwrap();
        let installed = install::read_settings(&path).unwrap();
        let sidecar = path.parent().unwrap().join(CODEX_COMPOSED_BACKUP);
        let stored = read_composed_backup(&sidecar).unwrap();
        assert_eq!(stored["pre_tool_use"], original);
        assert_eq!(
            stored["managed_pre_tool_use"],
            installed["hooks"]["PreToolUse"]
        );
        assert_eq!(
            fs::metadata(&sidecar).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(
            installed["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
                .as_str()
                .unwrap()
                .contains("hook composed-guard --provider codex --backup")
        );

        // The exact managed state can be installed repeatedly without a new
        // snapshot. Any later manual addition is deliberately a hard refusal.
        install_project_codex_at(home.path(), &path, Path::new("/tmp/pixel"), false).unwrap();
        let mut changed = install::read_settings(&path).unwrap();
        changed["hooks"]["PreToolUse"].as_array_mut().unwrap().push(
            json!({"matcher":"Bash","hooks":[{"type":"command","command":"later-user-guard"}]}),
        );
        install::write_settings(&path, &changed, false).unwrap();
        assert!(
            install_project_codex_at(home.path(), &path, Path::new("/tmp/pixel"), false).is_err()
        );
        assert_eq!(
            read_composed_backup(&sidecar).unwrap()["pre_tool_use"],
            original
        );
    }
}

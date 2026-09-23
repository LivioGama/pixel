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
/// Claude Code's team-shared project settings, usually committed.
pub(crate) const CLAUDE_SHARED_SETTINGS: &str = ".claude/settings.json";
/// Claude Code's personal per-project settings (gitignored by its convention):
/// the repo guard carries this machine's binary path, so it lives here.
pub(crate) const CLAUDE_LOCAL_SETTINGS: &str = ".claude/settings.local.json";
/// Devin CLI's personal per-project config; its `hooks` key is read like
/// `.devin/hooks.v1.json`, but it is not shared with the team.
pub(crate) const DEVIN_LOCAL_CONFIG: &str = ".devin/config.local.json";
/// Where an earlier `pixel install --repo` wrote the Devin guard. Devin CLI
/// never reads this file; install and uninstall take pixel's entry out of it.
pub(crate) const DEVIN_LEGACY_HOOKS: &str = ".devin/hooks.json";
/// Project-local snapshot of Codex `PreToolUse` groups adopted by the composed
/// guard.  It deliberately lives next to the project hook config so a runtime
/// never has to discover or execute the currently mutable hook configuration.
pub(crate) const CODEX_COMPOSED_BACKUP: &str = "pixel-composed-guard-backup.json";
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
    // Installs before `pixel run-hook` registered standalone scripts under
    // `~/.claude/hooks/`. An install that does not recognise them keeps the
    // script entry next to the new `run-hook` one: two SessionStart hooks.
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
    // Entries written before the command rename say `pixel hook <verb>`;
    // both spellings are pixel's and both must be recognised so an upgrade
    // replaces the old entry instead of stacking a second one next to it.
    command
        .rsplit_once(" run-hook ")
        .or_else(|| command.rsplit_once(" hook "))
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
                    "post-tool-use",
                    "post-tool-use --provider claude",
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
        || !group
            .get("matcher")
            .and_then(Value::as_str)
            .unwrap_or("")
            .is_empty()
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

/// The one RTK registration pixel adopts: `rtk hook claude` on `Bash`.
fn rtk_group() -> Value {
    json!({"matcher":"Bash","hooks":[{"type":"command","command":"rtk hook claude"}]})
}

fn exact_rtk(group: &Value) -> bool {
    group == &rtk_group()
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

/// The RTK backup under `home` when no pixel guard in
/// `~/.claude/settings.json` delegates to it: a leftover that `pixel install`
/// never applies, such as one an `install --repo` build wrote under `$HOME`
/// before the repository backup moved into the repository.
pub(crate) fn orphan_rtk_backup(home: &Path) -> crate::Result<Option<PathBuf>> {
    let backup = home.join(RTK_BACKUP);
    if !backup.is_file() {
        return Ok(None);
    }
    let settings = install::read_settings(&Provider::Claude.path(home))?;
    let delegated = settings
        .get("hooks")
        .and_then(Value::as_object)
        .is_some_and(has_delegate);
    Ok((!delegated).then_some(backup))
}

/// Which events a provider install may register. The doctrine reaches every
/// Claude process through the lifecycle hooks in the user-level settings
/// (`~/.claude/settings.json`), while enforcement stays repo-local
/// (`<repo>/.claude/settings.json`); neither file should carry the other's
/// half.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HookScope {
    /// Lifecycle + PreToolUse shell routing (the historical full install).
    All,
    /// SessionStart/UserPromptSubmit/PostToolUse/compaction only — no
    /// PreToolUse. Used for the global Claude install.
    LifecycleOnly,
    /// PreToolUse guard only — no lifecycle events. Used for repo-local
    /// enforcement (`<repo>/.claude/settings.json`).
    GuardOnly,
}

/// Apply the pure configuration transform and return (routing enabled, RTK
/// fragments adopted). Unknown overlapping hooks remain untouched.
fn configure(
    value: &mut Value,
    provider: Provider,
    exe: &Path,
    saved: &[Value],
) -> Result<(bool, Vec<Value>), String> {
    configure_scoped(value, provider, exe, saved, HookScope::All, &[])
}

/// A PreToolUse group that can run beside Pixel's guard: it cannot touch a
/// shell call, or it is one of the known observers that never rewrite one.
fn coexists_with_guard(group: &Value, provider: Provider) -> bool {
    !shell_overlap(group, provider)
        || passive_vibe_claude_bridge(group, provider)
        || passive_gitnexus_claude_hook(group, provider)
        || passive_cmux_codex_feed(group, provider)
        || inactive_orca_observer(group, provider)
}

/// `inherited` holds the PreToolUse groups another settings file contributes
/// to the same session (the shared project file when the guard goes into the
/// personal one). The harness merges them, so an unknown shell rewriter there
/// blocks the guard as surely as one in `value`; an exact RTK group there
/// blocks too, since it cannot be adopted from a file this call does not own.
fn configure_scoped(
    value: &mut Value,
    provider: Provider,
    exe: &Path,
    saved: &[Value],
    scope: HookScope,
    inherited: &[Value],
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
        // The delegate guard that ran RTK is gone: put RTK back where it was,
        // whatever the scope. Without a delegate the backup is only a record
        // of an earlier state and is not applied (the user may have removed
        // RTK since); `install_at_scoped` retires it.
        restore_rtk(hooks, saved);
    }
    let mut enabled = true;
    let mut adopted = Vec::new();
    if scope != HookScope::LifecycleOnly {
        let pre = hooks
            .entry("PreToolUse")
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .ok_or("PreToolUse is not an array")?;
        let blocked = pre.iter().any(|group| {
            !(coexists_with_guard(group, provider)
                || provider == Provider::Claude && exact_rtk(group))
        }) || inherited
            .iter()
            .any(|group| !coexists_with_guard(group, provider));
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
                    "{} run-hook guard --provider {}{delegate}",
                    quoted_executable(exe),
                    provider.name()
                ),
                Some(provider.shell_matcher()),
            ));
        }
        enabled = !blocked;
    }
    if scope == HookScope::GuardOnly {
        return Ok((enabled, adopted));
    }
    for (event, verb, matcher) in [
        (
            "PostToolUse",
            "post-tool-use",
            if provider == Provider::Claude {
                Some("Edit")
            } else {
                None
            },
        ),
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
            && matches!(verb, "prompt-submit" | "post-compaction" | "post-tool-use")
        {
            " --provider claude"
        } else {
            ""
        };
        groups.push(hook_group(
            format!("{} run-hook {verb}{provider_arg}", quoted_executable(exe)),
            matcher,
        ));
    }
    Ok((enabled, adopted))
}

#[cfg_attr(not(test), allow(dead_code))] // exercised by uninstall/routing tests
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
#[allow(dead_code)]
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

fn composed_backup_path(config_path: &Path) -> Result<PathBuf, String> {
    config_path
        .parent()
        .map(|parent| parent.join(CODEX_COMPOSED_BACKUP))
        .ok_or_else(|| "Codex hook configuration has no parent directory".into())
}

fn composed_codex_group(exe: &Path, backup: &Path) -> Value {
    hook_group(
        format!(
            "{} run-hook composed-guard --provider codex --backup {}",
            quoted_executable(exe),
            quoted_executable(backup)
        ),
        None,
    )
}

fn composed_backup(groups: Vec<Value>, managed_pre_tool_use: Value) -> Value {
    json!({
        "version": CODEX_COMPOSED_BACKUP_VERSION,
        "provider": "codex",
        "pre_tool_use": groups,
        "managed_pre_tool_use": managed_pre_tool_use,
    })
}

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

#[cfg_attr(not(test), allow(dead_code))] // exercised by uninstall/routing tests
pub(crate) fn install_at(
    home: &Path,
    path: &Path,
    exe: &Path,
    provider: Provider,
    dry_run: bool,
) -> crate::Result<install::InstallStep> {
    install_at_scoped(home, path, exe, provider, HookScope::All, &[], dry_run)
}

/// Scoped install: lifecycle hooks only (global Claude doctrine delivery) or
/// the PreToolUse guard only (repo-local Claude enforcement). Same merge and
/// foreign-hook preservation rules as the full install.
///
/// `backup_root` is the directory whose `.claude/pixel-rtk-hooks.json` holds
/// an adopted RTK group: `$HOME` for the global install, the repository for
/// `pixel install --repo`, so a repo's adoption never reaches the global
/// settings. `inherited` is passed through to [`configure_scoped`].
pub(crate) fn install_at_scoped(
    backup_root: &Path,
    path: &Path,
    exe: &Path,
    provider: Provider,
    scope: HookScope,
    inherited: &[Value],
    dry_run: bool,
) -> crate::Result<install::InstallStep> {
    let mut value = install::read_settings(path)?;
    let saved = if provider == Provider::Claude {
        load_rtk_backup(backup_root)?
    } else {
        Vec::new()
    };
    let (enabled, adopted) = configure_scoped(&mut value, provider, exe, &saved, scope, inherited)
        .map_err(|reason| InstallError::InvalidSettings {
            path: path.into(),
            reason,
        })?;
    // Persist only the adopted fragment, never restore a whole settings file
    // over later user edits. Save before changing its active registration.
    if !adopted.is_empty() {
        install::write_settings(&backup_root.join(RTK_BACKUP), &json!(adopted), dry_run)?;
    }
    let backup = install::write_settings(path, &value, dry_run)?;
    // Nothing delegates to the backup any more: RTK is back in the settings
    // or was dropped by the user. Retire it after the settings are written.
    if adopted.is_empty() && !saved.is_empty() && !dry_run {
        fs::remove_file(backup_root.join(RTK_BACKUP))?;
    }
    let summary_text = match scope {
        HookScope::LifecycleOnly => {
            format!(
                "{} lifecycle hooks configured (live unverified)",
                provider.name()
            )
        }
        HookScope::GuardOnly => format!(
            "{} guard {}",
            provider.name(),
            if enabled {
                "configured (live unverified)"
            } else {
                "not installed: unknown overlapping hook"
            }
        ),
        HookScope::All => format!(
            "{} lifecycle configured; shell routing {} (live unverified)",
            provider.name(),
            if enabled {
                "configured"
            } else {
                "not installed: unknown overlapping hook"
            }
        ),
    };
    Ok(install::InstallStep {
        id: format!("hooks.{}", provider.name()),
        status: if enabled {
            install::CheckStatus::Green
        } else {
            install::CheckStatus::Yellow
        },
        summary: install::dry_run_summary(dry_run, &summary_text),
        detail: Some(install::with_backup_note(
            path.display().to_string(),
            backup,
        )),
    })
}

/// Take Pixel's guard out of the `PreToolUse` event of a settings file and
/// return the groups left there, plus whether the file changed.
///
/// Only `PreToolUse` is touched, so a lifecycle entry the global install
/// wrote survives even when the repository is `$HOME`. A delegate guard gets
/// the RTK group it adopted back: that group has exactly one accepted shape
/// ([`rtk_group`]), so no backup file is needed to restore it.
pub(crate) fn remove_pre_tool_use_guard(
    path: &Path,
    dry_run: bool,
) -> crate::Result<(Vec<Value>, bool)> {
    if !path.is_file() {
        return Ok((Vec::new(), false));
    }
    let mut value = install::read_settings(path)?;
    let Some(hooks) = value.get_mut("hooks").and_then(Value::as_object_mut) else {
        return Ok((Vec::new(), false));
    };
    let Some(before) = hooks.get("PreToolUse").cloned() else {
        return Ok((Vec::new(), false));
    };
    let mut event = Map::new();
    event.insert("PreToolUse".into(), before.clone());
    let delegated = has_delegate(&event);
    remove_pixel_hooks(&mut event);
    if delegated {
        restore_rtk(&mut event, &[rtk_group()]);
    }
    let after = event.remove("PreToolUse");
    let left = after
        .as_ref()
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if after.as_ref() == Some(&before) {
        return Ok((left, false));
    }
    match after {
        Some(groups) => {
            hooks.insert("PreToolUse".into(), groups);
        }
        None => {
            hooks.remove("PreToolUse");
        }
    }
    install::write_settings(path, &value, dry_run)?;
    Ok((left, true))
}

/// Repo-local Claude guard: `<repo>/.claude/settings.local.json`, never the
/// team-shared `settings.json`, because the command names this machine's
/// binary. A guard an earlier install left in `settings.json` is taken out
/// first; whatever else that file registers on `PreToolUse` still runs in the
/// same session and is checked for overlap. An adopted RTK group is backed up
/// under the repository, not under `$HOME`.
pub(crate) fn install_project_claude_at(
    repo: &Path,
    home: &Path,
    exe: &Path,
    dry_run: bool,
) -> crate::Result<install::InstallStep> {
    let shared = repo.join(CLAUDE_SHARED_SETTINGS);
    let (mut inherited, migrated) = remove_pre_tool_use_guard(&shared, dry_run)?;
    let global_path = Provider::Claude.path(home);
    // A repository at `$HOME` has the global file as its shared one, which
    // was read above.
    let (global, unreadable) = if same_file(&shared, &global_path) {
        (Vec::new(), None)
    } else {
        global_pre_tool_use(&global_path)
    };
    let blocking = blocking_claude_groups(&global);
    let global_blockers = hook_commands(&blocking);
    // A guard a full install of an older release left in the global file:
    // the current global install takes it out.
    let stale_global_guard = commands(&blocking).any(is_pixel_hook);
    inherited.extend(global);
    let mut step = install_at_scoped(
        repo,
        &repo.join(CLAUDE_LOCAL_SETTINGS),
        exe,
        Provider::Claude,
        HookScope::GuardOnly,
        &inherited,
        dry_run,
    )?;
    if migrated {
        step.detail = Some(format!(
            "{}; pixel guard removed from shared {}",
            step.detail.unwrap_or_default(),
            shared.display()
        ));
    }
    if !global_blockers.is_empty() {
        step.summary = install::dry_run_summary(
            dry_run,
            &format!(
                "claude guard not installed: {} in {} also rewrites shell calls{}",
                global_blockers.join(", "),
                global_path.display(),
                if stale_global_guard {
                    " — run `pixel install` to take pixel's global guard out"
                } else {
                    ""
                }
            ),
        );
    }
    if let Some(warning) = unreadable {
        step.status = install::CheckStatus::Yellow;
        step.summary = format!("{}; {warning}", step.summary);
    }
    Ok(step)
}

/// The `PreToolUse` groups of the user-level settings at `path`, which
/// Claude Code merges into every project's session. Read only: a repo
/// install never writes the global file. An unreadable file yields no group
/// and the reason, so it cannot block the repo install on its own.
pub(crate) fn global_pre_tool_use(path: &Path) -> (Vec<Value>, Option<String>) {
    match install::read_settings(path) {
        Ok(value) => (
            value
                .get("hooks")
                .and_then(|hooks| hooks.get("PreToolUse"))
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
            None,
        ),
        Err(e) => (
            Vec::new(),
            Some(format!(
                "{} unreadable ({e}), its PreToolUse hooks were not checked",
                path.display()
            )),
        ),
    }
}

/// The groups that cannot run beside the Claude guard: shell rewriters,
/// the exact RTK group included, since only the file that holds it can hand
/// it to the guard.
pub(crate) fn blocking_claude_groups(groups: &[Value]) -> Vec<Value> {
    groups
        .iter()
        .filter(|group| !coexists_with_guard(group, Provider::Claude))
        .cloned()
        .collect()
}

/// Every hook command of `groups`, in order.
fn commands(groups: &[Value]) -> impl Iterator<Item = &str> {
    groups
        .iter()
        .filter_map(|group| group.get("hooks").and_then(Value::as_array))
        .flatten()
        .filter_map(|hook| hook.get("command").and_then(Value::as_str))
}

/// Every hook command of `groups`, backticked, in order.
pub(crate) fn hook_commands(groups: &[Value]) -> Vec<String> {
    commands(groups)
        .map(|command| format!("`{command}`"))
        .collect()
}

/// Whether `a` and `b` name the same file, comparing canonical paths when
/// both resolve.
pub(crate) fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// Whether any hook command in a settings value (`{"hooks": {<event>: [...]}}`)
/// is one of Pixel's: the evidence that Pixel was installed into that file.
pub(crate) fn has_pixel_hook(value: &Value) -> bool {
    value
        .get("hooks")
        .and_then(Value::as_object)
        .is_some_and(|events| {
            events
                .values()
                .filter_map(Value::as_array)
                .flatten()
                .filter_map(|group| group.get("hooks").and_then(Value::as_array))
                .flatten()
                .filter_map(|hook| hook.get("command").and_then(Value::as_str))
                .any(is_pixel_hook)
        })
}

/// Whether a Pixel `PreToolUse` command in a settings value contains `verb`
/// (`run-hook guard --provider claude`): the guard is registered, not merely
/// some other Pixel hook.
pub(crate) fn has_pixel_guard(value: &Value, verb: &str) -> bool {
    value
        .get("hooks")
        .and_then(|hooks| hooks.get("PreToolUse"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|group| group.get("hooks").and_then(Value::as_array))
        .flatten()
        .filter_map(|hook| hook.get("command").and_then(Value::as_str))
        .any(|command| is_pixel_hook(command) && command.contains(verb))
}

/// Merge a pixel `run-hook guard --provider devin` PreToolUse group into the
/// repository's personal Devin config, `<repo>/.devin/config.local.json`
/// (the command names this machine's binary, so not the shared
/// `.devin/config.json`). Devin's PreToolUse accepts multiple independent
/// groups, so the pixel group is appended after any foreign entries and a
/// reinstall replaces only pixel's own group. A guard an earlier install
/// wrote into `.devin/hooks.json`, a file Devin CLI does not read, is removed.
pub(crate) fn install_project_devin_at(
    repo: &Path,
    exe: &Path,
    dry_run: bool,
) -> crate::Result<install::InstallStep> {
    remove_pre_tool_use_guard(&repo.join(DEVIN_LEGACY_HOOKS), dry_run)?;
    let path = &repo.join(DEVIN_LOCAL_CONFIG);
    let mut value = install::read_settings(path)?;
    let root = value
        .as_object_mut()
        .ok_or_else(|| InstallError::InvalidSettings {
            path: path.into(),
            reason: "settings root is not an object".into(),
        })?;
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| InstallError::InvalidSettings {
            path: path.into(),
            reason: "hooks is not an object".into(),
        })?;
    let pixel_group = hook_group(
        format!(
            "{} run-hook guard --provider {}",
            quoted_executable(exe),
            Provider::Devin.name()
        ),
        Some(Provider::Devin.shell()),
    );
    let merged = config::merge_hook_entry(
        hooks.get("PreToolUse"),
        "run-hook guard --provider devin",
        pixel_group,
    );
    let unchanged = hooks.get("PreToolUse") == Some(&merged);
    let backup = if unchanged {
        None
    } else {
        hooks.insert("PreToolUse".into(), merged);
        install::write_settings(path, &value, dry_run)?
    };
    Ok(install::InstallStep {
        id: "hooks.devin".into(),
        status: install::CheckStatus::Green,
        summary: install::dry_run_summary(
            dry_run,
            if unchanged {
                "devin project guard verified (live unverified)"
            } else {
                "devin project guard configured (live unverified)"
            },
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
            "'/tmp/Pixel tools/pixel' run-hook guard --provider claude"
        ));
        assert!(is_pixel_hook(
            "'/tmp/Pixel'\\''s/pixel' run-hook prompt-submit"
        ));
        assert!(is_pixel_hook(
            "'/tmp/Pixel'\\''s/pixel' run-hook prompt-submit --provider claude"
        ));
        assert!(is_pixel_hook(
            "'/tmp/Pixel'\\''s/pixel' run-hook post-compaction --provider claude"
        ));
        for foreign in [
            "other-pixel hook guard",
            "echo /tmp/pixel hook session-start",
            "echo ~/.claude/hooks/pixel-prompt-submit",
            "echo 'pixel hook guard'",
            "'/tmp/pixel' run-hook session-start && security-check",
            "'/tmp/pixel' run-hook guard --provider claude; security-check",
            "'/tmp/pixel' run-hook prompt-submit > user-log",
        ] {
            assert!(!is_pixel_hook(foreign), "{foreign}");
        }
    }

    /// Installs before `pixel run-hook` registered bare scripts under
    /// `~/.claude/hooks/`; each must read as Pixel's so a reinstall replaces
    /// it instead of leaving it beside the new `run-hook` entry.
    #[test]
    fn is_pixel_hook_should_recognise_script_entries_of_pre_run_hook_installs() {
        for legacy in [
            "~/.claude/hooks/pixel-session-start",
            "/Users/dev/.claude/hooks/pixel-prompt-submit",
            "/Users/dev/.claude/hooks/pixel-post-compaction",
            "~/.claude/hooks/pixel-targets-guard",
            "~/.claude/hooks/gitpixel-targets-guard",
            "'/Users/a dev/.claude/hooks/pixel-session-start'",
        ] {
            assert!(is_pixel_hook(legacy), "{legacy}");
        }
        for foreign in [
            "~/.claude/hooks/pixel-session-start-audit",
            "my-tool ~/.claude/hooks/pixel-session-start",
            "/usr/local/bin/session-start",
        ] {
            assert!(!is_pixel_hook(foreign), "{foreign}");
        }
    }

    #[test]
    fn has_pixel_guard_should_need_a_pixel_command_carrying_the_verb() {
        let settings = |command: &str| json!({"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":command}]}]}});
        let verb = "run-hook guard --provider claude";
        assert!(has_pixel_guard(
            &settings("'/p/pixel' run-hook guard --provider claude"),
            verb
        ));
        assert!(
            !has_pixel_guard(&settings("echo run-hook guard --provider claude"), verb),
            "a foreign command merely naming the verb is not the guard"
        );
        assert!(
            !has_pixel_guard(
                &settings("'/p/pixel' run-hook guard --provider devin"),
                verb
            ),
            "another provider's guard is not this one"
        );
        let lifecycle_only = json!({"hooks":{"SessionStart":[{"hooks":[{"command":"'/p/pixel' run-hook guard --provider claude"}]}]}});
        assert!(
            !has_pixel_guard(&lifecycle_only, verb),
            "only PreToolUse registers a guard"
        );
        assert!(has_pixel_hook(&lifecycle_only));
        assert!(!has_pixel_hook(&settings("keep-security-check")));
        assert!(!has_pixel_hook(&Value::Null));
    }

    #[test]
    fn remove_pre_tool_use_guard_should_keep_lifecycle_and_foreign_groups() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        assert_eq!(
            remove_pre_tool_use_guard(&path, false).unwrap(),
            (Vec::new(), false),
            "an absent file is left absent"
        );
        assert!(!path.exists());
        let write = json!({"matcher":"Write","hooks":[{"type":"command","command":"keep-write"}]});
        let guard = json!({"matcher":"Bash","hooks":[{"type":"command","command":"'/old/pixel' run-hook guard --provider claude"}]});
        let start =
            json!({"hooks":[{"type":"command","command":"'/p/pixel' run-hook session-start"}]});
        let original =
            json!({"hooks":{"PreToolUse":[write.clone(), guard],"SessionStart":[start.clone()]}});
        install::write_settings(&path, &original, false).unwrap();

        let (left, changed) = remove_pre_tool_use_guard(&path, true).unwrap();
        assert!(changed);
        assert_eq!(left, vec![write.clone()]);
        assert_eq!(
            install::read_settings(&path).unwrap(),
            original,
            "a dry run reports without writing"
        );

        let (left, changed) = remove_pre_tool_use_guard(&path, false).unwrap();
        assert!(changed);
        assert_eq!(left, vec![write.clone()]);
        let after = install::read_settings(&path).unwrap();
        assert_eq!(after["hooks"]["PreToolUse"], json!([write.clone()]));
        assert_eq!(
            after["hooks"]["SessionStart"],
            json!([start]),
            "a lifecycle entry is never touched, even Pixel's own"
        );
        assert_eq!(
            remove_pre_tool_use_guard(&path, false).unwrap(),
            (vec![write], false),
            "nothing left to remove"
        );
    }

    #[test]
    fn remove_pre_tool_use_guard_should_drop_an_emptied_event_and_restore_a_delegated_rtk() {
        let dir = tempfile::tempdir().unwrap();
        let only_guard = dir.path().join("only.json");
        install::write_settings(
            &only_guard,
            &json!({"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"'/p/pixel' run-hook guard --provider claude"}]}]}}),
            false,
        )
        .unwrap();
        assert_eq!(
            remove_pre_tool_use_guard(&only_guard, false).unwrap(),
            (Vec::new(), true)
        );
        assert_eq!(
            install::read_settings(&only_guard).unwrap(),
            json!({"hooks":{}})
        );

        let delegated = dir.path().join("delegated.json");
        install::write_settings(
            &delegated,
            &json!({"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"'/p/pixel' run-hook guard --provider claude --delegate-rtk"}]}]}}),
            false,
        )
        .unwrap();
        let (left, changed) = remove_pre_tool_use_guard(&delegated, false).unwrap();
        assert!(changed);
        assert_eq!(
            left,
            vec![rtk_group()],
            "the RTK group the delegate adopted runs again"
        );
        assert_eq!(
            install::read_settings(&delegated).unwrap()["hooks"]["PreToolUse"],
            json!([rtk_group()])
        );

        let no_pre = dir.path().join("no-pre.json");
        let lifecycle = json!({"hooks":{"SessionStart":[{"hooks":[{"command":"x"}]}]}});
        install::write_settings(&no_pre, &lifecycle, false).unwrap();
        assert_eq!(
            remove_pre_tool_use_guard(&no_pre, false).unwrap(),
            (Vec::new(), false)
        );
        let no_hooks = dir.path().join("no-hooks.json");
        install::write_settings(&no_hooks, &json!({"theme":"dark"}), false).unwrap();
        assert_eq!(
            remove_pre_tool_use_guard(&no_hooks, false).unwrap(),
            (Vec::new(), false)
        );
    }

    /// A shell rewriter the shared project settings register runs in the
    /// same Claude session as the personal file's guard: two rewriters of one
    /// command race, so the guard stays out, exactly as when both sit in one
    /// file. A group that cannot touch a shell call blocks nothing.
    #[test]
    fn configure_scoped_should_treat_inherited_shell_rewriters_as_blocking() {
        let exe = Path::new("/tmp/pixel");
        let security =
            json!({"matcher":"Bash","hooks":[{"type":"command","command":"keep-security-check"}]});
        let write = json!({"matcher":"Write","hooks":[{"type":"command","command":"keep-write"}]});
        let gitnexus = json!({"matcher":"Grep|Glob|Bash","hooks":[{"type":"command","command":"node \"/Users/dev/.claude/hooks/gitnexus/gitnexus-hook.cjs\""}]});
        for (inherited, expect_enabled) in [
            (vec![security.clone()], false),
            (vec![rtk_group()], false),
            (vec![write.clone()], true),
            (vec![gitnexus], true),
            (Vec::new(), true),
        ] {
            let mut value = json!({});
            let (enabled, adopted) = configure_scoped(
                &mut value,
                Provider::Claude,
                exe,
                &[],
                HookScope::GuardOnly,
                &inherited,
            )
            .unwrap();
            assert_eq!(enabled, expect_enabled, "{inherited:?}");
            assert!(adopted.is_empty(), "an inherited group is never adopted");
            assert_eq!(
                has_pixel_guard(&value, "run-hook guard --provider claude"),
                expect_enabled,
                "{value}"
            );
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

    fn delegate_guard() -> Value {
        json!({"matcher":"Bash","hooks":[{"type":"command","command":"'/p/pixel' run-hook guard --provider claude --delegate-rtk"}]})
    }

    /// A home whose `~/.claude/settings.json` holds `pre` and whose backup
    /// holds the exact RTK group.
    fn home_with_backup(pre: &[Value]) -> tempfile::TempDir {
        let home = tempfile::tempdir().unwrap();
        fs::create_dir_all(home.path().join(".claude")).unwrap();
        fs::write(
            home.path().join(RTK_BACKUP),
            serde_json::to_string(&json!([rtk_group()])).unwrap(),
        )
        .unwrap();
        fs::write(
            Provider::Claude.path(home.path()),
            serde_json::to_string(&json!({"hooks":{"PreToolUse": pre}})).unwrap(),
        )
        .unwrap();
        home
    }

    fn global_install(home: &Path, dry_run: bool) -> Value {
        install_at_scoped(
            home,
            &Provider::Claude.path(home),
            Path::new("/p/pixel"),
            Provider::Claude,
            HookScope::LifecycleOnly,
            &[],
            dry_run,
        )
        .unwrap();
        install::read_settings(&Provider::Claude.path(home)).unwrap()
    }

    /// The backup exists so that a delegation cannot lose RTK. Without one
    /// it is only a record of the past: a user who removed `rtk hook claude`
    /// keeps it removed, and the stale file goes.
    #[test]
    fn global_install_should_not_bring_back_an_rtk_hook_the_user_removed() {
        let home = home_with_backup(&[]);
        let settings = global_install(home.path(), false);
        assert!(
            !settings.to_string().contains("rtk hook claude"),
            "{settings}"
        );
        assert!(!home.path().join(RTK_BACKUP).exists());
    }

    #[test]
    fn global_install_should_restore_a_delegated_rtk_and_retire_its_backup() {
        let home = home_with_backup(&[delegate_guard()]);
        let settings = global_install(home.path(), false);
        assert_eq!(settings["hooks"]["PreToolUse"], json!([rtk_group()]));
        assert!(!home.path().join(RTK_BACKUP).exists());
    }

    #[test]
    fn global_install_dry_run_should_keep_the_backup() {
        let home = home_with_backup(&[]);
        global_install(home.path(), true);
        assert!(home.path().join(RTK_BACKUP).is_file());
    }

    /// A guard that adopts RTK again on the same run still needs the backup.
    #[test]
    fn repo_install_should_keep_the_backup_while_the_guard_delegates() {
        let repo = home_with_backup(&[delegate_guard()]);
        let local = repo.path().join(CLAUDE_LOCAL_SETTINGS);
        fs::rename(Provider::Claude.path(repo.path()), &local).unwrap();
        install_at_scoped(
            repo.path(),
            &local,
            Path::new("/p/pixel"),
            Provider::Claude,
            HookScope::GuardOnly,
            &[],
            false,
        )
        .unwrap();
        let value = install::read_settings(&local).unwrap();
        assert!(has_delegate(value["hooks"].as_object().unwrap()), "{value}");
        assert_eq!(
            load_rtk_backup(repo.path()).unwrap(),
            vec![rtk_group()],
            "the delegate's backup must survive"
        );
    }

    #[test]
    fn orphan_rtk_backup_should_name_a_backup_no_guard_delegates_to() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(orphan_rtk_backup(home.path()).unwrap(), None);

        let home = home_with_backup(&[]);
        assert_eq!(
            orphan_rtk_backup(home.path()).unwrap(),
            Some(home.path().join(RTK_BACKUP))
        );

        let home = home_with_backup(&[delegate_guard()]);
        assert_eq!(orphan_rtk_backup(home.path()).unwrap(), None);
    }
}

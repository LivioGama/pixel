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
    /// Shell to install the wrapper block for, as a `$SHELL`-style value
    /// (`fish`, `/bin/bash`, …). Defaults to `$SHELL`. Set it when the
    /// invoking process does not run under the user's login shell.
    pub shell: Option<String>,
    /// The `claude` executable whose version decides whether the wrapper
    /// passes `--append-subagent-system-prompt-file`. Defaults to the first
    /// `claude` on PATH. Claude Code rejects the flag before 2.1.261 with
    /// `error: unknown option`, which would break every `claude -p` call.
    pub claude_executable: Option<PathBuf>,
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

/// First executable file named `name` on PATH.
pub(crate) fn find_on_path(name: &str) -> Option<PathBuf> {
    find_in_paths(name, &std::env::var_os("PATH")?)
}

/// First executable file named `name` in the PATH-style list `path`.
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

#[allow(dead_code)]
fn command_available(name: &str) -> bool {
    find_on_path(name).is_some()
}

/// A Claude Code version as printed by `claude --version` (`2.1.269 (Claude Code)`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ClaudeVersion(pub u64, pub u64, pub u64);

impl std::fmt::Display for ClaudeVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// First Claude Code release that accepts `--append-subagent-system-prompt-file`
/// (its CHANGELOG entry for 2.1.261). Earlier releases exit 1 on the unknown
/// option, so the wrapper must not pass it to them.
pub const MIN_CLAUDE_FOR_SUBAGENT_PROMPT: ClaudeVersion = ClaudeVersion(2, 1, 261);

/// Parse the first `major.minor.patch` token found anywhere in `claude
/// --version` output. Scanning every token, not just the first, survives a
/// preamble printed by an npm/corepack/version-manager shim; a pre-release
/// suffix (`2.1.270-rc1`) is dropped from the patch component.
pub(crate) fn parse_claude_version(output: &str) -> Option<ClaudeVersion> {
    output.split_whitespace().find_map(|token| {
        let mut parts = token.trim_start_matches('v').split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch = parts
            .next()?
            .split(|c: char| !c.is_ascii_digit())
            .next()?
            .parse()
            .ok()?;
        Some(ClaudeVersion(major, minor, patch))
    })
}

/// How long `claude --version` may take before the probe gives up. A shim
/// that installs on first use, or a stalled proxy, must not hang `pixel
/// install`; an expired probe is a [`SubagentSupport::Unknown`].
const CLAUDE_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// What `pixel install` and `pixel doctor` learned about the user's Claude Code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeProbe {
    /// Where `claude` was found, if anywhere.
    pub executable: Option<PathBuf>,
    /// Its version, if `claude --version` ran and parsed.
    pub version: Option<ClaudeVersion>,
}

/// Whether the observed Claude Code takes `--append-subagent-system-prompt-file`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentSupport {
    /// A Claude Code at or above [`MIN_CLAUDE_FOR_SUBAGENT_PROMPT`] was seen.
    Supported(ClaudeVersion),
    /// An older Claude Code was seen: it exits 1 on the flag.
    TooOld(ClaudeVersion),
    /// No usable `claude`: not on PATH, not answering, or an unparseable
    /// version. Not evidence either way — callers must not downgrade a
    /// working install on it.
    Unknown,
}

impl ClaudeProbe {
    pub fn support(&self) -> SubagentSupport {
        match self.version {
            Some(version) if version >= MIN_CLAUDE_FOR_SUBAGENT_PROMPT => {
                SubagentSupport::Supported(version)
            }
            Some(version) => SubagentSupport::TooOld(version),
            None => SubagentSupport::Unknown,
        }
    }

    /// True only when a Claude Code at or above
    /// [`MIN_CLAUDE_FOR_SUBAGENT_PROMPT`] was observed.
    pub fn supports_subagent_prompt(&self) -> bool {
        matches!(self.support(), SubagentSupport::Supported(_))
    }

    /// One phrase for install/doctor summaries explaining the decision.
    pub fn explanation(&self) -> String {
        match (&self.executable, self.support()) {
            (None, _) => "claude not found on PATH".to_string(),
            (Some(exe), SubagentSupport::Unknown) => format!(
                "{} --version did not report a version within {}s",
                exe.display(),
                CLAUDE_PROBE_TIMEOUT.as_secs()
            ),
            (Some(_), SubagentSupport::Supported(version)) => {
                format!("Claude Code {version} accepts --append-subagent-system-prompt-file")
            }
            (Some(_), SubagentSupport::TooOld(version)) => format!(
                "Claude Code {version} < {MIN_CLAUDE_FOR_SUBAGENT_PROMPT} rejects --append-subagent-system-prompt-file"
            ),
        }
    }
}

/// Run `exe --version` with stdin closed and a deadline; `None` on any
/// failure, non-zero exit, or timeout (the child is killed).
fn claude_version_output(exe: &Path) -> Option<String> {
    use std::process::{Command, Stdio};
    let mut child = Command::new(exe)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let stdout = child.stdout.take()?;
    // Read on a helper thread so a child that never exits cannot block the
    // probe on a full pipe; the deadline below is what actually bounds it.
    let reader = std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = String::new();
        let _ = std::io::BufReader::new(stdout).read_to_string(&mut buf);
        buf
    });
    let started = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let output = reader.join().ok()?;
                return status.success().then_some(output);
            }
            Ok(None) if started.elapsed() < CLAUDE_PROBE_TIMEOUT => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// Run `claude --version` on `claude_override`, or on the first `claude` on
/// PATH. Never fails: a missing or broken `claude` is a probe with no version.
pub fn probe_claude(claude_override: Option<&Path>) -> ClaudeProbe {
    let executable = match claude_override {
        Some(path) => Some(path.to_path_buf()),
        None => find_on_path("claude"),
    };
    let version = executable
        .as_ref()
        .and_then(|exe| parse_claude_version(&claude_version_output(exe)?));
    ClaudeProbe {
        executable,
        version,
    }
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
/// and sets up a `claude` shell wrapper plus the Codex `developer_instructions`
/// config key so every invocation includes the Pixel retrieval protocol —
/// and, when OpenCode is present, a managed block in its global
/// `~/.config/opencode/AGENTS.md`. No hooks, no managed blocks in the
/// home-level CLAUDE.md/AGENTS.md files, no provider-specific routing — the
/// system prompt is the single enforcement mechanism.
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
    let claude = probe_claude(options.claude_executable.as_deref());
    let codex_home = crate::codex_config::codex_home(&home, options.home.is_some());
    let mut steps = vec![
        deploy_agent_prompt(&home, dry_run)?,
        install_shell_wrappers(&home, options.shell.as_deref(), &claude, dry_run)?,
        crate::codex_config::install_developer_instructions(&codex_home, dry_run)?,
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

/// Prompt path written literally into the wrapper block, so the shell expands
/// `$HOME` itself and the block survives a moved home directory.
pub(crate) const PROMPT_PATH: &str = "$HOME/.local/share/pixel/agent-prompt.md";

/// Sub-agent prompt path, written literally into the wrapper block for the
/// same reason as [`PROMPT_PATH`].
pub(crate) const SUBAGENT_PROMPT_PATH: &str = "$HOME/.local/share/pixel/subagent-prompt.md";

/// The Claude Code flag the wrapper adds in print mode (2.1.261+).
pub(crate) const SUBAGENT_PROMPT_FLAG: &str = "--append-subagent-system-prompt-file";

/// The shell to install wrappers for: the caller's override, else the
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

/// Build the managed shell-wrapper block for `kind`. Uses a shell function
/// (not an alias) because functions pass subcommands and options through
/// untouched.
///
/// Codex gets the prompt through `developer_instructions` in its
/// `config.toml` (see [`crate::codex_config`]), which reaches every Codex
/// front end; there is no `codex` function any more.
///
/// The `claude` wrapper always appends `prompt_path` to the session prompt.
/// With `Some(subagent_prompt_path)` it appends that file to sub-agents only
/// when `--print`, `-p`, or a short-flag cluster containing `p` (`-pc`,
/// `-cp`: Claude Code splits those) is among the arguments: Claude Code
/// honours `--append-subagent-system-prompt-file` in print mode only
/// (checked by experiment on 2.1.269: the same sub-agent probe applies the
/// file's rule under `-p` and ignores it in an interactive session), and the
/// same wrapper also fronts interactive sessions. A false positive costs
/// nothing — the flag parses in interactive mode too — while an unconditional
/// flag would make a missing prompt file fatal for every `claude` call. With `None` (Claude Code older
/// than [`MIN_CLAUDE_FOR_SUBAGENT_PROMPT`], or not found) the wrapper is the
/// plain one-liner, because those releases exit on the unknown option.
pub(crate) fn shell_wrapper_block(
    kind: ShellKind,
    prompt_path: &str,
    subagent_prompt_path: Option<&str>,
) -> String {
    let body = match (kind, subagent_prompt_path) {
        (ShellKind::Posix, None) => format!(
            "claude() {{ command claude --append-system-prompt-file \"{prompt_path}\" \"$@\"; }}",
        ),
        (ShellKind::Fish, None) => format!(
            "function claude; command claude --append-system-prompt-file \"{prompt_path}\" $argv; end",
        ),
        (ShellKind::Posix, Some(subagent)) => format!(
            "claude() {{\n\
             \x20 local _pixel_arg\n\
             \x20 for _pixel_arg in \"$@\"; do\n\
             \x20   case \"$_pixel_arg\" in\n\
             \x20     -p*|-[!-]*p*|--print) command claude --append-system-prompt-file \"{prompt_path}\" --append-subagent-system-prompt-file \"{subagent}\" \"$@\"; return $?;;\n\
             \x20   esac\n\
             \x20 done\n\
             \x20 command claude --append-system-prompt-file \"{prompt_path}\" \"$@\"\n\
             }}",
        ),
        // fish: `function name; ...; end`, arguments as `$argv`. `contains
        // -- -p` needs the `--` so `-p` is looked up rather than parsed as an
        // option.
        (ShellKind::Fish, Some(subagent)) => format!(
            "function claude\n\
             \x20 if contains -- --print $argv; or string match -qr -- '^-[^-]*p' $argv\n\
             \x20   command claude --append-system-prompt-file \"{prompt_path}\" --append-subagent-system-prompt-file \"{subagent}\" $argv\n\
             \x20 else\n\
             \x20   command claude --append-system-prompt-file \"{prompt_path}\" $argv\n\
             \x20 end\n\
             end",
        ),
    };
    format!(
        "{PIXEL_MANAGED_BEGIN}\n\
         # Pixel agent system prompt — added by `pixel install`\n\
         # Remove with `pixel uninstall`\n\
         {body}\n\
         {PIXEL_MANAGED_END}",
    )
}

/// The block exactly as `pixel install` writes it for `kind` and this Claude
/// Code — what doctor compares the on-disk block against, so a block left
/// behind by another shell, an older pixel, or a Claude Code that has since
/// crossed the 2.1.261 line reads as stale instead of green.
pub(crate) fn expected_wrapper_block(kind: ShellKind, with_subagent_prompt: bool) -> String {
    shell_wrapper_block(
        kind,
        PROMPT_PATH,
        with_subagent_prompt.then_some(SUBAGENT_PROMPT_PATH),
    )
}

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

/// Install the `claude` shell wrapper in the user's shell profile so every
/// invocation automatically includes the Pixel system prompt.
fn install_shell_wrappers(
    home: &Path,
    shell_override: Option<&str>,
    claude: &ClaudeProbe,
    dry_run: bool,
) -> Result<InstallStep> {
    let shell = resolve_shell(shell_override);
    let (kind, profile) = shell_profile_for(&shell, home);
    let existing = fs::read_to_string(&profile).unwrap_or_default();
    // Yellow, not red, whenever the flag is not written: the session prompt
    // is wired either way, and yellow says why the sub-agent half is
    // missing. Without a usable `claude` there is no evidence either way, so
    // the decision an earlier install made with evidence is kept: a
    // re-install from a shell where `claude` is not on PATH (cron, an
    // agent's command tool) must not strip the flag from a working block,
    // and a first install without `claude` takes the safe plain block.
    let (with_subagent_prompt, status, subagent_note) = match claude.support() {
        SubagentSupport::Supported(_) => (true, CheckStatus::Green, String::new()),
        SubagentSupport::TooOld(_) => (
            false,
            CheckStatus::Yellow,
            format!(" without the sub-agent prompt ({})", claude.explanation()),
        ),
        SubagentSupport::Unknown => {
            let kept = extract_managed_block(&existing)
                .is_some_and(|block| block.contains(SUBAGENT_PROMPT_FLAG));
            let note = if kept {
                format!(
                    ", keeping the sub-agent prompt flag of the existing block ({}, could not re-check)",
                    claude.explanation()
                )
            } else {
                format!(
                    " without the sub-agent prompt ({}; run `pixel install` again with Claude Code on PATH)",
                    claude.explanation()
                )
            };
            (kept, CheckStatus::Yellow, note)
        }
    };
    let block = expected_wrapper_block(kind, with_subagent_prompt);
    let detail = Some(format!(
        "profile={} shell={} subagent_prompt={} claude={}",
        profile.display(),
        kind.as_str(),
        with_subagent_prompt,
        claude.explanation()
    ));
    if dry_run {
        return Ok(InstallStep {
            id: "shell-wrappers".into(),
            status,
            summary: format!(
                "would write {} shell wrappers to {}{subagent_note}",
                kind.as_str(),
                profile.display()
            ),
            detail,
        });
    }
    // fish's drop-in lives in a directory that may not exist yet.
    if let Some(parent) = profile.parent() {
        fs::create_dir_all(parent)?;
    }
    // An unterminated block is refused, not stripped to EOF: the lines after
    // it are the user's. Red, and the profile is left untouched.
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
    // Always write — the block may need refreshing even if old content was
    // clean. The write backs the previous profile up and goes through a temp
    // file and a rename, so a crash leaves the old profile readable.
    write_atomically(&profile, &new_content)?;
    Ok(InstallStep {
        id: "shell-wrappers".into(),
        status,
        summary: format!(
            "{} {} shell wrappers in {}{subagent_note}",
            if had_old_block {
                "updated"
            } else {
                "installed"
            },
            kind.as_str(),
            profile.display()
        ),
        detail,
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
mod claude_version_tests {
    use super::*;

    #[test]
    fn parses_the_version_claude_prints() {
        assert_eq!(
            parse_claude_version("2.1.269 (Claude Code)\n"),
            Some(ClaudeVersion(2, 1, 269))
        );
        assert_eq!(
            parse_claude_version("v2.1.261"),
            Some(ClaudeVersion(2, 1, 261))
        );
        // A shim's preamble or a pre-release suffix must not turn a known
        // version into "unknown" (which would withhold the flag silently).
        assert_eq!(
            parse_claude_version("npm notice: update available\n2.1.269 (Claude Code)"),
            Some(ClaudeVersion(2, 1, 269))
        );
        assert_eq!(
            parse_claude_version("2.1.270-rc1"),
            Some(ClaudeVersion(2, 1, 270))
        );
        assert_eq!(parse_claude_version(""), None);
        assert_eq!(parse_claude_version("Claude Code"), None);
        assert_eq!(parse_claude_version("2.1"), None);
    }

    #[test]
    fn the_threshold_is_the_first_release_that_accepts_the_flag() {
        let probe = |version| ClaudeProbe {
            executable: Some(PathBuf::from("claude")),
            version,
        };
        // Numeric, not lexicographic: 2.1.261 > 2.1.99 and 2.2.0 > 2.1.261.
        for supported in [
            ClaudeVersion(2, 1, 261),
            ClaudeVersion(2, 1, 269),
            ClaudeVersion(2, 2, 0),
            ClaudeVersion(3, 0, 0),
        ] {
            assert!(
                probe(Some(supported)).supports_subagent_prompt(),
                "{supported} accepts the flag"
            );
        }
        for rejected in [
            ClaudeVersion(2, 1, 260),
            ClaudeVersion(2, 1, 99),
            ClaudeVersion(2, 0, 999),
            ClaudeVersion(1, 9, 9),
        ] {
            assert!(
                !probe(Some(rejected)).supports_subagent_prompt(),
                "{rejected} exits on the unknown option"
            );
        }
        assert!(
            !probe(None).supports_subagent_prompt(),
            "no version means no proof: omitting the flag loses sub-agent rules, \
             passing it to an old Claude Code breaks every print-mode call"
        );
        assert!(
            !ClaudeProbe {
                executable: None,
                version: None
            }
            .supports_subagent_prompt()
        );
    }

    #[test]
    fn probe_survives_a_missing_or_silent_claude() {
        let missing = probe_claude(Some(Path::new("/nonexistent/claude")));
        assert_eq!(missing.version, None);
        assert_eq!(missing.support(), SubagentSupport::Unknown);
    }

    #[test]
    #[cfg(unix)]
    fn probe_gives_up_on_a_claude_that_never_answers() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let stuck = dir.path().join("claude");
        fs::write(&stuck, "#!/bin/sh\nsleep 60\n").unwrap();
        fs::set_permissions(&stuck, fs::Permissions::from_mode(0o755)).unwrap();
        let started = std::time::Instant::now();
        let probe = probe_claude(Some(&stuck));
        assert_eq!(probe.support(), SubagentSupport::Unknown);
        assert!(
            started.elapsed() < CLAUDE_PROBE_TIMEOUT + std::time::Duration::from_secs(2),
            "a hung claude must not hang pixel install: took {:?}",
            started.elapsed()
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

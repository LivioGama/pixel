//! Integration tests for pixel-install: doctor and install.

use std::fs;

use pixel_install::config::{MANAGED_BEGIN, MANAGED_END};
use pixel_install::doctor::{DoctorOptions, doctor};
use pixel_install::install::{InstallOptions, install};
use pixel_install::uninstall::{UninstallOptions, uninstall};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// test fixture: a fake "pixel" executable.
//
// pixel is a CLI + hooks tool, not an MCP server — `pixel install` no longer
// probes for an `mcp` subcommand or registers a pixel MCP server entry. The
// fixture below just stands in for a real pixel binary so install has
// something to write into the guard/session-start hook scripts.
// ---------------------------------------------------------------------------

/// Write a tiny shell script to `dir` standing in for a real pixel binary.
/// Used so the guard/session-start hook scripts point at a real executable.
#[cfg(unix)]
fn fake_pixel_exe(dir: &std::path::Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("pixel");
    fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

// The new install writes shell wrappers (a pixel-managed block) into the
// user's shell profile. `shell_profile_path` and the managed markers are
// `pub(crate)` in install.rs, so this mirrors the same logic for tests.
fn shell_profile_path(home: &std::path::Path) -> std::path::PathBuf {
    let shell = std::env::var("SHELL").unwrap_or_default();
    if shell.contains("bash") {
        home.join(".bashrc")
    } else {
        home.join(".zshrc")
    }
}
const PIXEL_MANAGED_BEGIN: &str = "# >>> pixel-managed >>>";

// ---------------------------------------------------------------------------
// doctor tests
// ---------------------------------------------------------------------------

#[test]
fn doctor_runs_and_returns_report() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();

    // Doctor with no installed config — should still run and return a report
    // (some checks will be red, which is expected).
    let options = DoctorOptions {
        home: Some(home.to_path_buf()),
        executable_path: None, // uses current_exe
        ..Default::default()
    };

    let report = doctor(&options).expect("doctor runs");
    assert!(!report.checks.is_empty(), "doctor should produce checks");
    assert!(
        report.summary.green + report.summary.yellow + report.summary.red > 0,
        "summary should tally checks"
    );
    // With no settings.json, install.mcp should be green (nothing to scrub).
    let mcp_check = report
        .checks
        .iter()
        .find(|c| c.id == "install.mcp")
        .expect("should have install.mcp check");
    assert!(
        mcp_check.status == pixel_install::doctor::CheckStatus::Green,
        "install.mcp should be green when no settings.json exists (nothing to scrub), got {:?}: {:?}",
        mcp_check.status,
        mcp_check.reason
    );
}

#[test]
fn doctor_after_install_reports_green_mcp() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();

    // Run install first — pixel is a CLI + hooks tool, not an MCP server,
    // so install only scrubs deprecated MCP entries and wires hooks. The
    // install.mcp doctor check verifies no deprecated servers linger, not
    // that pixel itself is registered as an MCP server.
    let install_opts = InstallOptions {
        home: Some(home.to_path_buf()),
        executable_path: Some(fake_pixel_exe(home)),
        dry_run: false,
    };
    install(&install_opts).expect("install");

    let doc_opts = DoctorOptions {
        home: Some(home.to_path_buf()),
        executable_path: None,
        ..Default::default()
    };
    let report = doctor(&doc_opts).expect("doctor after install");

    let mcp_check = report
        .checks
        .iter()
        .find(|c| c.id == "install.mcp")
        .expect("should have install.mcp check");
    assert!(
        mcp_check.status == pixel_install::doctor::CheckStatus::Green,
        "install.mcp should be green after install (no deprecated servers), got {:?}: {:?}",
        mcp_check.status,
        mcp_check.reason
    );

    let prompt_check = report
        .checks
        .iter()
        .find(|c| c.id == "install.agent-prompt")
        .expect("should have install.agent-prompt check");
    assert_eq!(
        prompt_check.status,
        pixel_install::doctor::CheckStatus::Green,
        "install.agent-prompt should be green after install, got {:?}: {:?}",
        prompt_check.status,
        prompt_check.reason
    );

    let wrappers_check = report
        .checks
        .iter()
        .find(|c| c.id == "install.shell-wrappers")
        .expect("should have install.shell-wrappers check");
    assert_eq!(
        wrappers_check.status,
        pixel_install::doctor::CheckStatus::Green,
        "install.shell-wrappers should be green after install, got {:?}: {:?}",
        wrappers_check.status,
        wrappers_check.reason
    );

    let old_hooks_check = report
        .checks
        .iter()
        .find(|c| c.id == "install.old-hooks-clean")
        .expect("should have install.old-hooks-clean check");
    assert_eq!(
        old_hooks_check.status,
        pixel_install::doctor::CheckStatus::Green,
        "install.old-hooks-clean should be green after install, got {:?}: {:?}",
        old_hooks_check.status,
        old_hooks_check.reason
    );
}

// ---------------------------------------------------------------------------
// install tests
// ---------------------------------------------------------------------------

#[test]
fn install_creates_config_with_managed_markers() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();

    // Pre-create a CLAUDE.md with some existing content AND a stale pixel
    // managed block from a previous (hook-based) install.
    let original = format!(
        "# My Project\n\nSome notes.\n\n{begin}\n# old pixel rules\n{end}\n",
        begin = MANAGED_BEGIN,
        end = MANAGED_END
    );
    fs::write(home.join("CLAUDE.md"), original).unwrap();

    let options = InstallOptions {
        home: Some(home.to_path_buf()),
        executable_path: Some(fake_pixel_exe(home)),
        dry_run: false,
    };
    let report = install(&options).expect("install");

    assert!(report.ok, "install should succeed (ok=true)");
    assert!(report.summary.red == 0, "no red steps");
    assert!(report.summary.green > 0, "should have green steps");

    // The new install strips old managed blocks; it never writes new ones.
    let claude = fs::read_to_string(home.join("CLAUDE.md")).expect("CLAUDE.md");
    assert!(
        !claude.contains(MANAGED_BEGIN),
        "CLAUDE.md should have no managed begin marker after install"
    );
    assert!(
        !claude.contains(MANAGED_END),
        "CLAUDE.md should have no managed end marker after install"
    );
    // Original user content should be preserved.
    assert!(
        claude.contains("Some notes."),
        "original CLAUDE.md content should be preserved"
    );
    // The shell wrappers + agent prompt are the new install artifacts.
    assert!(
        home.join(".local/share/pixel/agent-prompt.md").is_file(),
        "agent-prompt.md should be deployed"
    );
    let profile = shell_profile_path(home);
    assert!(
        fs::read_to_string(&profile)
            .unwrap_or_default()
            .contains(PIXEL_MANAGED_BEGIN),
        "shell wrappers should be installed in {}",
        profile.display()
    );
}

#[test]
fn install_is_idempotent() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();

    let options = InstallOptions {
        home: Some(home.to_path_buf()),
        executable_path: Some(fake_pixel_exe(home)),
        dry_run: false,
    };

    // First install.
    let r1 = install(&options).expect("install 1");
    assert!(r1.ok);

    // Second install — should succeed again without error.
    let r2 = install(&options).expect("install 2");
    assert!(r2.ok, "second install should succeed");
    assert!(r2.summary.red == 0, "no red steps on re-install");

    // The new install deploys the agent prompt and shell wrappers (no
    // managed blocks, no hooks). Both must be stable across re-installs.
    let prompt = home.join(".local/share/pixel/agent-prompt.md");
    let p1 = fs::read(&prompt).expect("agent-prompt deployed");
    let profile = shell_profile_path(home);
    let s1 = fs::read(&profile).expect("shell wrappers installed");

    install(&options).expect("install 3");
    assert_eq!(
        fs::read(&prompt).unwrap(),
        p1,
        "agent-prompt must be byte-identical across re-installs"
    );
    assert_eq!(
        fs::read(&profile).unwrap(),
        s1,
        "shell wrappers must be byte-identical across re-installs"
    );

    // No managed blocks are ever written by the new install.
    let claude = fs::read_to_string(home.join(".claude").join("CLAUDE.md")).unwrap_or_default();
    assert!(
        !claude.contains(MANAGED_BEGIN),
        "install must not write managed blocks"
    );
}

#[test]
fn install_wires_codex_lifecycle_hooks_without_blocking_guard() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    let codex_path = home.join(pixel_install::config::CODEX_HOOKS_FILE);
    fs::create_dir_all(codex_path.parent().unwrap()).unwrap();
    fs::write(
        &codex_path,
        serde_json::to_string_pretty(&serde_json::json!({
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [
                        { "type": "command", "command": "~/.claude/hooks/pixel-targets-guard" },
                        { "type": "command", "command": "~/.claude/hooks/keep-this-hook" }
                    ]
                }]
            },
            "unrelated": true
        }))
        .unwrap(),
    )
    .unwrap();

    let options = InstallOptions {
        home: Some(home.to_path_buf()),
        executable_path: Some(fake_pixel_exe(home)),
        dry_run: false,
    };
    install(&options).expect("install");

    let after: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&codex_path).unwrap()).unwrap();
    assert_eq!(
        after["unrelated"], true,
        "unrelated Codex config must survive"
    );
    let pre = after["hooks"]["PreToolUse"].as_array().unwrap();
    assert_eq!(
        pre.len(),
        1,
        "the outer PreToolUse group should be preserved"
    );
    assert_eq!(
        pre[0]["hooks"][0]["command"], "~/.claude/hooks/keep-this-hook",
        "unrelated hook in the same group must survive guard removal"
    );
    assert!(
        !after
            .to_string()
            .contains(pixel_install::config::GUARD_HOOK),
        "the blocking guard must be removed"
    );
    // The new install wires NO lifecycle hooks. SessionStart and
    // UserPromptSubmit must be absent after install.
    let hooks = after["hooks"].as_object().unwrap();
    for event in ["SessionStart", "UserPromptSubmit"] {
        assert!(
            hooks.get(event).is_none(),
            "Codex {event} hook should NOT be installed by the new install, got: {:?}",
            hooks.get(event)
        );
    }
}

#[test]
fn install_removes_deprecated_mcp_servers() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();

    // Pre-create settings.json with a deprecated server.
    let settings_dir = home.join(".claude");
    fs::create_dir_all(&settings_dir).unwrap();
    let settings = serde_json::json!({
        "mcpServers": {
            "usable-git": { "command": "old-binary", "args": ["mcp"] },
            "gitpixel": { "command": "old-binary", "args": ["mcp"] }
        }
    });
    fs::write(
        settings_dir.join("settings.json"),
        serde_json::to_string_pretty(&settings).unwrap(),
    )
    .unwrap();

    let options = InstallOptions {
        home: Some(home.to_path_buf()),
        executable_path: Some(fake_pixel_exe(home)),
        dry_run: false,
    };
    install(&options).expect("install");

    let after = fs::read_to_string(settings_dir.join("settings.json")).unwrap();
    assert!(
        !after.contains("\"usable-git\""),
        "deprecated usable-git server should be removed"
    );
    assert!(
        !after.contains("\"gitpixel\""),
        "deprecated gitpixel server should be removed"
    );
}

#[test]
fn install_leaves_settings_json_valid_after_install() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    // Pre-create .claude/settings.json so installed_agents detects Claude
    // even when the `claude` binary is not on PATH (e.g. Linux CI).
    let claude_dir = home.join(".claude");
    fs::create_dir_all(&claude_dir).unwrap();
    fs::write(claude_dir.join("settings.json"), "{}").unwrap();

    let options = InstallOptions {
        home: Some(home.to_path_buf()),
        executable_path: None,
        dry_run: false,
    };
    install(&options).expect("install");

    let settings_path = home.join(".claude").join("settings.json");
    let raw = fs::read_to_string(&settings_path).expect("settings.json readable");
    let parsed: Result<serde_json::Value, _> = serde_json::from_str(&raw);
    assert!(
        parsed.is_ok(),
        "settings.json must remain valid JSON after install, got parse error: {:?}\ncontent:\n{}",
        parsed.err(),
        raw
    );
    // Regression guard: settings.json must never be run through the
    // Markdown managed-marker rewrite (find_agent_configs must not list it).
    assert!(
        !raw.contains(MANAGED_BEGIN) && !raw.contains(MANAGED_END),
        "settings.json must never contain Markdown managed markers, got:\n{raw}"
    );
}

#[test]
fn find_agent_configs_never_includes_settings_json() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    let settings_dir = home.join(".claude");
    fs::create_dir_all(&settings_dir).unwrap();
    fs::write(settings_dir.join("settings.json"), "{}").unwrap();
    fs::write(home.join("CLAUDE.md"), "# Project\n").unwrap();

    let configs = pixel_install::config::find_agent_configs(home);
    assert!(
        !configs.iter().any(|p| p.ends_with("settings.json")),
        "find_agent_configs must never return settings.json, got: {configs:?}"
    );
    assert!(
        configs.iter().any(|p| p.ends_with("CLAUDE.md")),
        "find_agent_configs should still find CLAUDE.md, got: {configs:?}"
    );
}

#[test]
fn install_strips_stale_gitnexus_blocks() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();

    // Pre-create CLAUDE.md carrying (a) a stale pixel managed block from a
    // previous hook-based install and (b) a genuine GitNexus-generated
    // section. The new install only strips pixel-managed blocks — it no
    // longer rewrites agent-config or scrubs third-party stale blocks, so
    // the GitNexus section must survive (its cleanup is the user's job now
    // that install no longer rewrites CLAUDE.md).
    let original = format!(
        "\
# Project

## GitNexus — Code Intelligence
This project is indexed by GitNexus.

### Always Do
MUST run impact analysis before editing any symbol.

{begin}
# old pixel managed rules
{end}

## Notes
Real project notes.
",
        begin = MANAGED_BEGIN,
        end = MANAGED_END
    );
    fs::write(home.join("CLAUDE.md"), original).unwrap();

    let options = InstallOptions {
        home: Some(home.to_path_buf()),
        executable_path: None,
        dry_run: false,
    };
    let report = install(&options).expect("install");

    // The strip-old-blocks step should report the pixel managed block
    // removed (1 file stripped).
    let strip_step = report
        .steps
        .iter()
        .find(|s| s.id == "strip-old-blocks")
        .expect("should have strip-old-blocks step");
    assert_eq!(
        strip_step.status,
        pixel_install::install::CheckStatus::Green,
        "strip-old-blocks should be green: {:?}",
        strip_step.detail
    );

    let claude = fs::read_to_string(home.join("CLAUDE.md")).unwrap();
    // The pixel managed block is stripped.
    assert!(
        !claude.contains(MANAGED_BEGIN),
        "pixel managed block should be stripped entirely"
    );
    assert!(
        !claude.contains("old pixel managed rules"),
        "the managed block body should be gone"
    );
    // Non-pixel content survives, including the (now unmanaged) GitNexus
    // section — install no longer scrubs third-party stale blocks.
    assert!(
        claude.contains("Real project notes."),
        "non-stale content (a later, unrelated section) should be preserved"
    );
}

#[test]
fn stale_block_removal_never_deletes_incidental_mentions() {
    // Regression test for a real, confirmed bug: the OLD implementation
    // deleted any line merely CONTAINING "gitnexus"/"codebase-memory" as a
    // substring, anywhere. A real ~/.claude/CLAUDE.md rule reads: "...
    // override every other discovery protocol (codebase-memory, gitnexus,
    // generic exploration)." — a hand-written bullet point listing OTHER
    // tools it deprioritizes, not a stale GitNexus block. That line must
    // survive untouched; only a genuine section HEADER announcing a
    // GitNexus/codebase-memory block should trigger removal.
    let original = "\
# My Rules

- While a manifest is active, targets override every other discovery \
protocol (codebase-memory, gitnexus, generic exploration).
- Some other rule entirely.
";
    let (cleaned, removed) = pixel_install::config::strip_stale_blocks(original);
    assert_eq!(
        removed, 0,
        "no genuine stale block header exists; nothing should be removed"
    );
    assert_eq!(
        cleaned, original,
        "a bare incidental mention of gitnexus/codebase-memory in hand-written prose must survive verbatim"
    );
}

// ---------------------------------------------------------------------------
// dry-run tests
// ---------------------------------------------------------------------------

#[test]
fn dry_run_writes_nothing_on_a_clean_home() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();

    let options = InstallOptions {
        home: Some(home.to_path_buf()),
        executable_path: Some(fake_pixel_exe(home)),
        dry_run: true,
    };
    let report = install(&options).expect("dry-run install");

    // pixel is a CLI + hooks tool, not an MCP server — there is no mcp.pixel
    // step anymore. A dry-run on a clean home should report ok (all steps
    // green, nothing to write) and leave nothing on disk.
    assert!(report.dry_run, "report should mark itself as a dry run");
    assert!(
        report.ok,
        "dry-run on clean home should report ok: {report:?}"
    );

    // Nothing should exist on disk: no .claude dir, no CLAUDE.md, no hooks.
    assert!(
        !home.join(".claude").exists(),
        ".claude directory must not be created in dry-run mode"
    );
    assert!(
        !home.join("CLAUDE.md").exists(),
        "CLAUDE.md must not be created in dry-run mode"
    );
    assert!(
        !home.join(".claude").join("CLAUDE.md").exists(),
        ".claude/CLAUDE.md must not be created in dry-run mode"
    );
}

// ---------------------------------------------------------------------------
// deprecated-MCP scrub (pixel is a CLI + hooks tool, not an MCP server —
// install removes the retired usable-git/gitpixel/sniper MCP entries
// unconditionally, since pixel replaces them via Bash + the guard hook.)
// ---------------------------------------------------------------------------

#[test]
fn install_removes_deprecated_mcp_servers_unconditionally() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    let settings_dir = home.join(".claude");
    fs::create_dir_all(&settings_dir).unwrap();
    let settings_path = settings_dir.join("settings.json");
    fs::write(
        &settings_path,
        serde_json::to_string_pretty(&serde_json::json!({
            "mcpServers": {
                "usable-git": { "command": "old-usable-git-binary", "args": ["mcp"] },
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let options = InstallOptions {
        home: Some(home.to_path_buf()),
        executable_path: Some(fake_pixel_exe(home)),
        dry_run: false,
    };
    let report = install(&options).expect("install");

    // install should succeed — pixel is not an MCP server, so there's no
    // mcp.pixel step to fail. The deprecated usable-git entry is removed
    // unconditionally.
    assert!(report.ok, "install should succeed: {report:?}");
    assert!(
        report.steps.iter().all(|s| s.id != "mcp.pixel"),
        "no mcp.pixel step should exist — pixel is not an MCP server"
    );

    let after: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&settings_path).unwrap())
            .expect("still valid JSON");
    assert!(
        after["mcpServers"].get("usable-git").is_none(),
        "the deprecated usable-git MCP server must be removed — pixel replaces it via Bash + the guard hook"
    );
    assert!(
        after["mcpServers"].get("pixel").is_none(),
        "pixel must NOT register an MCP server entry — it is a CLI + hooks tool"
    );
}

#[test]
fn install_scrubs_deprecated_mcp_servers_from_global_claude_json() {
    // Claude Code keeps GLOBAL MCP registrations in ~/.claude.json, not in
    // ~/.claude/settings.json — a retired server registered there survived
    // every earlier install scrub and kept failing to connect at session
    // start (ENOENT on the removed binary).
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    let global_path = home.join(".claude.json");
    fs::write(
        &global_path,
        serde_json::to_string_pretty(&serde_json::json!({
            "mcpServers": {
                "usable-git": { "command": "/opt/homebrew/bin/usable-git", "args": ["mcp"] },
                "github": { "command": "gh-mcp" },
            },
            "unrelatedTopLevelKey": { "kept": true },
        }))
        .unwrap(),
    )
    .unwrap();

    let options = InstallOptions {
        home: Some(home.to_path_buf()),
        executable_path: Some(fake_pixel_exe(home)),
        dry_run: false,
    };
    let report = install(&options).expect("install");
    assert!(report.ok, "install should succeed: {report:?}");

    let after: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&global_path).unwrap()).expect("still valid JSON");
    assert!(
        after["mcpServers"].get("usable-git").is_none(),
        "the deprecated usable-git entry in the GLOBAL ~/.claude.json must be scrubbed"
    );
    assert!(
        after["mcpServers"].get("github").is_some(),
        "unrelated MCP servers in ~/.claude.json must be preserved"
    );
    assert!(
        after["unrelatedTopLevelKey"]["kept"]
            .as_bool()
            .unwrap_or(false),
        "unrelated top-level keys in ~/.claude.json must be preserved"
    );
}

#[test]
fn dry_run_leaves_pre_existing_files_byte_identical() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    // Pre-create .claude/settings.json so installed_agents detects Claude
    // even when the `claude` binary is not on PATH (e.g. Linux CI).
    let claude_dir = home.join(".claude");
    fs::create_dir_all(&claude_dir).unwrap();
    fs::write(claude_dir.join("settings.json"), "{}").unwrap();

    // Pre-create real state as if a previous non-dry-run install ran.
    let real_options = InstallOptions {
        home: Some(home.to_path_buf()),
        executable_path: None,
        dry_run: false,
    };
    install(&real_options).expect("real install");

    let settings_path = home.join(".claude").join("settings.json");
    let prompt_path = home.join(".local/share/pixel/agent-prompt.md");
    let profile_path = shell_profile_path(home);
    let before_settings = fs::read(&settings_path).unwrap();
    let before_prompt = fs::read(&prompt_path).unwrap();
    let before_profile = fs::read(&profile_path).unwrap();

    // A dry-run install afterwards must not touch anything, even though a
    // real install already exists (idempotent no-op path).
    let dry_options = InstallOptions {
        dry_run: true,
        ..real_options
    };
    let report = install(&dry_options).expect("dry-run install over existing state");
    assert!(report.dry_run);

    let after_settings = fs::read(&settings_path).unwrap();
    let after_prompt = fs::read(&prompt_path).unwrap();
    let after_profile = fs::read(&profile_path).unwrap();
    assert_eq!(
        before_settings, after_settings,
        "dry-run must not modify settings.json"
    );
    assert_eq!(
        before_prompt, after_prompt,
        "dry-run must not modify agent-prompt.md"
    );
    assert_eq!(
        before_profile, after_profile,
        "dry-run must not modify the shell profile"
    );

    // And it must not have written any backup files either.
    let home_entries: Vec<String> = fs::read_dir(home)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        !home_entries.iter().any(|n| n.contains("pixel-bak")),
        "dry-run must not create backup files, got: {home_entries:?}"
    );
}

// ---------------------------------------------------------------------------
// backup tests
// ---------------------------------------------------------------------------

#[test]
fn reinstall_backs_up_claude_md_only_when_content_actually_changes() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();

    // Pre-create a CLAUDE.md with hand-written content AND a stale pixel
    // managed block from a previous hook-based install. The new install
    // strips managed blocks (content changes) but, unlike the old
    // rewrite-agent-config path, the strip step writes directly without a
    // backup — so no CLAUDE.md.pixel-bak should appear.
    let original = format!(
        "# Project\n\nHand-written notes.\n\n{begin}\n# old pixel rules\n{end}\n",
        begin = MANAGED_BEGIN,
        end = MANAGED_END
    );
    fs::write(home.join("CLAUDE.md"), original).unwrap();

    let options = InstallOptions {
        home: Some(home.to_path_buf()),
        executable_path: None,
        dry_run: false,
    };
    install(&options).expect("install 1");

    // The managed block is stripped, hand-written content preserved.
    let claude = fs::read_to_string(home.join("CLAUDE.md")).unwrap();
    assert!(
        !claude.contains(MANAGED_BEGIN),
        "managed block should be stripped after install"
    );
    assert!(
        claude.contains("Hand-written notes."),
        "hand-written content should be preserved"
    );

    // No backup file is written by the strip step.
    let backups_after_first: Vec<_> = fs::read_dir(home)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .contains("CLAUDE.md.pixel-bak")
        })
        .collect();
    assert_eq!(
        backups_after_first.len(),
        0,
        "the new strip-old-blocks step writes directly without a backup, got: {backups_after_first:?}"
    );

    // Second install: content won't change (no managed block left), so the
    // file is untouched and still no backup.
    let before = fs::read(home.join("CLAUDE.md")).unwrap();
    install(&options).expect("install 2");
    let after = fs::read(home.join("CLAUDE.md")).unwrap();
    assert_eq!(
        before, after,
        "idempotent re-install must not change CLAUDE.md at all"
    );
    let backups_after_second: Vec<_> = fs::read_dir(home)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .contains("CLAUDE.md.pixel-bak")
        })
        .collect();
    assert_eq!(
        backups_after_second.len(),
        0,
        "idempotent re-install must not create a backup when nothing changed"
    );
}

// ---------------------------------------------------------------------------
// realistic settings.json fixture — models a real ~/.claude/settings.json
// with pre-existing MCP servers, multiple hook events (including a
// PreToolUse entry referencing the OLD guard hook by its real command
// shape, and a SessionStart array with multiple unrelated matcher groups
// from other tools), and unrelated top-level keys.
// ---------------------------------------------------------------------------

fn realistic_settings_json() -> serde_json::Value {
    serde_json::json!({
        "model": "claude-sonnet-5",
        "mcpServers": {
            "usable-git": { "command": "old-usable-git-binary", "args": ["mcp"] },
            "github": { "command": "gh-mcp-server", "args": ["serve"] },
        },
        "hooks": {
            "PreToolUse": [
                { "matcher": "*", "hooks": [{ "type": "command", "command": "/bin/sh -c 'echo bridge'" }] },
                {
                    "matcher": "Grep|Glob",
                    "hooks": [{
                        "type": "command",
                        "command": "~/.claude/hooks/gitpixel-targets-guard",
                        "timeout": 5,
                    }],
                },
            ],
            "SessionStart": [
                { "matcher": "startup", "hooks": [{ "type": "command", "command": "~/.claude/hooks/cbm-session-reminder" }] },
                { "matcher": "resume", "hooks": [{ "type": "command", "command": "~/.claude/hooks/cbm-session-reminder" }] },
                { "matcher": "clear", "hooks": [{ "type": "command", "command": "~/.claude/hooks/cbm-session-reminder" }] },
            ],
            "Stop": [
                { "hooks": [{ "type": "command", "command": "~/.claude/hooks/verify-before-done", "timeout": 10 }] },
            ],
        },
    })
}

#[test]
fn install_against_realistic_settings_json_is_safe() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    let settings_dir = home.join(".claude");
    fs::create_dir_all(&settings_dir).unwrap();
    let settings_path = settings_dir.join("settings.json");
    let before = realistic_settings_json();
    fs::write(
        &settings_path,
        serde_json::to_string_pretty(&before).unwrap(),
    )
    .unwrap();

    let options = InstallOptions {
        home: Some(home.to_path_buf()),
        executable_path: Some(fake_pixel_exe(home)),
        dry_run: false,
    };
    let report = install(&options).expect("install against realistic settings.json");
    assert!(report.ok, "install should succeed: {report:?}");

    // (a) settings.json must still be valid JSON.
    let raw = fs::read_to_string(&settings_path).expect("settings.json readable");
    let after: serde_json::Value = serde_json::from_str(&raw)
        .unwrap_or_else(|e| panic!("settings.json corrupted: {e}\ncontent:\n{raw}"));

    // (b) unrelated top-level content and unrelated entries survive.
    assert_eq!(
        after["model"], "claude-sonnet-5",
        "unrelated top-level key must survive"
    );
    assert_eq!(
        after["mcpServers"]["github"]["command"], "gh-mcp-server",
        "unrelated MCP server must survive"
    );
    assert!(
        after["mcpServers"].get("usable-git").is_none(),
        "deprecated usable-git MCP server should be removed"
    );
    let stop_hooks = after["hooks"]["Stop"]
        .as_array()
        .expect("Stop hooks array survives");
    assert_eq!(
        stop_hooks.len(),
        1,
        "unrelated Stop hook must survive untouched"
    );
    assert_eq!(
        stop_hooks[0]["hooks"][0]["command"],
        "~/.claude/hooks/verify-before-done"
    );

    // The three pre-existing, unrelated SessionStart entries from another
    // tool must all survive — install must MERGE, not overwrite.
    let session_start = after["hooks"]["SessionStart"]
        .as_array()
        .expect("SessionStart array survives");
    let cbm_entries = session_start
        .iter()
        .filter(|e| {
            e["hooks"][0]["command"]
                .as_str()
                .map(|c| c.contains("cbm-session-reminder"))
                .unwrap_or(false)
        })
        .count();
    assert_eq!(
        cbm_entries, 3,
        "all 3 pre-existing SessionStart entries from another tool must survive, got session_start={session_start:?}"
    );

    // (c) The new install wires NO hooks — it only scrubs deprecated MCP
    // entries and removes the old blocking guard. pixel is a CLI + hooks
    // tool, not an MCP server, so it must NOT be registered as one, and it
    // must NOT add its own SessionStart/PreToolUse entries.
    assert!(
        after["mcpServers"].get("pixel").is_none(),
        "pixel must NOT be registered as an MCP server — it is a CLI + hooks tool"
    );
    let has_pixel_session_start = session_start.iter().any(|e| {
        e["hooks"][0]["command"]
            .as_str()
            .map(|c| c.contains("hook session-start"))
            .unwrap_or(false)
    });
    assert!(
        !has_pixel_session_start,
        "pixel's own SessionStart entry should NOT be added by the new install"
    );

    // The old blocking guard is removed during migration, but the unrelated
    // PreToolUse bridge remains untouched. The new install removes the guard
    // without adding any new PreToolUse entry.
    let pre_tool_use = after["hooks"]["PreToolUse"]
        .as_array()
        .expect("PreToolUse array survives");
    assert_eq!(
        pre_tool_use.len(),
        1,
        "PreToolUse should retain only the unrelated bridge entry"
    );
    assert!(
        !raw.contains(pixel_install::config::GUARD_HOOK),
        "default install must not leave Pixel's blocking guard wired"
    );
    let bridge_entry = pre_tool_use
        .iter()
        .find(|e| e["matcher"] == "*")
        .expect("unrelated bridge PreToolUse entry survives");
    assert_eq!(
        bridge_entry["hooks"][0]["command"],
        "/bin/sh -c 'echo bridge'"
    );

    // (d) a .bak was written before the destructive rewrite. `install()`
    // touches settings.json across separate steps (scrub deprecated entries,
    // remove the old guard entry) — each backs up independently when its own
    // write actually changes the content, so multiple backup files can
    // legitimately exist in one install run. `fs::read_dir`'s order is
    // unspecified, so pick the EARLIEST one by its embedded nanosecond
    // timestamp (filenames are `settings.json.pixel-bak.<nanos>-<seq>`) to
    // find the TRUE pre-install snapshot rather than an arbitrary
    // intermediate one.
    let mut backups: Vec<_> = fs::read_dir(&settings_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .contains("settings.json.pixel-bak")
        })
        .collect();
    assert!(
        !backups.is_empty(),
        "settings.json must be backed up before rewrite"
    );
    backups.sort_by_key(|e| {
        e.file_name()
            .to_string_lossy()
            .rsplit("pixel-bak.")
            .next()
            .and_then(|rest| rest.split('-').next())
            .and_then(|nanos| nanos.parse::<u128>().ok())
            .unwrap_or(u128::MAX)
    });
    let earliest_backup = &backups[0];
    let backup_content = fs::read_to_string(earliest_backup.path()).unwrap();
    let backup_parsed: serde_json::Value = serde_json::from_str(&backup_content)
        .unwrap_or_else(|e| panic!("backup should itself be valid JSON: {e}"));
    assert_eq!(
        backup_parsed["mcpServers"]["usable-git"]["command"], "old-usable-git-binary",
        "the earliest backup should preserve the ORIGINAL pre-install content, got {backups:?}"
    );

    // Re-running install must stay idempotent: no new pixel entries, no
    // duplicate MCP server, no re-corruption. The new install adds no
    // hooks, so SessionStart stays at exactly the 3 foreign entries.
    install(&options).expect("second install");
    let raw2 = fs::read_to_string(&settings_path).unwrap();
    let after2: serde_json::Value =
        serde_json::from_str(&raw2).expect("still valid JSON after re-install");
    let session_start2 = after2["hooks"]["SessionStart"].as_array().unwrap();
    let pixel_count2 = session_start2
        .iter()
        .filter(|e| {
            e["hooks"][0]["command"]
                .as_str()
                .map(|c| c.contains("hook session-start"))
                .unwrap_or(false)
        })
        .count();
    assert_eq!(
        pixel_count2, 0,
        "re-install must not add pixel's own SessionStart entry"
    );
    assert_eq!(
        session_start2.len(),
        3,
        "3 foreign entries only, stable across re-install (no pixel lifecycle entries)"
    );
}

// ---------------------------------------------------------------------------
// capability advertisement — the SessionStart block is derived from the live
// op registry in pixel-proto (`SESSION_CAPABILITIES`, tested exhaustively
// there); the old hand-maintained duplicate registry in this crate is gone.
// ---------------------------------------------------------------------------

#[test]
fn session_capabilities_registry_is_live_and_excludes_internal_ops() {
    let caps = pixel_proto::op::SESSION_CAPABILITIES;
    for expected in [
        "search", "targets", "publish", "push", "ship", "resolve", "impact",
    ] {
        assert!(
            caps.contains(&expected),
            "expected capability {expected} missing from SESSION_CAPABILITIES"
        );
    }
    assert!(
        !caps.contains(&"shutdown"),
        "internal shutdown op must not be advertised as a capability"
    );
}

#[test]
fn reinstall_is_byte_for_byte_idempotent_on_managed_claude_md() {
    // Regression test: apply_managed_markers previously grew the file by
    // one trailing newline on every re-install (295 bytes -> 296 -> 297...)
    // because the tail extraction re-included the block's own trailing
    // newline. Three consecutive installs must produce byte-identical
    // output after the first.
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    fs::write(home.join("CLAUDE.md"), "# Project\n\nHand-written notes.\n").unwrap();
    let options = InstallOptions {
        home: Some(home.to_path_buf()),
        executable_path: None,
        dry_run: false,
    };
    install(&options).expect("install 1");
    let c1 = fs::read_to_string(home.join("CLAUDE.md")).unwrap();
    install(&options).expect("install 2");
    let c2 = fs::read_to_string(home.join("CLAUDE.md")).unwrap();
    install(&options).expect("install 3");
    let c3 = fs::read_to_string(home.join("CLAUDE.md")).unwrap();
    assert_eq!(c1, c2, "second install must not change CLAUDE.md at all");
    assert_eq!(c2, c3, "third install must not change CLAUDE.md at all");
}

#[test]
fn install_on_a_fresh_home_creates_claude_md_even_with_no_pre_existing_file() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    // Deliberately do NOT pre-create CLAUDE.md or AGENTS.md.
    let options = InstallOptions {
        home: Some(home.to_path_buf()),
        executable_path: None,
        dry_run: false,
    };
    install(&options).expect("install on fresh home");

    // The new install does NOT create or rewrite any CLAUDE.md/AGENTS.md —
    // it deploys the agent system prompt and shell wrappers instead.
    assert!(
        !home.join("CLAUDE.md").exists(),
        "fresh install must not create root CLAUDE.md"
    );
    assert!(
        !home.join(".claude").join("CLAUDE.md").exists(),
        "fresh install must not create .claude/CLAUDE.md"
    );
    assert!(
        !home.join("AGENTS.md").exists(),
        "fresh install must not create root AGENTS.md"
    );

    // The agent system prompt is deployed.
    let prompt_path = home.join(".local/share/pixel/agent-prompt.md");
    let prompt = fs::read_to_string(&prompt_path)
        .expect("agent-prompt.md should be deployed on a fresh home");
    assert!(
        prompt.contains("REPLACEMENT MAP"),
        "agent-prompt.md should carry the replacement map"
    );

    // Shell wrappers are installed in the shell profile.
    let profile = shell_profile_path(home);
    let profile_content = fs::read_to_string(&profile)
        .expect("shell profile should be created on a fresh home");
    assert!(
        profile_content.contains(PIXEL_MANAGED_BEGIN),
        "shell profile should carry the pixel-managed wrapper block"
    );
    assert!(
        profile_content.contains("claude()"),
        "shell profile should define the claude() wrapper"
    );
    assert!(
        profile_content.contains("codex()"),
        "shell profile should define the codex() wrapper"
    );
}

// ---------------------------------------------------------------------------
// doctor hook check tests (prompt-submit, Devin, Codex, Gemini, zcode)
// ---------------------------------------------------------------------------

#[test]
fn doctor_prompt_submit_check_permissions_and_settings() {
    // The old `install.prompt-submit-hook` check is gone — the new install
    // wires no hooks. Doctor now verifies the new install artifacts:
    //   - install.agent-prompt: the agent system prompt is deployed & fresh
    //   - install.shell-wrappers: the claude()/codex() wrappers are present
    //   - install.old-hooks-clean: no stale hook scripts linger in
    //     ~/.claude/hooks/
    // This test walks each check through its red and green states.
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();

    let doc_opts = DoctorOptions {
        home: Some(home.to_path_buf()),
        executable_path: None,
        ..Default::default()
    };

    // 1. With nothing installed, agent-prompt and shell-wrappers are red.
    let report = doctor(&doc_opts).expect("doctor runs");
    let prompt_check = report
        .checks
        .iter()
        .find(|c| c.id == "install.agent-prompt")
        .unwrap();
    assert_eq!(
        prompt_check.status,
        pixel_install::doctor::CheckStatus::Red,
        "agent-prompt should be red when not deployed"
    );
    let wrappers_check = report
        .checks
        .iter()
        .find(|c| c.id == "install.shell-wrappers")
        .unwrap();
    assert_eq!(
        wrappers_check.status,
        pixel_install::doctor::CheckStatus::Red,
        "shell-wrappers should be red when not installed"
    );

    // 2. A stale old guard hook script makes install.old-hooks-clean red.
    //    (install only removes the guard scripts; the session-start/prompt-
    //    submit scripts are not cleaned by install, only by uninstall.)
    let hooks_dir = home.join(".claude").join("hooks");
    fs::create_dir_all(&hooks_dir).unwrap();
    let stale_script = hooks_dir.join(pixel_install::config::GUARD_HOOK);
    fs::write(&stale_script, "#!/bin/sh\nexit 0\n").unwrap();
    let report = doctor(&doc_opts).expect("doctor runs");
    let old_hooks_check = report
        .checks
        .iter()
        .find(|c| c.id == "install.old-hooks-clean")
        .unwrap();
    assert_eq!(
        old_hooks_check.status,
        pixel_install::doctor::CheckStatus::Red,
        "old-hooks-clean should be red when a stale hook script lingers"
    );

    // 3. Run install: deploys agent-prompt + shell wrappers and removes the
    //    stale hook script → all three checks go green.
    install(&InstallOptions {
        home: Some(home.to_path_buf()),
        executable_path: Some(fake_pixel_exe(home)),
        dry_run: false,
    })
    .expect("install");

    let report = doctor(&doc_opts).expect("doctor runs");
    for id in [
        "install.agent-prompt",
        "install.shell-wrappers",
        "install.old-hooks-clean",
    ] {
        let check = report.checks.iter().find(|c| c.id == id).unwrap();
        assert_eq!(
            check.status,
            pixel_install::doctor::CheckStatus::Green,
            "{id} should be green after install, got {:?}: {:?}",
            check.status,
            check.reason
        );
    }
    assert!(
        !stale_script.is_file(),
        "install should remove the stale hook script"
    );

    // 4. Corrupting the agent prompt makes install.agent-prompt red (stale).
    let prompt_path = home.join(".local/share/pixel/agent-prompt.md");
    fs::write(&prompt_path, "# stale prompt without the required markers\n").unwrap();
    let report = doctor(&doc_opts).expect("doctor runs");
    let prompt_check = report
        .checks
        .iter()
        .find(|c| c.id == "install.agent-prompt")
        .unwrap();
    assert_eq!(
        prompt_check.status,
        pixel_install::doctor::CheckStatus::Red,
        "agent-prompt should be red when the deployed prompt is stale"
    );
}

#[test]
fn doctor_checks_devin_hooks_wiring() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    let config_dir = home.join(pixel_install::config::DEVIN_CONFIG_DIR);
    fs::create_dir_all(&config_dir).unwrap();
    let config_path = config_dir.join(pixel_install::config::DEVIN_CONFIG_FILE);

    let doc_opts = DoctorOptions {
        home: Some(home.to_path_buf()),
        executable_path: Some(fake_pixel_exe(home)),
        ..Default::default()
    };

    // Missing UserPromptSubmit
    let partial_hooks = serde_json::json!({
        "hooks": {
            "SessionStart": [{ "hooks": [{ "command": "~/.claude/hooks/pixel-session-start" }] }]
        }
    });
    fs::write(&config_path, serde_json::to_string(&partial_hooks).unwrap()).unwrap();

    let report = doctor(&doc_opts).expect("doctor runs");
    let check = report
        .checks
        .iter()
        .find(|c| c.id == "install.devin-hooks")
        .unwrap();
    assert_eq!(check.status, pixel_install::doctor::CheckStatus::Red);
    assert!(
        check
            .reason
            .as_ref()
            .unwrap()
            .contains("provider hook configuration differs")
    );

    // The new install wires NO provider hooks — it only scrubs deprecated
    // MCP entries, removes old guard hooks, deploys the agent prompt, and
    // installs shell wrappers. So the devin hooks config stays partial and
    // the install.devin-hooks check remains red after install.
    install(&InstallOptions {
        home: Some(home.to_path_buf()),
        executable_path: doc_opts.executable_path.clone(),
        dry_run: false,
    })
    .unwrap();

    let report = doctor(&doc_opts).expect("doctor runs");
    let check = report
        .checks
        .iter()
        .find(|c| c.id == "install.devin-hooks")
        .unwrap();
    assert_eq!(
        check.status,
        pixel_install::doctor::CheckStatus::Red,
        "install.devin-hooks should stay red — install no longer wires devin hooks, got {:?}: {:?}",
        check.status,
        check.reason
    );

    // The new install artifacts, however, must be green after install.
    for id in [
        "install.agent-prompt",
        "install.shell-wrappers",
        "install.old-hooks-clean",
    ] {
        let check = report.checks.iter().find(|c| c.id == id).unwrap();
        assert_eq!(
            check.status,
            pixel_install::doctor::CheckStatus::Green,
            "{id} should be green after install, got {:?}: {:?}",
            check.status,
            check.reason
        );
    }
}

#[test]
fn doctor_checks_codex_hooks_wiring() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    let config_path = home.join(pixel_install::config::CODEX_HOOKS_FILE);
    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent).unwrap();
    }

    let doc_opts = DoctorOptions {
        home: Some(home.to_path_buf()),
        executable_path: Some(fake_pixel_exe(home)),
        ..Default::default()
    };

    // Missing UserPromptSubmit
    let partial = serde_json::json!({
        "hooks": {
            "SessionStart": [{ "hooks": [{ "command": "~/.claude/hooks/pixel-session-start" }] }]
        }
    });
    fs::write(&config_path, serde_json::to_string(&partial).unwrap()).unwrap();

    let report = doctor(&doc_opts).expect("doctor runs");
    let check = report
        .checks
        .iter()
        .find(|c| c.id == "install.codex-hooks")
        .unwrap();
    assert_eq!(check.status, pixel_install::doctor::CheckStatus::Red);
    assert!(
        check
            .reason
            .as_ref()
            .unwrap()
            .contains("provider hook configuration differs")
    );

    // The new install wires NO provider hooks — it only scrubs deprecated
    // MCP entries, removes old guard hooks, deploys the agent prompt, and
    // installs shell wrappers. So the codex hooks config stays partial and
    // the install.codex-hooks check remains red after install.
    install(&InstallOptions {
        home: Some(home.to_path_buf()),
        executable_path: doc_opts.executable_path.clone(),
        dry_run: false,
    })
    .unwrap();

    let report = doctor(&doc_opts).expect("doctor runs");
    let check = report
        .checks
        .iter()
        .find(|c| c.id == "install.codex-hooks")
        .unwrap();
    assert_eq!(
        check.status,
        pixel_install::doctor::CheckStatus::Red,
        "install.codex-hooks should stay red — install no longer wires codex hooks, got {:?}: {:?}",
        check.status,
        check.reason
    );

    // The new install artifacts, however, must be green after install.
    for id in [
        "install.agent-prompt",
        "install.shell-wrappers",
        "install.old-hooks-clean",
    ] {
        let check = report.checks.iter().find(|c| c.id == id).unwrap();
        assert_eq!(
            check.status,
            pixel_install::doctor::CheckStatus::Green,
            "{id} should be green after install, got {:?}: {:?}",
            check.status,
            check.reason
        );
    }
}

#[test]
fn doctor_checks_gemini_hooks_wiring() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    let config_path = home.join(pixel_install::config::GEMINI_SETTINGS_FILE);
    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent).unwrap();
    }

    let doc_opts = DoctorOptions {
        home: Some(home.to_path_buf()),
        executable_path: None,
        ..Default::default()
    };

    // Missing BeforeAgent
    let partial = serde_json::json!({
        "hooks": {
            "BeforeTool": [{ "hooks": [{ "command": "echo existing" }] }]
        }
    });
    fs::write(&config_path, serde_json::to_string(&partial).unwrap()).unwrap();

    let report = doctor(&doc_opts).expect("doctor runs");
    let check = report
        .checks
        .iter()
        .find(|c| c.id == "install.gemini-hooks")
        .unwrap();
    assert_eq!(check.status, pixel_install::doctor::CheckStatus::Red);
    assert!(
        check
            .reason
            .as_ref()
            .unwrap()
            .contains("Gemini BeforeAgent (task boundary) hook not wired")
    );

    // With BeforeAgent
    let full = serde_json::json!({
        "hooks": {
            "BeforeTool": [{ "hooks": [{ "command": "echo existing" }] }],
            "BeforeAgent": [{ "hooks": [{ "command": "~/.claude/hooks/pixel-prompt-submit" }] }]
        }
    });
    fs::write(&config_path, serde_json::to_string(&full).unwrap()).unwrap();

    let report = doctor(&doc_opts).expect("doctor runs");
    let check = report
        .checks
        .iter()
        .find(|c| c.id == "install.gemini-hooks")
        .unwrap();
    assert_eq!(check.status, pixel_install::doctor::CheckStatus::Green);
    assert!(check.summary.contains("BeforeAgent"));
}

#[test]
fn doctor_checks_zcode_hooks_wiring() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    let config_path = home.join(pixel_install::config::ZCODE_CONFIG_FILE);
    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    let agents_md = home.join(".zcode").join("AGENTS.md");
    if let Some(parent) = agents_md.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(
        agents_md,
        format!("{}\n# pixel rules\n", pixel_install::config::MANAGED_BEGIN),
    )
    .unwrap();

    let doc_opts = DoctorOptions {
        home: Some(home.to_path_buf()),
        executable_path: None,
        ..Default::default()
    };

    // Missing UserPromptSubmit
    let partial = serde_json::json!({
        "hooks": {
            "enabled": true,
            "events": {
                "SessionStart": [{ "hooks": [{ "command": "~/.claude/hooks/pixel-session-start" }] }]
            }
        }
    });
    fs::write(&config_path, serde_json::to_string(&partial).unwrap()).unwrap();

    let report = doctor(&doc_opts).expect("doctor runs");
    let check = report
        .checks
        .iter()
        .find(|c| c.id == "install.zcode-hooks")
        .unwrap();
    assert_eq!(check.status, pixel_install::doctor::CheckStatus::Red);
    assert!(
        check
            .reason
            .as_ref()
            .unwrap()
            .contains("zcode UserPromptSubmit hook not wired")
    );

    // With UserPromptSubmit
    let full = serde_json::json!({
        "hooks": {
            "enabled": true,
            "events": {
                "SessionStart": [{ "hooks": [{ "command": "~/.claude/hooks/pixel-session-start" }] }],
                "UserPromptSubmit": [{ "hooks": [{ "command": "~/.claude/hooks/pixel-prompt-submit" }] }]
            }
        }
    });
    fs::write(&config_path, serde_json::to_string(&full).unwrap()).unwrap();

    let report = doctor(&doc_opts).expect("doctor runs");
    let check = report
        .checks
        .iter()
        .find(|c| c.id == "install.zcode-hooks")
        .unwrap();
    assert_eq!(check.status, pixel_install::doctor::CheckStatus::Green);
    assert!(
        check
            .summary
            .contains("PreToolUse, UserPromptSubmit, hooks.enabled")
    );
}

// ---------------------------------------------------------------------------
// uninstall tests
// ---------------------------------------------------------------------------

/// After uninstall, CLAUDE.md should have no managed block but the original
/// user content should be preserved. The new install no longer writes
/// managed blocks, so the fixture manually creates one (modeling a leftover
/// from a previous hook-based install) for uninstall to strip.
#[test]
fn uninstall_removes_managed_block_and_preserves_user_content() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();

    // Manually create a CLAUDE.md with user content AND a stale pixel
    // managed block (install() no longer writes these).
    let original = format!(
        "# My Project\n\nSome notes.\n\n{begin}\n# old pixel rules\n{end}\n",
        begin = MANAGED_BEGIN,
        end = MANAGED_END
    );
    fs::write(home.join("CLAUDE.md"), original).unwrap();

    let claude = fs::read_to_string(home.join("CLAUDE.md")).unwrap();
    assert!(
        claude.contains(MANAGED_BEGIN),
        "fixture should carry a managed block"
    );

    // Uninstall
    let uninstall_opts = UninstallOptions {
        home: Some(home.to_path_buf()),
        binary_path: Some(home.join("pixel")),
        dry_run: false,
    };
    let report = uninstall(&uninstall_opts).expect("uninstall");
    assert!(report.ok, "uninstall should succeed");
    assert_eq!(report.summary.red, 0, "no red steps");

    let claude = fs::read_to_string(home.join("CLAUDE.md")).unwrap();
    assert!(
        !claude.contains(MANAGED_BEGIN),
        "CLAUDE.md should have no managed block after uninstall"
    );
    assert!(
        claude.contains("Some notes."),
        "original user content should be preserved after uninstall"
    );
}

/// After uninstall, Claude settings.json should have no pixel hook entries,
/// and the hook scripts should be deleted. The new install no longer
/// installs hooks or scripts, so the fixture manually creates them
/// (modeling a leftover from a previous hook-based install) for uninstall
/// to remove.
#[test]
fn uninstall_removes_claude_hooks_and_scripts() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    let claude_dir = home.join(".claude");
    let hooks_dir = claude_dir.join("hooks");
    fs::create_dir_all(&hooks_dir).unwrap();

    // Manually wire pixel hook entries into settings.json (install() no
    // longer does this) — including the blocking guard, a session-start,
    // and a prompt-submit entry.
    let settings = claude_dir.join("settings.json");
    fs::write(
        &settings,
        serde_json::to_string_pretty(&serde_json::json!({
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [{ "type": "command", "command": "~/.claude/hooks/pixel-targets-guard" }]
                }],
                "SessionStart": [{
                    "hooks": [{ "type": "command", "command": "~/.claude/hooks/pixel-session-start" }]
                }],
                "UserPromptSubmit": [{
                    "hooks": [{ "type": "command", "command": "~/.claude/hooks/pixel-prompt-submit" }]
                }]
            }
        }))
        .unwrap(),
    )
    .unwrap();

    // Manually create the hook scripts (install() no longer does this).
    let guard_script = hooks_dir.join("pixel-targets-guard");
    let session_script = hooks_dir.join("pixel-session-start");
    let prompt_script = hooks_dir.join("pixel-prompt-submit");
    for script in [&guard_script, &session_script, &prompt_script] {
        fs::write(script, "#!/bin/sh\nexit 0\n").unwrap();
    }
    assert!(guard_script.is_file(), "fixture guard script present");
    assert!(session_script.is_file(), "fixture session script present");

    // Uninstall
    let uninstall_opts = UninstallOptions {
        home: Some(home.to_path_buf()),
        binary_path: Some(home.join("pixel")),
        dry_run: false,
    };
    uninstall(&uninstall_opts).expect("uninstall");

    // Settings should have no pixel hook references.
    let settings_content = fs::read_to_string(&settings).unwrap_or_default();
    assert!(
        !settings_content.contains("pixel-targets-guard"),
        "settings should have no pixel guard hook after uninstall"
    );
    assert!(
        !settings_content.contains("pixel-session-start"),
        "settings should have no pixel session-start hook after uninstall"
    );
    assert!(
        !settings_content.contains("pixel-prompt-submit"),
        "settings should have no pixel prompt-submit hook after uninstall"
    );

    // Hook scripts should be deleted.
    assert!(!guard_script.is_file(), "guard script should be deleted");
    assert!(
        !session_script.is_file(),
        "session-start script should be deleted"
    );
    assert!(
        !prompt_script.is_file(),
        "prompt-submit script should be deleted"
    );
}

/// Uninstall removes the pixel binary.
#[test]
fn uninstall_removes_binary() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    let bin = home.join("pixel");
    fs::write(&bin, "#!/bin/sh\nexit 0\n").unwrap();

    let opts = UninstallOptions {
        home: Some(home.to_path_buf()),
        binary_path: Some(bin.clone()),
        dry_run: false,
    };
    uninstall(&opts).expect("uninstall");

    assert!(!bin.is_file(), "binary should be deleted after uninstall");
}

/// Uninstall is idempotent: running twice does not error.
#[test]
fn uninstall_is_idempotent() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();

    let install_opts = InstallOptions {
        home: Some(home.to_path_buf()),
        executable_path: Some(fake_pixel_exe(home)),
        dry_run: false,
    };
    install(&install_opts).expect("install");

    let uninstall_opts = UninstallOptions {
        home: Some(home.to_path_buf()),
        binary_path: Some(home.join("pixel")),
        dry_run: false,
    };
    let r1 = uninstall(&uninstall_opts).expect("uninstall 1");
    assert!(r1.ok);

    // Second uninstall — should succeed, finding nothing to remove.
    let r2 = uninstall(&uninstall_opts).expect("uninstall 2");
    assert!(r2.ok, "second uninstall should succeed");
    assert_eq!(r2.summary.red, 0, "no red steps on re-uninstall");
}

/// Dry-run uninstall does not modify the filesystem. The new install no
/// longer writes managed blocks, so the fixture manually creates one (plus
/// the pixel binary) for the dry-run to report against without touching.
#[test]
fn uninstall_dry_run_does_not_modify() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();

    // Manually create a CLAUDE.md with a stale pixel managed block (install()
    // no longer writes these) and the pixel binary.
    let claude_path = home.join("CLAUDE.md");
    fs::write(
        &claude_path,
        format!(
            "# Project\n\n{begin}\n# old pixel rules\n{end}\n",
            begin = MANAGED_BEGIN,
            end = MANAGED_END
        ),
    )
    .unwrap();
    let bin = home.join("pixel");
    fs::write(&bin, "#!/bin/sh\nexit 0\n").unwrap();
    let before_claude = fs::read(&claude_path).unwrap();
    let before_bin = fs::read(&bin).unwrap();

    // Dry-run uninstall
    let uninstall_opts = UninstallOptions {
        home: Some(home.to_path_buf()),
        binary_path: Some(bin.clone()),
        dry_run: true,
    };
    let report = uninstall(&uninstall_opts).expect("dry-run uninstall");
    assert!(report.dry_run, "report should be dry-run");

    // Nothing should have changed.
    assert!(
        bin.is_file(),
        "binary should still exist after dry-run uninstall"
    );
    assert_eq!(
        fs::read(&bin).unwrap(),
        before_bin,
        "binary must be byte-identical after dry-run uninstall"
    );
    let claude = fs::read_to_string(&claude_path).unwrap();
    assert!(
        claude.contains(MANAGED_BEGIN),
        "managed block should still exist after dry-run uninstall"
    );
    assert_eq!(
        fs::read(&claude_path).unwrap(),
        before_claude,
        "CLAUDE.md must be byte-identical after dry-run uninstall"
    );
}

/// Uninstall removes pixel hook entries from Codex hooks.json while
/// preserving non-pixel entries.
#[test]
fn uninstall_removes_codex_hooks_preserving_others() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();

    // Pre-create Codex hooks.json with a pixel entry AND a non-pixel entry.
    let codex_path = home.join(".codex").join("hooks.json");
    fs::create_dir_all(codex_path.parent().unwrap()).unwrap();
    let initial = serde_json::json!({
        "hooks": {
            "PreToolUse": [
                { "matcher": "Bash", "hooks": [{ "type": "command", "command": "~/.claude/hooks/pixel-targets-guard" }] },
                { "matcher": "Bash", "hooks": [{ "type": "command", "command": "~/.claude/hooks/other-tool" }] }
            ]
        }
    });
    fs::write(&codex_path, serde_json::to_string_pretty(&initial).unwrap()).unwrap();

    let opts = UninstallOptions {
        home: Some(home.to_path_buf()),
        binary_path: Some(home.join("pixel")),
        dry_run: false,
    };
    uninstall(&opts).expect("uninstall");

    let after: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&codex_path).unwrap()).unwrap();
    let pre = after["hooks"]["PreToolUse"].as_array().unwrap();
    assert_eq!(pre.len(), 1, "only the non-pixel entry should remain");
    assert_eq!(
        pre[0]["hooks"][0]["command"].as_str().unwrap(),
        "~/.claude/hooks/other-tool",
        "the other-tool entry should be preserved"
    );
}

/// Uninstall removes the rule source file.
#[test]
fn uninstall_removes_rule_source() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();

    // Create the rule source file.
    let rules_dir = home.join(".agent-config").join("rules");
    fs::create_dir_all(&rules_dir).unwrap();
    let rule_file = rules_dir.join("pixel.md");
    fs::write(&rule_file, "# pixel rules\n").unwrap();

    let opts = UninstallOptions {
        home: Some(home.to_path_buf()),
        binary_path: Some(home.join("pixel")),
        dry_run: false,
    };
    uninstall(&opts).expect("uninstall");

    assert!(!rule_file.is_file(), "rule source file should be deleted");
}

#[test]
fn routing_full_install_rtk_round_trip_preserves_foreign_hooks() {
    let dir = TempDir::new().unwrap();
    let home = dir.path();
    let settings = home.join(".claude/settings.json");
    fs::create_dir_all(settings.parent().unwrap()).unwrap();
    let rtk = serde_json::json!({"matcher":"Bash","hooks":[{"type":"command","command":"rtk hook claude"}]});
    let foreign = serde_json::json!({"matcher":"startup","hooks":[{"type":"command","command":"keep-session-check"}]});
    let original = serde_json::json!({"hooks":{"PreToolUse":[rtk.clone()],"SessionStart":[foreign.clone()]},"unrelated":true});
    fs::write(&settings, serde_json::to_vec(&original).unwrap()).unwrap();
    let exe = fake_pixel_exe(home);
    let opts = InstallOptions {
        home: Some(home.into()),
        executable_path: Some(exe.clone()),
        dry_run: false,
    };
    // The new install wires NO hooks — it only scrubs deprecated MCP entries
    // and removes old guard hooks. The foreign RTK + SessionStart entries
    // are neither, so settings.json must pass through install untouched.
    install(&opts).unwrap();
    let once = fs::read(&settings).unwrap();
    install(&opts).unwrap();
    assert_eq!(
        fs::read(&settings).unwrap(),
        once,
        "repeat install must leave foreign hooks stable"
    );
    let installed: serde_json::Value = serde_json::from_slice(&once).unwrap();
    // No pixel delegate is added; the RTK entry survives verbatim.
    assert_eq!(
        installed["hooks"]["PreToolUse"].as_array().unwrap().len(),
        1,
        "PreToolUse should retain only the foreign RTK entry"
    );
    assert_eq!(
        installed["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap(),
        "rtk hook claude",
        "the foreign RTK entry must survive untouched (no delegate added)"
    );
    assert!(
        !installed
            .to_string()
            .contains(pixel_install::config::GUARD_HOOK),
        "install must not wire any pixel guard"
    );
    uninstall(&UninstallOptions {
        home: Some(home.into()),
        binary_path: Some(exe),
        dry_run: false,
    })
    .unwrap();
    let restored: serde_json::Value = serde_json::from_slice(&fs::read(settings).unwrap()).unwrap();
    assert_eq!(restored, original);
}

#[test]
#[cfg(unix)]
fn routing_providers_install_and_execute_without_ambient_claude() {
    for provider in ["claude", "codex", "devin"] {
        let home = TempDir::new().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "routing_isolated_provider_child", "--nocapture"])
            .env("PIXEL_INSTALL_TEST_CHILD", provider)
            .env("HOME", home.path())
            .env("PATH", "")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{provider}: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
#[cfg(unix)]
fn routing_isolated_provider_child() {
    use std::os::unix::fs::PermissionsExt;
    let Ok(provider) = std::env::var("PIXEL_INSTALL_TEST_CHILD") else {
        return;
    };
    let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap());
    let config = home.join(match provider.as_str() {
        "claude" => ".claude/settings.json",
        "codex" => ".codex/hooks.json",
        "devin" => ".config/devin/config.json",
        _ => panic!("unexpected provider"),
    });
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    fs::write(&config, "{}").unwrap();
    let bin_dir = home.join("Pixel hook tools' directory");
    fs::create_dir_all(&bin_dir).unwrap();
    let exe = bin_dir.join("pixel");
    fs::write(&exe, "#!/bin/sh\n/bin/cat >/dev/null\nprintf '%s' \"$*\"\n").unwrap();
    fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
    let opts = InstallOptions {
        home: Some(home.clone()),
        executable_path: Some(exe),
        dry_run: false,
    };
    // The new install wires NO provider hooks. The provider config must
    // pass through install untouched (still "{}"), and no hooks directory
    // or hook scripts are created for any provider.
    install(&opts).unwrap();
    let first = fs::read(&config).unwrap();
    install(&opts).unwrap();
    assert_eq!(
        fs::read(&config).unwrap(),
        first,
        "repeat install must leave the provider config stable"
    );
    assert_eq!(
        first.as_slice(),
        b"{}",
        "provider config must stay {{}} — install wires no hooks"
    );
    let value: serde_json::Value = serde_json::from_slice(&first).unwrap();
    assert!(
        value.get("hooks").is_none(),
        "no hooks key should be written by the new install, got: {value}"
    );
    // No provider gets a ~/.claude/hooks directory from the new install.
    assert!(
        !home.join(".claude/hooks").exists(),
        "install must not create ~/.claude/hooks for any provider"
    );
    // The new install artifacts (agent-prompt + shell wrappers) are deployed
    // regardless of provider.
    assert!(
        home.join(".local/share/pixel/agent-prompt.md").is_file(),
        "agent-prompt.md should be deployed"
    );
}

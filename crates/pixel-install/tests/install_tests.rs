//! Integration tests for pixel-install: doctor and install.

use std::fs;

use pixel_install::config::{MANAGED_BEGIN, MANAGED_END};
use pixel_install::doctor::{DoctorOptions, doctor};
use pixel_install::install::{InstallOptions, InstallReport, install};
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
// user's shell profile, whose location depends on the shell. Every test pins
// an explicit shell instead of inheriting the runner's `$SHELL`, so a
// developer running `cargo test` from fish gets the same result as one running
// it from zsh — and so the fish tests below exercise fish on every machine.
/// Claude Code versions on both sides of the `--append-subagent-system-prompt-file`
/// line (its CHANGELOG entry is 2.1.261; earlier releases exit 1 on the
/// unknown option).
const CLAUDE_WITH_SUBAGENT_FLAG: &str = "2.1.269";
const CLAUDE_WITHOUT_SUBAGENT_FLAG: &str = "2.1.260";

/// A fake `claude` that only answers `--version` the way Claude Code prints it.
#[cfg(unix)]
fn fake_claude_exe(dir: &std::path::Path, version: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(format!("claude-{version}"));
    fs::write(
        &path,
        format!("#!/bin/sh\nprintf '%s (Claude Code)\\n' '{version}'\n"),
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

const TEST_SHELL: &str = "/bin/zsh";
fn shell_profile_path(home: &std::path::Path) -> std::path::PathBuf {
    home.join(".zshrc")
}
const PIXEL_MANAGED_BEGIN: &str = "# >>> pixel-managed >>>";

/// This proves instruction delivery and stream preservation, not model obedience.
#[test]
#[cfg(unix)]
fn installed_metrics_guidance_reaches_wrapped_agents_without_rewriting_streams() {
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    let dir = TempDir::new().unwrap();
    let home = dir.path();
    install(&InstallOptions {
        home: Some(home.to_path_buf()),
        executable_path: Some(fake_pixel_exe(home)),
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
    })
    .unwrap();
    let prompt = fs::read_to_string(home.join(".local/share/pixel/agent-prompt.md")).unwrap();
    for required in [
        "## LIVE OPERATION METRICS",
        "same tool-call result",
        "exact line once",
        "global latest",
        "already relayed",
        "PIXEL_METRICS=0",
        "PIXEL_METRICS_ROUND_TRIP_MS",
        "sequential-v1",
        "Default `round_trip_ms` is 2000",
        "search-compat",
        "Do not invent",
    ] {
        assert!(
            prompt.contains(required),
            "missing relay contract: {required}"
        );
    }

    // Mock the agent boundary: verify the received prompt, then emit real-shape
    // tool streams. No model, external service, or paid evaluation is involved.
    let mock = r#"#!/bin/sh
case "$1" in
  --append-system-prompt-file) prompt="$(cat "$2")" || exit 82 ;;
  -c) prompt="${2#developer_instructions=}"; test "$prompt" != "$2" || exit 82 ;;
  *) exit 81 ;;
esac
printf '%s\n' "$prompt" | grep -q '## LIVE OPERATION METRICS' || exit 83
shift 2
test "$1" = 'real task with spaces' || exit 84
printf '%s\n' '{"result":"fixture"}'
printf '%s\n' '🟩 Pixel · impact · 12.4 ms · ~820 output tokens · ~3100 tokens saved (workflow estimate) · ~3.99 s saved (sequential estimate) · id=fixture-invocation' >&2
exit 7
"#;
    for agent in ["claude", "codex"] {
        let executable = home.join(agent);
        fs::write(&executable, mock).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        let result = Command::new("/bin/sh")
            .arg("-c")
            .arg(format!(". \"$1\"; {agent} 'real task with spaces'"))
            .arg("fixture")
            .arg(shell_profile_path(home))
            .env("HOME", home)
            .env("PATH", format!("{}:/usr/bin:/bin", home.display()))
            .output()
            .unwrap();
        assert_eq!(result.status.code(), Some(7), "{agent}: {result:?}");
        assert_eq!(result.stdout, b"{\"result\":\"fixture\"}\n");
        assert_eq!(
            String::from_utf8(result.stderr).unwrap(),
            "🟩 Pixel · impact · 12.4 ms · ~820 output tokens · ~3100 tokens saved (workflow estimate) · ~3.99 s saved (sequential estimate) · id=fixture-invocation\n"
        );
    }
}

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
        shell: Some(TEST_SHELL.into()),
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        ..Default::default()
    };

    let report = doctor(&options).expect("doctor runs");
    assert!(!report.checks.is_empty(), "doctor should produce checks");
    assert!(
        report.summary.green + report.summary.yellow + report.summary.red > 0,
        "summary should tally checks"
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
    fs::write(home.join("CLAUDE.md"), original.clone()).unwrap();

    let options = InstallOptions {
        home: Some(home.to_path_buf()),
        executable_path: Some(fake_pixel_exe(home)),
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
    };
    let report = install(&options).expect("install");

    assert!(report.ok, "install should succeed (ok=true)");
    assert!(report.summary.red == 0, "no red steps");
    assert!(report.summary.green > 0, "should have green steps");

    // The new install does NOT rewrite agent-config files — a stale managed
    // block from a previous install is left untouched (its cleanup is the
    // user's job, or `pixel uninstall`). Original content survives verbatim.
    let claude = fs::read_to_string(home.join("CLAUDE.md")).expect("CLAUDE.md");
    assert_eq!(
        claude, original,
        "CLAUDE.md must be byte-identical — install no longer rewrites agent configs"
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
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
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
fn install_leaves_codex_config_untouched() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    let codex_path = home.join(pixel_install::config::CODEX_HOOKS_FILE);
    fs::create_dir_all(codex_path.parent().unwrap()).unwrap();
    let original = serde_json::json!({
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
    });
    fs::write(
        &codex_path,
        serde_json::to_string_pretty(&original).unwrap(),
    )
    .unwrap();

    let options = InstallOptions {
        home: Some(home.to_path_buf()),
        executable_path: Some(fake_pixel_exe(home)),
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
    };
    install(&options).expect("install");

    // The new install does NOT touch provider hook configs — a pre-existing
    // Codex hooks.json (even one carrying a stale pixel guard) must pass
    // through install byte-identical. Cleanup of old guards is `pixel
    // uninstall`'s job now.
    let after = fs::read_to_string(&codex_path).unwrap();
    assert_eq!(
        after,
        serde_json::to_string_pretty(&original).unwrap(),
        "Codex hooks.json must be byte-identical — install no longer rewrites provider configs"
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
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
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
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        dry_run: true,
        shell: Some(TEST_SHELL.into()),
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
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
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
        shell: Some(TEST_SHELL.into()),
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
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
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
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
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
    let profile_content =
        fs::read_to_string(&profile).expect("shell profile should be created on a fresh home");
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
// doctor install-artifact check tests (agent-prompt, shell-wrappers)
// ---------------------------------------------------------------------------

#[test]
fn doctor_install_artifact_checks_red_and_green() {
    // The new install wires no hooks — doctor verifies only the two install
    // artifacts: install.agent-prompt and install.shell-wrappers. This test
    // walks each through its red and green states.
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();

    let doc_opts = DoctorOptions {
        home: Some(home.to_path_buf()),
        executable_path: None,
        shell: Some(TEST_SHELL.into()),
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
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

    // 2. Run install: deploys agent-prompt + shell wrappers → both go green.
    install(&InstallOptions {
        home: Some(home.to_path_buf()),
        executable_path: Some(fake_pixel_exe(home)),
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
    })
    .expect("install");

    let report = doctor(&doc_opts).expect("doctor runs");
    for id in ["install.agent-prompt", "install.shell-wrappers"] {
        let check = report.checks.iter().find(|c| c.id == id).unwrap();
        assert_eq!(
            check.status,
            pixel_install::doctor::CheckStatus::Green,
            "{id} should be green after install, got {:?}: {:?}",
            check.status,
            check.reason
        );
    }

    // 3. Corrupting the agent prompt makes install.agent-prompt red (stale).
    let prompt_path = home.join(".local/share/pixel/agent-prompt.md");
    fs::write(
        &prompt_path,
        "# stale prompt without the required markers\n",
    )
    .unwrap();
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
        shell: Some(TEST_SHELL.into()),
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
        shell: Some(TEST_SHELL.into()),
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
        shell: Some(TEST_SHELL.into()),
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
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
    };
    install(&install_opts).expect("install");

    let uninstall_opts = UninstallOptions {
        home: Some(home.to_path_buf()),
        binary_path: Some(home.join("pixel")),
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
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
        shell: Some(TEST_SHELL.into()),
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
        shell: Some(TEST_SHELL.into()),
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
        shell: Some(TEST_SHELL.into()),
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
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
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
        shell: Some(TEST_SHELL.into()),
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
        claude_executable: Some(fake_claude_exe(&home, CLAUDE_WITH_SUBAGENT_FLAG)),
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
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

// ---------------------------------------------------------------------------
// fish support
//
// fish reads neither ~/.zshrc nor ~/.bashrc, and rejects POSIX function
// syntax outright (`claude() { ...; }` is a parse error, `$@` does not exist).
// Before fish was handled, a fish user's `pixel install` wrote a POSIX block
// into ~/.zshrc: wrappers that never loaded, and a doctor that called them
// green. These tests pin both halves — the right file, and syntax the target
// shell actually accepts.
// ---------------------------------------------------------------------------

const FISH_SHELL: &str = "/opt/homebrew/bin/fish";

fn fish_dropin(home: &std::path::Path) -> std::path::PathBuf {
    home.join(".config/fish/conf.d/pixel.fish")
}

fn install_for_shell(home: &std::path::Path, shell: &str) {
    install(&InstallOptions {
        home: Some(home.to_path_buf()),
        executable_path: Some(fake_pixel_exe(home)),
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        dry_run: false,
        shell: Some(shell.into()),
    })
    .expect("install");
}

#[test]
fn fish_wrappers_land_in_the_fish_dropin_and_not_in_a_profile_fish_never_reads() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    install_for_shell(home, FISH_SHELL);

    let dropin = fs::read_to_string(fish_dropin(home)).expect(
        "fish wrappers belong in ~/.config/fish/conf.d/pixel.fish — fish sources conf.d \
         automatically for every session",
    );
    assert!(
        dropin.contains(PIXEL_MANAGED_BEGIN),
        "the drop-in should carry the pixel-managed block"
    );
    assert!(
        !home.join(".zshrc").exists() && !home.join(".bashrc").exists(),
        "a fish install must not write wrappers into a profile fish never sources"
    );
}

#[test]
fn fish_wrappers_are_written_in_fish_syntax_not_posix_syntax() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    install_for_shell(home, FISH_SHELL);
    let block = fs::read_to_string(fish_dropin(home)).expect("fish drop-in");

    for required in [
        "function claude\n",
        "command claude --append-system-prompt-file",
        "if contains -- --print $argv; or string match -qr -- '^-[^-]*p' $argv",
        "function codex; command codex -c",
        "| string collect) $argv; end",
        "$argv; end",
    ] {
        assert!(
            block.contains(required),
            "fish block must define functions in fish syntax, missing {required:?}:\n{block}"
        );
    }
    for forbidden in ["claude()", "codex()", "\"$@\""] {
        assert!(
            !block.contains(forbidden),
            "POSIX construct {forbidden:?} is a parse error in fish:\n{block}"
        );
    }
    assert!(
        block.contains("$HOME/.local/share/pixel/agent-prompt.md"),
        "the wrappers must still point at the deployed agent prompt:\n{block}"
    );
}

/// `<shell> -n` parses a file without executing it — the only check that
/// proves the installed block is loadable rather than merely plausible.
#[test]
#[cfg(unix)]
fn every_installed_block_parses_in_the_shell_it_was_written_for() {
    use std::process::Command;

    type ProfileOf = fn(&std::path::Path) -> std::path::PathBuf;
    let cases: [(&str, ProfileOf); 3] = [
        ("bash", |home| home.join(".bashrc")),
        ("zsh", |home| home.join(".zshrc")),
        ("fish", fish_dropin),
    ];
    let mut checked = 0;
    for (shell, profile_of) in cases {
        let dir = TempDir::new().expect("tempdir");
        let home = dir.path();
        install_for_shell(home, shell);
        let profile = profile_of(home);
        assert!(
            profile.is_file(),
            "{shell} install should have written {}",
            profile.display()
        );
        // Absent from this machine — nothing to prove here, other cases carry.
        let Ok(output) = Command::new(shell).arg("-n").arg(&profile).output() else {
            continue;
        };
        checked += 1;
        assert!(
            output.status.success(),
            "{shell} rejects the block pixel wrote for it in {}:\n{}",
            profile.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert!(
        checked > 0,
        "no shell was available to parse-check against — the assertion above never ran"
    );
}

#[test]
fn doctor_reads_the_profile_of_the_shell_it_is_asked_about() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    install_for_shell(home, FISH_SHELL);

    let wrappers_check = |shell: &str| {
        let report = doctor(&DoctorOptions {
            home: Some(home.to_path_buf()),
            executable_path: None,
            shell: Some(shell.into()),
            claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
            ..Default::default()
        })
        .expect("doctor runs");
        report
            .checks
            .iter()
            .find(|c| c.id == "install.shell-wrappers")
            .expect("shell-wrappers check")
            .clone()
    };

    assert_eq!(
        wrappers_check(FISH_SHELL).status,
        pixel_install::doctor::CheckStatus::Green,
        "a fish install must read back green for fish"
    );
    assert_eq!(
        wrappers_check("/bin/zsh").status,
        pixel_install::doctor::CheckStatus::Red,
        "nothing was installed for zsh — its profile does not exist"
    );
}

#[test]
fn doctor_refuses_to_green_a_posix_block_sitting_in_the_fish_dropin() {
    // Reproduces the shipped bug in its observable form: the markers are
    // present in the file fish sources, so a marker-only check calls this
    // installed. fish cannot parse a line of it.
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    install_for_shell(home, "/bin/zsh");
    let posix_block = fs::read_to_string(home.join(".zshrc")).expect("zsh profile");
    fs::create_dir_all(fish_dropin(home).parent().unwrap()).unwrap();
    fs::write(fish_dropin(home), &posix_block).unwrap();

    let report = doctor(&DoctorOptions {
        home: Some(home.to_path_buf()),
        executable_path: None,
        shell: Some(FISH_SHELL.into()),
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        ..Default::default()
    })
    .expect("doctor runs");
    let check = report
        .checks
        .iter()
        .find(|c| c.id == "install.shell-wrappers")
        .expect("shell-wrappers check");
    assert_eq!(
        check.status,
        pixel_install::doctor::CheckStatus::Red,
        "a POSIX block in the fish drop-in is not a working install, got {:?}",
        check.reason
    );
}

#[test]
fn uninstall_deletes_the_fish_dropin_it_created() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    install_for_shell(home, FISH_SHELL);
    assert!(
        fish_dropin(home).is_file(),
        "precondition: drop-in installed"
    );

    uninstall(&UninstallOptions {
        home: Some(home.to_path_buf()),
        binary_path: None,
        dry_run: false,
        shell: Some(FISH_SHELL.into()),
    })
    .expect("uninstall");

    assert!(
        !fish_dropin(home).exists(),
        "pixel created the drop-in and owns all of it — an empty leftover file is litter"
    );
}

#[test]
fn a_shell_path_that_merely_contains_fish_is_not_treated_as_fish() {
    // Substring matching on the whole path is what made `bash` detection
    // sloppy; a user whose home is /home/fisher must still get zsh wrappers.
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    install_for_shell(home, "/home/fisher/bin/zsh");

    assert!(
        home.join(".zshrc").is_file(),
        "the shell's executable name is zsh — the block belongs in ~/.zshrc"
    );
    assert!(
        !fish_dropin(home).exists(),
        "a path containing 'fish' is not a fish shell"
    );
}

#[test]
fn a_shell_path_that_merely_contains_bash_is_not_treated_as_bash() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    install_for_shell(home, "/home/bashful/bin/zsh");

    assert!(
        home.join(".zshrc").is_file(),
        "the shell's executable name is zsh — the block belongs in ~/.zshrc"
    );
    assert!(
        !home.join(".bashrc").exists(),
        "a path containing 'bash' is not a bash shell"
    );
}

// ---------------------------------------------------------------------------
// sub-agent prompt: `--append-subagent-system-prompt-file`
//
// Claude Code sub-agents receive neither the session's
// `--append-system-prompt-file` nor its history, so the `claude` wrapper's
// agent prompt never reaches them. Claude Code honours
// `--append-subagent-system-prompt-file` in print mode only, and the same
// wrapper fronts interactive sessions, so the flag is added exactly when
// `-p`/`--print` is among the arguments. These tests pin the asset, the
// argument-dependent flag in every supported shell, and the uninstall path.
// ---------------------------------------------------------------------------

const SUBAGENT_PROMPT_ASSET: &str = include_str!("../assets/pixel-subagent-prompt.md");

fn subagent_prompt_path(home: &std::path::Path) -> std::path::PathBuf {
    home.join(".local/share/pixel/subagent-prompt.md")
}

#[test]
fn install_deploys_the_bundled_subagent_prompt_under_two_kilobytes() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    install_for_shell(home, TEST_SHELL);

    let deployed = fs::read_to_string(subagent_prompt_path(home))
        .expect("subagent-prompt.md should be deployed next to agent-prompt.md");
    assert_eq!(
        deployed, SUBAGENT_PROMPT_ASSET,
        "the deployed file must be the bundled asset, byte for byte"
    );
    // A long sub-agent prompt loses to a long agent body; the whole point of
    // a separate file is that it stays short enough to be obeyed.
    assert!(
        deployed.len() <= 2048,
        "subagent-prompt.md must stay under 2 KB, is {} bytes",
        deployed.len()
    );
    for stale in ["--callers", "--callees", "~/.local/bin/pixel", "MANDATORY"] {
        assert!(
            !deployed.contains(stale),
            "sub-agent prompt must not carry syntax the CLI rejects or an install path that is often wrong: {stale}"
        );
    }
}

#[test]
fn dry_run_does_not_write_the_subagent_prompt() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    let report = install(&InstallOptions {
        home: Some(home.to_path_buf()),
        executable_path: Some(fake_pixel_exe(home)),
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        dry_run: true,
        shell: Some(TEST_SHELL.into()),
    })
    .expect("dry-run install");
    assert!(report.dry_run);
    assert!(
        !subagent_prompt_path(home).exists(),
        "dry-run must not deploy subagent-prompt.md"
    );
    let step = report
        .steps
        .iter()
        .find(|s| s.id == "agent-prompt")
        .expect("agent-prompt step");
    assert!(
        step.summary.contains("subagent-prompt.md"),
        "the dry-run report must announce the sub-agent prompt it would deploy: {}",
        step.summary
    );
}

/// Source the installed block in the shell it was written for, call the
/// `claude` wrapper through a mock that echoes its argv, and check which
/// prompt files it received. Runs in every shell found on the caller's PATH
/// (the mock's directory is prepended, the rest of PATH is kept so that a
/// Homebrew fish is found); a shell that cannot be spawned is skipped.
#[test]
#[cfg(unix)]
fn the_subagent_prompt_flag_is_passed_in_print_mode_only() {
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    type ProfileOf = fn(&std::path::Path) -> std::path::PathBuf;
    let cases: [(&str, ProfileOf); 3] = [
        ("bash", |home| home.join(".bashrc")),
        ("zsh", |home| home.join(".zshrc")),
        ("fish", fish_dropin),
    ];
    let mock = "#!/bin/sh\nfor a in \"$@\"; do printf '%s\\n' \"$a\"; done\n";
    let mut checked = 0;
    for (shell, profile_of) in cases {
        let dir = TempDir::new().expect("tempdir");
        let home = dir.path();
        install_for_shell(home, shell);
        let mock_path = home.join("claude");
        fs::write(&mock_path, mock).unwrap();
        fs::set_permissions(&mock_path, fs::Permissions::from_mode(0o755)).unwrap();

        let run = |args: &[&str]| -> Option<Vec<String>> {
            let quoted: Vec<String> = args.iter().map(|a| format!("'{a}'")).collect();
            let script = format!(
                "source '{}'; claude {}",
                profile_of(home).display(),
                quoted.join(" ")
            );
            let inherited = std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into());
            let output = Command::new(shell)
                .arg("-c")
                .arg(script)
                .env("HOME", home)
                .env("PATH", format!("{}:{inherited}", home.display()))
                .output()
                .ok()?;
            assert!(
                output.status.success(),
                "{shell}: wrapper failed for {args:?}:\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
            Some(
                String::from_utf8(output.stdout)
                    .unwrap()
                    .lines()
                    .map(str::to_string)
                    .collect(),
            )
        };
        // Absent from this machine — the other shells carry the assertion.
        let Some(interactive) = run(&["real task"]) else {
            continue;
        };
        checked += 1;
        let agent_prompt = home
            .join(".local/share/pixel/agent-prompt.md")
            .display()
            .to_string();
        let subagent_prompt = subagent_prompt_path(home).display().to_string();

        assert!(
            interactive
                .windows(2)
                .any(|w| w == ["--append-system-prompt-file", agent_prompt.as_str()]),
            "{shell}: interactive call must still carry the agent prompt: {interactive:?}"
        );
        assert!(
            !interactive
                .iter()
                .any(|a| a == "--append-subagent-system-prompt-file"),
            "{shell}: Claude Code ignores --append-subagent-system-prompt-file outside print mode, \
             so an interactive call must not carry it: {interactive:?}"
        );
        assert_eq!(
            interactive.last().map(String::as_str),
            Some("real task"),
            "{shell}: user arguments must survive untouched: {interactive:?}"
        );

        // Claude Code splits short-flag clusters, so `-pc` and `-cp` are
        // print mode too (verified: `claude -pv` prints the version).
        for print_flag in ["-p", "--print", "-pc", "-cp"] {
            let print = run(&["--model", "sonnet", print_flag, "real task"]).unwrap();
            assert!(
                print
                    .windows(2)
                    .any(|w| w == ["--append-system-prompt-file", agent_prompt.as_str()]),
                "{shell}: print-mode call must carry the agent prompt: {print:?}"
            );
            assert!(
                print.windows(2).any(|w| w
                    == [
                        "--append-subagent-system-prompt-file",
                        subagent_prompt.as_str()
                    ]),
                "{shell}: `{print_flag}` anywhere in the arguments must add the sub-agent prompt: {print:?}"
            );
            assert_eq!(
                print
                    .iter()
                    .filter(|a| *a == "--append-subagent-system-prompt-file")
                    .count(),
                1,
                "{shell}: the flag must be added exactly once: {print:?}"
            );
            assert_eq!(
                &print[print.len() - 4..],
                ["--model", "sonnet", print_flag, "real task"],
                "{shell}: user arguments must survive in order: {print:?}"
            );
        }
    }
    assert!(
        checked > 0,
        "no shell was available to run the wrapper in — the assertions above never ran"
    );
    if Command::new("fish").arg("--version").output().is_ok() {
        assert_eq!(
            checked, 3,
            "fish is on PATH but the fish block was not exercised"
        );
    }
}

/// Codex has no file-backed `developer_instructions`, so the wrapper reads the
/// prompt at call time and passes it inline. Every byte must survive the
/// shell (quotes, backslashes, `$`, backticks, `#`, blank lines, ~12 KB) and
/// arrive as ONE `-c` argument: a split or an expansion would hand Codex a
/// truncated protocol or a spurious extra argument. Runs the installed block
/// in every shell found on the caller's PATH; a shell that cannot be spawned
/// is skipped.
#[test]
#[cfg(unix)]
fn the_codex_wrapper_passes_the_agent_prompt_verbatim_as_developer_instructions() {
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    type ProfileOf = fn(&std::path::Path) -> std::path::PathBuf;
    let cases: [(&str, ProfileOf); 3] = [
        ("bash", |home| home.join(".bashrc")),
        ("zsh", |home| home.join(".zshrc")),
        ("fish", fish_dropin),
    ];
    // A fake `codex` that records its argv: one file per argument, so a
    // prompt split into several arguments shows up as extra files.
    let mock = "#!/bin/sh\nn=0\nfor a in \"$@\"; do n=$((n+1)); printf '%s' \"$a\" > \"$CODEX_ARGV_DIR/$n\"; done\n";
    // Every character class that a shell could mangle between `cat` and
    // the callee, plus a trailing run of newlines (command substitution
    // strips those, and only those).
    let tricky = "# heading with \"double\" and 'single' quotes\n\
                  \\backslash \\\\double $HOME ${VAR} $(rm -rf /) `backtick` #hash %s\n\
                  \n\
                  \x20 indented; semicolon | pipe & ampersand > redirect * glob ? {a,b}\n\
                  tab\there — unicode 🟩 Pixel ·\n\
                  last line without a newline\n\n\n";
    let mut checked = 0;
    for (shell, profile_of) in cases {
        let dir = TempDir::new().expect("tempdir");
        let home = dir.path();
        install_for_shell(home, shell);
        let mock_path = home.join("codex");
        fs::write(&mock_path, mock).unwrap();
        fs::set_permissions(&mock_path, fs::Permissions::from_mode(0o755)).unwrap();
        let prompt_path = home.join(".local/share/pixel/agent-prompt.md");
        let deployed = fs::read_to_string(&prompt_path).unwrap();

        let run = |prompt: &str, args: &[&str]| -> Option<Vec<String>> {
            fs::write(&prompt_path, prompt).unwrap();
            let argv_dir = TempDir::new().unwrap();
            let quoted: Vec<String> = args.iter().map(|a| format!("'{a}'")).collect();
            let script = format!(
                "source '{}'; codex {}",
                profile_of(home).display(),
                quoted.join(" ")
            );
            let inherited = std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into());
            let output = Command::new(shell)
                .arg("-c")
                .arg(script)
                .env("HOME", home)
                .env("PATH", format!("{}:{inherited}", home.display()))
                .env("CODEX_ARGV_DIR", argv_dir.path())
                .output()
                .ok()?;
            assert!(
                output.status.success(),
                "{shell}: wrapper failed for {args:?}:\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let mut argv = Vec::new();
            for n in 1.. {
                let Ok(arg) = fs::read_to_string(argv_dir.path().join(n.to_string())) else {
                    break;
                };
                argv.push(arg);
            }
            Some(argv)
        };
        // Absent from this machine — the other shells carry the assertion.
        let Some(argv) = run(&deployed, &["exec", "real task with spaces"]) else {
            continue;
        };
        checked += 1;
        assert_eq!(
            argv,
            [
                "-c",
                &format!("developer_instructions={}", deployed.trim_end_matches('\n')),
                "exec",
                "real task with spaces",
            ],
            "{shell}: the deployed prompt must reach codex as one -c value, \
             followed by the user's arguments in order"
        );
        let argv = run(tricky, &["exec"]).unwrap();
        assert_eq!(
            argv,
            [
                "-c",
                &format!("developer_instructions={}", tricky.trim_end_matches('\n')),
                "exec",
            ],
            "{shell}: quotes, backslashes, `$`, backticks, `#`, globs and blank \
             lines must survive byte for byte"
        );
    }
    assert!(
        checked > 0,
        "no shell was available to run the wrapper in — the assertions above never ran"
    );
    if Command::new("fish").arg("--version").output().is_ok() {
        assert_eq!(
            checked, 3,
            "fish is on PATH but the fish block was not exercised"
        );
    }
}

/// The prompt travels as a single `execve` argument. Linux caps one argument
/// at `MAX_ARG_STRLEN` = 32 pages = 128 KiB (macOS has no per-argument cap,
/// only the 1 MiB `ARG_MAX` total); past it `codex` fails with E2BIG for
/// every call. Codex also trims the value and strips one leading/trailing
/// `"` or `'` when it is not valid TOML, so the asset must not start or end
/// with either.
#[test]
fn the_agent_prompt_fits_in_one_codex_argument() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    install_for_shell(home, TEST_SHELL);
    let prompt = fs::read_to_string(home.join(".local/share/pixel/agent-prompt.md")).unwrap();
    let argument = format!("developer_instructions={}", prompt.trim_end_matches('\n'));
    const MAX_ARG_STRLEN: usize = 32 * 4096;
    assert!(
        argument.len() <= MAX_ARG_STRLEN / 2,
        "the codex -c argument is {} bytes; past {} bytes (half of Linux's MAX_ARG_STRLEN) \
         switch the codex wrapper to a file-backed mechanism",
        argument.len(),
        MAX_ARG_STRLEN / 2
    );
    let trimmed = prompt.trim();
    assert!(
        !trimmed.starts_with(['"', '\'']) && !trimmed.ends_with(['"', '\'']),
        "codex -c strips a leading/trailing quote from a raw string value"
    );
    assert!(
        trimmed.starts_with('#'),
        "the prompt must not parse as a TOML value; a leading `#` guarantees the raw-string path"
    );
}

#[test]
fn reinstall_keeps_one_managed_block_with_one_subagent_flag() {
    for shell in [TEST_SHELL, FISH_SHELL] {
        let dir = TempDir::new().expect("tempdir");
        let home = dir.path();
        install_for_shell(home, shell);
        install_for_shell(home, shell);
        install_for_shell(home, shell);
        let profile = match shell {
            FISH_SHELL => fish_dropin(home),
            _ => shell_profile_path(home),
        };
        let content = fs::read_to_string(&profile).expect("profile");
        assert_eq!(
            content.matches(PIXEL_MANAGED_BEGIN).count(),
            1,
            "{shell}: re-installs must replace the managed block, not stack it:\n{content}"
        );
        assert_eq!(
            content
                .matches("--append-subagent-system-prompt-file")
                .count(),
            1,
            "{shell}: the block carries the sub-agent flag on exactly one branch:\n{content}"
        );
        assert!(
            content.contains("$HOME/.local/share/pixel/subagent-prompt.md"),
            "{shell}: the flag must point at the deployed sub-agent prompt:\n{content}"
        );
    }
}

#[test]
fn doctor_flags_a_missing_or_stale_subagent_prompt() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    install_for_shell(home, TEST_SHELL);
    let check = |home: &std::path::Path| {
        doctor(&DoctorOptions {
            home: Some(home.to_path_buf()),
            shell: Some(TEST_SHELL.into()),
            claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
            ..Default::default()
        })
        .expect("doctor")
        .checks
        .into_iter()
        .find(|c| c.id == "install.subagent-prompt")
        .expect("install.subagent-prompt check")
        .status
    };
    assert_eq!(
        check(home),
        pixel_install::doctor::CheckStatus::Green,
        "freshly installed sub-agent prompt must be green"
    );
    fs::write(subagent_prompt_path(home), "pixel uses X --callers\n").unwrap();
    assert_eq!(
        check(home),
        pixel_install::doctor::CheckStatus::Red,
        "a sub-agent prompt that differs from the bundled asset is stale: every print-mode \
         sub-agent would be taught it"
    );
    fs::remove_file(subagent_prompt_path(home)).unwrap();
    assert_eq!(
        check(home),
        pixel_install::doctor::CheckStatus::Red,
        "the wrapper passes a path that no longer exists"
    );
}

#[test]
fn uninstall_removes_the_subagent_prompt() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    install_for_shell(home, TEST_SHELL);
    assert!(
        subagent_prompt_path(home).is_file(),
        "precondition: sub-agent prompt deployed"
    );

    uninstall(&UninstallOptions {
        home: Some(home.to_path_buf()),
        binary_path: None,
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
    })
    .expect("uninstall");

    assert!(
        !subagent_prompt_path(home).exists(),
        "uninstall must remove subagent-prompt.md with the wrapper that referenced it"
    );
    assert!(
        !home.join(".local/share/pixel/agent-prompt.md").exists(),
        "agent-prompt.md is removed alongside"
    );
}

// ---------------------------------------------------------------------------
// Claude Code version gate for the sub-agent prompt flag
//
// Claude Code accepts `--append-subagent-system-prompt-file` from 2.1.261.
// Older releases exit 1 with `error: unknown option`, so a wrapper that
// always passed it would break every `claude -p` call for them. The wrapper
// is therefore chosen at install time from `claude --version`, and doctor
// re-derives the expected block from the Claude Code found at check time.
// ---------------------------------------------------------------------------

fn install_with_claude(
    home: &std::path::Path,
    claude: Option<std::path::PathBuf>,
) -> InstallReport {
    install(&InstallOptions {
        home: Some(home.to_path_buf()),
        executable_path: Some(fake_pixel_exe(home)),
        claude_executable: claude,
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
    })
    .expect("install")
}

fn wrappers_step(report: &InstallReport) -> &pixel_install::install::InstallStep {
    report
        .steps
        .iter()
        .find(|s| s.id == "shell-wrappers")
        .expect("shell-wrappers step")
}

#[test]
#[cfg(unix)]
fn the_subagent_flag_is_written_only_for_claude_code_at_least_2_1_261() {
    for (version, expect_flag) in [
        ("2.1.261", true),
        ("2.1.269", true),
        ("2.2.0", true),
        ("3.0.0", true),
        ("2.1.260", false),
        // Numeric comparison: "2.1.99" sorts after "2.1.261" as text.
        ("2.1.99", false),
        ("1.9.9", false),
    ] {
        let dir = TempDir::new().expect("tempdir");
        let home = dir.path();
        let report = install_with_claude(home, Some(fake_claude_exe(home, version)));
        let profile = fs::read_to_string(shell_profile_path(home)).expect("profile");
        assert_eq!(
            profile.contains("--append-subagent-system-prompt-file"),
            expect_flag,
            "Claude Code {version}: flag expected={expect_flag}\n{profile}"
        );
        assert!(
            profile.contains("--append-system-prompt-file"),
            "Claude Code {version}: the session prompt is wired either way"
        );
        let step = wrappers_step(&report);
        if expect_flag {
            assert_eq!(step.status, pixel_install::install::CheckStatus::Green);
        } else {
            assert_eq!(
                step.status,
                pixel_install::install::CheckStatus::Yellow,
                "an old Claude Code must be reported, not silently accommodated"
            );
            assert!(
                step.summary.contains("2.1.261") && step.summary.contains(version),
                "the summary must name the version found and the one required: {}",
                step.summary
            );
        }
        assert!(
            subagent_prompt_path(home).is_file(),
            "the prompt file is deployed regardless, so a later `pixel install` only rewrites the block"
        );
    }
}

#[test]
#[cfg(unix)]
fn no_usable_claude_means_no_subagent_flag() {
    // Missing binary: the safe side is the plain wrapper. Passing the flag
    // to an unknown Claude Code could break every print-mode call.
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    let report = install_with_claude(home, Some(home.join("no-such-claude")));
    let profile = fs::read_to_string(shell_profile_path(home)).expect("profile");
    assert!(!profile.contains("--append-subagent-system-prompt-file"));
    assert_eq!(
        wrappers_step(&report).status,
        pixel_install::install::CheckStatus::Yellow
    );

    // A claude that prints something unparseable is treated the same way.
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    let odd = fake_claude_exe(home, "unknown");
    let report = install_with_claude(home, Some(odd));
    let profile = fs::read_to_string(shell_profile_path(home)).expect("profile");
    assert!(!profile.contains("--append-subagent-system-prompt-file"));
    assert_eq!(
        wrappers_step(&report).status,
        pixel_install::install::CheckStatus::Yellow
    );
}

#[test]
#[cfg(unix)]
fn a_reinstall_without_claude_keeps_the_flag_an_earlier_install_proved() {
    // No `claude` on the PATH of the re-installing process (cron, an agent's
    // command tool) is no evidence that Claude Code got older: stripping the
    // flag there would silently drop sub-agent rules from a working setup.
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    install_with_claude(home, Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)));
    let before = fs::read_to_string(shell_profile_path(home)).expect("profile");
    assert!(before.contains("--append-subagent-system-prompt-file"));

    let report = install_with_claude(home, Some(home.join("no-such-claude")));
    let after = fs::read_to_string(shell_profile_path(home)).expect("profile");
    assert_eq!(
        after, before,
        "the block must survive a blind re-install unchanged"
    );
    let step = wrappers_step(&report);
    assert_eq!(step.status, pixel_install::install::CheckStatus::Yellow);
    assert!(
        step.summary.contains("keeping the sub-agent prompt flag"),
        "the report must say the flag was kept on trust: {}",
        step.summary
    );

    // The reverse is not preserved by accident: a block proven old by a real
    // old Claude Code stays plain on a blind re-install.
    install_with_claude(
        home,
        Some(fake_claude_exe(home, CLAUDE_WITHOUT_SUBAGENT_FLAG)),
    );
    install_with_claude(home, Some(home.join("no-such-claude")));
    assert!(
        !fs::read_to_string(shell_profile_path(home))
            .unwrap()
            .contains("--append-subagent-system-prompt-file")
    );
}

#[test]
#[cfg(unix)]
fn doctor_without_claude_is_yellow_not_red() {
    // A red would send the user to `pixel install`, which has no better
    // evidence — and used to strip the flag.
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    install_with_claude(home, Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)));
    let check = doctor(&DoctorOptions {
        home: Some(home.to_path_buf()),
        shell: Some(TEST_SHELL.into()),
        claude_executable: Some(home.join("no-such-claude")),
        ..Default::default()
    })
    .expect("doctor")
    .checks
    .into_iter()
    .find(|c| c.id == "install.shell-wrappers")
    .expect("install.shell-wrappers check");
    assert_eq!(check.status, pixel_install::doctor::CheckStatus::Yellow);
    assert!(
        check
            .summary
            .contains("cannot verify the sub-agent prompt flag"),
        "{check:?}"
    );

    // A foreign block is still red even when claude is unknown.
    fs::write(
        shell_profile_path(home),
        "# >>> pixel-managed >>>\nclaude() { command claude \"$@\"; }\n# <<< pixel-managed <<<\n",
    )
    .unwrap();
    let check = doctor(&DoctorOptions {
        home: Some(home.to_path_buf()),
        shell: Some(TEST_SHELL.into()),
        claude_executable: Some(home.join("no-such-claude")),
        ..Default::default()
    })
    .expect("doctor")
    .checks
    .into_iter()
    .find(|c| c.id == "install.shell-wrappers")
    .expect("install.shell-wrappers check");
    assert_eq!(check.status, pixel_install::doctor::CheckStatus::Red);
}

#[test]
#[cfg(unix)]
fn the_plain_wrapper_for_old_claude_is_the_0_2_2_block() {
    // A 0.2.2 install in front of an old Claude Code must read green after
    // upgrading pixel, without a reinstall: the block pixel writes for that
    // Claude Code is byte-identical to what 0.2.2 wrote.
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    install_with_claude(
        home,
        Some(fake_claude_exe(home, CLAUDE_WITHOUT_SUBAGENT_FLAG)),
    );
    let profile = fs::read_to_string(shell_profile_path(home)).expect("profile");
    assert!(profile.contains(
        "claude() { command claude --append-system-prompt-file \"$HOME/.local/share/pixel/agent-prompt.md\" \"$@\"; }"
    ), "{profile}");
}

#[test]
#[cfg(unix)]
fn doctor_reds_a_block_that_disagrees_with_the_claude_code_found_now() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    let new_claude = fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG);
    let old_claude = fake_claude_exe(home, CLAUDE_WITHOUT_SUBAGENT_FLAG);
    let check = |claude: &std::path::Path| {
        doctor(&DoctorOptions {
            home: Some(home.to_path_buf()),
            shell: Some(TEST_SHELL.into()),
            claude_executable: Some(claude.to_path_buf()),
            ..Default::default()
        })
        .expect("doctor")
        .checks
        .into_iter()
        .find(|c| c.id == "install.shell-wrappers")
        .expect("install.shell-wrappers check")
    };

    // Installed against a new Claude Code: green now, red if Claude Code is
    // downgraded (the flag would make every `claude -p` exit 1).
    install_with_claude(home, Some(new_claude.clone()));
    assert_eq!(
        check(&new_claude).status,
        pixel_install::doctor::CheckStatus::Green
    );
    let downgraded = check(&old_claude);
    assert_eq!(downgraded.status, pixel_install::doctor::CheckStatus::Red);
    assert!(
        downgraded
            .reason
            .as_deref()
            .unwrap_or_default()
            .contains("pass the sub-agent prompt flag"),
        "{downgraded:?}"
    );

    // Installed against an old Claude Code: green now, red once Claude Code
    // is updated (sub-agents would silently miss the rules).
    install_with_claude(home, Some(old_claude.clone()));
    assert_eq!(
        check(&old_claude).status,
        pixel_install::doctor::CheckStatus::Green
    );
    let upgraded = check(&new_claude);
    assert_eq!(upgraded.status, pixel_install::doctor::CheckStatus::Red);
    assert!(
        upgraded
            .reason
            .as_deref()
            .unwrap_or_default()
            .contains("lack the sub-agent prompt flag"),
        "{upgraded:?}"
    );
}

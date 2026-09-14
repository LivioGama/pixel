//! Integration tests for pixel-install: doctor and install.

use std::fs;

use pixel_install::config::{MANAGED_BEGIN, MANAGED_END};
use pixel_install::doctor::{CheckStatus, DoctorOptions, doctor};
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
  *) exit 81 ;;
esac
printf '%s\n' "$prompt" | grep -q '## LIVE OPERATION METRICS' || exit 83
shift 2
test "$1" = 'real task with spaces' || exit 84
printf '%s\n' '{"result":"fixture"}'
printf '%s\n' '🟩 Pixel · impact · 12.4 ms · ~820 output tokens · ~3100 tokens saved (workflow estimate) · ~3.99 s saved (sequential estimate) · id=fixture-invocation' >&2
exit 7
"#;
    // Codex reads the prompt from config.toml itself: the value the file
    // carries is what its developer message gets.
    let codex_value = codex_developer_instructions(home).expect("developer_instructions written");
    assert!(
        codex_value.contains("## LIVE OPERATION METRICS"),
        "the relay contract must reach codex through config.toml"
    );
    let agent = "claude";
    {
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
        "# My Project\n\nSome notes.\n\n{MANAGED_BEGIN}\n# old pixel rules\n{MANAGED_END}\n"
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
        .filter_map(std::result::Result::ok)
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
        !profile_content.contains("codex"),
        "codex is wired through config.toml, not a shell function:\n{profile_content}"
    );
    assert!(
        codex_developer_instructions(home).is_some_and(|v| v.contains("MANDATORY WORKFLOW")),
        "a fresh install must write the agent prompt into ~/.codex/config.toml"
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

    // 3. A prompt edited after deployment is stale, even though it still
    //    carries the two headline sections the old substring heuristic looked
    //    for: the check asserts byte equality with the bundled asset.
    let prompt_path = home.join(".local/share/pixel/agent-prompt.md");
    let mut edited = fs::read_to_string(&prompt_path).expect("agent-prompt deployed");
    assert!(
        edited.contains("REPLACEMENT MAP") && edited.contains("MANDATORY WORKFLOW"),
        "fixture: the edited prompt must still satisfy the old heuristic"
    );
    edited.push_str("\nOne extra rule the bundled prompt does not carry.\n");
    fs::write(&prompt_path, &edited).unwrap();
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

/// Dry-run "parser" standing in for the CLI's clap definition: rejects the
/// one subcommand the tests plant, accepts everything else.
fn stub_validator(argv: &[String]) -> Result<(), String> {
    if argv.iter().any(|a| a == "bogus-subcommand") {
        Err("unrecognized subcommand 'bogus-subcommand'".into())
    } else {
        Ok(())
    }
}

fn check<'a>(
    report: &'a pixel_install::doctor::DoctorReport,
    id: &str,
) -> &'a pixel_install::doctor::DoctorCheck {
    report
        .checks
        .iter()
        .find(|c| c.id == id)
        .unwrap_or_else(|| panic!("doctor has no {id} check"))
}

/// 0.2.x installs write no managed block: the rule text agents receive is
/// the deployed `agent-prompt.md`, so that is what `rule.parity` and
/// `rule.scenarios` must validate. Before this, both reported yellow "no
/// installed rule text" on every current install, and a prompt documenting
/// a command line the binary rejects went unnoticed.
#[test]
fn doctor_rule_checks_validate_the_deployed_agent_prompt() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    let doc_opts = DoctorOptions {
        home: Some(home.to_path_buf()),
        executable_path: None,
        shell: Some(TEST_SHELL.into()),
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        syntax_validator: Some(stub_validator),
        ..Default::default()
    };

    // 1. Nothing installed: no rule text anywhere → yellow, pointing at install.
    let report = doctor(&doc_opts).expect("doctor runs");
    for id in ["rule.parity", "rule.scenarios"] {
        let c = check(&report, id);
        assert_eq!(
            c.status,
            pixel_install::doctor::CheckStatus::Yellow,
            "{id}: {c:?}"
        );
        assert!(
            c.summary.contains("agent-prompt.md") && c.summary.contains("pixel install"),
            "{id} names the missing artifact and the fix: {}",
            c.summary
        );
    }

    // 2. A plain install (no CLAUDE.md, no managed block): both checks read
    //    the deployed prompt and go green.
    install(&InstallOptions {
        home: Some(home.to_path_buf()),
        executable_path: Some(fake_pixel_exe(home)),
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
    })
    .expect("install");
    let prompt_path = home.join(".local/share/pixel/agent-prompt.md");
    let report = doctor(&doc_opts).expect("doctor runs");
    for id in ["rule.parity", "rule.scenarios"] {
        let c = check(&report, id);
        assert_eq!(
            c.status,
            pixel_install::doctor::CheckStatus::Green,
            "{id} after install: {:?} {:?}",
            c.summary,
            c.reason
        );
        assert_eq!(
            c.detail.as_ref().and_then(|d| d["source"].as_str()),
            Some(prompt_path.to_str().unwrap()),
            "{id} validated the deployed prompt"
        );
    }
    let parity = check(&report, "rule.parity");
    let parsed = parity.detail.as_ref().unwrap()["parsed_ok"]
        .as_u64()
        .unwrap();
    assert!(
        parsed > 0,
        "the deployed prompt documents pixel command lines: {parity:?}"
    );

    // 3. A deployed prompt documenting a command the binary rejects is red,
    //    even with a legacy managed block that would pass: the deployed
    //    prompt is what agents read, so it wins over the older text.
    let prompt = fs::read_to_string(&prompt_path).unwrap();
    fs::write(
        &prompt_path,
        format!(
            "{prompt}
```bash
pixel bogus-subcommand .
```
"
        ),
    )
    .unwrap();
    fs::write(
        home.join("CLAUDE.md"),
        format!(
            "# mine
{MANAGED_BEGIN}
```bash
pixel scope-task task
pixel find-code x
pixel plan-rollback
pixel sync-branch
pixel impact x
```
{MANAGED_END}
"
        ),
    )
    .unwrap();
    let report = doctor(&doc_opts).expect("doctor runs");
    let parity = check(&report, "rule.parity");
    assert_eq!(
        parity.status,
        pixel_install::doctor::CheckStatus::Red,
        "{parity:?}"
    );
    assert!(
        parity
            .reason
            .as_deref()
            .unwrap_or_default()
            .contains("bogus-subcommand"),
        "the rejected line is named: {:?}",
        parity.reason
    );
    // Red at all proves the precedence: the managed block alone parses
    // green, so the rejected line can only have come from the deployed
    // prompt (a red check carries no detail to name its source).

    // 4. With the deployed prompt gone, the legacy managed block is the
    //    fallback for installs that predate agent-prompt.md.
    fs::remove_file(&prompt_path).unwrap();
    let report = doctor(&doc_opts).expect("doctor runs");
    let parity = check(&report, "rule.parity");
    assert_eq!(
        parity.status,
        pixel_install::doctor::CheckStatus::Green,
        "{parity:?}"
    );
    assert_eq!(
        parity.detail.as_ref().and_then(|d| d["source"].as_str()),
        Some(home.join("CLAUDE.md").to_str().unwrap()),
        "legacy managed block is read when no prompt is deployed"
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
        "# My Project\n\nSome notes.\n\n{MANAGED_BEGIN}\n# old pixel rules\n{MANAGED_END}\n"
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
        ..Default::default()
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

/// After uninstall, Claude settings.json should have no pixel run-hook entries,
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

    // Manually wire pixel run-hook entries into settings.json (install() no
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
        ..Default::default()
    };
    uninstall(&uninstall_opts).expect("uninstall");

    // Settings should have no pixel run-hook references.
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
        ..Default::default()
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
        ..Default::default()
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
        format!("# Project\n\n{MANAGED_BEGIN}\n# old pixel rules\n{MANAGED_END}\n"),
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
        ..Default::default()
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

/// Uninstall removes pixel run-hook entries from Codex hooks.json while
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
        ..Default::default()
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
        ..Default::default()
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
        ..Default::default()
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
        "$argv\n",
    ] {
        assert!(
            block.contains(required),
            "fish block must define functions in fish syntax, missing {required:?}:\n{block}"
        );
    }
    for forbidden in ["claude()", "codex()", "function codex", "\"$@\""] {
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

/// The residue of an install that ran under an agent's `$SHELL`: a valid
/// fish install plus a pixel block in `~/.zshrc`, which fish never loads.
/// Doctor names the stray file, its shell and the two ways out, as yellow:
/// a machine that launches `claude` from a second shell on purpose keeps a
/// working install and must not be sent to `pixel install`.
#[test]
fn doctor_flags_a_pixel_block_in_another_shells_profile_as_yellow() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    install_for_shell(home, FISH_SHELL);
    let check = || {
        doctor(&DoctorOptions {
            home: Some(home.to_path_buf()),
            executable_path: None,
            shell: Some(FISH_SHELL.into()),
            claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
            ..Default::default()
        })
        .expect("doctor runs")
        .checks
        .into_iter()
        .find(|c| c.id == "install.shell-wrappers")
        .expect("shell-wrappers check")
    };

    let clean = check();
    assert_eq!(clean.status, pixel_install::doctor::CheckStatus::Green);
    assert_eq!(
        clean.detail.as_ref().unwrap()["stray_profiles"],
        serde_json::json!([])
    );

    // A `.zshrc` without a block is not a stray: every macOS account has one.
    fs::write(home.join(".zshrc"), "export EDITOR=vim\n").unwrap();
    assert_eq!(check().status, pixel_install::doctor::CheckStatus::Green);

    install_for_shell(home, "/bin/zsh");
    let stray = check();
    assert_eq!(stray.status, pixel_install::doctor::CheckStatus::Yellow);
    let zshrc = home.join(".zshrc").display().to_string();
    assert!(
        stray
            .summary
            .starts_with("fish shell wrappers installed in "),
        "{}",
        stray.summary
    );
    assert!(stray.summary.contains(&zshrc), "{}", stray.summary);
    assert!(
        stray.summary.contains("not loaded by fish"),
        "{}",
        stray.summary
    );
    assert!(
        stray
            .summary
            .contains("`pixel uninstall --wrappers-only --shell zsh`")
            && stray.summary.contains("`--shell zsh`"),
        "{}",
        stray.summary
    );
    assert_eq!(
        stray.detail.as_ref().unwrap()["stray_profiles"],
        serde_json::json!([{"shell": "zsh", "profile": zshrc}])
    );

    // The same install seen from zsh: green for zsh, with fish as the stray.
    let from_zsh = doctor(&DoctorOptions {
        home: Some(home.to_path_buf()),
        executable_path: None,
        shell: Some("/bin/zsh".into()),
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        ..Default::default()
    })
    .expect("doctor runs")
    .checks
    .into_iter()
    .find(|c| c.id == "install.shell-wrappers")
    .unwrap();
    assert_eq!(from_zsh.status, pixel_install::doctor::CheckStatus::Yellow);
    assert_eq!(
        from_zsh.detail.as_ref().unwrap()["stray_profiles"][0]["shell"],
        "fish"
    );
}

/// The way out doctor names: only the named shell's block goes, the
/// working install (the other shell's block, the prompt files) stays.
#[test]
fn uninstall_wrappers_only_removes_one_shells_block_and_nothing_else() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    install_for_shell(home, FISH_SHELL);
    install_for_shell(home, "/bin/zsh");
    let prompt = home.join(".local/share/pixel/agent-prompt.md");
    assert!(prompt.is_file(), "fixture: the prompt is installed");
    assert!(
        fs::read_to_string(home.join(".zshrc"))
            .unwrap()
            .contains("pixel-managed")
    );

    let report = uninstall(&UninstallOptions {
        home: Some(home.to_path_buf()),
        shell: Some("/bin/zsh".into()),
        wrappers_only: true,
        ..Default::default()
    })
    .expect("uninstall runs");
    assert!(report.ok, "{report:?}");
    assert_eq!(
        report.steps.len(),
        1,
        "only the wrapper step ran: {report:?}"
    );
    assert_eq!(report.steps[0].id, "shell-wrappers");
    assert_eq!(
        (
            report.summary.green,
            report.summary.yellow,
            report.summary.red
        ),
        (1, 0, 0),
        "{report:?}"
    );

    assert!(
        !fs::read_to_string(home.join(".zshrc"))
            .unwrap()
            .contains("pixel-managed"),
        "the zsh block is gone"
    );
    assert!(
        fs::read_to_string(fish_dropin(home))
            .unwrap()
            .contains("pixel-managed"),
        "the fish block stays"
    );
    assert!(prompt.is_file(), "the prompt files stay");

    // Doctor now reads the fish install back green with no stray.
    let check = doctor(&DoctorOptions {
        home: Some(home.to_path_buf()),
        executable_path: None,
        shell: Some(FISH_SHELL.into()),
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        ..Default::default()
    })
    .expect("doctor runs")
    .checks
    .into_iter()
    .find(|c| c.id == "install.shell-wrappers")
    .unwrap();
    assert_eq!(check.status, pixel_install::doctor::CheckStatus::Green);
}

/// A begin marker with no end marker used to mean "everything to EOF is
/// ours": the rest of the user's profile was deleted and the truncated file
/// rewritten. Both install and uninstall now refuse it and write nothing.
#[test]
fn an_unterminated_managed_block_refuses_the_rewrite_and_keeps_the_profile() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    let profile = shell_profile_path(home);
    let original = "# user aliases\n\
                    alias gs='git status'\n\
                    # >>> pixel-managed >>>\n\
                    # a block nobody closed\n\
                    alias last='kept'\n";
    fs::write(&profile, original).unwrap();

    let report = install(&InstallOptions {
        home: Some(home.to_path_buf()),
        executable_path: Some(fake_pixel_exe(home)),
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
    })
    .expect("install reports the refusal, it does not fail the run");
    let step = wrappers_step(&report);
    assert_eq!(
        step.status,
        pixel_install::install::CheckStatus::Red,
        "{step:?}"
    );
    assert!(
        step.summary.contains("pixel-managed"),
        "the step must name what is wrong: {step:?}"
    );
    assert_eq!(
        fs::read_to_string(&profile).unwrap(),
        original,
        "the lines after the unterminated marker are the user's"
    );

    let report = uninstall(&UninstallOptions {
        home: Some(home.to_path_buf()),
        binary_path: Some(home.join("pixel")),
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
        ..Default::default()
    })
    .expect("uninstall reports the refusal");
    let step = report
        .steps
        .iter()
        .find(|s| s.id == "shell-wrappers")
        .expect("shell-wrappers step");
    assert_eq!(
        step.status,
        pixel_install::install::CheckStatus::Red,
        "{step:?}"
    );
    assert_eq!(
        fs::read_to_string(&profile).unwrap(),
        original,
        "uninstall leaves the broken profile alone too"
    );
}

/// The write is destructive (it replaces the user's profile), so the bytes it
/// replaces are backed up first, and a re-install that changes nothing adds no
/// second backup.
#[test]
fn rewriting_the_shell_profile_backs_it_up_first() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    let profile = shell_profile_path(home);
    let original = "# user aliases\nexport EDITOR=vim\n";
    fs::write(&profile, original).unwrap();

    install_for_shell(home, TEST_SHELL);

    let profile_backups = |home: &std::path::Path| -> Vec<std::path::PathBuf> {
        let mut paths: Vec<std::path::PathBuf> = fs::read_dir(home)
            .unwrap()
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with(".zshrc.pixel-bak."))
            })
            .collect();
        paths.sort();
        paths
    };
    let backups = profile_backups(home);
    assert_eq!(backups.len(), 1, "one backup of the profile: {backups:?}");
    assert_eq!(
        fs::read_to_string(&backups[0]).unwrap(),
        original,
        "the backup holds the bytes install replaced"
    );
    let installed = fs::read_to_string(&profile).unwrap();
    assert!(installed.contains("export EDITOR=vim"), "{installed}");
    assert!(installed.contains(PIXEL_MANAGED_BEGIN), "{installed}");

    install_for_shell(home, TEST_SHELL);
    assert_eq!(
        profile_backups(home).len(),
        1,
        "an idempotent re-install must not back up an unchanged profile"
    );
    assert_eq!(
        fs::read_to_string(&profile).unwrap(),
        installed,
        "an idempotent re-install must leave the profile byte-identical"
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
        ..Default::default()
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

fn codex_config_path(home: &std::path::Path) -> std::path::PathBuf {
    home.join(".codex/config.toml")
}

/// The `developer_instructions` string of `~/.codex/config.toml`, read the way
/// Codex reads it (a TOML parse, not a substring search), or `None` when the
/// file or the key is absent.
fn codex_developer_instructions(home: &std::path::Path) -> Option<String> {
    let text = fs::read_to_string(codex_config_path(home)).ok()?;
    let doc: toml_edit::DocumentMut = text.parse().expect("config.toml must stay valid TOML");
    doc.get("developer_instructions")
        .and_then(|item| item.as_str())
        .map(str::to_string)
}

const PIXEL_BLOCK_BEGIN: &str = "<!-- pixel:managed:begin -->";
const PIXEL_BLOCK_END: &str = "<!-- pixel:managed:end -->";

/// A config.toml the way the Codex desktop app leaves it: comments, root
/// keys, sub-tables with dotted keys. Every line of it must survive an
/// install byte for byte — the app rewrites this file too, and a
/// regenerated layout would fight it.
const USER_CODEX_CONFIG: &str = r#"# my codex settings
personality = "pragmatic"
model = "gpt-5.6-sol"  # trailing comment

[mcp_servers.node_repl]
args = []
command = "/opt/node_repl"

[features]
js_repl = false
token_budget.enabled = true
"#;

/// Codex has no file-backed `developer_instructions`, so the prompt is
/// embedded in the file. Two things must hold: Codex reads back exactly the
/// bundled prompt (a TOML round trip, no escaping accident), and nothing else
/// in a file the desktop app also owns moves.
#[test]
fn install_writes_the_agent_prompt_into_codex_config_and_leaves_the_rest_of_the_file_alone() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    fs::create_dir_all(home.join(".codex")).unwrap();
    fs::write(codex_config_path(home), USER_CODEX_CONFIG).unwrap();
    install_for_shell(home, TEST_SHELL);

    let asset = fs::read_to_string(home.join(".local/share/pixel/agent-prompt.md")).unwrap();
    let value = codex_developer_instructions(home).expect("developer_instructions written");
    assert_eq!(
        value,
        format!("{PIXEL_BLOCK_BEGIN}\n{asset}{PIXEL_BLOCK_END}\n"),
        "codex must read back the bundled prompt between the pixel markers"
    );
    let written = fs::read_to_string(codex_config_path(home)).unwrap();
    for line in USER_CODEX_CONFIG.lines() {
        assert!(
            written.contains(line),
            "user line {line:?} must survive the install verbatim:\n{written}"
        );
    }
    assert!(
        written.contains("developer_instructions = '''\n"),
        "the prompt must be a literal multi-line string, so the file shows it unescaped:\n{written}"
    );
    let doc: toml_edit::DocumentMut = written.parse().unwrap();
    assert!(
        doc.get("developer_instructions")
            .is_some_and(toml_edit::Item::is_value),
        "the key must sit in the root table, not inside [features] at the end of the file"
    );
    assert_eq!(
        doc["features"]["token_budget"]["enabled"].as_bool(),
        Some(true),
        "sub-tables must be untouched"
    );

    install_for_shell(home, TEST_SHELL);
    assert_eq!(
        fs::read_to_string(codex_config_path(home)).unwrap(),
        written,
        "a re-install must be byte-for-byte idempotent"
    );
    let block = fs::read_to_string(shell_profile_path(home)).unwrap();
    assert!(
        !block.contains("codex"),
        "no codex shell function next to the config key — the CLI override would shadow it:\n{block}"
    );
}

#[test]
fn install_keeps_a_users_own_developer_instructions_and_refreshes_a_stale_pixel_block() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    fs::create_dir_all(home.join(".codex")).unwrap();
    fs::write(
        codex_config_path(home),
        "developer_instructions = \"Always answer in French.\"\n",
    )
    .unwrap();
    install_for_shell(home, TEST_SHELL);
    let asset = fs::read_to_string(home.join(".local/share/pixel/agent-prompt.md")).unwrap();
    let expected =
        format!("Always answer in French.\n\n{PIXEL_BLOCK_BEGIN}\n{asset}{PIXEL_BLOCK_END}\n");
    assert_eq!(
        codex_developer_instructions(home).as_deref(),
        Some(expected.as_str()),
        "the user's own instructions come first, the pixel block is appended"
    );

    // A block left by an older pixel (different prompt) plus text the user
    // added after it: only the block changes.
    fs::write(
        codex_config_path(home),
        format!(
            "developer_instructions = '''\nMine first.\n\n{PIXEL_BLOCK_BEGIN}\nold prompt\n{PIXEL_BLOCK_END}\nMine last.\n'''\n"
        ),
    )
    .unwrap();
    install_for_shell(home, TEST_SHELL);
    assert_eq!(
        codex_developer_instructions(home).as_deref(),
        Some(
            format!("Mine first.\n\n{PIXEL_BLOCK_BEGIN}\n{asset}{PIXEL_BLOCK_END}\nMine last.\n")
                .as_str()
        ),
        "a stale block is replaced in place, text on both sides survives"
    );
}

#[test]
fn install_refuses_to_rewrite_a_codex_config_it_cannot_parse() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    fs::create_dir_all(home.join(".codex")).unwrap();
    let broken = "model = \"gpt\"\n[features\njs_repl = false\n";
    fs::write(codex_config_path(home), broken).unwrap();
    let report = install(&InstallOptions {
        home: Some(home.to_path_buf()),
        executable_path: Some(fake_pixel_exe(home)),
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
    })
    .unwrap();
    let step = report
        .steps
        .iter()
        .find(|s| s.id == "codex-config")
        .expect("codex-config step");
    assert_eq!(
        step.status,
        pixel_install::install::CheckStatus::Red,
        "a file codex itself cannot load is reported, not repaired: {step:?}"
    );
    assert_eq!(
        fs::read_to_string(codex_config_path(home)).unwrap(),
        broken,
        "an unparseable config.toml must not be rewritten — that would drop what it holds"
    );
    assert!(
        !report.ok,
        "the report must not read ok with the codex step red"
    );
}

#[test]
fn dry_run_leaves_codex_config_absent_and_untouched() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    let options = InstallOptions {
        home: Some(home.to_path_buf()),
        executable_path: Some(fake_pixel_exe(home)),
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        dry_run: true,
        shell: Some(TEST_SHELL.into()),
    };
    let report = install(&options).unwrap();
    assert!(report.ok);
    assert!(
        !home.join(".codex").exists(),
        "dry-run must not create ~/.codex"
    );
    fs::create_dir_all(home.join(".codex")).unwrap();
    fs::write(codex_config_path(home), USER_CODEX_CONFIG).unwrap();
    let report = install(&options).unwrap();
    let step = report
        .steps
        .iter()
        .find(|s| s.id == "codex-config")
        .unwrap();
    assert!(
        step.summary.starts_with("[dry-run]"),
        "dry-run must say what it would do: {step:?}"
    );
    assert_eq!(
        fs::read_to_string(codex_config_path(home)).unwrap(),
        USER_CODEX_CONFIG
    );
}

#[test]
fn doctor_codex_config_check_is_red_until_the_current_block_is_in_place() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    let status = || {
        doctor(&DoctorOptions {
            home: Some(home.to_path_buf()),
            executable_path: None,
            shell: Some(TEST_SHELL.into()),
            claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
            ..Default::default()
        })
        .unwrap()
        .checks
        .into_iter()
        .find(|c| c.id == "install.codex-config")
        .expect("codex-config check")
    };
    assert_eq!(status().status, CheckStatus::Red, "nothing installed");

    fs::create_dir_all(home.join(".codex")).unwrap();
    fs::write(
        codex_config_path(home),
        "developer_instructions = \"Always answer in French.\"\n",
    )
    .unwrap();
    assert_eq!(
        status().status,
        CheckStatus::Red,
        "a value without the pixel block does not carry the prompt"
    );

    install_for_shell(home, TEST_SHELL);
    let check = status();
    assert_eq!(check.status, CheckStatus::Green, "{check:?}");

    let written = fs::read_to_string(codex_config_path(home)).unwrap();
    fs::write(
        codex_config_path(home),
        written.replace("## MANDATORY WORKFLOW", "## OPTIONAL WORKFLOW"),
    )
    .unwrap();
    let check = status();
    assert_eq!(
        check.status,
        CheckStatus::Red,
        "a block that differs from the bundled prompt is stale: {check:?}"
    );
    assert!(
        check.reason.as_deref().is_some_and(|r| r.contains("stale")),
        "{check:?}"
    );
}

#[test]
fn uninstall_takes_only_the_pixel_block_out_of_codex_config() {
    use pixel_install::uninstall::{UninstallOptions, uninstall};

    // Only pixel in the key: the key goes, the rest of the file stays.
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    fs::create_dir_all(home.join(".codex")).unwrap();
    fs::write(codex_config_path(home), USER_CODEX_CONFIG).unwrap();
    install_for_shell(home, TEST_SHELL);
    assert!(codex_developer_instructions(home).is_some());
    uninstall(&UninstallOptions {
        home: Some(home.to_path_buf()),
        shell: Some(TEST_SHELL.into()),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(
        codex_developer_instructions(home),
        None,
        "the key must be removed"
    );
    let after = fs::read_to_string(codex_config_path(home)).unwrap();
    for line in USER_CODEX_CONFIG.lines() {
        assert!(
            after.contains(line),
            "user line {line:?} lost by uninstall:\n{after}"
        );
    }

    // The user's own text around the block: the block goes, the text stays.
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    fs::create_dir_all(home.join(".codex")).unwrap();
    fs::write(
        codex_config_path(home),
        "developer_instructions = \"Always answer in French.\"\n",
    )
    .unwrap();
    install_for_shell(home, TEST_SHELL);
    uninstall(&UninstallOptions {
        home: Some(home.to_path_buf()),
        shell: Some(TEST_SHELL.into()),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(
        codex_developer_instructions(home).as_deref(),
        Some("Always answer in French.\n"),
        "uninstall must hand the key back to the user"
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
    fs::write(subagent_prompt_path(home), "pixel who-calls X --callers\n").unwrap();
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
        ..Default::default()
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
// Pi's APPEND_SYSTEM.md is a shared file: pixel owns a managed block inside
// it, never the file. `pixel install` used to replace the whole file and
// `pixel uninstall` deleted it, so the user's own pi instructions were lost.
// ---------------------------------------------------------------------------

fn pi_prompt_path(home: &std::path::Path) -> std::path::PathBuf {
    home.join(".pi/agent/APPEND_SYSTEM.md")
}

/// Backup files `pixel install` wrote for the pi prompt, newest last.
fn pi_backups(home: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut paths: Vec<std::path::PathBuf> = fs::read_dir(home.join(".pi/agent"))
        .map(|entries| {
            entries
                .filter_map(std::result::Result::ok)
                .map(|entry| entry.path())
                .filter(|path| {
                    path.file_name()
                        .is_some_and(|name| name.to_string_lossy().contains("pixel-bak"))
                })
                .collect()
        })
        .unwrap_or_default();
    paths.sort();
    paths
}

fn uninstall_home(home: &std::path::Path) {
    uninstall(&UninstallOptions {
        home: Some(home.to_path_buf()),
        binary_path: Some(home.join("pixel")),
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
        ..Default::default()
    })
    .expect("uninstall");
}

#[test]
fn install_and_uninstall_keep_the_users_own_text_in_pis_append_system_file() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    let pi_path = pi_prompt_path(home);
    fs::create_dir_all(pi_path.parent().unwrap()).unwrap();
    let original = "# my own pi instructions\nAlways answer in French.\n";
    fs::write(&pi_path, original).unwrap();

    install_for_shell(home, TEST_SHELL);

    let deployed = fs::read_to_string(&pi_path).expect("pi prompt deployed");
    assert!(
        deployed.contains(original.trim_end()),
        "the user's own instructions must survive install:\n{deployed}"
    );
    assert!(
        deployed.contains(MANAGED_BEGIN) && deployed.contains(MANAGED_END),
        "the bundled prompt must sit inside the managed markers:\n{deployed}"
    );
    let backups = pi_backups(home);
    assert_eq!(backups.len(), 1, "one backup of the file install changed");
    assert_eq!(
        fs::read_to_string(&backups[0]).unwrap(),
        original,
        "the backup holds the bytes install replaced"
    );

    // A second install that finds a current block changes nothing.
    let once = fs::read(&pi_path).unwrap();
    install_for_shell(home, TEST_SHELL);
    assert_eq!(
        fs::read(&pi_path).unwrap(),
        once,
        "a current managed block is rewritten with the same bytes"
    );

    uninstall_home(home);
    let after = fs::read_to_string(&pi_path).expect("the user's file survives uninstall");
    assert_eq!(after, original, "uninstall removes the block, not the file");
    assert!(!after.contains(MANAGED_BEGIN), "{after}");
}

#[test]
fn a_pi_prompt_written_by_an_earlier_install_is_wrapped_not_duplicated() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    install_for_shell(home, TEST_SHELL);
    // What `pixel install` wrote before it treated the file as shared: the
    // prompt verbatim, no markers around it.
    let asset = fs::read_to_string(home.join(".local/share/pixel/agent-prompt.md"))
        .expect("deployed prompt");
    let pi_path = pi_prompt_path(home);
    fs::write(&pi_path, &asset).unwrap();

    install_for_shell(home, TEST_SHELL);

    let deployed = fs::read_to_string(&pi_path).expect("pi prompt deployed");
    assert!(
        deployed.starts_with(MANAGED_BEGIN),
        "the upgrade must put the markers around the prompt, not above it:\n{deployed}"
    );
    assert_eq!(
        deployed.matches(asset.as_str()).count(),
        1,
        "the prompt must not appear twice after the upgrade"
    );
}

#[test]
fn uninstall_removes_the_pi_prompt_file_when_it_held_nothing_else() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    let pi_path = pi_prompt_path(home);
    fs::create_dir_all(pi_path.parent().unwrap()).unwrap();
    fs::write(
        &pi_path,
        format!("{MANAGED_BEGIN}\n# pixel's own prompt\n{MANAGED_END}\n"),
    )
    .unwrap();

    install_for_shell(home, TEST_SHELL);
    uninstall_home(home);

    assert!(
        !pi_path.exists(),
        "a file that held nothing but the pixel block is pixel's to delete"
    );
}

#[test]
fn uninstall_survives_a_missing_pi_prompt_file() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    let prompts = home.join(".local/share/pixel");
    fs::create_dir_all(&prompts).unwrap();
    fs::write(prompts.join("agent-prompt.md"), "deployed prompt\n").unwrap();

    uninstall_home(home);

    assert!(
        !prompts.join("agent-prompt.md").exists(),
        "the prompt is removed even when the pi file was never deployed"
    );
    assert!(!pi_prompt_path(home).exists());
}

#[test]
fn install_reports_a_pi_prompt_it_cannot_write_instead_of_greening_it() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    // A file where the directory should be: creating ~/.pi/agent fails
    // whatever the user's permissions are.
    fs::write(home.join(".pi"), "not a directory\n").unwrap();

    let result = install(&InstallOptions {
        home: Some(home.to_path_buf()),
        executable_path: Some(fake_pixel_exe(home)),
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        dry_run: false,
        shell: Some(TEST_SHELL.into()),
    });

    assert!(
        result.is_err(),
        "a pi prompt that cannot be written must fail the install, not report a green step: {result:?}"
    );
}

#[test]
fn doctor_pi_prompt_check_is_red_until_the_managed_block_is_current() {
    let dir = TempDir::new().expect("tempdir");
    let home = dir.path();
    install_for_shell(home, TEST_SHELL);
    let pi_path = pi_prompt_path(home);
    let doc_opts = DoctorOptions {
        home: Some(home.to_path_buf()),
        executable_path: None,
        shell: Some(TEST_SHELL.into()),
        claude_executable: Some(fake_claude_exe(home, CLAUDE_WITH_SUBAGENT_FLAG)),
        ..Default::default()
    };
    let status = |home: &std::path::Path| {
        check(
            &doctor(&DoctorOptions {
                home: Some(home.to_path_buf()),
                ..doc_opts.clone()
            })
            .expect("doctor"),
            "install.pi-prompt",
        )
        .status
    };
    assert_eq!(
        status(home),
        CheckStatus::Green,
        "a freshly installed pi prompt is green"
    );

    // The user's own text outside the markers is theirs: it does not make the
    // check stale.
    let with_user_text = format!("{}Be concise.\n", fs::read_to_string(&pi_path).unwrap());
    fs::write(&pi_path, &with_user_text).unwrap();
    assert_eq!(
        status(home),
        CheckStatus::Green,
        "text outside the pixel markers belongs to the user"
    );

    // A block that no longer matches the bundled prompt is stale.
    fs::write(
        &pi_path,
        format!("{MANAGED_BEGIN}\n# stale prompt\n{MANAGED_END}\n"),
    )
    .unwrap();
    assert_eq!(
        status(home),
        CheckStatus::Red,
        "a stale block must send the user back to pixel install"
    );

    // And the file must carry the block at all.
    fs::write(&pi_path, "my own instructions only\n").unwrap();
    assert_eq!(status(home), CheckStatus::Red, "no block, no green");
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

/// Plugin-manifest surfaces (skills/, .cursor/rules/, …) are generated from
/// `assets/pixel-agent-prompt.md` by `scripts/gen-plugin-assets.sh`. They must
/// never drift: an edited prompt with stale plugin files silently ships an old
/// protocol to every CLI that installs via plugin manifests.
#[test]
fn plugin_assets_are_in_sync() {
    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..");
    let script = repo.join("scripts").join("gen-plugin-assets.sh");
    assert!(script.is_file(), "missing {}", script.display());
    let out = std::process::Command::new("/bin/sh")
        .arg(&script)
        .arg("--check")
        .output()
        .expect("run gen-plugin-assets.sh --check");
    assert!(
        out.status.success(),
        "plugin assets stale — run scripts/gen-plugin-assets.sh\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

fn repo_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

/// A plugin root in a temp dir: the real hook script, the given context
/// files, and a `bin/` holding a fake `pixel` whose `repo-state --help`
/// exits with `repo_state_exit` (no `pixel` at all when `None`).
#[cfg(unix)]
fn plugin_root(context: &str, subagent: &str, repo_state_exit: Option<i32>) -> TempDir {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new().unwrap();
    fs::create_dir_all(dir.path().join("hooks")).unwrap();
    fs::create_dir_all(dir.path().join("bin")).unwrap();
    fs::copy(
        repo_root().join("hooks/pixel-context.sh"),
        dir.path().join("hooks/pixel-context.sh"),
    )
    .unwrap();
    fs::write(dir.path().join("PIXEL.md"), context).unwrap();
    fs::write(dir.path().join("PIXEL-SUBAGENT.md"), subagent).unwrap();
    if let Some(code) = repo_state_exit {
        let exe = dir.path().join("bin/pixel");
        fs::write(
            &exe,
            format!(
                "#!/bin/sh\n[ \"$1\" = --version ] && echo 'pixel 0.1.0' && exit 0\n[ \"$1\" = repo-state ] && exit {code}\nexit 0\n"
            ),
        )
        .unwrap();
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
    }
    dir
}

/// Run the hook as a harness does (stdin JSON, one event argument) with a
/// PATH of the fake `bin/` plus the system tools, and parse its one line.
#[cfg(unix)]
fn run_context_hook(root: &std::path::Path, event: &str) -> serde_json::Value {
    use std::io::Write;
    let mut child = std::process::Command::new("/bin/sh")
        .arg(root.join("hooks/pixel-context.sh"))
        .arg(event)
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", root.join("bin").display()),
        )
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"{\"hook_event_name\":\"SessionStart\"}")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(text.lines().count(), 1, "one JSON line: {text}");
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{e}: {text}"))
}

/// The protocol reaches the model byte for byte, whatever it contains:
/// quotes, backslashes, tabs and non-ASCII survive the JSON encoding, and a
/// sub-agent gets the short sub-agent prompt, not the 18 KB one.
#[cfg(unix)]
#[test]
fn context_hook_injects_the_prompt_for_its_event_verbatim() {
    let context = "# Pixel \u{1F7E9}\n\n| `grep \"x\"` | `pixel search-content \"x\"` |\n\tpath\\to\\file\r\nend";
    let root = plugin_root(context, "sub-agent prompt\n", Some(0));

    let session = run_context_hook(root.path(), "SessionStart");
    assert_eq!(
        session["hookSpecificOutput"]["hookEventName"],
        "SessionStart"
    );
    assert_eq!(session["hookSpecificOutput"]["additionalContext"], context);

    let subagent = run_context_hook(root.path(), "SubagentStart");
    assert_eq!(
        subagent["hookSpecificOutput"]["hookEventName"],
        "SubagentStart"
    );
    assert_eq!(
        subagent["hookSpecificOutput"]["additionalContext"],
        "sub-agent prompt"
    );

    let unknown = run_context_hook(root.path(), "Weird\"Event");
    assert_eq!(
        unknown["hookSpecificOutput"]["hookEventName"], "SessionStart",
        "the event name in the JSON is never taken from the argument verbatim"
    );
}

/// Without a usable binary the protocol is a list of failing commands: the
/// hook says why it was not loaded instead, and never tells the agent to
/// fetch an installer.
#[cfg(unix)]
#[test]
fn context_hook_replaces_the_protocol_with_a_notice_when_pixel_cannot_run_it() {
    let missing = plugin_root("PROTOCOL", "SUB", None);
    let notice = run_context_hook(missing.path(), "SessionStart");
    let text = notice["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(text.contains("`pixel` binary is not on PATH"), "{text}");
    assert!(!text.contains("PROTOCOL"), "{text}");
    assert!(!text.contains("curl"), "{text}");

    let outdated = plugin_root("PROTOCOL", "SUB", Some(2));
    let notice = run_context_hook(outdated.path(), "SubagentStart");
    let text = notice["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(
        text.contains("installed pixel 0.1.0 does not accept"),
        "{text}"
    );
    assert!(!text.contains("SUB"), "{text}");
    assert_eq!(
        notice["hookSpecificOutput"]["hookEventName"],
        "SubagentStart"
    );
}

/// Every manifest parses and every path it hands a harness exists in the
/// repository: a renamed hook script or a moved skills directory breaks the
/// plugin silently at install time, never in a build.
#[test]
fn plugin_manifests_parse_and_point_at_files_that_exist() {
    let repo = repo_root();
    let json = |rel: &str| -> serde_json::Value {
        let text = fs::read_to_string(repo.join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"));
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("{rel}: {e}"))
    };
    let exists = |owner: &str, rel: &str| {
        let rel = rel.trim_start_matches("./");
        assert!(
            repo.join(rel).exists(),
            "{owner} names {rel}, which is not in the repository"
        );
    };
    for rel in [
        ".claude-plugin/plugin.json",
        ".codex-plugin/plugin.json",
        ".qoder-plugin/plugin.json",
    ] {
        let manifest = json(rel);
        for field in ["skills", "hooks", "rules"] {
            if let Some(path) = manifest[field].as_str() {
                exists(rel, path);
            }
        }
    }
    for rel in [
        ".claude-plugin/marketplace.json",
        ".grok-plugin/marketplace.json",
        ".agents/plugins/marketplace.json",
    ] {
        let plugins = json(rel)["plugins"].as_array().cloned().unwrap_or_default();
        assert!(!plugins.is_empty(), "{rel} lists no plugin");
        for plugin in plugins {
            exists(rel, plugin["source"].as_str().unwrap());
        }
    }
    exists(
        "gemini-extension.json",
        json("gemini-extension.json")["contextFileName"]
            .as_str()
            .unwrap(),
    );
    let package = json("package.json");
    exists("package.json", package["main"].as_str().unwrap());
    for entry in package["files"].as_array().unwrap() {
        exists("package.json", entry.as_str().unwrap());
    }
    for entry in json("opencode.json")["plugin"].as_array().unwrap() {
        exists("opencode.json", entry.as_str().unwrap());
    }

    let hooks = json("hooks/plugin-hooks.json");
    let mut commands = 0;
    for (event, matchers) in hooks["hooks"].as_object().unwrap() {
        for matcher in matchers.as_array().unwrap() {
            for hook in matcher["hooks"].as_array().unwrap() {
                let command = hook["command"].as_str().unwrap();
                commands += 1;
                assert!(
                    command.starts_with(
                        "\"${CLAUDE_PLUGIN_ROOT:-$PLUGIN_ROOT}/hooks/pixel-context.sh\""
                    ),
                    "{event}: the script must resolve from the plugin root Claude Code and Codex set: {command}"
                );
                assert!(
                    command.ends_with(&format!(" {event}")),
                    "{event}: {command}"
                );
            }
        }
    }
    assert_eq!(commands, 2, "SessionStart and SubagentStart");
    exists("hooks/plugin-hooks.json", "hooks/pixel-context.sh");

    // A root `plugin.json` wins over the tool directories: Copilot CLI reads
    // it before `.claude-plugin/plugin.json`, and Codex's Agent Plugins loader
    // then ignores the hooks of `.codex-plugin/plugin.json`
    // (openai/codex#39895). A bare one shipped neither skills nor hooks.
    assert!(
        !repo.join("plugin.json").exists(),
        "a root plugin.json shadows .claude-plugin/ and .codex-plugin/"
    );
}

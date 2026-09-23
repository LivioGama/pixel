//! The pi guard of `pixel install --repo`: one extension file under the
//! repository's `.pi/extensions/`, the directory pi auto-discovers project
//! extensions from once the project is trusted (pi 0.87,
//! `docs/extensions.md`). pi resolves it against its working directory, so
//! the guard runs when pi is started from the repository root.
//!
//! No rules file is written next to it: pi's project prompt file,
//! `.pi/APPEND_SYSTEM.md`, replaces the global `~/.pi/agent/APPEND_SYSTEM.md`
//! that `pixel install` fills, and its only project context file is the
//! repository's own, usually shared, `AGENTS.md`.
//!
//! Releases up to 0.4.0 wrote the guard and a rules block under
//! `<repo>/.pi/agent/`, a directory pi reads only under `~`. Install and
//! uninstall take pixel's files out of it, and doctor reports a guard left
//! there, since it never ran.

use std::fs;
use std::path::{Path, PathBuf};

use crate::config;
use crate::install::{self, CheckStatus, InstallStep, Result};

/// The guard extension, relative to the repository.
pub(crate) const EXTENSION: &str = ".pi/extensions/pixel-guard.ts";

/// The directory, relative to the repository, where releases up to 0.4.0
/// wrote the guard (`extensions/pixel-guard.ts`) and a rules block
/// (`AGENTS.md`).
pub(crate) const LEGACY_DIR: &str = ".pi/agent";

/// The guard extension's source for the pixel binary at `exe`. pi's
/// `tool_call` event can rewrite a tool call by mutating `event.input`, so
/// the extension shells out to `pixel run-hook guard` and applies any
/// `updatedInput` the guard emits. A non-zero guard exit is advisory only —
/// the tool call stays allowed.
pub(crate) fn extension_source(exe: &Path) -> String {
    format!(
        r#"// pixel-guard extension — managed by `pixel install`
// {begin}
// {end}
import {{ spawnSync }} from "child_process";

const PIXEL_BIN = {exe_path:?};
const GUARD_TOOLS = new Set(["bash", "edit", "write", "read", "grep", "find", "ls", "sed", "awk", "perl", "ag", "ack", "egrep", "fgrep", "head", "tail", "cat", "xargs",
  // Antigravity/Gemini tool names
  "run_command", "view_file", "replace_file_content", "write_to_file", "grep_search", "find_by_name", "list_dir", "file_search", "edit_file"]);

export default function activate(pi) {{
  pi.on("tool_call", async (event, ctx) => {{
    const toolName = event.toolName;
    if (!GUARD_TOOLS.has(toolName)) return;

    // Build the PreToolUse-compatible payload that `pixel run-hook guard`
    // expects on stdin.
    const cwd = ctx?.cwd ?? process.cwd();
    const payload = {{
      hook_event_name: "PreToolUse",
      tool_name: toolName,
      tool_input: event.input ?? {{}},
      cwd,
    }};

    try {{
      const result = spawnSync(PIXEL_BIN, ["run-hook", "guard"], {{
        input: JSON.stringify(payload),
        timeout: 5000,
        encoding: "utf-8",
      }});

      // Keep the tool available even if a legacy guard path returns exit 2.
      if (result.status === 2) {{
        const reason = (result.stderr || "").trim() || "blocked by pixel guard";
        console.warn(`[pixel] advisory: ${{reason}}`);
        return;
      }}

      // exit 0 with stdout = possibly a rewrite (hookSpecificOutput.updatedInput).
      // pi docs: "Mutations to event.input affect the actual tool execution"
      // — mutate in place rather than returning a separate object.
      if (result.status === 0 && result.stdout) {{
        try {{
          const parsed = JSON.parse(result.stdout);
          const updated = parsed?.hookSpecificOutput?.updatedInput;
          if (updated && typeof updated === "object") {{
            Object.assign(event.input, updated);
            return;
          }}
        }} catch {{
          // stdout wasn't JSON — that's fine, the guard just allowed the call
        }}
      }}

      // Any other exit (including crash/timeout) = allow, don't block the
      // agent on a guard failure.
      return;
    }} catch {{
      // spawn failure — allow, don't block the agent.
      return;
    }}
  }});
}}
"#,
        begin = config::MANAGED_BEGIN,
        end = config::MANAGED_END,
        exe_path = exe.display().to_string(),
    )
}

/// Whether `path` is a guard extension pixel wrote: a file carrying the
/// managed marker. Anything else under that name belongs to the user.
fn is_managed_extension(path: &Path) -> bool {
    fs::read_to_string(path).is_ok_and(|text| text.contains(config::MANAGED_BEGIN))
}

/// `pixel install --repo` step: write the guard to [`EXTENSION`] and take
/// pixel's files out of [`LEGACY_DIR`].
pub(crate) fn install(repo: &Path, exe: &Path, dry_run: bool) -> Result<InstallStep> {
    let ext_file = repo.join(EXTENSION);
    let source = extension_source(exe);
    let migrated = remove_legacy(repo, dry_run)?;
    let backup = if dry_run {
        None
    } else {
        if let Some(parent) = ext_file.parent() {
            fs::create_dir_all(parent)?;
        }
        let backup = config::backup_if_changing(&ext_file, source.as_bytes())?;
        fs::write(&ext_file, &source)?;
        backup
    };
    let mut detail = format!("wrote {}", ext_file.display());
    if !migrated.is_empty() {
        detail.push_str(&format!("; removed {}", migrated.join(" ")));
    }
    Ok(InstallStep {
        id: "hooks.pi".into(),
        status: CheckStatus::Green,
        summary: install::dry_run_summary(
            dry_run,
            "pi guard extension installed (loads once pi trusts the project)",
        ),
        detail: Some(install::with_backup_note(detail, backup)),
    })
}

/// `pixel uninstall --repo` step: remove the guard pixel wrote to
/// [`EXTENSION`] and pixel's files in [`LEGACY_DIR`].
pub(crate) fn uninstall(repo: &Path, dry_run: bool) -> Result<InstallStep> {
    let ext_file = repo.join(EXTENSION);
    let mut removed = remove_legacy(repo, dry_run)?;
    if is_managed_extension(&ext_file) {
        if !dry_run {
            fs::remove_file(&ext_file)?;
            remove_empty_dirs(&[&repo.join(".pi/extensions"), &repo.join(".pi")]);
        }
        removed.insert(0, EXTENSION.to_string());
    }
    let summary = if removed.is_empty() {
        "no pi guard extension found".to_string()
    } else {
        format!("removed {}", removed.join(" "))
    };
    Ok(InstallStep {
        id: "hooks.pi".into(),
        status: CheckStatus::Green,
        summary: install::dry_run_summary(dry_run, &summary),
        detail: Some(format!("ext={}", ext_file.display())),
    })
}

/// Take pixel's files out of `<repo>/`[`LEGACY_DIR`]: the managed guard is
/// deleted, the managed block leaves `AGENTS.md` (deleted when nothing else
/// is left in it, backed up otherwise), and the directories are removed once
/// empty. Returns the repository-relative paths touched.
fn remove_legacy(repo: &Path, dry_run: bool) -> Result<Vec<String>> {
    let dir = repo.join(LEGACY_DIR);
    let ext_file = dir.join("extensions").join("pixel-guard.ts");
    let agents_md = dir.join("AGENTS.md");
    let mut removed = Vec::new();
    if is_managed_extension(&ext_file) {
        if !dry_run {
            fs::remove_file(&ext_file)?;
        }
        removed.push(format!("{LEGACY_DIR}/extensions/pixel-guard.ts"));
    }
    // Best effort: an unreadable legacy file holds nothing pi ever loaded.
    let agents = fs::read_to_string(&agents_md).unwrap_or_default();
    if agents.contains(config::MANAGED_BEGIN) {
        let rest = config::strip_managed_block(&agents);
        if !dry_run {
            if rest.trim().is_empty() {
                fs::remove_file(&agents_md)?;
            } else {
                config::backup_if_changing(&agents_md, rest.as_bytes())?;
                fs::write(&agents_md, &rest)?;
            }
        }
        removed.push(format!("{LEGACY_DIR}/AGENTS.md"));
    }
    if !dry_run {
        remove_empty_dirs(&[&dir.join("extensions"), &dir]);
    }
    Ok(removed)
}

/// Remove each directory in order when it is empty; a directory that still
/// holds anything, or is absent, is left as it is.
fn remove_empty_dirs(dirs: &[&Path]) {
    for dir in dirs {
        // `remove_dir` refuses a non-empty directory, which is the rule here.
        let _ = fs::remove_dir(dir);
    }
}

/// The state of a repository's pi guard, as `pixel doctor` reports it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum GuardState {
    /// No guard anywhere: `install --repo` was not run for pi.
    Absent,
    /// The managed guard sits at [`EXTENSION`].
    Installed(PathBuf),
    /// A file at [`EXTENSION`] that pixel did not write.
    Foreign(PathBuf),
    /// Only the guard of an older release, in [`LEGACY_DIR`], which pi never
    /// loads in a project.
    Legacy(PathBuf),
}

/// Where the repository's pi guard stands.
pub(crate) fn guard_state(repo: &Path) -> GuardState {
    let ext_file = repo.join(EXTENSION);
    if ext_file.is_file() {
        return if is_managed_extension(&ext_file) {
            GuardState::Installed(ext_file)
        } else {
            GuardState::Foreign(ext_file)
        };
    }
    let legacy = repo
        .join(LEGACY_DIR)
        .join("extensions")
        .join("pixel-guard.ts");
    if is_managed_extension(&legacy) {
        GuardState::Legacy(legacy)
    } else {
        GuardState::Absent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn legacy_ext(repo: &Path) -> PathBuf {
        repo.join(LEGACY_DIR)
            .join("extensions")
            .join("pixel-guard.ts")
    }

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn managed(text: &str) -> String {
        format!(
            "{}\n{text}\n{}\n",
            config::MANAGED_BEGIN,
            config::MANAGED_END
        )
    }

    #[test]
    fn extension_source_should_name_the_binary_and_carry_the_marker() {
        let source = extension_source(Path::new("/opt/bin/pixel"));
        assert!(
            source.contains("const PIXEL_BIN = \"/opt/bin/pixel\";"),
            "{source}"
        );
        assert!(source.contains(config::MANAGED_BEGIN), "{source}");
        assert!(source.contains(config::MANAGED_END), "{source}");
        assert!(source.contains("[\"run-hook\", \"guard\"]"), "{source}");
        assert!(
            source.contains("export default function activate(pi)"),
            "{source}"
        );
    }

    #[test]
    fn install_should_write_the_guard_where_pi_discovers_project_extensions() {
        let dir = tempfile::tempdir().unwrap();
        let step = install(dir.path(), Path::new("/opt/bin/pixel"), false).unwrap();
        let written = fs::read_to_string(dir.path().join(".pi/extensions/pixel-guard.ts")).unwrap();
        assert_eq!(written, extension_source(Path::new("/opt/bin/pixel")));
        assert_eq!(step.status, CheckStatus::Green);
        assert!(!dir.path().join(LEGACY_DIR).exists());
    }

    #[test]
    fn install_should_move_a_legacy_guard_and_drop_a_rules_file_holding_only_pixel() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        write(&legacy_ext(repo), &managed("old guard"));
        write(
            &repo.join(LEGACY_DIR).join("AGENTS.md"),
            &managed("old rules"),
        );

        let step = install(repo, Path::new("/opt/bin/pixel"), false).unwrap();

        assert!(repo.join(EXTENSION).is_file());
        assert!(!repo.join(LEGACY_DIR).exists(), "empty legacy dir must go");
        let detail = step.detail.unwrap();
        assert!(
            detail.contains("removed .pi/agent/extensions/pixel-guard.ts .pi/agent/AGENTS.md"),
            "{detail}"
        );
    }

    #[test]
    fn install_should_keep_user_text_and_files_in_the_legacy_dir() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        let agents = repo.join(LEGACY_DIR).join("AGENTS.md");
        write(&agents, &format!("mine\n{}", managed("old rules")));
        write(
            &repo.join(LEGACY_DIR).join("extensions/other.ts"),
            "user ext",
        );
        write(&legacy_ext(repo), &managed("old guard"));

        install(repo, Path::new("/opt/bin/pixel"), false).unwrap();

        assert_eq!(fs::read_to_string(&agents).unwrap(), "mine\n");
        assert!(repo.join(LEGACY_DIR).join("extensions/other.ts").is_file());
        assert!(!legacy_ext(repo).exists());
    }

    #[test]
    fn install_should_leave_an_unmanaged_legacy_file_alone() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        write(&legacy_ext(repo), "the user's own guard");
        write(&repo.join(LEGACY_DIR).join("AGENTS.md"), "the user's rules");

        let step = install(repo, Path::new("/opt/bin/pixel"), false).unwrap();

        assert_eq!(
            fs::read_to_string(legacy_ext(repo)).unwrap(),
            "the user's own guard"
        );
        assert_eq!(
            fs::read_to_string(repo.join(LEGACY_DIR).join("AGENTS.md")).unwrap(),
            "the user's rules"
        );
        assert!(!step.detail.unwrap().contains("removed"));
    }

    #[test]
    fn install_dry_run_should_write_and_remove_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        write(&legacy_ext(repo), &managed("old guard"));

        let step = install(repo, Path::new("/opt/bin/pixel"), true).unwrap();

        assert!(!repo.join(EXTENSION).exists());
        assert!(legacy_ext(repo).is_file());
        assert!(step.summary.starts_with("[dry-run]"), "{}", step.summary);
        assert!(step.detail.unwrap().contains("removed"));
    }

    #[test]
    fn uninstall_should_remove_the_guard_and_the_legacy_files() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        install(repo, Path::new("/opt/bin/pixel"), false).unwrap();
        write(&legacy_ext(repo), &managed("old guard"));

        let step = uninstall(repo, false).unwrap();

        assert!(!repo.join(EXTENSION).exists());
        assert!(!legacy_ext(repo).exists());
        assert!(!repo.join(".pi").exists(), "empty .pi dirs must go");
        assert_eq!(
            step.summary,
            "removed .pi/extensions/pixel-guard.ts .pi/agent/extensions/pixel-guard.ts"
        );
    }

    #[test]
    fn uninstall_should_keep_a_foreign_extension_and_its_directory() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        write(&repo.join(EXTENSION), "the user's own guard");

        let step = uninstall(repo, false).unwrap();

        assert_eq!(
            fs::read_to_string(repo.join(EXTENSION)).unwrap(),
            "the user's own guard"
        );
        assert_eq!(step.summary, "no pi guard extension found");
    }

    #[test]
    fn uninstall_dry_run_should_report_without_removing() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        install(repo, Path::new("/opt/bin/pixel"), false).unwrap();

        let step = uninstall(repo, true).unwrap();

        assert!(repo.join(EXTENSION).is_file());
        assert_eq!(
            step.summary,
            "[dry-run] would report: removed .pi/extensions/pixel-guard.ts"
        );
    }

    #[test]
    fn remove_empty_dirs_should_keep_a_directory_that_holds_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let full = dir.path().join("full");
        let empty = dir.path().join("empty");
        write(&full.join("f"), "x");
        fs::create_dir_all(&empty).unwrap();

        remove_empty_dirs(&[&full, &empty]);

        assert!(full.join("f").is_file());
        assert!(!empty.exists());
    }

    #[test]
    fn guard_state_should_tell_every_case_apart() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        assert_eq!(guard_state(repo), GuardState::Absent);

        write(&legacy_ext(repo), "not pixel's");
        assert_eq!(guard_state(repo), GuardState::Absent);

        write(&legacy_ext(repo), &managed("old guard"));
        assert_eq!(guard_state(repo), GuardState::Legacy(legacy_ext(repo)));

        write(&repo.join(EXTENSION), "not pixel's");
        assert_eq!(guard_state(repo), GuardState::Foreign(repo.join(EXTENSION)));

        write(&repo.join(EXTENSION), &managed("guard"));
        assert_eq!(
            guard_state(repo),
            GuardState::Installed(repo.join(EXTENSION))
        );
    }
}

//! Warp project MCP configuration for starting Pixel from a trusted project.
//!
//! Warp loads `.warp/.mcp.json` for a project after the user approves trusting
//! that project. This module writes only Pixel's own server entry and never
//! attempts to grant that trust on the user's behalf.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::InstallError;
use crate::config;
use crate::install::{self, CheckStatus, InstallStep, Result};

/// The Warp MCP configuration file, relative to a repository root.
pub(crate) const CONFIG_FILE: &str = ".warp/.mcp.json";

/// Install Pixel's project-scoped Warp MCP server without replacing user data.
pub(crate) fn install(repo: &Path, exe: &Path, dry_run: bool) -> Result<InstallStep> {
    let path = repo.join(CONFIG_FILE);
    if crate::repo_git::is_tracked(repo, CONFIG_FILE) {
        return Ok(InstallStep {
            id: "mcp.warp".into(),
            status: CheckStatus::Yellow,
            summary: install::dry_run_summary(
                dry_run,
                ".warp/.mcp.json is tracked by git; Warp config embeds this machine's Pixel executable and repository paths, so it was not modified",
            ),
            detail: Some(format!("config={}", path.display())),
        });
    }
    let repo_root = absolute_repo(repo)?;
    let executable = absolute_executable(exe, &path)?;
    let mut root = read_root(&path)?;
    let servers = ensure_servers(&mut root, &path)?;

    if servers
        .get("pixel")
        .is_some_and(|existing| !owned_by_repo(existing, &repo_root))
    {
        return Err(invalid_config(
            &path,
            "mcpServers.pixel already exists and does not point to this repository",
        ));
    }

    let entry = server_entry(&executable, &repo_root);
    let changed = servers.get("pixel") != Some(&entry);
    if changed {
        servers.insert("pixel".into(), entry);
    }

    if changed && !dry_run {
        let serialized = pretty_json(&root)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        config::backup_if_changing(&path, serialized.as_bytes())?;
        fs::write(&path, serialized)?;
    }

    let summary = if changed {
        "Warp project MCP server installed"
    } else {
        "Warp project MCP server already configured"
    };
    Ok(InstallStep {
        id: "mcp.warp".into(),
        status: CheckStatus::Green,
        summary: install::dry_run_summary(dry_run, summary),
        detail: Some(format!("config={}", path.display())),
    })
}

/// Remove this repository's Pixel Warp MCP entry and preserve every other one.
pub(crate) fn uninstall(repo: &Path, _exe: &Path, dry_run: bool) -> Result<InstallStep> {
    let path = repo.join(CONFIG_FILE);
    if crate::repo_git::is_tracked(repo, CONFIG_FILE) {
        return Ok(InstallStep {
            id: "mcp.warp".into(),
            status: CheckStatus::Yellow,
            summary: install::dry_run_summary(
                dry_run,
                ".warp/.mcp.json is tracked by git; refusing to remove machine-specific Warp config",
            ),
            detail: Some(format!("config={}", path.display())),
        });
    }
    let repo_root = absolute_repo(repo)?;
    let mut root = read_root(&path)?;
    let mut removed = false;

    let remove_server_map =
        if let Some(servers) = root.get_mut("mcpServers").and_then(Value::as_object_mut) {
            if servers
                .get("pixel")
                .is_some_and(|entry| owned_by_repo(entry, &repo_root))
            {
                servers.remove("pixel");
                removed = true;
                servers.is_empty()
            } else {
                false
            }
        } else {
            false
        };
    if remove_server_map {
        root.remove("mcpServers");
    }

    if removed && !dry_run {
        let serialized = pretty_json(&root)?;
        config::backup_if_changing(&path, serialized.as_bytes())?;
        fs::write(&path, serialized)?;
    }

    let summary = if removed {
        "removed this repository's Pixel Warp MCP server"
    } else {
        "no Pixel Warp MCP server for this repository found"
    };
    Ok(InstallStep {
        id: "mcp.warp".into(),
        status: CheckStatus::Green,
        summary: install::dry_run_summary(dry_run, summary),
        detail: Some(format!("config={}", path.display())),
    })
}

/// Report whether Warp has no Pixel entry, the expected entry, or a conflicting entry.
pub(crate) fn check(repo: &Path, exe: &Path) -> Result<Option<bool>> {
    let path = repo.join(CONFIG_FILE);
    if !path.exists() {
        return Ok(None);
    }
    let repo_root = absolute_repo(repo)?;
    let executable = absolute_executable(exe, &path)?;
    let root = read_root(&path)?;
    let Some(servers) = root.get("mcpServers") else {
        return Ok(None);
    };
    let servers = servers
        .as_object()
        .ok_or_else(|| invalid_config(&path, "mcpServers must be a JSON object"))?;
    let Some(entry) = servers.get("pixel") else {
        return Ok(None);
    };
    Ok(Some(entry == &server_entry(&executable, &repo_root)))
}

fn absolute_repo(repo: &Path) -> Result<PathBuf> {
    repo.canonicalize().map_err(InstallError::Io)
}

fn absolute_executable(exe: &Path, config_path: &Path) -> Result<String> {
    if !exe.is_absolute() {
        return Err(invalid_config(
            config_path,
            "Pixel executable path must be absolute",
        ));
    }
    Ok(exe.to_string_lossy().into_owned())
}

fn server_entry(executable: &str, repo_root: &Path) -> Value {
    let repo = repo_root.to_string_lossy();
    serde_json::json!({
        "command": executable,
        "args": ["mcp", repo.as_ref()],
        "working_directory": repo.as_ref(),
    })
}

fn owned_by_repo(entry: &Value, repo_root: &Path) -> bool {
    let Some(object) = entry.as_object() else {
        return false;
    };
    let Some(command) = object.get("command").and_then(Value::as_str) else {
        return false;
    };
    let Some(args) = object.get("args").and_then(Value::as_array) else {
        return false;
    };
    let root = repo_root.to_string_lossy();
    object.len() == 3
        && config::PIXEL_EXECUTABLES.contains(
            &Path::new(command)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default(),
        )
        && args.len() == 2
        && args[0].as_str() == Some("mcp")
        && args[1].as_str() == Some(root.as_ref())
        && object.get("working_directory").and_then(Value::as_str) == Some(root.as_ref())
}

fn read_root(path: &Path) -> Result<Map<String, Value>> {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Map::new()),
        Err(error) => return Err(error.into()),
    };
    let value: Value = serde_json::from_str(&raw).map_err(|error| invalid_config(path, error))?;
    value
        .as_object()
        .cloned()
        .ok_or_else(|| invalid_config(path, "top-level JSON value must be an object"))
}

fn ensure_servers<'a>(
    root: &'a mut Map<String, Value>,
    path: &Path,
) -> Result<&'a mut Map<String, Value>> {
    if !root.contains_key("mcpServers") {
        root.insert("mcpServers".into(), Value::Object(Map::new()));
    }
    root.get_mut("mcpServers")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| invalid_config(path, "mcpServers must be a JSON object"))
}

fn pretty_json(root: &Map<String, Value>) -> Result<String> {
    Ok(format!("{}\n", serde_json::to_string_pretty(root)?))
}

fn invalid_config(path: &Path, reason: impl ToString) -> InstallError {
    InstallError::InvalidSettings {
        path: path.to_path_buf(),
        reason: reason.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::Value;

    use super::*;

    fn config_path(repo: &Path) -> PathBuf {
        repo.join(CONFIG_FILE)
    }

    fn pixel_entry(repo: &Path, exe: &Path) -> Value {
        server_entry(exe.to_str().unwrap(), &repo.canonicalize().unwrap())
    }

    #[test]
    fn install_should_merge_pixel_and_preserve_other_warp_servers() {
        let parent = tempfile::tempdir().unwrap();
        let repo = parent.path().join("project with spaces");
        fs::create_dir_all(&repo).unwrap();
        let path = config_path(&repo);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"{"other":{"command":"keep"},"mcpServers":{"lint":{"command":"lint"}}}"#,
        )
        .unwrap();
        let exe = Path::new("/opt/Pixel Tools/pixel");

        install(&repo, exe, false).unwrap();

        let value: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["other"]["command"], "keep");
        assert_eq!(value["mcpServers"]["lint"]["command"], "lint");
        assert_eq!(value["mcpServers"]["pixel"], pixel_entry(&repo, exe));
        assert_eq!(check(&repo, exe).unwrap(), Some(true));
    }

    #[test]
    fn install_should_be_idempotent_and_dry_run_should_not_write() {
        let repo = tempfile::tempdir().unwrap();
        let exe = Path::new("/opt/pixel");

        let preview = install(repo.path(), exe, true).unwrap();
        assert!(preview.summary.starts_with("[dry-run]"));
        assert!(!config_path(repo.path()).exists());

        install(repo.path(), exe, false).unwrap();
        let before = fs::read(config_path(repo.path())).unwrap();
        install(repo.path(), exe, false).unwrap();
        assert_eq!(fs::read(config_path(repo.path())).unwrap(), before);
    }

    #[test]
    fn install_should_refuse_foreign_pixel_and_malformed_settings() {
        let repo = tempfile::tempdir().unwrap();
        let path = config_path(repo.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, r#"{"mcpServers":{"pixel":{"command":"other"}}}"#).unwrap();
        assert!(matches!(
            install(repo.path(), Path::new("/opt/pixel"), false),
            Err(InstallError::InvalidSettings { .. })
        ));
        let foreign = fs::read(&path).unwrap();
        assert_eq!(foreign, br#"{"mcpServers":{"pixel":{"command":"other"}}}"#);
        fs::write(&path, "{").unwrap();
        assert!(matches!(
            install(repo.path(), Path::new("/opt/pixel"), false),
            Err(InstallError::InvalidSettings { .. })
        ));
        assert_eq!(fs::read(&path).unwrap(), b"{");
    }

    #[test]
    fn install_should_refuse_pixel_shaped_entries_with_custom_fields() {
        let repo = tempfile::tempdir().unwrap();
        let path = config_path(repo.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let root = repo.path().canonicalize().unwrap();
        let original = serde_json::json!({
            "mcpServers": {
                "pixel": {
                    "command": "/opt/old/pixel",
                    "args": ["mcp", root],
                    "working_directory": root,
                    "user_note": "keep this",
                }
            }
        })
        .to_string();
        fs::write(&path, &original).unwrap();

        assert!(matches!(
            install(repo.path(), Path::new("/opt/new/pixel"), false),
            Err(InstallError::InvalidSettings { .. })
        ));
        assert_eq!(fs::read_to_string(path).unwrap(), original);
    }

    #[test]
    fn uninstall_should_remove_only_pixel_entry_for_this_repository() {
        let repo = tempfile::tempdir().unwrap();
        let path = config_path(repo.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut root = Map::new();
        root.insert(
            "mcpServers".into(),
            serde_json::json!({
                "pixel": pixel_entry(repo.path(), Path::new("/opt/pixel")),
                "lint": {"command":"lint"}
            }),
        );
        fs::write(&path, pretty_json(&root).unwrap()).unwrap();

        uninstall(repo.path(), Path::new("/opt/pixel"), false).unwrap();

        let value: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(value["mcpServers"].get("pixel").is_none());
        assert_eq!(value["mcpServers"]["lint"]["command"], "lint");
    }

    #[test]
    fn uninstall_should_keep_an_entry_owned_by_another_repository() {
        let repo = tempfile::tempdir().unwrap();
        let path = config_path(repo.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"{"mcpServers":{"pixel":{"command":"/opt/pixel","args":["mcp","/another/repo"]}}}"#,
        )
        .unwrap();
        let before = fs::read(&path).unwrap();

        uninstall(repo.path(), Path::new("/opt/pixel"), false).unwrap();

        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn uninstall_should_preserve_foreign_commands_with_pixel_shaped_arguments() {
        let repo = tempfile::tempdir().unwrap();
        let path = config_path(repo.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let root = repo.path().canonicalize().unwrap();
        let entry = serde_json::json!({
            "command": "/opt/unrelated/tool",
            "args": ["mcp", root],
            "working_directory": root,
        });
        let original = serde_json::json!({"mcpServers":{"pixel":entry}}).to_string();
        fs::write(&path, &original).unwrap();

        uninstall(repo.path(), Path::new("/opt/pixel"), false).unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn uninstall_should_preserve_pixel_entry_with_custom_fields() {
        let repo = tempfile::tempdir().unwrap();
        let path = config_path(repo.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let root = repo.path().canonicalize().unwrap();
        let entry = serde_json::json!({
            "command": "/opt/pixel",
            "args": ["mcp", root],
            "working_directory": root,
            "user_note": "keep this",
        });
        let original = serde_json::json!({"mcpServers":{"pixel":entry}}).to_string();
        fs::write(&path, &original).unwrap();

        uninstall(repo.path(), Path::new("/opt/pixel"), false).unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn uninstall_dry_run_should_report_without_changing_the_config() {
        let repo = tempfile::tempdir().unwrap();
        let path = config_path(repo.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut root = Map::new();
        root.insert(
            "mcpServers".into(),
            serde_json::json!({
                "pixel": pixel_entry(repo.path(), Path::new("/opt/pixel"))
            }),
        );
        let original = pretty_json(&root).unwrap();
        fs::write(&path, &original).unwrap();

        let step = uninstall(repo.path(), Path::new("/opt/pixel"), true).unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        assert!(step.summary.starts_with("[dry-run]"), "{}", step.summary);
    }

    #[test]
    fn check_should_require_the_expected_absolute_executable() {
        let repo = tempfile::tempdir().unwrap();
        let path = config_path(repo.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            pretty_json(&Map::from_iter([(
                "mcpServers".into(),
                serde_json::json!({"pixel":pixel_entry(repo.path(), Path::new("/opt/pixel"))}),
            )]))
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            check(repo.path(), Path::new("/opt/pixel")).unwrap(),
            Some(true)
        );
        assert_eq!(
            check(repo.path(), Path::new("/opt/other-pixel")).unwrap(),
            Some(false)
        );
        assert!(matches!(
            check(repo.path(), Path::new("pixel")),
            Err(InstallError::InvalidSettings { .. })
        ));
    }

    #[test]
    fn check_should_distinguish_absent_pixel_config_from_conflicting_entry() {
        let repo = tempfile::tempdir().unwrap();
        let exe = Path::new("/opt/pixel");
        assert_eq!(check(repo.path(), exe).unwrap(), None);

        let path = config_path(repo.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, r#"{"mcpServers":{"other":{"command":"other"}}}"#).unwrap();
        assert_eq!(check(repo.path(), exe).unwrap(), None);

        fs::write(&path, r#"{"mcpServers":{"pixel":{"command":"other"}}}"#).unwrap();
        assert_eq!(check(repo.path(), exe).unwrap(), Some(false));
    }

    #[test]
    fn install_should_leave_a_git_tracked_warp_config_untouched() {
        let repo = tempfile::tempdir().unwrap();
        let path = config_path(repo.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "user-owned tracked config\n").unwrap();
        let git = pixel_git::GitRunner::new(repo.path());
        assert!(git.run_opt(&["init"]).is_some());
        assert!(git.run_opt(&["add", "--", CONFIG_FILE]).is_some());
        let before = fs::read(&path).unwrap();

        let step = install(repo.path(), Path::new("/opt/pixel"), false).unwrap();

        assert_eq!(step.status, CheckStatus::Yellow);
        assert!(step.summary.contains("tracked by git"), "{}", step.summary);
        assert_eq!(fs::read(&path).unwrap(), before);

        let removal = uninstall(repo.path(), Path::new("/opt/pixel"), false).unwrap();
        assert_eq!(removal.status, CheckStatus::Yellow);
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn check_should_report_an_unreadable_warp_config_instead_of_absent() {
        let repo = tempfile::tempdir().unwrap();
        fs::create_dir_all(repo.path().join(".warp/.mcp.json")).unwrap();
        assert!(check(repo.path(), Path::new("/opt/pixel")).is_err());
    }
}

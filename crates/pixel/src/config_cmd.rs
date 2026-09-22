//! `pixel config` — persistent layered settings.
//!
//! The `metrics` key controls whether the live 🟩 footer is emitted on
//! stderr after an ordinary command. Settings are opt-out layers: the
//! nearest scope wins — a repo-level `on` overrides a global `off` — and
//! an unset layer defaults to on. `--metrics=off` and `PIXEL_METRICS=0`
//! still veto a single invocation above every file layer.
//!
//! Files: `<root>/.pixel/config.json` (repo) and `~/.pixel/config.json`
//! (global). Both are flat `{"metrics": "on"|"off"}` objects; unknown keys
//! are preserved on write so the file can grow new settings.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

/// The metrics setting one layer declares, or `None` when the layer does
/// not pronounce itself (missing file, missing key, or malformed value).
fn read_metrics(path: &Path) -> Option<bool> {
    let text = std::fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    match value.get("metrics")?.as_str()? {
        "on" => Some(true),
        "off" => Some(false),
        _ => None,
    }
}

fn repo_config_path(root: &Path) -> PathBuf {
    root.join(".pixel").join("config.json")
}

fn global_config_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".pixel").join("config.json"))
}

/// Effective live-metrics setting for `root`: repo layer first, then the
/// machine-wide `~/.pixel/config.json`, then the on-by-default baseline.
pub fn metrics_enabled(root: Option<&Path>) -> bool {
    if let Some(root) = root
        && let Some(on) = read_metrics(&repo_config_path(root))
    {
        return on;
    }
    global_config_path()
        .as_deref()
        .and_then(read_metrics)
        .unwrap_or(true)
}

/// Which layer produced the effective setting — for `pixel config metrics`.
#[derive(Debug, PartialEq, Eq)]
enum Source {
    Repo,
    Global,
    Default,
}

fn metrics_resolution(root: Option<&Path>) -> (bool, Source) {
    if let Some(root) = root
        && let Some(on) = read_metrics(&repo_config_path(root))
    {
        return (on, Source::Repo);
    }
    if let Some(path) = global_config_path()
        && let Some(on) = read_metrics(&path)
    {
        return (on, Source::Global);
    }
    (true, Source::Default)
}

fn write_metrics(path: &Path, on: bool) -> Result<(), String> {
    let mut doc: Value = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .filter(|v: &Value| v.is_object())
        .unwrap_or_else(|| json!({}));
    doc["metrics"] = Value::String(if on { "on" } else { "off" }.to_string());
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    }
    // A tmp name shared across concurrent invocations could rename one
    // command's content as another's — scope it to this process. Same-process
    // callers serialize on ENV_LOCK in tests; the pid separates real ones.
    let tmp = path.with_file_name(format!(
        "{}.{}.tmp",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("config.json"),
        std::process::id(),
    ));
    if let Err(e) = std::fs::write(&tmp, format!("{doc}\n")) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("write {}: {e}", tmp.display()));
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("rename {}: {e}", path.display()));
    }
    Ok(())
}

/// `pixel config metrics [on|off] [--global]`: without a value, report the
/// effective setting and the layer that set it; with a value, persist it to
/// the chosen layer.
pub fn run_metrics(path: &Path, global: bool, value: Option<bool>) -> Result<(), String> {
    let root = crate::discover_root(path).ok();
    match value {
        None => {
            let (on, source) = metrics_resolution(root.as_deref());
            let layer = match source {
                Source::Repo => {
                    format!(
                        "repo {}",
                        repo_config_path(root.as_deref().unwrap()).display()
                    )
                }
                Source::Global => format!(
                    "global {}",
                    global_config_path()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default()
                ),
                Source::Default => "default (no config sets it)".to_string(),
            };
            println!("metrics: {} — {layer}", if on { "on" } else { "off" });
            Ok(())
        }
        Some(on) => {
            let target = if global {
                global_config_path().ok_or("no HOME for the global config")?
            } else {
                repo_config_path(
                    &root.ok_or("no repository root here — pass --global or run inside a repo")?,
                )
            };
            write_metrics(&target, on)?;
            println!(
                "metrics: {} — wrote {}",
                if on { "on" } else { "off" },
                target.display()
            );
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct HomeGuard(PathBuf);

    impl HomeGuard {
        fn set() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "pixel-config-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            Self(dir)
        }
    }

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn home_env() -> Option<std::ffi::OsString> {
        std::env::var_os("HOME")
    }

    fn point_home(dir: &Path) {
        // SAFETY: under crate::ENV_LOCK in tests only.
        unsafe { std::env::set_var("HOME", dir) };
    }

    fn restore_home(saved: Option<std::ffi::OsString>) {
        // SAFETY: under crate::ENV_LOCK in tests only.
        unsafe {
            match saved {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    fn write(path: &Path, doc: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, doc).unwrap();
    }

    #[test]
    fn unset_layers_default_on_and_nearest_scope_wins() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let home = HomeGuard::set();
        let saved = home_env();
        point_home(&home.0);

        let repo = home.0.join("repo");
        std::fs::create_dir_all(repo.join(".pixel")).unwrap();

        assert!(metrics_enabled(Some(&repo)), "unset defaults on");

        write(&repo.join(".pixel/config.json"), "{\"metrics\": \"off\"}");
        assert!(!metrics_enabled(Some(&repo)), "repo off wins");

        write(&repo.join(".pixel/config.json"), "{\"metrics\": \"on\"}");
        write(&home.0.join(".pixel/config.json"), "{\"metrics\": \"off\"}");
        assert!(metrics_enabled(Some(&repo)), "repo on overrides global off");
        assert!(
            !metrics_enabled(Some(&repo.join("no-config-here"))),
            "global off applies where no repo layer speaks"
        );

        restore_home(saved);
    }

    #[test]
    fn malformed_layers_are_skipped_not_fatal() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let home = HomeGuard::set();
        let saved = home_env();
        point_home(&home.0);

        let repo = home.0.join("repo");
        write(&repo.join(".pixel/config.json"), "not json");
        write(&home.0.join(".pixel/config.json"), "{\"metrics\": \"off\"}");
        assert!(
            !metrics_enabled(Some(&repo)),
            "malformed repo falls through to global"
        );

        write(&repo.join(".pixel/config.json"), "{\"metrics\": \"loud\"}");
        assert!(
            !metrics_enabled(Some(&repo)),
            "unknown value is not a setting"
        );
        write(
            &home.0.join(".pixel/config.json"),
            "{\"metrics\": \"off\", \"future\": 1}",
        );
        assert!(!metrics_enabled(Some(&repo)));

        restore_home(saved);
    }

    #[test]
    fn write_metrics_preserves_unknown_keys_and_roundtrips() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let home = HomeGuard::set();
        let saved = home_env();
        point_home(&home.0);

        let repo = home.0.join("repo");
        std::fs::create_dir_all(repo.join(".pixel")).unwrap();
        let cfg = repo.join(".pixel/config.json");
        write(&cfg, "{\"future\": {\"nested\": true}}");

        write_metrics(&cfg, false).unwrap();
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(&cfg).unwrap()).unwrap();
        assert_eq!(doc["metrics"], "off");
        assert_eq!(doc["future"]["nested"], true);
        assert!(!metrics_enabled(Some(&repo)));

        write_metrics(&cfg, true).unwrap();
        assert!(metrics_enabled(Some(&repo)));
        assert!(
            !cfg.with_extension("json.tmp").exists(),
            "temp file is renamed away"
        );

        restore_home(saved);
    }

    #[test]
    fn resolution_names_the_layer_that_set_the_value() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let home = HomeGuard::set();
        let saved = home_env();
        point_home(&home.0);

        let repo = home.0.join("repo");
        std::fs::create_dir_all(repo.join(".pixel")).unwrap();

        assert_eq!(
            metrics_resolution(Some(&repo)),
            (true, Source::Default),
            "nothing set → default on"
        );
        assert!(
            metrics_enabled(None),
            "no repo handle still resolves through the default"
        );

        write(&home.0.join(".pixel/config.json"), "{\"metrics\": \"off\"}");
        assert_eq!(
            metrics_resolution(Some(&repo)),
            (false, Source::Global),
            "global off shows up as the global layer"
        );
        assert!(
            !metrics_enabled(None),
            "without a repo the global layer is the answer"
        );

        write(&repo.join(".pixel/config.json"), "{\"metrics\": \"on\"}");
        assert_eq!(
            metrics_resolution(Some(&repo)),
            (true, Source::Repo),
            "repo on overrides a global off"
        );

        restore_home(saved);
    }

    #[test]
    fn a_failed_publish_leaves_no_tmp_file_behind() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let home = HomeGuard::set();
        // Renaming the tmp file onto an existing directory must fail — and
        // the tmp file must not be left behind.
        let dir = home.0.join("target-is-dir");
        std::fs::create_dir_all(&dir).unwrap();
        let err = write_metrics(&dir, false).expect_err("rename onto a dir fails");
        assert!(err.contains("rename"), "{err}");
        let leftovers: Vec<_> = std::fs::read_dir(&home.0)
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "tmp cleaned up: {leftovers:?}");
    }
}

//! `pixel config` — persistent layered settings.
//!
//! The `metrics` key controls whether the live 🟩 footer is emitted on
//! stderr after an ordinary command. Settings are opt-out layers: the
//! nearest scope wins — a repo-level `on` overrides a global `off` — and
//! an unset layer defaults to on. `--metrics=off` and `PIXEL_METRICS=0`
//! still veto a single invocation above every file layer.
//!
//! YAML files live under `.pixel/` in the repository and home directory.
//! Legacy JSON remains readable until install/edit creates the YAML equivalent.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

/// The environment overrides and YAML switches exposed in the overview.
const FEATURES: &[(&str, &str)] = &[
    ("daemon_auto_start", "PIXEL_DAEMON_AUTO_START"),
    ("task_context", "PIXEL_TASK_CONTEXT"),
    ("task_boundary", "PIXEL_TASK_BOUNDARY"),
];

fn resolved(root: Option<&Path>, key: &str) -> (Option<bool>, String) {
    let paths = root
        .map(repo_config_path)
        .into_iter()
        .chain(global_config_path());
    for path in paths {
        if let Some(value) = read_config_doc(&path).and_then(|doc| doc.get(key)?.as_bool()) {
            return (Some(value), path.display().to_string());
        }
    }
    (None, "default".into())
}

/// Environment overrides win; absent feature switches preserve the enabled baseline.
pub fn feature_enabled(root: Option<&Path>, key: &str, env: &str) -> bool {
    feature_resolution(root, key, env).0
}

fn feature_resolution(root: Option<&Path>, key: &str, env: &str) -> (bool, String) {
    if let Ok(value) = std::env::var(env) {
        return (!matches!(value.as_str(), "0" | "false" | "off"), env.into());
    }
    let (value, source) = resolved(root, key);
    (value.unwrap_or(true), source)
}

pub fn ensure_template(root: Option<&Path>) -> Result<PathBuf, String> {
    let directory = if let Some(root) = root {
        root.join(".pixel")
    } else {
        PathBuf::from(std::env::var_os("HOME").ok_or("no HOME for the global config")?)
            .join(".pixel")
    };
    let path = directory.join(crate::config_file::FILE_NAME);
    crate::config_file::ensure(&path)?;
    Ok(path)
}

pub fn edit(path: &Path, repo: bool) -> Result<(), String> {
    let root = if repo {
        Some(crate::discover_root(path)?)
    } else {
        None
    };
    let path = ensure_template(root.as_deref())?;
    let editor = std::env::var("VISUAL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            std::env::var("EDITOR")
                .ok()
                .filter(|s| !s.trim().is_empty())
        })
        .unwrap_or_else(|| "vi".into());
    let args = shell_words::split(&editor).map_err(|_| "invalid quoting in VISUAL/EDITOR")?;
    let (program, args) = args.split_first().ok_or("empty editor command")?;
    let status = std::process::Command::new(program)
        .args(args)
        .arg(&path)
        .status()
        .map_err(|e| format!("launch editor: {e}"))?;
    if !status.success() {
        return Err(format!("editor exited with {status}"));
    }
    validate(&path)?;
    println!("configuration: {}", path.display());
    Ok(())
}

fn validate(path: &Path) -> Result<(), String> {
    let doc = crate::config_file::load(path)?;
    for (key, _) in FEATURES {
        if doc.get(key).is_some_and(|v| !v.is_boolean()) {
            return Err(format!("{}: {key} must be true or false", path.display()));
        }
    }
    if doc
        .get("metrics")
        .is_some_and(|v| !matches!(v.as_str(), Some("on" | "off")))
    {
        return Err(format!("{}: metrics must be on or off", path.display()));
    }
    classify_enabled_in(&doc)?;
    if let Some(classify) = doc.get("classify") {
        if !classify.is_object() {
            return Err(format!("{}: classify must be a mapping", path.display()));
        }
        if classify
            .get("engine")
            .is_some_and(|v| !matches!(v.as_str(), Some("auto" | "local" | "remote")))
        {
            return Err(format!(
                "{}: classify.engine must be auto, local, or remote",
                path.display()
            ));
        }
        if classify.get("remote_preset").is_some_and(|v| {
            v.as_str()
                .and_then(crate::decide_remote::Preset::parse_name)
                .is_none()
        }) {
            return Err(format!(
                "{}: unknown classify.remote_preset",
                path.display()
            ));
        }
    }
    Ok(())
}

/// Print only known public settings; arbitrary configuration can contain secrets.
pub fn overview(path: &Path) -> Result<(), String> {
    let root = crate::discover_root(path).ok();
    let global = global_config_path().ok_or("no HOME for the global config")?;
    println!("global: {}", global.display());
    validate(&global)?;
    if let Some(root) = root.as_deref() {
        let path = repo_config_path(root);
        println!("repo: {}", path.display());
        validate(&path)?;
    }
    let (metrics, source) = metrics_resolution(root.as_deref());
    if std::env::var_os("PIXEL_METRICS").is_some_and(|v| v == "0") {
        println!("metrics: off (PIXEL_METRICS)");
    } else {
        println!(
            "metrics: {} ({source:?})",
            if metrics { "on" } else { "off" }
        );
    }
    for (key, env) in FEATURES {
        let (enabled, source) = feature_resolution(root.as_deref(), key, env);
        println!("{key}: {enabled} ({source})");
    }
    println!("classify.enabled: {} (global)", classify_enabled()?);
    println!(
        "classify.engine: {}",
        classify_engine().unwrap_or_else(|| "auto (default)".into())
    );
    if let Some(preset) = classify_remote_preset() {
        println!("classify.remote_preset: {}", preset.display());
    }
    let doc = crate::config_file::load(&global)?;
    if let Some(keys) = doc.get("remote_keys").and_then(Value::as_object) {
        for (name, value) in keys {
            // Provider names are user input too: print only recognized presets.
            if let Some(preset) = crate::decide_remote::Preset::parse_name(name) {
                let name = preset.display();
                println!(
                    "remote_keys.{name}: {}",
                    if value.as_str().is_some_and(|s| !s.is_empty()) {
                        "set"
                    } else {
                        "unset"
                    }
                );
            }
        }
    }
    println!("Setup: pixel config setup (interactive global settings)");
    println!("Edit: pixel config edit (global), pixel config edit --repo (repository)");
    Ok(())
}

/// The metrics setting one layer declares, or `None` when the layer does
/// not pronounce itself (missing file, missing key, or malformed value).
fn read_metrics(path: &Path) -> Option<bool> {
    let value = read_config_doc(path)?;
    match value.get("metrics")?.as_str()? {
        "on" => Some(true),
        "off" => Some(false),
        _ => None,
    }
}

fn repo_config_path(root: &Path) -> PathBuf {
    crate::config_file::preferred_path(&root.join(".pixel"))
}

fn global_config_path() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(|home| crate::config_file::preferred_path(&PathBuf::from(home).join(".pixel")))
}

/// Effective live-metrics setting for `root`: repo layer first, then the
/// global configuration, then the on-by-default baseline.
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

fn write_doc(path: &Path, mutate: impl FnOnce(&mut Value)) -> Result<(), String> {
    let before = crate::config_file::load(path)?;
    let mut doc = before.clone();
    mutate(&mut doc);
    let rendered = crate::config_file::render(path, &before, &doc)?;
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
    if let Err(e) = write_private(&tmp, rendered.as_bytes()) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("write {}: {e}", tmp.display()));
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("rename {}: {e}", path.display()));
    }
    Ok(())
}

/// Write `bytes` to a new file only its owner can read. The global config
/// holds provider API keys and the rename keeps the new inode's mode, so
/// every write — not only the one storing a key — must create it 0600.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    // A leftover tmp from a crashed run would keep its old mode.
    let _ = std::fs::remove_file(path);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(bytes)
}

/// The key a `pixel config remote-key` value stands for: `-` reads the
/// first line of `stdin`, which keeps the secret out of shell history and
/// `ps`; anything else is the key itself.
pub fn key_from_arg(
    value: Option<String>,
    stdin: &mut dyn BufRead,
) -> Result<Option<String>, String> {
    if value.as_deref() != Some("-") {
        return Ok(value);
    }
    let mut line = String::new();
    stdin
        .read_line(&mut line)
        .map_err(|e| format!("remote-key: read stdin: {e}"))?;
    Ok(Some(line.trim().to_string()))
}

fn write_metrics(path: &Path, on: bool) -> Result<(), String> {
    write_doc(path, |doc| {
        doc["metrics"] = Value::String(if on { "on" } else { "off" }.to_string());
    })
}

/// Global kill switch: checked before classify reads input or opens an engine.
pub fn classify_enabled() -> Result<bool, String> {
    let path = global_config_path().ok_or("no HOME for the global config")?;
    classify_enabled_in(&crate::config_file::load(&path)?)
}

fn classify_enabled_in(doc: &Value) -> Result<bool, String> {
    match doc.get("classify").and_then(|c| c.get("enabled")) {
        None => Ok(false),
        Some(value) => value
            .as_bool()
            .ok_or_else(|| "classify.enabled must be true or false".into()),
    }
}

/// Save a global classify switch while retaining engine and credential settings.
pub fn set_classify_enabled(enabled: bool) -> Result<(), String> {
    let path = global_config_path().ok_or("no HOME for the global config")?;
    write_doc(&path, |doc| {
        if !doc.get("classify").is_some_and(Value::is_object) {
            doc["classify"] = json!({});
        }
        doc["classify"]["enabled"] = json!(enabled);
    })
}

/// Terminal adapter shared by explicit setup and interactive global installation.
#[cfg_attr(test, mutants::skip)] // Terminal and provider adapters; draft/save policy tested with injected I/O.
pub fn setup() -> Result<(), String> {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        return Err(
            "setup needs a terminal; use pixel config edit or pixel config classify off".into(),
        );
    }
    let path = ensure_template(None)?;
    let mut input = std::io::stdin().lock();
    let mut output = std::io::stderr().lock();
    if setup_with(&path, &mut input, &mut output)? && classify_enabled()? {
        crate::classify_setup::install_step(true, &mut input, &mut output)?;
    }
    Ok(())
}

fn setup_with(
    path: &Path,
    input: &mut dyn BufRead,
    output: &mut dyn Write,
) -> Result<bool, String> {
    validate(path)?;
    let mut doc = crate::config_file::load(path)?;
    writeln!(output, "Pixel setup — global settings\nFile: {}\nEnter keeps the shown value. q or Ctrl-D cancels without saving.\nRepository and environment overrides still apply.", path.display()).map_err(|e| e.to_string())?;
    let metrics = doc.get("metrics").and_then(Value::as_str) != Some("off");
    let Some(metrics) = ask_bool(
        input,
        output,
        "Show command timing and estimated savings?",
        metrics,
    )?
    else {
        return Ok(false);
    };
    doc["metrics"] = json!(if metrics { "on" } else { "off" });
    for (key, label) in [
        (
            "daemon_auto_start",
            "Start the background repository daemon on demand?",
        ),
        (
            "task_context",
            "Suggest relevant code when an agent receives a prompt?",
        ),
        ("task_boundary", "Detect task changes in agent prompts?"),
    ] {
        let current = doc.get(key).and_then(Value::as_bool).unwrap_or(true);
        let Some(value) = ask_bool(input, output, label, current)? else {
            return Ok(false);
        };
        doc[key] = json!(value);
    }
    writeln!(output, "Classify is optional AI classification, separate from code search. Local models need a download; remote providers receive your input and may charge per call.").map_err(|e| e.to_string())?;
    let Some(enabled) = ask_bool(
        input,
        output,
        "Allow pixel classify?",
        classify_enabled_in(&doc)?,
    )?
    else {
        return Ok(false);
    };
    if !doc.get("classify").is_some_and(Value::is_object) {
        doc["classify"] = json!({});
    }
    doc["classify"]["enabled"] = json!(enabled);
    writeln!(output, "Review: metrics={}, daemon_auto_start={}, task_context={}, task_boundary={}, classify.enabled={}", doc["metrics"], doc["daemon_auto_start"], doc["task_context"], doc["task_boundary"], doc["classify"]["enabled"]).map_err(|e| e.to_string())?;
    if ask_bool(input, output, "Save these settings?", false)? != Some(true) {
        return Ok(false);
    }
    write_doc(path, |current| *current = doc)?;
    writeln!(
        output,
        "Saved. Run pixel config to see effective settings or pixel config setup to change them."
    )
    .map_err(|e| e.to_string())?;
    Ok(true)
}

fn ask_bool(
    input: &mut dyn BufRead,
    output: &mut dyn Write,
    label: &str,
    current: bool,
) -> Result<Option<bool>, String> {
    loop {
        write!(
            output,
            "{label} [{}] > ",
            if current { "Y/n" } else { "y/N" }
        )
        .map_err(|e| e.to_string())?;
        output.flush().map_err(|e| e.to_string())?;
        let mut line = String::new();
        if input.read_line(&mut line).map_err(|e| e.to_string())? == 0 {
            return Ok(None);
        }
        match line.trim().to_ascii_lowercase().as_str() {
            "" => return Ok(Some(current)),
            "y" | "yes" => return Ok(Some(true)),
            "n" | "no" => return Ok(Some(false)),
            "q" => return Ok(None),
            _ => writeln!(output, "Type y, n, Enter, or q.").map_err(|e| e.to_string())?,
        }
    }
}

/// The stored classify engine preference: `local`, `remote`, or `auto`.
pub fn classify_engine() -> Option<String> {
    global_config_path()
        .as_deref()
        .and_then(read_config_doc)
        .and_then(|doc| {
            doc.get("classify")?
                .get("engine")?
                .as_str()
                .map(str::to_string)
        })
}

/// The provider selected by the interactive remote setup.
pub fn classify_remote_preset() -> Option<crate::decide_remote::Preset> {
    let doc = read_config_doc(&global_config_path()?)?;
    crate::decide_remote::Preset::parse_name(doc.get("classify")?.get("remote_preset")?.as_str()?)
}

/// Store the remote engine and its provider together, preserving other settings.
pub fn set_classify_remote(preset: crate::decide_remote::Preset) -> Result<(), String> {
    let path = global_config_path().ok_or("no HOME for the global config")?;
    write_doc(&path, |doc| {
        if !doc.get("classify").is_some_and(Value::is_object) {
            doc["classify"] = json!({});
        }
        doc["classify"]["engine"] = json!("remote");
        doc["classify"]["remote_preset"] = json!(preset.display());
    })
}

/// The recorded local Ollaya daemon launch (base, model name, env, argv).
pub fn ollaya_launch() -> Option<Value> {
    global_config_path()
        .as_deref()
        .and_then(read_config_doc)
        .and_then(|doc| doc.get("classify")?.get("ollaya").cloned())
}

/// Persist the classify engine preference.
pub fn set_classify_engine(value: &str) -> Result<(), String> {
    let path = global_config_path().ok_or("no HOME for the global config")?;
    write_doc(&path, |doc| {
        if !doc.get("classify").is_some_and(Value::is_object) {
            doc["classify"] = json!({});
        }
        doc["classify"]["engine"] = Value::String(value.to_string());
    })
}

/// Persist the local Ollaya server launch record.
pub fn set_ollaya_launch(launch: &Value) -> Result<(), String> {
    let path = global_config_path().ok_or("no HOME for the global config")?;
    write_doc(&path, |doc| {
        if !doc.get("classify").is_some_and(Value::is_object) {
            doc["classify"] = json!({});
        }
        doc["classify"]["ollaya"] = launch.clone();
    })
}

fn read_config_doc(path: &Path) -> Option<Value> {
    crate::config_file::load(path).ok()
}

/// The stored API key for a remote decision preset, if the global config
/// carries one. Keys live only in the global configuration under
/// `remote_keys` — never in the repo layer, never echoed back by the CLI.
pub fn remote_key(preset: crate::decide_remote::Preset) -> Option<String> {
    let path = global_config_path()?;
    let doc = read_config_doc(&path)?;
    doc.get("remote_keys")?
        .get(preset.display())?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// `pixel config remote-key <preset> [key]`: with a value, persist it to
/// the global configuration (created 0600 on unix — it holds secrets);
/// without one, report whether a key is stored. `--clear` removes it.
/// The key itself is never printed.
pub fn run_remote_key(
    preset: crate::decide_remote::Preset,
    value: Option<String>,
    clear: bool,
) -> Result<(), String> {
    let name = preset.display();
    let path = global_config_path().ok_or("no HOME for the global config")?;
    if clear {
        write_doc(&path, |doc| {
            if let Some(keys) = doc.get_mut("remote_keys").and_then(Value::as_object_mut) {
                keys.remove(name);
            }
        })?;
        println!("remote-key {name}: cleared — wrote {}", path.display());
        return Ok(());
    }
    match value {
        Some(key) if !key.is_empty() => {
            write_doc(&path, |doc| {
                if !doc.get("remote_keys").is_some_and(Value::is_object) {
                    doc["remote_keys"] = json!({});
                }
                doc["remote_keys"][name] = Value::String(key);
            })?;
            println!("remote-key {name}: set — wrote {}", path.display());
            Ok(())
        }
        Some(_) => Err("remote-key: empty key".to_string()),
        None => {
            let state = if remote_key(preset).is_some() {
                "set"
            } else {
                "unset"
            };
            println!("remote-key {name}: {state} — {}", path.display());
            Ok(())
        }
    }
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

    #[test]
    fn setup_should_save_choices_preserve_secrets_and_allow_cancellation() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let home = HomeGuard::set();
        let path = home.0.join("config.yaml");
        let original = "# personal comment\nremote_keys: {openrouter: hidden-secret}\nclassify: {engine: remote}\nunknown: 42\n";
        write(&path, original);
        for answers in ["q\n", "", "n\nn\nn\nn\nn\nn\n", "n\nn\nn\nn\nn\n"] {
            assert!(
                !setup_with(&path, &mut std::io::Cursor::new(answers), &mut Vec::new()).unwrap()
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        }
        let mut output = Vec::new();
        assert!(
            setup_with(
                &path,
                &mut std::io::Cursor::new("n\nn\nn\nn\nn\ny\n"),
                &mut output
            )
            .unwrap()
        );
        assert_eq!(
            crate::config_file::load(&path).unwrap(),
            json!({
                "remote_keys": {"openrouter":"hidden-secret"}, "unknown":42,
                "metrics":"off", "daemon_auto_start":false, "task_context":false,
                "task_boundary":false, "classify":{"engine":"remote", "enabled":false}
            })
        );
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("classify.enabled=false"));
        assert!(output.contains("Saved."));
        assert!(!output.contains("hidden-secret"));
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("# personal comment")
        );
        // Enter keeps saved values; explicit yes re-enables every switch.
        assert!(
            setup_with(
                &path,
                &mut std::io::Cursor::new("\n\n\n\n\ny\n"),
                &mut Vec::new()
            )
            .unwrap()
        );
        assert_eq!(
            crate::config_file::load(&path).unwrap()["classify"]["enabled"],
            false
        );
        assert!(
            setup_with(
                &path,
                &mut std::io::Cursor::new("y\ny\ny\ny\ny\ny\n"),
                &mut Vec::new()
            )
            .unwrap()
        );
        let doc = crate::config_file::load(&path).unwrap();
        assert_eq!(doc["metrics"], "on");
        for key in ["daemon_auto_start", "task_context", "task_boundary"] {
            assert_eq!(doc[key], true);
        }
        assert_eq!(doc["classify"]["enabled"], true);
    }

    #[test]
    fn setup_should_keep_classify_disabled_for_a_new_user() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let home = HomeGuard::set();
        let path = home.0.join("config.yaml");
        assert!(
            setup_with(
                &path,
                &mut std::io::Cursor::new("\n\n\n\n\ny\n"),
                &mut Vec::new()
            )
            .unwrap()
        );
        assert_eq!(
            crate::config_file::load(&path).unwrap(),
            json!({
                "metrics":"on", "daemon_auto_start":true, "task_context":true,
                "task_boundary":true, "classify":{"enabled":false}
            })
        );
    }

    #[test]
    fn prompts_should_keep_defaults_retry_invalid_answers_and_cancel_on_eof() {
        for (answer, current, expected) in [
            ("\n", true, Some(true)),
            ("\n", false, Some(false)),
            ("YES\n", false, Some(true)),
            ("no\n", true, Some(false)),
            ("q\ny\n", true, None),
            ("", true, None),
            ("invalid\nn\n", true, Some(false)),
        ] {
            let mut output = Vec::new();
            assert_eq!(
                ask_bool(
                    &mut std::io::Cursor::new(answer),
                    &mut output,
                    "Choice",
                    current
                )
                .unwrap(),
                expected
            );
            assert!(String::from_utf8(output).unwrap().contains("Choice"));
        }
    }

    #[test]
    fn classify_switch_should_default_off_and_reject_non_boolean_values() {
        for (doc, expected) in [
            (json!({}), false),
            (json!({"classify":{"engine":"remote"}}), false),
            (json!({"classify":{"enabled":true}}), true),
            (json!({"classify":{"enabled":false}}), false),
        ] {
            assert_eq!(classify_enabled_in(&doc).unwrap(), expected);
        }
        assert_eq!(
            classify_enabled_in(&json!({"classify":{"enabled":"false"}})).unwrap_err(),
            "classify.enabled must be true or false"
        );
    }

    #[test]
    fn fresh_template_should_have_no_active_options_and_use_the_requested_root() {
        let home = HomeGuard::set();
        let path = ensure_template(Some(&home.0)).unwrap();
        assert_eq!(path, home.0.join(".pixel/config.yaml"));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# metrics:"));
        assert!(
            text.lines()
                .all(|line| line.trim().is_empty() || line.starts_with('#')),
            "uncommenting an example must not conflict with an active empty mapping: {text}"
        );
        assert_eq!(crate::config_file::load(&path).unwrap(), json!({}));
        ensure_template(Some(&home.0)).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
    }

    #[test]
    fn render_should_reject_a_concurrent_change_instead_of_silently_losing_values() {
        let home = HomeGuard::set();
        let path = home.0.join("config.yaml");
        let before = json!({"metrics":"on"});
        write(&path, "metrics: 'off'\n");
        let error = crate::config_file::render(&path, &before, &before).unwrap_err();
        assert!(error.contains("file unchanged"));
        assert_eq!(std::fs::read_to_string(path).unwrap(), "metrics: 'off'\n");
    }

    #[test]
    fn yaml_should_drive_all_existing_readers_after_migration() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let home = HomeGuard::set();
        let saved = home_env();
        point_home(&home.0);
        let legacy = home.0.join(".pixel/config.json");
        write(
            &legacy,
            r#"{"metrics":"off","classify":{"engine":"remote","remote_preset":"deepseek","ollaya":{"base":"http://localhost:11435","argv":["ollaya","serve"]}},"remote_keys":{"deepseek":"key"}}"#,
        );
        let yaml = ensure_template(None).unwrap();
        write(&legacy, "{}");
        assert!(!metrics_enabled(None));
        assert_eq!(classify_engine().as_deref(), Some("remote"));
        assert_eq!(
            classify_remote_preset(),
            Some(crate::decide_remote::Preset::Deepseek)
        );
        assert_eq!(
            ollaya_launch(),
            Some(json!({"base":"http://localhost:11435","argv":["ollaya","serve"]}))
        );
        assert_eq!(
            remote_key(crate::decide_remote::Preset::Deepseek).as_deref(),
            Some("key")
        );
        set_classify_engine("local").unwrap();
        assert_eq!(classify_engine().as_deref(), Some("local"));
        assert_eq!(crate::config_file::load(&yaml).unwrap()["metrics"], "off");
        restore_home(saved);
    }

    #[test]
    fn daemon_start_should_be_disabled_by_repo_yaml_without_an_environment_switch() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let home = HomeGuard::set();
        write(
            &home.0.join(".pixel/config.yaml"),
            "daemon_auto_start: false\n",
        );
        let saved = std::env::var_os("PIXEL_DAEMON_AUTO_START");
        // SAFETY: process-wide environment access is serialized by ENV_LOCK.
        unsafe {
            std::env::remove_var("PIXEL_DAEMON_AUTO_START");
        }
        assert!(matches!(
            crate::auto_start_daemon(&home.0, &crate::Request::Status {}),
            Err(crate::InProcessReason::AutoStartDisabled)
        ));
        // SAFETY: same lock as above.
        unsafe {
            if let Some(value) = saved {
                std::env::set_var("PIXEL_DAEMON_AUTO_START", value);
            }
        }
    }

    #[test]
    fn yaml_should_preserve_comments_and_unknown_values_when_commands_update_settings() {
        let home = HomeGuard::set();
        let path = home.0.join("config.yaml");
        let original = "# my settings\nmetrics: \"on\" # keep footer note\nclassify:\n  # provider choice\n  engine: auto\n  custom: 42\nremote_keys:\n  ollama: old\n  openrouter: keep\n";
        write(&path, original);
        write_doc(&path, |doc| {
            doc["metrics"] = json!("off");
            doc["classify"]["engine"] = json!("remote");
            doc["remote_keys"].as_object_mut().unwrap().remove("ollama");
            doc["remote_keys"]["deepseek"] = json!("special: # ' \"\nsecret");
        })
        .unwrap();
        let result = std::fs::read_to_string(&path).unwrap();
        for comment in ["# my settings", "# keep footer note", "# provider choice"] {
            assert!(result.contains(comment), "{result}");
        }
        assert_eq!(
            crate::config_file::load(&path).unwrap(),
            json!({
                "metrics": "off", "classify": {"engine": "remote", "custom": 42},
                "remote_keys": {"openrouter": "keep", "deepseek": "special: # ' \"\nsecret"}
            })
        );
        #[cfg(unix)]
        assert_eq!(mode(&path), 0o600);
    }

    #[test]
    fn migration_should_preserve_legacy_values_and_leave_existing_yaml_alone() {
        let home = HomeGuard::set();
        let yaml = home.0.join("config.yaml");
        let legacy = home.0.join("config.json");
        let original = r#"{"metrics":"off","remote_keys":{"ollama":"secret"},"future":[1,2]}"#;
        write(&legacy, original);
        assert_eq!(crate::config_file::preferred_path(&home.0), legacy);
        crate::config_file::ensure(&yaml).unwrap();
        assert_eq!(
            crate::config_file::load(&yaml).unwrap(),
            serde_json::from_str::<Value>(original).unwrap()
        );
        assert_eq!(std::fs::read_to_string(&legacy).unwrap(), original);
        assert_eq!(crate::config_file::preferred_path(&home.0), yaml);
        let contents = std::fs::read_to_string(&yaml).unwrap();
        assert!(contents.contains("# metrics:"));
        write(&legacy, "{}");
        crate::config_file::ensure(&yaml).unwrap();
        assert_eq!(std::fs::read_to_string(&yaml).unwrap(), contents);
        #[cfg(unix)]
        assert_eq!(mode(&yaml), 0o600);
    }

    #[test]
    fn invalid_config_should_fail_without_overwriting_or_echoing_secrets() {
        let home = HomeGuard::set();
        for (extension, contents) in [
            ("yaml", "remote_keys: [secret-invalid"),
            ("json", "{\"secret-invalid\":"),
            ("yaml", "- secret-invalid"),
        ] {
            let path = home.0.join(format!("config.{extension}"));
            write(&path, contents);
            let err = write_metrics(&path, false).unwrap_err();
            assert!(err.contains("configuration"));
            assert!(!err.contains("secret-invalid"));
            assert_eq!(std::fs::read_to_string(path).unwrap(), contents);
        }
    }

    #[test]
    fn invalid_feature_values_should_fall_through_without_claiming_their_source() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let home = HomeGuard::set();
        let saved = home_env();
        point_home(&home.0);
        let repo = home.0.join("repo");
        let global = home.0.join(".pixel/config.yaml");
        let local = repo.join(".pixel/config.yaml");
        let env = format!("PIXEL_TEST_INVALID_FEATURE_{}", std::process::id());
        for (key, _) in FEATURES {
            for invalid in ["'off'", "'no'", "0", "null", "{}"] {
                write(&global, &format!("{key}: false\n"));
                write(&local, &format!("{key}: {invalid}\n"));
                assert_eq!(
                    feature_resolution(Some(&repo), key, &env),
                    (false, global.display().to_string()),
                    "invalid repository {key}={invalid} must not hide the global opt-out"
                );
                write(&global, &format!("{key}: {invalid}\n"));
                assert_eq!(
                    feature_resolution(Some(&repo), key, &env),
                    (true, "default".into()),
                    "invalid values at both layers must leave the default as the source"
                );
            }
        }
        restore_home(saved);
    }

    #[test]
    fn features_should_resolve_environment_then_repo_then_global_then_default() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let home = HomeGuard::set();
        let saved = home_env();
        point_home(&home.0);
        let repo = home.0.join("repo");
        let env = format!("PIXEL_TEST_FEATURE_{}", std::process::id());
        for (key, _) in FEATURES {
            let global = home.0.join(".pixel/config.yaml");
            let local = repo.join(".pixel/config.yaml");
            write(&global, "{}");
            write(&local, "{}");
            assert_eq!(
                feature_resolution(Some(&repo), key, &env),
                (true, "default".into())
            );
            write(&global, &format!("{key}: false\n"));
            assert_eq!(
                feature_resolution(Some(&repo), key, &env),
                (false, global.display().to_string())
            );
            write(&local, &format!("{key}: true\n"));
            assert_eq!(
                feature_resolution(Some(&repo), key, &env),
                (true, local.display().to_string())
            );
            for (value, enabled) in [
                ("0", false),
                ("off", false),
                ("false", false),
                ("1", true),
                ("", true),
                ("no", true),
            ] {
                // SAFETY: this test owns the unique environment name under ENV_LOCK.
                unsafe {
                    std::env::set_var(&env, value);
                }
                assert_eq!(
                    feature_resolution(Some(&repo), key, &env),
                    (enabled, env.clone())
                );
                assert_eq!(feature_enabled(Some(&repo), key, &env), enabled);
            }
            // SAFETY: same lock and unique variable as above.
            unsafe {
                std::env::remove_var(&env);
            }
        }
        restore_home(saved);
    }

    #[test]
    fn template_should_be_inert_and_validation_should_reject_wrong_known_types() {
        let home = HomeGuard::set();
        let path = home.0.join("config.yaml");
        crate::config_file::ensure(&path).unwrap();
        assert_eq!(crate::config_file::load(&path).unwrap(), json!({}));
        validate(&path).unwrap();
        for invalid in [
            "metrics: false",
            "task_context: 'off'",
            "classify: []",
            "classify: {engine: invalid}",
            "classify: {remote_preset: invalid}",
        ] {
            write(&path, invalid);
            assert!(validate(&path).is_err(), "{invalid}");
        }
        write(
            &path,
            "metrics: 'on'\nclassify: {engine: local, remote_preset: deepseek}\ntask_context: true",
        );
        validate(&path).unwrap();
    }

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

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[cfg(unix)]
    #[test]
    fn every_config_write_leaves_the_stored_keys_owner_only() {
        let home = HomeGuard::set();
        let cfg = home.0.join(".pixel/config.json");
        write(&cfg, "{\"remote_keys\": {\"openrouter\": \"sk-secret\"}}");
        // A world-readable leftover tmp from a crashed write must not lend
        // its mode to the next one.
        let tmp = cfg.with_file_name(format!("config.json.{}.tmp", std::process::id()));
        write(&tmp, "{}");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        write_metrics(&cfg, false).unwrap();
        assert_eq!(
            mode(&cfg),
            0o600,
            "an unrelated write keeps the keys private"
        );
        let text = std::fs::read_to_string(&cfg).unwrap();
        assert!(
            text.contains("sk-secret") && text.contains("\"off\""),
            "{text}"
        );
    }

    #[test]
    fn a_dash_reads_the_key_from_stdin_and_anything_else_is_the_key() {
        let mut stdin = std::io::Cursor::new(b"sk-from-stdin\nignored\n".to_vec());
        assert_eq!(
            key_from_arg(Some("-".into()), &mut stdin),
            Ok(Some("sk-from-stdin".into()))
        );
        let mut untouched = std::io::Cursor::new(b"never read\n".to_vec());
        assert_eq!(
            key_from_arg(Some("sk-inline".into()), &mut untouched),
            Ok(Some("sk-inline".into()))
        );
        assert_eq!(key_from_arg(None, &mut untouched), Ok(None));
        assert_eq!(untouched.position(), 0, "stdin is read only for `-`");
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
    fn remote_key_roundtrips_masked_and_env_free() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let home = HomeGuard::set();
        let saved = home_env();
        point_home(&home.0);
        let preset = crate::decide_remote::Preset::Ollama;

        assert!(remote_key(preset).is_none(), "nothing stored → unset");
        run_remote_key(preset, Some("sk-test-secret".to_string()), false).unwrap();
        assert_eq!(remote_key(preset).as_deref(), Some("sk-test-secret"));

        let cfg: Value = serde_saphyr::from_str(
            &std::fs::read_to_string(home.0.join(".pixel/config.yaml")).unwrap(),
        )
        .unwrap();
        assert_eq!(cfg["remote_keys"]["ollama"], "sk-test-secret");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(home.0.join(".pixel/config.yaml"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "config holds secrets: {mode:o}");
        }

        run_remote_key(preset, None, true).unwrap();
        assert!(remote_key(preset).is_none(), "clear removes the key");

        restore_home(saved);
    }

    #[test]
    fn a_failed_publish_leaves_no_tmp_file_behind() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let home = HomeGuard::set();
        // A directory cannot be read as configuration and must remain untouched.
        let dir = home.0.join("target-is-dir");
        std::fs::create_dir_all(&dir).unwrap();
        let err = write_metrics(&dir, false).expect_err("directory is not configuration");
        assert!(err.contains("cannot read configuration"), "{err}");
        let leftovers: Vec<_> = std::fs::read_dir(&home.0)
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "tmp cleaned up: {leftovers:?}");
    }

    #[test]
    fn ollaya_launch_should_replace_malformed_classify_without_losing_other_settings() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let home = HomeGuard::set();
        let saved = home_env();
        point_home(&home.0);
        let path = home.0.join(".pixel/config.json");
        write(&path, r#"{"metrics":"off","classify":"stale"}"#);
        let launch = json!({"base": "http://127.0.0.1:11435", "argv": ["ollaya", "serve"]});
        set_ollaya_launch(&launch).unwrap();
        assert_eq!(ollaya_launch(), Some(launch));
        let stored: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(stored["metrics"], "off");
        restore_home(saved);
    }

    #[test]
    fn classify_preferences_roundtrip_without_losing_sibling_settings() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let home = HomeGuard::set();
        let saved = home_env();
        point_home(&home.0);

        let path = home.0.join(".pixel/config.json");
        write(&path, r#"{"metrics":"off","classify":"stale"}"#);
        set_classify_engine("local").unwrap();
        assert_eq!(classify_engine().as_deref(), Some("local"));
        write(&path, r#"{"metrics":"off","classify":"stale"}"#);
        assert_eq!(classify_remote_preset(), None);
        set_classify_remote(crate::decide_remote::Preset::Deepseek).unwrap();
        assert_eq!(
            classify_remote_preset(),
            Some(crate::decide_remote::Preset::Deepseek)
        );
        assert_eq!(classify_engine().as_deref(), Some("remote"));
        set_classify_engine("local").unwrap();
        assert_eq!(classify_engine().as_deref(), Some("local"));
        set_classify_remote(crate::decide_remote::Preset::OpencodeGo).unwrap();
        assert_eq!(
            classify_remote_preset(),
            Some(crate::decide_remote::Preset::OpencodeGo)
        );
        set_classify_engine("local").unwrap();

        let launch = json!({
            "base": "http://127.0.0.1:11435",
            "model": "winnow:e4b",
            "argv": ["ollaya", "serve"],
        });
        set_ollaya_launch(&launch).unwrap();
        assert_eq!(ollaya_launch(), Some(launch));
        assert_eq!(classify_engine().as_deref(), Some("local"));
        assert_eq!(
            classify_remote_preset(),
            Some(crate::decide_remote::Preset::OpencodeGo)
        );
        set_classify_remote(crate::decide_remote::Preset::OpencodeGo).unwrap();
        set_classify_engine("local").unwrap();
        assert_eq!(
            classify_remote_preset(),
            Some(crate::decide_remote::Preset::OpencodeGo)
        );

        let stored: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(stored["metrics"], "off");
        assert_eq!(stored["classify"]["engine"], "local");
        assert_eq!(stored["classify"]["ollaya"]["model"], "winnow:e4b");

        restore_home(saved);
    }
}

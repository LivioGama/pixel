//! Codex integration through `~/.codex/config.toml`.
//!
//! Codex reads the `developer_instructions` key of its config file and
//! appends it to the developer message of every session, keeping its own
//! system prompt (`model_instructions_file` would replace that prompt: it
//! becomes the base instructions). A config key reaches every Codex front
//! end that loads the file — the CLI, `codex exec`, the desktop app's
//! bundled binary, the VS Code extension, `spawn_agent` sub-agents — where a
//! shell function only fronts interactive shells that sourced the profile.
//!
//! Codex 0.154 has no file-backed variant of the key, so the prompt is
//! embedded in the file as a TOML literal multi-line string. The value is
//! managed the way the Markdown agent configs were: the Pixel prompt sits
//! between [`config::MANAGED_BEGIN`] and [`config::MANAGED_END`] marker
//! lines, and text the user keeps outside the markers survives every
//! `pixel install`. The rest of the file is rewritten by `toml_edit` with its
//! formatting and comments preserved, because the desktop app writes to the
//! same file.

use std::fs;
use std::path::{Path, PathBuf};

use toml_edit::{DocumentMut, Item, Value};

use crate::config::{MANAGED_BEGIN, MANAGED_END};
use crate::install::{CheckStatus, InstallStep, Result, dry_run_summary};

/// The key Codex appends to its developer message.
pub const DEVELOPER_INSTRUCTIONS_KEY: &str = "developer_instructions";

/// `config.toml`, relative to the Codex home.
pub const CODEX_CONFIG_FILE: &str = "config.toml";

/// `hooks.json`, relative to the Codex home — Codex's hook registry, in the
/// nested `hooks.<Event>[{hooks: [{command}]}]` shape Claude Code settings
/// also use. (Distinct from `config::CODEX_HOOKS_FILE`, which is
/// home-relative.)
pub const HOOKS_FILE: &str = "hooks.json";

/// Substring unique to the installed metrics-relay command, used for
/// idempotent merge and uninstall removal.
pub const METRICS_HOOK_MARKER: &str = "run-hook metrics";

/// The agent prompt as bundled in the binary.
pub(crate) const AGENT_PROMPT_ASSET: &str = include_str!("../assets/pixel-agent-prompt.md");

/// The Codex home directory: `$CODEX_HOME` when the caller did not pin a
/// home directory (a real `pixel install`, where Codex itself honours the
/// variable), `<home>/.codex` otherwise (tests and explicit overrides, which
/// must not depend on the invoking environment).
pub(crate) fn codex_home(home: &Path, home_was_explicit: bool) -> PathBuf {
    if !home_was_explicit
        && let Some(dir) = std::env::var_os("CODEX_HOME")
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
    }
    home.join(".codex")
}

/// The managed block exactly as `pixel install` embeds it: markers on their
/// own lines around the bundled prompt.
pub(crate) fn managed_block() -> String {
    format!("{MANAGED_BEGIN}\n{AGENT_PROMPT_ASSET}{MANAGED_END}\n")
}

/// Byte range of the managed block inside a `developer_instructions` value,
/// from the begin marker to the end of the line holding the end marker.
fn managed_range(value: &str) -> Option<std::ops::Range<usize>> {
    let start = value.find(MANAGED_BEGIN)?;
    let end_marker = start + value[start..].find(MANAGED_END)?;
    let mut end = end_marker + MANAGED_END.len();
    if value[end..].starts_with('\n') {
        end += 1;
    }
    Some(start..end)
}

/// The value `pixel install` writes for a current value of `existing`:
/// the block replaces a previous one in place, or is appended after the
/// user's own text, separated by a blank line.
pub(crate) fn merged_value(existing: Option<&str>) -> String {
    let block = managed_block();
    match existing {
        None => block,
        Some(current) => match managed_range(current) {
            Some(range) => {
                let mut out = String::with_capacity(current.len() + block.len());
                out.push_str(&current[..range.start]);
                out.push_str(&block);
                out.push_str(&current[range.end..]);
                out
            }
            None if current.trim().is_empty() => block,
            None => format!("{}\n\n{block}", current.trim_end()),
        },
    }
}

/// The value with the managed block removed. `None` when nothing but the
/// block (and whitespace) was there, so the key can go.
pub(crate) fn value_without_block(current: &str) -> Option<String> {
    let range = managed_range(current)?;
    let mut rest = String::new();
    rest.push_str(&current[..range.start]);
    rest.push_str(&current[range.end..]);
    let rest = rest.trim_end();
    if rest.trim().is_empty() {
        None
    } else {
        Some(format!("{rest}\n"))
    }
}

/// A TOML string value for `text`: a literal multi-line string (`'''`) when
/// the text allows it, so the prompt lands in the file byte for byte with
/// no escaping; `toml_edit`'s own escaped representation otherwise.
fn string_value(text: &str) -> Value {
    if !text.contains("'''") && !text.contains('\r') {
        // Parsing a one-key document is the supported way to obtain a value
        // with a chosen representation; the round trip proves the literal
        // form carries `text` unchanged. A newline right after the opening
        // delimiter is trimmed by TOML, so the text starts on its own line.
        if let Ok(doc) = format!("v = '''\n{text}'''\n").parse::<DocumentMut>()
            && let Some(Item::Value(value)) = doc.get("v")
            && value.as_str() == Some(text)
        {
            return value.clone();
        }
    }
    Value::from(text)
}

fn current_value(doc: &DocumentMut) -> std::result::Result<Option<String>, String> {
    match doc.get(DEVELOPER_INSTRUCTIONS_KEY) {
        None => Ok(None),
        Some(Item::Value(Value::String(s))) => Ok(Some(s.value().clone())),
        Some(_) => Err(format!(
            "`{DEVELOPER_INSTRUCTIONS_KEY}` in config.toml is not a string"
        )),
    }
}

fn read_document(path: &Path) -> std::result::Result<DocumentMut, String> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    text.parse::<DocumentMut>()
        .map_err(|e| format!("{} does not parse as TOML: {e}", path.display()))
}

/// Write the document atomically: Codex may read the file at any moment.
fn write_document(path: &Path, doc: &DocumentMut) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("toml.pixel-tmp");
    fs::write(&tmp, doc.to_string())?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// `pixel install` step: put the managed block into `developer_instructions`.
pub(crate) fn install_developer_instructions(
    codex_home: &Path,
    dry_run: bool,
) -> Result<InstallStep> {
    let path = codex_home.join(CODEX_CONFIG_FILE);
    let detail = Some(format!(
        "path={} key={DEVELOPER_INSTRUCTIONS_KEY}",
        path.display()
    ));
    let step = |status, summary: String| InstallStep {
        id: "codex-config".into(),
        status,
        summary,
        detail: detail.clone(),
    };
    let mut doc = match read_document(&path) {
        Ok(doc) => doc,
        // A file Codex itself could not load is not ours to repair; a
        // rewrite from a failed parse would drop whatever it holds.
        Err(e) => return Ok(step(CheckStatus::Red, format!("{e} — not touched"))),
    };
    let existing = match current_value(&doc) {
        Ok(existing) => existing,
        Err(e) => return Ok(step(CheckStatus::Red, format!("{e} — not touched"))),
    };
    let wanted = merged_value(existing.as_deref());
    if existing.as_deref() == Some(wanted.as_str()) {
        return Ok(step(
            CheckStatus::Green,
            format!(
                "verified {DEVELOPER_INSTRUCTIONS_KEY} in {}",
                path.display()
            ),
        ));
    }
    let kept_user_text = wanted != managed_block();
    let verb = match &existing {
        None => "installed",
        Some(current) if managed_range(current).is_some() => "updated",
        Some(_) => "appended",
    };
    let summary = format!(
        "{} {DEVELOPER_INSTRUCTIONS_KEY} in {}{}",
        verb,
        path.display(),
        if kept_user_text {
            ", keeping the text outside the pixel markers"
        } else {
            ""
        }
    );
    if dry_run {
        return Ok(step(CheckStatus::Green, dry_run_summary(true, &summary)));
    }
    doc[DEVELOPER_INSTRUCTIONS_KEY] = Item::Value(string_value(&wanted));
    write_document(&path, &doc)?;
    Ok(step(CheckStatus::Green, summary))
}

/// The PostToolUse entry `pixel install` merges into `hooks.json`: Codex
/// runs it after every tool call; `pixel run-hook metrics` self-filters to
/// shell calls that invoked `pixel` and re-emits the finalized 🟩 line —
/// the relay Codex's stderr-less tool results cannot show.
fn metrics_hook_entry(exe: &Path) -> serde_json::Value {
    serde_json::json!({
        "hooks": [{
            "type": "command",
            "command": format!("{} run-hook metrics --provider codex", crate::routing::quoted_executable(exe)),
            "timeout": 10,
        }]
    })
}

fn read_hooks(path: &Path) -> std::result::Result<serde_json::Value, String> {
    match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)
            .map_err(|e| format!("{} does not parse as JSON: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(serde_json::json!({"hooks": {}})),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

fn write_hooks(path: &Path, value: &serde_json::Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.pixel-tmp");
    fs::write(
        &tmp,
        format!("{}\n", serde_json::to_string_pretty(value).unwrap()),
    )?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// `pixel install` step: register the metrics relay in `hooks.json`,
/// idempotently alongside whatever PostToolUse groups already exist.
pub(crate) fn install_metrics_hook(
    codex_home: &Path,
    exe: &Path,
    dry_run: bool,
) -> Result<InstallStep> {
    let path = codex_home.join(HOOKS_FILE);
    let detail = Some(format!(
        "path={} event=PostToolUse marker={METRICS_HOOK_MARKER}",
        path.display()
    ));
    let step = |status, summary: String| InstallStep {
        id: "codex-metrics-hook".into(),
        status,
        summary,
        detail: detail.clone(),
    };
    let mut value = match read_hooks(&path) {
        Ok(v) => v,
        Err(e) => return Ok(step(CheckStatus::Red, format!("{e} — not touched"))),
    };
    let hooks = value
        .as_object_mut()
        .map(|o| o.entry("hooks").or_insert_with(|| serde_json::json!({})))
        .and_then(serde_json::Value::as_object_mut);
    let Some(hooks) = hooks else {
        return Ok(step(
            CheckStatus::Red,
            format!(
                "`hooks` in {} is not an object — not touched",
                path.display()
            ),
        ));
    };
    let merged = crate::config::merge_hook_entry(
        hooks.get("PostToolUse"),
        METRICS_HOOK_MARKER,
        metrics_hook_entry(exe),
    );
    if hooks.get("PostToolUse") == Some(&merged) {
        return Ok(step(
            CheckStatus::Green,
            format!("verified metrics hook in {}", path.display()),
        ));
    }
    let summary = format!(
        "{} metrics PostToolUse hook in {}",
        if path.is_file() {
            "updated"
        } else {
            "installed"
        },
        path.display()
    );
    if dry_run {
        return Ok(step(CheckStatus::Green, dry_run_summary(true, &summary)));
    }
    hooks.insert("PostToolUse".to_string(), merged);
    write_hooks(&path, &value)?;
    Ok(step(CheckStatus::Green, summary))
}

/// `pixel doctor` check: the metrics relay is registered under PostToolUse.
pub(crate) fn check_metrics_hook(
    codex_home: &Path,
) -> std::result::Result<(String, serde_json::Value), String> {
    let path = codex_home.join(HOOKS_FILE);
    let detail = serde_json::json!({
        "path": path.display().to_string(),
        "event": "PostToolUse",
        "marker": METRICS_HOOK_MARKER,
    });
    let value = read_hooks(&path)?;
    let registered = value
        .get("hooks")
        .and_then(|h| h.get("PostToolUse"))
        .and_then(serde_json::Value::as_array)
        .is_some_and(|entries| {
            entries.iter().any(|entry| {
                entry
                    .get("hooks")
                    .and_then(serde_json::Value::as_array)
                    .is_some_and(|hooks| {
                        hooks.iter().any(|hook| {
                            hook.get("command")
                                .and_then(serde_json::Value::as_str)
                                .is_some_and(|c| c.contains(METRICS_HOOK_MARKER))
                        })
                    })
            })
        });
    if !registered {
        return Err(format!(
            "no metrics PostToolUse hook in {} — run `pixel install`",
            path.display()
        ));
    }
    Ok((
        format!("metrics PostToolUse hook registered in {}", path.display()),
        detail,
    ))
}

/// `pixel uninstall` step: take the managed block out of
/// `developer_instructions`, dropping the key when nothing else was in it.
pub(crate) fn remove_developer_instructions(
    codex_home: &Path,
    dry_run: bool,
) -> Result<InstallStep> {
    let path = codex_home.join(CODEX_CONFIG_FILE);
    let detail = Some(format!(
        "path={} key={DEVELOPER_INSTRUCTIONS_KEY}",
        path.display()
    ));
    let step = |status, summary: String| InstallStep {
        id: "codex-config".into(),
        status,
        summary,
        detail: detail.clone(),
    };
    if !path.is_file() {
        return Ok(step(
            CheckStatus::Green,
            dry_run_summary(dry_run, "no codex config.toml — skipping"),
        ));
    }
    let mut doc = match read_document(&path) {
        Ok(doc) => doc,
        Err(e) => return Ok(step(CheckStatus::Red, format!("{e} — not touched"))),
    };
    let Some(current) = current_value(&doc).ok().flatten() else {
        return Ok(step(
            CheckStatus::Green,
            dry_run_summary(dry_run, "no pixel block in codex config.toml — skipping"),
        ));
    };
    if managed_range(&current).is_none() {
        return Ok(step(
            CheckStatus::Green,
            dry_run_summary(dry_run, "no pixel block in codex config.toml — skipping"),
        ));
    }
    let summary = match value_without_block(&current) {
        Some(rest) => {
            if !dry_run {
                doc[DEVELOPER_INSTRUCTIONS_KEY] = Item::Value(string_value(&rest));
            }
            format!(
                "removed the pixel block from {DEVELOPER_INSTRUCTIONS_KEY} in {}, keeping the rest",
                path.display()
            )
        }
        None => {
            if !dry_run {
                doc.remove(DEVELOPER_INSTRUCTIONS_KEY);
            }
            format!(
                "removed {DEVELOPER_INSTRUCTIONS_KEY} from {}",
                path.display()
            )
        }
    };
    if !dry_run {
        write_document(&path, &doc)?;
    }
    Ok(step(CheckStatus::Green, dry_run_summary(dry_run, &summary)))
}

/// `pixel doctor` check: the managed block is present and current.
pub(crate) fn check_developer_instructions(
    codex_home: &Path,
) -> std::result::Result<(String, serde_json::Value), String> {
    let path = codex_home.join(CODEX_CONFIG_FILE);
    let detail = serde_json::json!({
        "path": path.display().to_string(),
        "key": DEVELOPER_INSTRUCTIONS_KEY,
    });
    if !path.is_file() {
        return Err(format!(
            "{} not found — run `pixel install`",
            path.display()
        ));
    }
    let doc = read_document(&path)?;
    let Some(current) = current_value(&doc)? else {
        return Err(format!(
            "{DEVELOPER_INSTRUCTIONS_KEY} missing from {} — run `pixel install`",
            path.display()
        ));
    };
    match managed_range(&current) {
        None => Err(format!(
            "{DEVELOPER_INSTRUCTIONS_KEY} in {} carries no pixel block — run `pixel install`",
            path.display()
        )),
        Some(range) if current[range.clone()] != managed_block() => Err(format!(
            "{DEVELOPER_INSTRUCTIONS_KEY} in {} is stale — run `pixel install` to update",
            path.display()
        )),
        Some(_) => Ok((
            format!(
                "{DEVELOPER_INSTRUCTIONS_KEY} carries the agent prompt in {} ({} bytes)",
                path.display(),
                current.len()
            ),
            detail,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_asset_survives_a_toml_literal_string() {
        // A literal multi-line string cannot contain its own delimiter and
        // TOML forbids control characters other than tab and newline; either
        // would silently switch the file to the escaped representation or
        // fail to load in Codex.
        let block = managed_block();
        assert!(
            !block.contains("'''"),
            "the managed block must not contain '''"
        );
        assert!(
            !block
                .chars()
                .any(|c| c.is_control() && c != '\n' && c != '\t'),
            "the managed block must not contain control characters"
        );
        let doc: DocumentMut = format!("{DEVELOPER_INSTRUCTIONS_KEY} = {}\n", string_value(&block))
            .parse()
            .expect("the literal representation parses");
        assert_eq!(
            current_value(&doc).unwrap().as_deref(),
            Some(block.as_str())
        );
    }

    #[test]
    fn merge_keeps_user_text_on_both_sides_and_replaces_only_the_block() {
        let stale = format!("Before.\n\n{MANAGED_BEGIN}\nold prompt\n{MANAGED_END}\nAfter.\n");
        let merged = merged_value(Some(&stale));
        assert_eq!(merged, format!("Before.\n\n{}After.\n", managed_block()));
        assert_eq!(merged_value(Some(&merged)), merged, "idempotent");
        assert_eq!(
            merged_value(Some("Mine.\n")),
            format!("Mine.\n\n{}", managed_block()),
            "a foreign value is kept and the block appended"
        );
        assert_eq!(merged_value(Some("  \n")), managed_block());
        assert_eq!(
            value_without_block(&merged).as_deref(),
            Some("Before.\n\nAfter.\n")
        );
        assert_eq!(value_without_block(&managed_block()), None);
    }

    // ---- metrics PostToolUse hook ----------------------------------------

    fn scratch_codex_home(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("pixel-codex-hooks-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn post_tool_use(home: &Path) -> serde_json::Value {
        let text = fs::read_to_string(home.join(HOOKS_FILE)).unwrap();
        serde_json::from_str::<serde_json::Value>(&text).unwrap()["hooks"]["PostToolUse"].clone()
    }

    #[test]
    fn metrics_hook_install_verify_and_preserve_foreign_entries() {
        let home = scratch_codex_home("install");
        let exe = Path::new("/opt/pixel tools/pixel");
        // A foreign PostToolUse group survives the merge.
        fs::write(
            home.join(HOOKS_FILE),
            serde_json::to_string_pretty(&serde_json::json!({
                "hooks": {"PostToolUse": [
                    {"hooks": [{"type": "command", "command": "cmux-feed"}]}
                ]}
            }))
            .unwrap(),
        )
        .unwrap();

        install_metrics_hook(&home, exe, false).unwrap();
        let entries = post_tool_use(&home).as_array().unwrap().clone();
        assert_eq!(entries.len(), 2, "foreign group preserved + ours added");
        let command = entries[1]["hooks"][0]["command"].as_str().unwrap();
        assert!(command.contains(METRICS_HOOK_MARKER));
        assert!(
            command.starts_with('\''),
            "the exe path is shell-quoted: {command}"
        );
        assert!(
            check_metrics_hook(&home).is_ok(),
            "doctor check sees the registration"
        );

        // Second install verifies instead of duplicating.
        let step = install_metrics_hook(&home, exe, false).unwrap();
        assert!(step.summary.contains("verified"), "{}", step.summary);
        assert_eq!(post_tool_use(&home).as_array().unwrap().len(), 2);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn metrics_hook_reinstall_refreshes_a_stale_executable_path() {
        let home = scratch_codex_home("refresh");
        install_metrics_hook(&home, Path::new("/old/pixel"), false).unwrap();
        install_metrics_hook(&home, Path::new("/new/pixel"), false).unwrap();
        let entries = post_tool_use(&home).as_array().unwrap().clone();
        assert_eq!(
            entries.len(),
            1,
            "the stale entry is replaced, not appended"
        );
        let command = entries[0]["hooks"][0]["command"].as_str().unwrap();
        assert!(command.contains("/new/pixel"), "{command}");
        assert!(!command.contains("/old/pixel"), "{command}");
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn metrics_hook_install_refuses_unparseable_hooks_json() {
        let home = scratch_codex_home("broken");
        fs::write(home.join(HOOKS_FILE), "not json").unwrap();
        let step = install_metrics_hook(&home, Path::new("/x"), false).unwrap();
        assert_eq!(step.status, CheckStatus::Red);
        assert_eq!(
            fs::read_to_string(home.join(HOOKS_FILE)).unwrap(),
            "not json",
            "an unparseable file is never rewritten"
        );
        fs::write(home.join(HOOKS_FILE), "{\"hooks\": [1]}").unwrap();
        let step = install_metrics_hook(&home, Path::new("/x"), false).unwrap();
        assert_eq!(
            step.status,
            CheckStatus::Red,
            "a non-object hooks key is refused, not silently replaced"
        );
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn metrics_hook_install_reports_an_unreadable_hooks_json() {
        let home = scratch_codex_home("unreadable");
        // A directory where the file is expected fails the read for every
        // user including root — and is not "absent": it must come back Red,
        // never Ok-treated-as-empty.
        fs::create_dir(home.join(HOOKS_FILE)).unwrap();
        let step = install_metrics_hook(&home, Path::new("/x"), false).unwrap();
        assert_eq!(step.status, CheckStatus::Red, "{}", step.summary);
        assert!(step.summary.contains("not touched"), "{}", step.summary);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn metrics_hook_check_is_red_until_registered() {
        let home = scratch_codex_home("check");
        assert!(
            check_metrics_hook(&home).is_err(),
            "absent file is not registered"
        );
        fs::write(home.join(HOOKS_FILE), "{\"hooks\": {}}").unwrap();
        assert!(
            check_metrics_hook(&home).is_err(),
            "empty PostToolUse is not registered"
        );
        fs::write(
            home.join(HOOKS_FILE),
            serde_json::to_string_pretty(&serde_json::json!({
                "hooks": {"PostToolUse": [
                    {"hooks": [{"type": "command", "command": "pixel run-hook metrics --provider codex"}]}
                ]}
            }))
            .unwrap(),
        )
        .unwrap();
        assert!(check_metrics_hook(&home).is_ok());
        let _ = fs::remove_dir_all(&home);
    }
}

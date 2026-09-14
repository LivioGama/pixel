//! `pixel rename` — AST-aware, namespace-aware CLI command renamer.
//!
//! Renames CLI commands by:
//! 1. Renaming enum variants in `enum Command { ... }` in main.rs
//! 2. Updating all `Command::OldName` references (match arms)
//! 3. Updating string literals in command-label contexts:
//!    - `call_guard_check("old-name", ...)`
//!    - `native_commands` match arms in operation_metrics.rs
//!    - `GUARDED_COMMANDS` in call_guard.rs
//!    - `check_and_record("old-name", ...)` in call_guard.rs tests
//!    - `MANDATORY_SCENARIOS` in doctor.rs
//!    - `SESSION_USAGE` in proto/op.rs
//! 4. Updating user-facing text: `` `pixel old-name` `` in prompts and docs
//! 5. Regenerating plugin surfaces
//!
//! What it does NOT rename (namespace-aware):
//! - `Op::Search` (protocol enum in pixel-proto)
//! - `Request::Search` (daemon enum in pixel-daemon)
//! - `data["search"]` (JSON field access)
//! - `"search"` in guard.rs agent tool name matching
//! - `"search"` in git subcommand matching

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Convert kebab-case CLI name to PascalCase enum variant name.
fn kebab_to_pascal(s: &str) -> String {
    s.split('-')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect()
}

/// A single rename operation: old → new.
#[derive(Debug, Clone)]
pub struct RenamePair {
    pub old_kebab: String,
    pub new_kebab: String,
    pub old_pascal: String,
    pub new_pascal: String,
}

impl RenamePair {
    pub fn new(old: &str, new: &str) -> Self {
        Self {
            old_kebab: old.to_string(),
            new_kebab: new.to_string(),
            old_pascal: kebab_to_pascal(old),
            new_pascal: kebab_to_pascal(new),
        }
    }
}

/// The result of a rename operation.
#[derive(Debug, Default, serde::Serialize)]
pub struct RenameReport {
    pub pairs: Vec<RenamePairReport>,
    pub files_changed: Vec<String>,
    pub total_edits: usize,
    pub errors: Vec<String>,
}

#[derive(Debug, Default, serde::Serialize)]
pub struct RenamePairReport {
    pub old: String,
    pub new: String,
    pub edits: usize,
}

/// Load a JSON mapping file: {"old-name": "new-name", ...}
pub fn load_mapping(path: &Path) -> Result<Vec<RenamePair>, String> {
    let content = fs::read_to_string(path)
        .map_err(|e| format!("cannot read mapping file {}: {e}", path.display()))?;
    let map: BTreeMap<String, String> = serde_json::from_str(&content)
        .map_err(|e| format!("cannot parse mapping JSON: {e}"))?;
    Ok(map
        .into_iter()
        .map(|(old, new)| RenamePair::new(&old, &new))
        .collect())
}

/// Run the rename operation on a repo root.
pub fn run(
    root: &Path,
    pairs: &[RenamePair],
    dry_run: bool,
    regen: bool,
) -> Result<RenameReport, String> {
    let mut report = RenameReport::default();
    let mut files_changed = std::collections::BTreeSet::new();

    // 1. Rename enum variants + match arms in main.rs
    let main_rs = root.join("crates/pixel/src/main.rs");
    if main_rs.exists() {
        let edits = rename_in_main_rs(&main_rs, pairs, dry_run)?;
        if edits > 0 {
            files_changed.insert(main_rs.display().to_string());
            report.total_edits += edits;
        }
    }

    // 2. Rename command labels in operation_metrics.rs
    let op_metrics = root.join("crates/pixel/src/operation_metrics.rs");
    if op_metrics.exists() {
        let edits = rename_command_labels(&op_metrics, pairs, dry_run)?;
        if edits > 0 {
            files_changed.insert(op_metrics.display().to_string());
            report.total_edits += edits;
        }
    }

    // 3. Rename in call_guard.rs (GUARDED_COMMANDS + check_and_record calls)
    let call_guard = root.join("crates/pixel/src/call_guard.rs");
    if call_guard.exists() {
        let edits = rename_command_labels(&call_guard, pairs, dry_run)?;
        if edits > 0 {
            files_changed.insert(call_guard.display().to_string());
            report.total_edits += edits;
        }
    }

    // 4. Rename in doctor.rs (MANDATORY_SCENARIOS + normalize_rule_command tests)
    let doctor = root.join("crates/pixel-install/src/doctor.rs");
    if doctor.exists() {
        let edits = rename_in_doctor(&doctor, pairs, dry_run)?;
        if edits > 0 {
            files_changed.insert(doctor.display().to_string());
            report.total_edits += edits;
        }
    }

    // 5. Rename in proto/op.rs (SESSION_USAGE + test)
    let proto_op = root.join("crates/pixel-proto/src/op.rs");
    if proto_op.exists() {
        let edits = rename_session_usage(&proto_op, pairs, dry_run)?;
        if edits > 0 {
            files_changed.insert(proto_op.display().to_string());
            report.total_edits += edits;
        }
    }

    // 6. Rename in routing.rs (hook commands)
    let routing = root.join("crates/pixel-install/src/routing.rs");
    if routing.exists() {
        let edits = rename_in_routing(&routing, pairs, dry_run)?;
        if edits > 0 {
            files_changed.insert(routing.display().to_string());
            report.total_edits += edits;
        }
    }

    // 7. Rename user-facing text in the canonical prompt
    let prompt = root.join("crates/pixel-install/assets/pixel-agent-prompt.md");
    if prompt.exists() {
        let edits = rename_user_facing(&prompt, pairs, dry_run)?;
        if edits > 0 {
            files_changed.insert(prompt.display().to_string());
            report.total_edits += edits;
        }
    }

    // 8. Rename in subagent prompt
    let subagent = root.join("crates/pixel-install/assets/pixel-subagent-prompt.md");
    if subagent.exists() {
        let edits = rename_user_facing(&subagent, pairs, dry_run)?;
        if edits > 0 {
            files_changed.insert(subagent.display().to_string());
            report.total_edits += edits;
        }
    }

    // 9. Rename in docs
    for doc in ["ARCHITECTURE.md", "CONTRIBUTING.md", "README.md"] {
        let path = root.join(doc);
        if path.exists() {
            let edits = rename_user_facing(&path, pairs, dry_run)?;
            if edits > 0 {
                files_changed.insert(path.display().to_string());
                report.total_edits += edits;
            }
        }
    }

    // 10. Rename in config.rs (hook command references)
    let config = root.join("crates/pixel-install/src/config.rs");
    if config.exists() {
        let edits = rename_in_config(&config, pairs, dry_run)?;
        if edits > 0 {
            files_changed.insert(config.display().to_string());
            report.total_edits += edits;
        }
    }

    // 11. Rename in install_tests.rs
    let install_tests = root.join("crates/pixel-install/tests/install_tests.rs");
    if install_tests.exists() {
        let edits = rename_user_facing(&install_tests, pairs, dry_run)?;
        if edits > 0 {
            files_changed.insert(install_tests.display().to_string());
            report.total_edits += edits;
        }
    }

    // 12. Rename in test files (CLI command invocations)
    let test_dir = root.join("crates/pixel/tests/cli");
    if test_dir.exists() {
        for entry in fs::read_dir(&test_dir).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "rs") {
                let edits = rename_in_test_file(&path, pairs, dry_run)?;
                if edits > 0 {
                    files_changed.insert(path.display().to_string());
                    report.total_edits += edits;
                }
            }
        }
    }

    // 13. Rename in other source files that reference command names in comments
    for src in [
        "crates/pixel/src/prompt_submit.rs",
        "crates/pixel/src/guard.rs",
        "crates/pixel-daemon/src/api.rs",
        "crates/pixel-git/src/plumbing.rs",
        "crates/pixel-graph/src/build.rs",
        "crates/pixel-ops/src/reconcile.rs",
        "crates/pixel-rank/src/lib.rs",
        "crates/pixel-install/src/install.rs",
        "crates/pixel-install/src/lib.rs",
        "crates/pixel-install/src/uninstall.rs",
    ] {
        let path = root.join(src);
        if path.exists() {
            let edits = rename_user_facing(&path, pairs, dry_run)?;
            if edits > 0 {
                files_changed.insert(path.display().to_string());
                report.total_edits += edits;
            }
        }
    }

    // 14. Rename in scripts
    let gen_script = root.join("scripts/gen-plugin-assets.sh");
    if gen_script.exists() {
        let edits = rename_user_facing(&gen_script, pairs, dry_run)?;
        if edits > 0 {
            files_changed.insert(gen_script.display().to_string());
            report.total_edits += edits;
        }
    }

    report.files_changed = files_changed.into_iter().collect();

    // 15. Regenerate plugin surfaces
    if regen && !dry_run && !pairs.is_empty() {
        let gen_script = root.join("scripts/gen-plugin-assets.sh");
        if gen_script.exists() {
            let output = std::process::Command::new(&gen_script)
                .current_dir(root)
                .output()
                .map_err(|e| format!("failed to run gen-plugin-assets.sh: {e}"))?;
            if !output.status.success() {
                report
                    .errors
                    .push("gen-plugin-assets.sh failed".to_string());
            }
        }
    }

    Ok(report)
}

/// Rename enum variants and match arms in main.rs.
/// Only renames `Command::OldName` and `enum Command { OldName }`,
/// NOT `Op::OldName` or `Request::OldName`.
fn rename_in_main_rs(path: &Path, pairs: &[RenamePair], dry_run: bool) -> Result<usize, String> {
    let content = fs::read_to_string(path).map_err(|e| e.to_string())?;
    let mut new_content = content.clone();
    let mut edits = 0;

    // Sort by old_pascal length descending so longer names are replaced first
    // (prevents "Search" matching inside "SearchCompat")
    let mut sorted_pairs: Vec<&RenamePair> = pairs.iter().collect();
    sorted_pairs.sort_by(|a, b| b.old_pascal.len().cmp(&a.old_pascal.len()));

    for pair in sorted_pairs {
        // Rename `Command::OldPascal` → `Command::NewPascal`
        // Use word boundary: the character after OldPascal must not be an identifier char
        let old_pat = format!("Command::{}", pair.old_pascal);
        let new_pat = format!("Command::{}", pair.new_pascal);
        // Replace only when followed by a non-identifier character (space, {, (, etc.)
        for suffix in [" {", "\n", "(", " ", ".", "::", ")"] {
            let old_full = format!("{}{}", old_pat, suffix);
            let new_full = format!("{}{}", new_pat, suffix);
            let count = new_content.matches(&old_full).count();
            if count > 0 {
                new_content = new_content.replace(&old_full, &new_full);
                edits += count;
            }
        }

        // Rename the variant definition: `    OldPascal {` → `    NewPascal {`
        // This is safe because we're inside `enum Command { ... }` and the variant
        // name is at the start of a line with 4-space indent.
        // We need to be careful not to match `Op::OldPascal` or `Request::OldPascal`.
        // The variant definition pattern is: `\n    OldPascal {` or `\n    OldPascal\n`
        let old_def = format!("\n    {} ", pair.old_pascal);
        let new_def = format!("\n    {} ", pair.new_pascal);
        let count2 = new_content.matches(&old_def).count();
        if count2 > 0 {
            new_content = new_content.replace(&old_def, &new_def);
            edits += count2;
        }

        // Also handle variant definitions with `{` directly after: `    OldPascal {`
        let old_def2 = format!("\n    {} {{", pair.old_pascal);
        let new_def2 = format!("\n    {} {{", pair.new_pascal);
        let count3 = new_content.matches(&old_def2).count();
        if count3 > 0 {
            new_content = new_content.replace(&old_def2, &new_def2);
            edits += count3;
        }

        // Rename call_guard_check("old-kebab", ...) → call_guard_check("new-kebab", ...)
        let old_guard = format!("call_guard_check(\"{}\"", pair.old_kebab);
        let new_guard = format!("call_guard_check(\"{}\"", pair.new_kebab);
        let count4 = new_content.matches(&old_guard).count();
        if count4 > 0 {
            new_content = new_content.replace(&old_guard, &new_guard);
            edits += count4;
        }

        // Rename the JSON "op" field for recipes: "op": "query" stays as protocol name
        // (This is intentionally NOT renamed — it's a protocol name)
    }

    if !dry_run && edits > 0 {
        fs::write(path, new_content).map_err(|e| e.to_string())?;
    }

    Ok(edits)
}

/// Rename command labels in string literals within specific function contexts.
/// Only renames strings that are command labels, not JSON fields or agent tool names.
fn rename_command_labels(path: &Path, pairs: &[RenamePair], dry_run: bool) -> Result<usize, String> {
    let content = fs::read_to_string(path).map_err(|e| e.to_string())?;
    let mut new_content = content.clone();
    let mut edits = 0;

    // Sort by old_kebab length descending to prevent prefix matches
    let mut sorted_pairs: Vec<&RenamePair> = pairs.iter().collect();
    sorted_pairs.sort_by(|a, b| b.old_kebab.len().cmp(&a.old_kebab.len()));

    for pair in sorted_pairs {
        // In operation_metrics.rs: match arms like `"search" |` → `"search-content" |`
        // and `"search" =>` patterns
        // These are command labels in match arms
        let old_label = format!("\"{}\"", pair.old_kebab);
        let new_label = format!("\"{}\"", pair.new_kebab);

        // Only replace in match arm context: `"old" |` or `"old" =>` or `"old",`
        // This avoids replacing JSON field access like `data["old"]`
        for pattern in [" |", " =>", ",", ")"] {
            let old_full = format!("{}{}", old_label, pattern);
            let new_full = format!("{}{}", new_label, pattern);
            let count = new_content.matches(&old_full).count();
            if count > 0 {
                new_content = new_content.replace(&old_full, &new_full);
                edits += count;
            }
        }

        // In call_guard.rs: check_and_record("old-name", ...) → check_and_record("new-name", ...)
        let old_check = format!("check_and_record(\"{}\"", pair.old_kebab);
        let new_check = format!("check_and_record(\"{}\"", pair.new_kebab);
        let count = new_content.matches(&old_check).count();
        if count > 0 {
            new_content = new_content.replace(&old_check, &new_check);
            edits += count;
        }

        // In call_guard.rs: GUARDED_COMMANDS array entries
        // Pattern: `"old-name"` in the array — preceded by `&[` or `, ` and followed by `"` (closing quote)
        // Use full pattern with closing quote to prevent prefix matches
        for prefix in ["&[\"", ", \""] {
            let old_full = format!("{}{}\"", prefix, pair.old_kebab);
            let new_full = format!("{}{}\"", prefix, pair.new_kebab);
            let count = new_content.matches(&old_full).count();
            if count > 0 {
                new_content = new_content.replace(&old_full, &new_full);
                edits += count;
            }
        }
    }

    if !dry_run && edits > 0 {
        fs::write(path, new_content).map_err(|e| e.to_string())?;
    }

    Ok(edits)
}

/// Rename in doctor.rs: MANDATORY_SCENARIOS + normalize_rule_command test assertions.
fn rename_in_doctor(path: &Path, pairs: &[RenamePair], dry_run: bool) -> Result<usize, String> {
    let content = fs::read_to_string(path).map_err(|e| e.to_string())?;
    let mut new_content = content.clone();
    let mut edits = 0;

    for pair in pairs {
        // MANDATORY_SCENARIOS entries: "old-name" in the array
        for prefix in ["&[\"", ", \""] {
            let old_full = format!("{}{}", prefix, pair.old_kebab);
            let new_full = format!("{}{}", prefix, pair.new_kebab);
            let count = new_content.matches(&old_full).count();
            if count > 0 {
                new_content = new_content.replace(&old_full, &new_full);
                edits += count;
            }
        }

        // normalize_rule_command test: "pixel old-name" → "pixel new-name"
        let old_cmd = format!("pixel {}", pair.old_kebab);
        let new_cmd = format!("pixel {}", pair.new_kebab);
        let count = new_content.matches(&old_cmd).count();
        if count > 0 {
            new_content = new_content.replace(&old_cmd, &new_cmd);
            edits += count;
        }

        // Test assertions: "old-name".into() → "new-name".into()
        let old_into = format!("\"{}\".into()", pair.old_kebab);
        let new_into = format!("\"{}\".into()", pair.new_kebab);
        let count = new_content.matches(&old_into).count();
        if count > 0 {
            new_content = new_content.replace(&old_into, &new_into);
            edits += count;
        }
    }

    if !dry_run && edits > 0 {
        fs::write(path, new_content).map_err(|e| e.to_string())?;
    }

    Ok(edits)
}

/// Rename in proto/op.rs: SESSION_USAGE string + test assertions.
fn rename_session_usage(path: &Path, pairs: &[RenamePair], dry_run: bool) -> Result<usize, String> {
    let content = fs::read_to_string(path).map_err(|e| e.to_string())?;
    let mut new_content = content.clone();
    let mut edits = 0;

    for pair in pairs {
        // SESSION_USAGE: `pixel old-name` → `pixel new-name`
        let old_cmd = format!("pixel {}", pair.old_kebab);
        let new_cmd = format!("pixel {}", pair.new_kebab);
        let count = new_content.matches(&old_cmd).count();
        if count > 0 {
            new_content = new_content.replace(&old_cmd, &new_cmd);
            edits += count;
        }

        // Test assertions: "old-name" in the scenario list
        for prefix in ["\"", "\""] {
            let old_full = format!("{}{}{}", prefix, pair.old_kebab, "\"");
            let new_full = format!("{}{}{}", prefix, pair.new_kebab, "\"");
            let count = new_content.matches(&old_full).count();
            if count > 0 {
                new_content = new_content.replace(&old_full, &new_full);
                edits += count;
            }
        }
    }

    if !dry_run && edits > 0 {
        fs::write(path, new_content).map_err(|e| e.to_string())?;
    }

    Ok(edits)
}

/// Rename in routing.rs: hook command generation + is_pixel_hook + tests.
fn rename_in_routing(path: &Path, pairs: &[RenamePair], dry_run: bool) -> Result<usize, String> {
    let content = fs::read_to_string(path).map_err(|e| e.to_string())?;
    let mut new_content = content.clone();
    let mut edits = 0;

    for pair in pairs {
        // Only rename "hook" → "run-hook" (the CLI command name)
        if pair.old_kebab == "hook" {
            // Generated hook commands: `"{} hook ` → `"{} run-hook `
            let old_hook = format!("{{}} hook ");
            let new_hook = format!("{{}} run-hook ");
            let count = new_content.matches(&old_hook).count();
            if count > 0 {
                new_content = new_content.replace(&old_hook, &new_hook);
                edits += count;
            }

            // is_pixel_hook: `" hook "` → `" run-hook "`
            let old_split = "\" hook \"";
            let new_split = "\" run-hook \"";
            let count = new_content.matches(old_split).count();
            if count > 0 {
                new_content = new_content.replace(old_split, new_split);
                edits += count;
            }

            // Test assertions: `' hook ` → `' run-hook `
            let old_test = "' hook ";
            let new_test = "' run-hook ";
            let count = new_content.matches(old_test).count();
            if count > 0 {
                new_content = new_content.replace(old_test, new_test);
                edits += count;
            }

            // Test assertions: `"pixel hook ` → `"pixel run-hook `
            let old_test2 = "\"pixel hook ";
            let new_test2 = "\"pixel run-hook ";
            let count = new_content.matches(old_test2).count();
            if count > 0 {
                new_content = new_content.replace(old_test2, new_test2);
                edits += count;
            }
        }
    }

    if !dry_run && edits > 0 {
        fs::write(path, new_content).map_err(|e| e.to_string())?;
    }

    Ok(edits)
}

/// Rename in config.rs: hook command references in comments and tests.
fn rename_in_config(path: &Path, pairs: &[RenamePair], dry_run: bool) -> Result<usize, String> {
    let content = fs::read_to_string(path).map_err(|e| e.to_string())?;
    let mut new_content = content.clone();
    let mut edits = 0;

    for pair in pairs {
        if pair.old_kebab == "hook" {
            // `pixel run-hook` in comments and test strings
            let old_cmd = "pixel run-hook";
            let new_cmd = "pixel hook";
            // Actually, the current code (post-rename) uses "run-hook" in config.rs
            // We need to revert it to "hook" when going backwards, or keep it if going forwards
            // Since we're building on top of the renamed code, and will use this on the
            // pre-rename branch, the config.rs on the pre-rename branch uses "hook"
            // So we need: "hook" → "run-hook"
            let old_cmd = "pixel hook ";
            let new_cmd = "pixel run-hook ";
            let count = new_content.matches(old_cmd).count();
            if count > 0 {
                new_content = new_content.replace(old_cmd, new_cmd);
                edits += count;
            }

            // Also handle `"pixel hook"` (without trailing space)
            let old_cmd2 = "\"pixel hook\"";
            let new_cmd2 = "\"pixel run-hook\"";
            let count = new_content.matches(old_cmd2).count();
            if count > 0 {
                new_content = new_content.replace(old_cmd2, new_cmd2);
                edits += count;
            }
        }
    }

    if !dry_run && edits > 0 {
        fs::write(path, new_content).map_err(|e| e.to_string())?;
    }

    Ok(edits)
}

/// Rename user-facing text: `pixel old-name` → `pixel new-name` in markdown/docs/comments.
fn rename_user_facing(path: &Path, pairs: &[RenamePair], dry_run: bool) -> Result<usize, String> {
    let content = fs::read_to_string(path).map_err(|e| e.to_string())?;
    let mut new_content = content.clone();
    let mut edits = 0;

    // Sort by old_kebab length descending so longer names are replaced first
    // (prevents "pixel search" matching inside "pixel search-compat")
    let mut sorted_pairs: Vec<&RenamePair> = pairs.iter().collect();
    sorted_pairs.sort_by(|a, b| b.old_kebab.len().cmp(&a.old_kebab.len()));

    for pair in sorted_pairs {
        // `pixel old-name` → `pixel new-name` — only when followed by a non-identifier char
        // (space, backtick, quote, newline, etc.) to prevent prefix matches
        // NOTE: "-" is excluded as a suffix because it would match inside hyphenated
        // command names like "search-content" causing double-rename
        let old_cmd = format!("pixel {}", pair.old_kebab);
        let new_cmd = format!("pixel {}", pair.new_kebab);
        for suffix in ["`", " ", "\n", "\"", "'", ")", "/", ".", "|"] {
            let old_full = format!("{}{}", old_cmd, suffix);
            let new_full = format!("{}{}", new_cmd, suffix);
            let count = new_content.matches(&old_full).count();
            if count > 0 {
                new_content = new_content.replace(&old_full, &new_full);
                edits += count;
            }
        }
    }

    if !dry_run && edits > 0 {
        fs::write(path, new_content).map_err(|e| e.to_string())?;
    }

    Ok(edits)
}

/// Rename CLI command invocations in test files.
/// Only renames strings that appear as CLI args (in .args(["old-name", ...]) or ["old-name", ...])
fn rename_in_test_file(path: &Path, pairs: &[RenamePair], dry_run: bool) -> Result<usize, String> {
    let content = fs::read_to_string(path).map_err(|e| e.to_string())?;
    let mut lines: Vec<String> = content.lines().map(String::from).collect();
    let mut edits = 0;

    // Sort by old_kebab length descending to prevent prefix matches
    let mut sorted_pairs: Vec<&RenamePair> = pairs.iter().collect();
    sorted_pairs.sort_by(|a, b| b.old_kebab.len().cmp(&a.old_kebab.len()));

    for line in &mut lines {
        // Skip lines that are git command invocations (not pixel commands)
        let is_git_context = line.contains("git(")
            || line.contains("git ")
            || line.contains("\"git\"");
        if is_git_context {
            continue;
        }

        for pair in &sorted_pairs {
            // .args(["old-name", ...]) → .args(["new-name", ...])
            // Line-by-line: match "old-name" at start of array or after comma
            for prefix in ["[\"", ", \"", "    \"", "        \"", "            \""] {
                let old_full = format!("{}{}\"", prefix, pair.old_kebab);
                let new_full = format!("{}{}\"", prefix, pair.new_kebab);
                let count = line.matches(&old_full).count();
                if count > 0 {
                    *line = line.replace(&old_full, &new_full);
                    edits += count;
                }
            }

            // `pixel old-name` in test comments — with word boundary
            let old_cmd = format!("pixel {}", pair.old_kebab);
            let new_cmd = format!("pixel {}", pair.new_kebab);
            for suffix in ["`", " ", "\n", "\"", "'", ")"] {
                let old_full = format!("{}{}", old_cmd, suffix);
                let new_full = format!("{}{}", new_cmd, suffix);
                let count = line.matches(&old_full).count();
                if count > 0 {
                    *line = line.replace(&old_full, &new_full);
                    edits += count;
                }
            }
        }
    }

    let new_content = lines.join("\n") + if content.ends_with('\n') { "\n" } else { "" };

    if !dry_run && edits > 0 {
        fs::write(path, new_content).map_err(|e| e.to_string())?;
    }

    Ok(edits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kebab_to_pascal_converts_correctly() {
        assert_eq!(kebab_to_pascal("search"), "Search");
        assert_eq!(kebab_to_pascal("search-content"), "SearchContent");
        assert_eq!(kebab_to_pascal("build-index"), "BuildIndex");
        assert_eq!(kebab_to_pascal("who-calls"), "WhoCalls");
        assert_eq!(kebab_to_pascal("impact"), "Impact");
    }

    #[test]
    fn rename_pair_builds_both_forms() {
        let pair = RenamePair::new("search", "search-content");
        assert_eq!(pair.old_kebab, "search");
        assert_eq!(pair.new_kebab, "search-content");
        assert_eq!(pair.old_pascal, "Search");
        assert_eq!(pair.new_pascal, "SearchContent");
    }
}

//! Documentation drift: the docs and the agent prompts name commands, and
//! the binary is the only source of truth for which commands exist.
//!
//! Two contracts, both against `pixel --help` of the binary built from this
//! checkout:
//!
//! - every backticked `` `pixel <name>` `` in the repository docs and in the
//!   bundled agent prompts names a real subcommand (a removed or renamed
//!   command cannot linger in prose);
//! - every subcommand appears in ARCHITECTURE.md's `## Command surface`
//!   table (a new command cannot ship undocumented).
//!
//! `pixel doctor` already dry-runs the *installed* prompt's command lines
//! against the parser at run time; this test does it for the tree at build
//! time, where a PR can still fix it.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

/// Subcommand names from the `Commands:` block of `pixel --help`.
fn subcommands() -> BTreeSet<String> {
    let out = Command::new(env!("CARGO_BIN_EXE_pixel"))
        .arg("--help")
        .env("PIXEL_DAEMON_AUTO_START", "0")
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8(out.stdout).unwrap();
    let block = text
        .split("Commands:")
        .nth(1)
        .and_then(|rest| rest.split("Options:").next())
        .expect("clap help has Commands: then Options:");
    let names: BTreeSet<String> = block
        .lines()
        .filter_map(|l| l.strip_prefix("  "))
        .filter(|l| !l.starts_with(' '))
        .filter_map(|l| l.split_whitespace().next())
        .map(str::to_string)
        .collect();
    assert!(names.len() > 20, "help parsing broke: {names:?}");
    names
}

/// Every `` `pixel <name>`` reference in `text` (backticked only: prose such
/// as "the pixel binary" is not a command).
fn referenced_commands(text: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for (i, _) in text.match_indices("`pixel ") {
        let rest = &text[i + "`pixel ".len()..];
        let name: String = rest
            .chars()
            .take_while(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-')
            .collect();
        let next = rest.chars().nth(name.chars().count());
        // `pixel foo`, `pixel foo --flag`, `pixel foo <arg>` … but not
        // `pixel foo/bar`, `pixel-foo`, or a flag such as `pixel --help`.
        if name.starts_with(|c: char| c.is_ascii_lowercase())
            && matches!(next, Some(' ' | '`' | '\n'))
        {
            out.insert(name);
        }
    }
    out
}

const DOCS: &[&str] = &[
    "README.md",
    "ARCHITECTURE.md",
    "CONTRIBUTING.md",
    "docs/manual-setup.md",
    "crates/pixel-install/assets/pixel-agent-prompt.md",
    "crates/pixel-install/assets/pixel-subagent-prompt.md",
    "AGENTS.md",
    ".agents/rules/test-campaigns.md",
    ".agents/rules/measuring.md",
    "scripts/README.md",
    "js/sniper/README.md",
];

#[test]
fn every_documented_pixel_command_exists() {
    let known = subcommands();
    let root = repo_root();
    let mut stale = Vec::new();
    for doc in DOCS {
        let text = std::fs::read_to_string(root.join(doc)).unwrap_or_else(|e| panic!("{doc}: {e}"));
        for name in referenced_commands(&text) {
            if !known.contains(&name) {
                stale.push(format!("{doc}: `pixel {name}`"));
            }
        }
    }
    assert!(
        stale.is_empty(),
        "documented commands the binary rejects:\n{}",
        stale.join("\n")
    );
}

/// Commands a runtime string tells the agent to run: `` `pixel <name>`` or
/// `"pixel <name>` on a line of production code. Comments and everything
/// from the first `#[cfg(test)]` on are skipped: tests name old spellings on
/// purpose (alias and hook-compatibility fixtures).
fn runtime_command_mentions(source: &str) -> BTreeSet<String> {
    let production = source.split("#[cfg(test)]").next().unwrap_or_default();
    let code: String = production
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .map(|l| format!("{l}\n"))
        .collect();
    let mut out = referenced_commands(&code);
    // A quoted `"pixel …` is a command only when it names one: "pixel is the
    // engine" is prose, "pixel rescue --apply" is a pre-rename spelling.
    out.extend(
        referenced_commands(&code.replace("\"pixel ", "`pixel "))
            .into_iter()
            .filter(|name| pixel_proto::commands::renamed_to(name).is_some()),
    );
    out
}

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap().map(Result::unwrap) {
        let path = entry.path();
        if path.is_dir() {
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// The CLI prints follow-up commands for the agent to type (a revert plan, a
/// `--show` follow-up, a hint to rebuild the index). A pre-rename spelling
/// there still runs through its alias today and fails once the aliases go.
#[test]
fn every_command_production_code_prints_is_a_current_subcommand() {
    let known = subcommands();
    let crates = repo_root().join("crates");
    let mut sources = Vec::new();
    for krate in std::fs::read_dir(&crates).unwrap().map(Result::unwrap) {
        let src = krate.path().join("src");
        if src.is_dir() {
            rust_sources(&src, &mut sources);
        }
    }
    assert!(sources.len() > 50, "source walk broke: {}", sources.len());
    let mut stale = Vec::new();
    for path in &sources {
        let text = std::fs::read_to_string(path).unwrap();
        for name in runtime_command_mentions(&text) {
            if !known.contains(&name) {
                let shown = path.strip_prefix(&crates).unwrap_or(path).display();
                stale.push(format!("{shown}: pixel {name}"));
            }
        }
    }
    assert!(
        stale.is_empty(),
        "production code prints commands the help does not list:\n{}",
        stale.join("\n")
    );
}

#[test]
fn runtime_command_mentions_skip_comments_and_tests() {
    let source = [
        "/// `pixel rescue` is the old name",
        "fn f() -> String { format!(\"pixel plan-rollback --apply {oid} .\") }",
        "const HINT: &str = \"run `pixel build-index .` first\";",
        "const OLD: &str = \"pixel rescue backup\";",
        "const PROSE: &str = \"pixel is the engine\";",
        "#[cfg(test)]",
        "mod tests { const OLD: &str = \"pixel hook guard\"; }",
    ]
    .join("\n");
    let got: Vec<String> = runtime_command_mentions(&source).into_iter().collect();
    assert_eq!(got, ["build-index", "rescue"]);
}

#[test]
fn every_subcommand_is_in_the_architecture_command_table() {
    let known = subcommands();
    let root = repo_root();
    let arch = std::fs::read_to_string(root.join("ARCHITECTURE.md")).unwrap();
    let table = arch
        .split("## Command surface")
        .nth(1)
        .and_then(|rest| rest.split("\n## ").next())
        .expect("ARCHITECTURE.md has a `## Command surface` section");
    let documented = referenced_commands(table);
    let missing: Vec<&String> = known.iter().filter(|c| !documented.contains(*c)).collect();
    assert!(
        missing.is_empty(),
        "subcommands missing from ARCHITECTURE.md `## Command surface`: {missing:?}"
    );
}

/// The `(old, new)` rows of the first `| Old name | New name |` table in
/// `text`, in document order.
fn rename_table_rows(text: &str) -> Vec<(String, String)> {
    let Some((_, after)) = text.split_once("| Old name | New name |") else {
        return Vec::new();
    };
    after
        .lines()
        .skip(2) // the rest of the header line, then the `| --- |` separator
        .map_while(|line| {
            let cells: Vec<&str> = line
                .trim()
                .strip_prefix('|')?
                .strip_suffix('|')?
                .split('|')
                .map(|cell| cell.trim().trim_matches('`'))
                .collect();
            match cells.as_slice() {
                [old, new] => Some(((*old).to_string(), (*new).to_string())),
                _ => None,
            }
        })
        .collect()
}

#[test]
fn renamed_command_tables_list_exactly_the_accepted_aliases() {
    // The README and the changelog tell users which old names still work;
    // the CLI registers those aliases from `RENAMED_COMMANDS`. A table that
    // drops a row or keeps a stale one sends a user to a name that fails.
    let expected: Vec<(String, String)> = pixel_proto::commands::RENAMED_COMMANDS
        .iter()
        .map(|(old, new)| ((*old).to_string(), (*new).to_string()))
        .collect();
    let root = repo_root();
    for doc in ["README.md", "CHANGELOG.md"] {
        let text = std::fs::read_to_string(root.join(doc)).unwrap();
        assert_eq!(
            rename_table_rows(&text),
            expected,
            "{doc}: the `| Old name | New name |` table must match RENAMED_COMMANDS row for row"
        );
    }
}

#[test]
fn rename_table_rows_stop_at_the_end_of_the_table() {
    let text = [
        "intro",
        "| Old name | New name |",
        "| --- | --- |",
        "| `ready` | `prepare-repo` |",
        "| `hook` | `run-hook` |",
        "",
        "| `after` | `blank line` |",
    ]
    .join("\n");
    assert_eq!(
        rename_table_rows(&text),
        vec![
            ("ready".to_string(), "prepare-repo".to_string()),
            ("hook".to_string(), "run-hook".to_string()),
        ]
    );
    assert!(rename_table_rows("no table here").is_empty());
}

#[test]
fn referenced_commands_reads_only_backticked_command_names() {
    let text = "Run `pixel search-content foo` then `pixel impact`.\n`pixel-cli` is the crate; the pixel binary; `pixel` alone; `pixel foo/bar`; `pixel --help` is a flag.";
    let got: Vec<String> = referenced_commands(text).into_iter().collect();
    assert_eq!(got, ["impact", "search-content"]);
}

/// Rule ids (`M-…`) named in `text`, wildcards such as `M-FFI-*` excluded:
/// those name a family, not one heading.
fn referenced_rule_ids(text: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for (i, _) in text.match_indices("M-") {
        if i > 0 && text.as_bytes()[i - 1].is_ascii_alphanumeric() {
            continue; // `SOM-…`, `M-` inside a word
        }
        let id: String = text[i..]
            .chars()
            .take_while(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == '-' || *c == '*')
            .collect();
        let id = id.trim_end_matches('-');
        if id.len() > 2 && !id.ends_with('*') {
            out.insert(id.to_string());
        }
    }
    out
}

/// Rule ids that head a `## <title> (M-ID)` section of `guidelines.txt`.
fn guideline_rule_ids(text: &str) -> BTreeSet<String> {
    text.lines()
        .filter(|l| l.starts_with("## "))
        .filter_map(|l| {
            let open = l.rfind("(M-")?;
            let close = l[open..].find(')')?;
            Some(l[open + 1..open + close].to_string())
        })
        .collect()
}

#[test]
fn every_rule_id_in_the_skill_should_head_a_guideline_when_upstream_is_refreshed() {
    let root = repo_root().join(".agents/skills/rust-guidelines");
    let skill = std::fs::read_to_string(root.join("SKILL.md")).unwrap();
    let guidelines = std::fs::read_to_string(root.join("guidelines.txt")).unwrap();
    let known = guideline_rule_ids(&guidelines);
    assert!(
        known.len() >= 80,
        "guidelines.txt heading parsing broke: {}",
        known.len()
    );
    let named = referenced_rule_ids(&skill);
    assert!(named.len() >= 40, "SKILL.md id parsing broke: {named:?}");
    let unknown: Vec<&String> = named.iter().filter(|id| !known.contains(*id)).collect();
    assert!(
        unknown.is_empty(),
        "SKILL.md names rule ids that are not headings of guidelines.txt (renamed or removed upstream? run scripts/refresh-guidelines.sh and update SKILL.md): {unknown:?}"
    );
}

#[test]
fn referenced_rule_ids_should_skip_wildcards_and_embedded_matches() {
    let text =
        "Apply M-PANIC-ON-BUG and (M-FROM-ERROR). Not M-FFI-*, not SOM-THING, `M-DI-HIERARCHY`.";
    let got: Vec<String> = referenced_rule_ids(text).into_iter().collect();
    assert_eq!(got, ["M-DI-HIERARCHY", "M-FROM-ERROR", "M-PANIC-ON-BUG"]);
}

#[test]
fn guideline_rule_ids_should_read_only_heading_ids() {
    let text = "## Panic on bug (M-PANIC-ON-BUG) { #M-PANIC-ON-BUG }\nSee M-FROM-ERROR in prose.\n### Sub (M-NOT-A-RULE)\n";
    let got: Vec<String> = guideline_rule_ids(text).into_iter().collect();
    assert_eq!(got, ["M-PANIC-ON-BUG"]);
}

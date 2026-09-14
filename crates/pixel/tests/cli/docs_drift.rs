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

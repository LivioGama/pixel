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
    let text = "Run `pixel search foo` then `pixel impact`.\n`pixel-cli` is the crate; the pixel binary; `pixel` alone; `pixel foo/bar`; `pixel --help` is a flag.";
    let got: Vec<String> = referenced_commands(text).into_iter().collect();
    assert_eq!(got, ["impact", "search"]);
}

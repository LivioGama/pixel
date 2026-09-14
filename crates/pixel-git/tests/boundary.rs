//! The git boundary: `pixel-git` is the only crate that spawns `git` in
//! production code, and inside it only `runner.rs` does. Every other crate —
//! and every other module of `pixel-git` — goes through `GitRunner`, so
//! every call gets the wall-clock timeout, the stdout cap and stderr
//! redaction.
//!
//! Fourteen bare `Command::new("git")` sites had drifted into the other
//! crates (the guard hook, the task sandbox, `pixel status`, `pixel doctor`,
//! `pixel repo-state`, the sniper run) before this test existed; one of them
//! could hang an agent's tool call on a stuck `git status`. This test walks
//! every other crate's `src/` and fails on a spawn outside a `#[cfg(test)]
//! mod`. `pixel-git` is no exception on the inside: a second test walks its
//! own `src/` and fails on any production spawn outside `src/runner.rs`,
//! which is where the bounded primitive lives (`merge-file` used to be
//! spawned directly by `plumbing.rs`, with no timeout, cap or captured
//! stderr).
//!
//! Test code is exempt: fixtures drive real git directly by design (see
//! `.agents/rules/test-hygiene.md`). A `#[cfg(test)]` attribute followed by
//! a `mod` item starts the test region of a file, which runs to the end of
//! the file: the workspace keeps test modules last.

use std::path::{Path, PathBuf};

const SPAWN: &str = "Command::new(\"git\")";

fn crates_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .canonicalize()
        .unwrap()
}

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Byte offset where the file's test region starts: the first
/// `#[cfg(test)]` whose next non-attribute line declares a `mod`. A
/// `#[cfg(test)]` on a lone function does not open the region.
fn test_region_start(text: &str) -> Option<usize> {
    let mut offset = 0;
    let lines: Vec<&str> = text.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        if line.trim() == "#[cfg(test)]" {
            let next = lines[i + 1..]
                .iter()
                .find(|l| !l.trim_start().starts_with("#["))
                .map_or("", |l| l.trim_start());
            let item = next
                .trim_start_matches("pub(crate) ")
                .trim_start_matches("pub ");
            if item.starts_with("mod ") {
                return Some(offset);
            }
        }
        offset += line.len() + 1;
    }
    None
}

/// Production lines (1-based) of `text` that spawn git directly.
fn production_spawns(text: &str) -> Vec<usize> {
    let production = test_region_start(text).map_or(text, |end| &text[..end]);
    production
        .lines()
        .enumerate()
        .filter(|(_, line)| line.contains(SPAWN))
        .map(|(i, _)| i + 1)
        .collect()
}

#[test]
fn only_pixel_git_spawns_git_in_production_code() {
    let crates = crates_dir();
    let mut files = Vec::new();
    for entry in std::fs::read_dir(&crates).unwrap() {
        let krate = entry.unwrap().path();
        let name = krate.file_name().unwrap().to_string_lossy().into_owned();
        // The bench crate is not shipped; pixel-git is the boundary itself.
        if name == "pixel-git" || name == "pixel-bench" {
            continue;
        }
        let src = krate.join("src");
        if src.is_dir() {
            rust_sources(&src, &mut files);
        }
    }
    assert!(files.len() > 50, "walked too few files: {}", files.len());

    let mut offenders = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(&file).unwrap();
        for line in production_spawns(&text) {
            offenders.push(format!(
                "{}:{line}",
                file.strip_prefix(&crates).unwrap().display()
            ));
        }
    }
    assert!(
        offenders.is_empty(),
        "git spawned outside pixel-git; use pixel_git::GitRunner (timeout, output cap, redaction):\n  {}",
        offenders.join("\n  ")
    );
}

#[test]
fn pixel_git_spawns_git_only_in_the_runner() {
    // The crate is the boundary for everyone else; its own boundary is
    // `GitRunner`'s bounded primitive in `runner.rs`. A bare spawn in
    // another module has no timeout, no output cap and no captured stderr,
    // and the cross-crate test above (which skips `pixel-git` entirely)
    // could not see it.
    let krate = Path::new(env!("CARGO_MANIFEST_DIR"));
    let src = krate.join("src");
    let mut files = Vec::new();
    rust_sources(&src, &mut files);
    assert!(
        files.iter().any(|f| f.ends_with("runner.rs")),
        "walked {files:?} under {}: the walk is broken, not the code",
        src.display()
    );

    let mut offenders = Vec::new();
    for file in files {
        let name = file.file_name().unwrap().to_string_lossy().into_owned();
        if name == "runner.rs" {
            continue;
        }
        let text = std::fs::read_to_string(&file).unwrap();
        for line in production_spawns(&text) {
            offenders.push(format!("src/{name}:{line}"));
        }
    }
    assert!(
        offenders.is_empty(),
        "git spawned outside src/runner.rs; use GitRunner (timeout, output cap, redaction):\n  {}",
        offenders.join("\n  ")
    );
}

#[test]
fn a_test_module_exempts_only_what_follows_it() {
    let text = [
        "fn prod() {",
        "    Command::new(\"git\");",
        "}",
        "#[cfg(test)]",
        "fn helper_only_in_tests() {",
        "    Command::new(\"git\");",
        "}",
        "#[cfg(test)]",
        "mod tests {",
        "    fn fixture() { Command::new(\"git\"); }",
        "}",
    ]
    .join("\n");
    // The lone cfg(test) fn at line 5 is still production for this test's
    // purposes (it is not a module); only the `mod tests` region is exempt.
    assert_eq!(production_spawns(&text), vec![2, 6]);

    let no_tests = "fn prod() {\n    Command::new(\"git\");\n}\n";
    assert_eq!(production_spawns(no_tests), vec![2]);

    let pub_mod = [
        "#[cfg(test)]",
        "pub(crate) mod testutil {",
        "    fn git() { Command::new(\"git\"); }",
        "}",
    ]
    .join("\n");
    assert_eq!(production_spawns(&pub_mod), Vec::<usize>::new());
}

//! `gitpixel rescue` — plan correctness and apply safety invariants.

use std::path::{Path, PathBuf};
use std::process::Command;

fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn git(dir: &Path, args: &[&str]) {
    git_out(dir, args);
}

fn gitpixel(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_pixel"))
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap()
}

const V1: &str = "pub fn calc_total(items: &[u32]) -> u32 {\n    items.iter().sum()\n}\n";
const V2: &str = "pub fn calc_total(items: &[u32]) -> u32 {\n    items.iter().product()\n}\n";

/// Two commits: v1 good, v2 breaks calc (subject names it).
fn fixture(tag: &str) -> (PathBuf, String, String) {
    let dir = std::env::temp_dir().join(format!("gpx-rescue-{tag}-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    git(&dir, &["init", "-q"]);
    std::fs::write(dir.join("src/calc.rs"), V1).unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "good calc totals"]);
    let v1 = git_out(&dir, &["rev-parse", "HEAD"]).trim().to_string();
    std::fs::write(dir.join("src/calc.rs"), V2).unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "rework calc aggregation"]);
    let v2 = git_out(&dir, &["rev-parse", "HEAD"]).trim().to_string();
    (dir, v1, v2)
}

#[test]
fn plan_flags_suspect_and_recommends_last_good() {
    let (dir, v1, v2) = fixture("plan");
    let out = gitpixel(&dir, &["rescue", "calc totals broken", ".", "--json"]);
    assert!(out.status.success(), "rescue plan failed: {out:?}");
    let plan: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();

    let target = plan["targets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["path"] == "src/calc.rs")
        .expect("calc.rs must be targeted");
    let versions = target["versions"].as_array().unwrap();
    assert_eq!(versions.len(), 2);
    // Newest commit's subject contains "calc" → suspect.
    assert_eq!(versions[0]["oid"], v2.as_str());
    assert_eq!(versions[0]["suspect"], true);
    // Recommended = the version before the suspect.
    assert_eq!(target["recommended"]["oid"], v1.as_str());
    // Decision block carries both options.
    assert_eq!(plan["decision"]["options"][0]["id"], "revert");
    assert_eq!(plan["decision"]["options"][1]["id"], "fix_forward");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn apply_restores_working_tree_only() {
    let (dir, v1, _) = fixture("apply");
    let out = gitpixel(
        &dir,
        &["rescue", "--apply", &v1, "--file", "src/calc.rs", "."],
    );
    assert!(out.status.success(), "apply failed: {out:?}");
    assert_eq!(
        std::fs::read_to_string(dir.join("src/calc.rs")).unwrap(),
        V1
    );
    // Index and HEAD untouched: restore shows as an ordinary unstaged diff.
    assert_eq!(
        git_out(&dir, &["diff", "--cached", "--name-only"]).trim(),
        ""
    );
    assert!(!git_out(&dir, &["diff", "--name-only"]).trim().is_empty());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn apply_refuses_dirty_without_strategy() {
    let (dir, v1, _) = fixture("dirty");
    let in_progress = format!("{V2}// in-progress work\n");
    std::fs::write(dir.join("src/calc.rs"), &in_progress).unwrap();

    let out = gitpixel(
        &dir,
        &["rescue", "--apply", &v1, "--file", "src/calc.rs", "."],
    );
    assert!(!out.status.success(), "must refuse dirty overwrite");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("uncommitted work"), "unexpected error: {err}");
    // Nothing was touched.
    assert_eq!(
        std::fs::read_to_string(dir.join("src/calc.rs")).unwrap(),
        in_progress
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn apply_merge_keeps_in_progress_edits() {
    let (dir, v1, _) = fixture("merge");
    // In-progress edit in a different region than the v1↔v2 change.
    let in_progress = format!("// header comment added in progress\n{V2}");
    std::fs::write(dir.join("src/calc.rs"), &in_progress).unwrap();

    let out = gitpixel(
        &dir,
        &[
            "rescue",
            "--apply",
            &v1,
            "--file",
            "src/calc.rs",
            ".",
            "--merge",
        ],
    );
    assert!(out.status.success(), "merge apply failed: {out:?}");
    let merged = std::fs::read_to_string(dir.join("src/calc.rs")).unwrap();
    assert!(
        merged.contains("header comment added in progress"),
        "lost in-progress edit: {merged}"
    );
    assert!(
        merged.contains("items.iter().sum()"),
        "old good version not restored: {merged}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn apply_rejects_bad_ref() {
    let (dir, _, _) = fixture("badref");
    let out = gitpixel(
        &dir,
        &[
            "rescue",
            "--apply",
            "deadbeef",
            "--file",
            "src/calc.rs",
            ".",
        ],
    );
    assert!(!out.status.success());
    std::fs::remove_dir_all(&dir).ok();
}

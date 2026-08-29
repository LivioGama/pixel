//! Thin shell-out helpers over the `git` CLI.
//!
//! Every function degrades gracefully outside a git repository (returns
//! `None` / empty). Rename detection is disabled (`--no-renames`) so a rename
//! always surfaces as a delete + add, which the delta layer handles natively.

use std::path::Path;
use std::process::Command;

fn git_out(root: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(out.stdout)
}

/// HEAD commit OID, truncated to 40 hex chars (the shard header width).
/// `None` when not a git repo or the repo has no commits yet.
pub fn rev_parse_head(root: &Path) -> Option<String> {
    let out = git_out(root, &["rev-parse", "HEAD"])?;
    let s = String::from_utf8_lossy(&out).trim().to_string();
    if s.is_empty() {
        return None;
    }
    Some(s.chars().take(40).collect())
}

/// Tracked files (repo-relative, NUL-safe). Empty outside a git repo.
pub fn ls_files(root: &Path) -> Vec<String> {
    let Some(out) = git_out(root, &["ls-files", "-z"]) else {
        return Vec::new();
    };
    out.split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect()
}

/// Blob content of `path` as it exists in commit `oid` (`git show oid:path`).
/// Returns the raw bytes git stores for that path at that commit — for a
/// symlink this is the target text (a few bytes), never a traversal. `None`
/// on any git failure or missing path at that commit.
pub fn show_blob(root: &Path, oid: &str, rel: &str) -> Option<Vec<u8>> {
    let spec = format!("{oid}:{rel}");
    git_out(root, &["show", "--end-of-options", &spec])
}

/// Size of a committed blob without materializing it.
pub fn blob_size(root: &Path, oid: &str, rel: &str) -> Option<u64> {
    let spec = format!("{oid}:{rel}");
    let out = git_out(root, &["cat-file", "-s", &spec])?;
    String::from_utf8(out).ok()?.trim().parse().ok()
}

/// `git diff --name-status --no-renames -z <from> <to>` as (status, path).
/// Statuses are single chars: A, M, D, T (typechange), etc.
pub fn diff_name_status(root: &Path, from: &str, to: &str) -> Vec<(char, String)> {
    let Some(out) = git_out(
        root,
        &["diff", "--name-status", "--no-renames", "-z", from, to],
    ) else {
        return Vec::new();
    };
    let mut fields = out.split(|&b| b == 0).filter(|s| !s.is_empty());
    let mut result = Vec::new();
    while let Some(status) = fields.next() {
        let Some(path) = fields.next() else { break };
        let c = status.first().copied().unwrap_or(b'M') as char;
        result.push((c, String::from_utf8_lossy(path).into_owned()));
    }
    result
}

/// `git status --porcelain -z --untracked-files=all --no-renames` as
/// (XY, path). Untracked files appear with XY `"??"`.
pub fn status_porcelain(root: &Path) -> Vec<(String, String)> {
    let Some(out) = git_out(
        root,
        &[
            "status",
            "--porcelain",
            "-z",
            "--untracked-files=all",
            "--no-renames",
        ],
    ) else {
        return Vec::new();
    };
    out.split(|&b| b == 0)
        .filter(|s| s.len() > 3)
        .map(|entry| {
            let xy = String::from_utf8_lossy(&entry[0..2]).into_owned();
            let path = String::from_utf8_lossy(&entry[3..]).into_owned();
            (xy, path)
        })
        .collect()
}

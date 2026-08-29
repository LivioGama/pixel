//! `gitpixel rescue` — surgical revert planner + gated apply.
//!
//! "Something was working before" is a retrieval problem, not a coding
//! problem: locate the files the problem points at (reusing the sniper-target
//! engine), list each file's recent versions with the likely-breaking commit
//! flagged, and recommend a last-known-good candidate. Plan by default —
//! nothing is written without `--apply <oid>`, and apply obeys hard safety
//! invariants: never `reset`, never touches the index or HEAD, never
//! overwrites uncommitted work unless explicitly told how (`--merge`,
//! `--stash-first`, or `--allow-dirty`).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// git plumbing (all refs validated; paths always after `--`)
// ---------------------------------------------------------------------------

fn git(root: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map_err(|e| format!("spawn git: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Reject anything that is not a plain hex object id or simple ref name —
/// in particular anything starting with `-` (option injection).
fn validate_ref(r: &str) -> Result<(), String> {
    let ok = !r.is_empty()
        && !r.starts_with('-')
        && r.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '/' | '.' | '~' | '^'));
    if ok {
        Ok(())
    } else {
        Err(format!("invalid git ref {r:?}"))
    }
}

fn blob_oid(root: &Path, commit: &str, path: &str) -> Option<String> {
    git(root, &["rev-parse", &format!("{commit}:{path}")])
        .ok()
        .map(|s| s.trim().to_string())
}

/// Working-tree dirty map: path -> porcelain status (e.g. " M", "??").
fn dirty_map(root: &Path) -> Result<BTreeMap<String, String>, String> {
    let out = git(root, &["status", "--porcelain", "-z"])?;
    let mut map = BTreeMap::new();
    for entry in out.split('\0').filter(|e| e.len() > 3) {
        let (status, path) = entry.split_at(3);
        map.insert(path.to_string(), status.trim().to_string());
    }
    Ok(map)
}

// ---------------------------------------------------------------------------
// plan
// ---------------------------------------------------------------------------

struct VersionRow {
    oid: String,
    unix_date: u64,
    subject: String,
    suspect: bool,
    blob: Option<String>,
}

/// Build the rescue plan for one repo. `target_paths` come from --file hints
/// or the targets engine (P0 slice). `keywords` come from the tokenized
/// problem text and mark suspect commits by subject match.
pub fn plan(
    root: &Path,
    problem: &str,
    target_paths: &[String],
    keywords: &[String],
    depth: usize,
) -> Result<Value, String> {
    let dirty = dirty_map(root)?;
    let mut targets: Vec<Value> = Vec::new();

    for path in target_paths {
        let log = git(
            root,
            &[
                "log",
                "--follow",
                "-n",
                &depth.to_string(),
                "--format=%H%x1f%ct%x1f%s",
                "--",
                path,
            ],
        )
        .unwrap_or_default();

        let head_blob = blob_oid(root, "HEAD", path);
        let mut versions: Vec<VersionRow> = Vec::new();
        for line in log.lines() {
            let mut parts = line.split('\u{1f}');
            let (Some(oid), Some(ct), Some(subject)) = (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            let subject_lc = subject.to_lowercase();
            let suspect = keywords.iter().any(|k| subject_lc.contains(k.as_str()));
            versions.push(VersionRow {
                oid: oid.to_string(),
                unix_date: ct.parse().unwrap_or(0),
                subject: subject.to_string(),
                suspect,
                blob: blob_oid(root, oid, path),
            });
        }

        // Recommended last-known-good: newest version strictly older than the
        // oldest suspect commit; without suspects, the newest version whose
        // blob differs from HEAD's (i.e. the state before the last change).
        let oldest_suspect_idx = versions.iter().rposition(|v| v.suspect);
        let by_suspect = oldest_suspect_idx
            .filter(|i| i + 1 < versions.len())
            .and_then(|i| {
                versions.get(i + 1).map(|v| {
                    json!({
                        "oid": v.oid,
                        "reason": format!(
                            "last version before suspect commit {}",
                            &versions[i].oid[..7.min(versions[i].oid.len())]
                        ),
                    })
                })
            });
        // Fallback (no usable suspect — none matched, or every version
        // matched): the newest version whose content differs from HEAD, i.e.
        // the state just before the file's most recent change.
        let recommended = by_suspect.or_else(|| {
            versions
                .iter()
                .find(|v| v.blob.is_some() && v.blob != head_blob)
                .map(|v| {
                    json!({
                        "oid": v.oid,
                        "reason": "newest version whose content differs from HEAD",
                    })
                })
        });

        targets.push(json!({
            "path": path,
            "dirty": dirty.contains_key(path),
            "versions": versions.iter().map(|v| json!({
                "oid": v.oid,
                "short": &v.oid[..7.min(v.oid.len())],
                "unix_date": v.unix_date,
                "subject": v.subject,
                "suspect": v.suspect,
                "blob_differs_from_head": v.blob.is_some() && v.blob != head_blob,
            })).collect::<Vec<_>>(),
            "recommended": recommended.unwrap_or(Value::Null),
        }));
    }

    let dirty_in_plan: Vec<&String> = target_paths
        .iter()
        .filter(|p| dirty.contains_key(*p))
        .collect();
    let mut caveats: Vec<String> = Vec::new();
    for p in &dirty_in_plan {
        caveats.push(format!(
            "{p} has uncommitted changes; --apply needs --merge (3-way, keeps your edits), --stash-first, or --allow-dirty"
        ));
    }
    let rec_oids: BTreeSet<String> = targets
        .iter()
        .filter_map(|t| t["recommended"]["oid"].as_str().map(str::to_string))
        .collect();
    let revert_cmd = rec_oids.iter().next().map(|oid| {
        let files: Vec<String> = target_paths.iter().map(|p| format!("--file {p}")).collect();
        format!("gitpixel rescue --apply {oid} {} .", files.join(" "))
    });

    Ok(json!({
        "problem": problem,
        "root": root.display().to_string(),
        "dirty_files": dirty.iter().map(|(p, s)| json!({
            "path": p, "status": s, "in_plan": target_paths.contains(p),
        })).collect::<Vec<_>>(),
        "targets": targets,
        "decision": {
            "options": [
                {
                    "id": "revert",
                    "label": format!("Revert {} file(s) to the recommended version (working tree only, undoable)", target_paths.len()),
                    "command": revert_cmd,
                },
                {
                    "id": "fix_forward",
                    "label": "Keep current code; fix forward",
                },
            ],
            "caveats": caveats,
        },
    }))
}

// ---------------------------------------------------------------------------
// apply
// ---------------------------------------------------------------------------

pub struct ApplyOptions {
    pub merge: bool,
    pub stash_first: bool,
    pub allow_dirty: bool,
}

/// Restore `files` to their content at `oid`. Working tree only: the index
/// and HEAD are never touched, so every write shows up as an ordinary diff
/// the user can still undo. Dirty files are refused unless a strategy flag
/// says how to preserve the in-progress work.
pub fn apply(
    root: &Path,
    oid: &str,
    files: &[String],
    opts: &ApplyOptions,
) -> Result<Value, String> {
    validate_ref(oid)?;
    git(
        root,
        &["rev-parse", "--verify", "-q", &format!("{oid}^{{commit}}")],
    )
    .map_err(|_| format!("{oid} is not a commit in this repository"))?;
    if files.is_empty() {
        return Err("--apply requires at least one --file".to_string());
    }

    let dirty = dirty_map(root)?;
    let dirty_planned: Vec<&String> = files.iter().filter(|p| dirty.contains_key(*p)).collect();
    let has_strategy = opts.merge || opts.stash_first || opts.allow_dirty;
    if !dirty_planned.is_empty() && !has_strategy {
        return Err(format!(
            "refusing to overwrite uncommitted work in: {}. Choose a strategy: \
             --merge (3-way, keeps your edits, may leave conflict markers), \
             --stash-first (git stash push the planned files first), or \
             --allow-dirty (overwrite).",
            dirty_planned
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    if opts.stash_first && !dirty_planned.is_empty() {
        let mut args: Vec<String> = vec![
            "stash".into(),
            "push".into(),
            "-m".into(),
            "gitpixel rescue backup".into(),
            "--".into(),
        ];
        args.extend(dirty_planned.iter().map(|s| (*s).clone()));
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        git(root, &arg_refs)?;
    }

    let mut results: Vec<Value> = Vec::new();
    for path in files {
        let content = git(root, &["show", &format!("{oid}:{path}")])
            .map_err(|e| format!("{path} does not exist at {oid}: {e}"))?;
        let abs = root.join(path);
        let was_dirty = dirty.contains_key(path);

        if opts.merge && was_dirty && !opts.stash_first {
            // Deterministic 3-way merge: ours = working tree (in-progress
            // work), base = HEAD's version, theirs = the rescued version.
            let base = git(root, &["show", &format!("HEAD:{path}")]).unwrap_or_default();
            let tmp_base = abs.with_extension("gpx-rescue-base");
            let tmp_theirs = abs.with_extension("gpx-rescue-theirs");
            std::fs::write(&tmp_base, &base).map_err(|e| e.to_string())?;
            std::fs::write(&tmp_theirs, &content).map_err(|e| e.to_string())?;
            let out = Command::new("git")
                .arg("-C")
                .arg(root)
                .args([
                    "merge-file",
                    "-L",
                    "in-progress",
                    "-L",
                    "HEAD",
                    "-L",
                    &format!("rescue:{}", &oid[..7.min(oid.len())]),
                ])
                .arg(path)
                .arg(&tmp_base)
                .arg(&tmp_theirs)
                .output()
                .map_err(|e| format!("spawn git merge-file: {e}"))?;
            std::fs::remove_file(&tmp_base).ok();
            std::fs::remove_file(&tmp_theirs).ok();
            let code = out.status.code().unwrap_or(-1);
            if code < 0 {
                return Err(format!(
                    "merge-file failed for {path}: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
            results.push(json!({
                "path": path,
                "action": "merged",
                "conflicts": code,
            }));
        } else {
            // Plain restore via temp file + atomic rename.
            let tmp = abs.with_extension("gpx-rescue-tmp");
            std::fs::write(&tmp, &content).map_err(|e| format!("write {}: {e}", tmp.display()))?;
            std::fs::rename(&tmp, &abs).map_err(|e| format!("publish {}: {e}", abs.display()))?;
            results.push(json!({
                "path": path,
                "action": if was_dirty { "overwritten" } else { "restored" },
            }));
        }
    }

    Ok(json!({
        "applied": oid,
        "files": results,
        "note": "working tree only — index and HEAD untouched; undo with `git checkout -- <file>` (or `git stash pop` if --stash-first was used)",
    }))
}

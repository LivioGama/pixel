//! Isolated, refusal-first Git worktrees for Pixel task candidates.
//!
//! This module deliberately does not call reconcile or any merge strategy.
//! A candidate is promoted only when the live checkout is still at the
//! recorded commit, candidate-owned primary paths have not drifted since their
//! snapshot, and the candidate's complete tracked diff stays inside its
//! declared ownership set.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct Candidate {
    pub(crate) task_id: String,
    pub(crate) candidate_id: String,
    pub(crate) base_oid: String,
    pub(crate) primary_root: PathBuf,
    pub(crate) sandbox_root: PathBuf,
    pub(crate) owned_paths: BTreeSet<String>,
    /// Tree representing the candidate's starting filesystem, including the
    /// primary checkout's tracked WIP when it existed at creation time.
    pub(crate) baseline_tree_oid: String,
    /// Hashes of the candidate-owned files at the instant the sandbox was
    /// created. Promotion only rejects primary drift on paths the candidate
    /// will actually apply.
    #[serde(default)]
    pub(crate) primary_path_hashes: BTreeMap<String, Option<String>>,
    /// The immutable tracked-WIP patch applied to this sandbox, when any.
    #[serde(default)]
    pub(crate) overlay: Option<DirtyOverlay>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct DirtyOverlay {
    /// Git's content address of the exact binary patch.
    pub(crate) digest: String,
    /// Paths changed by the primary checkout WIP. Contents never appear in
    /// metadata or diagnostics.
    pub(crate) paths: BTreeSet<String>,
    /// Content-addressed patch storage below Pixel's task metadata.
    pub(crate) patch_path: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct CandidateInspection {
    pub(crate) candidate: Candidate,
    pub(crate) changed_paths: Vec<String>,
    pub(crate) untracked_paths: Vec<String>,
    pub(crate) verdict: CandidateVerdict,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum CandidateVerdict {
    Eligible,
    Rejected { reason: String },
    Promoted,
}

/// Create or reopen an isolated worktree at the current primary HEAD.
///
/// Tracked primary WIP is captured as an immutable, content-addressed overlay
/// and applied to the sandbox. Unsupported/untracked or credential-shaped WIP
/// is refused rather than copied.
pub(crate) fn create(
    root: &Path,
    task_id: &str,
    candidate_id: &str,
    owned_paths: impl IntoIterator<Item = String>,
) -> Result<Candidate, String> {
    let root = canonical_root(root)?;
    validate_id(task_id, "task")?;
    validate_id(candidate_id, "candidate")?;
    let owned_paths = normalize_owned_paths(owned_paths)?;
    if owned_paths.is_empty() {
        return Err("candidate ownership must contain at least one path".to_string());
    }
    reject_credential_paths(&owned_paths)?;
    let base_oid = git_success(&root, ["rev-parse", "HEAD"])?
        .trim()
        .to_string();
    let metadata = metadata_path(&root, task_id, candidate_id);
    if metadata.exists() {
        let existing: Candidate = read_json(&metadata)?;
        if existing.primary_root == root
            && existing.base_oid == base_oid
            && existing.sandbox_root.exists()
        {
            return Ok(existing);
        }
        return Err(
            "candidate metadata exists but cannot be safely reused; cleanup first".to_string(),
        );
    }

    let common_dir = common_git_dir(&root)?;
    let _lock = CommonGitLock::acquire(&common_dir)?;
    let overlay = capture_dirty_overlay(&root, task_id, &base_oid)?;
    let primary_path_hashes = snapshot_path_hashes(&root, &owned_paths)?;
    let sandbox_root = sandbox_path(&root, task_id, candidate_id);
    if sandbox_root.exists() {
        return Err(format!(
            "sandbox path already exists: {}",
            sandbox_root.display()
        ));
    }
    let parent = sandbox_root
        .parent()
        .ok_or_else(|| "sandbox path has no parent".to_string())?;
    fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    git_success(
        &root,
        [
            "worktree",
            "add",
            "--detach",
            sandbox_root.to_str().ok_or("non-UTF8 sandbox path")?,
            &base_oid,
        ],
    )?;
    if let Some(overlay) = &overlay
        && let Err(error) = apply_patch_file(&sandbox_root, &overlay.patch_path)
    {
        let _ = git_success(
            &root,
            [
                "worktree",
                "remove",
                "--force",
                sandbox_root.to_str().unwrap_or(""),
            ],
        );
        return Err(error);
    }
    let baseline_tree_oid = match baseline_tree(&sandbox_root) {
        Ok(tree) => tree,
        Err(error) => {
            let _ = git_success(
                &root,
                [
                    "worktree",
                    "remove",
                    "--force",
                    sandbox_root.to_str().unwrap_or(""),
                ],
            );
            return Err(error);
        }
    };
    let candidate = Candidate {
        task_id: task_id.to_string(),
        candidate_id: candidate_id.to_string(),
        base_oid,
        primary_root: root,
        sandbox_root,
        owned_paths,
        baseline_tree_oid,
        primary_path_hashes,
        overlay,
    };
    if let Err(error) = write_json(&metadata, &candidate) {
        let _ = git_success(
            &candidate.primary_root,
            [
                "worktree",
                "remove",
                "--force",
                candidate.sandbox_root.to_str().unwrap_or(""),
            ],
        );
        return Err(error);
    }
    Ok(candidate)
}

/// Load Pixel-owned candidate metadata without reconstructing paths or trust
/// boundaries from CLI input. Corrupt or absent metadata is deliberately
/// treated as unavailable so callers can refuse the operation safely.
pub(crate) fn load(
    root: &Path,
    task_id: &str,
    candidate_id: &str,
) -> Result<Option<Candidate>, String> {
    let root = canonical_root(root)?;
    validate_id(task_id, "task")?;
    validate_id(candidate_id, "candidate")?;
    let metadata = metadata_path(&root, task_id, candidate_id);
    if !metadata.exists() {
        return Ok(None);
    }
    let candidate: Candidate = match read_json(&metadata) {
        Ok(candidate) => candidate,
        Err(_) => return Ok(None),
    };
    if candidate.primary_root != root
        || candidate.task_id != task_id
        || candidate.candidate_id != candidate_id
    {
        return Ok(None);
    }
    Ok(Some(candidate))
}

/// Return a factual verdict. It never mutates either worktree.
pub(crate) fn inspect(candidate: &Candidate) -> Result<CandidateInspection, String> {
    let candidate = validate_candidate(candidate)?;
    let primary_head = git_success(&candidate.primary_root, ["rev-parse", "HEAD"])?
        .trim()
        .to_string();
    if primary_head != candidate.base_oid {
        return Ok(rejected(
            candidate,
            Vec::new(),
            Vec::new(),
            "primary_head_drift",
        ));
    }
    let changed_paths = lines(&git_success(
        &candidate.sandbox_root,
        ["diff", "--name-only", &candidate.baseline_tree_oid, "--"],
    )?);
    let untracked_paths = lines(&git_success(
        &candidate.sandbox_root,
        ["ls-files", "--others", "--exclude-standard"],
    )?);
    if changed_paths.is_empty() && untracked_paths.is_empty() {
        return Ok(rejected(candidate, changed_paths, untracked_paths, "no_op"));
    }
    if !untracked_paths.is_empty() {
        return Ok(rejected(
            candidate,
            changed_paths,
            untracked_paths,
            "untracked_promotion_unsupported",
        ));
    }
    if changed_paths
        .iter()
        .any(|path| !candidate.owned_paths.contains(path))
    {
        return Ok(rejected(
            candidate,
            changed_paths,
            untracked_paths,
            "out_of_ownership",
        ));
    }
    let drifted_path = changed_paths
        .iter()
        .find(
            |path| match current_path_hash(&candidate.primary_root, path) {
                Ok(current) => candidate.primary_path_hashes.get(*path) != Some(&current),
                Err(_) => true,
            },
        )
        .cloned();
    if let Some(path) = drifted_path {
        return Ok(rejected(
            candidate,
            changed_paths,
            untracked_paths,
            &format!("primary_path_drift:{path}"),
        ));
    }
    Ok(CandidateInspection {
        candidate,
        changed_paths,
        untracked_paths,
        verdict: CandidateVerdict::Eligible,
    })
}

/// Apply an eligible candidate's binary diff to the live worktree under the
/// common-git-dir lock. This is a compare-and-apply, never a merge.
pub(crate) fn promote(candidate: &Candidate) -> Result<CandidateInspection, String> {
    let candidate = validate_candidate(candidate)?;
    let common_dir = common_git_dir(&candidate.primary_root)?;
    let _lock = CommonGitLock::acquire(&common_dir)?;
    let inspection = inspect(&candidate)?;
    if inspection.verdict != CandidateVerdict::Eligible {
        return Ok(inspection);
    }
    let patch = git_success(
        &candidate.sandbox_root,
        ["diff", "--binary", &candidate.baseline_tree_oid, "--"],
    )?;
    let mut child = Command::new("git")
        .arg("apply")
        .arg("--whitespace=nowarn")
        .current_dir(&candidate.primary_root)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn git apply: {e}"))?;
    child
        .stdin
        .as_mut()
        .ok_or("git apply stdin unavailable")?
        .write_all(patch.as_bytes())
        .map_err(|e| format!("write git apply patch: {e}"))?;
    let output = child
        .wait_with_output()
        .map_err(|e| format!("wait git apply: {e}"))?;
    if !output.status.success() {
        return Ok(rejected(
            candidate,
            inspection.changed_paths,
            inspection.untracked_paths,
            &format!(
                "apply_failed:{}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ));
    }
    Ok(CandidateInspection {
        candidate,
        changed_paths: inspection.changed_paths,
        untracked_paths: inspection.untracked_paths,
        verdict: CandidateVerdict::Promoted,
    })
}

/// Safe to repeat. It only removes a path recorded in Pixel's metadata.
pub(crate) fn cleanup(root: &Path, task_id: &str, candidate_id: &str) -> Result<bool, String> {
    let root = canonical_root(root)?;
    let metadata = metadata_path(&root, task_id, candidate_id);
    if !metadata.exists() {
        return Ok(false);
    }
    let candidate: Candidate = read_json(&metadata)?;
    if candidate.primary_root != root {
        return Err("candidate primary root mismatch".to_string());
    }
    let common_dir = common_git_dir(&root)?;
    let _lock = CommonGitLock::acquire(&common_dir)?;
    if candidate.sandbox_root.exists() {
        git_success(
            &root,
            [
                "worktree",
                "remove",
                "--force",
                candidate
                    .sandbox_root
                    .to_str()
                    .ok_or("non-UTF8 sandbox path")?,
            ],
        )?;
    }
    fs::remove_file(&metadata).map_err(|e| format!("remove {}: {e}", metadata.display()))?;
    Ok(true)
}

fn rejected(
    candidate: Candidate,
    changed_paths: Vec<String>,
    untracked_paths: Vec<String>,
    reason: &str,
) -> CandidateInspection {
    CandidateInspection {
        candidate,
        changed_paths,
        untracked_paths,
        verdict: CandidateVerdict::Rejected {
            reason: reason.to_string(),
        },
    }
}

fn validate_candidate(candidate: &Candidate) -> Result<Candidate, String> {
    let primary_root = canonical_root(&candidate.primary_root)?;
    let sandbox_root = candidate
        .sandbox_root
        .canonicalize()
        .map_err(|e| format!("sandbox unavailable: {e}"))?;
    if !sandbox_root.join(".git").exists() {
        return Err("sandbox is not a Git worktree".to_string());
    }
    Ok(Candidate {
        primary_root,
        sandbox_root,
        ..candidate.clone()
    })
}

fn canonical_root(root: &Path) -> Result<PathBuf, String> {
    let root = root
        .canonicalize()
        .map_err(|e| format!("canonicalize {}: {e}", root.display()))?;
    git_success(&root, ["rev-parse", "--is-inside-work-tree"])?;
    Ok(root)
}

fn common_git_dir(root: &Path) -> Result<PathBuf, String> {
    let raw = git_success(
        root,
        ["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    PathBuf::from(raw.trim())
        .canonicalize()
        .map_err(|e| format!("canonicalize common git dir: {e}"))
}

fn sandbox_path(root: &Path, task_id: &str, candidate_id: &str) -> PathBuf {
    // Keep worktrees outside the primary checkout, but namespace them by the
    // canonical checkout directory so independently-created fixtures/repos
    // cannot collide merely because they use the same task/candidate IDs.
    let repo_name = root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("repo");
    root.parent()
        .unwrap_or(root)
        .join(".pixel-sandboxes")
        .join(repo_name)
        .join(task_id)
        .join(candidate_id)
}

fn metadata_path(root: &Path, task_id: &str, candidate_id: &str) -> PathBuf {
    root.join(".pixel")
        .join("tasks")
        .join(task_id)
        .join("candidates")
        .join(format!("{candidate_id}.json"))
}

fn normalize_owned_paths(
    paths: impl IntoIterator<Item = String>,
) -> Result<BTreeSet<String>, String> {
    paths
        .into_iter()
        .map(|path| {
            validate_owned_path(&path)?;
            Ok(path)
        })
        .collect()
}

fn validate_owned_path(path: &str) -> Result<(), String> {
    if path.is_empty()
        || Path::new(path).is_absolute()
        || path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(format!("invalid owned path: {path:?}"));
    }
    Ok(())
}

fn validate_id(value: &str, kind: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 96
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(format!("invalid {kind} id"));
    }
    Ok(())
}

fn git_success<const N: usize>(root: &Path, args: [&str; N]) -> Result<String, String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .map_err(|e| format!("spawn git: {e}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(format!(
            "git failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn lines(text: &str) -> Vec<String> {
    text.lines()
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn capture_dirty_overlay(
    root: &Path,
    task_id: &str,
    base_oid: &str,
) -> Result<Option<DirtyOverlay>, String> {
    let untracked_paths = lines(&git_success(
        root,
        ["ls-files", "--others", "--exclude-standard"],
    )?);
    if let Some(path) = untracked_paths
        .iter()
        .find(|path| !is_pixel_runtime_artifact(path))
    {
        let reason = if credential_path(path) {
            "dirty_credential_path"
        } else {
            "dirty_untracked_unsupported"
        };
        return Err(format!("{reason}:{path}"));
    }

    let paths = BTreeSet::from_iter(lines(&git_success(
        root,
        ["diff", "--name-only", base_oid, "--"],
    )?));
    if paths.is_empty() {
        return Ok(None);
    }
    reject_credential_paths(&paths)?;
    let patch = git_success(root, ["diff", "--binary", base_oid, "--"])?;
    let digest = hash_object(patch.as_bytes())?;
    let patch_path = overlay_path(root, task_id, &digest);
    if !patch_path.exists() {
        write_bytes(&patch_path, patch.as_bytes())?;
    }
    Ok(Some(DirtyOverlay {
        digest,
        paths,
        patch_path,
    }))
}

/// Pixel creates these facts while accepting and scheduling a task. They are
/// neither user WIP nor candidate input, so refusing them would make the first
/// real task command poison its own subsequent sandbox creation. Keep this an
/// exact allowlist: arbitrary files under `.pixel/` still block handoff.
fn is_pixel_runtime_artifact(path: &str) -> bool {
    matches!(path, ".pixel/actions.jsonl" | ".pixel/task-runtime.json")
        || path.starts_with(".pixel/tasks/")
}

fn baseline_tree(root: &Path) -> Result<String, String> {
    // Materialize the overlay into a temporary index tree, then restore the
    // normal worktree/index presentation for the worker. `git diff <tree>`
    // later compares against this exact overlay baseline, including staged
    // worker edits.
    git_success(root, ["add", "-A"])?;
    let tree = git_success(root, ["write-tree"])?;
    git_success(root, ["reset", "--mixed", "HEAD"])?;
    Ok(tree.trim().to_string())
}

fn snapshot_path_hashes(
    root: &Path,
    paths: &BTreeSet<String>,
) -> Result<BTreeMap<String, Option<String>>, String> {
    paths
        .iter()
        .map(|path| Ok((path.clone(), current_path_hash(root, path)?)))
        .collect()
}

fn current_path_hash(root: &Path, path: &str) -> Result<Option<String>, String> {
    validate_owned_path(path)?;
    let full_path = root.join(path);
    if !full_path.exists() && !full_path.is_symlink() {
        return Ok(None);
    }
    let output = Command::new("git")
        .args(["hash-object", "--", path])
        .current_dir(root)
        .output()
        .map_err(|e| format!("spawn git hash-object: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "git hash-object failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(Some(
        String::from_utf8_lossy(&output.stdout).trim().to_string(),
    ))
}

fn hash_object(bytes: &[u8]) -> Result<String, String> {
    let mut child = Command::new("git")
        .args(["hash-object", "--stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn git hash-object: {e}"))?;
    child
        .stdin
        .as_mut()
        .ok_or("git hash-object stdin unavailable")?
        .write_all(bytes)
        .map_err(|e| format!("write git hash-object input: {e}"))?;
    let output = child
        .wait_with_output()
        .map_err(|e| format!("wait git hash-object: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "git hash-object failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn apply_patch_file(root: &Path, patch_path: &Path) -> Result<(), String> {
    let output = Command::new("git")
        .args(["apply", "--binary", "--whitespace=nowarn"])
        .arg(patch_path)
        .current_dir(root)
        .output()
        .map_err(|e| format!("spawn git apply overlay: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "apply dirty overlay failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn overlay_path(root: &Path, task_id: &str, digest: &str) -> PathBuf {
    root.join(".pixel")
        .join("tasks")
        .join(task_id)
        .join("overlays")
        .join(format!("{digest}.patch"))
}

fn credential_path(path: &str) -> bool {
    let file_name = Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let lower_file_name = file_name.to_ascii_lowercase();
    file_name == ".env"
        || file_name.starts_with(".env.")
        || file_name.ends_with(".env")
        || file_name.starts_with("credentials.")
        || file_name.starts_with("secrets.")
        || lower_file_name.contains("secret")
        || matches!(
            Path::new(path)
                .extension()
                .and_then(|extension| extension.to_str()),
            Some("pem" | "key" | "p12" | "pfx" | "jks" | "keystore" | "truststore")
        )
        || path.starts_with("config/secrets/")
        || path.contains("/secrets/")
        || matches!(file_name, "serviceAccountKey.json")
        || file_name.ends_with("-credentials.json")
}

fn reject_credential_paths(paths: &BTreeSet<String>) -> Result<(), String> {
    if let Some(path) = paths.iter().find(|path| credential_path(path)) {
        return Err(format!("dirty_credential_path:{path}"));
    }
    Ok(())
}

fn read_json<T: for<'a> Deserialize<'a>>(path: &Path) -> Result<T, String> {
    let raw = fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    serde_json::from_slice(&raw).map_err(|e| format!("parse {}: {e}", path.display()))
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<(), String> {
    let parent = path.parent().ok_or("metadata path has no parent")?;
    fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    let temp = path.with_extension(format!("{}.tmp", std::process::id()));
    fs::write(
        &temp,
        serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("write {}: {e}", temp.display()))?;
    fs::rename(&temp, path).map_err(|e| format!("publish {}: {e}", path.display()))
}

fn write_bytes(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path.parent().ok_or("overlay path has no parent")?;
    fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    let temp = path.with_extension(format!("{}.tmp", std::process::id()));
    fs::write(&temp, bytes).map_err(|e| format!("write {}: {e}", temp.display()))?;
    fs::rename(&temp, path).map_err(|e| format!("publish {}: {e}", path.display()))
}

struct CommonGitLock {
    path: PathBuf,
}
impl CommonGitLock {
    fn acquire(common_dir: &Path) -> Result<Self, String> {
        let path = common_dir.join("pixel-task-sandbox.lock");
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| format!("acquire {}: {e}", path.display()))?;
        Ok(Self { path })
    }
}
impl Drop for CommonGitLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn fixture(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "pixel-sandbox-{name}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        git_success(&root, ["init", "-q"]).unwrap();
        git_success(&root, ["config", "user.email", "pixel@example.test"]).unwrap();
        git_success(&root, ["config", "user.name", "Pixel Test"]).unwrap();
        fs::write(root.join("owned.txt"), "base\n").unwrap();
        fs::write(root.join("other.txt"), "base\n").unwrap();
        git_success(&root, ["add", "."]).unwrap();
        git_success(&root, ["commit", "-qm", "base"]).unwrap();
        root
    }

    #[test]
    fn promotes_owned_change_and_refuses_drift_or_out_of_ownership() {
        let root = fixture("promote");
        let candidate = create(&root, "task1", "candidate1", ["owned.txt".to_string()]).unwrap();
        fs::write(candidate.sandbox_root.join("owned.txt"), "winner\n").unwrap();
        assert!(matches!(
            inspect(&candidate).unwrap().verdict,
            CandidateVerdict::Eligible
        ));
        assert!(matches!(
            promote(&candidate).unwrap().verdict,
            CandidateVerdict::Promoted
        ));
        assert_eq!(
            fs::read_to_string(root.join("owned.txt")).unwrap(),
            "winner\n"
        );
        assert!(cleanup(&root, "task1", "candidate1").unwrap());
        assert!(!cleanup(&root, "task1", "candidate1").unwrap());

        git_success(&root, ["add", "owned.txt"]).unwrap();
        git_success(&root, ["commit", "-qm", "winner"]).unwrap();
        let out = create(&root, "task2", "candidate2", ["owned.txt".to_string()]).unwrap();
        fs::write(out.sandbox_root.join("other.txt"), "not allowed\n").unwrap();
        assert!(
            matches!(inspect(&out).unwrap().verdict, CandidateVerdict::Rejected { ref reason } if reason == "out_of_ownership")
        );
        assert!(cleanup(&root, "task2", "candidate2").unwrap());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn carries_tracked_wip_and_refuses_overlapping_primary_drift() {
        let root = fixture("reject");
        fs::write(root.join("owned.txt"), "staged user wip\n").unwrap();
        git_success(&root, ["add", "owned.txt"]).unwrap();
        fs::write(root.join("other.txt"), "unstaged user wip\n").unwrap();
        let candidate = create(&root, "task1", "candidate1", ["owned.txt".to_string()]).unwrap();
        assert_eq!(
            fs::read_to_string(candidate.sandbox_root.join("owned.txt")).unwrap(),
            "staged user wip\n"
        );
        assert_eq!(
            fs::read_to_string(candidate.sandbox_root.join("other.txt")).unwrap(),
            "unstaged user wip\n"
        );
        assert!(candidate.overlay.is_some());
        fs::write(candidate.sandbox_root.join("owned.txt"), "winner\n").unwrap();
        assert!(matches!(
            promote(&candidate).unwrap().verdict,
            CandidateVerdict::Promoted
        ));
        assert_eq!(
            fs::read_to_string(root.join("owned.txt")).unwrap(),
            "winner\n"
        );
        assert_eq!(
            fs::read_to_string(root.join("other.txt")).unwrap(),
            "unstaged user wip\n"
        );
        cleanup(&root, "task1", "candidate1").unwrap();

        let candidate = create(&root, "task2", "candidate2", ["owned.txt".to_string()]).unwrap();
        fs::write(candidate.sandbox_root.join("owned.txt"), "candidate\n").unwrap();
        fs::write(root.join("owned.txt"), "new user wip\n").unwrap();
        assert!(matches!(
            inspect(&candidate).unwrap().verdict,
            CandidateVerdict::Rejected { ref reason } if reason == "primary_path_drift:owned.txt"
        ));
        cleanup(&root, "task2", "candidate2").unwrap();

        git_success(&root, ["reset", "--", "owned.txt"]).unwrap();
        git_success(&root, ["checkout", "--", "owned.txt", "other.txt"]).unwrap();
        let candidate = create(&root, "task3", "candidate3", ["owned.txt".to_string()]).unwrap();
        assert!(
            matches!(inspect(&candidate).unwrap().verdict, CandidateVerdict::Rejected { ref reason } if reason == "no_op")
        );
        cleanup(&root, "task3", "candidate3").unwrap();

        let candidate = create(&root, "task4", "candidate4", ["owned.txt".to_string()]).unwrap();
        fs::write(candidate.sandbox_root.join("owned.txt"), "candidate\n").unwrap();
        fs::write(root.join("other.txt"), "unrelated primary wip\n").unwrap();
        assert!(matches!(
            inspect(&candidate).unwrap().verdict,
            CandidateVerdict::Eligible
        ));
        cleanup(&root, "task4", "candidate4").unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn refuses_dirty_credential_or_untracked_paths_without_copying_them() {
        let root = fixture("credential");
        fs::write(root.join(".env.local"), "not-read\n").unwrap();
        assert!(
            create(&root, "task1", "candidate1", ["owned.txt".to_string()])
                .unwrap_err()
                .starts_with("dirty_credential_path:.env.local")
        );
        fs::remove_file(root.join(".env.local")).unwrap();
        fs::create_dir_all(root.join(".pixel")).unwrap();
        fs::write(root.join(".pixel/actions.jsonl"), "pixel-owned\n").unwrap();
        create(&root, "task-pixel", "candidate1", ["owned.txt".to_string()])
            .expect("Pixel-owned action log must not poison a task sandbox");
        cleanup(&root, "task-pixel", "candidate1").unwrap();
        fs::write(root.join("scratch.txt"), "not-copied\n").unwrap();
        assert!(
            create(&root, "task1", "candidate1", ["owned.txt".to_string()])
                .unwrap_err()
                .starts_with("dirty_untracked_unsupported:scratch.txt")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn load_reads_only_matching_pixel_owned_metadata() {
        let root = fixture("load");
        let candidate = create(&root, "task1", "candidate1", ["owned.txt".to_string()]).unwrap();
        assert_eq!(load(&root, "task1", "candidate1").unwrap(), Some(candidate));
        assert_eq!(load(&root, "task1", "missing").unwrap(), None);
        fs::write(metadata_path(&root, "task1", "candidate1"), "not json").unwrap();
        assert_eq!(load(&root, "task1", "candidate1").unwrap(), None);
        fs::remove_dir_all(root).unwrap();
    }
}

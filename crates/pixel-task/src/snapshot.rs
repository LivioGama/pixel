//! Strict source manifests and private copies used by task verification.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::model::{SourceFile, SourceSnapshot, TaskContract, overlaps, relative_path};
use crate::{Error, Result, digest, now_ms};

/// Refuse unexpectedly large snapshots instead of silently omitting source.
const MAX_SOURCE_FILES: usize = 100_000;
const MAX_FILE_BYTES: u64 = 268_435_456;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Stat {
    dev: u64,
    ino: u64,
    size: u64,
    mode: u32,
    mtime: i64,
    mtime_ns: i64,
    ctime: i64,
    ctime_ns: i64,
}

impl From<&fs::Metadata> for Stat {
    fn from(meta: &fs::Metadata) -> Self {
        Self {
            dev: meta.dev(),
            ino: meta.ino(),
            size: meta.len(),
            mode: meta.mode(),
            mtime: meta.mtime(),
            mtime_ns: meta.mtime_nsec(),
            ctime: meta.ctime(),
            ctime_ns: meta.ctime_nsec(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Cached {
    stat: Stat,
    file: SourceFile,
}

fn excluded(path: &str) -> bool {
    path == ".git"
        || path.starts_with(".git/")
        || path == ".pixel"
        || path.starts_with(".pixel/")
        || path == "target"
        || path.starts_with("target/")
}

fn git(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    pixel_git::GitRunner::new(root)
        .with_max_output_bytes(Some(33_554_432))
        .run_isolated(args)
        .map_err(|error| Error::Unavailable(format!("git source enumeration: {error}")))
}

fn git_refs(root: &Path) -> Result<Vec<u8>> {
    git(
        root,
        &[
            "for-each-ref",
            "--format=%(refname)%00%(objectname)%00%(symref)",
        ],
    )
}

fn git_state(root: &Path) -> Result<(Option<String>, String, String)> {
    let head = String::from_utf8(git(root, &["rev-parse", "--revs-only", "HEAD"])?)
        .map_err(|_| Error::Unavailable("invalid Git HEAD identity".into()))?;
    let head = head.trim();
    if !head.is_empty()
        && (head.len() != 40 && head.len() != 64 || !head.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err(Error::Unavailable("invalid Git HEAD identity".into()));
    }
    let index = git(root, &["ls-files", "--stage", "-v", "-z"])?;
    Ok((
        (!head.is_empty()).then(|| head.to_string()),
        hex::encode(Sha256::digest(index)),
        hex::encode(Sha256::digest(git_refs(root)?)),
    ))
}

pub(crate) fn source_identity(
    files: &[SourceFile],
    head: &Option<String>,
    index_id: &str,
    refs_id: &str,
) -> Result<String> {
    digest(&(files, head, index_id, refs_id))
}

fn listed(root: &Path, contract: &TaskContract) -> Result<BTreeSet<String>> {
    let bytes = git(
        root,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
    )?;
    let mut paths = BTreeSet::new();
    for raw in bytes.split(|byte| *byte == 0).filter(|raw| !raw.is_empty()) {
        let path = std::str::from_utf8(raw)
            .map_err(|_| Error::Unavailable("non-UTF8 source path".into()))?;
        relative_path(path, false)?;
        if excluded(path) {
            if !git(root, &["ls-files", "-z", "--", path])?.is_empty() {
                return Err(Error::Unavailable(format!(
                    "tracked source is in a reserved runtime directory: {path}"
                )));
            }
            continue;
        }
        if contract.outputs.iter().any(|output| overlaps(path, output)) {
            // A tracked file may never be declared disposable output.
            let tracked = git(root, &["ls-files", "-z", "--", path])?;
            if !tracked.is_empty() {
                return Err(Error::Invalid(format!(
                    "output overlaps tracked source: {path}"
                )));
            }
            continue;
        }
        match fs::symlink_metadata(root.join(path)) {
            Ok(meta) if meta.is_dir() => {
                return Err(Error::Unavailable(format!(
                    "submodule or directory source requires explicit capture support: {path}"
                )));
            }
            Ok(_) => {
                paths.insert(path.to_string());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
    }
    for input in &contract.inputs {
        if excluded(input) {
            return Err(Error::Invalid(format!("reserved source input: {input}")));
        }
        add_input(root, root.join(input), &mut paths)?;
    }
    if paths.len() > MAX_SOURCE_FILES {
        return Err(Error::Unavailable("source file limit exceeded".into()));
    }
    Ok(paths)
}

fn add_input(root: &Path, path: PathBuf, paths: &mut BTreeSet<String>) -> Result<()> {
    let mut pending = vec![path];
    let mut visited = 0;
    while let Some(path) = pending.pop() {
        visited += 1;
        if visited > MAX_SOURCE_FILES {
            return Err(Error::Unavailable(
                "declared input enumeration limit exceeded".into(),
            ));
        }
        let meta = fs::symlink_metadata(&path)?;
        if meta.is_dir() {
            for entry in fs::read_dir(path)? {
                pending.push(entry?.path());
            }
        } else {
            let relative = path
                .strip_prefix(root)
                .map_err(|_| Error::Invalid("input escaped root".into()))?
                .to_str()
                .ok_or_else(|| Error::Invalid("non-UTF8 input path".into()))?;
            paths.insert(relative.to_string());
        }
    }
    Ok(())
}

fn source_file(root: &Path, path: &str, paths: &BTreeSet<String>) -> Result<SourceFile> {
    let absolute = root.join(path);
    let before = fs::symlink_metadata(&absolute)?;
    if before.len() > MAX_FILE_BYTES {
        return Err(Error::Unavailable(format!(
            "source file exceeds capture limit: {path}"
        )));
    }
    let (bytes, link) = if before.file_type().is_symlink() {
        let target = fs::read_link(&absolute)?;
        if target.is_absolute() {
            return Err(Error::Unavailable(format!(
                "absolute source symlink: {path}"
            )));
        }
        let resolved = absolute.canonicalize()?;
        let relative = resolved
            .strip_prefix(root)
            .map_err(|_| Error::Unavailable(format!("source symlink escapes snapshot: {path}")))?;
        let relative = relative
            .to_str()
            .ok_or_else(|| Error::Unavailable("non-UTF8 symlink target".into()))?;
        if !paths.contains(relative)
            && !paths
                .iter()
                .any(|candidate| Path::new(candidate).starts_with(relative))
        {
            return Err(Error::Unavailable(format!(
                "symlink target is not a captured input: {path}"
            )));
        }
        let text = target
            .to_str()
            .ok_or_else(|| Error::Unavailable("non-UTF8 source symlink".into()))?
            .to_string();
        (text.as_bytes().to_vec(), Some(text))
    } else if before.is_file() {
        (fs::read(&absolute)?, None)
    } else {
        return Err(Error::Unavailable(format!(
            "unsupported source file type: {path}"
        )));
    };
    if Stat::from(&before) != Stat::from(&fs::symlink_metadata(&absolute)?) {
        return Err(Error::Unavailable(format!(
            "source changed during capture: {path}"
        )));
    }
    Ok(SourceFile {
        path: path.to_string(),
        sha256: hex::encode(Sha256::digest(&bytes)),
        mode: before.mode() & 0o777,
        symlink: link,
    })
}

/// Capture current source; strict mode bypasses the metadata digest cache.
pub fn capture(root: &Path, contract: &TaskContract, strict: bool) -> Result<SourceSnapshot> {
    contract.validate()?;
    let root = root.canonicalize()?;
    let (head, index_id, refs_id) = git_state(&root)?;
    let before = listed(&root, contract)?;
    let cache_path = root.join(".pixel/tasks/source-cache.json");
    let cache: BTreeMap<String, Cached> = if strict {
        BTreeMap::new()
    } else {
        fs::read(&cache_path)
            .ok()
            .and_then(|raw| serde_json::from_slice(&raw).ok())
            .unwrap_or_default()
    };
    let mut next = BTreeMap::new();
    let mut files = Vec::with_capacity(before.len());
    for path in &before {
        let stat = Stat::from(&fs::symlink_metadata(root.join(path))?);
        let file = match cache
            .get(path)
            .filter(|entry| entry.stat == stat && entry.file.symlink.is_none())
        {
            Some(entry) => entry.file.clone(),
            None => source_file(&root, path, &before)?,
        };
        next.insert(
            path.clone(),
            Cached {
                stat,
                file: file.clone(),
            },
        );
        files.push(file);
    }
    if (head.clone(), index_id.clone(), refs_id.clone()) != git_state(&root)?
        || before != listed(&root, contract)?
        || next.iter().any(|(path, entry)| {
            fs::symlink_metadata(root.join(path))
                .map(|meta| Stat::from(&meta))
                .ok()
                .as_ref()
                != Some(&entry.stat)
        })
    {
        return Err(Error::Unavailable(
            "source changed during snapshot capture".into(),
        ));
    }
    let content_id = source_identity(&files, &head, &index_id, &refs_id)?;
    if let Some(parent) = cache_path.parent() {
        fs::create_dir_all(parent)?;
        pixel_ops::durable::write_durably(&cache_path, &serde_json::to_vec(&next)?)?;
    }
    Ok(SourceSnapshot {
        content_id,
        root: root.display().to_string(),
        head,
        index_id,
        refs_id,
        captured_ms: now_ms(),
        files,
    })
}

/// A private copy owns every source file; live worktree edits cannot affect it.
pub fn materialize(snapshot: &SourceSnapshot) -> Result<tempfile::TempDir> {
    let directory = tempfile::Builder::new()
        .prefix("pixel-task-check-")
        .tempdir()?;
    let live_root = Path::new(&snapshot.root);
    let expected_git = (
        snapshot.head.clone(),
        snapshot.index_id.clone(),
        snapshot.refs_id.clone(),
    );
    if git_state(live_root)? != expected_git {
        return Err(Error::Unavailable(
            "Git state changed before capture copy".into(),
        ));
    }
    if !git(live_root, &["ls-files", "--unmerged", "-z"])?.is_empty()
        || !git(live_root, &["rev-parse", "--shared-index-path"])?.is_empty()
    {
        return Err(Error::Unavailable(
            "unmerged or split Git index cannot yet be captured safely".into(),
        ));
    }
    let index_path = String::from_utf8(git(live_root, &["rev-parse", "--git-path", "index"])?)
        .map_err(|_| Error::Unavailable("non-UTF8 Git index path".into()))?;
    let index_path = live_root.join(index_path.trim());
    let index_bytes = match fs::read(index_path) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let refs = git_refs(live_root)?;
    // Copy object storage and history, without shared inodes or borrowed object
    // databases. Do not commit the captured worktree: its real diff is evidence.
    git(
        live_root,
        &[
            "clone",
            "--quiet",
            "--mirror",
            "--no-hardlinks",
            "--local",
            "--dissociate",
            "--",
            &snapshot.root,
            directory
                .path()
                .join(".git")
                .to_str()
                .ok_or_else(|| Error::Unavailable("non-UTF8 capture path".into()))?,
        ],
    )?;
    git(directory.path(), &["config", "core.bare", "false"])?;
    git(directory.path(), &["config", "core.hooksPath", "/dev/null"])?;
    git(directory.path(), &["config", "core.fsmonitor", "false"])?;
    // Clone may dereference symbolic refs. Recreate their captured topology.
    for record in refs
        .split(|byte| *byte == b'\n')
        .filter(|record| !record.is_empty())
    {
        let fields: Vec<_> = record.split(|byte| *byte == 0).collect();
        if fields.len() != 3 {
            return Err(Error::Unavailable("invalid captured Git reference".into()));
        }
        if !fields[2].is_empty() {
            let name = std::str::from_utf8(fields[0])
                .map_err(|_| Error::Unavailable("non-UTF8 Git reference".into()))?;
            let target = std::str::from_utf8(fields[2])
                .map_err(|_| Error::Unavailable("non-UTF8 Git symbolic target".into()))?;
            git(directory.path(), &["symbolic-ref", name, target])?;
        }
    }
    if let Some(head) = &snapshot.head {
        git(
            directory.path(),
            &["update-ref", "--no-deref", "HEAD", head],
        )?;
    }
    if let Some(bytes) = index_bytes {
        fs::write(directory.path().join(".git/index"), bytes)?;
    }
    let paths: BTreeSet<_> = snapshot
        .files
        .iter()
        .map(|file| file.path.clone())
        .collect();
    for file in &snapshot.files {
        let live = source_file(Path::new(&snapshot.root), &file.path, &paths)?;
        if &live != file {
            return Err(Error::Unavailable(format!(
                "source changed before copy: {}",
                file.path
            )));
        }
        let target = directory.path().join(&file.path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        if let Some(link) = &file.symlink {
            symlink(link, &target)?;
        } else {
            let bytes = fs::read(Path::new(&snapshot.root).join(&file.path))?;
            if hex::encode(Sha256::digest(&bytes)) != file.sha256 {
                return Err(Error::Unavailable(format!(
                    "source changed while copying: {}",
                    file.path
                )));
            }
            fs::write(&target, bytes)?;
            fs::set_permissions(&target, fs::Permissions::from_mode(file.mode))?;
        }
    }
    if git_state(live_root)? != expected_git || git_state(directory.path())? != expected_git {
        return Err(Error::Unavailable(
            "Git state changed during capture copy".into(),
        ));
    }
    Ok(directory)
}

/// Check captured source paths, and reject unexpected undeclared output files.
pub fn unchanged(root: &Path, snapshot: &SourceSnapshot, contract: &TaskContract) -> Result<bool> {
    let current = capture(root, contract, true)?;
    Ok(current.content_id == snapshot.content_id)
}

/// Metadata of captured files detects write-and-restore during a private check.
pub(crate) fn mutation_marker(root: &Path, snapshot: &SourceSnapshot) -> Result<String> {
    let stats: Vec<_> = snapshot
        .files
        .iter()
        .map(|file| {
            fs::symlink_metadata(root.join(&file.path))
                .map(|meta| (file.path.clone(), Stat::from(&meta)))
        })
        .collect::<std::io::Result<_>>()?;
    digest(&stats)
}

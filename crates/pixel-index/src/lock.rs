//! Per-root exclusive build lock via `flock(2)`.
//!
//! When multiple CLI invocations (or CLI + daemon) race to build the same
//! index, the first process acquires an exclusive lock on
//! `root/.pixel/build.lock`, builds the shard, and releases. Concurrent
//! callers block on the lock, then load the already-built shard — no
//! duplicated work, no write races.
//!
//! The lock is advisory (`flock`) and automatically released when the file
//! descriptor is closed (process exit, panic, or explicit drop).

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

use fs2::FileExt;

use crate::index::SHARD_DIR;

/// A guard that holds the build lock. Dropping it releases the lock.
pub struct BuildLock {
    _file: File,
}

/// Comment header placed above our `.pixel/` entry when a `.gitignore` is
/// created from scratch (kept short on purpose).
const GITIGNORE_HEADER: &str = "# pixel index sidecar\n";

/// True if a repo-root `.gitignore` contains only pixel housekeeping — our
/// [`GITIGNORE_HEADER`] comment (optional) plus the `.pixel/` entry line
/// and nothing else. Such a file is a from-scratch write by
/// [`ensure_pixel_gitignored`] for a repo that previously had no ignore file:
/// it must never pollute the index/search file universe any more than `.pixel/`
/// itself does. A real user `.gitignore` carrying any other ignore rule is
/// *not* housekeepingand stays fully indexed.
pub fn is_pixel_only_gitignore(root: &Path) -> bool {
    let Ok(content) = std::fs::read_to_string(root.join(".gitignore")) else {
        return false;
    };
    is_pixel_only_gitignore_text(&content)
}

/// [`is_pixel_only_gitignore`] over the file's content: for a `.gitignore`
/// read from a commit rather than the working tree.
pub fn is_pixel_only_gitignore_text(content: &str) -> bool {
    // A purely-housekeeping `.gitignore` has at most two kinds of lines: the
    // optional header comment and the `.pixel`/`.pixel/` ignore entry. Anything
    // else (blank flames included) means real user content — index it normally.
    content.lines().all(|l| {
        let t = l.trim();
        t.is_empty() || t == ".pixel" || t == ".pixel/" || t == GITIGNORE_HEADER.trim()
    })
}

/// Append `.pixel/` to the repo's `.gitignore` so the sidecar never shows up
/// in `git status`. No-op outside a git work tree; idempotent; non-fatal.
pub fn ensure_pixel_gitignored(root: &Path) {
    // Only act inside a git work tree (root/.git exists as dir or file).
    let git_marker = root.join(".git");
    if !git_marker.is_dir() && !git_marker.is_file() {
        return;
    }
    let gitignore = root.join(".gitignore");
    // Preserve existing content exactly; non-fatal on any io error.
    let Ok(existing) = std::fs::read_to_string(&gitignore) else {
        // Read failure but file may not exist — attempt to (re)create below.
        if !gitignore.exists() {
            let _ = std::fs::write(&gitignore, format!("{GITIGNORE_HEADER}.pixel/\n"));
        }
        return;
    };
    if existing
        .lines()
        .any(|l| l.trim_end() == ".pixel" || l.trim_end() == ".pixel/")
    {
        return;
    }
    let to_append = if existing.is_empty() || existing.ends_with('\n') {
        ".pixel/\n".to_string()
    } else {
        "\n.pixel/\n".to_string()
    };
    let _ = std::fs::write(&gitignore, format!("{existing}{to_append}"));
}

impl BuildLock {
    /// Acquire an exclusive lock on `root/.pixel/build.lock`, blocking
    /// until it is available. The `.pixel` directory is created if missing.
    pub fn acquire(root: &Path) -> io::Result<Self> {
        let dir = root.join(SHARD_DIR);
        std::fs::create_dir_all(&dir)?;
        ensure_pixel_gitignored(root);
        let lock_path = dir.join("build.lock");
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)?;
        file.lock_exclusive()?;
        Ok(BuildLock { _file: file })
    }

    /// Try to acquire an exclusive lock without blocking. Returns `None`
    /// if the lock is held by another process.
    pub fn try_acquire(root: &Path) -> io::Result<Option<Self>> {
        let dir = root.join(SHARD_DIR);
        std::fs::create_dir_all(&dir)?;
        ensure_pixel_gitignored(root);
        let lock_path = dir.join("build.lock");
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)?;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(Some(BuildLock { _file: file })),
            Err(_) => Ok(None),
        }
    }
}

impl Drop for BuildLock {
    fn drop(&mut self) {
        // `fs2` unlocks automatically on drop, but be explicit.
        let _ = self._file.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only pixel's own housekeeping `.gitignore` is not project content:
    /// the header, `.pixel` or `.pixel/` and blank lines, read from the
    /// worktree file or from committed text alike.
    #[test]
    fn pixel_only_gitignore_is_recognised_from_the_file_and_from_text() {
        let dir = temp_dir();
        assert!(!is_pixel_only_gitignore(&dir), "no file is not pixel-only");
        std::fs::write(
            dir.join(".gitignore"),
            format!("{GITIGNORE_HEADER}.pixel/\n"),
        )
        .unwrap();
        assert!(is_pixel_only_gitignore(&dir));
        std::fs::write(dir.join(".gitignore"), ".pixel/\ntarget/\n").unwrap();
        assert!(!is_pixel_only_gitignore(&dir));
        assert!(is_pixel_only_gitignore_text(".pixel\n\n"));
        assert!(!is_pixel_only_gitignore_text("node_modules\n"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn gitignore_created_when_missing() {
        let dir = temp_dir();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        ensure_pixel_gitignored(&dir);
        let gi = std::fs::read_to_string(dir.join(".gitignore")).unwrap();
        assert!(gi.contains(".pixel/\n"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn gitignore_appended_without_pixel_entry() {
        let dir = temp_dir();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join(".gitignore"), "target/\n").unwrap();
        ensure_pixel_gitignored(&dir);
        let gi = std::fs::read_to_string(dir.join(".gitignore")).unwrap();
        assert!(
            gi.ends_with(".pixel/\n"),
            "expected .pixel/ appended, got: {gi:?}"
        );
        assert!(gi.starts_with("target/\n"), "existing content preserved");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn gitignore_not_duplicated_when_already_present() {
        for entry in [".pixel/", ".pixel"] {
            let dir = temp_dir();
            std::fs::create_dir_all(dir.join(".git")).unwrap();
            std::fs::write(dir.join(".gitignore"), format!("target/\n{entry}\n")).unwrap();
            // Run once.
            ensure_pixel_gitignored(&dir);
            ensure_pixel_gitignored(&dir);
            let gi = std::fs::read_to_string(dir.join(".gitignore")).unwrap();
            assert_eq!(
                gi.lines()
                    .filter(|l| *l == ".pixel" || *l == ".pixel/")
                    .count(),
                1,
                "exactly one .pixel entry, got: {gi:?}"
            );
            std::fs::remove_dir_all(&dir).ok();
        }
    }

    #[test]
    fn gitignore_untouched_outside_git_repo() {
        let dir = temp_dir();
        ensure_pixel_gitignored(&dir);
        assert!(!dir.join(".gitignore").exists(), "no .gitignore created");
        std::fs::remove_dir_all(&dir).ok();
    }

    fn temp_dir() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "pixel-gitignore-{}-{}-{}",
            std::process::id(),
            seq,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn lock_blocks_second_attempt() {
        let dir = std::env::temp_dir().join(format!(
            "pixel-lock-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();

        // First acquire succeeds.
        let _guard = BuildLock::acquire(&dir).unwrap();
        // Try-acquire (non-blocking) should return None while held.
        let second = BuildLock::try_acquire(&dir).unwrap();
        assert!(
            second.is_none(),
            "second try_acquire should fail while lock is held"
        );

        // After dropping, try-acquire succeeds.
        drop(_guard);
        let third = BuildLock::try_acquire(&dir).unwrap();
        assert!(
            third.is_some(),
            "try_acquire should succeed after lock is released"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}

//! pixel-facts — M3 / Engine 2: history-wide fact & diff ingest, search,
//! lifecycle, and rescue-v2 discovery. Owns `.pixel/history.db` plus trigram
//! history segments, with a dedicated low-priority ingest thread that never
//! blocks queries.

pub mod excavate;
pub mod ingest;
pub mod lifecycle;
pub mod poison;
pub mod search;
pub mod store;

pub use store::{FactsError, FactsStore, IndexState};

/// Git fixtures shared by the unit tests of every module: a throwaway
/// repository built with real `git`, so the parsers and the phase
/// functions are exercised against the plumbing they wrap.
#[cfg(test)]
pub(crate) mod testutil {
    use std::path::Path;
    use std::process::Command;

    pub(crate) fn git(root: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("spawn git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// An empty repository on `develop` with identity configured.
    pub(crate) fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::TempDir::new().expect("tempdir");
        git(dir.path(), &["init", "-q", "-b", "develop"]);
        git(dir.path(), &["config", "commit.gpgsign", "false"]);
        dir
    }

    /// Write `files`, stage everything and commit; returns the commit oid.
    pub(crate) fn commit(root: &Path, files: &[(&str, &[u8])], message: &str) -> String {
        for (path, bytes) in files {
            let full = root.join(path);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(full, bytes).unwrap();
        }
        git(root, &["add", "-A"]);
        git(root, &["commit", "-q", "-m", message]);
        git(root, &["rev-parse", "HEAD"])
    }
}

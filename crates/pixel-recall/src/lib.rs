//! pixel-recall — machine-wide LLM transcript retrieval corpus.
//!
//! Ingests every parseable CLI transcript store (Claude Code, Codex,
//! opencode, Devin, Cursor CLI, zcode, Gemini history, pi) into one SQLite
//! corpus of turn-granular text, then serves lexical (trigram) and semantic
//! (embedding) retrieval over it. Unlike the repo index, this corpus is
//! global: transcripts belong to the machine, not to a repository.

pub mod ask;
pub mod code_search;
pub mod embed;
pub mod export;
pub mod hybrid;
pub mod ingest;
pub mod intent;
pub mod model;
pub mod search;
pub mod segment;
pub mod sources;
pub mod store;
pub mod vector;

use std::path::PathBuf;

/// Corpus root: `$PIXEL_RECALL_DIR`, else `~/.local/share/pixel/recall`.
/// Falls back to the legacy `~/.local/share/gitpixel/recall` path if it
/// already exists (migration compat). Created on demand with owner-only
/// permissions — this directory concentrates every transcript on the machine.
pub fn recall_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("PIXEL_RECALL_DIR")
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let new_path = PathBuf::from(&home).join(".local/share/pixel/recall");
    // Migration: if the new path doesn't exist but the legacy gitpixel path
    // does, keep using the legacy path so existing users don't lose their
    // corpus. New users get the new path.
    let legacy_path = PathBuf::from(&home).join(".local/share/gitpixel/recall");
    if !new_path.exists() && legacy_path.exists() {
        return legacy_path;
    }
    new_path
}

/// Ensure the corpus root exists with mode 0700.
pub fn ensure_recall_dir() -> std::io::Result<PathBuf> {
    let dir = recall_dir();
    std::fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    Ok(dir)
}

pub fn db_path() -> PathBuf {
    recall_dir().join("recall.db")
}

pub fn segments_dir() -> PathBuf {
    recall_dir().join("segments")
}

pub fn vectors_dir() -> PathBuf {
    recall_dir().join("vectors")
}

/// Embedding model cache — shared across rebuilds, never inside a repo.
pub fn models_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let new_path = PathBuf::from(&home).join(".local/share/pixel/models");
    // Migration: use legacy path if it exists and the new one doesn't.
    let legacy_path = PathBuf::from(&home).join(".local/share/gitpixel/models");
    if !new_path.exists() && legacy_path.exists() {
        return legacy_path;
    }
    new_path
}

/// Store fixtures shared by the unit tests: sessions inserted straight
/// through `RecallStore::replace_session`, no source adapter involved.
#[cfg(test)]
pub(crate) mod testutil {
    use crate::model::{IntentSource, Role, TsSource, UnifiedSession, UnifiedTurn};
    use crate::store::{IngestState, RecallStore};

    pub(crate) const TS: i64 = 1_760_000_000_000; // 2025-10-09

    pub(crate) fn state() -> IngestState {
        IngestState {
            file_size: 1,
            mtime_ms: 1,
            bytes_ingested: 1,
            cursor: None,
        }
    }

    /// One session of `agent` with the given turns (role, text); every turn
    /// gets `TS` plus one minute per position, user turns count as human
    /// intent. Returns the session id.
    pub(crate) fn add_session(
        store: &mut RecallStore,
        agent: &'static str,
        source_session_id: &str,
        turns: &[(Role, &str)],
    ) -> i64 {
        let session = UnifiedSession {
            agent,
            source_session_id: source_session_id.to_string(),
            source_path: format!("/fake/{agent}/{source_session_id}.jsonl"),
            cwd: Some("/work/pixel".to_string()),
            git_branch: None,
            title: None,
            ts_source: TsSource::Iso,
            is_subagent: false,
            parent_source_session_id: None,
        };
        let turns: Vec<UnifiedTurn> = turns
            .iter()
            .enumerate()
            .map(|(i, (role, text))| UnifiedTurn {
                role: *role,
                intent_source: (*role == Role::User).then_some(IntentSource::Human),
                ts: Some(TS + i as i64 * 60_000),
                text: (*text).to_string(),
                truncated: false,
                source_byte_start: None,
                source_byte_len: None,
            })
            .collect();
        store
            .replace_session(&session, &turns, source_session_id, &state())
            .expect("replace_session")
    }
}

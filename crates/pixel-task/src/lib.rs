//! Durable task contracts, workflow decisions and snapshot-bound verification.

pub mod evaluation;
pub mod model;
pub mod policy;
pub mod replay;
pub mod runner;
pub mod snapshot;
pub mod store;

use std::time::{SystemTime, UNIX_EPOCH};

pub use model::*;
pub use store::Store;

/// Errors never become successful or absent task evidence.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid task input: {0}")]
    Invalid(String),
    #[error("task state unavailable: {0}")]
    Unavailable(String),
    #[error("task journal corrupt: {0}")]
    Corrupt(String),
    #[error("task not found: {0}")]
    NotFound(String),
    #[error("task busy: {0}")]
    Busy(String),
    #[error("task revision conflict: expected {expected}, actual {actual}")]
    Conflict { expected: u64, actual: u64 },
    #[error("request id reused with different input: {0}")]
    Idempotency(String),
    #[error("workflow gate blocked: {0}")]
    Blocked(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Wall time for durable event provenance, in milliseconds since Unix epoch.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as u64)
}

/// Full SHA-256 of a serialized, deterministic value.
pub fn digest<T: serde::Serialize + ?Sized>(value: &T) -> Result<String> {
    use sha2::{Digest, Sha256};
    Ok(hex::encode(Sha256::digest(serde_json::to_vec(value)?)))
}

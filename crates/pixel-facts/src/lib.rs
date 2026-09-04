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

//! pixel-session — one-look error capture: every error from every layer
//! lands at throw-time in one structured local SQLite sink, queryable in one
//! call (CLI + MCP over one shared query layer).

pub mod dedup;
pub mod format;
pub mod mcp;
pub mod parsers;
pub mod query;
pub mod run;
pub mod store;
pub mod types;

pub use store::{Store, StoreError, resolve_project_root};

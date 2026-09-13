//! Single integration-test binary for `pixel-session`: each former `tests/<name>.rs`
//! is a module here, so cargo links one executable instead of one per file.

mod query_mcp;
mod run_wrapper;
mod store;

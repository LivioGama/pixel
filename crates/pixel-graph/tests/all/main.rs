//! Single integration-test binary for `pixel-graph`: each former `tests/<name>.rs`
//! is a module here, so cargo links one executable instead of one per file.

mod changes_suggested_tests;
mod concept_engine1_audit;
mod concept_tests;
mod import_resolution;
mod resolve_receiver_shadowing;
mod scoped_symbol_lookup;
mod unresolved_diagnostic;

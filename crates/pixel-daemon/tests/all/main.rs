//! Single integration-test binary for `pixel-daemon` (see CONTRIBUTING.md:
//! one binary per crate). Each module builds its own git fixture in a temp
//! dir and drives `Service::handle` directly.

mod graph_incremental;
mod targets;

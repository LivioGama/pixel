//! Single integration-test binary for the CLI crate. Each former
//! `tests/<name>.rs` is a module here, so cargo links one executable
//! instead of one per file (linking dominated `cargo test -p pixel-cli`).

mod ask_contract;
#[cfg(unix)]
mod flow_cli;
mod guard_deny;
mod json_contract;
mod metrics_cli;
mod post_edit_cli;
mod release_check_cli;
mod rescue_cli;
mod search_compat_cli;
mod targets_cli;
#[cfg(unix)]
mod upgrade_cli;

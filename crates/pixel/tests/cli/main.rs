//! Single integration-test binary for the CLI crate. Each former
//! `tests/<name>.rs` is a module here, so cargo links one executable
//! instead of one per file (linking dominated `cargo test -p pixel-cli`).

mod ask_contract;
mod docs_drift;
mod evaluate_cli;
#[cfg(unix)]
mod flow_cli;
mod guard_deny;
mod json_contract;
mod metrics_cli;
mod post_edit_cli;
mod publish_cli;
mod recall_cli;
mod release_check_cli;
mod rename_cli;
mod renamed_commands;
mod rescue_cli;
mod scope_task_precision;
mod search_compat_cli;
mod support;
mod targets_cli;
mod uninstall_cli;
#[cfg(unix)]
mod upgrade_cli;
mod version_cli;
mod web_search_cli;

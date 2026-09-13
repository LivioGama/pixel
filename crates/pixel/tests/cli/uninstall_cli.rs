//! `pixel uninstall --wrappers-only`: the flag reaches the library through
//! the CLI and limits the run to the one shell-wrapper step, so the fix
//! `pixel doctor` names for a stray block never takes the install with it.

use std::path::Path;

use crate::support::{Scratch, pixel_command};

fn run(home: &Path, args: &[&str]) -> serde_json::Value {
    let out = pixel_command()
        .args(args)
        .env("HOME", home)
        .env("CODEX_HOME", home.join(".codex"))
        .current_dir(home)
        .output()
        .unwrap();
    assert!(out.status.success(), "pixel {args:?}: {out:?}");
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{args:?}: {e}"))
}

#[test]
fn wrappers_only_removes_one_block_and_keeps_the_rest_installed() {
    let home = Scratch::for_test("uninstall-cli", "wrappers-only");
    run(&home, &["install", "--shell", "zsh", "--json"]);
    let prompt = home.join(".local/share/pixel/agent-prompt.md");
    assert!(prompt.is_file(), "fixture: install wrote the prompt");
    assert!(
        std::fs::read_to_string(home.join(".zshrc"))
            .unwrap()
            .contains("pixel-managed")
    );

    let report = run(
        &home,
        &["uninstall", "--wrappers-only", "--shell", "zsh", "--json"],
    );
    let steps = report["steps"].as_array().expect("steps");
    assert_eq!(steps.len(), 1, "{report}");
    assert_eq!(steps[0]["id"], "shell-wrappers");
    assert_eq!(report["ok"], true, "{report}");
    assert!(
        !std::fs::read_to_string(home.join(".zshrc"))
            .unwrap()
            .contains("pixel-managed"),
        "the zsh block is gone"
    );
    assert!(
        prompt.is_file(),
        "the prompt survives a wrappers-only uninstall"
    );

    // Without the flag the same command is the full uninstall.
    let full = run(&home, &["uninstall", "--shell", "zsh", "--json"]);
    assert!(full["steps"].as_array().unwrap().len() > 1, "{full}");
    assert!(!prompt.exists(), "a full uninstall removes the prompt");
}

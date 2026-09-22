//! `pixel classify`: the parse and dispatch contract — text argument,
//! `--label`/`--criterion` validation, and `--jsonl` serve mode — exercised
//! without the embedding model (validation errors precede model load, so
//! the suite stays offline).

use crate::support::pixel_command;

#[test]
fn classify_requires_labels_unless_jsonl() {
    let out = pixel_command()
        .args(["classify", "some state text"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--label") || stderr.contains("required"),
        "{stderr}"
    );
}

#[test]
fn classify_jsonl_does_not_require_text_or_labels() {
    // An empty stdin closes immediately; the command must not demand
    // --label/--text. It may still fail at model load (offline CI) — what
    // this pins is that clap accepted the invocation shape.
    let out = pixel_command()
        .args(["classify", "--jsonl"])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("--label") && !stderr.contains("<TEXT>"),
        "clap rejected the jsonl shape: {stderr}"
    );
}

#[test]
fn classify_accepts_context_beside_the_text_argument() {
    // The shared framing is its own flag, not a second positional: a caller
    // concatenating it onto TEXT is the mistake this guards against, so clap
    // must take `--context` without reading it as the text.
    let out = pixel_command()
        .args([
            "classify",
            "the state",
            "--context",
            "the rubric preamble",
            "--label",
            "yes,no",
        ])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("unexpected argument") && !stderr.contains("--context"),
        "clap rejected --context: {stderr}"
    );
}

//! CLI measurement adapter for pixel-actionlog. No descriptor redirection,
//! comparison subprocesses, source reads, or changes to terminal detection.
use pixel_actionlog::WorkflowEvidence;
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use std::io::{self, Write};
use std::sync::atomic::{AtomicU64, Ordering};
pub static OUTPUT_BYTES: AtomicU64 = AtomicU64::new(0);
pub struct Counted<W>(pub W);
impl<W: Write> Write for Counted<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let n = self.0.write(bytes)?;
        OUTPUT_BYTES.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

/// A reader that closed its end (`… | head -1`) is a success, the policy
/// `write_stdout` applies to the capped JSON path; every other write failure
/// keeps the ordinary `print!` contract.
fn write_absorbing_closed_reader<W: Write>(
    sink: &mut W,
    args: std::fmt::Arguments<'_>,
) -> io::Result<()> {
    match sink.write_fmt(args) {
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        other => other,
    }
}

pub fn print(args: std::fmt::Arguments<'_>) {
    // Retain the ordinary print! failure contract. Only successful writes count.
    if let Err(error) = write_absorbing_closed_reader(&mut Counted(io::stdout().lock()), args) {
        panic!("failed printing to stdout: {error}");
    }
}

pub fn print_error(args: std::fmt::Arguments<'_>) {
    if let Err(error) = write_absorbing_closed_reader(&mut Counted(io::stderr().lock()), args) {
        panic!("failed printing to stderr: {error}");
    }
}

#[derive(Default)]
struct Evidence {
    root: PathBuf,
    files: HashSet<PathBuf>,
    relationships: HashSet<String>,
    partial: bool,
    unavailable: bool,
}
static EVIDENCE: Mutex<Option<Evidence>> = Mutex::new(None);

pub fn begin(root: &Path) {
    OUTPUT_BYTES.store(0, Ordering::Relaxed);
    if let Ok(mut slot) = EVIDENCE.lock() {
        *slot = Some(Evidence {
            root: root.to_path_buf(),
            ..Evidence::default()
        });
    }
}

pub fn output_bytes() -> u64 {
    OUTPUT_BYTES.load(Ordering::Relaxed)
}

pub fn unavailable() {
    if let Ok(mut slot) = EVIDENCE.lock()
        && let Some(e) = slot.as_mut()
    {
        e.unavailable = true;
    }
}

fn collect(value: &Value, evidence: &mut Evidence, depth: usize) {
    if depth > 48 {
        evidence.unavailable = true;
        return;
    }
    match value {
        Value::Array(items) => {
            for item in items {
                collect(item, evidence, depth + 1);
            }
        }
        Value::Object(fields) => {
            for (key, value) in fields {
                if (matches!(key.as_str(), "truncated" | "lower_bound" | "partial")
                    || key.ends_with("_truncated"))
                    && value == true
                {
                    evidence.partial = true;
                }
                if key == "offset" && value.as_u64().is_some_and(|v| v > 0) {
                    evidence.partial = true;
                }
                if matches!(key.as_str(), "path" | "file" | "file_path")
                    && let Some(path) = value.as_str().filter(|p| !p.is_empty())
                {
                    evidence.files.insert(evidence.root.join(path));
                }
                if matches!(
                    key.as_str(),
                    "callers"
                        | "callees"
                        | "edges"
                        | "d1_will_break"
                        | "d2_likely_affected"
                        | "d3_may_need_tests"
                ) && let Some(items) = value.as_array()
                {
                    for item in items {
                        // Only returned relationships, not total counts or unseen pages.
                        evidence.relationships.insert(format!("{key}:{item}"));
                    }
                }
                // Do not reinterpret source text, user annotations, or history hunks.
                if value.is_array() || value.is_object() {
                    collect(value, evidence, depth + 1);
                }
            }
        }
        _ => (),
    }
}

pub fn observe(value: &Value) {
    if let Ok(mut slot) = EVIDENCE.lock()
        && let Some(e) = slot.as_mut()
    {
        collect(value, e, 0);
    }
}

/// v1 command counts are explicit workflow policy, not measured executions.
fn native_commands(command: &str) -> Option<u64> {
    match command {
        "search-content" | "run-recipe" | "search-meaning" | "find-code" | "find-symbol"
        | "pack-context" | "list-signatures" | "repo-map" | "scope-task" | "who-calls"
        | "impact" | "call-path" | "what-changed" | "list-areas" | "list-flows"
        | "commit-history" | "search-history" | "dig-history" | "file-history" | "who-wrote"
        | "diff" | "list-branches" | "fetch" | "new-branch" | "fast-forward" => Some(1),
        "repo-state" | "review-changes" | "commit" => Some(3),
        "commit-and-push" => Some(4),
        // Recovery/task/flow/reconcile depend on the actual guarded plan; do not
        // invent the native steps for these or administrative commands.
        _ => None,
    }
}

pub fn evidence(command: &str, succeeded: bool) -> Option<WorkflowEvidence> {
    if !succeeded {
        return None;
    }
    let commands = native_commands(command)?;
    let slot = EVIDENCE.lock().ok()?;
    let e = slot.as_ref()?;
    if e.unavailable {
        return None;
    }
    let reads_evidence = matches!(
        command,
        "search-content"
            | "run-recipe"
            | "search-meaning"
            | "find-code"
            | "find-symbol"
            | "pack-context"
            | "list-signatures"
            | "repo-map"
            | "scope-task"
            | "who-calls"
            | "impact"
            | "call-path"
            | "what-changed"
            | "list-areas"
            | "list-flows"
    );
    Some(WorkflowEvidence {
        distinct_files: if reads_evidence {
            e.files.len() as u64
        } else {
            0
        },
        relationships: if reads_evidence {
            e.relationships.len() as u64
        } else {
            0
        },
        native_commands: commands,
        known_file_bytes: None,
        partial: e.partial,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A sink that refuses every write with one fixed error kind.
    struct Refusing(io::ErrorKind);

    impl Write for Refusing {
        fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
            Err(self.0.into())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A sink that keeps what it is asked to print.
    #[derive(Default)]
    struct Recording(Vec<u8>);

    impl Write for Recording {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_closed_reader_is_success_but_other_write_failures_survive() {
        let closed = write_absorbing_closed_reader(
            &mut Refusing(io::ErrorKind::BrokenPipe),
            format_args!("line\n"),
        );
        assert!(
            closed.is_ok(),
            "a reader that closed its end is not a failed write: {closed:?}"
        );

        let failed = write_absorbing_closed_reader(
            &mut Refusing(io::ErrorKind::WriteZero),
            format_args!("line\n"),
        );
        assert_eq!(
            failed
                .expect_err("a real write failure must not be absorbed")
                .kind(),
            io::ErrorKind::WriteZero
        );
    }

    #[test]
    fn successful_writes_reach_the_sink_unchanged() {
        let count = 7;
        let mut sink = Recording::default();
        write_absorbing_closed_reader(&mut sink, format_args!("counted {count}\n")).unwrap();
        assert_eq!(sink.0, b"counted 7\n");
    }

    #[test]
    fn metadata_is_distinct_bounded_and_never_extrapolated() {
        let mut e = Evidence {
            root: PathBuf::from("/fixture"),
            ..Evidence::default()
        };
        collect(
            &json!({"matches":[{"path":"src/a.rs"},{"path":"src/a.rs"}],
            "match_count": 800_000, "truncated":true,
            "callees":[{"path":"src/b.rs","uid":"b"}]}),
            &mut e,
            0,
        );
        assert_eq!(e.files.len(), 2);
        assert_eq!(e.relationships.len(), 1);
        assert!(e.partial);
        assert!(!e.unavailable);
        assert_eq!(native_commands("commit"), Some(3));
        assert_eq!(native_commands("task-state"), None);
    }
}

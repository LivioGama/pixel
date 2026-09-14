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

pub fn print(args: std::fmt::Arguments<'_>) {
    // Retain the ordinary print! failure contract. Only successful writes count.
    Counted(io::stdout().lock())
        .write_fmt(args)
        .expect("failed printing to stdout");
}

pub fn print_error(args: std::fmt::Arguments<'_>) {
    Counted(io::stderr().lock())
        .write_fmt(args)
        .expect("failed printing to stderr");
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

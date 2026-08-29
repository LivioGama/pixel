//! `sniper run -- <cmd>`: spawn a command, tee its output through live, and
//! on failure turn the captured output into structured error records — tsc
//! diagnostics parsed per TS code, everything else a generic tail record.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde_json::json;
use sha2::{Digest, Sha256};

use crate::parsers::{generic, tsc};
use crate::store::{Store, now_ms};
use crate::types::{ErrorInput, EventInput, EventKind, RunInput, Surface};

const TAIL_LINES: usize = 100;

/// How a wrapped command's success/failure is classified from its argv.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandClass {
    Tsc,
    Test,
    Build,
}

/// Sniff argv: `tsc` anywhere → Tsc; vitest / `cargo test` / `bun test` /
/// jest / pytest → Test; everything else → Build.
pub fn classify(argv: &[String]) -> CommandClass {
    let has = |needle: &str| {
        argv.iter().any(|a| {
            Path::new(a)
                .file_name()
                .map(|f| f.to_string_lossy() == needle)
                .unwrap_or(false)
                || a == needle
        })
    };
    if has("tsc") {
        return CommandClass::Tsc;
    }
    if has("vitest") || has("jest") || has("pytest") {
        return CommandClass::Test;
    }
    for pair in argv.windows(2) {
        if (pair[0].ends_with("cargo") || pair[0] == "cargo" || pair[0].ends_with("bun")
            || pair[0] == "bun")
            && pair[1] == "test"
        {
            return CommandClass::Test;
        }
    }
    CommandClass::Build
}

fn sha256_file(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let digest = Sha256::digest(&bytes);
    let mut hex = String::with_capacity(64);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    Some(hex)
}

fn git_head(root: &Path) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let head = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (!head.is_empty()).then_some(head)
}

fn lockfile_hash(root: &Path) -> Option<String> {
    const LOCKFILES: [&str; 6] = [
        "bun.lock",
        "bun.lockb",
        "package-lock.json",
        "yarn.lock",
        "pnpm-lock.yaml",
        "Cargo.lock",
    ];
    LOCKFILES
        .iter()
        .find_map(|name| sha256_file(&root.join(name)))
}

/// Tee one child stream to one of our streams while capturing it. Chunked,
/// so output stays live rather than buffered-then-dumped.
fn tee<R: Read + Send + 'static, W: Write + Send + 'static>(
    mut from: R,
    mut to: W,
    captured: Arc<Mutex<Vec<u8>>>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match from.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let _ = to.write_all(&buf[..n]);
                    let _ = to.flush();
                    if let Ok(mut captured) = captured.lock() {
                        captured.extend_from_slice(&buf[..n]);
                    }
                }
            }
        }
    })
}

fn record_tsc_failure(
    store: &Store,
    run_id: &str,
    label: &str,
    output: &str,
    exit_code: i32,
) -> Result<bool, String> {
    let errors = tsc::parse(output);
    if errors.is_empty() {
        return Ok(false);
    }
    for (code, group) in tsc::group_by_code(&errors) {
        let locations: Vec<String> = group
            .iter()
            .take(50)
            .map(|e| format!("{}:{}:{}", e.file, e.line, e.column))
            .collect();
        store
            .record_error(&ErrorInput {
                surface: Surface::Tsc,
                message: group[0].message.clone(),
                kind: Some(code.clone()),
                stack_raw: None,
                frames: None,
                values: None,
                http: None,
                extra: Some(json!({"count": group.len(), "locations": locations})),
                run_id: Some(run_id.to_owned()),
                ts: None,
            })
            .map_err(|e| e.to_string())?;
    }
    let codes: serde_json::Map<String, serde_json::Value> = tsc::group_by_code(&errors)
        .into_iter()
        .map(|(code, group)| (code, json!(group.len())))
        .collect();
    store
        .record_error(&ErrorInput {
            surface: Surface::Tsc,
            message: tsc::summary_message(&errors),
            kind: Some("summary".into()),
            stack_raw: None,
            frames: None,
            values: None,
            http: None,
            extra: Some(json!({
                "total": errors.len(),
                "codes": codes,
                "label": label,
                "exitCode": exit_code,
            })),
            run_id: Some(run_id.to_owned()),
            ts: None,
        })
        .map_err(|e| e.to_string())?;
    Ok(true)
}

fn record_generic_failure(
    store: &Store,
    run_id: &str,
    label: &str,
    output: &str,
    exit_code: i32,
) -> Result<(), String> {
    store
        .record_error(&ErrorInput {
            surface: Surface::RunWrapper,
            message: format!("{label} exited {exit_code}"),
            kind: Some(format!("exit-{exit_code}")),
            stack_raw: None,
            frames: None,
            values: None,
            http: None,
            extra: Some(json!({"tail": generic::tail_lines(output, TAIL_LINES)})),
            run_id: Some(run_id.to_owned()),
            ts: None,
        })
        .map_err(|e| e.to_string())?;
    store
        .record_raw_fallback(&format!("run:{label}"), output, None)
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Spawn `argv`, tee output live, record the outcome. Returns the wrapped
/// command's exit code (which the CLI mirrors).
pub fn run_wrapped(store: &Store, label: Option<&str>, argv: &[String]) -> Result<i32, String> {
    let Some(program) = argv.first() else {
        return Err("no command given (usage: sniper run [--label name] -- <cmd> [args...])".into());
    };
    let label = label.unwrap_or(program).to_owned();
    let class = classify(argv);
    let started = Instant::now();

    let mut child = Command::new(program)
        .args(&argv[1..])
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn {program}: {e}"))?;

    let run_id = format!("run-{:x}-{}", now_ms(), child.id());
    let root = store.project_root().to_path_buf();
    store
        .record_run(&RunInput {
            run_id: run_id.clone(),
            pid: Some(i64::from(child.id())),
            port: None,
            git_head: git_head(&root),
            lockfile_hash: lockfile_hash(&root),
            vite_dep_hash: None,
            fingerprint: Some(json!({
                "kind": "sniper-run",
                "argv": argv,
                "label": label,
            })),
            changed_since_last_run: None,
            ts: None,
        })
        .map_err(|e| e.to_string())?;

    let out_buf = Arc::new(Mutex::new(Vec::new()));
    let err_buf = Arc::new(Mutex::new(Vec::new()));
    let out_thread = child
        .stdout
        .take()
        .map(|stdout| tee(stdout, std::io::stdout(), out_buf.clone()));
    let err_thread = child
        .stderr
        .take()
        .map(|stderr| tee(stderr, std::io::stderr(), err_buf.clone()));

    let status = child.wait().map_err(|e| format!("wait {program}: {e}"))?;
    for thread in [out_thread, err_thread].into_iter().flatten() {
        let _ = thread.join();
    }
    let duration_ms = started.elapsed().as_millis() as i64;
    let exit_code = status.code().unwrap_or(1);

    let stdout_text = String::from_utf8_lossy(&out_buf.lock().unwrap()).into_owned();
    let stderr_text = String::from_utf8_lossy(&err_buf.lock().unwrap()).into_owned();

    if exit_code == 0 {
        let kind = match class {
            CommandClass::Test => EventKind::TestPass,
            CommandClass::Tsc | CommandClass::Build => EventKind::BuildOk,
        };
        store
            .record_event(&EventInput {
                kind,
                data: Some(json!({
                    "durationMs": duration_ms,
                    "argv": argv,
                    "label": label,
                })),
                run_id: Some(run_id),
                ts: None,
            })
            .map_err(|e| e.to_string())?;
        return Ok(0);
    }

    let combined = if stderr_text.is_empty() {
        stdout_text.clone()
    } else if stdout_text.is_empty() {
        stderr_text.clone()
    } else {
        format!("{stdout_text}\n{stderr_text}")
    };

    let parsed = match class {
        // tsc prints diagnostics on stdout; fall back to combined output.
        CommandClass::Tsc => {
            record_tsc_failure(store, &run_id, &label, &stdout_text, exit_code)?
                || record_tsc_failure(store, &run_id, &label, &stderr_text, exit_code)?
        }
        // vitest structured parsing arrives with the reporter; generic for now.
        CommandClass::Test | CommandClass::Build => false,
    };
    if !parsed {
        record_generic_failure(store, &run_id, &label, &combined, exit_code)?;
    }
    Ok(exit_code)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn classify_tsc() {
        assert_eq!(classify(&argv(&["tsc", "--noEmit"])), CommandClass::Tsc);
        assert_eq!(
            classify(&argv(&["bunx", "tsc", "--noEmit", "--pretty", "false"])),
            CommandClass::Tsc
        );
        assert_eq!(
            classify(&argv(&["node_modules/.bin/tsc"])),
            CommandClass::Tsc
        );
    }

    #[test]
    fn classify_test_runners() {
        assert_eq!(classify(&argv(&["vitest", "run"])), CommandClass::Test);
        assert_eq!(classify(&argv(&["bunx", "vitest"])), CommandClass::Test);
        assert_eq!(classify(&argv(&["cargo", "test"])), CommandClass::Test);
        assert_eq!(classify(&argv(&["bun", "test"])), CommandClass::Test);
        assert_eq!(classify(&argv(&["jest"])), CommandClass::Test);
    }

    #[test]
    fn classify_build_fallback() {
        assert_eq!(classify(&argv(&["cargo", "build"])), CommandClass::Build);
        assert_eq!(classify(&argv(&["sh", "-c", "exit 3"])), CommandClass::Build);
        assert_eq!(classify(&argv(&["bun", "run", "build"])), CommandClass::Build);
    }
}

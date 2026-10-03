//! Execute frozen checks in private source workspaces and capture factual receipts.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::model::{Check, CheckOutcome, SourceSnapshot, TaskContract, VerificationReceipt};
use crate::{Error, Result, digest, now_ms, snapshot};

const MAX_OUTPUT_BYTES: u64 = 16_777_216;

#[derive(Debug, Serialize, Deserialize)]
struct ProcessLease {
    state: String,
    process_group: Option<u32>,
}

fn lease(path: Option<&Path>, state: &str, process_group: Option<u32>) -> Result<()> {
    if let Some(path) = path {
        pixel_ops::durable::write_durably(
            path,
            &serde_json::to_vec(&ProcessLease {
                state: state.into(),
                process_group,
            })?,
        )?;
    }
    Ok(())
}

/// An interrupted launch is unknown; a recorded live group cannot be rerun.
pub fn check_recovery(path: &Path) -> Result<()> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let lease: ProcessLease = serde_json::from_slice(&bytes)?;
    if lease.state == "finished" {
        return Ok(());
    }
    if lease.state != "running" {
        return Err(Error::Blocked(
            "interrupted process launch has unknown ownership; no automatic rerun".into(),
        ));
    }
    let pid = lease
        .process_group
        .and_then(|pid| i32::try_from(pid).ok())
        .ok_or_else(|| Error::Corrupt("invalid verification process group".into()))?;
    // SAFETY: signal zero only probes the recorded process group.
    if unsafe { libc::kill(-pid, 0) } == 0
        || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    {
        return Err(Error::Busy(
            "interrupted verification process group is still alive".into(),
        ));
    }
    Ok(())
}

/// Bind declared child toolchains to observed executable bytes as well.
pub fn contract_check_identity(check: &Check, contract: &TaskContract) -> Result<String> {
    execution(check, &contract.toolchain).map(|execution| execution.identity)
}

struct Execution {
    binary: PathBuf,
    environment: BTreeMap<OsString, OsString>,
    identity: String,
}

fn execution(check: &Check, toolchain: &BTreeMap<String, String>) -> Result<Execution> {
    let program = check
        .argv
        .first()
        .ok_or_else(|| Error::Invalid("empty check command".into()))?;
    let binary = executable(program)?;
    let mut environment: BTreeMap<_, _> = std::env::vars_os().collect();
    environment.retain(|name, _| {
        !name.as_bytes().starts_with(b"GIT_") && !name.as_bytes().starts_with(b"PIXEL_TASK_")
    });
    // These values are either forbidden, private-workspace overrides, or shell
    // bookkeeping. Remove them from both the identity and the child process.
    for name in [
        "ANTHROPIC_API_KEY",
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "CARGO_TARGET_DIR",
        "PWD",
        "OLDPWD",
        "SHLVL",
        "_",
    ] {
        environment.remove(std::ffi::OsStr::new(name));
    }
    environment.insert("PIXEL_VERIFICATION".into(), "1".into());
    environment.insert("GIT_CONFIG_GLOBAL".into(), "/dev/null".into());
    environment.insert("GIT_CONFIG_SYSTEM".into(), "/dev/null".into());
    let environment_bytes: Vec<_> = environment
        .iter()
        .map(|(name, value)| (name.as_bytes(), value.as_bytes()))
        .collect();
    let mut observed_toolchain = Vec::with_capacity(toolchain.len());
    for (program, expected) in toolchain {
        let path = executable(program)?;
        let observed = hex::encode(Sha256::digest(fs::read(&path)?));
        if observed != *expected {
            return Err(Error::Blocked(format!(
                "toolchain executable differs from contract: {program}"
            )));
        }
        observed_toolchain.push((program, path.display().to_string(), observed));
    }
    let identity = digest(&(
        check,
        binary.display().to_string(),
        hex::encode(Sha256::digest(fs::read(&binary)?)),
        digest(&environment_bytes)?,
        // The actual private output directory changes per run; its semantic
        // location is fixed and never points at the live checkout.
        ("CARGO_TARGET_DIR", "<captured-workspace>/target"),
        observed_toolchain,
    ))?;
    Ok(Execution {
        binary,
        environment,
        identity,
    })
}

fn executable(program: &str) -> Result<PathBuf> {
    let candidate = Path::new(program);
    if candidate.is_absolute() {
        return candidate.canonicalize().map_err(Into::into);
    }
    if candidate.components().count() > 1 {
        return Err(Error::Invalid("check executable must be an absolute path or a PATH program; use a script interpreter for repo scripts".into()));
    }
    let paths =
        std::env::var_os("PATH").ok_or_else(|| Error::Unavailable("PATH is unavailable".into()))?;
    for directory in std::env::split_paths(&paths) {
        let path = directory.join(program);
        if let Ok(meta) = fs::metadata(&path)
            && meta.is_file()
            && meta.permissions().mode() & 0o111 != 0
        {
            return path.canonicalize().map_err(Into::into);
        }
    }
    Err(Error::Unavailable(format!(
        "check executable unavailable: {program}"
    )))
}

fn output_file(path: &Path) -> Result<File> {
    Ok(OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?)
}

fn output_summary(path: &Path) -> Result<(String, u64)> {
    use std::io::Read;
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut bytes = 0;
    let mut buffer = vec![0; 8192];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
        bytes += count as u64;
    }
    Ok((hex::encode(hash.finalize()), bytes))
}

fn terminate_group(pid: u32) {
    if let Ok(pid) = i32::try_from(pid) {
        // SAFETY: the child was started in its own process group; a negative
        // PID addresses that group, never this process's group.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
}

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        terminate_group(self.0.id());
        let _ = self.0.wait();
    }
}

/// Runs only contract checks; callers cannot submit a successful receipt.
pub(crate) fn verify_with_lease(
    snapshot: &SourceSnapshot,
    contract: &TaskContract,
    checks: &[Check],
    run_id: &str,
    lease_path: Option<&Path>,
) -> Result<Vec<VerificationReceipt>> {
    let workspace = snapshot::materialize(snapshot)?;
    let logs = tempfile::Builder::new()
        .prefix("pixel-check-output-")
        .tempdir()?;
    let mut receipts = Vec::with_capacity(checks.len());
    for check in checks {
        let source_marker = snapshot::mutation_marker(workspace.path(), snapshot)?;
        let started_ms = now_ms();
        let started = Instant::now();
        let stdout_path = logs.path().join(format!("{}.stdout", check.id));
        let stderr_path = logs.path().join(format!("{}.stderr", check.id));
        let stdout = output_file(&stdout_path)?;
        let stderr = output_file(&stderr_path)?;
        let execution = execution(check, &contract.toolchain)?;
        let cwd = workspace.path().join(&check.cwd).canonicalize()?;
        if !cwd.starts_with(workspace.path().canonicalize()?) {
            return Err(Error::Invalid(
                "check cwd escapes captured workspace".into(),
            ));
        }
        let mut command = Command::new(&execution.binary);
        command
            .args(&check.argv[1..])
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr)
            .env_clear()
            .envs(&execution.environment)
            .env("CARGO_TARGET_DIR", workspace.path().join("target"))
            .process_group(0);
        lease(lease_path, "starting", None)?;
        let mut guard = match command.spawn() {
            Ok(child) => ChildGuard(child),
            Err(error) => {
                lease(lease_path, "finished", None)?;
                return Err(error.into());
            }
        };
        let child = &mut guard.0;
        let pid = child.id();
        if let Err(error) = lease(lease_path, "running", Some(pid)) {
            terminate_group(pid);
            let _ = child.wait();
            return Err(error);
        }
        let deadline = Instant::now() + Duration::from_millis(check.timeout_ms);
        let (outcome, exit_code) = loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    terminate_group(pid);
                    break (
                        if status.success() {
                            CheckOutcome::Passed
                        } else {
                            CheckOutcome::Failed
                        },
                        status.code(),
                    );
                }
                Ok(None)
                    if fs::metadata(&stdout_path)?.len() > MAX_OUTPUT_BYTES
                        || fs::metadata(&stderr_path)?.len() > MAX_OUTPUT_BYTES =>
                {
                    terminate_group(pid);
                    child.wait()?;
                    break (CheckOutcome::Unavailable, None);
                }
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                Ok(None) => {
                    terminate_group(pid);
                    child.wait()?;
                    break (CheckOutcome::TimedOut, None);
                }
                Err(error) => {
                    terminate_group(pid);
                    let _ = child.wait();
                    return Err(error.into());
                }
            }
        };
        lease(lease_path, "finished", Some(pid))?;
        let intact = snapshot::unchanged(workspace.path(), snapshot, contract)?
            && snapshot::mutation_marker(workspace.path(), snapshot)? == source_marker;
        let (stdout_sha256, stdout_bytes) = output_summary(&stdout_path)?;
        let (stderr_sha256, stderr_bytes) = output_summary(&stderr_path)?;
        let identity_current = contract_check_identity(check, contract)
            .is_ok_and(|identity| identity == execution.identity);
        let outcome = if stdout_bytes > MAX_OUTPUT_BYTES
            || stderr_bytes > MAX_OUTPUT_BYTES
            || !identity_current
        {
            CheckOutcome::Unavailable
        } else if intact {
            outcome
        } else {
            CheckOutcome::SourceChanged
        };
        receipts.push(VerificationReceipt {
            run_id: run_id.to_string(),
            check_id: check.id.clone(),
            source_id: snapshot.content_id.clone(),
            contract_id: contract.id()?,
            check_digest: execution.identity,
            outcome,
            exit_code,
            started_ms,
            finished_ms: now_ms(),
            duration_ms: started.elapsed().as_millis() as u64,
            stdout_sha256,
            stderr_sha256,
            stdout_bytes,
            stderr_bytes,
            execution_root: workspace.path().display().to_string(),
            diagnostic: if !intact {
                Some("check modified its captured source".into())
            } else if !identity_current {
                Some("check execution environment changed during verification".into())
            } else {
                None
            },
        });
        if !intact {
            break;
        }
    }
    Ok(receipts)
}

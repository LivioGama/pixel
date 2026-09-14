//! Upgrade control never touches unrelated daemons and never waits indefinitely.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

struct Fixture(PathBuf);

impl Fixture {
    fn new(tag: &str) -> Self {
        let root = PathBuf::from("/tmp").join(format!("px-upg-{tag}-{}", std::process::id()));
        for path in ["target/release", "runtime", "home", "other"] {
            std::fs::create_dir_all(root.join(path)).unwrap();
        }
        std::fs::write(
            root.join("target/release/pixel"),
            b"candidate fixture bytes\n",
        )
        .unwrap();
        Self(root)
    }

    fn socket(&self, root: &Path) -> PathBuf {
        self.0
            .join("runtime")
            .join(pixel_daemon::socket_path(root).file_name().unwrap())
    }

    fn upgrade(&self) -> (Output, Duration, bool) {
        self.upgrade_with(&["--install-path"], &[self.0.join("installed/pixel")], None)
    }

    fn upgrade_with(
        &self,
        extra: &[&str],
        extra_paths: &[PathBuf],
        path_var: Option<&Path>,
    ) -> (Output, Duration, bool) {
        self.upgrade_env(extra, extra_paths, path_var, &[])
    }

    /// `upgrade_with` plus environment variables. The package-manager roots
    /// read from the environment are cleared first, so the developer's own
    /// `brew shellenv` or mise settings never decide a test's outcome.
    fn upgrade_env(
        &self,
        extra: &[&str],
        extra_paths: &[PathBuf],
        path_var: Option<&Path>,
        envs: &[(&str, &Path)],
    ) -> (Output, Duration, bool) {
        let start = Instant::now();
        let mut command = Command::new(env!("CARGO_BIN_EXE_pixel"));
        command.args(["self-update", "--build", "/usr/bin/true"]);
        command.args(extra);
        command.args(extra_paths);
        command
            .env_remove("MISE_DATA_DIR")
            .env_remove("HOMEBREW_CELLAR");
        command.envs(envs.iter().copied());
        if let Some(p) = path_var {
            command.env("PATH", p);
        }
        let mut child = command
            .arg("--repo")
            .arg(&self.0)
            .current_dir(&self.0)
            .env("HOME", self.0.join("home"))
            .env("TMPDIR", self.0.join("runtime"))
            .env("XDG_RUNTIME_DIR", self.0.join("runtime"))
            .env("PIXEL_METRICS", "0")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut timed_out = false;
        loop {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            if start.elapsed() > Duration::from_secs(5) {
                child.kill().unwrap();
                timed_out = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        (
            child.wait_with_output().unwrap(),
            start.elapsed(),
            timed_out,
        )
    }
}

/// Accept one client or panic after `deadline`: a blocking `accept()` on a
/// fake daemon that the upgrade never contacts would hang the test thread
/// (and the mutation gate) forever instead of failing it.
fn accept_within(listener: &UnixListener, deadline: Duration) -> std::os::unix::net::UnixStream {
    listener.set_nonblocking(true).unwrap();
    let start = Instant::now();
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).unwrap();
                return stream;
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(
                    start.elapsed() < deadline,
                    "no client connected to the fake daemon within {deadline:?}"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => panic!("accept: {e}"),
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            crate::support::assert_no_daemon(&self.0);
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn upgrade_shutdown_is_scoped_to_selected_repository() {
    let fixture = Fixture::new("scoped");
    let selected = UnixListener::bind(fixture.socket(&fixture.0)).unwrap();
    let unrelated = UnixListener::bind(fixture.socket(&fixture.0.join("other"))).unwrap();
    unrelated.set_nonblocking(true).unwrap();
    let server = std::thread::spawn(move || {
        let mut stream = accept_within(&selected, Duration::from_secs(10));
        stream
            .set_read_timeout(Some(Duration::from_secs(4)))
            .unwrap();
        let mut line = String::new();
        BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut line)
            .unwrap();
        let request: pixel_daemon::Request = serde_json::from_str(&line).unwrap();
        assert!(matches!(request, pixel_daemon::Request::Shutdown));
        let response =
            pixel_proto::Envelope::success("shutdown", serde_json::json!({"stopping": true}));
        writeln!(stream, "{}", serde_json::to_string(&response).unwrap()).unwrap();
        // Closing the listener simulates this daemon's completed shutdown.
    });
    let (output, _, timed_out) = fixture.upgrade();
    server.join().unwrap();
    assert!(!timed_out, "upgrade exceeded bounded fixture deadline");
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        unrelated.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert_eq!(
        std::fs::read(fixture.0.join("installed/pixel")).unwrap(),
        b"candidate fixture bytes\n"
    );
}

#[test]
fn upgrade_reports_unresponsive_daemon_without_claiming_completion() {
    let fixture = Fixture::new("timeout");
    let listener = UnixListener::bind(fixture.socket(&fixture.0)).unwrap();
    let server = std::thread::spawn(move || {
        let stream = accept_within(&listener, Duration::from_secs(10));
        stream
            .set_read_timeout(Some(Duration::from_secs(4)))
            .unwrap();
        let mut line = String::new();
        BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut line)
            .unwrap();
        // Real socket boundary: receive the request, then withhold a reply.
        std::thread::sleep(Duration::from_secs(3));
        line
    });
    let (output, elapsed, timed_out) = fixture.upgrade();
    let request = server.join().unwrap();
    assert!(!request.is_empty());
    assert!(!timed_out, "upgrade waited indefinitely: {output:?}");
    assert!(elapsed < Duration::from_secs(4), "{elapsed:?}");
    assert!(
        !output.status.success(),
        "timeout was silently treated as absent: {output:?}"
    );
    assert!(!String::from_utf8_lossy(&output.stderr).contains("Upgrade complete"));
    assert!(output.stdout.is_empty());
    assert_eq!(
        std::fs::read(fixture.0.join("installed/pixel")).unwrap(),
        b"candidate fixture bytes\n"
    );
}

/// Without `--install-path`, the upgrade must replace the `pixel` a shell
/// actually runs. The test binary lives in cargo's `target/`, so the
/// resolver falls through to PATH: a `pixel` sitting in a `shims` dir is a
/// launcher and must be skipped, the install behind it is the target, and
/// `~/.local/bin/pixel` (the old fixed default) must stay untouched. The
/// install sits outside any package manager's tree, so nothing refuses it.
#[test]
fn upgrade_without_install_path_replaces_the_pixel_on_path() {
    let fixture = Fixture::new("onpath");
    let shim = fixture.0.join("tools/shims/pixel");
    let managed = fixture.0.join("tools/installs/pixel/rev-1/bin/pixel");
    for p in [&shim, &managed] {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, b"old bytes\n").unwrap();
    }
    // System dirs stay last so `sh` (the build runner) still resolves.
    let path_var = std::env::join_paths([
        shim.parent().unwrap().to_path_buf(),
        managed.parent().unwrap().to_path_buf(),
        PathBuf::from("/usr/bin"),
        PathBuf::from("/bin"),
    ])
    .unwrap();
    let (output, _, timed_out) = fixture.upgrade_with(&[], &[], Some(Path::new(&path_var)));
    assert!(!timed_out);
    assert!(output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("(first pixel on PATH)"),
        "must say why the path was chosen: {stderr}"
    );
    assert_eq!(
        std::fs::read(&managed).unwrap(),
        b"candidate fixture bytes\n"
    );
    assert_eq!(
        std::fs::read(&shim).unwrap(),
        b"old bytes\n",
        "shim untouched"
    );
    assert!(
        !fixture.0.join("home/.local/bin/pixel").exists(),
        "legacy default must not receive a second copy"
    );
}

/// A stale copy earlier on PATH is exactly how an upgrade "succeeded" while
/// `pixel --version` kept answering the old build. The upgrade must say so.
#[test]
fn upgrade_warns_when_another_pixel_precedes_the_target_on_path() {
    let fixture = Fixture::new("shadow");
    let stale = fixture.0.join("stale/bin/pixel");
    std::fs::create_dir_all(stale.parent().unwrap()).unwrap();
    std::fs::write(&stale, b"stale\n").unwrap();
    let path_var = std::env::join_paths([
        stale.parent().unwrap().to_path_buf(),
        PathBuf::from("/usr/bin"),
        PathBuf::from("/bin"),
    ])
    .unwrap();
    let (output, _, timed_out) = fixture.upgrade_with(
        &["--install-path"],
        &[fixture.0.join("installed/pixel")],
        Some(Path::new(&path_var)),
    );
    assert!(!timed_out);
    assert!(output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("warning:") && stderr.contains("precedes"),
        "{stderr}"
    );
    assert!(
        stderr.contains(&stale.canonicalize().unwrap().display().to_string()),
        "{stderr}"
    );
    assert_eq!(std::fs::read(&stale).unwrap(), b"stale\n");
}

/// `--dry-run` is the way to check where an upgrade WOULD land on a given
/// machine: it must print the resolved path on stdout and touch nothing.
#[test]
fn upgrade_dry_run_prints_target_and_installs_nothing() {
    let fixture = Fixture::new("dryrun");
    let managed = fixture.0.join("cellar/bin/pixel");
    std::fs::create_dir_all(managed.parent().unwrap()).unwrap();
    std::fs::write(&managed, b"old bytes\n").unwrap();
    let path_var = std::env::join_paths([
        managed.parent().unwrap().to_path_buf(),
        PathBuf::from("/usr/bin"),
        PathBuf::from("/bin"),
    ])
    .unwrap();
    // The fixture's build runner is `/usr/bin/true`; the target file's
    // unchanged bytes prove the dry run never reached the install step.
    let (output, _, _) = fixture.upgrade_with(&["--dry-run"], &[], Some(Path::new(&path_var)));
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        managed.canonicalize().unwrap().display().to_string()
    );
    assert_eq!(std::fs::read(&managed).unwrap(), b"old bytes\n");
}

/// The layout `mise use github:LivioGama/pixel` leaves under a fixture HOME:
/// a shim (skipped by the resolver) and the install dir mise put on PATH.
/// Returns the installed binary and a PATH whose first `pixel` is it.
fn mise_install(fixture: &Fixture) -> (PathBuf, std::ffi::OsString) {
    let data = fixture.0.join("home/.local/share/mise");
    let shim = data.join("shims/pixel");
    let installed = data.join("installs/github-livio-gama-pixel/0.2.4/bin/pixel");
    for p in [&shim, &installed] {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, b"mise 0.2.4 bytes\n").unwrap();
    }
    let path_var = std::env::join_paths([
        shim.parent().unwrap().to_path_buf(),
        installed.parent().unwrap().to_path_buf(),
        PathBuf::from("/usr/bin"),
        PathBuf::from("/bin"),
    ])
    .unwrap();
    (installed, path_var)
}

/// Assert the refusal message gives the user what they need to act: the
/// resolved path it would have written, the owner, and the three ways out.
fn assert_refusal_names(stderr: &str, resolved: &Path, manager: &str) {
    let resolved = resolved.display().to_string();
    assert!(stderr.contains("refusing to install over"), "{stderr}");
    assert!(stderr.contains(&resolved), "names {resolved}: {stderr}");
    assert!(stderr.contains(manager), "names {manager}: {stderr}");
    for way_out in ["--dry-run", "--install-path", "--dev"] {
        assert!(stderr.contains(way_out), "proposes {way_out}: {stderr}");
    }
}

/// A bare `pixel self-update` resolved to the mise install dir and replaced
/// mise's 0.2.4 with a local dirty build while `mise ls` kept saying 0.2.4
/// (2026-09-14). A default that lands in mise's `installs/` must fail
/// before anything is written, and say where and why.
#[test]
fn upgrade_refuses_a_mise_install_without_explicit_install_path() {
    let fixture = Fixture::new("mise");
    let (installed, path_var) = mise_install(&fixture);
    let (output, _, timed_out) = fixture.upgrade_with(&[], &[], Some(Path::new(&path_var)));
    assert!(!timed_out);
    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_refusal_names(&stderr, &installed.canonicalize().unwrap(), "mise");
    assert!(!stderr.contains("Upgrade complete"), "{stderr}");
    assert_eq!(std::fs::read(&installed).unwrap(), b"mise 0.2.4 bytes\n");
    assert!(
        !fixture.0.join("home/.local/bin/pixel").exists(),
        "a refusal must not fall back to a second copy that shadows mise"
    );
}

/// Homebrew links `bin/pixel` to `../Cellar/pixel/<ver>/bin/pixel`. Writing
/// through that link corrupts the keg `brew` believes it installed, so the
/// refusal must follow the symlink and name the Cellar file it points to.
#[test]
fn upgrade_refuses_a_homebrew_cellar_reached_through_a_symlink() {
    let fixture = Fixture::new("cellar");
    let prefix = fixture.0.join("homebrew");
    let keg = prefix.join("Cellar/pixel/0.2.4/bin/pixel");
    std::fs::create_dir_all(keg.parent().unwrap()).unwrap();
    std::fs::write(&keg, b"brew 0.2.4 bytes\n").unwrap();
    let link = prefix.join("bin/pixel");
    std::fs::create_dir_all(link.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink("../Cellar/pixel/0.2.4/bin/pixel", &link).unwrap();
    let path_var = std::env::join_paths([
        link.parent().unwrap().to_path_buf(),
        PathBuf::from("/usr/bin"),
        PathBuf::from("/bin"),
    ])
    .unwrap();
    let cellar = prefix.join("Cellar");
    let (output, _, timed_out) = fixture.upgrade_env(
        &[],
        &[],
        Some(Path::new(&path_var)),
        &[("HOMEBREW_CELLAR", &cellar)],
    );
    assert!(!timed_out);
    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_refusal_names(&stderr, &keg.canonicalize().unwrap(), "Homebrew");
    assert_eq!(std::fs::read(&keg).unwrap(), b"brew 0.2.4 bytes\n");
    assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink());
}

/// `--install-path` is the escape hatch the refusal proposes: someone who
/// names the mise binary on purpose (to test a fix in the exact place a
/// wrapper runs it) gets the historical behaviour, no second-guessing.
#[test]
fn upgrade_writes_a_mise_install_when_install_path_is_explicit() {
    let fixture = Fixture::new("explicit");
    let (installed, path_var) = mise_install(&fixture);
    let (output, _, timed_out) = fixture.upgrade_with(
        &["--install-path"],
        std::slice::from_ref(&installed),
        Some(Path::new(&path_var)),
    );
    assert!(!timed_out);
    assert!(output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("refusing"), "{stderr}");
    assert!(stderr.contains("Upgrade complete"), "{stderr}");
    assert_eq!(
        std::fs::read(&installed).unwrap(),
        b"candidate fixture bytes\n"
    );
}

/// The refusal tells the user to run `--dry-run` to see where an upgrade
/// lands. On a refused path the dry run must still print that path on
/// stdout, repeat the refusal, exit non-zero (so `--dry-run && self-update`
/// stops there) and write nothing.
#[test]
fn upgrade_dry_run_on_a_mise_install_prints_the_path_and_writes_nothing() {
    let fixture = Fixture::new("dryrefuse");
    let (installed, path_var) = mise_install(&fixture);
    let (output, _, timed_out) =
        fixture.upgrade_with(&["--dry-run"], &[], Some(Path::new(&path_var)));
    assert!(!timed_out);
    let resolved = installed.canonicalize().unwrap();
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        resolved.display().to_string()
    );
    assert!(!output.status.success(), "{output:?}");
    assert_refusal_names(&String::from_utf8_lossy(&output.stderr), &resolved, "mise");
    assert_eq!(std::fs::read(&installed).unwrap(), b"mise 0.2.4 bytes\n");
    assert!(!fixture.0.join("home/.local/bin").exists());
}

/// `--dev` is how a local build gets exercised on a mise machine: it lands
/// in `~/.local/bin/pixel-dev`, a name no `pixel` lookup ever picks, so the
/// managed `pixel` keeps its bytes and nothing on PATH is shadowed (and no
/// shadow warning about an unrelated `pixel` is printed).
#[test]
fn upgrade_dev_installs_pixel_dev_and_leaves_pixel_alone() {
    let fixture = Fixture::new("dev");
    let (installed, path_var) = mise_install(&fixture);
    let (output, _, timed_out) = fixture.upgrade_with(&["--dev"], &[], Some(Path::new(&path_var)));
    assert!(!timed_out);
    assert!(output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("(--dev)"), "{stderr}");
    assert!(!stderr.contains("warning:"), "{stderr}");
    assert_eq!(
        std::fs::read(fixture.0.join("home/.local/bin/pixel-dev")).unwrap(),
        b"candidate fixture bytes\n"
    );
    assert!(!fixture.0.join("home/.local/bin/pixel").exists());
    assert_eq!(std::fs::read(&installed).unwrap(), b"mise 0.2.4 bytes\n");
}

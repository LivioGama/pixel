//! Upgrade control never touches unrelated daemons and never waits indefinitely.
#![cfg(unix)]

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
        let start = Instant::now();
        let mut child = Command::new(env!("CARGO_BIN_EXE_pixel"))
            .args(["upgrade", "--build", "/usr/bin/true", "--install-path"])
            .arg(self.0.join("installed/pixel"))
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

impl Drop for Fixture {
    fn drop(&mut self) {
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
        let (mut stream, _) = selected.accept().unwrap();
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
        let (stream, _) = listener.accept().unwrap();
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

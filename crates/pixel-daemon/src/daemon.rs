//! Unix-socket NDJSON daemon: one JSON `Request` per line, one JSON
//! `Response` line back. Single-threaded request handling (requests are
//! fast); an accept thread and a notify watcher feed one mpsc channel.

use std::collections::BTreeMap;
use std::io::{BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use fs2::FileExt;
use notify::{RecursiveMode, Watcher};

use crate::api::{Request, Response, ServeError, Service, failure_response};

const IDLE_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const DEBOUNCE: Duration = Duration::from_millis(500);
/// Longest the main loop sleeps before re-checking that the served root
/// still exists. A daemon whose root was deleted (a removed worktree, a
/// test fixture) has nothing left to serve and must not sit on the machine
/// for the rest of `IDLE_TIMEOUT`: auto-started daemons are detached from
/// their parent, so nothing else would ever reap them.
const ROOT_POLL: Duration = Duration::from_secs(5);
/// Idle poll interval for the facts ingest thread once fresh. A ref move
/// re-triggers ingest on the next poll without blocking queries.
const INGEST_IDLE_POLL: Duration = Duration::from_secs(5);
/// Backoff after a transient ingest error (e.g. a git lock held by another
/// process) before retrying.
const INGEST_ERROR_BACKOFF: Duration = Duration::from_secs(2);
const IGNORED_DIRS: &[&str] = &[
    ".pixel",
    ".git",
    "target",
    "node_modules",
    "bower_components",
    "Pods",
    "vendor",
    "_build",
    "DerivedData",
    "dist",
    "build",
    "out",
    ".gradle",
    "__pycache__",
    ".venv",
    "venv",
    ".tox",
    "site-packages",
    ".terraform",
    ".next",
    ".nuxt",
    ".turbo",
    ".cache",
    ".npm",
    ".yarn",
    ".pnpm-store",
];
/// Maximum length of a single NDJSON request line. A request larger than this
/// is rejected to prevent a malicious client from exhausting memory with a
/// multi-GB line. The largest legitimate request (a search pattern) is well
/// under 1 KiB.
const MAX_REQUEST_LINE: usize = 64 * 1024;
const CONNECTION_DEADLINE: Duration = Duration::from_secs(5);
/// Maximum number of requests served on a single connection before it is
/// closed. Prevents a single long-lived client from monopolizing the
/// single-threaded daemon indefinitely.
const MAX_REQUESTS_PER_CONN: u32 = 64;

/// $TMPDIR/pixel-<xxh3-of-canonical-root>.sock
///
/// On Linux, `TMPDIR` defaults to world-writable `/tmp`, which allows any
/// local user to predict the socket path and squat on it before the daemon
/// binds. We prefer `XDG_RUNTIME_DIR` (per-user, 0700, tmpfs) when available,
/// falling back to `TMPDIR` only on macOS (where `TMPDIR` is already per-user
/// 0700). On Linux without `XDG_RUNTIME_DIR`, we use `~/.cache/pixel/sockets/`
/// created with 0700 permissions.
///
/// Deliberately distinct from the legacy gitpixel tool's `gitpixel-*.sock`
/// prefix: the two daemons speak incompatible response envelopes (gitpixel's
/// `{ok,error,data}` vs pixel's `{op,protocol,...}`), so an old gitpixel
/// daemon and this one must bind to different socket paths and coexist
/// independently rather than collide on the same one.
pub fn socket_path(root: &Path) -> PathBuf {
    let canon = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let h = xxhash_rust::xxh3::xxh3_64(canon.to_string_lossy().as_bytes());
    let sock_name = format!("pixel-{h:016x}.sock");
    runtime_dir().join(sock_name)
}

/// Return a per-user directory for socket files. On macOS, `TMPDIR` is
/// already per-user with 0700 permissions. On Linux, prefer
/// `XDG_RUNTIME_DIR`; if unset, create `~/.cache/pixel/sockets/` with 0700.
fn runtime_dir() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        std::env::temp_dir()
    }
    #[cfg(not(target_os = "macos"))]
    {
        if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR")
            && !dir.is_empty()
            && std::path::Path::new(&dir).exists()
        {
            return PathBuf::from(dir);
        }
        // Fallback: ~/.cache/pixel/sockets/ with 0700 perms.
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        let dir = PathBuf::from(home).join(".cache/pixel/sockets");
        let _ = std::fs::create_dir_all(&dir);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        }
        dir
    }
}

pub fn pid_path(root: &Path) -> PathBuf {
    socket_path(root).with_extension("pid")
}

enum Msg {
    Conn(UnixStream),
    Fs(notify::Event),
}

/// A corpus a daemon can serve: the repo `Service`, or the machine-wide
/// transcript recall service. The transport (socket, watcher, debounce,
/// framing) is identical for every corpus.
pub trait Corpus {
    /// Root that keys the socket path and is watched by default.
    fn root(&self) -> &Path;
    fn handle(&mut self, req: Request) -> Response;
    /// Debounced watcher callback with the absolute changed path.
    fn apply_change(&mut self, abs: &Path, removed: bool);
    /// Debounced watcher callback with a batch of changed paths (default: loops apply_change).
    fn apply_changes(&mut self, changes: &[(PathBuf, bool)]) {
        for (abs, removed) in changes {
            self.apply_change(abs, *removed);
        }
    }
    /// Directories the watcher observes (default: the root).
    fn watch_paths(&self) -> Vec<PathBuf> {
        vec![self.root().to_path_buf()]
    }
    /// How often the loop calls `sweep` regardless of watcher events
    /// (default: never). A corpus whose writers hold files open for
    /// minutes (streamed transcripts) sets this, because FSEvents on macOS
    /// defers the modify event until the writer closes the file.
    fn sweep_interval(&self) -> Option<Duration> {
        None
    }
    /// Periodic maintenance, called every `sweep_interval` from the loop
    /// thread (default: nothing).
    fn sweep(&mut self) {}
}

impl Corpus for Service {
    fn root(&self) -> &Path {
        Service::root(self)
    }

    fn handle(&mut self, req: Request) -> Response {
        Service::handle(self, req)
    }

    fn apply_change(&mut self, abs: &Path, removed: bool) {
        let root = Service::root(self).to_path_buf();
        let Ok(rel) = abs.strip_prefix(&root) else {
            return;
        };
        let rel = rel.to_string_lossy().into_owned();
        if rel.is_empty() {
            return;
        }
        if removed {
            self.remove_file(&rel);
        } else {
            self.refresh_file(&rel);
        }
    }

    fn apply_changes(&mut self, changes: &[(PathBuf, bool)]) {
        let root = Service::root(self).to_path_buf();
        let rel_changes: Vec<(String, bool)> = changes
            .iter()
            .filter_map(|(abs, removed)| {
                let rel = abs.strip_prefix(&root).ok()?.to_string_lossy().into_owned();
                if rel.is_empty() {
                    None
                } else {
                    Some((rel, *removed))
                }
            })
            .collect();
        let slice: Vec<(&str, bool)> = rel_changes
            .iter()
            .map(|(r, rem)| (r.as_str(), *rem))
            .collect();
        self.refresh_files(&slice);
    }
}

/// Run the repo daemon in the foreground until Shutdown, idle timeout, or
/// error.
pub fn run(root: &Path) -> Result<(), ServeError> {
    let service = Service::open(root)?;
    spawn_facts_ingest(root);
    run_corpus(service)
}

/// Spawn a low-priority background thread that periodically ticks the facts
/// ingest (history.db) until fresh, then idle-polls so a ref move re-triggers
/// ingest. Queries never block on it: the ingest shares the WAL-mode
/// connection and yields every tick budget.
fn spawn_facts_ingest(root: &Path) {
    let root = root.to_path_buf();
    std::thread::spawn(move || {
        let mut store = match pixel_facts::FactsStore::open(&root) {
            Ok(s) => s,
            Err(_) => return,
        };
        let opts = pixel_facts::ingest::IngestOptions::default();
        // Periodic tick loop: keep ingesting until fresh, then idle-poll so a
        // ref move re-triggers ingest. Each tick is budget-bounded, so queries
        // on the same WAL-mode connection are never starved.
        let mut tick_count = 0u64;
        loop {
            match pixel_facts::ingest::ingest_tick(&mut store, &opts) {
                Ok(report) if report.fresh => {
                    let _ = store.wal_checkpoint();
                    std::thread::sleep(INGEST_IDLE_POLL);
                }
                Ok(_) => {
                    tick_count += 1;
                    if tick_count.is_multiple_of(20) {
                        let _ = store.wal_checkpoint();
                    }
                    std::thread::sleep(Duration::from_millis(250));
                }
                Err(_) => {
                    // Transient error (e.g. git lock): back off and retry.
                    std::thread::sleep(INGEST_ERROR_BACKOFF);
                }
            }
        }
    });
}

/// Run any corpus daemon in the foreground.
pub fn run_corpus(mut service: impl Corpus) -> Result<(), ServeError> {
    let root = service.root().to_path_buf();
    let sock = socket_path(&root);

    // Advisory lock on pid_path to prevent concurrent startup race and duplicate running daemons.
    let lock_path = pid_path(&root).with_extension("lock");
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)?;
    if lock_file.try_lock_exclusive().is_err() {
        return Err(ServeError::Msg(format!(
            "daemon lock already held by another process for {}",
            root.display()
        )));
    }

    // A live socket means another daemon owns this root.
    if UnixStream::connect(&sock).is_ok() {
        return Err(ServeError::Msg(format!(
            "daemon already running for {} ({})",
            root.display(),
            sock.display()
        )));
    }
    let _ = std::fs::remove_file(&sock); // stale leftover

    let listener = UnixListener::bind(&sock)?;
    std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600))?;
    std::fs::write(pid_path(&root), std::process::id().to_string())?;

    let (tx, rx) = mpsc::channel::<Msg>();

    // Accept thread: forwards connections into the single-threaded loop.
    let tx_conn = tx.clone();
    let accept_listener = listener.try_clone()?;
    std::thread::spawn(move || {
        for stream in accept_listener.incoming() {
            match stream {
                Ok(s) => {
                    if tx_conn.send(Msg::Conn(s)).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    // Watcher: raw notify events into the channel; debounced below.
    let tx_fs = tx.clone();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res {
            let _ = tx_fs.send(Msg::Fs(ev));
        }
    })
    .map_err(|e| ServeError::Msg(format!("watcher init: {e}")))?;
    let watch_paths = service.watch_paths();
    for wp in &watch_paths {
        watcher
            .watch(wp, RecursiveMode::Recursive)
            .map_err(|e| ServeError::Msg(format!("watch {}: {e}", wp.display())))?;
    }

    eprintln!(
        "pixel daemon: root={} socket={}",
        root.display(),
        sock.display()
    );

    // absolute path -> removed?
    let mut pending: BTreeMap<PathBuf, bool> = BTreeMap::new();
    let mut flush_at: Option<Instant> = None;
    let mut last_activity = Instant::now();
    let mut shutdown = false;
    let sweep_every = service.sweep_interval();
    let mut next_sweep = sweep_every.map(|every| Instant::now() + every);

    while !shutdown {
        let now = Instant::now();
        let idle_left = IDLE_TIMEOUT
            .checked_sub(now.duration_since(last_activity))
            .unwrap_or(Duration::ZERO);
        let timeout = match flush_at {
            Some(at) => at.saturating_duration_since(now).min(idle_left),
            None => idle_left,
        }
        .min(ROOT_POLL);
        let timeout = match next_sweep {
            Some(at) => at.saturating_duration_since(now).min(timeout),
            None => timeout,
        };

        match rx.recv_timeout(timeout.max(Duration::from_millis(10))) {
            Ok(Msg::Conn(stream)) => {
                last_activity = Instant::now();
                handle_conn(&mut service, stream, &mut shutdown);
            }
            Ok(Msg::Fs(ev)) => {
                record_event(&ev, &mut pending);
                if !pending.is_empty() {
                    flush_at = Some(Instant::now() + DEBOUNCE);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }

        if let Some(at) = flush_at
            && Instant::now() >= at
        {
            let batch: Vec<(PathBuf, bool)> = std::mem::take(&mut pending).into_iter().collect();
            service.apply_changes(&batch);
            flush_at = None;
        }

        if let (Some(at), Some(every)) = (next_sweep, sweep_every)
            && Instant::now() >= at
        {
            service.sweep();
            next_sweep = Some(Instant::now() + every);
        }

        if last_activity.elapsed() >= IDLE_TIMEOUT {
            eprintln!("pixel daemon: idle timeout, exiting");
            break;
        }
        if root_removed(&root) {
            eprintln!(
                "pixel daemon: root {} no longer exists, exiting",
                root.display()
            );
            break;
        }
    }

    let _ = std::fs::remove_file(&sock);
    let _ = std::fs::remove_file(pid_path(&root));
    let _ = std::fs::remove_file(&lock_path);
    Ok(())
}

/// The served root has been deleted (or replaced by a non-directory): every
/// answer the daemon could give from here on would describe a tree that no
/// longer exists, so the loop exits and releases the socket, pid and lock.
fn root_removed(root: &Path) -> bool {
    !root.is_dir()
}

fn record_event(ev: &notify::Event, pending: &mut BTreeMap<PathBuf, bool>) {
    for path in &ev.paths {
        if path.components().any(|c| match c {
            Component::Normal(s) => IGNORED_DIRS.iter().any(|d| s == *d),
            _ => false,
        }) {
            continue;
        }
        if path.is_dir() {
            continue;
        }
        let removed = matches!(ev.kind, notify::EventKind::Remove(_)) || !path.exists();
        // A later create/modify wins over an earlier remove and vice versa.
        pending.insert(path.clone(), removed);
    }
}

fn handle_conn(service: &mut dyn Corpus, stream: UnixStream, shutdown: &mut bool) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });
    let mut writer = stream;
    let mut line = String::new();
    let mut request_count: u32 = 0;
    let deadline = Instant::now() + CONNECTION_DEADLINE;
    loop {
        // Cap requests per connection to prevent starvation.
        if request_count >= MAX_REQUESTS_PER_CONN {
            let resp = failure_response("error", "request limit exceeded");
            let _ = writer.write_all(serde_json::to_vec(&resp).unwrap_or_default().as_slice());
            let _ = writer.write_all(b"\n");
            break;
        }
        line.clear();
        // Cap line length: read in chunks and abort if the line exceeds the
        // limit, so a multi-GB line cannot exhaust memory.
        match read_capped_line(&mut reader, &mut line, MAX_REQUEST_LINE, deadline) {
            ReadResult::Ok => {}
            ReadResult::Eof => break,
            ReadResult::TooLong => {
                let resp = failure_response("error", "request line too long");
                let _ = writer.write_all(serde_json::to_vec(&resp).unwrap_or_default().as_slice());
                let _ = writer.write_all(b"\n");
                break;
            }
            ReadResult::InvalidUtf8 => {
                request_count += 1;
                let resp = failure_response("error", "request is not valid UTF-8");
                let _ = writer.write_all(serde_json::to_vec(&resp).unwrap_or_default().as_slice());
                let _ = writer.write_all(b"\n");
                continue;
            }
            ReadResult::TimedOut | ReadResult::Err => break,
        }
        request_count += 1;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let (resp, is_shutdown) = match serde_json::from_str::<Request>(trimmed) {
            Ok(req) => {
                let is_shutdown = matches!(req, Request::Shutdown);
                (service.handle(req), is_shutdown)
            }
            Err(e) => (
                failure_response("error", format!("bad request: {e}")),
                false,
            ),
        };
        let out = serde_json::to_string(&resp).unwrap_or_else(|_| {
            // Fallback: a minimal failure envelope if serialization itself
            // fails (should never happen for a Value-typed envelope).
            r#"{"ok":false,"op":"error","protocol":1,"error":{"code":"INVARIANT_VIOLATION","message":"serialize failure"}}"#
                .to_string()
        });
        let mut out = out;
        out.push('\n');
        if writer.write_all(out.as_bytes()).is_err() || writer.flush().is_err() {
            break;
        }
        if is_shutdown {
            *shutdown = true;
            break;
        }
    }
}

enum ReadResult {
    Ok,
    Eof,
    TooLong,
    InvalidUtf8,
    TimedOut,
    Err,
}

/// Read one line into `buf`, returning `TooLong` if it exceeds `max_bytes`
/// before a newline is found. The trailing newline is consumed but not
/// included in `buf` (same semantics as `read_line` minus the newline).
fn read_capped_line(
    reader: &mut BufReader<std::os::unix::net::UnixStream>,
    buf: &mut String,
    max_bytes: usize,
    deadline: Instant,
) -> ReadResult {
    use std::io::Read;
    let mut bytes = Vec::with_capacity(max_bytes.min(4096));
    let mut byte = [0u8; 1];
    loop {
        if Instant::now() >= deadline {
            return ReadResult::TimedOut;
        }
        match reader.read(&mut byte) {
            Ok(0) => {
                if bytes.is_empty() {
                    return ReadResult::Eof;
                }
                break;
            }
            Ok(_) => {
                if byte[0] == b'\n' {
                    break;
                }
                bytes.push(byte[0]);
                if bytes.len() > max_bytes {
                    return ReadResult::TooLong;
                }
            }
            Err(_) => return ReadResult::Err,
        }
    }
    match String::from_utf8(bytes) {
        Ok(line) => {
            *buf = line;
            ReadResult::Ok
        }
        Err(_) => ReadResult::InvalidUtf8,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A corpus with no index behind it: enough to drive `run_corpus`'s
    /// transport loop from a test.
    struct StubCorpus(PathBuf);

    impl Corpus for StubCorpus {
        fn root(&self) -> &Path {
            &self.0
        }
        fn handle(&mut self, _req: Request) -> Response {
            failure_response("stub", "stub corpus")
        }
        fn apply_change(&mut self, _abs: &Path, _removed: bool) {}
    }

    /// A corpus that asks for a periodic sweep and counts the calls.
    struct SweptCorpus {
        root: PathBuf,
        every: Option<Duration>,
        sweeps: Arc<AtomicUsize>,
    }

    impl Corpus for SweptCorpus {
        fn root(&self) -> &Path {
            &self.root
        }
        fn handle(&mut self, _req: Request) -> Response {
            failure_response("stub", "stub corpus")
        }
        fn apply_change(&mut self, _abs: &Path, _removed: bool) {}
        fn sweep_interval(&self) -> Option<Duration> {
            self.every
        }
        fn sweep(&mut self) {
            self.sweeps.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn scratch_root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("pixel-daemon-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root.canonicalize().unwrap()
    }

    fn shutdown(sock: &Path) {
        let Ok(mut stream) = UnixStream::connect(sock) else {
            return;
        };
        let mut line = serde_json::to_string(&Request::Shutdown).unwrap();
        line.push('\n');
        let _ = stream.write_all(line.as_bytes());
        let mut reader = BufReader::new(stream);
        let mut reply = String::new();
        let _ = std::io::BufRead::read_line(&mut reader, &mut reply);
    }

    /// The watcher alone misses the transcript an agent is streaming (macOS
    /// FSEvents defers the modify event while the file stays open), so a
    /// corpus that asks for a sweep must get it on its interval with no
    /// filesystem event and no request in between.
    #[test]
    fn corpus_sweep_runs_on_its_interval_without_events() {
        let root = scratch_root("sweep");
        let sock = socket_path(&root);
        let sweeps = Arc::new(AtomicUsize::new(0));
        let corpus = SweptCorpus {
            root: root.clone(),
            every: Some(Duration::from_millis(100)),
            sweeps: Arc::clone(&sweeps),
        };
        let started = Instant::now();
        let daemon = std::thread::spawn(move || run_corpus(corpus));
        wait_until("socket to answer", Duration::from_secs(10), || ping(&sock));

        wait_until("three sweeps", Duration::from_secs(10), || {
            sweeps.load(Ordering::SeqCst) >= 3
        });
        // Three sweeps at 100 ms cannot land before 200 ms: the loop
        // reschedules from the interval, it does not sweep every iteration.
        assert!(
            started.elapsed() >= Duration::from_millis(200),
            "sweeps ran back to back instead of on the interval"
        );
        // The count keeps rising: sweeps are periodic, not a one-off after
        // the first request.
        let seen = sweeps.load(Ordering::SeqCst);
        wait_until("a further sweep", Duration::from_secs(10), || {
            sweeps.load(Ordering::SeqCst) > seen
        });

        shutdown(&sock);
        wait_until("daemon to exit", Duration::from_secs(10), || {
            daemon.is_finished()
        });
        daemon.join().unwrap().unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The trait default is "no sweep": the repo corpus relies on it, and a
    /// default interval would make every repo daemon run periodic work.
    #[test]
    fn default_corpus_has_no_sweep_interval() {
        let stub = StubCorpus(PathBuf::from("/nonexistent"));
        assert_eq!(stub.sweep_interval(), None);
    }

    /// A corpus without an interval is never swept, so a corpus without
    /// periodic work pays nothing.
    #[test]
    fn corpus_without_interval_is_never_swept() {
        let root = scratch_root("nosweep");
        let sock = socket_path(&root);
        let sweeps = Arc::new(AtomicUsize::new(0));
        let corpus = SweptCorpus {
            root: root.clone(),
            every: None,
            sweeps: Arc::clone(&sweeps),
        };
        let daemon = std::thread::spawn(move || run_corpus(corpus));
        wait_until("socket to answer", Duration::from_secs(10), || ping(&sock));
        std::thread::sleep(Duration::from_millis(400));
        assert!(ping(&sock));
        assert_eq!(sweeps.load(Ordering::SeqCst), 0);

        shutdown(&sock);
        wait_until("daemon to exit", Duration::from_secs(10), || {
            daemon.is_finished()
        });
        daemon.join().unwrap().unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    fn wait_until(what: &str, limit: Duration, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + limit;
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// One round trip on the daemon socket: forces a loop iteration and
    /// proves the daemon is serving.
    fn ping(sock: &Path) -> bool {
        let Ok(mut stream) = UnixStream::connect(sock) else {
            return false;
        };
        let mut line = serde_json::to_string(&Request::Ping).unwrap();
        line.push('\n');
        if stream.write_all(line.as_bytes()).is_err() {
            return false;
        }
        let mut reader = BufReader::new(stream);
        let mut reply = String::new();
        std::io::BufRead::read_line(&mut reader, &mut reply).is_ok() && !reply.is_empty()
    }

    /// An auto-started daemon is detached from its parent and would otherwise
    /// live the full idle timeout after its root is deleted: a test suite
    /// that runs the CLI against throwaway fixtures left one daemon per
    /// fixture behind (18 per run, load average past 70 after a few runs).
    #[test]
    fn daemon_exits_once_its_root_is_deleted() {
        let root = std::env::temp_dir().join(format!(
            "pixel-daemon-root-gone-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let sock = socket_path(&root);
        let served = root.clone();
        let daemon = std::thread::spawn(move || run_corpus(StubCorpus(served)));

        wait_until("socket to answer", Duration::from_secs(10), || ping(&sock));
        // The first request ran one loop iteration with the root present; a
        // daemon that exits on that iteration has released its socket by the
        // time of the second request. The exit below is therefore tied to
        // the removal, not to the check firing unconditionally.
        std::thread::sleep(Duration::from_millis(300));
        assert!(ping(&sock), "daemon stopped serving while its root existed");
        assert!(
            !daemon.is_finished(),
            "daemon exited while its root existed"
        );

        std::fs::remove_dir_all(&root).unwrap();
        wait_until(
            "daemon to exit after root removal",
            ROOT_POLL + Duration::from_secs(10),
            || daemon.is_finished(),
        );
        daemon.join().unwrap().unwrap();
        assert!(!sock.exists(), "socket must be released on exit");
        assert!(
            !pid_path(&root).exists(),
            "pid file must be released on exit"
        );
    }

    #[test]
    fn capped_line_preserves_utf8() {
        let (mut writer, reader) = UnixStream::pair().unwrap();
        writer.write_all("📋\n".as_bytes()).unwrap();
        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        assert!(matches!(
            read_capped_line(
                &mut reader,
                &mut line,
                16,
                Instant::now() + Duration::from_secs(1)
            ),
            ReadResult::Ok
        ));
        assert_eq!(line, "📋");
    }

    #[test]
    fn oversized_line_is_rejected_without_unbounded_drain() {
        let (mut writer, reader) = UnixStream::pair().unwrap();
        writer.write_all(b"0123456789\nnext\n").unwrap();
        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        assert!(matches!(
            read_capped_line(
                &mut reader,
                &mut line,
                4,
                Instant::now() + Duration::from_secs(1)
            ),
            ReadResult::TooLong
        ));
    }

    #[test]
    fn expired_connection_deadline_stops_frame_read() {
        let (_writer, reader) = UnixStream::pair().unwrap();
        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        assert!(matches!(
            read_capped_line(&mut reader, &mut line, 4, Instant::now()),
            ReadResult::TimedOut
        ));
    }
}

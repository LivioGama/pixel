//! Bounded JSONL evidence bridge for interactive harnesses.
//!
//! The normal daemon protocol remains request/response. This bridge is a
//! long-lived stdio adapter that correlates concurrent bundle requests, emits
//! useful lexical/structural evidence before completion, and never permits a
//! slow client to consume more than its small in-flight allotment.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::Service;

pub const EVIDENCE_PROTOCOL_VERSION: u64 = 1;
const MAX_IN_FLIGHT: usize = 4;
const MAX_QUERIES_PER_BUNDLE: usize = 8;
const MAX_QUERY_LIMIT: usize = 32;

#[derive(Debug, Deserialize)]
struct Request {
    op: String,
    version: u64,
    #[serde(rename = "requestId")]
    request_id: String,
    generation: Option<u64>,
    #[serde(rename = "deadlineMs")]
    deadline_ms: Option<u64>,
    queries: Option<Vec<Query>>,
    #[serde(rename = "targetRequestId")]
    target_request_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Query {
    id: String,
    kind: String,
    query: String,
    limit: Option<usize>,
}

fn frame(request_id: &str, kind: &str, body: Value) -> Value {
    let mut frame = json!({
        "version": EVIDENCE_PROTOCOL_VERSION,
        "requestId": request_id,
        "type": kind,
    });
    if let Some(map) = frame.as_object_mut()
        && let Some(body) = body.as_object()
    {
        map.extend(body.clone());
    }
    frame
}

fn write_frame<W: Write>(writer: &Arc<Mutex<W>>, value: Value) -> Result<(), String> {
    let encoded = serde_json::to_string(&value).map_err(|error| error.to_string())?;
    let mut writer = writer
        .lock()
        .map_err(|_| "evidence stdout lock poisoned".to_string())?;
    writer
        .write_all(encoded.as_bytes())
        .and_then(|_| writer.write_all(b"\n"))
        .and_then(|_| writer.flush())
        .map_err(|error| format!("evidence stdout: {error}"))
}

fn capabilities(request_id: &str) -> Value {
    frame(
        request_id,
        "capabilities",
        json!({
            "protocolVersions": [EVIDENCE_PROTOCOL_VERSION],
            "features": ["bundle", "partial_results", "cancellation", "deadlines", "bounded_queue"],
            "maxInFlight": MAX_IN_FLIGHT,
        }),
    )
}

fn complete(request_id: &str, generation: Option<u64>, status: &str, error: Option<&str>) -> Value {
    let mut body = json!({ "status": status });
    if let Some(generation) = generation {
        body["generation"] = json!(generation);
    }
    if let Some(error) = error {
        body["error"] = json!(error);
    }
    frame(request_id, "complete", body)
}

fn interrupted_status(
    cancelled: &AtomicBool,
    started: Instant,
    deadline_ms: Option<u64>,
) -> Option<&'static str> {
    if cancelled.load(Ordering::Acquire) {
        return Some("cancelled");
    }
    if deadline_ms.is_some_and(|deadline| started.elapsed().as_millis() >= u128::from(deadline)) {
        return Some("deadline");
    }
    None
}

fn register_active(
    active: &Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
    request_id: &str,
    cancelled: Arc<AtomicBool>,
) -> Result<bool, String> {
    let mut active = active
        .lock()
        .map_err(|_| "evidence active lock poisoned".to_string())?;
    if active.contains_key(request_id) {
        return Ok(false);
    }
    active.insert(request_id.to_string(), cancelled);
    Ok(true)
}

fn reap_finished_workers(workers: &mut Vec<JoinHandle<()>>) {
    let mut pending = Vec::with_capacity(workers.len());
    for worker in std::mem::take(workers) {
        if worker.is_finished() {
            let _ = worker.join();
        } else {
            pending.push(worker);
        }
    }
    *workers = pending;
}

fn run_bundle<W: Write + Send + 'static>(
    service_slot: Arc<Mutex<Option<Arc<Mutex<Service>>>>>,
    root: std::path::PathBuf,
    writer: Arc<Mutex<W>>,
    active: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
    in_flight: Arc<AtomicUsize>,
    request: Request,
    cancelled: Arc<AtomicBool>,
    started: Instant,
) {
    let request_id = request.request_id.clone();
    let result = (|| -> Result<&'static str, String> {
        let queries = request
            .queries
            .ok_or_else(|| "bundle requires queries".to_string())?;
        if queries.is_empty() || queries.len() > MAX_QUERIES_PER_BUNDLE {
            return Err(format!(
                "bundle requires 1..={MAX_QUERIES_PER_BUNDLE} queries"
            ));
        }
        if let Some(status) = interrupted_status(&cancelled, started, request.deadline_ms) {
            return Ok(status);
        }
        // The clone is quick and shares the indexed text generation, but owns
        // its SQLite connection: readers never hold the master service lock.
        let service = {
            let mut slot = service_slot
                .lock()
                .map_err(|_| "evidence service slot lock poisoned".to_string())?;
            if slot.is_none() {
                *slot = Some(Arc::new(Mutex::new(
                    Service::open(&root).map_err(|error| error.to_string())?,
                )));
            }
            Arc::clone(slot.as_ref().expect("service initialized above"))
        };
        let mut reader = service
            .lock()
            .map_err(|_| "evidence service lock poisoned".to_string())?
            .read_replica();
        for query in queries {
            if let Some(status) = interrupted_status(&cancelled, started, request.deadline_ms) {
                return Ok(status);
            }
            let limit = query.limit.unwrap_or(8).clamp(1, MAX_QUERY_LIMIT);
            let (publication, result) = reader.read_evidence(&query.kind, &query.query, limit);
            if let Some(status) = interrupted_status(&cancelled, started, request.deadline_ms) {
                return Ok(status);
            }
            let mut body = json!({
                "generation": request.generation,
                "indexGeneration": publication.generation,
                "queryId": query.id,
            });
            match result {
                Ok(result) => body["result"] = result,
                Err(error) => body["error"] = json!(error),
            }
            write_frame(&writer, frame(&request_id, "partial", body))?;
        }
        Ok("complete")
    })();
    let (status, error) = match result {
        Ok(status) => (status, None),
        Err(error) => ("error", Some(error)),
    };
    let _ = write_frame(
        &writer,
        complete(&request_id, request.generation, status, error.as_deref()),
    );
    in_flight.fetch_sub(1, Ordering::AcqRel);
    if let Ok(mut active) = active.lock() {
        active.remove(&request_id);
    }
}

/// Serve the evidence JSONL protocol. EOF waits for accepted work to produce
/// its terminal frames, so a short-lived caller does not silently lose output.
pub fn serve<R: BufRead, W: Write + Send + 'static>(
    root: &Path,
    reader: R,
    writer: W,
) -> Result<(), String> {
    // Capability negotiation must be independent of index construction: Pi
    // gives this bridge only 500ms to answer before it takes the legacy path.
    let service_slot = Arc::new(Mutex::new(None));
    let root = root.to_path_buf();
    let writer = Arc::new(Mutex::new(writer));
    let active = Arc::new(Mutex::new(HashMap::<String, Arc<AtomicBool>>::new()));
    let in_flight = Arc::new(AtomicUsize::new(0));
    let mut workers: Vec<JoinHandle<()>> = Vec::new();

    for line in reader.lines() {
        reap_finished_workers(&mut workers);
        let line = line.map_err(|error| format!("evidence stdin: {error}"))?;
        if line.len() > 65_536 {
            return Err("evidence request exceeds 64KiB".into());
        }
        let request: Request = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(error) => {
                let _ = write_frame(
                    &writer,
                    frame(
                        "unknown",
                        "complete",
                        json!({"status":"error", "error": format!("invalid request: {error}")}),
                    ),
                );
                continue;
            }
        };
        if request.version != EVIDENCE_PROTOCOL_VERSION {
            let _ = write_frame(
                &writer,
                complete(
                    &request.request_id,
                    request.generation,
                    "error",
                    Some("unsupported protocol version"),
                ),
            );
            continue;
        }
        match request.op.as_str() {
            "capabilities" => write_frame(&writer, capabilities(&request.request_id))?,
            "cancel" => {
                if let Some(target) = request.target_request_id.as_deref()
                    && let Ok(active) = active.lock()
                    && let Some(cancelled) = active.get(target)
                {
                    cancelled.store(true, Ordering::Release);
                }
                write_frame(
                    &writer,
                    complete(&request.request_id, request.generation, "complete", None),
                )?;
            }
            "bundle" => {
                if in_flight.fetch_add(1, Ordering::AcqRel) >= MAX_IN_FLIGHT {
                    in_flight.fetch_sub(1, Ordering::AcqRel);
                    write_frame(
                        &writer,
                        complete(
                            &request.request_id,
                            request.generation,
                            "backpressure",
                            Some("evidence queue is full"),
                        ),
                    )?;
                    continue;
                }
                let cancelled = Arc::new(AtomicBool::new(false));
                if !register_active(&active, &request.request_id, Arc::clone(&cancelled))? {
                    in_flight.fetch_sub(1, Ordering::AcqRel);
                    write_frame(
                        &writer,
                        complete(
                            &request.request_id,
                            request.generation,
                            "error",
                            Some("duplicate active evidence requestId"),
                        ),
                    )?;
                    continue;
                }
                let started = Instant::now();
                workers.push(std::thread::spawn({
                    let service_slot = Arc::clone(&service_slot);
                    let root = root.clone();
                    let writer = Arc::clone(&writer);
                    let active = Arc::clone(&active);
                    let in_flight = Arc::clone(&in_flight);
                    move || {
                        run_bundle(
                            service_slot,
                            root,
                            writer,
                            active,
                            in_flight,
                            request,
                            cancelled,
                            started,
                        )
                    }
                }));
            }
            _ => write_frame(
                &writer,
                complete(
                    &request.request_id,
                    request.generation,
                    "error",
                    Some("unsupported evidence operation"),
                ),
            )?,
        }
    }
    for worker in workers {
        let _ = worker.join();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[derive(Clone)]
    struct SharedBuffer(Arc<Mutex<Vec<u8>>>);
    impl Write for SharedBuffer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn capability_frame_advertises_the_client_required_features() {
        let value = capabilities("cap-1");
        assert_eq!(value["version"], EVIDENCE_PROTOCOL_VERSION);
        assert_eq!(value["requestId"], "cap-1");
        assert_eq!(value["type"], "capabilities");
        assert!(
            value["features"]
                .as_array()
                .unwrap()
                .iter()
                .any(|feature| feature == "bundle")
        );
        assert!(
            value["features"]
                .as_array()
                .unwrap()
                .iter()
                .any(|feature| feature == "partial_results")
        );
        assert!(
            value["features"]
                .as_array()
                .unwrap()
                .iter()
                .any(|feature| feature == "cancellation")
        );
        assert!(
            value["features"]
                .as_array()
                .unwrap()
                .iter()
                .any(|feature| feature == "deadlines")
        );
        assert!(
            value["features"]
                .as_array()
                .unwrap()
                .iter()
                .any(|feature| feature == "bounded_queue")
        );
    }

    #[test]
    fn capability_handshake_does_not_open_the_repository_service() {
        let input = b"{\"op\":\"capabilities\",\"version\":1,\"requestId\":\"cap-1\"}\n";
        let output = Arc::new(Mutex::new(Vec::new()));
        serve(
            Path::new("/definitely-not-an-indexed-repository"),
            std::io::Cursor::new(input),
            SharedBuffer(Arc::clone(&output)),
        )
        .unwrap();
        let value: Value = serde_json::from_slice(&output.lock().unwrap()).unwrap();
        assert_eq!(value["type"], "capabilities");
    }

    #[test]
    fn interruption_checks_cancel_and_deadline_before_read_work() {
        let cancelled = AtomicBool::new(false);
        let started = Instant::now() - std::time::Duration::from_millis(1);
        assert_eq!(
            interrupted_status(&cancelled, started, Some(0)),
            Some("deadline")
        );

        cancelled.store(true, Ordering::Release);
        assert_eq!(
            interrupted_status(&cancelled, Instant::now(), None),
            Some("cancelled")
        );
    }

    #[test]
    fn active_request_ids_cannot_replace_a_cancellation_handle() {
        let active = Arc::new(Mutex::new(HashMap::new()));
        let first = Arc::new(AtomicBool::new(false));
        assert!(register_active(&active, "bundle-1", Arc::clone(&first)).unwrap());
        assert!(!register_active(&active, "bundle-1", Arc::new(AtomicBool::new(false))).unwrap());

        active.lock().unwrap()["bundle-1"].store(true, Ordering::Release);
        assert!(first.load(Ordering::Acquire));
    }

    #[test]
    fn completed_workers_are_reaped_before_accepting_more_requests() {
        let mut workers = vec![std::thread::spawn(|| {})];
        while !workers[0].is_finished() {
            std::thread::yield_now();
        }
        reap_finished_workers(&mut workers);
        assert!(workers.is_empty());
    }
}

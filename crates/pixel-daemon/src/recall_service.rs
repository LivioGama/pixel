//! The transcript-corpus daemon service: watches every CLI's transcript
//! store, ingests changes incrementally, keeps the embedding model warm,
//! and serves `search` / `ask` over the standard daemon transport.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use pixel_recall::ask::{ask, format_group};
use pixel_recall::embed::{Embedder, open_default_embedder, run_backfill};
use pixel_recall::ingest::{IngestReport, ingest_recent, ingest_source};
use pixel_recall::search::{SearchFilters, format_hit, search};
use pixel_recall::segment::SegmentSet;
use pixel_recall::sources::SourceAdapter;
use pixel_recall::store::RecallStore;
use pixel_recall::vector::VectorStore;
use serde_json::{Value, json};

/// The embed backlog the daemon drains inline. The daemon loop is
/// single-threaded, and a bulk backfill would block the socket for minutes
/// (that is `pixel recall embed`'s job).
const MAX_INLINE_BACKLOG: i64 = 5_000;
/// How often the daemon re-stats the recently modified transcripts
/// independently of the watcher. FSEvents on macOS holds back the modify
/// event of a file while its writer keeps it open, and an agent streams
/// its transcript exactly that way: the session being written right now
/// is the one the watcher does not report. The sweep is one directory
/// walk per source; unchanged files are skipped on size and mtime.
const SWEEP_INTERVAL: Duration = Duration::from_secs(5);
/// Only transcripts modified this recently are re-stated by the sweep;
/// anything older is either already ingested or will arrive through the
/// watcher when its writer closes it.
const SWEEP_WINDOW_MS: i64 = 21_600_000; // 6 h

use crate::api::{PROTOCOL_VERSION, Request, Response, ServeError, failure_response};
use crate::daemon::Corpus;
use pixel_proto::Envelope;

/// One transcript store the daemon serves: the directory whose changes
/// mean "new transcript content" and the adapter that parses it.
pub struct RecallSource {
    pub root: PathBuf,
    pub adapter: Box<dyn SourceAdapter>,
}

pub struct RecallService {
    root: PathBuf,
    store: RecallStore,
    segments_dir: PathBuf,
    vectors_dir: PathBuf,
    sources: Vec<RecallSource>,
    /// Lazily opened on first ask; kept warm for the daemon's lifetime.
    embedder: Option<Box<dyn Embedder>>,
    embedder_unavailable: bool,
}

/// Every transcript store this machine's agents write, under `HOME`.
fn machine_sources() -> Vec<RecallSource> {
    use pixel_recall::sources::*;
    let home = std::env::var("HOME").unwrap_or_default();
    let h = |suffix: &str| PathBuf::from(&home).join(suffix);
    vec![
        RecallSource {
            root: h(".claude/projects"),
            adapter: Box::new(claude::ClaudeAdapter::new()),
        },
        RecallSource {
            root: h(".codex/sessions"),
            adapter: Box::new(codex::Adapter::new()),
        },
        RecallSource {
            root: h(".cursor/projects"),
            adapter: Box::new(cursor::Adapter::new()),
        },
        RecallSource {
            root: h(".gemini/antigravity-cli"),
            adapter: Box::new(gemini::Adapter::new()),
        },
        RecallSource {
            root: h(".local/share/opencode"),
            adapter: Box::new(opencode::Adapter::new()),
        },
        RecallSource {
            root: h(".zcode/cli/db"),
            adapter: Box::new(zcode::Adapter::new()),
        },
        RecallSource {
            root: h(".local/share/devin/cli"),
            adapter: Box::new(devin::Adapter::new()),
        },
    ]
}

impl RecallService {
    pub fn open() -> Result<Self, ServeError> {
        let root = pixel_recall::ensure_recall_dir()
            .map_err(|e| ServeError::Msg(format!("recall dir: {e}")))?;
        let store = RecallStore::open(&pixel_recall::db_path())
            .map_err(|e| ServeError::Msg(format!("recall.db: {e}")))?;
        Ok(Self {
            root,
            store,
            segments_dir: pixel_recall::segments_dir(),
            vectors_dir: pixel_recall::vectors_dir(),
            sources: machine_sources(),
            embedder: None,
            embedder_unavailable: false,
        })
    }

    /// A service over explicit directories and sources, no `HOME` and no
    /// embedding model: the seam the tests drive `sweep` through.
    #[cfg(test)]
    fn with_sources(
        root: PathBuf,
        store: RecallStore,
        segments_dir: PathBuf,
        vectors_dir: PathBuf,
        sources: Vec<RecallSource>,
    ) -> Self {
        Self {
            root,
            store,
            segments_dir,
            vectors_dir,
            sources,
            embedder: None,
            embedder_unavailable: true,
        }
    }

    /// Lazy-load the model once; afterwards `self.embedder` stays warm.
    fn ensure_embedder(&mut self) {
        if self.embedder.is_none() && !self.embedder_unavailable {
            match open_default_embedder(false) {
                Ok(e) => self.embedder = Some(e),
                Err(_) => self.embedder_unavailable = true,
            }
        }
    }

    fn op(&mut self, action: &str, params: Value) -> Result<Value, String> {
        match action {
            "search" => {
                let pattern = params
                    .get("pattern")
                    .and_then(Value::as_str)
                    .ok_or("missing pattern")?
                    .to_string();
                let word = params.get("word").and_then(Value::as_bool).unwrap_or(false);
                let limit = params
                    .get("limit")
                    .and_then(Value::as_u64)
                    .unwrap_or(20)
                    .min(200) as usize;
                let offset = params.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
                let filters: SearchFilters = params
                    .get("filters")
                    .cloned()
                    .map(serde_json::from_value)
                    .transpose()
                    .map_err(|e| format!("bad filters: {e}"))?
                    .unwrap_or_default();
                let segments = SegmentSet::open(&self.segments_dir)?;
                let result = search(
                    &self.store,
                    &segments,
                    &pattern,
                    word,
                    &filters,
                    offset,
                    limit,
                )?;
                let mut text = String::new();
                if result.hits.is_empty() {
                    text.push_str(&format!(
                        "no matches ({} turns considered) — the pattern does not appear in the indexed corpus\n",
                        result.turns_considered
                    ));
                } else {
                    for h in &result.hits {
                        text.push_str(&format_hit(h));
                        text.push('\n');
                    }
                    if result.truncated {
                        text.push_str(&format!(
                            "(showing {} — more matches exist, use --offset {} or narrow the query)\n",
                            result.hits.len(),
                            offset + result.hits.len()
                        ));
                    }
                }
                Ok(json!({
                    "text": text,
                    "json": {
                        "hits": result.hits,
                        "turns_considered": result.turns_considered,
                        "truncated": result.truncated,
                    },
                }))
            }
            "ask" => {
                let query = params
                    .get("query")
                    .and_then(Value::as_str)
                    .ok_or("missing query")?
                    .to_string();
                let k = params
                    .get("k")
                    .and_then(Value::as_u64)
                    .unwrap_or(10)
                    .clamp(1, 50) as usize;
                let lexical_only = params
                    .get("lexical_only")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let filters: SearchFilters = params
                    .get("filters")
                    .cloned()
                    .map(serde_json::from_value)
                    .transpose()
                    .map_err(|e| format!("bad filters: {e}"))?
                    .unwrap_or_default();
                let segments = SegmentSet::open(&self.segments_dir)?;
                let vectors = VectorStore::open(&self.vectors_dir)?;
                if !lexical_only {
                    self.ensure_embedder();
                }
                let embedder = if lexical_only {
                    None
                } else {
                    self.embedder.as_deref_mut()
                };
                let result = ask(
                    &self.store,
                    &segments,
                    &vectors,
                    embedder,
                    &query,
                    &filters,
                    k,
                )?;
                let mut text = String::new();
                if let Some(n) = &result.notice {
                    text.push_str(&format!("note: {n}\n"));
                }
                if result.groups.is_empty() {
                    text.push_str(
                        "no matching sessions — nothing in the corpus resembles that query\n",
                    );
                }
                for g in &result.groups {
                    text.push_str(&format_group(g));
                    text.push('\n');
                }
                Ok(json!({
                    "text": text,
                    "json": {
                        "groups": result.groups.iter().map(|g| json!({
                            "session_id": g.best.session_id,
                            "agent": g.best.agent,
                            "source_session_id": g.best.source_session_id,
                            "title": g.session_title,
                            "cwd": g.best.cwd,
                            "turn_id": g.best.turn_id,
                            "seq": g.best.seq,
                            "ts": g.best.ts,
                            "ts_source": g.best.ts_source,
                            "snippet": g.best.snippet,
                            "score": g.best.score,
                            "matched_lexical": g.best.matched_lexical,
                            "matched_semantic": g.best.matched_semantic,
                            "extra_hits": g.extra_hits,
                        })).collect::<Vec<_>>(),
                        "notice": result.notice,
                    },
                }))
            }
            other => Err(format!("unknown recall action '{other}'")),
        }
    }

    /// Incrementally ingest the agents whose stores changed, refresh the
    /// lexical segments, and drain the embed backlog while the model is
    /// warm. Best-effort: watcher-driven maintenance must never kill the
    /// daemon.
    // Watcher-driven glue over this machine's real agent stores (HOME);
    // `ingest_source`, the adapters, segments and backfill are unit-tested
    // in pixel-recall.
    #[cfg_attr(test, mutants::skip)]
    fn refresh_agents(&mut self, agents: &std::collections::BTreeSet<&'static str>) {
        for source in &self.sources {
            if !agents.contains(source.adapter.agent()) {
                continue;
            }
            let report = ingest_source(&mut self.store, source.adapter.as_ref());
            Self::log_ingest(source.adapter.agent(), report);
        }
        self.after_ingest();
    }

    /// The periodic sweep: re-stat the transcripts modified within
    /// `SWEEP_WINDOW_MS` for every source present on this machine and
    /// ingest the ones that grew, whether or not the watcher reported them.
    fn sweep_recent(&mut self) {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as i64);
        for source in &self.sources {
            if !source.root.exists() {
                continue;
            }
            let report = ingest_recent(
                &mut self.store,
                source.adapter.as_ref(),
                now_ms,
                SWEEP_WINDOW_MS,
            );
            Self::log_ingest(source.adapter.agent(), report);
        }
        self.after_ingest();
    }

    #[cfg_attr(test, mutants::skip)] // stderr diagnostics only
    fn log_ingest(agent: &str, report: Result<IngestReport, pixel_recall::sources::IngestError>) {
        match report {
            Ok(report) => {
                if report.sessions_written > 0 {
                    eprintln!(
                        "recall daemon: {} +{} sessions, +{} turns",
                        report.agent, report.sessions_written, report.turns_written
                    );
                }
            }
            Err(e) => eprintln!("recall daemon: ingest {agent}: {e}"),
        }
    }

    /// Refresh the lexical segments and drain a small embed backlog after
    /// any ingest pass.
    // Glue over the real segments and vectors directories; both are
    // unit-tested in pixel-recall.
    #[cfg_attr(test, mutants::skip)]
    fn after_ingest(&mut self) {
        match SegmentSet::open(&self.segments_dir) {
            Ok(mut segments) => {
                if let Err(e) = segments.index_new(&self.store) {
                    eprintln!("recall daemon: segment index: {e}");
                }
            }
            Err(e) => eprintln!("recall daemon: segments: {e}"),
        }
        // Drain the embed backlog only when it is small (see
        // `MAX_INLINE_BACKLOG`).
        match self.store.embed_backlog() {
            Ok(backlog) if backlog > 0 && backlog <= MAX_INLINE_BACKLOG => {
                self.ensure_embedder();
                if let Some(embedder) = self.embedder.as_deref_mut() {
                    let mut vectors = match VectorStore::open(&self.vectors_dir) {
                        Ok(v) => v,
                        Err(e) => {
                            eprintln!("recall daemon: vectors: {e}");
                            return;
                        }
                    };
                    if let Err(e) = run_backfill(&self.store, &mut vectors, embedder, |_, _| {}) {
                        eprintln!("recall daemon: embed: {e}");
                    }
                }
            }
            Ok(backlog) if backlog > MAX_INLINE_BACKLOG => {
                eprintln!(
                    "recall daemon: embed backlog {backlog} exceeds inline cap — run `pixel recall embed`"
                );
            }
            _ => {}
        }
    }
}

impl Corpus for RecallService {
    fn root(&self) -> &Path {
        &self.root
    }

    fn handle(&mut self, req: Request) -> Response {
        let op_name = req.op_name();
        match req {
            Request::Ping => Envelope::success(
                op_name,
                json!({
                    "pong": true,
                    "root": self.root.display().to_string(),
                    "corpus": "recall",
                    "protocol_version": PROTOCOL_VERSION,
                }),
            ),
            Request::Shutdown => Envelope::success(op_name, json!({"shutting_down": true})),
            Request::Recall { action, params } => match self.op(&action, params) {
                Ok(data) => Envelope::success(op_name, data),
                Err(e) => failure_response(op_name, e),
            },
            _ => failure_response(
                op_name,
                "this daemon serves the transcript corpus; repository ops go to a repo daemon"
                    .to_string(),
            ),
        }
    }

    fn apply_change(&mut self, abs: &Path, _removed: bool) {
        let mut touched = std::collections::BTreeSet::new();
        for source in &self.sources {
            if abs.starts_with(&source.root) {
                touched.insert(source.adapter.agent());
            }
        }
        if !touched.is_empty() {
            self.refresh_agents(&touched);
        }
    }

    fn watch_paths(&self) -> Vec<PathBuf> {
        self.sources
            .iter()
            .map(|s| s.root.clone())
            .filter(|p| p.exists())
            .collect()
    }

    fn sweep_interval(&self) -> Option<Duration> {
        Some(SWEEP_INTERVAL)
    }

    fn sweep(&mut self) {
        self.sweep_recent();
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::SystemTime;

    use pixel_recall::sources::claude::ClaudeAdapter;

    use super::*;

    /// A recall service over a scratch corpus and one Claude store holding a
    /// single-turn session in `projects/-work-pixel/<id>.jsonl`.
    struct Fixture {
        scratch: PathBuf,
        transcript: PathBuf,
        service: RecallService,
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            let scratch = std::env::temp_dir().join(format!(
                "pixel-recall-sweep-{tag}-{}-{}",
                std::process::id(),
                line!()
            ));
            let _ = fs::remove_dir_all(&scratch);
            let projects = scratch.join("projects");
            let slug = projects.join("-work-pixel");
            fs::create_dir_all(&slug).unwrap();
            let transcript = slug.join("0123abcd-0000-4000-8000-000000000001.jsonl");
            let record = json!({
                "type": "user",
                "cwd": "/work/pixel",
                "timestamp": "2025-10-09T08:53:20.000Z",
                "message": {"content": [{"type": "text", "text": "the streamed needle"}]},
            });
            fs::write(&transcript, format!("{record}\n")).unwrap();
            let store = RecallStore::open(&scratch.join("recall.db")).unwrap();
            let service = RecallService::with_sources(
                scratch.clone(),
                store,
                scratch.join("segments"),
                scratch.join("vectors"),
                vec![RecallSource {
                    root: projects,
                    adapter: Box::new(ClaudeAdapter::with_root(
                        slug.parent().unwrap().to_path_buf(),
                    )),
                }],
            );
            Self {
                scratch,
                transcript,
                service,
            }
        }

        fn age_transcript(&self, age: Duration) {
            let f = fs::OpenOptions::new()
                .write(true)
                .open(&self.transcript)
                .unwrap();
            f.set_modified(SystemTime::now() - age).unwrap();
        }

        fn turns(&self) -> i64 {
            self.service.store.total_turns().unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.scratch);
        }
    }

    /// The reason the sweep exists: a transcript that grew without a
    /// watcher event is in the corpus after one `sweep`.
    #[test]
    fn sweep_ingests_a_transcript_modified_within_the_window() {
        let mut fx = Fixture::new("fresh");
        assert_eq!(fx.turns(), 0);
        fx.service.sweep();
        assert_eq!(
            fx.turns(),
            1,
            "the streamed turn must be ingested by the sweep"
        );
        // A second sweep re-stats the file and reparses nothing.
        fx.service.sweep();
        assert_eq!(fx.turns(), 1);
    }

    /// The window bounds the sweep's work: a transcript older than it is
    /// left to the watcher, and picked up as soon as it is touched again.
    #[test]
    fn sweep_leaves_a_transcript_older_than_the_window_to_the_watcher() {
        let mut fx = Fixture::new("stale");
        fx.age_transcript(
            Duration::from_millis(SWEEP_WINDOW_MS as u64) + Duration::from_secs(3600),
        );
        fx.service.sweep();
        assert_eq!(
            fx.turns(),
            0,
            "a transcript outside the window must not be re-stated"
        );
        fx.age_transcript(Duration::ZERO);
        fx.service.sweep();
        assert_eq!(fx.turns(), 1);
    }

    /// A source whose root is absent on this machine is skipped, not an
    /// error that stops the other sources.
    #[test]
    fn sweep_skips_sources_whose_root_is_missing() {
        let mut fx = Fixture::new("missing");
        fx.service.sources.insert(
            0,
            RecallSource {
                root: fx.scratch.join("no-such-store"),
                adapter: Box::new(ClaudeAdapter::with_root(fx.scratch.join("no-such-store"))),
            },
        );
        fx.service.sweep();
        assert_eq!(fx.turns(), 1);
    }

    /// The recall corpus opts into the sweep at the documented cadence; the
    /// repo corpus does not.
    #[test]
    fn recall_service_sweeps_every_five_seconds() {
        let fx = Fixture::new("interval");
        assert_eq!(fx.service.sweep_interval(), Some(Duration::from_secs(5)));
    }
}

//! Embedding seam: the pluggable model behind semantic search.
//!
//! The corpus side only ever sees this trait; the concrete model (fastembed
//! ONNX today) lives behind the `fastembed` cargo feature so the crate
//! builds and every lexical feature works with no ML dependency at all.

/// E5-family models embed queries and passages with different prefixes;
/// the kind travels with every batch so the impl can prepend correctly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbedKind {
    Query,
    Passage,
}

pub trait Embedder: Send {
    /// Stable id recorded in the vector store — model swaps are detected,
    /// never silently mixed.
    fn model_id(&self) -> &str;
    fn dims(&self) -> usize;
    fn embed_batch(&mut self, texts: &[&str], kind: EmbedKind) -> Result<Vec<Vec<f32>>, String>;
}

/// Chunking: turns longer than this embed as several windows.
pub const CHUNK_MAX: usize = 1500;
pub const CHUNK_OVERLAP: usize = 200;

/// Deterministic chunk windows (byte offsets, char-boundary aligned) for a
/// turn's text — reproducible at query time for snippets.
pub fn chunk_offsets(text: &str) -> Vec<(usize, usize)> {
    if text.len() <= CHUNK_MAX {
        return vec![(0, text.len())];
    }
    let mut out = Vec::new();
    let mut start = 0usize;
    loop {
        let mut end = (start + CHUNK_MAX).min(text.len());
        while end < text.len() && !text.is_char_boundary(end) {
            end += 1;
        }
        out.push((start, end));
        if end >= text.len() {
            break;
        }
        let mut next = end.saturating_sub(CHUNK_OVERLAP);
        while next > 0 && !text.is_char_boundary(next) {
            next -= 1;
        }
        // Guarantee forward progress.
        start = next.max(start + 1);
    }
    out
}

/// The text actually embedded for a chunk: a tiny context header improves
/// "which session did X" queries at negligible cost.
pub fn embed_text(agent: &str, cwd: Option<&str>, role: &str, chunk: &str) -> String {
    let repo = cwd
        .and_then(|c| c.rsplit('/').next())
        .unwrap_or("-");
    format!("[{agent}] [{repo}] {role}: {chunk}")
}

pub const POTION_MODEL_ID: &str = "potion-multilingual-128m";
pub const E5_MODEL_ID: &str = "multilingual-e5-small-q";

/// The model id the existing vector store was built with, if any.
fn stored_model_id() -> Option<String> {
    let bytes = std::fs::read(crate::vectors_dir().join("meta.json")).ok()?;
    let meta: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let id = meta.get("model_id")?.as_str()?.to_string();
    (!id.is_empty()).then_some(id)
}

/// Open the default embedding model from the shared model cache.
///
/// Resolution: `PIXEL_RECALL_MODEL` env ("potion" | "e5") → the model
/// the existing vector store was built with → potion (the fast static
/// tier; ~3 orders of magnitude faster than transformer inference, which
/// makes the full-corpus backfill minutes instead of hours). Errors when
/// the build lacks embedding support or the model is absent and
/// `download` is false — callers treat that as "semantic channel
/// unavailable", never as a crash.
pub fn open_default_embedder(download: bool) -> Result<Box<dyn Embedder>, String> {
    let choice = match std::env::var("PIXEL_RECALL_MODEL") {
        Ok(v) if !v.is_empty() => v,
        _ => stored_model_id().unwrap_or_else(|| POTION_MODEL_ID.to_string()),
    };
    if choice.contains("e5") {
        #[cfg(feature = "fastembed")]
        {
            return fast::FastEmbedder::open(&crate::models_dir(), download)
                .map(|e| Box::new(e) as Box<dyn Embedder>);
        }
        #[cfg(not(feature = "fastembed"))]
        return Err("e5 requested but this build lacks the fastembed feature".to_string());
    }
    #[cfg(feature = "model2vec")]
    {
        potion::PotionEmbedder::open(&crate::models_dir(), download)
            .map(|e| Box::new(e) as Box<dyn Embedder>)
    }
    #[cfg(not(feature = "model2vec"))]
    {
        let _ = download;
        Err("this build has no embedding support (model2vec feature disabled)".to_string())
    }
}

#[derive(Debug, Default)]
pub struct BackfillReport {
    pub turns_embedded: usize,
    pub chunks_written: usize,
    pub segments_written: usize,
    pub backlog_remaining: i64,
    pub elapsed_ms: u128,
}

const BATCH_TURNS: usize = 128;
const FLUSH_CHUNKS: usize = 8192;

/// Drain the embed backlog into vector segments. Resumable: an interrupted
/// run leaves turns unmarked and orphan chunk rows, both healed on entry.
pub fn run_backfill(
    store: &crate::store::RecallStore,
    vectors: &mut crate::vector::VectorStore,
    embedder: &mut dyn Embedder,
    mut progress: impl FnMut(usize, i64),
) -> Result<BackfillReport, String> {
    let started = std::time::Instant::now();
    let mut report = BackfillReport::default();
    store
        .drop_orphan_chunks(vectors.meta.last_chunk_id)
        .map_err(|e| e.to_string())?;
    vectors.check_model(embedder.model_id(), embedder.dims())?;

    let mut pending_rows: Vec<(i64, Vec<f32>)> = Vec::new();
    let mut pending_turns: Vec<i64> = Vec::new();
    store.mark_policy_skips().map_err(|e| e.to_string())?;
    // Keyset cursor over turn ids: rows behind it are either flushed or
    // sitting in `pending_rows` awaiting flush — never re-fetched.
    let mut after_id = 0i64;
    loop {
        let batch = store
            .pending_embed(after_id, BATCH_TURNS)
            .map_err(|e| e.to_string())?;
        if batch.is_empty() {
            // Ingest may have added policy-excluded turns concurrently;
            // sweep once more and only stop when both queues are empty.
            let swept = store.mark_policy_skips().map_err(|e| e.to_string())?;
            if swept == 0 {
                break;
            }
            continue;
        }
        after_id = batch.last().map(|t| t.turn_id).unwrap_or(after_id);
        let mut texts: Vec<String> = Vec::new();
        let mut chunk_ids: Vec<i64> = Vec::new();
        for turn in &batch {
            let offsets = chunk_offsets(&turn.text);
            let ids = store
                .insert_chunks(turn.turn_id, &offsets)
                .map_err(|e| e.to_string())?;
            for ((start, end), id) in offsets.iter().zip(&ids) {
                texts.push(embed_text(
                    &turn.agent,
                    turn.cwd.as_deref(),
                    &turn.role,
                    &turn.text[*start..*end],
                ));
                chunk_ids.push(*id);
            }
        }
        let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        let vecs = embedder.embed_batch(&refs, EmbedKind::Passage)?;
        if vecs.len() != chunk_ids.len() {
            return Err("embedding count mismatch".to_string());
        }
        pending_rows.extend(chunk_ids.into_iter().zip(vecs));
        pending_turns.extend(batch.iter().map(|t| t.turn_id));
        report.turns_embedded += batch.len();

        if pending_rows.len() >= FLUSH_CHUNKS {
            flush(store, vectors, embedder, &mut pending_rows, &mut pending_turns, &mut report)?;
            let backlog = store.embed_backlog().map_err(|e| e.to_string())?;
            progress(report.turns_embedded, backlog);
        }
    }
    if !pending_rows.is_empty() {
        flush(store, vectors, embedder, &mut pending_rows, &mut pending_turns, &mut report)?;
    }
    report.backlog_remaining = store.embed_backlog().map_err(|e| e.to_string())?;
    report.elapsed_ms = started.elapsed().as_millis();
    Ok(report)
}

fn flush(
    store: &crate::store::RecallStore,
    vectors: &mut crate::vector::VectorStore,
    embedder: &dyn Embedder,
    rows: &mut Vec<(i64, Vec<f32>)>,
    turns: &mut Vec<i64>,
    report: &mut BackfillReport,
) -> Result<(), String> {
    report.chunks_written += rows.len();
    vectors.append_segment(embedder.model_id(), embedder.dims(), rows)?;
    report.segments_written += 1;
    store.mark_embedded(turns).map_err(|e| e.to_string())?;
    rows.clear();
    turns.clear();
    Ok(())
}

#[cfg(feature = "model2vec")]
pub mod potion {
    use super::{EmbedKind, Embedder};
    use std::path::Path;

    /// Static-embedding fast tier (Model2Vec potion-multilingual-128M,
    /// 256d, distilled from bge-m3). No transformer inference: token
    /// lookups + pooling, so bulk backfill runs at tens of thousands of
    /// texts per second on CPU.
    pub struct PotionEmbedder {
        model: model2vec_rs::model::StaticModel,
        dims: usize,
        model_id: String,
    }

    const REPO: &str = "minishlab/potion-multilingual-128M";

    /// Resolve the potion repo to load. `PIXEL_RECALL_MODEL_REPO` overrides
    /// the default so `pixel ask` can select a code-specialized static model
    /// (e.g. `minishlab/potion-code-16M-v2`, distilled from CodeRankEmbed)
    /// for much better code-domain recall, while the transcript path keeps
    /// the multilingual default.
    fn resolved_repo() -> String {
        match std::env::var("PIXEL_RECALL_MODEL_REPO") {
            Ok(v) if !v.is_empty() => v,
            _ => REPO.to_string(),
        }
    }

    impl PotionEmbedder {
        pub fn open(cache_dir: &Path, download: bool) -> Result<Self, String> {
            let repo = resolved_repo();
            let marker = cache_dir.join("potion.ok");
            if !download && !marker.exists() {
                return Err(
                    "embedding model not present — run `gitpixel recall setup` first".to_string(),
                );
            }
            let _ = std::fs::create_dir_all(cache_dir);
            // Route the HF hub cache under gitpixel's model dir. set_var is
            // process-global; both CLI and daemon call this before any
            // threads that read the environment.
            unsafe {
                std::env::set_var("HF_HOME", cache_dir.join("hf"));
            }
            let model = model2vec_rs::model::StaticModel::from_pretrained(&repo, None, None, None)
                .map_err(|e| format!("potion model load: {e}"))?;
            let dims = model.encode_single("probe").len();
            if dims == 0 {
                return Err("potion model produced empty embeddings".to_string());
            }
            if download {
                let _ = std::fs::write(&marker, &repo);
            }
            Ok(Self {
                model,
                dims,
                model_id: repo,
            })
        }
    }

    impl Embedder for PotionEmbedder {
        fn model_id(&self) -> &str {
            &self.model_id
        }

        fn dims(&self) -> usize {
            self.dims
        }

        fn embed_batch(
            &mut self,
            texts: &[&str],
            _kind: EmbedKind, // static embeddings have no query/passage split
        ) -> Result<Vec<Vec<f32>>, String> {
            let owned: Vec<String> = texts.iter().map(|t| t.to_string()).collect();
            Ok(self.model.encode_with_args(&owned, Some(512), 1024))
        }
    }
}

#[cfg(feature = "fastembed")]
pub mod fast {
    use super::{EmbedKind, Embedder};

    pub struct FastEmbedder {
        model: fastembed::TextEmbedding,
        model_id: String,
        dims: usize,
    }

    impl FastEmbedder {
        /// Load multilingual-e5-small (int8 ONNX) from the local cache dir;
        /// `download` controls whether a missing model may be fetched.
        pub fn open(cache_dir: &std::path::Path, download: bool) -> Result<Self, String> {
            if !download && !cache_dir.exists() {
                return Err(
                    "embedding model not present — run `gitpixel recall setup` first".to_string(),
                );
            }
            let options = fastembed::InitOptions::new(fastembed::EmbeddingModel::MultilingualE5Small)
                .with_cache_dir(cache_dir.to_path_buf())
                .with_show_download_progress(download);
            let model = fastembed::TextEmbedding::try_new(options)
                .map_err(|e| format!("embedding model init: {e}"))?;
            Ok(Self {
                model,
                model_id: "multilingual-e5-small-q".to_string(),
                dims: 384,
            })
        }
    }

    impl Embedder for FastEmbedder {
        fn model_id(&self) -> &str {
            &self.model_id
        }

        fn dims(&self) -> usize {
            self.dims
        }

        fn embed_batch(
            &mut self,
            texts: &[&str],
            kind: EmbedKind,
        ) -> Result<Vec<Vec<f32>>, String> {
            // E5 prefix convention — encoded here, never at call sites.
            let prefix = match kind {
                EmbedKind::Query => "query: ",
                EmbedKind::Passage => "passage: ",
            };
            let prefixed: Vec<String> = texts.iter().map(|t| format!("{prefix}{t}")).collect();
            self.model
                .embed(prefixed, None)
                .map_err(|e| format!("embed: {e}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StubEmbedder;

    impl Embedder for StubEmbedder {
        fn model_id(&self) -> &str {
            "stub"
        }
        fn dims(&self) -> usize {
            4
        }
        fn embed_batch(
            &mut self,
            texts: &[&str],
            _kind: EmbedKind,
        ) -> Result<Vec<Vec<f32>>, String> {
            Ok(texts.iter().map(|_| vec![0.5; 4]).collect())
        }
    }

    /// Regression: turns are only marked embedded at segment flush, so the
    /// backfill must page by id — a head query would re-chunk and re-embed
    /// the same batch until the flush threshold (once produced a 65×
    /// duplication of every chunk on the real corpus).
    #[test]
    fn backfill_embeds_each_turn_exactly_once() {
        use crate::model::{Role, TsSource, UnifiedSession, UnifiedTurn};
        let dir = std::env::temp_dir().join(format!("gpx-embed-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut store = crate::store::RecallStore::open(&dir.join("recall.db")).unwrap();
        let session = UnifiedSession {
            agent: "claude",
            source_session_id: "s1".into(),
            source_path: "test".into(),
            cwd: Some("/tmp/x".into()),
            git_branch: None,
            title: None,
            ts_source: TsSource::Iso,
            is_subagent: false,
            parent_source_session_id: None,
        };
        // More turns than one batch so the loop must page past BATCH_TURNS
        // without a flush in between (FLUSH_CHUNKS >> turn count here).
        let turns: Vec<UnifiedTurn> = (0..300)
            .map(|i| UnifiedTurn {
                role: Role::Assistant,
                intent_source: None,
                ts: Some(i),
                text: format!("turn number {i}"),
                truncated: false,
                source_byte_start: None,
                source_byte_len: None,
            })
            .collect();
        let st = crate::store::IngestState {
            file_size: 1,
            mtime_ms: 1,
            bytes_ingested: 1,
            cursor: None,
        };
        store.replace_session(&session, &turns, "u1", &st).unwrap();

        let mut vectors = crate::vector::VectorStore::open(&dir.join("vectors")).unwrap();
        let mut embedder = StubEmbedder;
        let report = run_backfill(&store, &mut vectors, &mut embedder, |_, _| {}).unwrap();
        assert_eq!(report.turns_embedded, 300);
        let chunk_count: i64 = store
            .connection()
            .query_row("SELECT COUNT(*) FROM vector_chunks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(chunk_count, 300, "each short turn must yield exactly one chunk");
        assert_eq!(store.embed_backlog().unwrap(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn chunking_covers_and_overlaps() {
        let text = "x".repeat(4000);
        let chunks = chunk_offsets(&text);
        assert!(chunks.len() >= 3);
        assert_eq!(chunks[0].0, 0);
        assert_eq!(chunks.last().unwrap().1, 4000);
        for w in chunks.windows(2) {
            assert!(w[1].0 < w[0].1, "windows must overlap");
        }
        assert_eq!(chunk_offsets("short"), vec![(0, 5)]);
    }
}

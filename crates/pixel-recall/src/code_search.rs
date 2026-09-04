//! Semantic code search over a code tree via static embeddings.
//!
//! `ask(root, query, k, max_files)` answers open-ended questions like "how is
//! authentication handled?" by embedding the question and every candidate code
//! chunk, then ranking by cosine similarity. Reuses this crate's embedding seam
//! (`Embedder` trait + `PotionEmbedder` behind the `model2vec` feature), so the
//! model downloads once into the shared recall model cache on first use and the
//! ML dependency stays behind a feature.
//!
//! This is an AUGMENTATIVE channel, deliberately NOT a replacement for
//! `resolve`/`search`: deterministic resolution keeps its contract; `ask`
//! adds a semantic layer on top. NDCG is the gate (see
//! crates/pixel-bench/benches/ndcg_relevance.rs).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::embed::{EmbedKind, chunk_offsets, open_default_embedder};

/// One ranked hit: a file matched for the question, with a representative
/// snippet (head of the best-matching chunk) and the cosine score.
pub struct AskHit {
    pub path: String,
    pub score: f32,
    pub snippet: String,
}

const MAX_FILE_BYTES: usize = 512 * 1024;

/// Walk a tree, returning code-like files. Rejects binary/noise paths.
fn collect_files(root: &Path, max_files: usize) -> Vec<PathBuf> {
    use std::collections::VecDeque;
    let mut out: Vec<PathBuf> = Vec::new();
    let mut queue: VecDeque<PathBuf> = VecDeque::new();
    queue.push_back(root.to_path_buf());
    while let Some(dir) = queue.pop_front() {
        if out.len() >= max_files {
            break;
        }
        let Ok(read) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in read.flatten() {
            let p = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if p.is_dir() {
                if !skip_dir(&name) {
                    queue.push_back(p);
                }
                continue;
            }
            if !is_code_file(&name) {
                continue;
            }
            out.push(p);
            if out.len() >= max_files {
                break;
            }
        }
    }
    out
}

/// Skips vendored/generated dirs so a tree walk stays tight.
fn skip_dir(name: &str) -> bool {
    matches!(
        name,
        "target"
            | "node_modules"
            | ".git"
            | ".pixel"
            | "dist"
            | "build"
            | "vendor"
            | ".cache"
            | "assets"
            | "reference"
            | "examples"
            | "tests"
    )
}

fn is_code_file(name: &str) -> bool {
    let ext = name
        .rsplit('.')
        .next()
        .map(|e| e.to_lowercase())
        .unwrap_or_default();
    matches!(
        ext.as_str(),
        "rs" | "toml"
            | "py"
            | "ts"
            | "tsx"
            | "js"
            | "jsx"
            | "go"
            | "c"
            | "h"
            | "cpp"
            | "hpp"
            | "java"
            | "rb"
            | "sh"
            | "md"
            | "json"
            | "yaml"
            | "yml"
            | "sql"
            | "zig"
            | "swift"
            | "kt"
            | "css"
    )
}

/// Cosine similarity between two vectors (defensive normalize on top of the
/// embedder's built-in L2 normalization).
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    let denom = (na * nb).sqrt();
    if denom > 0.0 { dot / denom } else { 0.0 }
}

struct CorpusEntry {
    path: String,
    text: String,
}

/// Answer a natural-language question over a code tree.
///
/// Returns the top `k` files ranked by max chunk cosine similarity, each with
/// a snippet from its best-matching chunk.
pub fn ask(root: &Path, query: &str, k: usize, max_files: usize) -> Result<Vec<AskHit>, String> {
    let files = collect_files(root, max_files);
    if files.is_empty() {
        return Ok(Vec::new());
    }

    // Opens the embedding model (downloads once into the recall model cache).
    // Prefer the code-specialized static model for code-domain questions
    // (better recall than multilingual for identifiers + symbols); the
    // transcript `recall` path keeps the multilingual default.
    unsafe {
        std::env::set_var("PIXEL_RECALL_MODEL_REPO", "minishlab/potion-code-16M-v2");
    }
    let mut embedder = open_default_embedder(true)?;

    // Build the corpus (chunk every file) and embed the chunks in batches.
    let mut corpus: Vec<CorpusEntry> = Vec::new();
    let mut chunk_texts: Vec<String> = Vec::new();
    for file in &files {
        let Ok(bytes) = std::fs::read(file) else {
            continue;
        };
        if bytes.len() > MAX_FILE_BYTES {
            continue; // skip huge / binary / generated files
        }
        let text = String::from_utf8_lossy(&bytes).into_owned();
        for (start, end) in chunk_offsets(&text) {
            let chunk = text[start..end].to_string();
            corpus.push(CorpusEntry {
                path: file.display().to_string(),
                text: chunk.clone(),
            });
            chunk_texts.push(chunk);
        }
    }
    if corpus.is_empty() {
        return Ok(Vec::new());
    }

    // Embed the query and all chunks.
    let qvec = embedder
        .embed_batch(&[query], EmbedKind::Query)?
        .into_iter()
        .next()
        .ok_or("empty query embedding")?;
    let refs: Vec<&str> = chunk_texts.iter().map(String::as_str).collect();
    let cvecs = embedder.embed_batch(&refs, EmbedKind::Passage)?;
    if cvecs.len() != corpus.len() {
        return Err(format!(
            "embedding count mismatch: {} chunks vs {} vectors",
            corpus.len(),
            cvecs.len()
        ));
    }

    // Per-file best score across its chunks.
    let mut best: HashMap<String, f32> = HashMap::new();
    let mut snippet_of: HashMap<String, String> = HashMap::new();
    for (entry, vec) in corpus.iter().zip(&cvecs) {
        let s = cosine(&qvec, vec);
        if best.get(&entry.path).copied().unwrap_or(f32::MIN) < s {
            best.insert(entry.path.clone(), s);
            snippet_of.insert(entry.path.clone(), make_snippet(&entry.text));
        }
    }

    // --- lexical channel (what a standalone-embedding sibling misses) ---
    // Count word-boundary keyword hits per file across the query's
    // identifier words, then rank. This makes `ask` a HYBRID (RRF of lexical
    // + semantic) instead of pure-embedding — the same design pixel-recall's
    // ask uses, and why it beats lexical-only retrieval on open-ended asks.
    let query_words: Vec<String> = query
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|w| w.len() >= 3)
        .map(|w| w.to_lowercase())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    let mut lexical_score: HashMap<String, u32> = HashMap::new();
    if !query_words.is_empty() {
        for entry in &corpus {
            let lower = entry.text.to_lowercase();
            let mut hits = 0u32;
            for w in &query_words {
                if lower.contains(&**w) {
                    hits += 1;
                }
            }
            if hits > 0 {
                *lexical_score.entry(entry.path.clone()).or_default() += hits;
            }
        }
    }

    // --- fuse the two orderings with RRF (semantic 2.0 > lexical 1.0) ---
    let mut sem_sorted: Vec<(f32, String)> = best.iter().map(|(p, s)| (*s, p.clone())).collect();
    sem_sorted.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.1.cmp(&b.1))
    });
    let sem_ordered: Vec<String> = sem_sorted.iter().map(|(_, p)| p.clone()).collect();
    let mut lex_sorted: Vec<(String, u32)> =
        lexical_score.iter().map(|(p, c)| (p.clone(), *c)).collect();
    lex_sorted.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let lex_ordered: Vec<String> = lex_sorted.iter().map(|(p, _)| p.clone()).collect();

    let fused = pixel_rank::rrf_fuse(
        &[(&sem_ordered, 2.0), (&lex_ordered, 1.0)],
        pixel_rank::RRF_K,
    );

    let ranked: Vec<(f32, String, String)> = fused
        .into_iter()
        .take(k)
        .filter_map(|(p, _)| {
            best.get(&p)
                .map(|s| (*s, p.clone(), snippet_of.remove(&p).unwrap_or_default()))
        })
        .collect();

    Ok(ranked
        .into_iter()
        .map(|(score, path, snippet)| AskHit {
            path,
            score,
            snippet,
        })
        .collect())
}
fn make_snippet(text: &str) -> String {
    let joined: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut cut = joined;
    if cut.len() > 160 {
        // Truncate at a char boundary to avoid a multi-byte-char panic.
        let mut boundary = 160;
        while boundary > 0 && !cut.is_char_boundary(boundary) {
            boundary -= 1;
        }
        cut.truncate(boundary);
        cut.push('…');
    }
    cut
}

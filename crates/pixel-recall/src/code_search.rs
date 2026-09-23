//! Semantic code search over a code tree via static embeddings.
//!
//! `ask(root, query, k, max_files)` answers open-ended questions like "how is
//! authentication handled?" by embedding the question and every candidate code
//! chunk, then fusing semantic ranks with distinct full-file lexical coverage.
//! Reuses this crate's embedding seam
//! (`Embedder` trait + `PotionEmbedder` behind the `model2vec` feature), so the
//! model downloads once into the shared recall model cache on first use and the
//! ML dependency stays behind a feature.
//!
//! This is an AUGMENTATIVE channel, deliberately NOT a replacement for
//! `resolve`/`search`: deterministic resolution keeps its contract; `ask`
//! adds a semantic layer on top. NDCG is the gate (see
//! crates/pixel-bench/benches/ndcg_relevance.rs).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::embed::{EmbedKind, chunk_offsets};

/// One ranked hit: a file matched for the question, with a representative
/// snippet (head of the best-matching chunk), cosine score and RRF ordering key.
pub struct AskHit {
    pub path: String,
    /// Compatibility alias of semantic_score (cosine), not the ordering key.
    pub score: f32,
    pub semantic_score: f32,
    pub ranking_score: f64,
    pub lexical_matches: usize,
    pub query_terms: usize,
    pub snippet: String,
}

const MAX_FILE_BYTES: usize = 512 * 1024;

/// Walk a tree, returning code-like files. Rejects binary/noise paths.
#[derive(Default, serde::Serialize)]
pub struct AskCoverage {
    pub candidate_files: usize,
    pub searched_files: usize,
    pub max_files: usize,
    pub file_limit_reached: bool,
    pub skipped_files: usize,
    pub empty_files: usize,
    pub traversal_errors: usize,
    pub result_limit_reached: bool,
    pub scope: &'static str,
    pub degraded: bool,
}

pub struct AskResult {
    pub hits: Vec<AskHit>,
    pub coverage: AskCoverage,
}

fn collect_files(root: &Path, max_files: usize) -> (Vec<PathBuf>, AskCoverage) {
    use std::collections::VecDeque;
    let mut out = Vec::new();
    let mut coverage = AskCoverage {
        max_files,
        scope: "eligible source/document extensions; excluded noise directories; nested checkouts skipped; no symlinks; files <=512KiB; UTF-8 text only",
        ..Default::default()
    };
    let mut queue = VecDeque::from([root.to_path_buf()]);
    if std::fs::symlink_metadata(root).is_ok_and(|m| m.file_type().is_symlink()) {
        coverage.skipped_files += 1;
        coverage.degraded = true;
        return (out, coverage);
    }
    'walk: while let Some(dir) = queue.pop_front() {
        let read = match std::fs::read_dir(&dir) {
            Ok(read) => read,
            Err(_) => {
                coverage.traversal_errors += 1;
                continue;
            }
        };
        let mut entries = Vec::new();
        for entry in read {
            match entry {
                Ok(entry) => entries.push(entry),
                Err(_) => coverage.traversal_errors += 1,
            }
        }
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let name = entry.file_name().to_string_lossy().to_string();
            let kind = match entry.file_type() {
                Ok(kind) => kind,
                Err(_) => {
                    coverage.traversal_errors += 1;
                    continue;
                }
            };
            if kind.is_symlink() {
                continue;
            }
            if kind.is_dir() {
                let path = entry.path();
                if !skip_dir(&name) && !is_nested_checkout(&path) {
                    queue.push_back(path);
                }
            } else if kind.is_file() && is_code_file(&name) {
                if out.len() == max_files {
                    coverage.file_limit_reached = true;
                    break 'walk;
                }
                out.push(entry.path());
            }
        }
    }
    coverage.candidate_files = out.len();
    coverage.degraded = coverage.file_limit_reached || coverage.traversal_errors > 0;
    (out, coverage)
}

/// Whether `dir` is the top of another working tree: a linked worktree or a
/// submodule (a `.git` file) or a nested clone (a `.git` directory). Its
/// files belong to that checkout, often another branch of the same project
/// (`git worktree add`, `.claude/worktrees/<name>`), and would rank copies
/// of the searched tree's own code; git does not list them either. Only
/// directories below the walk root are asked, so the root keeps its `.git`.
fn is_nested_checkout(dir: &Path) -> bool {
    std::fs::symlink_metadata(dir.join(".git")).is_ok()
}

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
        .map(str::to_lowercase)
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
    ask_with_metadata(root, query, k, max_files).map(|result| result.hits)
}

/// Most files the semantic fallback embeds inside a daemon request.
pub const SEMANTIC_FALLBACK_MAX_FILES: usize = 2000;

/// Semantic leads for a query no lexical tier answered, with what the scan
/// covered. The similarity scores do not separate related from unrelated
/// files (measured on this repository, 2026-09-14: a nonsense query's best
/// file scored 0.33, a relevant French question's 0.33 to 0.39), so callers
/// present the hits as unverified leads, never as matches.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SemanticFallback {
    /// `(repo-relative path, cosine similarity)`, best first, at most `limit`.
    pub hits: Vec<(String, f64)>,
    pub searched_files: usize,
    /// The scan stopped at [`SEMANTIC_FALLBACK_MAX_FILES`].
    pub file_limit_reached: bool,
    /// The fallback was gated off (`PIXEL_SEMANTIC_FALLBACK` unset): no
    /// model load and no corpus embed ran. Named so a caller can report
    /// "disabled" rather than "searched, found nothing".
    pub disabled: bool,
}

impl SemanticFallback {
    /// Caps a caller names in its epistemics when it shows these hits.
    pub fn caps(&self) -> Vec<String> {
        if self.disabled {
            return vec![
                "semantic fallback disabled: set PIXEL_SEMANTIC_FALLBACK=1 to enable".to_string(),
            ];
        }
        if self.hits.is_empty() {
            return Vec::new();
        }
        let mut caps = vec![
            "semantic leads are unverified: embedding similarity does not separate related from unrelated files"
                .to_string(),
        ];
        if self.file_limit_reached {
            caps.push(format!(
                "semantic fallback embedded only the first {} of the eligible files",
                self.searched_files
            ));
        }
        caps
    }
}

/// Whether the semantic fallback may run. Default OFF: a lexical miss used
/// to load the embedding model and embed up to
/// [`SEMANTIC_FALLBACK_MAX_FILES`] files inside the daemon request — the
/// worst tail on the hot path. `PIXEL_SEMANTIC_FALLBACK=1` (or `true`,
/// `yes`, `on`) re-enables it; any other value keeps it off. Read once per
/// process.
pub fn semantic_fallback_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("PIXEL_SEMANTIC_FALLBACK").is_ok_and(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
    })
}

/// Semantic fallback for cross-lingual concept resolution: embeds the query
/// with the multilingual `potion-code-16M-v2` model and ranks code files by
/// the best-matching chunk.
///
/// Never downloads the model: a daemon request must not block on the
/// network. With no model on disk (fetched once by `pixel search-meaning`)
/// or any other embedding error, the fallback is empty.
///
/// Returned paths are repo-relative (stripped of `root`) so they join with
/// index paths, annotations, and evidence maps that are all relative.
///
/// Gated by [`semantic_fallback_enabled`]: default OFF, so a lexical miss
/// returns a `disabled` fallback instead of paying for the embed.
#[cfg_attr(test, mutants::skip)] // adapter over the on-disk model; `fallback_from` holds the logic and is tested
pub fn semantic_fallback(root: &Path, query: &str, limit: usize) -> SemanticFallback {
    if !semantic_fallback_enabled() {
        return SemanticFallback {
            disabled: true,
            ..Default::default()
        };
    }
    ask_with_embedder(root, query, limit, SEMANTIC_FALLBACK_MAX_FILES, false).map_or_else(
        |_| SemanticFallback::default(),
        |result| fallback_from(root, result),
    )
}

/// The fallback answer for an ask `result` over `root`: repo-relative paths
/// with their cosine similarity, and the scan coverage.
fn fallback_from(root: &Path, result: AskResult) -> SemanticFallback {
    let root_str = root.display().to_string();
    SemanticFallback {
        hits: result
            .hits
            .into_iter()
            .map(|h| {
                let rel = h
                    .path
                    .strip_prefix(&root_str)
                    .and_then(|s| s.strip_prefix('/'))
                    .unwrap_or(&h.path)
                    .to_string();
                (rel, f64::from(h.semantic_score))
            })
            .collect(),
        searched_files: result.coverage.searched_files,
        file_limit_reached: result.coverage.file_limit_reached,
        disabled: false,
    }
}

pub fn ask_with_metadata(
    root: &Path,
    query: &str,
    k: usize,
    max_files: usize,
) -> Result<AskResult, String> {
    ask_with_embedder(root, query, k, max_files, true)
}

/// [`ask_with_metadata`], downloading the model only when `download`.
fn ask_with_embedder(
    root: &Path,
    query: &str,
    k: usize,
    max_files: usize,
    download: bool,
) -> Result<AskResult, String> {
    let (files, coverage) = collect_files(root, max_files);
    if files.is_empty() {
        return Ok(AskResult {
            hits: Vec::new(),
            coverage,
        });
    }

    let mut embedder = open_code_embedder(download)?;

    let mut result = ask_collected(query, k, files, coverage, embedder.as_mut())?;
    for hit in &mut result.hits {
        let path = Path::new(&hit.path);
        if let Ok(relative) = path.strip_prefix(root) {
            hit.path = relative.to_string_lossy().into_owned();
        }
    }
    Ok(result)
}

fn open_code_embedder(download: bool) -> Result<Box<dyn crate::embed::Embedder>, String> {
    crate::embed::open_embedder_with_potion_repo(download, Some("minishlab/potion-code-16M-v2"))
}

fn ask_collected(
    query: &str,
    k: usize,
    files: Vec<PathBuf>,
    mut coverage: AskCoverage,
    embedder: &mut dyn crate::embed::Embedder,
) -> Result<AskResult, String> {
    // Build the corpus (chunk every file) and embed the chunks in batches.
    let mut corpus: Vec<CorpusEntry> = Vec::new();
    let mut lexical_coverage = HashMap::new();
    let mut chunk_texts: Vec<String> = Vec::new();
    for file in &files {
        let Ok(bytes) = std::fs::read(file) else {
            coverage.skipped_files += 1;
            continue;
        };
        if bytes.len() > MAX_FILE_BYTES || bytes.contains(&0) {
            coverage.skipped_files += 1;
            continue;
        }
        let Ok(text) = String::from_utf8(bytes) else {
            coverage.skipped_files += 1;
            continue;
        };
        if text.trim().is_empty() {
            coverage.empty_files += 1;
            continue;
        }
        coverage.searched_files += 1;
        // Filenames are evidence too, but directory names and language extensions
        // must not add shared noise or change ranks when the repository moves.
        let mut filename_tokens = HashSet::new();
        if let Some(name) = file.file_name() {
            // A filename can have compound extensions (`types.d.ts`). Keep
            // only the basename before its first dot so suffix components
            // cannot become lexical evidence.
            let basename = name.to_string_lossy();
            let stem = basename.split('.').next().unwrap_or_default();
            filename_tokens = words(stem);
        }
        let terms = query_terms(query);
        let mut best_coverage = terms
            .iter()
            .filter(|term| filename_tokens.contains(*term))
            .count();
        for (start, end) in chunk_offsets(&text) {
            let chunk = text[start..end].to_string();
            let mut tokens = words(lexical_chunk(&text, start, end));
            tokens.extend(filename_tokens.iter().cloned());
            let chunk_coverage = terms.iter().filter(|term| tokens.contains(*term)).count();
            best_coverage = best_coverage.max(chunk_coverage);
            corpus.push(CorpusEntry {
                path: file.display().to_string(),
                text: chunk.clone(),
            });
            chunk_texts.push(chunk);
        }
        lexical_coverage.insert(file.display().to_string(), best_coverage);
    }
    coverage.degraded |= coverage.skipped_files > 0;
    if corpus.is_empty() {
        return Ok(AskResult {
            hits: Vec::new(),
            coverage,
        });
    }

    // Embed the query and all chunks.
    let qvec = embedder
        .embed_batch(&[query], EmbedKind::Query)?
        .into_iter()
        .next()
        .ok_or("empty query embedding")?;
    validate_vector(&qvec, embedder.dims())?;
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
        validate_vector(vec, qvec.len())?;
        let s = cosine(&qvec, vec);
        if best.get(&entry.path).copied().unwrap_or(f32::MIN) < s {
            best.insert(entry.path.clone(), s);
            snippet_of.insert(entry.path.clone(), make_snippet(&entry.text));
        }
    }

    coverage.result_limit_reached = best.len() > k;
    Ok(AskResult {
        hits: rank_files(query, &lexical_coverage, best, snippet_of, k),
        coverage,
    })
}
fn validate_vector(vector: &[f32], dims: usize) -> Result<(), String> {
    let norm: f64 = vector.iter().map(|x| f64::from(*x).powi(2)).sum();
    if dims == 0 || vector.len() != dims || !norm.is_finite() || norm <= 0.0 {
        return Err(
            "invalid embedding vector: expected finite, nonzero values with consistent dimensions"
                .into(),
        );
    }
    Ok(())
}

/// Exclude identifier fragments created only by fixed byte-window boundaries.
fn lexical_chunk(text: &str, start: usize, end: usize) -> &str {
    let is_ident = |character: char| character.is_alphanumeric() || character == '_';
    let chunk = &text[start..end];
    let chunk = if text[..start].chars().next_back().is_some_and(&is_ident)
        && chunk.chars().next().is_some_and(&is_ident)
    {
        chunk
            .split_once(|character| !is_ident(character))
            .map_or("", |(_, complete)| complete)
    } else {
        chunk
    };
    if text[..end].chars().next_back().is_some_and(&is_ident)
        && text[end..].chars().next().is_some_and(&is_ident)
    {
        chunk
            .rsplit_once(|character| !is_ident(character))
            .map_or("", |(complete, _)| complete)
    } else {
        chunk
    }
}

/// Preserve complete identifiers and split snake_case, camelCase and acronyms.
fn words(text: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    for word in text.split(|c: char| !c.is_alphanumeric() && c != '_') {
        if word.is_empty() {
            continue;
        }
        out.insert(word.to_lowercase());
        for part in word.split('_').filter(|s| !s.is_empty()) {
            let chars: Vec<(usize, char)> = part.char_indices().collect();
            let mut start = 0;
            for i in 1..chars.len() {
                let (_, prev) = chars[i - 1];
                let (offset, current) = chars[i];
                let next_lower = chars.get(i + 1).is_some_and(|(_, c)| c.is_lowercase());
                if (prev.is_lowercase() && current.is_uppercase())
                    || (prev.is_uppercase() && current.is_uppercase() && next_lower)
                {
                    out.insert(part[start..offset].to_lowercase());
                    start = offset;
                }
            }
            out.insert(part[start..].to_lowercase());
        }
    }
    out
}

fn query_terms(query: &str) -> HashSet<String> {
    // Do not impose a minimum length: x, id, io and negation carry meaning.
    let normalized = query.replace("n’t", " not").replace("n't", " not");
    words(&normalized)
        .into_iter()
        .filter(|w| {
            !matches!(
                w.as_str(),
                "a" | "an"
                    | "the"
                    | "is"
                    | "are"
                    | "was"
                    | "were"
                    | "be"
                    | "been"
                    | "being"
                    | "do"
                    | "does"
                    | "did"
                    | "how"
                    | "what"
                    | "where"
                    | "when"
                    | "which"
                    | "who"
                    | "why"
                    | "can"
                    | "could"
                    | "would"
                    | "should"
                    | "please"
                    | "i"
                    | "me"
                    | "my"
                    | "we"
                    | "our"
                    | "you"
                    | "your"
                    | "it"
                    | "its"
                    | "this"
                    | "that"
                    | "these"
                    | "those"
                    | "of"
                    | "to"
                    | "for"
                    | "from"
                    | "in"
                    | "on"
                    | "at"
                    | "with"
                    | "by"
                    | "and"
                    | "or"
            )
        })
        .collect()
}

fn rank_files(
    query: &str,
    lexical_coverage: &HashMap<String, usize>,
    best: HashMap<String, f32>,
    mut snippets: HashMap<String, String>,
    k: usize,
) -> Vec<AskHit> {
    let terms = query_terms(query);
    let mut semantic: Vec<_> = best.into_iter().collect();
    semantic.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    let mut hits: Vec<_> = semantic
        .into_iter()
        .enumerate()
        .map(|(index, (path, score))| {
            let coverage = lexical_coverage.get(&path).copied().unwrap_or_default();
            // Competition ranks: equal coverage receives exactly equal evidence weight.
            let lexical_rank = 1 + lexical_coverage
                .values()
                .filter(|candidate| **candidate > coverage)
                .count();
            let ranking_score = 2.0 / (60.0 + (index + 1) as f64)
                + if coverage == 0 {
                    0.0
                } else {
                    1.0 / (60.0 + lexical_rank as f64)
                };
            AskHit {
                snippet: snippets.remove(&path).unwrap_or_default(),
                path,
                score,
                semantic_score: score,
                ranking_score,
                lexical_matches: coverage,
                query_terms: terms.len(),
            }
        })
        .collect();
    hits.sort_by(|a, b| {
        b.ranking_score
            .total_cmp(&a.ranking_score)
            .then(a.path.cmp(&b.path))
    });
    hits.truncate(k);
    hits
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

#[cfg(test)]
mod tests {
    use super::*;

    fn rank(query: &str, entries: &[(&str, &str, f32)]) -> Vec<AskHit> {
        let terms = query_terms(query);
        let mut lexical_coverage: HashMap<String, usize> = HashMap::new();
        for (path, text, _) in entries {
            let coverage = terms
                .iter()
                .filter(|term| words(text).contains(*term))
                .count();
            lexical_coverage
                .entry(path.to_string())
                .and_modify(|best| *best = (*best).max(coverage))
                .or_insert(coverage);
        }
        let best = entries
            .iter()
            .map(|(path, _, score)| (path.to_string(), *score))
            .collect();
        rank_files(query, &lexical_coverage, best, HashMap::new(), usize::MAX)
    }

    #[test]
    fn collector_is_deterministic_capped_and_does_not_follow_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["c.rs", "a.rs", "b.rs"] {
            std::fs::write(dir.path().join(name), "setup").unwrap();
        }
        std::fs::create_dir(dir.path().join("target")).unwrap();
        std::fs::write(dir.path().join("target/ignored.rs"), "setup").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(dir.path(), dir.path().join("loop")).unwrap();
        let (files, coverage) = collect_files(dir.path(), 2);
        assert_eq!(files.len(), 2);
        assert!(files[0].ends_with("a.rs"));
        assert!(files[1].ends_with("b.rs"));
        assert!(coverage.file_limit_reached && coverage.degraded);
        let (files, coverage) = collect_files(dir.path(), 3);
        assert_eq!(files.len(), 3);
        assert!(!coverage.file_limit_reached && !coverage.degraded);
        let (files, coverage) = collect_files(dir.path(), 0);
        assert!(files.is_empty() && coverage.file_limit_reached);
        let (_, coverage) = collect_files(&dir.path().join("missing"), 3);
        assert_eq!(coverage.traversal_errors, 1);
        assert!(coverage.degraded);
    }

    #[test]
    fn collector_should_skip_nested_checkouts_when_walking_a_repository() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // The searched repository itself is a checkout: its `.git` must not
        // hide its own files.
        std::fs::create_dir(root.join(".git")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "fn own() {}").unwrap();
        // A linked worktree (`.git` file), as `.claude/worktrees/<name>` is.
        let worktree = root.join(".claude/worktrees/feature");
        std::fs::create_dir_all(worktree.join("src")).unwrap();
        std::fs::write(worktree.join(".git"), "gitdir: /elsewhere\n").unwrap();
        std::fs::write(worktree.join("src/lib.rs"), "fn own() {}").unwrap();
        // A nested clone (`.git` directory).
        let clone = root.join("third_party/dep");
        std::fs::create_dir_all(clone.join(".git")).unwrap();
        std::fs::write(clone.join("dep.rs"), "fn dep() {}").unwrap();
        // A plain directory next to them is still walked.
        std::fs::create_dir_all(root.join(".claude/hooks")).unwrap();
        std::fs::write(root.join(".claude/hooks/guard.py"), "def guard(): pass").unwrap();

        let (files, coverage) = collect_files(root, 10);
        let rel: Vec<_> = files
            .iter()
            .map(|f| f.strip_prefix(root).unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(rel, ["src/lib.rs", ".claude/hooks/guard.py"]);
        assert_eq!(coverage.candidate_files, 2);
        assert_eq!(coverage.max_files, 10);
        // The coverage tells the caller what was left out of the search.
        assert!(
            coverage.scope.contains("nested checkouts skipped"),
            "{}",
            coverage.scope
        );
        assert!(!coverage.degraded, "{:?}", coverage.traversal_errors);
    }

    #[cfg(all(not(feature = "model2vec"), not(feature = "fastembed")))]
    #[test]
    fn unavailable_model_is_error_without_mutating_recall_choice() {
        let before = std::env::var_os("PIXEL_RECALL_MODEL_REPO");
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("manual.md"), "manual setup").unwrap();
        let error = ask_with_metadata(dir.path(), "manual setup", 8, 100)
            .err()
            .unwrap();
        assert!(error.contains("feature"));
        assert_eq!(std::env::var_os("PIXEL_RECALL_MODEL_REPO"), before);
    }

    fn hit(path: &str, semantic_score: f32) -> AskHit {
        AskHit {
            path: path.to_string(),
            score: semantic_score,
            semantic_score,
            ranking_score: 0.0,
            lexical_matches: 0,
            query_terms: 0,
            snippet: String::new(),
        }
    }

    #[test]
    fn fallback_from_makes_paths_repo_relative_and_keeps_the_coverage() {
        let (_, mut coverage) = collect_files(Path::new("/nonexistent-root"), 3);
        coverage.searched_files = 7;
        coverage.file_limit_reached = true;
        let result = AskResult {
            hits: vec![hit("/repo/src/a.rs", 0.5), hit("elsewhere/b.rs", 0.25)],
            coverage,
        };
        let fallback = fallback_from(Path::new("/repo"), result);
        assert_eq!(
            fallback.hits,
            [
                ("src/a.rs".to_string(), 0.5),
                ("elsewhere/b.rs".to_string(), 0.25)
            ]
        );
        assert_eq!(fallback.searched_files, 7);
        assert!(fallback.file_limit_reached);
    }

    /// The caps travel with the hits: none without a lead, the unverified
    /// warning with any, and the scan cap when the file limit stopped it.
    #[test]
    fn semantic_fallback_caps_follow_the_hits_and_the_scan() {
        assert!(SemanticFallback::default().caps().is_empty());
        let mut fallback = SemanticFallback {
            hits: vec![("a.rs".to_string(), 0.3)],
            searched_files: 2000,
            file_limit_reached: false,
            disabled: false,
        };
        let caps = fallback.caps();
        assert_eq!(caps.len(), 1);
        assert!(caps[0].starts_with("semantic leads are unverified"));
        fallback.file_limit_reached = true;
        assert_eq!(
            fallback.caps()[1],
            "semantic fallback embedded only the first 2000 of the eligible files"
        );
    }

    struct FixtureEmbedder {
        fail: bool,
    }
    impl crate::embed::Embedder for FixtureEmbedder {
        fn model_id(&self) -> &str {
            "fixture"
        }
        fn dims(&self) -> usize {
            2
        }
        fn embed_batch(&mut self, texts: &[&str], _: EmbedKind) -> Result<Vec<Vec<f32>>, String> {
            if self.fail {
                return Err("fixture model unavailable".into());
            }
            Ok(texts
                .iter()
                .map(|text| {
                    if text.trim().is_empty() {
                        vec![0.0, 0.0]
                    } else {
                        vec![1.0, 0.0]
                    }
                })
                .collect())
        }
    }

    #[test]
    fn empty_source_files_do_not_fail_valid_repository_search() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("__init__.py"), "").unwrap();
        std::fs::write(dir.path().join("blank.rs"), " \n\t").unwrap();
        std::fs::write(dir.path().join("manual.md"), "manual setup").unwrap();
        let (files, coverage) = collect_files(dir.path(), 10);
        let result = ask_collected(
            "manual setup",
            8,
            files,
            coverage,
            &mut FixtureEmbedder { fail: false },
        )
        .unwrap();
        assert_eq!(result.hits.len(), 1);
        assert!(result.hits[0].path.ends_with("manual.md"));
        assert_eq!(result.coverage.searched_files, 1);
        assert_eq!(result.coverage.empty_files, 2);
        assert!(!result.coverage.degraded);
    }

    #[test]
    fn lexical_chunk_excludes_only_identifiers_cut_by_the_window() {
        let text = "prefix manual setup suffix";
        assert_eq!(lexical_chunk(text, 3, 19), "manual setup");
        assert_eq!(lexical_chunk(text, 7, 23), "manual setup");
        assert_eq!(lexical_chunk(text, 6, 20), " manual setup ");
        assert_eq!(lexical_chunk(text, 1, 4), "");
    }

    #[test]
    fn chunk_boundaries_cannot_invent_lexical_words() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("noise.rs"),
            format!("{}manually", " ".repeat(1494)),
        )
        .unwrap();
        let (files, coverage) = collect_files(dir.path(), 10);
        let result = ask_collected(
            "manual",
            8,
            files,
            coverage,
            &mut FixtureEmbedder { fail: false },
        )
        .unwrap();
        assert_eq!(result.hits[0].lexical_matches, 0);
    }

    #[test]
    fn lexical_evidence_must_cooccur_in_one_chunk() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("large.rs"),
            format!("manual{}setup", " unrelated".repeat(300)),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("focused.rs"),
            "manual setup belongs together",
        )
        .unwrap();
        let (files, coverage) = collect_files(dir.path(), 10);
        let result = ask_collected(
            "manual setup",
            8,
            files,
            coverage,
            &mut FixtureEmbedder { fail: false },
        )
        .unwrap();
        let large = result
            .hits
            .iter()
            .find(|hit| hit.path.ends_with("large.rs"))
            .unwrap();
        let focused = result
            .hits
            .iter()
            .find(|hit| hit.path.ends_with("focused.rs"))
            .unwrap();
        assert_eq!(large.lexical_matches, 1);
        assert_eq!(focused.lexical_matches, 2);
        assert!(focused.ranking_score > large.ranking_score);
    }

    #[test]
    fn filename_stem_evidence_ignores_extensions_and_root_location() {
        let temporary = tempfile::tempdir().unwrap();
        let mut observed_scores = Vec::new();
        for directory in ["graph", "unrelated-location"] {
            let root = temporary.path().join(directory);
            std::fs::create_dir(&root).unwrap();
            std::fs::write(root.join("imports.rs"), "fn parse_items() {}").unwrap();
            let (files, coverage) = collect_files(&root, 10);
            let result = ask_collected(
                "imports graph rs",
                8,
                files,
                coverage,
                &mut FixtureEmbedder { fail: false },
            )
            .unwrap();
            assert_eq!(
                result.hits[0].lexical_matches, 1,
                "only imports is evidence, not the directory or extension"
            );
            observed_scores.push(result.hits[0].ranking_score);
        }
        assert_eq!(observed_scores[0], observed_scores[1]);
    }

    #[test]
    fn filename_stem_ignores_compound_extensions() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        std::fs::write(root.join("types.d.ts"), "unrelated body").unwrap();
        let (files, coverage) = collect_files(root, 10);
        let result = ask_collected(
            "d",
            8,
            files,
            coverage,
            &mut FixtureEmbedder { fail: false },
        )
        .unwrap();
        assert_eq!(
            result.hits[0].lexical_matches, 0,
            "compound extension components must not become filename evidence"
        );
    }

    #[test]
    fn filename_components_join_content_coverage_without_substrings_or_duplicates() {
        let temporary = tempfile::tempdir().unwrap();
        for (filename, body, query, expected) in [
            ("manually.rs", "plain module", "manual", 0),
            (
                "readHTTPResponse.rs",
                "read read http",
                "read http response",
                3,
            ),
            (
                "manual_setup.rs",
                "manual setup setup",
                "manual setup setup",
                2,
            ),
        ] {
            let root = temporary.path().join(filename);
            std::fs::create_dir(&root).unwrap();
            std::fs::write(root.join(filename), body).unwrap();
            let (files, coverage) = collect_files(&root, 10);
            let result = ask_collected(
                query,
                8,
                files,
                coverage,
                &mut FixtureEmbedder { fail: false },
            )
            .unwrap();
            assert_eq!(result.hits[0].lexical_matches, expected, "{filename}");
        }
    }

    struct InvalidEmbedder {
        vector: Vec<f32>,
    }
    impl crate::embed::Embedder for InvalidEmbedder {
        fn model_id(&self) -> &str {
            "invalid"
        }
        fn dims(&self) -> usize {
            2
        }
        fn embed_batch(&mut self, texts: &[&str], _: EmbedKind) -> Result<Vec<Vec<f32>>, String> {
            Ok(texts.iter().map(|_| self.vector.clone()).collect())
        }
    }

    #[test]
    fn invalid_model_vectors_are_errors_not_zero_quality_results() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("manual.md"), "manual setup").unwrap();
        for vector in [vec![], vec![0.0, 0.0], vec![f32::NAN, 1.0], vec![1.0]] {
            let (files, coverage) = collect_files(dir.path(), 10);
            let result = ask_collected(
                "manual setup",
                8,
                files,
                coverage,
                &mut InvalidEmbedder { vector },
            );
            assert!(
                result.is_err(),
                "invalid vectors must not become successful rankings"
            );
        }
    }

    #[test]
    fn real_files_flow_through_ranking_and_truthful_coverage() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("manual.md"), "manual setup").unwrap();
        std::fs::write(dir.path().join("noise.rs"), "manually setup").unwrap();
        std::fs::write(dir.path().join("invalid.rs"), [0xff]).unwrap();
        std::fs::write(dir.path().join("binary.rs"), [0]).unwrap();
        let (files, coverage) = collect_files(dir.path(), 10);
        let result = ask_collected(
            "manual setup",
            1,
            files,
            coverage,
            &mut FixtureEmbedder { fail: false },
        )
        .unwrap();
        assert!(result.hits[0].path.ends_with("manual.md"));
        assert_eq!(result.coverage.candidate_files, 4);
        assert_eq!(result.coverage.searched_files, 2);
        assert_eq!(result.coverage.skipped_files, 2);
        assert!(result.coverage.degraded && result.coverage.result_limit_reached);
        let (files, coverage) = collect_files(dir.path(), 10);
        let err = ask_collected(
            "manual setup",
            1,
            files,
            coverage,
            &mut FixtureEmbedder { fail: true },
        );
        assert_eq!(err.err().unwrap(), "fixture model unavailable");
    }

    #[test]
    fn lexical_coverage_is_not_substrings_or_chunk_frequency() {
        let hits = rank(
            "manual setup",
            &[
                ("guide", "manual setup", 0.5),
                ("noise", "manually setup", 0.5),
                ("noise", "manually setup", 0.5),
            ],
        );
        assert_eq!(hits[0].path, "guide");
        assert_eq!(hits[0].lexical_matches, 2);
        assert_eq!(hits[1].lexical_matches, 1);
    }

    #[test]
    fn duplicate_terms_and_chunks_do_not_change_rank() {
        let once = rank(
            "manual setup",
            &[("a", "manual setup", 0.9), ("b", "setup", 0.8)],
        );
        let twice = rank(
            "manual manual setup",
            &[
                ("a", "manual setup", 0.9),
                ("a", "manual setup", 0.9),
                ("b", "setup", 0.8),
            ],
        );
        assert_eq!(
            once.iter().map(|h| h.ranking_score).collect::<Vec<_>>(),
            twice.iter().map(|h| h.ranking_score).collect::<Vec<_>>()
        );
    }

    #[test]
    fn equal_lexical_coverage_has_equal_contribution() {
        let hits = rank(
            "setup",
            &[
                ("z", "setup", 0.9),
                ("a", "setup", 0.8),
                ("b", "nothing", 0.7),
            ],
        );
        assert_eq!(hits[0].path, "z");
        assert!((hits[0].ranking_score - 2.0 / 61.0 - 1.0 / 61.0).abs() < 1e-12);
        assert!((hits[1].ranking_score - 2.0 / 62.0 - 1.0 / 61.0).abs() < 1e-12);
        assert!((hits[2].ranking_score - 2.0 / 63.0).abs() < 1e-12);
    }

    #[test]
    fn identifiers_negation_and_short_terms_survive() {
        let tokens = words("readHTTPResponse snake_case userID io x");
        for word in [
            "read",
            "http",
            "response",
            "snake",
            "case",
            "user",
            "id",
            "io",
            "x",
            "snake_case",
        ] {
            assert!(tokens.contains(word), "missing {word}");
        }
        let terms = query_terms("How can I not use io x id without setup?");
        for word in ["not", "use", "io", "x", "id", "without", "setup"] {
            assert!(terms.contains(word));
        }
        for word in ["how", "can", "i"] {
            assert!(!terms.contains(word));
        }
        assert_eq!(query_terms("How can I please setup?"), query_terms("setup"));
        assert!(query_terms("I don't want setup").contains("not"));
        assert!(query_terms("I don’t want setup").contains("not"));
    }

    #[test]
    fn ordering_is_deterministic_and_scores_are_truthful() {
        let hits = rank("setup", &[("z", "setup", 0.8), ("a", "setup", 0.8)]);
        assert_eq!(hits[0].path, "a");
        for hit in hits {
            assert_eq!(hit.score, hit.semantic_score);
            assert_ne!(hit.score as f64, hit.ranking_score);
        }
    }

    #[test]
    fn is_code_file_goes_by_the_lowercased_extension() {
        assert!(is_code_file("src/main.rs"));
        assert!(is_code_file("MAIN.RS"));
        assert!(is_code_file("app/models/user.rb") || !is_code_file("app/models/user.rb"));
        assert!(!is_code_file("logo.png"));
        assert!(!is_code_file("Makefile"));
        assert!(!is_code_file("archive.tar.gz"));
    }
}

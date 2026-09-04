//! `pixel-rank` — the pure fusion core for `pixel targets` and (later) ranked
//! `search`/`resolve`.
//!
//! Task text in, closed prioritized file list out. The `op_targets` service
//! op in `pixel-daemon` gathers `SignalInputs` from the trigram index and the
//! code graph; this crate tokenizes the task, fuses the per-signal ranked
//! lists with reciprocal-rank fusion (mirroring `pixel-recall`'s hybrid
//! channel fusion), and assigns P0/P1/P2 tiers. Everything here is
//! deterministic: total orders everywhere, ties broken by path ascending,
//! scores rounded for byte-stable JSON.
//!
//! No I/O, no daemon, no index — pure functions over caller-supplied
//! `SignalInputs`. Per `PLAN.md`'s crate table this holds "the pure fusion
//! core (port of serve/targets.rs): signal registry + weighted RRF (K=60),
//! P0/P1/P2 tiering; slots for recency/churn/session signals."

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde::Serialize;
use serde_json::{Value, json};

pub mod rerank;
pub mod signals;

pub use pixel_graph::split_ident_words;
use pixel_graph::store::SymbolKind;
use pixel_graph::targets::SymbolHit;

// ---------------------------------------------------------------------------
// tokenizer
// ---------------------------------------------------------------------------

/// Pure-function words that carry no retrieval signal in a task description.
/// Domain-ish words (error, parse, auth, …) deliberately stay searchable.
const STOPWORDS: &[&str] = &[
    // articles / prepositions / pronouns / auxiliaries
    "the",
    "and",
    "for",
    "with",
    "that",
    "this",
    "its",
    "it",
    "a",
    "an",
    "to",
    "in",
    "of",
    "on",
    "at",
    "by",
    "is",
    "are",
    "be",
    "was",
    "were",
    "or",
    "as",
    "so",
    "we",
    "our",
    "my",
    "your",
    "into",
    "from",
    "when",
    "then",
    "than",
    "them",
    "they",
    "there",
    "all",
    "any",
    "can",
    "will",
    "should",
    "must",
    "not",
    "but",
    "how",
    "what",
    "why",
    // task verbs / filler
    "fix",
    "add",
    "implement",
    "support",
    "make",
    "create",
    "update",
    "remove",
    "change",
    "refactor",
    "improve",
    "ensure",
    "allow",
    "use",
    "need",
    "want",
    "please",
    "new",
    "also",
    "file",
    "files",
    "code",
    "feature",
    "bug",
    "task",
    "issue",
];

const MAX_KEYWORDS: usize = 12;
const MIN_KEYWORD_LEN: usize = 3;

/// Static, code-domain thesaurus for semantic keyword expansion. Each keyword
/// is mapped to a small, conservative set of related terms used only to
/// broaden path/filename matching; the original keywords remain canonical for
/// content probes and report labels.
const SEMANTIC_RELATIONS: &[(&str, &[&str])] = &[
    ("login", &["auth", "authenticate", "signin", "session"]),
    ("auth", &["login", "authenticate", "session", "user"]),
    ("authenticate", &["login", "auth", "session", "user"]),
    ("signin", &["login", "auth", "authenticate"]),
    ("user", &["account", "login", "profile"]),
    ("account", &["user", "login", "profile"]),
    ("session", &["cookie", "login", "auth", "token"]),
    ("cookie", &["session", "auth", "login"]),
    ("token", &["jwt", "session", "auth", "credential"]),
    ("jwt", &["token", "auth", "session"]),
    ("credential", &["token", "password", "auth", "login"]),
    ("password", &["credential", "auth", "login"]),
    ("config", &["settings", "configuration", "options"]),
    ("settings", &["config", "configuration", "options"]),
    ("configuration", &["config", "settings", "options"]),
    ("database", &["sql", "query", "schema"]),
    ("sql", &["database", "query"]),
    ("query", &["sql", "database", "request"]),
    ("api", &["endpoint", "route", "request", "response"]),
    ("endpoint", &["api", "route", "request"]),
    ("request", &["response", "api", "route", "endpoint"]),
    ("response", &["request", "api", "route", "endpoint"]),
    ("route", &["endpoint", "api", "request"]),
    ("error", &["exception", "failure", "panic"]),
    ("exception", &["error", "failure"]),
    ("failure", &["error", "exception"]),
    ("panic", &["error", "exception"]),
    ("cache", &["store", "storage"]),
    ("store", &["cache", "storage"]),
    ("storage", &["cache", "store"]),
    ("queue", &["job", "worker", "background"]),
    ("job", &["queue", "worker", "task"]),
    ("worker", &["queue", "job", "background"]),
    ("background", &["queue", "job", "worker"]),
    ("email", &["mail"]),
    ("mail", &["email"]),
    // Languages, compilers, parser & AST
    (
        "csharp",
        &["language", "extract", "grammar", "c_sharp", "cs"],
    ),
    ("python", &["language", "extract", "grammar", "py"]),
    ("rust", &["language", "extract", "grammar", "rs"]),
    ("javascript", &["language", "extract", "grammar", "js"]),
    ("typescript", &["language", "extract", "grammar", "ts"]),
    ("golang", &["language", "extract", "grammar", "go"]),
    ("java", &["language", "extract", "grammar"]),
    ("cpp", &["language", "extract", "grammar", "cplusplus"]),
    ("language", &["extract", "grammar", "parser", "syntax"]),
    ("parser", &["language", "extract", "grammar", "syntax"]),
    ("grammar", &["language", "extract", "parser"]),
];

pub const SHORT_TECH_KEYWORDS: &[&str] = &[
    "c", "r", "go", "rs", "ts", "js", "py", "rb", "sh", "ui", "ci", "cd", "db", "os", "io", "ip",
    "ai", "ml",
];

/// Normalize compound technical terms (e.g. "c#", "c++", ".net", "node.js")
/// into clean alphanumeric words before word splitting.
pub fn normalize_task_compounds(task: &str) -> String {
    let mut out = task.to_string();
    let replacements = [
        ("c#", "csharp"),
        ("c-sharp", "csharp"),
        ("c_sharp", "csharp"),
        ("c++", "cpp"),
        ("f#", "fsharp"),
        (".net", "dotnet"),
        ("node.js", "nodejs"),
        ("vue.js", "vuejs"),
        ("next.js", "nextjs"),
        ("react.js", "reactjs"),
    ];
    for (from, to) in replacements {
        let mut lower = out.to_ascii_lowercase();
        while let Some(idx) = lower.find(from) {
            let mut result = String::with_capacity(out.len() + to.len());
            result.push_str(&out[..idx]);
            result.push_str(to);
            result.push_str(&out[idx + from.len()..]);
            out = result;
            lower = out.to_ascii_lowercase();
        }
    }
    out
}

/// Expand a single keyword to its semantic relatives.
pub fn semantic_expand(word: &str) -> Vec<&'static str> {
    for (term, related) in SEMANTIC_RELATIONS {
        if *term == word {
            return related.to_vec();
        }
    }
    Vec::new()
}

/// Expand a list of keywords, returning related terms that are not already
/// present in the original list. Deterministic: preserves keyword order and
/// then first-occurrence order of the thesaurus.
pub fn expand_keywords(keywords: &[String]) -> Vec<String> {
    let mut seen: HashSet<&str> = keywords.iter().map(String::as_str).collect();
    let mut out = Vec::new();
    for kw in keywords {
        for related in semantic_expand(kw) {
            if seen.insert(related) {
                out.push(related.to_string());
            }
        }
    }
    out
}

#[derive(Debug, Clone, Default)]
pub struct TaskQuery {
    /// Backticked/quoted identifiers taken verbatim: `` `login_user` `` →
    /// exact symbol-name probe (case preserved).
    pub exact_tokens: Vec<String>,
    /// Lowercased keywords, first-occurrence order, deduped, len ≥ 3 (or short tech keyword), ≤ 12.
    pub keywords: Vec<String>,
    /// True when the task contained MORE searchable keywords than
    /// [`MAX_KEYWORDS`]: the dropped words contributed no signal, so any
    /// result built from this query is a lower bound, not exhaustive.
    pub keywords_truncated: bool,
}

fn is_ident(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Tokenize a task description. Errors when nothing searchable survives.
pub fn tokenize_task(task: &str) -> Result<TaskQuery, String> {
    let normalized = normalize_task_compounds(task);
    let stop: HashSet<&str> = STOPWORDS.iter().copied().collect();
    let mut exact_tokens: Vec<String> = Vec::new();
    let mut keywords: Vec<String> = Vec::new();
    let mut seen_kw: HashSet<String> = HashSet::new();
    let mut keywords_truncated = false;

    let push_words = |text: &str,
                      keywords: &mut Vec<String>,
                      seen: &mut HashSet<String>,
                      truncated: &mut bool| {
        for chunk in text.split(|c: char| !c.is_ascii_alphanumeric() && c != '_') {
            for w in split_ident_words(chunk) {
                let is_valid_len =
                    w.len() >= MIN_KEYWORD_LEN || SHORT_TECH_KEYWORDS.contains(&w.as_str());
                if is_valid_len && !stop.contains(w.as_str()) {
                    if keywords.len() >= MAX_KEYWORDS {
                        // A distinct searchable word was dropped by the cap —
                        // record it so callers can surface the truncation.
                        if !seen.contains(&w) {
                            *truncated = true;
                        }
                        continue;
                    }
                    if seen.insert(w.clone()) {
                        keywords.push(w);
                    }
                }
            }
        }
    };

    // Pass 1: quoted spans become exact tokens AND feed the keyword pass.
    let mut rest = String::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for c in normalized.chars() {
        match quote {
            Some(q) if c == q => {
                if is_ident(&cur) && !exact_tokens.contains(&cur) {
                    exact_tokens.push(cur.clone());
                }
                rest.push(' ');
                rest.push_str(&cur);
                cur.clear();
                quote = None;
            }
            Some(_) => cur.push(c),
            None if c == '`' || c == '"' || c == '\'' => quote = Some(c),
            None => rest.push(c),
        }
    }
    if !cur.is_empty() {
        rest.push(' ');
        rest.push_str(&cur);
    }
    push_words(&rest, &mut keywords, &mut seen_kw, &mut keywords_truncated);

    if exact_tokens.is_empty() && keywords.is_empty() {
        return Err("task description yields no searchable keywords".to_string());
    }
    Ok(TaskQuery {
        exact_tokens,
        keywords,
        keywords_truncated,
    })
}

// ---------------------------------------------------------------------------
// fusion inputs / outputs
// ---------------------------------------------------------------------------

pub struct TargetsOptions {
    pub limit: usize,
    /// When set, drop files at tiers above this. `"P0"` = P0 only, `"P1"` =
    /// P0+P1, `"P2"` or `None` = all tiers (default).
    pub max_tier: Option<String>,
    /// When true, apply a score-gap cutoff: if the score drops by more than
    /// `SCORE_GAP_RATIO` between the last P0 and the first P1, drop all P1/P2
    /// files below the gap threshold. This dramatically improves precision on
    /// simple tasks where 1 file is the clear answer.
    pub precision_mode: bool,
}

impl Default for TargetsOptions {
    fn default() -> Self {
        TargetsOptions {
            limit: 20,
            max_tier: None,
            precision_mode: false,
        }
    }
}

pub const DEFAULT_LIMIT: usize = 20;
pub const MAX_LIMIT: usize = 100;

/// Everything the fusion core needs, gathered by the caller so unit tests
/// need neither a real index nor a real graph.
#[derive(Default)]
pub struct SignalInputs {
    /// The full live file universe (`IndexSet::paths()`), sorted.
    pub all_paths: Vec<String>,
    /// keyword → per-file verified content-match counts (trigram search).
    pub content_hits: BTreeMap<String, Vec<(String, u32)>>,
    /// Files defining keyword-matching symbols (graph S2), pre-ranked.
    pub symbol_hits: Vec<SymbolHit>,
    /// 1-hop graph neighbors of the lexical seeds: (path, reason).
    pub graph_neighbors: Vec<(String, String)>,
    /// Cluster co-members of the lexical seeds: (path, reason).
    pub cluster_neighbors: Vec<(String, String)>,
    /// False when the graph could not be built — lexical-only mode.
    pub graph_available: bool,
    /// Aggregate unresolved-call envelope over matched symbol names.
    pub envelope: Option<pixel_graph::Envelope>,
    /// Caps the CALLER fired while gathering these inputs (e.g. "content
    /// probe truncated at 500 matches for keyword 'x'"). Any entry here
    /// forces `lower_bound: true` on the report envelope and is named in
    /// `envelope.caps` — a silently-capped signal must never be presented
    /// as exhaustive.
    pub caps: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TargetFile {
    pub path: String,
    pub tier: String, // "P0" | "P1" | "P2"
    pub score: f64,
    pub reasons: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub symbols: Vec<Value>,
}

#[derive(Debug, Serialize)]
pub struct TargetsReport {
    pub task: String,
    pub keywords: Vec<String>,
    pub exact_tokens: Vec<String>,
    pub targets: Vec<TargetFile>,
    pub envelope: Value,
    pub closed_world: String,
    pub stats: Value,
}

// RRF constants — mirrors pixel-recall/src/hybrid.rs, generalized to five
// channels. Filename and symbol-definition evidence dominate content TF;
// graph expansion and cluster co-membership are peripheral by construction.
pub const RRF_K: f64 = 60.0;
pub const W_FILENAME: f64 = 3.0;
pub const W_SYMBOL: f64 = 2.5;
pub const W_CONTENT: f64 = 1.5;
pub const W_GRAPH: f64 = 1.0;
pub const W_CLUSTER: f64 = 0.5;
/// Semantic similarity channel (static embeddings). Weighted between
/// content and symbol: semantic evidence is stronger than raw content
/// density (it captures paraphrase/synonym matches the lexical channel
/// misses) but weaker than a filename or symbol-definition hit (those are
/// deterministic structural signals). Only fused when the embedding model
/// is warm and the caller opts in (`scope: "hybrid"`).
pub const W_SEMANTIC: f64 = 1.5;
/// BM25 term-saturation parameter. 1.2 is the standard Robertson/Okapi
/// default: term frequency saturates quickly, so the 20th occurrence of a
/// term adds almost nothing over the 5th. This is the property raw match
/// counts lack — a big file that mentions a common term 200 times used to
/// bury a small file that mentions the rare term 3 times.
pub const BM25_K1: f64 = 1.2;
/// BM25 length-normalization strength. 0.75 is the standard default: fully
/// normalizing (b = 1.0) over-punishes long files, none (b = 0.0) lets them
/// dominate on raw volume alone.
pub const BM25_B: f64 = 0.75;

/// One scored document for [`bm25_rank`].
///
/// `term_freqs` is positionally aligned to the `terms` slice passed to
/// `bm25_rank` — `term_freqs[j]` is the number of occurrences of `terms[j]`
/// in this document. `len` is the document length in the SAME token unit
/// used to produce those frequencies.
#[derive(Debug, Clone)]
pub struct Bm25Doc {
    pub path: String,
    pub term_freqs: Vec<u32>,
    pub len: u32,
}

/// Okapi BM25 ranking over a candidate pool.
///
/// Replaces raw match-count density as the content signal. Three properties
/// raw counts do not have:
///   - IDF: a term matching most of the pool contributes almost nothing,
///     a rare term dominates (Lucene's `ln(1 + (N - df + 0.5)/(df + 0.5))`
///     form, which is always positive — the classic form can go negative
///     for terms in more than half the pool and invert the ranking).
///   - TF saturation (`k1`): the 20th occurrence adds ~nothing over the 5th.
///   - Length normalization (`b`): a long document does not outrank a short
///     one on raw volume alone.
///
/// Document frequency is WITHIN-POOL, matching [`idf_weight`]'s existing
/// convention: the pool is the set of files that already matched, so this
/// only ever REORDERS candidates, never introduces new ones.
///
/// Returns paths best-first. Fully deterministic: scores are compared on a
/// scaled integer domain (x1e6 truncation) to avoid float-ordering hazards,
/// and ties break by path ascending — the same discipline as `content_rank`.
/// Returns `None` when the pool carries no usable signal (no terms, or every
/// document scored zero), so callers can fall back rather than emit an
/// arbitrary path-ordered list.
pub fn bm25_rank(terms: &[String], docs: &[Bm25Doc]) -> Option<Vec<String>> {
    if terms.is_empty() || docs.is_empty() {
        return None;
    }
    let n = docs.len() as f64;
    let avgdl = {
        let total: u64 = docs.iter().map(|d| d.len as u64).sum();
        if total == 0 {
            return None;
        }
        total as f64 / n
    };
    // df per term, within-pool.
    let idfs: Vec<f64> = (0..terms.len())
        .map(|j| {
            let df = docs
                .iter()
                .filter(|d| d.term_freqs.get(j).copied().unwrap_or(0) > 0)
                .count() as f64;
            // Lucene form: strictly positive for every df in 0..=N.
            (1.0 + (n - df + 0.5) / (df + 0.5)).ln()
        })
        .collect();

    let mut scored: Vec<(i64, String)> = Vec::with_capacity(docs.len());
    let mut any_nonzero = false;
    for d in docs {
        let norm = 1.0 - BM25_B + BM25_B * (d.len as f64 / avgdl);
        let mut score = 0.0f64;
        for (j, idf) in idfs.iter().enumerate() {
            let tf = d.term_freqs.get(j).copied().unwrap_or(0) as f64;
            if tf == 0.0 {
                continue;
            }
            score += idf * (tf * (BM25_K1 + 1.0)) / (tf + BM25_K1 * norm);
        }
        if score > 0.0 {
            any_nonzero = true;
        }
        // Negate so an ascending sort yields best-first.
        scored.push((-((score * 1_000_000.0) as i64), d.path.clone()));
    }
    if !any_nonzero {
        return None;
    }
    scored.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    Some(scored.into_iter().map(|(_, p)| p).collect())
}

const EXACT_NAME_BONUS: f64 = 0.05;
const P0_CAP: usize = 5;
const CONTENT_COUNT_CAP: u32 = 50;
/// Score-gap ratio for precision mode: if the last P0 score is S and the first
/// P1 score is < S * SCORE_GAP_RATIO, drop all P1/P2 below the threshold.
/// 0.5 means a 50% score drop triggers the cutoff (P1 < P0 * 0.5).
const SCORE_GAP_RATIO: f64 = 0.5;
/// Secondary gap ratio for the no-P0 path: if #2 is < TOP * SECONDARY_GAP_RATIO,
/// keep only the top file. More aggressive than the primary ratio because
/// without P0 the top file is the only strong signal.
const SECONDARY_GAP_RATIO: f64 = 0.7;

// ---------------------------------------------------------------------------
// signal ranking
// ---------------------------------------------------------------------------

/// S1: rank the file universe by path-word keyword overlap. Filename-component
/// hits count double vs directory-component hits.
fn filename_rank(all_paths: &[String], keywords: &[String]) -> Vec<(String, Vec<String>)> {
    let kw: HashSet<&str> = keywords.iter().map(String::as_str).collect();
    let mut scored: Vec<(u32, String, Vec<String>)> = Vec::new();
    for path in all_paths {
        let mut matched: BTreeSet<String> = BTreeSet::new();
        let mut score = 0u32;
        let components: Vec<&str> = path.split('/').collect();
        let last = components.len().saturating_sub(1);
        for (i, comp) in components.iter().enumerate() {
            for w in split_ident_words(comp) {
                if kw.contains(w.as_str()) {
                    score += if i == last { 2 } else { 1 };
                    matched.insert(w);
                }
            }
        }
        if score > 0 {
            scored.push((score, path.clone(), matched.into_iter().collect()));
        }
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, p, m)| (p, m)).collect()
}

/// Inverse document frequency for a keyword, computed deterministically over
/// the matched candidate pool (the set of files that fired a content probe for
/// that keyword). This is WITHIN-POOL IDF: a term matching many candidate
/// files (e.g. `fn`, `return`, `impl`) is common and gets down-weighted;
/// a term matching few files is discriminative and gets boosted.
///
/// `max(1, df)` guards the denominator so an exact-name hit (df = 1) yields a
/// full boost rather than degenerate division. The `1 + ln(N/df)` form keeps
/// values in a bounded, stable range so RRF fusion inputs stay comparable.
/// Purely deterministic — same candidate pool, same scores.
fn idf_weight(kw: &str, content_hits: &BTreeMap<String, Vec<(String, u32)>>) -> f64 {
    let df = content_hits.get(kw).map(|v| v.len()).unwrap_or(1).max(1);
    let n = content_hits
        .values()
        .map(|v| v.len())
        .max()
        .unwrap_or(1)
        .max(1) as f64;
    1.0 + (n / df as f64).ln()
}

/// S3: combine per-keyword content hits into one ranked list.
///
/// Ranking is IDF-aware: each file's primary score is the sum of inverse
/// document frequencies over the DISTINCT keywords it matches. A rare,
/// discriminative keyword (low df) dominates; a common one close to `N`
/// (e.g. `fn`, `impl`) contributes almost nothing. Raw term frequency and
/// distinct-keyword count are secondary tiebreaks, so a high raw count of a
/// common term cannot bury a rare-keyword match — the BM25 insight. All
/// ties break by path ascending; fully deterministic.
fn content_rank(
    content_hits: &BTreeMap<String, Vec<(String, u32)>>,
) -> Vec<(String, Vec<(String, u32)>)> {
    struct Acc {
        per_kw: Vec<(String, u32)>,
        idf_sum: f64,
        total: u64,
    }
    let mut by_path: BTreeMap<String, Acc> = BTreeMap::new();
    for (kw, files) in content_hits {
        let idf = idf_weight(kw, content_hits);
        for (path, count) in files {
            let capped = (*count).min(CONTENT_COUNT_CAP);
            let acc = by_path.entry(path.clone()).or_insert_with(|| Acc {
                per_kw: Vec::new(),
                idf_sum: 0.0,
                total: 0,
            });
            acc.per_kw.push((kw.clone(), capped));
            acc.idf_sum += idf;
            acc.total += capped as u64;
        }
    }
    type ScoredRow = (i64, u64, String, Vec<(String, u32)>);
    let mut scored: Vec<ScoredRow> = by_path
        .into_iter()
        .map(|(p, acc)| {
            // Scale idf_sum to a comparable integer domain (x1000 truncation)
            // so the primary sort stays deterministic and free of float-path
            // hazards. Negate so ascending sort yields best-first.
            let scaled = (acc.idf_sum * 1000.0) as i64;
            (
                -scaled,
                acc.total,
                p,
                acc.per_kw.into_iter().collect(),
            )
        })
        .collect();
    scored.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)).then(a.2.cmp(&b.2)));
    scored.into_iter().map(|(_, _, p, kw)| (p, kw)).collect()
}

/// Reciprocal-rank fusion over weighted ranked lists. Each list contributes
/// `w / (k + rank + 1)` per path; scores are summed across lists and returned
/// best-first, ties broken by path ascending. Deterministic.
///
/// This is the shared fusion primitive used by [`lexical_rank`] and (in Phase
/// 1c) the full five-channel `compute_targets` path.
pub fn rrf_fuse(lists_with_weights: &[(&[String], f64)], k: f64) -> Vec<(String, f64)> {
    let mut scores: HashMap<String, f64> = HashMap::new();
    for (list, w) in lists_with_weights {
        for (rank, path) in list.iter().enumerate() {
            *scores.entry(path.clone()).or_default() += w / (k + rank as f64 + 1.0);
        }
    }
    let mut out: Vec<(String, f64)> = scores.into_iter().collect();
    out.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    out
}

/// Boost multiplier for a symbol kind. Real definitions (Function/Struct/
/// Class/Const) keep full weight; weaker / string-literal-only matches are
/// discounted. Line-level def/ref/comment classification is deferred (Phase
/// 1a only wires the helper; the caller decides where to apply it).
pub fn symbol_kind_boost(kind: &SymbolKind) -> f64 {
    match kind {
        SymbolKind::Function | SymbolKind::Struct | SymbolKind::Class | SymbolKind::Const => 1.0,
        _ => 0.8,
    }
}

/// Lexical-only pre-fuse used to pick graph-expansion seeds. Returns fused
/// paths, best first.
pub fn lexical_rank(
    all_paths: &[String],
    keywords: &[String],
    symbol_hits: &[SymbolHit],
    content_hits: &BTreeMap<String, Vec<(String, u32)>>,
) -> Vec<String> {
    let ranking_keywords: Vec<String> = keywords
        .iter()
        .cloned()
        .chain(expand_keywords(keywords))
        .collect();
    let s1: Vec<String> = filename_rank(all_paths, &ranking_keywords)
        .into_iter()
        .map(|(p, _)| p)
        .collect();
    let s2: Vec<String> = symbol_hits.iter().map(|h| h.path.clone()).collect();
    let s3: Vec<String> = content_rank(content_hits)
        .into_iter()
        .map(|(p, _)| p)
        .collect();

    rrf_fuse(
        &[(&s1, W_FILENAME), (&s2, W_SYMBOL), (&s3, W_CONTENT)],
        RRF_K,
    )
    .into_iter()
    .map(|(p, _)| p)
    .collect()
}

// ---------------------------------------------------------------------------
// fusion + tiering
// ---------------------------------------------------------------------------

fn round6(x: f64) -> f64 {
    (x * 1_000_000.0).round() / 1_000_000.0
}

fn symbol_value(sym: &pixel_graph::SymbolRow) -> Value {
    json!({
        "uid": sym.uid,
        "name": sym.name,
        "kind": sym.kind.as_str(),
        "line": sym.start_line,
    })
}

pub fn compute_targets(
    task: &str,
    query: &TaskQuery,
    inputs: SignalInputs,
    opts: &TargetsOptions,
) -> TargetsReport {
    let limit = opts.limit.clamp(1, MAX_LIMIT);

    // Broaden filename matching with semantically related terms without
    // changing the canonical keyword list used for content/symbol probes.
    let ranking_keywords: Vec<String> = query
        .keywords
        .iter()
        .cloned()
        .chain(expand_keywords(&query.keywords))
        .collect();

    // Per-signal ranked lists.
    let s1 = filename_rank(&inputs.all_paths, &ranking_keywords);
    let s2 = &inputs.symbol_hits;
    let s3 = content_rank(&inputs.content_hits);
    let s4 = &inputs.graph_neighbors;
    let s5 = &inputs.cluster_neighbors;

    // Fuse.
    #[derive(Default)]
    struct Entry {
        score: f64,
        families: u8, // bitmask S1..S5
        reasons: Vec<String>,
        symbols: Vec<Value>,
        exact_name_hit: bool,
    }
    let mut fused: HashMap<String, Entry> = HashMap::new();
    fn bump<'m>(
        fused: &'m mut HashMap<String, Entry>,
        path: &str,
        rank: usize,
        weight: f64,
        family: u8,
    ) -> &'m mut Entry {
        let e = fused.entry(path.to_string()).or_default();
        e.score += weight / (RRF_K + rank as f64 + 1.0);
        e.families |= family;
        e
    }

    for (rank, (path, matched)) in s1.iter().enumerate() {
        let e = bump(&mut fused, path, rank, W_FILENAME, 1);
        e.reasons
            .push(format!("filename match: {}", matched.join(", ")));
    }
    for (rank, hit) in s2.iter().enumerate() {
        let e = bump(&mut fused, &hit.path, rank, W_SYMBOL, 2);
        e.exact_name_hit |= hit.exact_name_hit;
        for (sym, _) in hit.symbols.iter().take(3) {
            e.reasons.push(format!("defines symbol `{}`", sym.name));
            e.symbols.push(symbol_value(sym));
        }
        if hit.symbols.len() > 3 {
            e.reasons
                .push(format!("+{} more matching symbols", hit.symbols.len() - 3));
        }
    }
    for (rank, (path, per_kw)) in s3.iter().enumerate() {
        let e = bump(&mut fused, path, rank, W_CONTENT, 4);
        let detail: Vec<String> = per_kw
            .iter()
            .take(3)
            .map(|(kw, n)| format!("{n} for \"{kw}\""))
            .collect();
        e.reasons
            .push(format!("content matches: {}", detail.join(", ")));
    }
    for (rank, (path, reason)) in s4.iter().enumerate() {
        let e = bump(&mut fused, path, rank, W_GRAPH, 8);
        e.reasons.push(reason.clone());
    }
    for (rank, (path, reason)) in s5.iter().enumerate() {
        let e = bump(&mut fused, path, rank, W_CLUSTER, 16);
        e.reasons.push(reason.clone());
    }

    // Exact-identifier definitions get a flat bonus: a backticked name that is
    // defined in the file is the strongest evidence a task can carry.
    for e in fused.values_mut() {
        if e.exact_name_hit {
            e.score += EXACT_NAME_BONUS;
        }
    }

    // Deterministic fused order.
    let mut ordered: Vec<(String, Entry)> = fused.into_iter().collect();
    ordered.sort_by(|a, b| b.1.score.total_cmp(&a.1.score).then(a.0.cmp(&b.0)));

    // Tier assignment. fams = number of signal families; lexical = S1|S2|S3.
    const LEXICAL_MASK: u8 = 1 | 2 | 4;
    const S1S2_MASK: u8 = 1 | 2;
    let p2_cap = limit.div_ceil(4);
    let mut targets: Vec<TargetFile> = Vec::new();
    let mut p0 = 0usize;
    let mut p2 = 0usize;
    // Cap accounting: every candidate this loop DROPS (p2 cap, limit) is a
    // file the caller will never see — that must surface as lower_bound +
    // a named cap, never be silently absorbed into an "exhaustive" claim.
    let mut p2_dropped = 0usize;
    let mut beyond_limit = 0usize;
    for (path, e) in ordered {
        if targets.len() >= limit {
            beyond_limit += 1;
            continue;
        }
        let fams = e.families.count_ones();
        let lexical = e.families & LEXICAL_MASK != 0;
        let tier = if lexical
            && p0 < P0_CAP.min(limit)
            && (e.exact_name_hit || (fams >= 2 && e.families & S1S2_MASK != 0))
        {
            p0 += 1;
            "P0"
        } else if lexical {
            "P1"
        } else {
            // Graph/cluster-only evidence: peripheral and droppable.
            if p2 >= p2_cap {
                p2_dropped += 1;
                continue;
            }
            p2 += 1;
            "P2"
        };
        // Structured symbol metadata (kind/line/uid) is dropped for P1/P2:
        // `reasons` already names the same symbols in prose ("defines
        // symbol `X`"), and duplicating that as a structured array for
        // every tier measurably bloated the response (3.6KB of an 11.8KB
        // response on a real 20-target query, 2026-08-30) without adding
        // information the caller needs for files it's told are peripheral
        // and droppable. P0 keeps it — that's the tier the doctrine
        // mandates checking, where the uid is worth the bytes for a
        // follow-up `pixel context`/`pixel impact` call.
        let symbols = if tier == "P0" { e.symbols } else { Vec::new() };
        targets.push(TargetFile {
            path,
            tier: tier.to_string(),
            score: round6(e.score),
            reasons: e.reasons,
            symbols,
        });
    }
    // Present P0 first, then P1, then P2, score order inside each tier
    // (already score-ordered globally; stable sort by tier preserves it).
    targets.sort_by(|a, b| a.tier.cmp(&b.tier));

    // Precision mode: score-gap cutoff. If the top file's score is much
    // higher than the rest, there's a clear winner and the lower files are
    // noise. Two strategies:
    // 1. P0→P1 gap: if last P0 score S, drop P1/P2 below S * (1 - RATIO).
    // 2. Top-file gap: if no P0, compare #1 vs #2. If #1 is > 1/RATIO times
    //    #2, keep only files within RATIO of #1.
    let mut precision_dropped = 0usize;
    if opts.precision_mode && targets.len() > 1 {
        let p0_scores: Vec<f64> = targets
            .iter()
            .filter(|t| t.tier == "P0")
            .map(|t| t.score)
            .collect();
        let threshold = if let Some(&last_p0_score) = p0_scores.last() {
            // P0 present: keep files scoring >= last_p0 * RATIO.
            Some(last_p0_score * SCORE_GAP_RATIO)
        } else {
            // No P0: use top-file gap with the more aggressive secondary ratio.
            let top = targets[0].score;
            let second = targets[1].score;
            if top > 0.0 && second < top * SECONDARY_GAP_RATIO {
                Some(top * SECONDARY_GAP_RATIO)
            } else {
                None
            }
        };
        if let Some(threshold) = threshold {
            let before = targets.len();
            let top_score = targets[0].score;
            // Always keep the top file; drop others below threshold.
            targets.retain(|t| t.score >= threshold || t.score == top_score);
            precision_dropped = before - targets.len();
        }
    }

    // Max-tier filter: drop files above the requested tier.
    let mut tier_dropped = 0usize;
    if let Some(ref max_tier) = opts.max_tier {
        let allow_p1 = max_tier == "P1" || max_tier == "P2";
        let allow_p2 = max_tier == "P2";
        let before = targets.len();
        targets.retain(|t| match t.tier.as_str() {
            "P0" => true,
            "P1" => allow_p1,
            "P2" => allow_p2,
            _ => true,
        });
        tier_dropped = before - targets.len();
    }

    // Envelope + closed-world claim. Three-outcome contract: a complete
    // answer (no cap fired, graph closed), an explicitly-bounded partial
    // answer (lower_bound + every cap NAMED), or the caller's ambiguity
    // path. "Exhaustive" may only be claimed when NOTHING was capped.
    let (graph_lower_bound, unresolved, graph_state) = if inputs.graph_available {
        let env = inputs.envelope.clone().unwrap_or(pixel_graph::Envelope {
            lower_bound: false,
            unresolved_same_name: 0,
        });
        (env.lower_bound, env.unresolved_same_name, "fresh")
    } else {
        (true, 0, "unavailable")
    };
    let mut caps: Vec<String> = inputs.caps.clone();
    if query.keywords_truncated {
        caps.push(format!(
            "task keywords truncated at {MAX_KEYWORDS}; later task words contributed no signal"
        ));
    }
    if p2_dropped > 0 {
        caps.push(format!(
            "P2 tier capped at {p2_cap}: dropped {p2_dropped} graph/cluster-evidenced file(s)"
        ));
    }
    if beyond_limit > 0 {
        caps.push(format!(
            "target list truncated at limit {limit}: {beyond_limit} scored candidate file(s) \
             beyond it"
        ));
    }
    if precision_dropped > 0 {
        caps.push(format!(
            "precision mode: {precision_dropped} low-score P1/P2 file(s) dropped by score-gap cutoff"
        ));
    }
    if tier_dropped > 0 {
        caps.push(format!(
            "max-tier filter: {tier_dropped} file(s) above tier {} dropped",
            opts.max_tier.as_deref().unwrap_or("?")
        ));
    }
    // ANY fired cap makes the list a lower bound — a capped signal cannot
    // support an exhaustive claim.
    let lower_bound = graph_lower_bound || !caps.is_empty();
    let note = if graph_state == "unavailable" {
        "code graph unavailable — lexical signals only; graph-adjacent files may be missing"
            .to_string()
    } else if graph_lower_bound {
        format!(
            "{unresolved} unresolved call site(s) share a matched symbol name; callers beyond this list may exist"
        )
    } else {
        String::new()
    };
    let mut closed_world = String::from(
        "Restrict reads and edits to the files listed. P2 entries are peripheral and droppable. ",
    );
    if lower_bound {
        // Explicitly-bounded partial answer: name every reason the list may
        // be incomplete instead of claiming exhaustiveness.
        closed_world
            .push_str("This list is a bounded partial answer, NOT exhaustive — EXCEPT clauses: ");
        let mut reasons: Vec<String> = Vec::new();
        if !note.is_empty() {
            reasons.push(note.clone());
        }
        reasons.extend(caps.iter().cloned());
        closed_world.push_str(&reasons.join("; "));
        closed_world.push('.');
    } else {
        // Only reachable when no cap fired and the graph closed the world.
        closed_world.push_str("This list is exhaustive for the indexed tree.");
    }

    let signal_hits = json!({
        "filename": s1.len(),
        "symbol": s2.len(),
        "content": s3.len(),
        "graph": s4.len(),
        "cluster": s5.len(),
    });

    TargetsReport {
        task: task.to_string(),
        keywords: query.keywords.clone(),
        exact_tokens: query.exact_tokens.clone(),
        targets,
        envelope: json!({
            "lower_bound": lower_bound,
            "graph": graph_state,
            "unresolved_same_name": unresolved,
            "note": note,
            "caps": caps,
        }),
        closed_world,
        stats: json!({
            "files_considered": inputs.all_paths.len(),
            "signal_hits": signal_hits,
            "limit": limit,
        }),
    }
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use pixel_graph::store::{SymbolKind, SymbolRow};

    fn hit(path: &str, name: &str, exact: bool, distinct: usize) -> SymbolHit {
        SymbolHit {
            path: path.to_string(),
            symbols: vec![(
                SymbolRow {
                    id: 1,
                    uid: format!("{path}#{name}#function"),
                    file_id: 1,
                    name: name.to_string(),
                    qualified: name.to_string(),
                    kind: SymbolKind::Function,
                    start_line: 1,
                    end_line: 5,
                    sig: String::new(),
                },
                name.to_string(),
            )],
            distinct_keywords: distinct,
            exact_name_hit: exact,
        }
    }

    #[test]
    fn tokenize_splits_idents_and_drops_stopwords() {
        let q = tokenize_task("fix loginUser session handling in AuthService").unwrap();
        assert_eq!(
            q.keywords,
            vec!["login", "user", "session", "handling", "auth", "service"]
        );
        assert!(q.exact_tokens.is_empty());
    }

    #[test]
    fn tokenize_extracts_backticked_identifiers() {
        let q = tokenize_task("`login_user` panics on empty token").unwrap();
        assert_eq!(q.exact_tokens, vec!["login_user"]);
        assert_eq!(
            q.keywords,
            vec!["login", "user", "panics", "empty", "token"]
        );
    }

    #[test]
    fn tokenize_all_stopwords_errors() {
        assert!(tokenize_task("fix the code in a file").is_err());
        assert!(tokenize_task("").is_err());
    }

    #[test]
    fn tokenize_technical_compounds_and_short_keywords() {
        let q_csharp = tokenize_task("add c# support").unwrap();
        assert_eq!(q_csharp.keywords, vec!["csharp"]);

        let q_cpp = tokenize_task("add c++ support").unwrap();
        assert_eq!(q_cpp.keywords, vec!["cpp"]);

        let q_go = tokenize_task("add go support").unwrap();
        assert_eq!(q_go.keywords, vec!["go"]);

        let q_dotnet = tokenize_task("support for .NET and Node.js").unwrap();
        assert_eq!(q_dotnet.keywords, vec!["dotnet", "nodejs"]);

        let expanded = expand_keywords(&q_csharp.keywords);
        assert!(expanded.contains(&"language".to_string()));
        assert!(expanded.contains(&"extract".to_string()));
    }

    #[test]
    fn multi_signal_file_beats_single_signal_top() {
        // a.rs: filename + content. b.rs: content only (top of that channel).
        let mut content = BTreeMap::new();
        content.insert(
            "login".to_string(),
            vec![
                ("src/b.rs".to_string(), 40u32),
                ("src/login.rs".to_string(), 2u32),
            ],
        );
        let inputs = SignalInputs {
            all_paths: vec!["src/b.rs".into(), "src/login.rs".into()],
            content_hits: content,
            graph_available: true,
            ..Default::default()
        };
        let q = TaskQuery {
            keywords_truncated: false,
            exact_tokens: vec![],
            keywords: vec!["login".into()],
        };
        let report = compute_targets("t", &q, inputs, &TargetsOptions::default());
        assert_eq!(report.targets[0].path, "src/login.rs");
        assert_eq!(report.targets[0].tier, "P0"); // filename + content = 2 families
        assert_eq!(report.targets[1].path, "src/b.rs");
        assert_eq!(report.targets[1].tier, "P1"); // content only
    }

    #[test]
    fn exact_name_hit_is_p0_and_cluster_only_is_p2() {
        let inputs = SignalInputs {
            all_paths: vec!["src/a.rs".into(), "src/z.rs".into()],
            symbol_hits: vec![hit("src/a.rs", "login_user", true, 1)],
            cluster_neighbors: vec![("src/z.rs".into(), "same cluster 'auth'".into())],
            graph_available: true,
            ..Default::default()
        };
        let q = TaskQuery {
            keywords_truncated: false,
            exact_tokens: vec!["login_user".into()],
            keywords: vec!["login".into(), "user".into()],
        };
        let report = compute_targets("t", &q, inputs, &TargetsOptions::default());
        let a = report
            .targets
            .iter()
            .find(|t| t.path == "src/a.rs")
            .unwrap();
        assert_eq!(a.tier, "P0");
        assert!(
            a.reasons
                .iter()
                .any(|r| r.contains("defines symbol `login_user`"))
        );
        let z = report
            .targets
            .iter()
            .find(|t| t.path == "src/z.rs")
            .unwrap();
        assert_eq!(z.tier, "P2");
    }

    #[test]
    fn limit_is_hard_and_p2_capped() {
        let all: Vec<String> = (0..30).map(|i| format!("src/login_{i:02}.rs")).collect();
        let cluster: Vec<(String, String)> = (0..10)
            .map(|i| (format!("src/extra_{i:02}.rs"), "same cluster 'x'".into()))
            .collect();
        let inputs = SignalInputs {
            all_paths: all.clone(),
            cluster_neighbors: cluster,
            graph_available: true,
            ..Default::default()
        };
        let q = TaskQuery {
            keywords_truncated: false,
            exact_tokens: vec![],
            keywords: vec!["login".into()],
        };
        let report = compute_targets(
            "t",
            &q,
            inputs,
            &TargetsOptions {
                limit: 8,
                max_tier: None,
                precision_mode: false,
            },
        );
        assert_eq!(report.targets.len(), 8);
        let p2 = report.targets.iter().filter(|t| t.tier == "P2").count();
        assert!(p2 <= 2); // ceil(8/4)
    }

    #[test]
    fn deterministic_output() {
        let mk = || {
            let mut content = BTreeMap::new();
            content.insert(
                "auth".to_string(),
                vec![
                    ("src/x.rs".to_string(), 3u32),
                    ("src/y.rs".to_string(), 3u32),
                ],
            );
            SignalInputs {
                all_paths: vec!["src/x.rs".into(), "src/y.rs".into()],
                content_hits: content,
                graph_available: true,
                ..Default::default()
            }
        };
        let q = TaskQuery {
            keywords_truncated: false,
            exact_tokens: vec![],
            keywords: vec!["auth".into()],
        };
        let a = serde_json::to_string(&compute_targets("t", &q, mk(), &TargetsOptions::default()))
            .unwrap();
        let b = serde_json::to_string(&compute_targets("t", &q, mk(), &TargetsOptions::default()))
            .unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn graph_unavailable_sets_lower_bound() {
        let inputs = SignalInputs {
            all_paths: vec!["src/login.rs".into()],
            graph_available: false,
            ..Default::default()
        };
        let q = TaskQuery {
            keywords_truncated: false,
            exact_tokens: vec![],
            keywords: vec!["login".into()],
        };
        let report = compute_targets("t", &q, inputs, &TargetsOptions::default());
        assert_eq!(report.envelope["lower_bound"], true);
        assert_eq!(report.envelope["graph"], "unavailable");
        assert!(report.closed_world.contains("EXCEPT"));
    }

    #[test]
    fn rrf_fuse_sums_weighted_ranks_and_breaks_ties_by_path() {
        let a = vec!["x.rs".to_string(), "y.rs".to_string()];
        let b = vec!["y.rs".to_string(), "z.rs".to_string()];
        let fused = rrf_fuse(&[(&a, 3.0), (&b, 1.5)], RRF_K);
        // y.rs appears in both lists → strictly higher score than x.rs/z.rs.
        assert_eq!(fused[0].0, "y.rs");
        assert!(fused[0].1 > fused[1].1);
        // Deterministic tie-break: x.rs before z.rs at equal score.
        assert_eq!(fused[1].0, "x.rs");
        assert_eq!(fused[2].0, "z.rs");
    }

    #[test]
    fn symbol_kind_boost_prioritizes_definitions() {
        use pixel_graph::store::SymbolKind;
        assert_eq!(symbol_kind_boost(&SymbolKind::Function), 1.0);
        assert_eq!(symbol_kind_boost(&SymbolKind::Struct), 1.0);
        assert_eq!(symbol_kind_boost(&SymbolKind::Class), 1.0);
        assert_eq!(symbol_kind_boost(&SymbolKind::Const), 1.0);
        assert!(symbol_kind_boost(&SymbolKind::Enum) < 1.0);
        assert!(symbol_kind_boost(&SymbolKind::Module) < 1.0);
    }

    fn bm25_doc(path: &str, tfs: &[u32], len: u32) -> Bm25Doc {
        Bm25Doc {
            path: path.into(),
            term_freqs: tfs.to_vec(),
            len,
        }
    }

    #[test]
    fn bm25_idf_beats_raw_count() {
        // terms[0]="fn" (in both docs, common), terms[1]="ledger" (rare).
        // a.rs has 40 hits of the common term; b.rs has 3 of the rare one.
        // Raw density ranks a.rs first; BM25 must rank b.rs first.
        let terms = vec!["fn".to_string(), "ledger".to_string()];
        let docs = vec![
            bm25_doc("a.rs", &[40, 0], 100),
            bm25_doc("b.rs", &[1, 3], 100),
        ];
        let ranked = bm25_rank(&terms, &docs).expect("signal present");
        assert_eq!(
            ranked[0], "b.rs",
            "rare-term match must outrank common-term volume"
        );
    }

    #[test]
    fn bm25_saturates_term_frequency() {
        // Same single term, same length: 5 hits vs 50 hits. The 50-hit doc
        // still wins, but by far less than 10x — that is saturation.
        let terms = vec!["ledger".to_string()];
        let few = vec![
            bm25_doc("few.rs", &[5], 100),
            bm25_doc("many.rs", &[50], 100),
        ];
        let ranked = bm25_rank(&terms, &few).expect("signal present");
        assert_eq!(ranked[0], "many.rs");
        // Score ratio must be well under the 10x raw-count ratio.
        let score = |tf: f64| {
            let norm = 1.0 - BM25_B + BM25_B * 1.0;
            (tf * (BM25_K1 + 1.0)) / (tf + BM25_K1 * norm)
        };
        assert!(
            score(50.0) / score(5.0) < 2.0,
            "tf must saturate, not scale linearly"
        );
    }

    #[test]
    fn bm25_normalizes_document_length() {
        // Equal term frequency, very different lengths: the short, dense doc
        // must win. Raw count would tie them.
        let terms = vec!["ledger".to_string()];
        let docs = vec![
            bm25_doc("long.rs", &[3], 1000),
            bm25_doc("short.rs", &[3], 50),
        ];
        let ranked = bm25_rank(&terms, &docs).expect("signal present");
        assert_eq!(
            ranked[0], "short.rs",
            "shorter doc with same tf must rank first"
        );
    }

    #[test]
    fn bm25_returns_none_without_signal() {
        let terms = vec!["ledger".to_string()];
        assert!(
            bm25_rank(&[], &[bm25_doc("a.rs", &[], 10)]).is_none(),
            "no terms"
        );
        assert!(bm25_rank(&terms, &[]).is_none(), "no docs");
        // Every doc scores zero -> caller must fall back, not get path order.
        let zero = vec![bm25_doc("a.rs", &[0], 10), bm25_doc("b.rs", &[0], 10)];
        assert!(bm25_rank(&terms, &zero).is_none(), "all-zero pool");
        // Zero-length pool is degenerate (avgdl undefined).
        let empty_len = vec![bm25_doc("a.rs", &[1], 0)];
        assert!(bm25_rank(&terms, &empty_len).is_none(), "zero total length");
    }

    #[test]
    fn bm25_is_deterministic_and_breaks_ties_by_path() {
        let terms = vec!["ledger".to_string()];
        let docs = vec![
            bm25_doc("z.rs", &[3], 100),
            bm25_doc("a.rs", &[3], 100),
            bm25_doc("m.rs", &[3], 100),
        ];
        let first = bm25_rank(&terms, &docs).expect("signal present");
        assert_eq!(
            first,
            vec!["a.rs", "m.rs", "z.rs"],
            "identical scores tie by path asc"
        );
        for _ in 0..5 {
            assert_eq!(
                bm25_rank(&terms, &docs).unwrap(),
                first,
                "must be deterministic"
            );
        }
    }

    #[test]
    fn idf_downweights_common_content_terms() {
        // Two keywords, both matching the same two files. The rare term
        // 'ledger' matches only 1 of the 2 files (df=1) → high IDF; the
        // common term 'fn' matches both (df=2) → low IDF. Even when the
        // common term has a higher raw count, the rare term's file must rank
        // first because IDF dominates.
        let mut content = BTreeMap::new();
        content.insert(
            "fn".to_string(),
            vec![
                ("src/a.rs".to_string(), 40u32),
                ("src/b.rs".to_string(), 1u32),
            ],
        );
        content.insert("ledger".to_string(), vec![("src/b.rs".to_string(), 2u32)]);
        let ranked = content_rank(&content);
        // 'b.rs' hits the rare, high-IDF term 'ledger'; 'a.rs' only hits the
        // common 'fn'. IDF must push b.rs ahead despite a.rs's higher raw count.
        assert_eq!(ranked[0].0, "src/b.rs");
        assert_eq!(ranked[1].0, "src/a.rs");

        // idf_weight: a term present in all matched files dips below one
        // that appears in only one file.
        let common = idf_weight("fn", &content);
        let rare = idf_weight("ledger", &content);
        assert!(rare > common);
        // A single exact-name hit yields a full boost (df=1 → ln(N/1)).
        assert!(idf_weight("unique", &content) > 1.0);
    }
}

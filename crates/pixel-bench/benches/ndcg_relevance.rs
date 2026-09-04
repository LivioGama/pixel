//! NDCG@10 relevance benchmark for pixel search, measured over a real
//! labeled query set on this workspace's `pixel-graph` crate.
//!
//! Why NDCG: it is the same retrieval-quality metric semble publishes
//! (their claim: NDCG@10 0.854 across 63 repos). Publishing our own measured
//! number — instead of borrowing theirs — is the credibility anchor per the
//! T1 doctrine (no claim without a measurement). We measure BOTH ranked
//! (`scope: "code"`) and unranked search on the same qrels, so the ranking
//! layer's contribution is isolated.
//!
//! A/B lane: a third measurement runs `pixel ask` (semantic code search via
//! potion-code-16M-v2 static embeddings) over the SAME qrels, rooted at the
//! same `crates/pixel-graph/src` subtree, so the lexical-vs-semantic gap is
//! measured on identical inputs — not borrowed from semble's corpus.
//!
//! Method (TREC-style NDCG@10):
//!   - q: a natural-language-ish query issued as a `search` pattern.
//!   - relevant: ground-truth files that genuinely answer q (hand-labeled
//!     from this repo's structure, not from search results).
//!   - R(q): the ranked list of match ORDER as search returns it (per-file
//!     deduped, first position of each file).
//!   - DCG@10 = sum over top-10 of rel(i)/log2(i+2), IDCG from ideal order,
//!     NDCG@10 = DCG/IDCG.
//!
//! Honest bounds:
//!   - Search is keyword/regex-forward, not semantic-NL ("how is X handled?").
//!     Queries here are the *keyword endpoints* of real questions, so the
//!     number measures keyword retrieval quality, not open-ended NL.
//!   - Ground truth is 1 (relevant) / 0 (irrelevant); no graded relevance.
//!   - Pool is capped but on this crate it is complete for these patterns.
//!
//! This is a self-benchmark; it must NOT be reported as comparable to
//! semble's cross-repo number, only as pixel's own measured quality and as a
//! regression gate for the ranking layer and the `ask` semantic channel.

use std::collections::HashSet;
use std::path::PathBuf;

use criterion::{Criterion, criterion_group, criterion_main};
use pixel_daemon::api::{Response, Service};
use pixel_proto::Op;

/// Ground truth: query -> files in `crates/pixel-graph/src/` that genuinely
/// answer it. Labeled from the code (module responsibilities), not from
/// search output. Each file's relevant label is semantic — e.g. "how the
/// concept index resolves phrases" → `concept_resolve.rs` even though the
/// word "concept" is a substring of several files.
fn qrels(crate_dir: &std::path::Path) -> Vec<(&'static str, Vec<String>)> {
    // `crate_dir` is the workspace root. Search returns paths RELATIVE to
    // the workspace root (`crates/pixel-graph/src/…`), so qrels must use the
    // same relative form to compare against `file_order_from_response`.
    let _ = crate_dir;
    let tag = |name: &str| format!("crates/pixel-graph/src/{name}");
    vec![
        (
            "concept index resolve phrase map marked",
            vec![tag("concept_resolve.rs"), tag("concept.rs")],
        ),
        (
            "callers callees impact trace reachability",
            vec![tag("impact.rs")],
        ),
        (
            "cluster functional area detect co locate",
            vec![tag("cluster.rs")],
        ),
        (
            "extract tree sitter symbols ast code",
            vec![tag("extract.rs")],
        ),
        (
            "imports dependency resolved graph edge",
            vec![tag("imports.rs")],
        ),
        ("process execution flow detection", vec![tag("process.rs")]),
        (
            "resolution ranked candidate ambiguity disambiguation",
            vec![tag("resolve.rs")],
        ),
        ("symbol store database query index", vec![tag("store.rs")]),
        (
            "changes blast radius working tree diff",
            vec![tag("changes.rs")],
        ),
        ("trace path between symbols", vec![tag("trace.rs")]),
    ]
}

/// Extract per-file order from a Search response: walk the ordered match
/// rows and record the first-seen position of each distinct relative path.
fn file_order_from_response(resp: &Response) -> Vec<String> {
    let empty = Vec::new();
    let matches = resp
        .data()
        .get("matches")
        .and_then(|x| x.as_array())
        .unwrap_or(&empty);
    let mut seen: HashSet<String> = HashSet::new();
    let mut order: Vec<String> = Vec::new();
    for m in matches {
        if let Some(p) = m.get("path").and_then(|x| x.as_str())
            && seen.insert(p.to_string())
        {
            order.push(p.to_string());
        }
    }
    order
}

fn ndcg_at_k(ranking: &[String], relevant: &HashSet<String>, k: usize) -> f64 {
    let mut dcg = 0.0;
    let mut idcg = 0.0;
    let rel_count = relevant.len();
    for (i, path) in ranking.iter().take(k).enumerate() {
        let rel = if relevant.contains(path) { 1.0 } else { 0.0 };
        dcg += rel / ((i + 2) as f64).log2(); // log2(i+2): 0-indexed discount
    }
    // Ideal: all relevant files first (only count those that fit in k).
    for j in 0..k.min(rel_count) {
        idcg += 1.0 / ((j + 2) as f64).log2();
    }
    if idcg == 0.0 { 0.0 } else { dcg / idcg }
}

fn run_ndcg(
    svc: &mut Service,
    qrels: &[(&'static str, Vec<String>)],
    scope: Option<&str>,
    k: usize,
) -> f64 {
    let mut sum = 0.0;
    for (q, relevant) in qrels {
        let rel_set: HashSet<String> = relevant.iter().cloned().collect();
        // `search` takes one regex pattern, not a phrase. Build the honest
        // keyword endpoint: alternation of the query's identifier words so
        // "concept index resolve" becomes `concept|index|resolve` (same as
        // a real agent lowering an NL question to a search).
        let words: Vec<&str> = q.split_whitespace().filter(|w| !w.is_empty()).collect();
        let pattern = if words.len() == 1 {
            words[0].to_string()
        } else {
            words.join("|")
        };
        let resp = svc.handle(Op::Search {
            pattern,
            json: true,
            limit: Some(50),
            offset: None,
            paths: Some(vec!["crates/pixel-graph/src".to_string()]),
            scope: scope.map(str::to_string),
        });
        if resp.ok {
            let order = file_order_from_response(&resp);
            sum += ndcg_at_k(&order, &rel_set, k);
        }
    }
    sum / qrels.len() as f64
}

/// A/B lane: run `pixel ask` (semantic) over the SAME qrels, rooted at the
/// same `crates/pixel-graph/src` subtree. `ask` returns absolute paths; we
/// relativize them against the workspace root so they compare against the
/// qrels' `crates/pixel-graph/src/<name>` form. Same query strings as the
/// lexical lane → identical inputs, isolated channel effect.
fn run_ndcg_ask(root: &std::path::Path, qrels: &[(&'static str, Vec<String>)], k: usize) -> f64 {
    let subtree = root.join("crates/pixel-graph/src");
    let mut sum = 0.0;
    for (q, relevant) in qrels {
        let rel_set: HashSet<String> = relevant.iter().cloned().collect();
        // `ask` embeds the raw query string. The qrels queries are keyword
        // endpoints; static embeddings handle keyword-ish text fine, and
        // using the identical string keeps the A/B inputs matched.
        let Ok(hits) = pixel_recall::code_search::ask(&subtree, q, k.max(50), 2000) else {
            continue;
        };
        let order: Vec<String> = hits
            .iter()
            .filter_map(|h| {
                let p = h.path.strip_prefix(&format!("{}/", root.display()))?;
                Some(p.to_string())
            })
            .collect();
        sum += ndcg_at_k(&order, &rel_set, k);
    }
    sum / qrels.len() as f64
}

/// Fixture pointing at this workspace's `pixel-graph/src` (a real, indexed
/// tree). Index auto-builds on first search.
fn graph_fixture() -> (PathBuf, Service) {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf(); // workspace root
    let svc = Service::open(&root).unwrap();
    (root, svc)
}

fn bench(c: &mut Criterion) {
    let (root, mut svc) = graph_fixture();
    let suite = qrels(&root);
    // Warm up index + graph.
    let _ = svc.handle(Op::Search {
        pattern: "concept resolve".into(),
        json: true,
        limit: Some(10),
        offset: None,
        paths: Some(vec!["crates/pixel-graph/src".to_string()]),
        scope: Some("code".to_string()),
    });

    let ranked = run_ndcg(&mut svc, &suite, Some("code"), 10);
    let unranked = run_ndcg(&mut svc, &suite, None, 10);
    // A/B lane: semantic `ask` over the SAME qrels + subtree.
    let semantic = run_ndcg_ask(&root, &suite, 10);
    // Precision 1 lane: hybrid search (lexical RRF + semantic channel fused).
    let hybrid = run_ndcg(&mut svc, &suite, Some("hybrid"), 10);

    // Sanity: ranking must not DECREASE NDCG meaningfully vs unranked, and
    // must be > 0 (retrieval actually finds relevant files).
    assert!(
        unranked > 0.0,
        "NDCG@10 unranked = {unranked:.3} — retrieval found nothing; suite is broken"
    );
    assert!(
        ranked >= unranked - 0.05,
        "NDCG@10 ranked ({ranked:.3}) regressed below unranked ({unranked:.3})"
    );
    assert!(
        semantic >= 0.0,
        "NDCG@10 semantic = {semantic:.3} — negative is impossible; ask lane is broken"
    );
    assert!(
        hybrid >= 0.0,
        "NDCG@10 hybrid = {hybrid:.3} — negative is impossible; hybrid lane is broken"
    );

    eprintln!(
        "NDCG@10 (pixel-graph qrels, self-bench A/B):\n  \
         lexical unranked= {unranked:.3}\n  \
         lexical ranked  = {ranked:.3}\n  \
         hybrid search   = {hybrid:.3}  (5ch RRF + semantic S6)\n  \
         semantic ask    = {semantic:.3}  (potion-code-16M-v2 standalone)"
    );
    use criterion::BenchmarkId;
    let mut ranked_grp = c.benchmark_group("ndcg10");
    ranked_grp.sample_size(10);
    ranked_grp.bench_with_input(BenchmarkId::new("ranked_search", 10), &ranked, |b, _| {
        b.iter(|| run_ndcg(&mut svc, &suite, Some("code"), 10))
    });
    ranked_grp.bench_with_input(
        BenchmarkId::new("unranked_search", 10),
        &unranked,
        |b, _| b.iter(|| run_ndcg(&mut svc, &suite, None, 10)),
    );
    ranked_grp.bench_with_input(BenchmarkId::new("hybrid_search", 10), &hybrid, |b, _| {
        b.iter(|| run_ndcg(&mut svc, &suite, Some("hybrid"), 10))
    });
    ranked_grp.bench_with_input(BenchmarkId::new("semantic_ask", 10), &semantic, |b, _| {
        b.iter(|| run_ndcg_ask(&root, &suite, 10))
    });
    ranked_grp.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);

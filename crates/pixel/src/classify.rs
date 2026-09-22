//! `pixel classify` — deterministic zero-shot label decision, no LLM.
//!
//! The decision half of the refinement contract: embed the text to judge and
//! each option's criterion with the shared static embedding model, then map
//! cosine similarities through a fixed-temperature softmax. The output is a
//! probability distribution over the caller's labels — the same shape a
//! Jev-class decision model returns, produced deterministically and offline.
//! No daemon, no index required.
//!
//! # Why `context` is a field and not a prefix on `text`
//!
//! The model is a static embedding (Model2Vec): a document's vector is the
//! mean of its token vectors, so every character of `text` competes for the
//! same budget. Framing that is identical for all candidates — the question,
//! the rubric preamble — therefore *dilutes* the part that actually varies,
//! and it pulls every query toward the same point, compressing exactly the
//! cosine gaps the decision reads. Folded into each candidate instead, the
//! same words are common to both sides of every comparison and cancel.
//!
//! This is not a tuning knob; it is where shared text has to go for a
//! mean-pooled model. Measured on the 231 public JevBench v1.2 items, moving
//! the shared instructions out of `text` and into `context` took the tiers
//! from 64.6 / 36.1 / 45.0 % to 89.6 / 54.2 / 47.7 % — chance-corrected
//! Intelligence 19.5 → 38.3 (`docs/bench/jevbench.md`).
//!
//! `pixel classify --jsonl` serves one decision per stdin line with the
//! model resident, so per-decision latency never includes model load.

use pixel_recall::embed::{EmbedKind, Embedder};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// Softmax temperature over cosine similarities. Fixed a priori — CLIP's
/// learned temperature — never tuned on any benchmark item.
pub const TAU: f64 = 0.07;
/// Input cap: static embeddings mean-pool, so longer text only dilutes the
/// signal; bounding it keeps worst-case latency flat.
const TEXT_CAP_CHARS: usize = 32_768;

/// One decision request: the text to judge, the framing every candidate
/// shares, the allowed labels, and an optional criterion description per
/// label (what the label *means*).
#[derive(Debug, Clone, PartialEq)]
pub struct Spec {
    /// The part that varies from decision to decision — the state to judge.
    pub text: String,
    /// Framing shared by every candidate: the question being asked, the
    /// rubric preamble. Embedded into each candidate, never into `text` —
    /// see the module docs for why that placement decides the answer.
    pub context: String,
    pub labels: Vec<String>,
    pub criteria: BTreeMap<String, String>,
}

impl Spec {
    /// Validated construction: at least two distinct non-empty labels, and
    /// every criterion must name a real label (a stray criterion silently
    /// skews the distribution if dropped, so it is an error instead).
    pub fn checked(
        text: String,
        context: String,
        labels: Vec<String>,
        criteria: BTreeMap<String, String>,
    ) -> Result<Self, String> {
        let mut seen = std::collections::HashSet::new();
        if labels
            .iter()
            .any(|l| l.is_empty() || !seen.insert(l.clone()))
        {
            return Err("labels must be non-empty and distinct".to_string());
        }
        if labels.len() < 2 {
            return Err("at least two labels are required".to_string());
        }
        if let Some(bad) = criteria.keys().find(|k| !labels.contains(k)) {
            return Err(format!("criterion for unknown label {bad:?}"));
        }
        Ok(Spec {
            text,
            context,
            labels,
            criteria,
        })
    }
}

/// What gets embedded for a candidate: the shared `context`, then the
/// criterion text when the caller supplied one, else the label name itself.
/// Every candidate carries the same context, so it cancels in the comparison
/// instead of diluting the query.
fn candidate_text(label: &str, spec: &Spec) -> String {
    let base = spec.criteria.get(label).map_or(label, String::as_str);
    if spec.context.is_empty() {
        return base.to_string();
    }
    let context = &spec.context;
    format!("{context} {base}")
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let (mut dot, mut na, mut nb) = (0.0f32, 0.0f32, 0.0f32);
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// Softmax over similarities with the fixed temperature. Subtracting the
/// max keeps exp() finite for any input.
fn softmax(sims: &[f64]) -> Vec<f64> {
    let max = sims.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let exps: Vec<f64> = sims.iter().map(|s| ((s - max) / TAU).exp()).collect();
    let sum: f64 = exps.iter().sum();
    exps.iter().map(|e| e / sum).collect()
}

/// The decision core over an embedding seam: tests inject a fake Embedder.
/// Returns label → probability in the caller's label order.
fn decide(embedder: &mut dyn Embedder, spec: &Spec) -> Result<BTreeMap<String, f64>, String> {
    let candidates: Vec<String> = spec
        .labels
        .iter()
        .map(|l| candidate_text(l, spec))
        .collect();
    // Kind is per-batch, so question and candidates embed in two calls:
    // E5-family models prefix queries and passages differently.
    let query = embedder
        .embed_batch(&[spec.text.as_str()], EmbedKind::Query)?
        .into_iter()
        .next()
        .ok_or("embedder returned no query vector")?;
    let refs: Vec<&str> = candidates.iter().map(String::as_str).collect();
    let cand_vecs = embedder.embed_batch(&refs, EmbedKind::Passage)?;
    let sims: Vec<f64> = cand_vecs.iter().map(|v| cosine(&query, v) as f64).collect();
    Ok(spec.labels.iter().cloned().zip(softmax(&sims)).collect())
}

/// The argmax label — first in the caller's label order on a tie
/// (deterministic, never alphabetical accident).
fn predicted<'a>(probs: &BTreeMap<String, f64>, labels: &'a [String]) -> &'a str {
    // max_by returns the LAST maximum; reversing makes a tie resolve to the
    // first label in the caller's order.
    labels
        .iter()
        .rev()
        .max_by(|a, b| probs[*a].total_cmp(&probs[*b]))
        .map_or("", String::as_str)
}

/// The per-decision JSON document: probs, argmax, and the epistemics
/// envelope — `basis` records that this is embedding similarity, not a
/// trained classifier.
fn document(model: &str, spec: &Spec, probs: &BTreeMap<String, f64>) -> Value {
    json!({
        "marker": "complete",
        "predicted": predicted(probs, &spec.labels),
        "probs": probs,
        "epistemics": {
            "closed_world": false,
            "lower_bound": false,
            "basis": "zero-shot embedding similarity (cosine + softmax), not a trained classifier",
            "confidence": "complete",
        },
        "snapshot": {
            "model": model,
            "temperature": TAU,
            "labels": spec.labels,
        },
    })
}

fn clip_text(text: &str) -> String {
    text.chars().take(TEXT_CAP_CHARS).collect()
}

/// Open the shared embedding model; first call may download it.
#[cfg_attr(test, mutants::skip)] // thin adapter over the model cache; logic lives in `decide`
fn open_embedder() -> Result<Box<dyn Embedder>, String> {
    pixel_recall::embed::open_default_embedder(true)
}

/// Parse one JSONL spec line (serve mode and tests share this path).
fn parse_spec_line(line: &str) -> Result<Spec, String> {
    let v: Value = serde_json::from_str(line).map_err(|e| format!("invalid spec JSON: {e}"))?;
    let text = v
        .get("text")
        .and_then(Value::as_str)
        .ok_or("spec needs a \"text\" string")?
        .to_string();
    let context = v
        .get("context")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let labels = v
        .get("labels")
        .and_then(Value::as_array)
        .ok_or("spec needs a \"labels\" array")?
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect::<Vec<_>>();
    let criteria = v
        .get("criteria")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default();
    Spec::checked(clip_text(&text), clip_text(&context), labels, criteria)
}

/// One result line per spec line; a bad line fails that line only — the
/// stream keeps serving (a benchmark run must not abort on one item).
#[cfg_attr(test, mutants::skip)] // stdin/stdout adapter; logic lives in `parse_spec_line`/`decide`
fn serve_jsonl(embedder: &mut dyn Embedder) -> Result<(), String> {
    use std::io::BufRead;
    let stdin = std::io::stdin();
    let model = embedder.model_id().to_string();
    for line in stdin.lock().lines() {
        let line = line.map_err(|e| format!("stdin read: {e}"))?;
        if line.trim().is_empty() {
            continue;
        }
        let out = match parse_spec_line(&line).and_then(|s| decide(embedder, &s).map(|p| (s, p))) {
            Ok((spec, probs)) => {
                let mut doc = document(&model, &spec, &probs);
                doc["ok"] = json!(true);
                doc
            }
            Err(e) => json!({"ok": false, "error": e}),
        };
        let line_out = serde_json::to_string(&out).map_err(|e| format!("result encode: {e}"))?;
        crate::write_stdout(&line_out)?;
        crate::write_stdout("\n")?;
    }
    Ok(())
}

pub struct ClassifyOptions {
    pub text: Option<String>,
    pub context: Option<String>,
    pub labels: Vec<String>,
    pub criteria: Vec<String>,
    pub jsonl: bool,
    pub json: bool,
}

/// Parse `--criterion label=description` pairs.
fn parse_criteria(pairs: &[String]) -> Result<BTreeMap<String, String>, String> {
    pairs
        .iter()
        .map(|p| {
            p.split_once('=')
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .ok_or_else(|| format!("--criterion needs label=description, got {p:?}"))
        })
        .collect()
}

#[cfg_attr(test, mutants::skip)] // model/IO adapter; logic lives in `decide`/`document`/`parse_spec_line`
pub fn run(opts: ClassifyOptions) -> Result<(), String> {
    let mut embedder = open_embedder()?;
    if opts.jsonl {
        return serve_jsonl(embedder.as_mut());
    }
    let text = opts
        .text
        .ok_or("classify needs a text argument (or --jsonl)")?;
    let spec = Spec::checked(
        clip_text(&text),
        clip_text(opts.context.as_deref().unwrap_or_default()),
        opts.labels.clone(),
        parse_criteria(&opts.criteria)?,
    )?;
    let probs = decide(embedder.as_mut(), &spec)?;
    if opts.json {
        crate::print_data(&document(embedder.model_id(), &spec, &probs), true)
    } else {
        let mut out = String::new();
        for (label, p) in &probs {
            out.push_str(&format!("{label}: {p:.3}\n"));
        }
        out.push_str(&format!("predicted: {}\n", predicted(&probs, &spec.labels)));
        crate::write_stdout(&out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic fake: vector = one-hot on which keyword the text
    /// contains ("alpha"→dim0, "beta"→dim1, else zeros).
    struct FakeEmbedder;
    impl Embedder for FakeEmbedder {
        fn model_id(&self) -> &str {
            "fake"
        }
        fn dims(&self) -> usize {
            2
        }
        fn embed_batch(
            &mut self,
            texts: &[&str],
            _kind: EmbedKind,
        ) -> Result<Vec<Vec<f32>>, String> {
            Ok(texts
                .iter()
                .map(|t| {
                    if t.contains("alpha") {
                        vec![1.0, 0.0]
                    } else if t.contains("beta") {
                        vec![0.0, 1.0]
                    } else {
                        vec![0.0, 0.0]
                    }
                })
                .collect())
        }
    }

    /// Mean-pooling fake, the property that makes placement matter: a text's
    /// vector is the mean of its whitespace tokens, "alpha"/"beta" carrying
    /// one dimension each and every other word a shared third one. Any
    /// static embedding behaves this way, which is why shared framing in
    /// `text` competes with the state for the same budget.
    struct MeanPoolEmbedder;
    impl Embedder for MeanPoolEmbedder {
        fn model_id(&self) -> &str {
            "mean-pool"
        }
        fn dims(&self) -> usize {
            3
        }
        fn embed_batch(
            &mut self,
            texts: &[&str],
            _kind: EmbedKind,
        ) -> Result<Vec<Vec<f32>>, String> {
            Ok(texts
                .iter()
                .map(|t| {
                    let mut v = [0.0f32; 3];
                    let words: Vec<&str> = t.split_whitespace().collect();
                    for w in &words {
                        match *w {
                            "alpha" => v[0] += 1.0,
                            "beta" => v[1] += 1.0,
                            _ => v[2] += 1.0,
                        }
                    }
                    let n = words.len().max(1) as f32;
                    v.iter().map(|x| x / n).collect()
                })
                .collect())
        }
    }

    fn spec(text: &str, context: &str, labels: &[&str], criteria: &[(&str, &str)]) -> Spec {
        Spec::checked(
            text.to_string(),
            context.to_string(),
            labels.iter().map(ToString::to_string).collect(),
            criteria
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        )
        .unwrap()
    }

    #[test]
    fn spec_rejects_bad_label_sets() {
        let labels = |v: &[&str]| v.iter().map(ToString::to_string).collect();
        let checked = |l| Spec::checked("t".into(), String::new(), l, BTreeMap::new());
        assert!(checked(labels(&["a"])).is_err());
        assert!(checked(labels(&["a", "a"])).is_err());
        assert!(checked(labels(&["a", ""])).is_err());
        assert!(checked(labels(&["a", "b"])).is_ok());
        let mut stray = BTreeMap::new();
        stray.insert("zz".to_string(), "desc".to_string());
        assert!(Spec::checked("t".into(), String::new(), labels(&["a", "b"]), stray).is_err());
    }

    #[test]
    fn candidate_text_prefers_criterion_over_label_name() {
        let s = spec("t", "", &["yes", "no"], &[("yes", "Every condition holds")]);
        assert_eq!(candidate_text("yes", &s), "Every condition holds");
        assert_eq!(candidate_text("no", &s), "no");
    }

    #[test]
    fn candidate_text_prefixes_every_candidate_with_the_context() {
        let s = spec(
            "t",
            "Under the policy,",
            &["yes", "no"],
            &[("yes", "it holds")],
        );
        assert_eq!(candidate_text("yes", &s), "Under the policy, it holds");
        assert_eq!(candidate_text("no", &s), "Under the policy, no");
    }

    /// The contract the `context` field exists for. Shared framing is long
    /// and says nothing about which label is right, so mean-pooled into
    /// `text` it swamps the state and hands the decision to whichever
    /// criterion happens to be wordiest. Carried by every candidate instead,
    /// it cancels. Same words, same model, opposite answers — so this fails
    /// the moment `candidate_text` stops folding the context in, or a caller
    /// is told to concatenate it onto `text` again.
    #[test]
    fn shared_framing_belongs_in_context_not_in_text() {
        let framing = "under the stated policy decide whether the action is permitted";
        let criteria: &[(&str, &str)] = &[
            ("yes", "alpha"),
            ("no", "beta and otherwise a great many qualifying words"),
        ];
        let mut e = MeanPoolEmbedder;

        // Framing concatenated onto the state, as the first mapping did.
        let diluted = spec(&format!("{framing} alpha"), "", &["yes", "no"], criteria);
        let probs = decide(&mut e, &diluted).unwrap();
        assert_eq!(
            predicted(&probs, &diluted.labels),
            "no",
            "shared framing in `text` is expected to swamp the state here"
        );

        // Same words, moved to where they cancel.
        let framed = spec("alpha", framing, &["yes", "no"], criteria);
        let probs = decide(&mut e, &framed).unwrap();
        assert_eq!(predicted(&probs, &framed.labels), "yes");
    }

    #[test]
    fn cosine_is_scale_invariant_and_zero_safe() {
        assert_eq!(cosine(&[1.0, 0.0], &[5.0, 0.0]), 1.0);
        assert_eq!(cosine(&[1.0, 0.0], &[0.0, 1.0]), 0.0);
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
    }

    #[test]
    fn softmax_is_normalized_and_orders_by_similarity() {
        let p = softmax(&[0.5, -0.5]);
        assert!((p.iter().sum::<f64>() - 1.0).abs() < 1e-9);
        assert!(p[0] > p[1]);
        // Identical sims → uniform distribution.
        let u = softmax(&[0.3, 0.3, 0.3]);
        assert!((u[0] - 1.0 / 3.0).abs() < 1e-6);
    }

    /// The gap is the contract, not the ranking: at `TAU`, a 0.6 cosine
    /// difference is a factor `exp(0.6 / TAU)` in probability, and that is
    /// what the caller thresholds on. Ordering survives almost any
    /// arithmetic on `(s - max) / TAU`; the value does not.
    #[test]
    fn softmax_scales_the_gap_by_the_fixed_temperature() {
        let p = softmax(&[0.9, 0.3]);
        let top = 1.0 / (1.0 + (-0.6f64 / TAU).exp());
        assert!((p[0] - top).abs() < 1e-12, "{p:?}");
        assert!((p[1] - (1.0 - top)).abs() < 1e-12, "{p:?}");
    }

    /// Subtracting the max is what keeps `exp()` finite. It is invisible on
    /// cosines, which live in [-1, 1], so it takes an input no cosine would
    /// produce to show it: added instead of subtracted, both exponentials
    /// overflow and every probability comes back NaN.
    #[test]
    fn softmax_stays_finite_on_extreme_similarities() {
        let p = softmax(&[900.0, 0.0]);
        assert!(p.iter().all(|x| x.is_finite()), "{p:?}");
        assert_eq!(p[0], 1.0, "{p:?}");
        assert!((p.iter().sum::<f64>() - 1.0).abs() < 1e-12, "{p:?}");
    }

    /// The cap bounds worst-case latency, and it counts characters rather
    /// than bytes so a multi-byte character is never cut in half.
    #[test]
    fn clip_text_bounds_long_input_and_leaves_short_input_alone() {
        assert_eq!(clip_text("hello"), "hello");
        assert_eq!(clip_text(""), "");
        let long = "é".repeat(TEXT_CAP_CHARS + 10);
        let clipped = clip_text(&long);
        assert_eq!(clipped.chars().count(), TEXT_CAP_CHARS);
        assert!(clipped.chars().all(|c| c == 'é'));
    }

    #[test]
    fn decide_picks_the_semantically_matching_criterion() {
        let mut e = FakeEmbedder;
        let s = spec(
            "alpha happens here",
            "",
            &["no", "yes"],
            &[("no", "beta outcome"), ("yes", "alpha outcome")],
        );
        let probs = decide(&mut e, &s).unwrap();
        assert!(probs["yes"] > probs["no"]);
        assert_eq!(predicted(&probs, &s.labels), "yes");
    }

    #[test]
    fn predicted_resolves_ties_to_first_label_in_order() {
        let probs = BTreeMap::from([("b".to_string(), 0.5f64), ("a".to_string(), 0.5f64)]);
        let labels = vec!["b".to_string(), "a".to_string()];
        assert_eq!(predicted(&probs, &labels), "b");
        let labels = vec!["a".to_string(), "b".to_string()];
        assert_eq!(predicted(&probs, &labels), "a");
    }

    #[test]
    fn parse_spec_line_validates_and_defaults() {
        let s = parse_spec_line(r#"{"text":"hello","labels":["a","b"],"criteria":{"a":"desc a"}}"#)
            .unwrap();
        assert_eq!(s.labels, vec!["a", "b"]);
        assert_eq!(s.criteria["a"], "desc a");
        // `context` is optional and defaults to empty — never to the text.
        assert_eq!(s.context, "");
        let s = parse_spec_line(r#"{"text":"hello","context":"the rubric","labels":["a","b"]}"#)
            .unwrap();
        assert_eq!(s.context, "the rubric");
        assert_eq!(s.text, "hello");
        assert!(parse_spec_line("not json").is_err());
        assert!(parse_spec_line(r#"{"text":"t"}"#).is_err());
        assert!(parse_spec_line(r#"{"labels":["a","b"]}"#).is_err());
        // One label is a checked error, not a panic.
        assert!(parse_spec_line(r#"{"text":"t","labels":["a"]}"#).is_err());
    }

    #[test]
    fn document_carries_probs_argmax_and_envelope() {
        let s = spec("t", "", &["a", "b"], &[]);
        let probs = BTreeMap::from([("a".to_string(), 0.9f64), ("b".to_string(), 0.1f64)]);
        let doc = document("potion-multilingual-128m", &s, &probs);
        assert_eq!(doc["marker"], "complete");
        assert_eq!(doc["predicted"], "a");
        assert_eq!(doc["probs"]["a"], 0.9);
        assert_eq!(
            doc["epistemics"]["basis"],
            json!("zero-shot embedding similarity (cosine + softmax), not a trained classifier")
        );
        assert_eq!(doc["snapshot"]["temperature"], 0.07);
        assert_eq!(doc["snapshot"]["labels"], json!(["a", "b"]));
    }

    #[test]
    fn parse_criteria_requires_key_value_pairs() {
        assert!(parse_criteria(&["a=desc".to_string()]).is_ok());
        assert!(parse_criteria(&["missing-eq".to_string()]).is_err());
    }
}

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
//! mean of its token vectors, so framing prefixed to `text` changes the query
//! and dilutes the state that varies between decisions. `context` keeps that
//! state query untouched and instead replicates the shared framing into each
//! candidate before its own criterion. This placement is not mathematical
//! cancellation; it is an explicit choice of which representations carry the
//! shared words.
//!
//! On the 231 public JevBench items scored under v1.3, moving the shared
//! instructions from `text` to `context` changed the measured tiers from
//! 64.6 / 36.1 / 45.0 % to 89.6 / 54.2 / 47.8 % and chance-corrected
//! Intelligence from 19.5 to 38.3 (`docs/bench/jevbench.md`). These are
//! historical measurements, not results from every later code revision.
//!
//! `pixel classify --jsonl` serves one decision per stdin line with the
//! model resident, so per-decision latency never includes model load.

use pixel_recall::embed::{EmbedKind, Embedder};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::BufRead;

/// Softmax temperature over cosine similarities. Fixed a priori — CLIP's
/// learned temperature — never tuned on any benchmark item.
pub const TAU: f64 = 0.07;
/// Per-component character cap for state, context, and criterion/fallback text.
/// Components stay separate so context truncation cannot erase a criterion.
const TEXT_CAP_CHARS: usize = 32_768;

/// One decision request: the text to judge, the framing every candidate
/// shares, the allowed labels, and an optional criterion description per
/// label (what the label *means*).
#[derive(Debug, Clone, PartialEq)]
pub struct Spec {
    /// The part that varies from decision to decision — the state to judge.
    pub text: String,
    /// Framing shared by every candidate: the question being asked and rubric.
    pub context: String,
    pub labels: Vec<String>,
    /// Normalized explicit criteria plus bounded label fallbacks when needed.
    pub criteria: BTreeMap<String, String>,
    clipped_fields: Vec<String>,
}

impl Spec {
    /// Validate identities, then cap every component used by the embedder.
    pub fn checked(
        text: String,
        context: String,
        labels: Vec<String>,
        criteria: BTreeMap<String, String>,
    ) -> Result<Self, String> {
        let mut seen = std::collections::HashSet::new();
        if labels
            .iter()
            .any(|label| label.is_empty() || !seen.insert(label.clone()))
        {
            return Err("labels must be non-empty and distinct".to_string());
        }
        if labels.len() < 2 {
            return Err("at least two labels are required".to_string());
        }
        if let Some(bad) = criteria.keys().find(|key| !labels.contains(key)) {
            return Err(format!("criterion for unknown label {bad:?}"));
        }

        let mut clipped_fields = Vec::new();
        let (text, text_clipped) = clip_text(&text);
        if text_clipped {
            clipped_fields.push("text".to_string());
        }
        let (context, context_clipped) = clip_text(&context);
        if context_clipped {
            clipped_fields.push("context".to_string());
        }
        let mut normalized = BTreeMap::new();
        for (label, criterion) in criteria {
            let (criterion, clipped) = clip_text(&criterion);
            if clipped {
                clipped_fields.push(format!("criteria.{label}"));
            }
            normalized.insert(label, criterion);
        }
        for label in &labels {
            if !normalized.contains_key(label) {
                let (fallback, clipped) = clip_text(label);
                if clipped {
                    clipped_fields.push(format!("label_fallback.{label}"));
                    normalized.insert(label.clone(), fallback);
                }
            }
        }

        Ok(Spec {
            text,
            context,
            labels,
            criteria: normalized,
            clipped_fields,
        })
    }

    fn was_clipped(&self) -> bool {
        !self.clipped_fields.is_empty()
    }
}

/// Build one candidate from separately bounded context and criterion/fallback.
/// Explicit empty criteria remain empty; omitted short criteria use the label.
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
///
/// `Embedder` promises nothing about the shape of what it returns, and every
/// way it can disagree with the request is silent downstream: a short
/// candidate batch drops labels off the end of the `zip`, leaving `predicted`
/// to index a probability that is not there — a panic — while a long one
/// feeds `softmax` candidates that are then discarded, so what is returned
/// does not sum to one. Both are checked here instead.
fn decide(embedder: &mut dyn Embedder, spec: &Spec) -> Result<BTreeMap<String, f64>, String> {
    let candidates: Vec<String> = spec
        .labels
        .iter()
        .map(|l| candidate_text(l, spec))
        .collect();
    // Kind is per-batch, so question and candidates embed in two calls:
    // E5-family models prefix queries and passages differently.
    let mut queries = embedder.embed_batch(&[spec.text.as_str()], EmbedKind::Query)?;
    if queries.len() != 1 {
        return Err(format!(
            "embedder returned {} vectors for 1 query text",
            queries.len()
        ));
    }
    let query = queries.remove(0);
    let refs: Vec<&str> = candidates.iter().map(String::as_str).collect();
    let cand_vecs = embedder.embed_batch(&refs, EmbedKind::Passage)?;
    if cand_vecs.len() != spec.labels.len() {
        return Err(format!(
            "embedder returned {} vectors for {} labels",
            cand_vecs.len(),
            spec.labels.len()
        ));
    }
    // `cosine` zips, so a shorter candidate vector would silently compare on
    // a prefix and score higher than it should.
    if let Some(v) = cand_vecs.iter().find(|v| v.len() != query.len()) {
        return Err(format!(
            "embedder returned a {}-dim candidate vector against a {}-dim query",
            v.len(),
            query.len()
        ));
    }
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

/// Build the per-decision JSON document and disclose any input clipping.
fn document(model: &str, spec: &Spec, probs: &BTreeMap<String, f64>) -> Value {
    let mut basis =
        "zero-shot embedding similarity (cosine + softmax), not a trained classifier".to_string();
    if spec.was_clipped() {
        basis.push_str(&format!(
            "; caps: {TEXT_CAP_CHARS} characters per input component; affected fields: {}",
            spec.clipped_fields.join(", ")
        ));
    }
    let marker = if spec.was_clipped() {
        "capped"
    } else {
        "complete"
    };
    let mut out = json!({
        "marker": marker,
        "predicted": predicted(probs, &spec.labels),
        "probs": probs,
        "epistemics": {
            "closed_world": false,
            "lower_bound": false,
            "basis": basis,
            "confidence": marker,
        },
        "snapshot": {
            "model": model,
            "temperature": TAU,
            "labels": spec.labels,
        },
    });
    if spec.was_clipped() {
        let message = format!(
            "input capped at {TEXT_CAP_CHARS} characters per component; affected fields: {}",
            spec.clipped_fields.join(", ")
        );
        out["caps"] = json!([{
            "name": "input_chars_per_component",
            "limit": TEXT_CAP_CHARS,
            "affected_fields": spec.clipped_fields,
        }]);
        out["warnings"] = json!([{"code": "INPUT_CAPPED", "message": message}]);
    }
    out
}

/// Return the bounded text and whether at least one character was removed.
fn clip_text(text: &str) -> (String, bool) {
    let mut chars = text.chars();
    let clipped = chars.by_ref().take(TEXT_CAP_CHARS).collect();
    (clipped, chars.next().is_some())
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
    let context = match v.get("context") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(context)) => context.clone(),
        Some(other) => return Err(format!("\"context\" must be a string or null, got {other}")),
    };
    // Dropping a non-string silently would classify a request the caller
    // never sent: `["a", 7, "b"]` would pass validation as two labels, and a
    // numeric criterion would fall back to embedding the label's own name.
    let labels = v
        .get("labels")
        .and_then(Value::as_array)
        .ok_or("spec needs a \"labels\" array")?
        .iter()
        .map(|l| {
            l.as_str()
                .map(str::to_string)
                .ok_or_else(|| format!("every label must be a string, got {l}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let criteria = match v.get("criteria") {
        None | Some(Value::Null) => BTreeMap::new(),
        Some(Value::Object(m)) => m
            .iter()
            .map(|(k, v)| {
                v.as_str()
                    .map(|s| (k.clone(), s.to_string()))
                    .ok_or_else(|| format!("criterion {k:?} must be a string, got {v}"))
            })
            .collect::<Result<BTreeMap<_, _>, _>>()?,
        Some(other) => return Err(format!("\"criteria\" must be an object, got {other}")),
    };
    Spec::checked(text, context, labels, criteria)
}

/// One input line to zero or one output lines. `None` is a blank line, which
/// carries no decision and so produces no result; a bad line answers `{"ok":
/// false}` rather than ending the stream, because one malformed item must not
/// abort a run of several hundred.
fn serve_line(embedder: &mut dyn Embedder, model: &str, line: &str) -> Option<String> {
    if line.trim().is_empty() {
        return None;
    }
    let out = match parse_spec_line(line).and_then(|s| decide(embedder, &s).map(|p| (s, p))) {
        Ok((spec, probs)) => {
            let mut doc = document(model, &spec, &probs);
            doc["ok"] = json!(true);
            doc
        }
        Err(e) => json!({"ok": false, "error": e}),
    };
    // A `Value` built here is always encodable; a failure would still have to
    // answer on this line rather than take the stream down with it.
    Some(serde_json::to_string(&out).unwrap_or_else(|e| {
        format!("{{\"ok\":false,\"error\":\"result encode: {e}\"}}").replace('\n', " ")
    }))
}

trait ClassifyOutput {
    fn write_text(&mut self, text: &str) -> Result<(), String>;
    fn print_document(&mut self, document: &Value) -> Result<(), String>;
}

struct ProductionOutput;

impl ClassifyOutput for ProductionOutput {
    #[cfg_attr(test, mutants::skip)] // thin adapter preserving stdout accounting
    fn write_text(&mut self, text: &str) -> Result<(), String> {
        crate::write_stdout(text)
    }

    #[cfg_attr(test, mutants::skip)] // thin adapter preserving JSON output caps
    fn print_document(&mut self, document: &Value) -> Result<(), String> {
        crate::print_data(document, true)
    }
}

/// Stream stdin lines through one resident model and preserve line framing.
fn serve_jsonl(
    reader: impl BufRead,
    embedder: &mut dyn Embedder,
    output: &mut dyn ClassifyOutput,
) -> Result<(), String> {
    let model = embedder.model_id().to_string();
    for line in reader.lines() {
        let line = line.map_err(|error| format!("stdin read: {error}"))?;
        if let Some(line_output) = serve_line(embedder, &model, &line) {
            output.write_text(&line_output)?;
            output.write_text("\n")?;
        }
    }
    Ok(())
}

#[derive(clap::Args, Debug)]
#[group(multiple = true)]
pub struct ClassifyOptions {
    /// The state text to judge — the part that varies (omit with --jsonl).
    pub text: Option<String>,
    /// Framing every candidate shares (the question and rubric preamble).
    /// It is replicated into each candidate rather than added to the state.
    #[arg(long, conflicts_with = "jsonl")]
    pub context: Option<String>,
    /// Candidate labels (repeatable or comma-separated).
    #[arg(
        long = "label",
        value_delimiter = ',',
        required_unless_present = "jsonl"
    )]
    pub labels: Vec<String>,
    /// Criterion text per label: --criterion label="description".
    #[arg(long = "criterion")]
    pub criteria: Vec<String>,
    /// Serve mode: JSONL spec lines on stdin, one result per line.
    #[arg(long)]
    pub jsonl: bool,
    #[arg(long)]
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

/// The `Spec` a one-shot invocation asks for. Separate from `run` so the
/// argument contract is checked before anything touches the model: on a
/// fresh install `open_embedder` downloads, and a caller who forgot
/// `--label` should be told that, not made to wait for a model they will
/// not use.
fn one_shot_spec(opts: &ClassifyOptions) -> Result<Spec, String> {
    let text = opts
        .text
        .as_deref()
        .ok_or("classify needs a text argument (or --jsonl)")?;
    Spec::checked(
        text.to_string(),
        opts.context.clone().unwrap_or_default(),
        opts.labels.clone(),
        parse_criteria(&opts.criteria)?,
    )
}

/// Render probabilities and disclose component clipping only when it occurred.
fn render_probs(probs: &BTreeMap<String, f64>, spec: &Spec) -> String {
    let mut out = String::new();
    for (label, probability) in probs {
        out.push_str(&format!("{label}: {probability:.3}\n"));
    }
    let top = predicted(probs, &spec.labels);
    out.push_str(&format!("predicted: {top}\n"));
    if spec.was_clipped() {
        out.push_str("marker: capped\nconfidence: capped\n");
        out.push_str(&format!(
            "warning: input capped at {TEXT_CAP_CHARS} characters per component; affected fields: {}\n",
            spec.clipped_fields.join(", ")
        ));
    }
    out
}

fn run_with(
    opts: ClassifyOptions,
    opener: impl FnOnce() -> Result<Box<dyn Embedder>, String>,
    reader: impl BufRead,
    output: &mut dyn ClassifyOutput,
) -> Result<(), String> {
    if opts.jsonl {
        let mut embedder = opener()?;
        return serve_jsonl(reader, embedder.as_mut(), output);
    }

    let spec = one_shot_spec(&opts)?;
    let mut embedder = opener()?;
    let probs = decide(embedder.as_mut(), &spec)?;
    if opts.json {
        output.print_document(&document(embedder.model_id(), &spec, &probs))
    } else {
        output.write_text(&render_probs(&probs, &spec))
    }
}

#[cfg_attr(test, mutants::skip)] // thin environment adapter over model, stdin, and stdout
pub fn run(opts: ClassifyOptions) -> Result<(), String> {
    let stdin = std::io::stdin();
    let mut output = ProductionOutput;
    run_with(opts, open_embedder, stdin.lock(), &mut output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use std::io::{self, Cursor, Read};
    use std::sync::{Arc, Mutex};

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

    type RecordedCalls = Arc<Mutex<Vec<(EmbedKind, Vec<String>)>>>;

    struct RecordingEmbedder {
        calls: RecordedCalls,
        fail_next_passage: bool,
    }

    impl Embedder for RecordingEmbedder {
        fn model_id(&self) -> &str {
            "recording"
        }

        fn dims(&self) -> usize {
            2
        }

        fn embed_batch(
            &mut self,
            texts: &[&str],
            kind: EmbedKind,
        ) -> Result<Vec<Vec<f32>>, String> {
            self.calls
                .lock()
                .unwrap()
                .push((kind, texts.iter().map(|text| (*text).to_string()).collect()));
            if kind == EmbedKind::Passage && self.fail_next_passage {
                self.fail_next_passage = false;
                return Err("embedding failed".to_string());
            }
            Ok(texts
                .iter()
                .map(|text| {
                    if text.contains("alpha") {
                        vec![1.0, 0.0]
                    } else if text.contains("beta") {
                        vec![0.0, 1.0]
                    } else {
                        vec![0.0, 0.0]
                    }
                })
                .collect())
        }
    }

    #[derive(Default)]
    struct RecordingOutput {
        text: String,
        documents: Vec<Value>,
        fail_text: bool,
        fail_document: bool,
    }

    impl ClassifyOutput for RecordingOutput {
        fn write_text(&mut self, text: &str) -> Result<(), String> {
            if self.fail_text {
                return Err("text output failed".to_string());
            }
            self.text.push_str(text);
            Ok(())
        }

        fn print_document(&mut self, document: &Value) -> Result<(), String> {
            if self.fail_document {
                return Err("document output failed".to_string());
            }
            self.documents.push(document.clone());
            Ok(())
        }
    }

    struct FailingReader;

    impl Read for FailingReader {
        fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("reader failed"))
        }
    }

    impl BufRead for FailingReader {
        fn fill_buf(&mut self) -> io::Result<&[u8]> {
            Err(io::Error::other("reader failed"))
        }

        fn consume(&mut self, _amount: usize) {}
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

    fn parse_classify(args: &[&str]) -> ClassifyOptions {
        const PARSER_TEST_STACK: usize = 16_777_216;
        let args: Vec<String> = args.iter().map(ToString::to_string).collect();
        std::thread::Builder::new()
            .stack_size(PARSER_TEST_STACK)
            .spawn(move || {
                let cli = crate::Cli::try_parse_from(args).unwrap();
                let crate::Command::Classify(options) = cli.command else {
                    panic!("classify argv parsed as another command");
                };
                options
            })
            .unwrap()
            .join()
            .unwrap()
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
    /// criterion happens to be wordiest. Replicating it into each candidate
    /// leaves the state query untouched. Same words, same model, opposite
    /// answers — so this fails if placement regresses.
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

        // Same words, moved to the candidate representations.
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
    fn clip_text_bounds_long_input_and_reports_only_removed_characters() {
        assert_eq!(clip_text("hello"), ("hello".to_string(), false));
        assert_eq!(clip_text(""), (String::new(), false));
        let exact = "é".repeat(TEXT_CAP_CHARS);
        assert_eq!(clip_text(&exact), (exact, false));
        let long = "é".repeat(TEXT_CAP_CHARS + 1);
        let (clipped, was_clipped) = clip_text(&long);
        assert!(was_clipped);
        assert_eq!(clipped.chars().count(), TEXT_CAP_CHARS);
        assert!(clipped.chars().all(|character| character == 'é'));
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
        assert_eq!(
            parse_spec_line(r#"{"text":"hello","context":"","labels":["a","b"]}"#)
                .unwrap()
                .context,
            ""
        );
        assert_eq!(
            parse_spec_line(r#"{"text":"hello","context":null,"labels":["a","b"]}"#)
                .unwrap()
                .context,
            ""
        );
        for context in ["7", "true", "[]", "{}"] {
            let line = format!(r#"{{"text":"hello","context":{context},"labels":["a","b"]}}"#);
            let error = parse_spec_line(&line).unwrap_err();
            assert!(error.contains("must be a string or null"), "{error}");
        }
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

    /// An embedder that answers with the wrong shape. `decide` has to say so:
    /// a short batch used to drop labels off the `zip` and leave `predicted`
    /// indexing a probability that was never inserted, which panics.
    struct ShapeEmbedder {
        n_query: usize,
        n_cand: usize,
        dims: usize,
    }
    impl Embedder for ShapeEmbedder {
        fn model_id(&self) -> &str {
            "shape"
        }
        fn dims(&self) -> usize {
            self.dims
        }
        fn embed_batch(
            &mut self,
            _texts: &[&str],
            kind: EmbedKind,
        ) -> Result<Vec<Vec<f32>>, String> {
            let (n, d) = match kind {
                EmbedKind::Query => (self.n_query, 3),
                EmbedKind::Passage => (self.n_cand, self.dims),
            };
            Ok((0..n).map(|i| vec![i as f32 + 1.0; d]).collect())
        }
    }

    #[test]
    fn decide_rejects_an_embedder_that_answers_with_the_wrong_shape() {
        let s = spec("t", "", &["a", "b", "c"], &[]);
        // One candidate short: the old zip dropped "c" and `predicted` panicked.
        let err = decide(
            &mut ShapeEmbedder {
                n_query: 1,
                n_cand: 2,
                dims: 3,
            },
            &s,
        )
        .unwrap_err();
        assert!(err.contains("2 vectors for 3 labels"), "{err}");

        // One candidate too many: softmax would normalise over a candidate
        // the zip then throws away, so the probabilities would not sum to 1.
        let err = decide(
            &mut ShapeEmbedder {
                n_query: 1,
                n_cand: 4,
                dims: 3,
            },
            &s,
        )
        .unwrap_err();
        assert!(err.contains("4 vectors for 3 labels"), "{err}");

        // More than one query vector: which one was the question?
        let err = decide(
            &mut ShapeEmbedder {
                n_query: 2,
                n_cand: 3,
                dims: 3,
            },
            &s,
        )
        .unwrap_err();
        assert!(err.contains("2 vectors for 1 query"), "{err}");

        // Mismatched width: `cosine` zips, so it would score on a prefix.
        let err = decide(
            &mut ShapeEmbedder {
                n_query: 1,
                n_cand: 3,
                dims: 2,
            },
            &s,
        )
        .unwrap_err();
        assert!(err.contains("2-dim candidate vector"), "{err}");

        // The well-shaped case still answers.
        assert!(
            decide(
                &mut ShapeEmbedder {
                    n_query: 1,
                    n_cand: 3,
                    dims: 3
                },
                &s
            )
            .is_ok()
        );
    }

    #[test]
    fn parse_spec_line_rejects_non_string_labels_and_criteria() {
        // Silently dropping the 7 would classify a two-label request the
        // caller never sent.
        let e = parse_spec_line(r#"{"text":"t","labels":["a",7,"b"]}"#).unwrap_err();
        assert!(e.contains("every label must be a string"), "{e}");
        let e =
            parse_spec_line(r#"{"text":"t","labels":["a","b"],"criteria":{"a":7}}"#).unwrap_err();
        assert!(e.contains("must be a string"), "{e}");
        let e = parse_spec_line(r#"{"text":"t","labels":["a","b"],"criteria":[]}"#).unwrap_err();
        assert!(e.contains("must be an object"), "{e}");
        // An absent or null `criteria` is still the documented default.
        assert!(parse_spec_line(r#"{"text":"t","labels":["a","b"],"criteria":null}"#).is_ok());
    }

    #[test]
    fn serve_line_skips_blanks_and_isolates_a_bad_line() {
        let mut e = FakeEmbedder;
        assert_eq!(serve_line(&mut e, "fake", ""), None);
        assert_eq!(serve_line(&mut e, "fake", "   \t "), None);

        // A bad line answers on that line; the caller keeps serving.
        let out = serve_line(&mut e, "fake", "not json").unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["ok"], json!(false));
        assert!(v["error"].as_str().unwrap().contains("invalid spec JSON"));

        // A good line carries the full envelope plus `ok`.
        let line =
            r#"{"text":"alpha","labels":["no","yes"],"criteria":{"yes":"alpha","no":"beta"}}"#;
        let out = serve_line(&mut e, "fake", line).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["ok"], json!(true));
        assert_eq!(v["predicted"], json!("yes"));
        assert_eq!(v["marker"], json!("complete"));
        assert_eq!(v["snapshot"]["model"], json!("fake"));
        assert!(out.lines().count() == 1, "one result line per input line");
    }

    #[test]
    fn one_shot_spec_validates_before_any_model_is_opened() {
        let opts = |text: Option<&str>, labels: &[&str], criteria: &[&str]| ClassifyOptions {
            text: text.map(str::to_string),
            context: Some("the rubric".to_string()),
            labels: labels.iter().map(ToString::to_string).collect(),
            criteria: criteria.iter().map(ToString::to_string).collect(),
            jsonl: false,
            json: false,
        };
        let e = one_shot_spec(&opts(None, &["a", "b"], &[])).unwrap_err();
        assert!(e.contains("needs a text argument"), "{e}");
        let e = one_shot_spec(&opts(Some("t"), &["a"], &[])).unwrap_err();
        assert!(e.contains("at least two labels"), "{e}");
        let e = one_shot_spec(&opts(Some("t"), &["a", "b"], &["no-equals"])).unwrap_err();
        assert!(e.contains("label=description"), "{e}");

        let s = one_shot_spec(&opts(Some("t"), &["a", "b"], &["a=desc"])).unwrap();
        assert_eq!(s.text, "t");
        assert_eq!(s.context, "the rubric");
        assert_eq!(s.criteria["a"], "desc");
    }

    #[test]
    fn checked_caps_components_and_preserves_label_and_key_identity() {
        let exact = "é".repeat(TEXT_CAP_CHARS);
        let over = "é".repeat(TEXT_CAP_CHARS + 1);
        let labels = vec!["yes".to_string(), "no".to_string()];
        let exact_spec = Spec::checked(
            exact.clone(),
            exact.clone(),
            labels.clone(),
            BTreeMap::from([("yes".to_string(), exact.clone())]),
        )
        .unwrap();
        assert!(!exact_spec.was_clipped());
        assert_eq!(exact_spec.text, exact);
        assert_eq!(exact_spec.criteria["yes"].chars().count(), TEXT_CAP_CHARS);

        let capped = Spec::checked(
            over.clone(),
            over.clone(),
            labels.clone(),
            BTreeMap::from([("yes".to_string(), over), ("no".to_string(), String::new())]),
        )
        .unwrap();
        assert_eq!(capped.labels, labels);
        assert_eq!(
            capped.criteria.keys().cloned().collect::<Vec<_>>(),
            ["no", "yes"]
        );
        assert_eq!(capped.criteria["no"], "");
        assert_eq!(capped.text.chars().count(), TEXT_CAP_CHARS);
        assert_eq!(capped.context.chars().count(), TEXT_CAP_CHARS);
        assert_eq!(capped.criteria["yes"].chars().count(), TEXT_CAP_CHARS);
        assert_eq!(capped.clipped_fields, ["text", "context", "criteria.yes"]);
    }

    #[test]
    fn checked_bounds_omitted_label_fallback_without_changing_label_identity() {
        let long_label = "x".repeat(TEXT_CAP_CHARS + 1);
        let capped = Spec::checked(
            "state".to_string(),
            String::new(),
            vec![long_label.clone(), "short".to_string()],
            BTreeMap::new(),
        )
        .unwrap();
        assert_eq!(capped.labels[0], long_label);
        assert_eq!(capped.criteria[&capped.labels[0]].len(), TEXT_CAP_CHARS);
        assert_eq!(
            candidate_text(&capped.labels[0], &capped).len(),
            TEXT_CAP_CHARS
        );
        assert_eq!(
            capped.clipped_fields,
            [format!("label_fallback.{}", capped.labels[0])]
        );

        let explicit_empty = Spec::checked(
            "state".to_string(),
            String::new(),
            vec![capped.labels[0].clone(), "short".to_string()],
            BTreeMap::from([(capped.labels[0].clone(), String::new())]),
        )
        .unwrap();
        assert_eq!(
            candidate_text(&explicit_empty.labels[0], &explicit_empty),
            ""
        );
        assert!(!explicit_empty.was_clipped());
    }

    #[test]
    fn disclosure_changes_only_capped_json_jsonl_and_human_output() {
        let probs = BTreeMap::from([("no".to_string(), 0.25), ("yes".to_string(), 0.75)]);
        let uncapped = spec("t", "", &["no", "yes"], &[]);
        let uncapped_doc = document("fake", &uncapped, &probs);
        assert_eq!(uncapped_doc["marker"], "complete");
        assert!(uncapped_doc.get("caps").is_none());
        assert!(uncapped_doc.get("warnings").is_none());
        assert_eq!(
            render_probs(&probs, &uncapped),
            "no: 0.250\nyes: 0.750\npredicted: yes\n"
        );

        let capped = spec(&"x".repeat(TEXT_CAP_CHARS + 1), "", &["no", "yes"], &[]);
        let capped_doc = document("fake", &capped, &probs);
        assert_eq!(capped_doc["marker"], "capped");
        assert_eq!(capped_doc["epistemics"]["confidence"], "capped");
        assert_eq!(capped_doc["caps"][0]["limit"], TEXT_CAP_CHARS);
        assert_eq!(capped_doc["caps"][0]["affected_fields"], json!(["text"]));
        assert_eq!(capped_doc["warnings"][0]["code"], "INPUT_CAPPED");
        assert!(
            capped_doc["epistemics"]["basis"]
                .as_str()
                .unwrap()
                .contains("affected fields: text")
        );
        let human = render_probs(&probs, &capped);
        assert!(human.contains("marker: capped\nconfidence: capped\n"));
        assert!(human.contains("affected fields: text"));

        let mut embedder = FakeEmbedder;
        let line = format!(
            r#"{{"text":"{}","labels":["no","yes"],"criteria":{{"yes":"alpha","no":"beta"}}}}"#,
            "x".repeat(TEXT_CAP_CHARS + 1)
        );
        let jsonl: Value =
            serde_json::from_str(&serve_line(&mut embedder, "fake", &line).unwrap()).unwrap();
        assert_eq!(jsonl["ok"], true);
        assert_eq!(jsonl["marker"], "capped");
        assert_eq!(jsonl["caps"][0]["affected_fields"], json!(["text"]));
    }

    #[test]
    fn parsed_one_shot_payload_runs_offline_and_preserves_placement() {
        let options = parse_classify(&[
            "pixel",
            "classify",
            "state alpha",
            "--context",
            "the rubric",
            "--label",
            "yes,no",
            "--criterion",
            "yes=alpha",
            "--criterion",
            "no=beta",
        ]);
        assert_eq!(options.text.as_deref(), Some("state alpha"));
        assert_eq!(options.context.as_deref(), Some("the rubric"));
        assert_eq!(options.labels, ["yes", "no"]);
        assert_eq!(options.criteria, ["yes=alpha", "no=beta"]);

        let calls = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&calls);
        let mut output = RecordingOutput::default();
        run_with(
            options,
            move || {
                Ok(Box::new(RecordingEmbedder {
                    calls: recorded,
                    fail_next_passage: false,
                }))
            },
            Cursor::new(Vec::<u8>::new()),
            &mut output,
        )
        .unwrap();
        assert!(output.text.contains("predicted: yes"));
        assert!(output.documents.is_empty());
        let calls = calls.lock().unwrap();
        assert_eq!(
            calls[0],
            (EmbedKind::Query, vec!["state alpha".to_string()])
        );
        assert_eq!(
            calls[1],
            (
                EmbedKind::Passage,
                vec![
                    "the rubric alpha".to_string(),
                    "the rubric beta".to_string()
                ]
            )
        );
    }

    #[test]
    fn parser_preserves_all_one_shot_arguments_and_json_flag() {
        let options = parse_classify(&[
            "pixel",
            "classify",
            "the state",
            "--context",
            "the rubric",
            "--label",
            "yes,no",
            "--label",
            "maybe",
            "--criterion",
            "yes=allowed",
            "--criterion",
            "maybe=unknown",
            "--json",
        ]);
        assert_eq!(options.text.as_deref(), Some("the state"));
        assert_eq!(options.context.as_deref(), Some("the rubric"));
        assert_eq!(options.labels, ["yes", "no", "maybe"]);
        assert_eq!(options.criteria, ["yes=allowed", "maybe=unknown"]);
        assert!(options.json);
        assert!(!options.jsonl);
    }

    #[test]
    fn parser_accepts_jsonl_with_omitted_text_and_labels() {
        let options = parse_classify(&["pixel", "classify", "--jsonl"]);
        assert!(options.text.is_none());
        assert!(options.labels.is_empty());
        assert!(options.jsonl);
    }

    #[test]
    fn run_with_validates_before_open_and_selects_json_output() {
        let opens = Arc::new(Mutex::new(0usize));
        let opened = Arc::clone(&opens);
        let invalid = ClassifyOptions {
            text: None,
            context: None,
            labels: vec!["yes".to_string(), "no".to_string()],
            criteria: Vec::new(),
            jsonl: false,
            json: false,
        };
        let mut output = RecordingOutput::default();
        let error = run_with(
            invalid,
            move || {
                *opened.lock().unwrap() += 1;
                Ok(Box::new(FakeEmbedder))
            },
            Cursor::new(Vec::<u8>::new()),
            &mut output,
        )
        .unwrap_err();
        assert_eq!(error, "classify needs a text argument (or --jsonl)");
        assert_eq!(*opens.lock().unwrap(), 0);
        assert!(output.text.is_empty());
        assert!(output.documents.is_empty());

        let options = ClassifyOptions {
            text: Some("alpha".to_string()),
            context: None,
            labels: vec!["yes".to_string(), "no".to_string()],
            criteria: vec!["yes=alpha".to_string(), "no=beta".to_string()],
            jsonl: false,
            json: true,
        };
        run_with(
            options,
            || Ok(Box::new(FakeEmbedder)),
            Cursor::new(Vec::<u8>::new()),
            &mut output,
        )
        .unwrap();
        assert!(output.text.is_empty());
        assert_eq!(output.documents.len(), 1);
        assert_eq!(output.documents[0]["predicted"], "yes");
    }

    #[test]
    fn run_with_jsonl_opens_once_and_continues_across_line_errors() {
        let input = [
            "",
            "not json",
            r#"{"text":"alpha","context":"rubric","labels":["yes","no"],"criteria":{"yes":"alpha","no":"beta"}}"#,
            r#"{"text":"beta","context":null,"labels":["yes","no"],"criteria":{"yes":"alpha","no":"beta"}}"#,
        ]
        .join("\n");
        let opens = Arc::new(Mutex::new(0usize));
        let opened = Arc::clone(&opens);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&calls);
        let options = ClassifyOptions {
            text: None,
            context: None,
            labels: Vec::new(),
            criteria: Vec::new(),
            jsonl: true,
            json: false,
        };
        let mut output = RecordingOutput::default();
        run_with(
            options,
            move || {
                *opened.lock().unwrap() += 1;
                Ok(Box::new(RecordingEmbedder {
                    calls: recorded,
                    fail_next_passage: false,
                }))
            },
            Cursor::new(input),
            &mut output,
        )
        .unwrap();
        assert_eq!(*opens.lock().unwrap(), 1);
        assert!(output.text.ends_with('\n'));
        let lines: Vec<Value> = output
            .text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0]["ok"], false);
        assert_eq!(lines[1]["ok"], true);
        assert_eq!(lines[1]["predicted"], "yes");
        assert_eq!(lines[2]["ok"], true);
        assert_eq!(lines[2]["predicted"], "no");
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 4);
        assert_eq!(
            calls[1],
            (
                EmbedKind::Passage,
                vec!["rubric alpha".to_string(), "rubric beta".to_string()]
            )
        );
    }

    #[test]
    fn jsonl_embedding_error_is_a_line_error_and_next_request_continues() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let input = [
            r#"{"text":"alpha","labels":["yes","no"],"criteria":{"yes":"alpha","no":"beta"}}"#,
            r#"{"text":"beta","labels":["yes","no"],"criteria":{"yes":"alpha","no":"beta"}}"#,
        ]
        .join("\n");
        let mut output = RecordingOutput::default();
        let mut embedder = RecordingEmbedder {
            calls,
            fail_next_passage: true,
        };
        serve_jsonl(Cursor::new(input), &mut embedder, &mut output).unwrap();
        let lines: Vec<Value> = output
            .text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["ok"], false);
        assert_eq!(lines[0]["error"], "embedding failed");
        assert_eq!(lines[1]["ok"], true);
        assert_eq!(lines[1]["predicted"], "no");
    }

    #[test]
    fn run_with_propagates_opener_reader_and_output_failures() {
        let jsonl_options = || ClassifyOptions {
            text: None,
            context: None,
            labels: Vec::new(),
            criteria: Vec::new(),
            jsonl: true,
            json: false,
        };
        let mut output = RecordingOutput::default();
        let error = run_with(
            jsonl_options(),
            || Err("open failed".to_string()),
            Cursor::new(Vec::<u8>::new()),
            &mut output,
        )
        .unwrap_err();
        assert_eq!(error, "open failed");

        let error = run_with(
            jsonl_options(),
            || Ok(Box::new(FakeEmbedder)),
            FailingReader,
            &mut output,
        )
        .unwrap_err();
        assert_eq!(error, "stdin read: reader failed");

        output.fail_text = true;
        let line =
            r#"{"text":"alpha","labels":["yes","no"],"criteria":{"yes":"alpha","no":"beta"}}"#;
        let error = run_with(
            jsonl_options(),
            || Ok(Box::new(FakeEmbedder)),
            Cursor::new(line),
            &mut output,
        )
        .unwrap_err();
        assert_eq!(error, "text output failed");

        let mut output = RecordingOutput {
            fail_document: true,
            ..RecordingOutput::default()
        };
        let error = run_with(
            ClassifyOptions {
                text: Some("alpha".to_string()),
                context: None,
                labels: vec!["yes".to_string(), "no".to_string()],
                criteria: vec!["yes=alpha".to_string(), "no=beta".to_string()],
                jsonl: false,
                json: true,
            },
            || Ok(Box::new(FakeEmbedder)),
            Cursor::new(Vec::<u8>::new()),
            &mut output,
        )
        .unwrap_err();
        assert_eq!(error, "document output failed");
    }

    #[test]
    fn render_probs_lists_every_label_then_the_argmax() {
        let probs = BTreeMap::from([("no".to_string(), 0.25f64), ("yes".to_string(), 0.75f64)]);
        let spec = spec("t", "", &["no", "yes"], &[]);
        assert_eq!(
            render_probs(&probs, &spec),
            "no: 0.250\nyes: 0.750\npredicted: yes\n"
        );
    }
}

//! `pixel classify` — bounded label decision through a remote LLM.
//!
//! The decision half of the refinement contract: one OpenAI-compatible chat
//! completion (OpenRouter, Ollama Cloud, or a local llama-server) maps the
//! state, shared framing, labels and criteria onto a probability
//! distribution over the caller's labels — the shape a Jev-class decision
//! model returns. Non-deterministic and network-bound by design: the local
//! static/verdict backends were removed because no off-the-shelf local
//! model beat Jev on the coding benchmark (see
//! `docs/bench/decide-bakeoff.md`).
//!
//! `context` stays a separate field — it is the shared framing every
//! candidate sees, never prefixed into the state text.
//!
//! `pixel classify --jsonl` serves one decision per stdin line on one
//! connection, so per-decision latency excludes setup.

use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::BufRead;

/// Disclosed basis for every remote decision — probabilities are verbalized
/// by the model, not a calibration-head output.
const REMOTE_BASIS: &str = "remote LLM, non-deterministic, verbalized probabilities (self-reported, renormalized to sum 1)";
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
    /// Validate identities, then cap every component sent to the model.
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

/// The decision engine seam behind one invocation: production is the remote
/// chat adapter; tests inject a fake with the same surface — a probability
/// distribution over the caller's labels.
trait DecisionEngine {
    fn model_id(&self) -> String;
    /// Provider preset surfaced in the snapshot (`None` for test engines).
    fn provider(&self) -> Option<&'static str>;
    /// Remote decisions are verbalized, so this is always false in production.
    fn deterministic(&self) -> bool;
    fn decide(&mut self, spec: &Spec) -> Result<BTreeMap<String, f64>, String>;
}

impl DecisionEngine for crate::decide_remote::Remote {
    fn model_id(&self) -> String {
        self.model_id().to_string()
    }

    fn provider(&self) -> Option<&'static str> {
        Some(self.provider())
    }

    fn deterministic(&self) -> bool {
        self.deterministic()
    }

    fn decide(&mut self, spec: &Spec) -> Result<BTreeMap<String, f64>, String> {
        crate::decide_remote::Remote::decide(self, spec)
    }
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
fn document(engine: &dyn DecisionEngine, spec: &Spec, probs: &BTreeMap<String, f64>) -> Value {
    let mut basis = REMOTE_BASIS.to_string();
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
            "model": engine.model_id(),
            "temperature": 0.0,
            "labels": spec.labels,
            "deterministic": engine.deterministic(),
        },
    });
    if let Some(provider) = engine.provider() {
        out["snapshot"]["provider"] = json!(provider);
    }
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

/// Open the remote decision engine from the resolved preset/model config.
#[cfg_attr(test, mutants::skip)] // thin adapter; logic lives in decide_remote
fn open_engine(
    preset: crate::decide_remote::Preset,
    model: Option<String>,
) -> Result<crate::decide_remote::Remote, String> {
    let config = crate::decide_remote::resolve_config(preset, model, remote_key_value(preset));
    Ok(crate::decide_remote::Remote::open(config))
}

/// Read the remote API-key value from the preset's key env var (or
/// `PIXEL_REMOTE_KEY_ENV` when set) by name. The value is consumed here and
/// held only inside the `Remote` adapter — it is never logged or written to
/// a document. A missing key is a hard error for presets that need one, so
/// the caller learns why the network call did not happen.
fn remote_key_value(preset: crate::decide_remote::Preset) -> Option<String> {
    let explicit = std::env::var("PIXEL_REMOTE_KEY_ENV")
        .ok()
        .filter(|s| !s.is_empty());
    let name = explicit.or_else(|| preset.key_env().map(str::to_string));
    name.and_then(|name| std::env::var(name).ok().filter(|v| !v.is_empty()))
        // Env wins; `pixel config remote-key <preset>` is the fallback so a
        // key need not live in every shell's environment.
        .or_else(|| crate::config_cmd::remote_key(preset))
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
fn serve_line(engine: &mut dyn DecisionEngine, line: &str) -> Option<String> {
    if line.trim().is_empty() {
        return None;
    }
    let out = match parse_spec_line(line).and_then(|s| engine.decide(&s).map(|p| (s, p))) {
        Ok((spec, probs)) => {
            let mut doc = document(engine, &spec, &probs);
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
    engine: &mut dyn DecisionEngine,
    output: &mut dyn ClassifyOutput,
) -> Result<(), String> {
    for line in reader.lines() {
        let line = line.map_err(|error| format!("stdin read: {error}"))?;
        if let Some(line_output) = serve_line(engine, &line) {
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
    /// Remote provider preset: `openrouter`, `ollama`, or `local`.
    /// Selects the base URL and the API-key env var; see `PIXEL_REMOTE_*`.
    #[arg(long, value_enum, default_value_t = crate::decide_remote::Preset::Openrouter)]
    pub remote_preset: crate::decide_remote::Preset,
    /// Remote model id (overrides the preset default and `PIXEL_REMOTE_MODEL`).
    #[arg(long)]
    pub remote_model: Option<String>,
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
    opener: impl FnOnce() -> Result<Box<dyn DecisionEngine>, String>,
    reader: impl BufRead,
    output: &mut dyn ClassifyOutput,
) -> Result<(), String> {
    if opts.jsonl {
        let mut engine = opener()?;
        return serve_jsonl(reader, engine.as_mut(), output);
    }

    let spec = one_shot_spec(&opts)?;
    let mut engine = opener()?;
    let probs = engine.decide(&spec)?;
    if opts.json {
        output.print_document(&document(engine.as_ref(), &spec, &probs))
    } else {
        output.write_text(&render_probs(&probs, &spec))
    }
}

#[cfg_attr(test, mutants::skip)] // thin environment adapter over model, stdin, and stdout
pub fn run(opts: ClassifyOptions) -> Result<(), String> {
    let remote_preset = opts.remote_preset;
    let remote_model = opts.remote_model.clone();
    let stdin = std::io::stdin();
    let mut output = ProductionOutput;
    run_with(
        opts,
        move || open_engine(remote_preset, remote_model).map(|e| Box::new(e) as _),
        stdin.lock(),
        &mut output,
    )
}

#[cfg(test)]
#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use std::io::{self, Cursor, Read};
    use std::sync::{Arc, Mutex};

    /// Deterministic fake engine: "yes" wins when the spec mentions "alpha",
    /// "no" wins on "beta"; `fail_next` turns the next decide into an error.
    struct FakeEngine {
        calls: Arc<Mutex<Vec<Spec>>>,
        fail_next: bool,
    }

    impl FakeEngine {
        fn new(calls: Arc<Mutex<Vec<Spec>>>) -> Self {
            FakeEngine {
                calls,
                fail_next: false,
            }
        }
    }

    impl DecisionEngine for FakeEngine {
        fn model_id(&self) -> String {
            "fake".to_string()
        }

        fn provider(&self) -> Option<&'static str> {
            None
        }

        fn deterministic(&self) -> bool {
            true
        }

        fn decide(&mut self, spec: &Spec) -> Result<BTreeMap<String, f64>, String> {
            self.calls.lock().unwrap().push(spec.clone());
            if self.fail_next {
                self.fail_next = false;
                return Err("decision failed".to_string());
            }
            let top = spec.text.contains("alpha");
            Ok(spec
                .labels
                .iter()
                .map(|l| {
                    let p = match (l.as_str(), top) {
                        ("yes", true) => 0.9,
                        ("yes", false) => 0.1,
                        (_, true) => 0.1 / (spec.labels.len() - 1) as f64,
                        (_, false) if l.as_str() == "no" => 0.9,
                        (_, false) => 0.1 / (spec.labels.len() - 1) as f64,
                    };
                    (l.clone(), p)
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

    fn fake_engine(calls: Arc<Mutex<Vec<Spec>>>) -> Result<Box<dyn DecisionEngine>, String> {
        Ok(Box::new(FakeEngine::new(calls)))
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

    /// The cap bounds worst-case request size, and it counts characters rather
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
    fn document_carries_probs_argmax_and_remote_envelope() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let engine = FakeEngine::new(calls);
        let s = spec("t", "", &["a", "b"], &[]);
        let probs = BTreeMap::from([("a".to_string(), 0.9f64), ("b".to_string(), 0.1f64)]);
        let doc = document(&engine, &s, &probs);
        assert_eq!(doc["marker"], "complete");
        assert_eq!(doc["predicted"], "a");
        assert_eq!(doc["probs"]["a"], 0.9);
        assert!(
            doc["epistemics"]["basis"]
                .as_str()
                .unwrap()
                .contains("remote LLM")
        );
        assert_eq!(doc["snapshot"]["model"], "fake");
        assert_eq!(doc["snapshot"]["temperature"], 0.0);
        assert_eq!(doc["snapshot"]["deterministic"], true);
        assert_eq!(doc["snapshot"]["labels"], json!(["a", "b"]));
    }

    #[test]
    fn parse_criteria_requires_key_value_pairs() {
        assert!(parse_criteria(&["a=desc".to_string()]).is_ok());
        assert!(parse_criteria(&["missing-eq".to_string()]).is_err());
    }

    #[test]
    fn serve_line_skips_blanks_and_isolates_a_bad_line() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut e = FakeEngine::new(calls);
        assert_eq!(serve_line(&mut e, ""), None);
        assert_eq!(serve_line(&mut e, "   \t "), None);

        // A bad line answers on that line; the caller keeps serving.
        let out = serve_line(&mut e, "not json").unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["ok"], json!(false));
        assert!(v["error"].as_str().unwrap().contains("invalid spec JSON"));

        // A good line carries the full envelope plus `ok`.
        let line =
            r#"{"text":"alpha","labels":["no","yes"],"criteria":{"yes":"alpha","no":"beta"}}"#;
        let out = serve_line(&mut e, line).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["ok"], json!(true));
        assert_eq!(v["predicted"], json!("yes"));
        assert_eq!(v["marker"], json!("complete"));
        assert_eq!(v["snapshot"]["model"], json!("fake"));
        assert!(out.lines().count() == 1, "one result line per input line");
    }

    #[test]
    fn one_shot_spec_validates_before_any_engine_is_opened() {
        let opts = |text: Option<&str>, labels: &[&str], criteria: &[&str]| ClassifyOptions {
            text: text.map(str::to_string),
            context: Some("the rubric".to_string()),
            labels: labels.iter().map(ToString::to_string).collect(),
            criteria: criteria.iter().map(ToString::to_string).collect(),
            remote_preset: crate::decide_remote::Preset::Openrouter,
            remote_model: None,
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
            capped.clipped_fields,
            [format!("label_fallback.{}", capped.labels[0])]
        );
    }

    #[test]
    fn disclosure_changes_only_capped_json_jsonl_and_human_output() {
        let probs = BTreeMap::from([("no".to_string(), 0.25), ("yes".to_string(), 0.75)]);
        let calls = Arc::new(Mutex::new(Vec::new()));
        let engine = FakeEngine::new(Arc::clone(&calls));
        let uncapped = spec("t", "", &["no", "yes"], &[]);
        let uncapped_doc = document(&engine, &uncapped, &probs);
        assert_eq!(uncapped_doc["marker"], "complete");
        assert!(uncapped_doc.get("caps").is_none());
        assert!(uncapped_doc.get("warnings").is_none());
        assert_eq!(
            render_probs(&probs, &uncapped),
            "no: 0.250\nyes: 0.750\npredicted: yes\n"
        );

        let capped = spec(&"x".repeat(TEXT_CAP_CHARS + 1), "", &["no", "yes"], &[]);
        let capped_doc = document(&engine, &capped, &probs);
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

        let mut engine = FakeEngine::new(calls);
        let line = format!(
            r#"{{"text":"{}","labels":["no","yes"],"criteria":{{"yes":"alpha","no":"beta"}}}}"#,
            "x".repeat(TEXT_CAP_CHARS + 1)
        );
        let jsonl: Value = serde_json::from_str(&serve_line(&mut engine, &line).unwrap()).unwrap();
        assert_eq!(jsonl["ok"], true);
        assert_eq!(jsonl["marker"], "capped");
        assert_eq!(jsonl["caps"][0]["affected_fields"], json!(["text"]));
    }

    #[test]
    fn parsed_one_shot_payload_runs_offline() {
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
            move || fake_engine(recorded),
            Cursor::new(Vec::<u8>::new()),
            &mut output,
        )
        .unwrap();
        assert!(output.text.contains("predicted: yes"));
        assert!(output.documents.is_empty());
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].text, "state alpha");
        assert_eq!(calls[0].context, "the rubric");
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
    fn parser_rejects_the_removed_backend_flag() {
        const PARSER_TEST_STACK: usize = 16_777_216;
        std::thread::Builder::new()
            .stack_size(PARSER_TEST_STACK)
            .spawn(move || {
                assert!(
                    crate::Cli::try_parse_from([
                        "pixel",
                        "classify",
                        "--backend",
                        "remote",
                        "--jsonl"
                    ])
                    .is_err()
                );
            })
            .unwrap()
            .join()
            .unwrap()
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
            remote_preset: crate::decide_remote::Preset::Openrouter,
            remote_model: None,
            jsonl: false,
            json: false,
        };
        let mut output = RecordingOutput::default();
        let error = run_with(
            invalid,
            move || {
                *opened.lock().unwrap() += 1;
                fake_engine(Arc::new(Mutex::new(Vec::new())))
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
            remote_preset: crate::decide_remote::Preset::Openrouter,
            remote_model: None,
            jsonl: false,
            json: true,
        };
        run_with(
            options,
            || fake_engine(Arc::new(Mutex::new(Vec::new()))),
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
            remote_preset: crate::decide_remote::Preset::Openrouter,
            remote_model: None,
            jsonl: true,
            json: false,
        };
        let mut output = RecordingOutput::default();
        run_with(
            options,
            move || {
                *opened.lock().unwrap() += 1;
                fake_engine(recorded)
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
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].context, "rubric");
    }

    #[test]
    fn jsonl_decide_error_is_a_line_error_and_next_request_continues() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let input = [
            r#"{"text":"alpha","labels":["yes","no"],"criteria":{"yes":"alpha","no":"beta"}}"#,
            r#"{"text":"beta","labels":["yes","no"],"criteria":{"yes":"alpha","no":"beta"}}"#,
        ]
        .join("\n");
        let mut output = RecordingOutput::default();
        let mut engine = FakeEngine {
            calls,
            fail_next: true,
        };
        serve_jsonl(Cursor::new(input), &mut engine, &mut output).unwrap();
        let lines: Vec<Value> = output
            .text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["ok"], false);
        assert_eq!(lines[0]["error"], "decision failed");
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
            remote_preset: crate::decide_remote::Preset::Openrouter,
            remote_model: None,
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
            || fake_engine(Arc::new(Mutex::new(Vec::new()))),
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
            || fake_engine(Arc::new(Mutex::new(Vec::new()))),
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
                remote_preset: crate::decide_remote::Preset::Openrouter,
                remote_model: None,
                jsonl: false,
                json: true,
            },
            || fake_engine(Arc::new(Mutex::new(Vec::new()))),
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

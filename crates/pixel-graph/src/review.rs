//! `review-gate` — a deterministic pre-review pass over the working-tree
//! diff.
//!
//! Reuses [`crate::changes::detect`] for the graph side (uncovered ranges,
//! suggested tests, change-set risk, the lower-bound envelope) and adds the
//! mechanical rules a code review would otherwise start from scratch on:
//! a changed symbol whose callers were not themselves changed
//! (`producer-reader-divergence`), and an added line that carries a
//! credential-shaped value or a secret-named assignment
//! (`possible-secret`), plus the remap of the report's completeness signals
//! onto findings (`uncovered-change`, `unanchored-symbol`,
//! `changed-symbol-without-test`, `risk-climb`,
//! `unresolved-callers-lower-bound`).
//!
//! Findings never carry the matched secret value, only the pattern class
//! and the anchor; every finding names the witness that established it, and
//! every cap the pass fires is named in the report's `caps` (the daemon
//! mirrors it into the epistemics envelope and a `RESULT_CAPPED` warning).

use std::collections::BTreeSet;
use std::path::Path;

use serde::Serialize;

use crate::changes::{ChangedSymbol, ChangesReport};
use crate::changes::{FileDiff, FileStatus, parse_diff};
use crate::concept::is_test_path;
use crate::impact::{file_path_by_id, symbol_by_id};
use crate::store::{EdgeKind, GraphStore};
use pixel_git::GitRunner;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Cap on consumer-divergence findings per changed symbol. A hot function
/// has thousands of callers; the first `n` name the shape, and the cap is
/// surfaced in `caps` so the gate never suggests it saw all of them.
const CONSUMER_CAP: usize = 20;

/// Cap on `possible-secret` findings total: a vendored blob of secrets must
/// not flood the report past the first anchors.
const SECRET_CAP: usize = 50;

/// Cap on files read to scan added lines for secrets. Each one is a read of
/// the working tree; beyond the cap the pass abstains (surfaced in `caps`),
/// it never claims the unscanned files hold no secret.
const SECRET_FILES_CAP: usize = 200;

/// Total cap on findings after assembly: a change touching a thousand
/// symbols still produces a bounded report, CRITICAL and HIGH kept first.
const MAX_FINDINGS: usize = 200;

/// A credential-shaped value token: the token alone flags the line,
/// whatever surrounds it. `(pattern class, needles)`; the class names the
/// finding's evidence without printing the matched value.
const SECRET_VALUE_PATTERNS: &[(&str, &[&str])] = &[
    ("private-key", &["-----BEGIN ", "PRIVATE KEY-----"]),
    ("cloud-access-key", &["AKIA"]),
    (
        "github-token",
        &["ghp_", "github_pat_", "gho_", "ghu_", "ghs_"],
    ),
    ("openai-key", &["sk-proj-", "sk-ant-", "sk-svca-"]),
    ("slack-token", &["xoxb-", "xoxp-", "xoxa-", "xoxr-"]),
];

/// A name shaped like a credential. Only flagged when the same line also
/// assigns it (`=` or `:`), so `fn parse_token()` is not a finding.
const SECRET_NAME_ASSIGN: &[&str] = &[
    "api_key",
    "api-key",
    "apikey",
    "secret",
    "password",
    "passwd",
    "passphrase",
    "credential",
];

/// One deterministic finding: a rule that fired, its severity, the anchor it
/// points at, and the witness — the evidence text — that reconstructs it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReviewFinding {
    pub rule: String,
    /// "LOW" | "MEDIUM" | "HIGH" | "CRITICAL"
    pub severity: String,
    /// Repo-relative path; `None` for change-set-level findings.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// 1-based line in the working tree; `None` when the rule has no line.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    pub evidence: String,
    pub fix_hint: &'static str,
}

/// The full deterministic review verdict for one change set.
#[derive(Debug, Clone, Serialize)]
pub struct ReviewReport {
    /// Findings, CRITICAL first, then by file/line/rule.
    pub findings: Vec<ReviewFinding>,
    /// The ref the diff ran against: `None` means the working tree's
    /// uncommitted diff against the index/HEAD; `Some(oid)` on a clean
    /// feature branch is the merge-base with the remote default branch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// Named caps the pass fired; the daemon mirrors them into
    /// `epistemics.lower_bound` and a `RESULT_CAPPED` envelope warning.
    pub caps: Vec<String>,
}

/// A clean tree on a non-default branch means "review the branch": the
/// merge-base of HEAD with the remote default (`origin/HEAD`, falling back
/// to `origin/main`). `None` on the default branch itself, in detached
/// HEAD, or when no remote default resolves.
fn branch_merge_base(runner: &GitRunner) -> Option<String> {
    let branch = runner.current_branch()?;
    let remote_default = runner
        .run_opt(&["symbolic-ref", "--short", "refs/remotes/origin/HEAD"])
        .map(|o| String::from_utf8_lossy(&o).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "origin/main".to_string());
    if branch == remote_default || Some(branch.as_str()) == remote_default.strip_prefix("origin/") {
        return None;
    }
    runner
        .run_opt(&["merge-base", "HEAD", &remote_default])
        .map(|o| String::from_utf8_lossy(&o).trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Run the deterministic review of the working tree against `base_ref`
/// (default HEAD). `detect` supplies the graph side; the two extra passes
/// walk the diff once more for added lines and read every changed symbol's
/// callers.
pub fn review(
    store: &GraphStore,
    root: &Path,
    base_ref: Option<&str>,
) -> Result<ReviewReport, BoxError> {
    let runner = GitRunner::new(root);
    // No explicit base: a dirty tree reviews the uncommitted diff; a clean
    // tree on a feature branch reviews the whole branch diff.
    let mut base = base_ref.map(str::to_string);
    let mut diff_bytes = runner.diff_unified0(base.as_deref()).unwrap_or_default();
    if base_ref.is_none() && diff_bytes.is_empty() {
        if let Some(merge_base) = branch_merge_base(&runner) {
            let branch_diff = runner.diff_unified0(Some(&merge_base)).unwrap_or_default();
            if !branch_diff.is_empty() {
                base = Some(merge_base);
                diff_bytes = branch_diff;
            }
        }
    }
    let diff = String::from_utf8_lossy(&diff_bytes).into_owned();
    let file_diffs = parse_diff(&diff);
    let changed_paths: BTreeSet<String> = file_diffs.iter().map(|fd| fd.path.clone()).collect();
    let report = crate::changes::detect(store, root, base.as_deref(), true)?;

    let mut findings = Vec::new();
    let mut caps = Vec::new();

    findings.extend(graph_findings(&report, &mut caps));

    let (consumers, consumers_capped) =
        consumers_outside_change(store, &report.symbols, &changed_paths)?;
    if consumers_capped {
        caps.push(format!(
            "consumer readership capped at {CONSUMER_CAP} per changed symbol; more readers exist"
        ));
    }
    findings.extend(consumers);

    let (secrets, secrets_capped) = added_secret_findings(root, &file_diffs)?;
    if secrets_capped {
        caps.push(format!(
            "secret scan capped at {SECRET_FILES_CAP} files / {SECRET_CAP} findings; the rest was not scanned"
        ));
    }
    findings.extend(secrets);

    findings.sort_by(|a, b| {
        severity_rank(&b.severity)
            .cmp(&severity_rank(&a.severity))
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.cmp(&b.line))
            .then_with(|| a.rule.cmp(&b.rule))
    });
    if findings.len() > MAX_FINDINGS {
        caps.push(format!(
            "findings truncated at {MAX_FINDINGS}; lower-severity tail not listed"
        ));
        findings.truncate(MAX_FINDINGS);
    }

    Ok(ReviewReport {
        findings,
        base,
        caps,
    })
}

/// Findings that are the `detect` report re-stated as a review vocabulary,
/// plus the caps its completeness signals stand for. Pure over the report:
/// no store, no I/O, so the severity mappings get the bracket tests.
fn graph_findings(report: &ChangesReport, caps: &mut Vec<String>) -> Vec<ReviewFinding> {
    let mut out = Vec::new();

    if let Some(finding) = risk_finding(&report.risk) {
        out.push(finding);
    }

    // The envelope note is built by `detect` from the same-name site list;
    // its prefix is the stable marker that lower-bound sites exist. The
    // finding stays change-set-level: the report carries the count of
    // affected names, not their paths.
    if report.envelope_note.starts_with("lower bound:") {
        out.push(ReviewFinding {
            rule: "unresolved-callers-lower-bound".into(),
            severity: "HIGH".into(),
            file: None,
            line: None,
            evidence: report.envelope_note.clone(),
            fix_hint: "resolve the same-name call sites (imports, aliases, dynamic dispatch) \
                       before trusting a caller-free graph answer",
        });
    }

    for u in &report.uncovered_changes {
        out.push(ReviewFinding {
            rule: "uncovered-change".into(),
            severity: motif_severity(&u.motif).into(),
            file: Some(u.path.clone()),
            line: u.new_lines.or(u.old_lines).map(|[a, _]| a),
            evidence: format!(
                "changed range maps to no symbol ({motif:?})",
                motif = u.motif
            ),
            fix_hint: "index the file, or add symbol anchors, so the change is attributable",
        });
    }
    if report.uncovered_lower_bound {
        caps.push(
            "uncovered ranges truncated, or the old side of some file was not examined".to_string(),
        );
    }

    for u in &report.unanchored {
        out.push(ReviewFinding {
            rule: "unanchored-symbol".into(),
            severity: "HIGH".into(),
            file: Some(u.path.clone()),
            line: Some(u.old_lines[0]),
            evidence: format!(
                "deleted symbol {name} has no anchor in the current graph",
                name = u.name
            ),
            fix_hint: "confirm references to {name} were renamed or removed, or re-index",
        });
    }

    // A truncated test walk cannot prove absence: abstain, and say why.
    if report.suggested_tests_lower_bound {
        caps.push(
            "suggested tests truncated; a changed symbol without a listed test may still \
                   have one"
                .to_string(),
        );
        return out;
    }
    let covered: BTreeSet<&str> = report
        .suggested_tests
        .iter()
        .flat_map(|t| t.matched_symbols.iter().map(String::as_str))
        .collect();
    for s in &report.symbols {
        if is_test_path(&s.path) || covered.contains(s.name.as_str()) {
            continue;
        }
        out.push(ReviewFinding {
            rule: "changed-symbol-without-test".into(),
            severity: "MEDIUM".into(),
            file: Some(s.path.clone()),
            line: None,
            evidence: format!("{name} changed with no suggested test", name = s.name),
            fix_hint: "add a test that exercises the new behaviour",
        });
    }

    out
}

/// The risk climb a change set reaches, as a finding: `HIGH` and above only,
/// so a quiet change set emits nothing.
fn risk_finding(risk: &str) -> Option<ReviewFinding> {
    match risk {
        "CRITICAL" => Some(report_level("risk-climb", "CRITICAL", risk)),
        "HIGH" => Some(report_level("risk-climb", "HIGH", risk)),
        _ => None,
    }
}

/// The change-set-level finding shape shared by the report-remap rules.
fn report_level(rule: &str, severity: &str, risk: &str) -> ReviewFinding {
    ReviewFinding {
        rule: rule.into(),
        severity: severity.into(),
        file: None,
        line: None,
        evidence: format!("change-set risk reached {risk}"),
        fix_hint: "review the high-blast-radius change as a whole before landing it",
    }
}

/// Severity of an uncovered range by what made it unanchored: a binary or
/// mode-only change is not a coverage gap, everything else is.
fn motif_severity(motif: &crate::changes::UncoveredMotif) -> &'static str {
    match motif {
        crate::changes::UncoveredMotif::NonTextChange => "LOW",
        _ => "MEDIUM",
    }
}

/// Total order for stabilising a report: CRITICAL first, then HIGH, then
/// MEDIUM, then LOW — the tail a truncation cuts first.
fn severity_rank(severity: &str) -> u8 {
    match severity {
        "CRITICAL" => 4,
        "HIGH" => 3,
        "MEDIUM" => 2,
        _ => 1,
    }
}

/// A changed symbol, its depth-1 callers walked: a caller reading the
/// changed behaviour whose own file is not part of the change set is a
/// divergence the change-propagation rule makes mechanical. Never claims
/// the caller is broken — only that it was not routed through the change.
fn consumers_outside_change(
    store: &GraphStore,
    changed: &[ChangedSymbol],
    changed_paths: &BTreeSet<String>,
) -> Result<(Vec<ReviewFinding>, bool), BoxError> {
    let mut out = Vec::new();
    let mut capped = false;
    for cs in changed {
        let Some(sym) = store.symbol_by_uid(&cs.uid)? else {
            continue;
        };
        let edges = store.edges_to(sym.id, Some(EdgeKind::Calls))?;
        if edges.len() > CONSUMER_CAP {
            capped = true;
        }
        for edge in edges.into_iter().take(CONSUMER_CAP) {
            let Some(caller) = symbol_by_id(store, edge.src_id)? else {
                continue;
            };
            let path = file_path_by_id(store, caller.file_id)?;
            if changed_paths.contains(&path) {
                continue;
            }
            out.push(ReviewFinding {
                rule: "producer-reader-divergence".into(),
                severity: "MEDIUM".into(),
                file: Some(path),
                line: Some(caller.start_line),
                evidence: format!(
                    "{producer} changed; {consumer} reads it and was not changed",
                    producer = cs.name,
                    consumer = caller.name
                ),
                fix_hint: "route this consumer through the new behaviour, or record why none \
                           is needed",
            });
        }
    }
    Ok((out, capped))
}

/// The pattern class an added line matches, or `None`. The tuple's second
/// field says whether the token is a credential-shaped *value* (strong) or
/// only a secret-named *assignment* (weak).
fn scan_line_for_secret(line: &str) -> Option<(&'static str, bool)> {
    for &(class, needles) in SECRET_VALUE_PATTERNS {
        if needles.iter().any(|needle| line.contains(needle)) {
            return Some((class, true));
        }
    }
    if SECRET_NAME_ASSIGN
        .iter()
        .any(|needle| line.contains(needle))
        && (line.contains('=') || line.contains(':'))
    {
        return Some(("credential-named-assignment", false));
    }
    None
}

/// Test and fixture paths carry credential-shaped strings on purpose —
/// a table of patterns a scanner test feeds itself is not a leak. The
/// finding still fires one rung lower; a real key pasted into a test file
/// is still committed.
fn test_fixture_path(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    path.contains("/tests/")
        || path.contains("/fixtures/")
        || path.contains("/fixture")
        || name.starts_with("test_")
        || name.contains("_test.")
        || name.contains("_tests.")
        || name.contains(".test.")
        || name.contains("tests.rs")
}

/// Scan every added line of every changed file for credential-shaped
/// content. Only *added* ranges carry risk — a deletion removes, it does
/// not introduce — and the finding never echoes the matched value, only the
/// pattern class and the anchor.
fn added_secret_findings(
    root: &Path,
    file_diffs: &[FileDiff],
) -> Result<(Vec<ReviewFinding>, bool), BoxError> {
    let mut out = Vec::new();
    let mut capped = false;
    let mut files_read = 0usize;
    for fd in file_diffs {
        if !fd.text || fd.status == FileStatus::Deleted || fd.added_ranges.is_empty() {
            continue;
        }
        if files_read >= SECRET_FILES_CAP {
            capped = true;
            break;
        }
        files_read += 1;
        let Ok(content) = std::fs::read_to_string(root.join(&fd.path)) else {
            // Gone since the diff was written: nothing to scan.
            continue;
        };
        // Everything after `#[cfg(test)]` is test code: fixture strings in
        // an inline test module are data, not leaks.
        let test_region_start = content
            .lines()
            .position(|line| line.contains("#[cfg(test)]"))
            .map(|i| i as u32 + 1);
        let test_fixture_path = test_fixture_path(&fd.path);
        for &(start, end) in &fd.added_ranges {
            for (i, line) in content
                .lines()
                .skip((start - 1) as usize)
                .take((end - start + 1) as usize)
                .enumerate()
            {
                let Some((class, strong)) = scan_line_for_secret(line) else {
                    continue;
                };
                // Fixture strings legitimately match the patterns; a live
                // key in a test file is still a leak, so the finding is
                // kept but drops a rung: CRITICAL→MEDIUM, MEDIUM→LOW.
                // A bare literal (`"ghp_abc",`, `("github-token", &[…])`,
                // `&["sk-proj-", …]`) is a pattern-table entry, not an
                // assignment — same downgrade.
                let trimmed = line.trim_start();
                let bare_literal = !trimmed.contains('=')
                    && trimmed
                        .chars()
                        .next()
                        .is_some_and(|c| matches!(c, '"' | '(' | '&'));
                let test_fixture = test_fixture_path
                    || bare_literal
                    || test_region_start.is_some_and(|t| start + i as u32 >= t);
                out.push(ReviewFinding {
                    rule: "possible-secret".into(),
                    severity: match (strong, test_fixture) {
                        (true, false) => "CRITICAL",
                        (true, true) | (false, false) => "MEDIUM",
                        (false, true) => "LOW",
                    }
                    .into(),
                    file: Some(fd.path.clone()),
                    line: Some(start + i as u32),
                    evidence: format!("added line matches the {class} pattern"),
                    fix_hint: if strong {
                        "confirm this is not a live credential; rotate it if it is"
                    } else {
                        "confirm the assigned value is not a real secret; prefer env/config \
                         injection"
                    },
                });
                if out.len() >= SECRET_CAP {
                    capped = true;
                    return Ok((out, capped));
                }
            }
        }
    }
    Ok((out, capped))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changes::UncoveredMotif;

    /// An added-line fixture is a protocol fixture: each case names the
    /// pattern class it must land in, built as data so the scanner is
    /// tested against the table, not against a narrative.
    const SECRET_CASES: &[(&str, Option<(&str, bool)>)] = &[
        ("const K: &str = \"ghp_abc\";", Some(("github-token", true))),
        ("sku = sk-proj-abcdef", Some(("openai-key", true))),
        (
            "-----BEGIN RSA PRIVATE KEY-----",
            Some(("private-key", true)),
        ),
        (
            "value = \"AKIA1234567890\"",
            Some(("cloud-access-key", true)),
        ),
        ("xoxb-1234 = \"ignored\"", Some(("slack-token", true))),
        (
            "api_key = env(\"API_KEY\")",
            Some(("credential-named-assignment", false)),
        ),
        ("let login_token = \"abc\";", None),
        // A name-shaped token without an assignment on the line is not a
        // finding: a type or a function is named after the concept.
        ("fn parse_token(line: &str)", None),
        // A plain `=` with none of the names: no finding either.
        ("let size = 40;", None),
    ];

    #[test]
    fn scan_line_for_secret_lands_each_case_in_its_class() {
        for (line, expected) in SECRET_CASES {
            assert_eq!(scan_line_for_secret(line), *expected, "line: {line:?}");
        }
    }

    #[test]
    fn severity_rank_orders_critical_first_and_low_last() {
        assert!(severity_rank("CRITICAL") > severity_rank("HIGH"));
        assert!(severity_rank("HIGH") > severity_rank("MEDIUM"));
        assert!(severity_rank("MEDIUM") > severity_rank("LOW"));
        assert_eq!(severity_rank("LOW"), 1);
    }

    #[test]
    fn risk_finding_only_fires_high_and_critical() {
        assert_eq!(risk_finding("LOW"), None);
        assert_eq!(risk_finding("MEDIUM"), None);
        let high = risk_finding("HIGH").expect("HIGH fires");
        assert_eq!(high.severity, "HIGH");
        assert_eq!(high.file, None);
        let critical = risk_finding("CRITICAL").expect("CRITICAL fires");
        assert_eq!(critical.severity, "CRITICAL");
    }

    #[test]
    fn motif_severity_treats_non_text_as_low_only() {
        assert_eq!(motif_severity(&UncoveredMotif::NonTextChange), "LOW");
        assert_eq!(motif_severity(&UncoveredMotif::OutsideSymbol), "MEDIUM");
        assert_eq!(motif_severity(&UncoveredMotif::NotIndexed), "MEDIUM");
    }

    // --- integration: a real git repo and a built graph ---

    fn tmpdir(tag: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("review-gate-{tag}-"))
            .tempdir()
            .unwrap()
    }

    fn git(dir: &std::path::Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "review")
            .env("GIT_AUTHOR_EMAIL", "review@test")
            .env("GIT_COMMITTER_NAME", "review")
            .env("GIT_COMMITTER_EMAIL", "review@test")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    /// Build a fresh graph over `root` and open it: the fixture commits
    /// first, so the store is a snapshot of the base state and the working
    /// tree edits after it are what the diff sees.
    fn store_for(root: &Path) -> GraphStore {
        let db = root.join("graph.db");
        crate::build::build_graph(root, &db).expect("fixture graph builds");
        GraphStore::open(&db).expect("fixture store opens")
    }

    /// A two-module crate: `src/a.rs` is changed, `src/b.rs` (which calls
    /// into it) is not. The consumer-in-another-file shape is what the
    /// divergence rule exists for — a caller inside the changed file is by
    /// definition part of the change.
    fn divergence_and_secret_fixture() -> tempfile::TempDir {
        let dir = tmpdir("divergence");
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub mod a;\npub mod b;\n").unwrap();
        std::fs::write(root.join("src/a.rs"), "pub fn produce() -> i32 { 1 }\n").unwrap();
        std::fs::write(
            root.join("src/b.rs"),
            "use crate::a::produce;\npub fn consume() -> i32 { produce() }\n",
        )
        .unwrap();
        git(root, &["init", "-q"]);
        git(root, &["add", "."]);
        git(root, &["commit", "-qm", "base"]);
        dir
    }

    #[test]
    fn review_flags_divergence_secrets_and_untested_changes() {
        let dir = divergence_and_secret_fixture();
        let root = dir.path();
        let store = store_for(root);
        // Change `produce`, append a credential-shaped value (strong) and a
        // secret-named assignment (weak). `src/b.rs` is untouched.
        std::fs::write(
            root.join("src/a.rs"),
            "pub fn produce() -> i32 { 2 }\n\
             const TRAILING_KEY: &str = \"ghp_1234567890abcdef\";\n\
             const api_key_value: &str = env!(\"API_KEY\");\n",
        )
        .unwrap();

        let report = review(&store, root, None).expect("review runs on a changed tree");
        let rules: Vec<&str> = report.findings.iter().map(|f| f.rule.as_str()).collect();
        // The cap the rules ride must not have fired on this tiny change set.
        assert!(report.caps.is_empty(), "{report:?}");

        let divergence: Vec<&ReviewFinding> = report
            .findings
            .iter()
            .filter(|f| f.rule == "producer-reader-divergence")
            .collect();
        assert!(
            !divergence.is_empty(),
            "changed `produce` read by unchanged `consume` must be flagged: {rules:?}"
        );
        let d = &divergence[0];
        assert_eq!(d.severity, "MEDIUM");
        assert_eq!(d.file.as_deref(), Some("src/b.rs"));
        assert_eq!(d.line, Some(2), "{}", d.evidence);
        assert!(d.evidence.contains("consume"), "{}", d.evidence);
        assert!(d.evidence.contains("produce"), "{}", d.evidence);

        // Strong and weak secret shapes both surface, at their exact lines,
        // and neither echoes the matched value.
        let secrets: Vec<&ReviewFinding> = report
            .findings
            .iter()
            .filter(|f| f.rule == "possible-secret")
            .collect();
        assert_eq!(secrets.len(), 2, "strong + weak: {rules:?}");
        let s = &secrets[0];
        assert_eq!(s.severity, "CRITICAL");
        assert_eq!(s.file.as_deref(), Some("src/a.rs"));
        assert_eq!(s.line, Some(2));
        let weak = &secrets[1];
        assert_eq!(weak.severity, "MEDIUM");
        assert_eq!(weak.line, Some(3));
        assert!(
            weak.evidence.contains("credential-named-assignment"),
            "{}",
            weak.evidence
        );
        for f in &secrets {
            assert!(
                !f.evidence.contains("ghp_") && !f.evidence.contains("API_KEY"),
                "evidence must not echo the matched value: {}",
                f.evidence
            );
        }

        assert!(
            rules.contains(&"changed-symbol-without-test"),
            "changed `produce` reached by no test: {rules:?}"
        );
        // The added const lines sit between symbols on line 2: uncovered.
        let uncovered: Vec<&ReviewFinding> = report
            .findings
            .iter()
            .filter(|f| f.rule == "uncovered-change")
            .collect();
        assert!(
            !uncovered.is_empty(),
            "added module-level const is outside every symbol: {rules:?}"
        );
        assert_eq!(uncovered[0].file.as_deref(), Some("src/a.rs"));
        assert_eq!(uncovered[0].line, Some(2));
    }

    /// A strong token in a `#[cfg(test)]` region or a bare literal in a
    /// pattern table is fixture data, not a leak — one rung lower. The same
    /// token assigned in production code stays CRITICAL.
    #[test]
    fn review_downgrades_secret_matches_in_test_regions_and_pattern_tables() {
        let dir = divergence_and_secret_fixture();
        let root = dir.path();
        let store = store_for(root);
        std::fs::write(
            root.join("src/a.rs"),
            "pub fn produce() -> i32 { 2 }\n\
             const REAL: &str = \"ghp_1234567890abcdef\";\n\
             const TABLE: &[&str] = &[\n\
             \x20   \"ghp_abcdef\",\n\
             ];\n\
             #[cfg(test)]\nmod tests {\n\
             \x20   const CASE: &str = \"ghp_feedface\";\n\
             }\n",
        )
        .unwrap();

        let report = review(&store, root, None).expect("review runs");
        let secrets: Vec<&ReviewFinding> = report
            .findings
            .iter()
            .filter(|f| f.rule == "possible-secret")
            .collect();
        let at = |line: u32| {
            secrets
                .iter()
                .find(|f| f.line == Some(line))
                .unwrap_or_else(|| panic!("no possible-secret at line {line}: {secrets:?}"))
        };
        assert_eq!(at(2).severity, "CRITICAL", "assigned in prod code");
        assert_eq!(at(4).severity, "MEDIUM", "bare literal in a table");
        assert_eq!(at(8).severity, "MEDIUM", "inside the cfg(test) region");
    }

    /// A changed symbol whose file name is a test path is its own test: the
    /// `changed-symbol-without-test` rule must not flag it.
    #[test]
    fn review_skips_a_changed_symbol_inside_a_test_file() {
        let dir = tmpdir("test-path");
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub mod a_test;\n").unwrap();
        std::fs::write(root.join("src/a_test.rs"), "pub fn helper() -> i32 { 1 }\n").unwrap();
        git(root, &["init", "-q"]);
        git(root, &["add", "."]);
        git(root, &["commit", "-qm", "base"]);
        let store = store_for(root);

        std::fs::write(root.join("src/a_test.rs"), "pub fn helper() -> i32 { 2 }\n").unwrap();
        let report = review(&store, root, None).expect("review runs");
        let seen: Vec<&ReviewFinding> = report
            .findings
            .iter()
            .filter(|f| f.rule == "changed-symbol-without-test")
            .collect();
        assert!(
            seen.is_empty(),
            "a change inside a _test file is its own test: {seen:?}"
        );
    }

    /// Two same-named definitions plus an importless call site is an
    /// unresolved same-name site: the lower-bound cap must surface as a
    /// HIGH change-set finding, not be silently ranked HIGH.
    #[test]
    fn review_surfaces_an_unresolved_same_name_lower_bound() {
        let dir = tmpdir("lower-bound");
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        for (name, body) in [
            ("a", "pub fn produce() -> i32 { 1 }\n"),
            ("b", "pub fn produce() -> i32 { 2 }\n"),
            ("c", "pub fn consumer() -> i32 { produce() }\n"),
        ] {
            std::fs::write(root.join(format!("src/{name}.rs")), body).unwrap();
        }
        std::fs::write(
            root.join("src/lib.rs"),
            "pub mod a;\npub mod b;\npub mod c;\n",
        )
        .unwrap();
        git(root, &["init", "-q"]);
        git(root, &["add", "."]);
        git(root, &["commit", "-qm", "base"]);
        let store = store_for(root);

        std::fs::write(root.join("src/a.rs"), "pub fn produce() -> i32 { 3 }\n").unwrap();
        let report = review(&store, root, None).expect("review runs");
        let lb: Vec<&ReviewFinding> = report
            .findings
            .iter()
            .filter(|f| f.rule == "unresolved-callers-lower-bound")
            .collect();
        assert_eq!(
            lb.len(),
            1,
            "unresolved `produce` must be surfaced: {report:?}"
        );
        assert_eq!(lb[0].severity, "HIGH", "{report:?}");
        assert_eq!(lb[0].file, None, "{report:?}");
        assert!(
            lb[0].evidence.contains("produce"),
            "the note names the affected symbol: {}",
            lb[0].evidence
        );
    }

    /// A deleted symbol lives in no graph: the change is unanchored, HIGH,
    /// and nothing claims the deletion introduced a secret.
    #[test]
    fn review_marks_a_deleted_symbol_unanchored() {
        let dir = tmpdir("unanchored");
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub mod del;\npub mod a;\n").unwrap();
        std::fs::write(root.join("src/del.rs"), "pub fn doomed() -> i32 { 1 }\n").unwrap();
        std::fs::write(root.join("src/a.rs"), "pub fn idle() -> i32 { 0 }\n").unwrap();
        git(root, &["init", "-q"]);
        git(root, &["add", "."]);
        git(root, &["commit", "-qm", "base"]);
        let store = store_for(root);

        std::fs::remove_file(root.join("src/del.rs")).unwrap();
        std::fs::write(root.join("src/a.rs"), "pub fn idle() -> i32 { 1 }\n").unwrap();
        let report = review(&store, root, None).expect("review runs");
        let unanchored: Vec<&ReviewFinding> = report
            .findings
            .iter()
            .filter(|f| f.rule == "unanchored-symbol")
            .collect();
        assert_eq!(unanchored.len(), 1, "{report:?}");
        assert_eq!(unanchored[0].severity, "HIGH");
        assert_eq!(unanchored[0].file.as_deref(), Some("src/del.rs"));
        assert!(
            unanchored[0].evidence.contains("doomed"),
            "{}",
            unanchored[0].evidence
        );
        // A deletion introduces nothing: no `possible-secret` for the file
        // the diff only removed.
        assert!(
            report
                .findings
                .iter()
                .all(|f| f.rule != "possible-secret" || f.file.as_deref() != Some("src/del.rs")),
            "{report:?}"
        );
    }

    #[test]
    fn review_on_a_clean_tree_emits_no_findings() {
        let dir = tmpdir("clean");
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.rs"), "pub fn idle() {}\n").unwrap();
        git(root, &["init", "-q"]);
        git(root, &["add", "."]);
        git(root, &["commit", "-qm", "base"]);
        let store = store_for(root);

        let report = review(&store, root, None).expect("review runs on a clean tree");
        assert!(
            report.findings.is_empty(),
            "nothing changed, nothing to flag: {report:?}"
        );
        assert!(report.caps.is_empty(), "{report:?}");
    }
}

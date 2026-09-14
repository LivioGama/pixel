//! `pixel plan` command — deterministic todo list generation.
//!
//! The daemon owns the graph: the findings come from the `plan` op, which
//! refreshes the graph incrementally before it runs the queries, and this
//! module only renders them.

use std::collections::HashSet;
use std::path::PathBuf;

use pixel_daemon::api::Request;
use pixel_graph::plan::PlanFinding;
use serde_json::json;

#[derive(Debug, Clone)]
pub struct PlanOptions {
    pub prompt: Option<String>,
    pub path: PathBuf,
    pub query: Option<String>,
    pub tag: Option<String>,
    pub limit: Option<usize>,
    pub format: String,
    pub no_verify: bool,
    pub max_todos: Option<usize>,
}

pub fn run(opts: PlanOptions) -> Result<(), String> {
    let data = crate::execute(
        &opts.path,
        Request::Plan {
            prompt: opts.prompt.clone(),
            query: opts.query.clone(),
            tag: opts.tag.clone(),
            limit: opts.limit,
        },
        false,
    )?;
    let findings = findings_of(&data)?;
    render(opts, findings)
}

/// The findings of a `plan` op answer.
fn findings_of(data: &serde_json::Value) -> Result<Vec<PlanFinding>, String> {
    let findings = data
        .get("findings")
        .cloned()
        .ok_or_else(|| "plan: the answer carries no findings".to_string())?;
    serde_json::from_value(findings).map_err(|e| format!("plan: unreadable findings: {e}"))
}

fn render(opts: PlanOptions, findings: Vec<PlanFinding>) -> Result<(), String> {
    let mut findings = findings;
    if let Some(cap) = opts.max_todos {
        findings.truncate(cap);
    }
    match opts.format.as_str() {
        "json" => render_json(&opts, &findings),
        "compact" => render_compact(&opts, &findings),
        _ => render_markdown(&opts, &findings),
    }
}

#[cfg_attr(test, mutants::skip)] // one print over `markdown`, which is tested
fn render_markdown(opts: &PlanOptions, findings: &[PlanFinding]) -> Result<(), String> {
    print!("{}", markdown(opts.no_verify, findings));
    Ok(())
}

/// The markdown checklist: a numbered `[ ]` line per finding, a leading
/// "map" item once there are enough findings to summarise, and a trailing
/// verify item unless `--no-verify`.
fn markdown(no_verify: bool, findings: &[PlanFinding]) -> String {
    let mut output = String::new();
    if findings.is_empty() {
        output.push_str("No plan findings.\n");
    } else if is_summary_eligible(findings) {
        let files = file_count(findings);
        output.push_str(&format!(
            "1. [ ] Map {} findings across {} files (ranked by fan-in)\n",
            findings.len(),
            files
        ));
        for (i, f) in findings.iter().enumerate() {
            output.push_str(&format!(
                "{}. [ ] {} in {} (line {}, fan-in: {}) [{}]\n",
                i + 2,
                f.label,
                f.file,
                f.line,
                f.fan_in,
                f.severity.as_str()
            ));
        }
        if !no_verify {
            let n = findings.len() + 2;
            output.push_str(&format!(
                "{n}. [ ] Verify all plan targets in the running build\n"
            ));
        }
    } else {
        for (i, f) in findings.iter().enumerate() {
            output.push_str(&format!(
                "{}. [ ] {} in {} (line {}, fan-in: {}) [{}]\n",
                i + 1,
                f.label,
                f.file,
                f.line,
                f.fan_in,
                f.severity.as_str()
            ));
        }
        if !no_verify {
            let n = findings.len() + 1;
            output.push_str(&format!(
                "{n}. [ ] Verify all plan targets in the running build\n"
            ));
        }
    }
    output
}

fn render_compact(opts: &PlanOptions, findings: &[PlanFinding]) -> Result<(), String> {
    for f in findings {
        println!(
            "{}:{} {} [{}]",
            f.file,
            f.line,
            f.label,
            f.severity.as_str()
        );
    }
    if !opts.no_verify {
        println!("verify all plan targets in the running build");
    }
    Ok(())
}

fn render_json(opts: &PlanOptions, findings: &[PlanFinding]) -> Result<(), String> {
    let verify = !opts.no_verify;
    let payload = json!({
        "findings": findings,
        "verify": verify,
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&payload).map_err(|e| e.to_string())?
    );
    Ok(())
}

fn is_summary_eligible(findings: &[PlanFinding]) -> bool {
    findings.len() >= 3
}

fn file_count(findings: &[PlanFinding]) -> usize {
    let mut set = HashSet::new();
    for f in findings {
        set.insert(&f.file);
    }
    set.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pixel_graph::plan::Severity;

    fn finding(file: &str, line: u32, fan_in: u32) -> PlanFinding {
        PlanFinding {
            file: file.to_string(),
            line,
            label: format!("Review {file}"),
            fan_in,
            severity: Severity::from_fan_in(fan_in),
        }
    }

    #[test]
    fn markdown_lists_findings_with_map_and_verify_items() {
        assert_eq!(markdown(false, &[]), "No plan findings.\n");
        assert_eq!(markdown(true, &[]), "No plan findings.\n");

        // Under the summary threshold: plain numbering, verify last.
        let one = [finding("src/a.rs", 3, 0)];
        assert_eq!(
            markdown(false, &one),
            "1. [ ] Review src/a.rs in src/a.rs (line 3, fan-in: 0) [LOW]\n\
             2. [ ] Verify all plan targets in the running build\n"
        );
        assert_eq!(
            markdown(true, &one),
            "1. [ ] Review src/a.rs in src/a.rs (line 3, fan-in: 0) [LOW]\n"
        );

        // Three findings across two files: a leading map item shifts the
        // numbering by one and the verify item closes the list.
        let three = [
            finding("src/a.rs", 1, 9),
            finding("src/a.rs", 7, 3),
            finding("src/b.rs", 2, 0),
        ];
        let text = markdown(false, &three);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines,
            vec![
                "1. [ ] Map 3 findings across 2 files (ranked by fan-in)",
                "2. [ ] Review src/a.rs in src/a.rs (line 1, fan-in: 9) [HIGH]",
                "3. [ ] Review src/a.rs in src/a.rs (line 7, fan-in: 3) [MEDIUM]",
                "4. [ ] Review src/b.rs in src/b.rs (line 2, fan-in: 0) [LOW]",
                "5. [ ] Verify all plan targets in the running build",
            ]
        );
        assert!(!markdown(true, &three).contains("Verify"));
    }
}

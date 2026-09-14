//! `pixel plan` command — deterministic todo list generation.

use std::collections::HashSet;
use std::path::PathBuf;

use pixel_git::{GitRunner, discover_root};
use pixel_graph::plan::{PlanFinding, PlanQuery};
use pixel_graph::{GraphStore, build};
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
    let root =
        discover_root(&opts.path).ok_or_else(|| "could not discover repo root".to_string())?;
    let db_path = root.join(pixel_index::index::SHARD_DIR).join("graph.db");
    // Always rebuild the graph before planning so the analysis is current.
    build::build_graph(&root, &db_path).map_err(|e| format!("graph build failed: {e}"))?;
    let store = GraphStore::open(&db_path).map_err(|e| format!("graph store: {e}"))?;
    let runner = GitRunner::new(&root);
    let queries = build_queries(&opts)?;
    let findings = pixel_graph::plan::run_plan_queries(&store, &root, &runner, &queries)
        .map_err(|e| format!("plan: {e}"))?;
    render(opts, findings)?;
    Ok(())
}

fn build_queries(opts: &PlanOptions) -> Result<Vec<PlanQuery>, String> {
    if let Some(q) = &opts.query {
        return parse_explicit_query(q, opts);
    }
    let prompt = opts
        .prompt
        .as_deref()
        .ok_or_else(|| "missing prompt (or pass --query)".to_string())?;
    Ok(classify_prompt(prompt))
}

fn parse_explicit_query(q: &str, opts: &PlanOptions) -> Result<Vec<PlanQuery>, String> {
    match q {
        "dead-interactive" => Ok(vec![PlanQuery::DeadInteractive {
            tag_filter: opts.tag.clone(),
        }]),
        "dead-code" => Ok(vec![PlanQuery::DeadCode]),
        "hotspots" => Ok(vec![PlanQuery::Hotspots {
            limit: opts.limit.unwrap_or(10),
        }]),
        "recent-changes" => Ok(vec![PlanQuery::RecentChanges {
            max_files: opts.limit.unwrap_or(20),
        }]),
        "by-concept" => {
            let query = opts
                .prompt
                .clone()
                .ok_or_else(|| "by-concept requires a prompt".to_string())?;
            Ok(vec![PlanQuery::ByConcept { query }])
        }
        _ => Err(format!("unknown query '{q}'")),
    }
}

fn classify_prompt(prompt: &str) -> Vec<PlanQuery> {
    let lower = prompt.to_lowercase();
    let mut queries = Vec::new();

    // Interactive-element queries: push all matching variants so multi-intent
    // prompts ("links or buttons") get full coverage, not just the first hit.
    if lower.contains("clickable") || lower.contains("interactive") {
        queries.push(PlanQuery::DeadInteractive { tag_filter: None });
    } else {
        let mut tags = Vec::new();
        if lower.contains("button") {
            tags.push("button");
        }
        if lower.contains("link") || lower.contains("navigation") {
            tags.push("a");
            tags.push("Link");
        }
        if lower.contains("click") && tags.is_empty() {
            queries.push(PlanQuery::DeadInteractive { tag_filter: None });
        }
        for tag in tags {
            queries.push(PlanQuery::DeadInteractive {
                tag_filter: Some(tag.to_string()),
            });
        }
    }

    if lower.contains("dead code") || lower.contains("unused") || lower.contains("remove") {
        queries.push(PlanQuery::DeadCode);
    }
    if lower.contains("refactor") || lower.contains("hotspot") || lower.contains("priority") {
        queries.push(PlanQuery::Hotspots { limit: 10 });
    }
    if lower.contains("recent") || lower.contains("bug") || lower.contains("regression") {
        queries.push(PlanQuery::RecentChanges { max_files: 20 });
    }

    // Always try concept match as a fallback (plan Option A).
    if queries.is_empty() {
        queries.push(PlanQuery::ByConcept {
            query: prompt.to_string(),
        });
    }
    queries
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

fn render_markdown(opts: &PlanOptions, findings: &[PlanFinding]) -> Result<(), String> {
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
        if !opts.no_verify {
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
        if !opts.no_verify {
            let n = findings.len() + 1;
            output.push_str(&format!(
                "{n}. [ ] Verify all plan targets in the running build\n"
            ));
        }
    }
    print!("{}", output);
    Ok(())
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

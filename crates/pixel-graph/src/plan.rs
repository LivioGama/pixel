//! Deterministic plan query engine for `pixel plan`.
//!
//! No LLM is used for the code analysis: queries run against the graph.db
//! schema (symbols, edges, imports, jsx_elements, concepts) and git history.

use std::collections::HashMap;
use std::path::Path;

use crate::store::GraphStore;
use crate::{concept_resolve, concept_resolve::ResolveOptions};

/// JSX tags that are inherently interactive — a dead element with one of
/// these tags is a real "broken button/link" finding. Structural tags (div,
/// span, svg, etc.) are not reported as dead interactive.
const INTERACTIVE_TAGS: &[&str] = &["button", "a", "Link", "NavLink"];

/// A predefined, deterministic query that `pixel plan` can run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanQuery {
    /// Find JSX elements missing event handlers (dead buttons/links).
    DeadInteractive { tag_filter: Option<String> },
    /// Find functions/methods with zero callers (dead code).
    DeadCode,
    /// Find files with highest fan-in (most depended on — fix these first).
    Hotspots { limit: usize },
    /// Find symbols matching a concept (uses existing concept_resolve).
    ByConcept { query: String },
    /// Find files changed in recent git history (recent churn = likely bug area).
    RecentChanges { max_files: usize },
}

/// Severity derived from fan-in for prioritization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    High,
    Medium,
    Low,
}

impl Severity {
    pub fn from_fan_in(fan_in: u32) -> Self {
        match fan_in {
            0..=2 => Severity::Low,
            3..=7 => Severity::Medium,
            _ => Severity::High,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Severity::High => "HIGH",
            Severity::Medium => "MEDIUM",
            Severity::Low => "LOW",
        }
    }
}

/// One item that becomes a todo entry.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PlanFinding {
    pub file: String,
    pub line: u32,
    pub label: String,
    pub fan_in: u32,
    pub severity: Severity,
}

/// Run a list of plan queries and merge the results.
///
/// Findings are sorted by severity (high → low) and then fan-in descending.
/// Duplicate (file, line, label) tuples are collapsed.
pub fn run_plan_queries(
    store: &GraphStore,
    root: &Path,
    runner: &pixel_git::GitRunner,
    queries: &[PlanQuery],
) -> Result<Vec<PlanFinding>, Box<dyn std::error::Error + Send + Sync>> {
    let mut out: Vec<PlanFinding> = Vec::new();
    let mut seen: HashMap<(String, u32, String), ()> = HashMap::new();

    for q in queries {
        let batch = match q {
            PlanQuery::DeadInteractive { tag_filter } => {
                dead_interactive(store, tag_filter.as_deref())?
            }
            PlanQuery::DeadCode => dead_code(store)?,
            PlanQuery::Hotspots { limit } => hotspots(store, *limit)?,
            PlanQuery::ByConcept { query } => by_concept(store, query)?,
            PlanQuery::RecentChanges { max_files } => {
                recent_changes(store, root, runner, *max_files)?
            }
        };
        for f in batch {
            let key = (f.file.clone(), f.line, f.label.clone());
            if seen.insert(key, ()).is_none() {
                out.push(f);
            }
        }
    }

    // Sort: severity high first, then fan-in descending, then path/line ascending.
    out.sort_by(|a, b| {
        let sev_a = match a.severity {
            Severity::High => 2,
            Severity::Medium => 1,
            Severity::Low => 0,
        };
        let sev_b = match b.severity {
            Severity::High => 2,
            Severity::Medium => 1,
            Severity::Low => 0,
        };
        sev_b
            .cmp(&sev_a)
            .then_with(|| b.fan_in.cmp(&a.fan_in))
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.cmp(&b.line))
    });
    Ok(out)
}

fn dead_interactive(
    store: &GraphStore,
    tag_filter: Option<&str>,
) -> Result<Vec<PlanFinding>, Box<dyn std::error::Error + Send + Sync>> {
    // When no tag filter is given, only return known interactive tags — a
    // div or span with no handler is just structural, not "dead interactive".
    let tags: Vec<&str> = match tag_filter {
        Some(t) => vec![t],
        None => INTERACTIVE_TAGS.to_vec(),
    };
    let mut rows = Vec::new();
    for tag in tags {
        rows.extend(store.jsx_elements_dead(None, Some(tag))?);
    }
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let file_ids: Vec<i64> = rows.iter().map(|r| r.file_id).collect();
    let fan_in = fan_in_for_files(store, &file_ids)?;
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let file = store
            .file_by_id(r.file_id)?
            .map(|f| f.path)
            .unwrap_or_default();
        let label = if r.text_content.is_empty() {
            format!("Wire unnamed {} element", r.tag)
        } else {
            format!("Wire '{}' {}", r.text_content, r.tag)
        };
        let fi = *fan_in.get(&file).unwrap_or(&0);
        out.push(PlanFinding {
            file,
            line: r.start_line,
            label,
            fan_in: fi,
            severity: Severity::from_fan_in(fi),
        });
    }
    Ok(out)
}

fn dead_code(
    store: &GraphStore,
) -> Result<Vec<PlanFinding>, Box<dyn std::error::Error + Send + Sync>> {
    // Only functions and methods can be "dead code" — modules, classes, and
    // structs are structural containers, not callable targets. References
    // edges count as evidence of use: a function passed as a callback
    // (`schema.plugin(fn)`) is registered, not dead.
    let mut stmt = store.conn().prepare(
        "SELECT s.id, s.file_id, s.name, s.qualified, s.kind, s.start_line, f.path \
         FROM symbols s \
         JOIN files f ON s.file_id = f.id \
         WHERE s.kind IN ('function', 'method') \
         AND NOT EXISTS (SELECT 1 FROM edges e WHERE e.dst_id = s.id AND e.kind IN ('calls', 'references')) \
         ORDER BY s.id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, u32>(5)?,
            r.get::<_, String>(6)?,
        ))
    })?;
    let mut syms: Vec<(i64, i64, String, String, String, u32, String)> = Vec::new();
    for r in rows {
        syms.push(r?);
    }
    if syms.is_empty() {
        return Ok(Vec::new());
    }
    let file_ids: Vec<i64> = syms.iter().map(|s| s.1).collect();
    let fan_in = fan_in_for_files(store, &file_ids)?;
    let mut out = Vec::with_capacity(syms.len());
    for (_, _, name, qualified, kind, line, file) in syms {
        // Impact envelope: if unresolved same-name call sites exist, the
        // resolver gave up — the symbol may have callers we couldn't link.
        // Don't flag it as dead; that would be a false positive.
        let envelope = store.envelope_for_name(&name)?;
        if envelope.lower_bound {
            continue;
        }
        let fi = *fan_in.get(&file).unwrap_or(&0);
        let label = format!("Remove unused {kind} `{name}` (qualified: {qualified})");
        out.push(PlanFinding {
            file,
            line,
            label,
            fan_in: fi,
            severity: Severity::from_fan_in(fi),
        });
    }
    Ok(out)
}

fn hotspots(
    store: &GraphStore,
    limit: usize,
) -> Result<Vec<PlanFinding>, Box<dyn std::error::Error + Send + Sync>> {
    let mut stmt = store.conn().prepare(
        "SELECT f.path, COUNT(DISTINCT f2.id) AS fan_in \
         FROM edges e \
         JOIN symbols s ON e.dst_id = s.id \
         JOIN files f ON s.file_id = f.id \
         JOIN symbols s2 ON e.src_id = s2.id \
         JOIN files f2 ON s2.file_id = f2.id \
         WHERE e.kind = 'calls' AND f2.id != f.id \
         GROUP BY f.path \
         ORDER BY fan_in DESC",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u32))
    })?;
    let mut out = Vec::new();
    for (i, r) in rows.enumerate() {
        if i >= limit {
            break;
        }
        let (path, fi) = r?;
        out.push(PlanFinding {
            file: path.clone(),
            line: 1,
            label: format!("Refactor hotspot file {path} ({fi} dependents)"),
            fan_in: fi,
            severity: Severity::from_fan_in(fi),
        });
    }
    Ok(out)
}

fn by_concept(
    store: &GraphStore,
    query: &str,
) -> Result<Vec<PlanFinding>, Box<dyn std::error::Error + Send + Sync>> {
    let opts = ResolveOptions {
        limit: 20,
        ..ResolveOptions::default()
    };
    let outcome = concept_resolve::resolve(store, query, &opts)?;
    let paths: Vec<String> = outcome.matches.iter().map(|m| m.path.clone()).collect();
    let fan_in = fan_in_for_file_paths(store, &paths)?;
    let mut out = Vec::with_capacity(outcome.matches.len());
    for m in outcome.matches {
        let fi = *fan_in.get(&m.path).unwrap_or(&0);
        let label = if let Some(owner) = m.owner {
            format!("{owner}: {}", m.raw)
        } else {
            m.raw
        };
        out.push(PlanFinding {
            file: m.path,
            line: m.start_line,
            label,
            fan_in: fi,
            severity: Severity::from_fan_in(fi),
        });
    }
    Ok(out)
}

fn recent_changes(
    store: &GraphStore,
    _root: &Path,
    runner: &pixel_git::GitRunner,
    max_files: usize,
) -> Result<Vec<PlanFinding>, Box<dyn std::error::Error + Send + Sync>> {
    let output = runner
        .run_opt(&["log", "--since=30.days", "--name-only", "--pretty=format:"])
        .unwrap_or_default();
    let text = String::from_utf8_lossy(&output);
    let mut paths: Vec<String> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(ToString::to_string)
        .collect();
    paths.sort();
    paths.dedup();
    paths.truncate(max_files);
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let fan_in = fan_in_for_file_paths(store, &paths)?;
    let mut out = Vec::with_capacity(paths.len());
    for p in paths {
        if store.file_by_path(&p)?.is_some() {
            let fi = *fan_in.get(&p).unwrap_or(&0);
            out.push(PlanFinding {
                file: p.clone(),
                line: 1,
                label: format!("Review recent changes in {p}"),
                fan_in: fi,
                severity: Severity::from_fan_in(fi),
            });
        }
    }
    Ok(out)
}

fn fan_in_for_files(
    store: &GraphStore,
    file_ids: &[i64],
) -> Result<HashMap<String, u32>, Box<dyn std::error::Error + Send + Sync>> {
    if file_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let placeholders: Vec<&str> = file_ids.iter().map(|_| "?").collect();
    let sql = format!(
        "SELECT f.path, COUNT(DISTINCT f2.id) AS fan_in \
         FROM edges e \
         JOIN symbols s ON e.dst_id = s.id \
         JOIN files f ON s.file_id = f.id \
         JOIN symbols s2 ON e.src_id = s2.id \
         JOIN files f2 ON s2.file_id = f2.id \
         WHERE e.kind = 'calls' AND f2.id != f.id AND f.id IN ({}) \
         GROUP BY f.path",
        placeholders.join(",")
    );
    let mut stmt = store.conn().prepare(&sql)?;
    let ids: Vec<Box<dyn rusqlite::ToSql>> = file_ids
        .iter()
        .map(|id| Box::new(*id) as Box<dyn rusqlite::ToSql>)
        .collect();
    let refs: Vec<&dyn rusqlite::ToSql> = ids.iter().map(AsRef::as_ref).collect();
    let rows = stmt.query_map(refs.as_slice(), |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u32))
    })?;
    let mut out = HashMap::new();
    for r in rows {
        let (p, c) = r?;
        out.insert(p, c);
    }
    Ok(out)
}

fn fan_in_for_file_paths(
    store: &GraphStore,
    paths: &[String],
) -> Result<HashMap<String, u32>, Box<dyn std::error::Error + Send + Sync>> {
    if paths.is_empty() {
        return Ok(HashMap::new());
    }
    let placeholders: Vec<&str> = paths.iter().map(|_| "?").collect();
    let sql = format!(
        "SELECT f.path, COUNT(DISTINCT f2.id) AS fan_in \
         FROM edges e \
         JOIN symbols s ON e.dst_id = s.id \
         JOIN files f ON s.file_id = f.id \
         JOIN symbols s2 ON e.src_id = s2.id \
         JOIN files f2 ON s2.file_id = f2.id \
         WHERE e.kind = 'calls' AND f2.id != f.id AND f.path IN ({}) \
         GROUP BY f.path",
        placeholders.join(",")
    );
    let mut stmt = store.conn().prepare(&sql)?;
    let params: Vec<Box<dyn rusqlite::ToSql>> = paths
        .iter()
        .map(|p| Box::new(p.clone()) as Box<dyn rusqlite::ToSql>)
        .collect();
    let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(AsRef::as_ref).collect();
    let rows = stmt.query_map(refs.as_slice(), |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u32))
    })?;
    let mut out = HashMap::new();
    for r in rows {
        let (p, c) = r?;
        out.insert(p, c);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{EdgeKind, EdgeRow, GraphStore, SymbolKind, Tier};

    fn file(store: &mut GraphStore, path: &str) -> i64 {
        store.replace_file(path, "oid", "ts").unwrap()
    }

    fn sym(store: &mut GraphStore, fid: i64, name: &str, kind: SymbolKind, line: u32) -> i64 {
        store
            .insert_symbol(
                fid,
                &format!("{name}#uid"),
                name,
                name,
                kind,
                line,
                line + 5,
                "",
            )
            .unwrap()
    }

    fn call(store: &mut GraphStore, src: i64, dst: i64) {
        store
            .insert_edge(&EdgeRow {
                src_id: src,
                dst_id: dst,
                kind: EdgeKind::Calls,
                tier: Tier::Exact,
                site_line: 1,
                receiver: None,
            })
            .unwrap();
    }

    #[test]
    fn severity_thresholds() {
        assert_eq!(Severity::from_fan_in(0), Severity::Low);
        assert_eq!(Severity::from_fan_in(1), Severity::Low);
        assert_eq!(Severity::from_fan_in(2), Severity::Low);
        assert_eq!(Severity::from_fan_in(3), Severity::Medium);
        assert_eq!(Severity::from_fan_in(7), Severity::Medium);
        assert_eq!(Severity::from_fan_in(8), Severity::High);
    }

    #[test]
    fn dead_code_skips_envelope_lower_bound() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let fa = file(&mut store, "src/a.ts");
        let fb = file(&mut store, "src/b.ts");
        // `used` is called from b — not dead.
        let used = sym(&mut store, fa, "used", SymbolKind::Function, 1);
        let caller = sym(&mut store, fb, "caller", SymbolKind::Function, 1);
        call(&mut store, caller, used);
        // `truly_dead` has zero callers and no unresolved same-name calls.
        sym(&mut store, fa, "truly_dead", SymbolKind::Function, 10);
        // `maybe_dead` has zero callers but an unresolved same-name call site
        // — the resolver gave up, so we must NOT flag it as dead.
        store
            .insert_unresolved_call(fa, "maybe_dead", None, 42, None, "calls")
            .unwrap();
        sym(&mut store, fa, "maybe_dead", SymbolKind::Function, 20);

        let findings = dead_code(&store).unwrap();
        let names: Vec<&str> = findings.iter().map(|f| f.label.as_str()).collect();
        assert!(
            names.iter().any(|n| n.contains("`truly_dead`")),
            "truly_dead should be flagged: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n.contains("`maybe_dead`")),
            "maybe_dead has envelope lower_bound — must not be flagged: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n.contains("`used`")),
            "used has a caller — must not be flagged: {names:?}"
        );
    }

    #[test]
    fn dead_code_skips_callback_references() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let fa = file(&mut store, "src/a.ts");
        let fb = file(&mut store, "src/b.ts");
        // `plugin_fn` is passed as a callback argument — references edge only.
        // Registered ≠ dead: must not be flagged.
        let plugin_fn = sym(&mut store, fa, "plugin_fn", SymbolKind::Function, 1);
        let registrar = sym(&mut store, fb, "registrar", SymbolKind::Function, 1);
        store
            .insert_edge(&EdgeRow {
                src_id: registrar,
                dst_id: plugin_fn,
                kind: EdgeKind::References,
                tier: Tier::Probable,
                site_line: 3,
                receiver: None,
            })
            .unwrap();
        // `orphan` has no edges at all — genuinely dead.
        sym(&mut store, fa, "orphan", SymbolKind::Function, 20);

        let findings = dead_code(&store).unwrap();
        let names: Vec<&str> = findings.iter().map(|f| f.label.as_str()).collect();
        assert!(
            !names.iter().any(|n| n.contains("`plugin_fn`")),
            "callback-referenced fn must not be flagged dead: {names:?}"
        );
        assert!(
            names.iter().any(|n| n.contains("`orphan`")),
            "orphan should be flagged: {names:?}"
        );
    }

    #[test]
    fn dead_code_excludes_non_callable_symbols() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let fa = file(&mut store, "src/a.ts");
        // A module and a struct with zero incoming calls — not dead code.
        sym(&mut store, fa, "MyModule", SymbolKind::Module, 1);
        sym(&mut store, fa, "MyStruct", SymbolKind::Struct, 5);
        let findings = dead_code(&store).unwrap();
        assert!(findings.is_empty(), "modules/structs are not dead code");
    }

    #[test]
    fn hotspots_count_distinct_files() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let fa = file(&mut store, "src/a.ts");
        let fb = file(&mut store, "src/b.ts");
        let fc = file(&mut store, "src/c.ts");
        // Three symbols in a, each called from b — fan_in should be 1 (one
        // distinct source file), not 3 (edges).
        let a1 = sym(&mut store, fa, "a1", SymbolKind::Function, 1);
        let a2 = sym(&mut store, fa, "a2", SymbolKind::Function, 5);
        let a3 = sym(&mut store, fa, "a3", SymbolKind::Function, 10);
        let b1 = sym(&mut store, fb, "b1", SymbolKind::Function, 1);
        let c1 = sym(&mut store, fc, "c1", SymbolKind::Function, 1);
        call(&mut store, b1, a1);
        call(&mut store, b1, a2);
        call(&mut store, b1, a3);
        call(&mut store, c1, a1);

        let findings = hotspots(&store, 10).unwrap();
        let a_finding = findings.iter().find(|f| f.file == "src/a.ts").unwrap();
        assert_eq!(a_finding.fan_in, 2, "two distinct files (b, c) call into a");
    }

    #[test]
    fn run_plan_queries_deduplicates_and_sorts() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let fa = file(&mut store, "src/a.ts");
        let fb = file(&mut store, "src/b.ts");
        let _dead = sym(&mut store, fa, "dead_fn", SymbolKind::Function, 1);
        let _caller = sym(&mut store, fb, "caller", SymbolKind::Function, 1);
        // No edge → both are dead. Dedup: DeadCode twice → each symbol once.
        let queries = vec![PlanQuery::DeadCode, PlanQuery::DeadCode];
        let root = std::path::Path::new(".");
        let runner = pixel_git::GitRunner::new(root);
        let findings = run_plan_queries(&store, root, &runner, &queries).unwrap();
        let dead_count = findings
            .iter()
            .filter(|f| f.label.contains("dead_fn"))
            .count();
        assert_eq!(dead_count, 1);
    }
}

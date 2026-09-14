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

impl PlanQuery {
    /// The `--query` spelling of this query.
    pub fn name(&self) -> &'static str {
        match self {
            PlanQuery::DeadInteractive { .. } => "dead-interactive",
            PlanQuery::DeadCode => "dead-code",
            PlanQuery::Hotspots { .. } => "hotspots",
            PlanQuery::ByConcept { .. } => "by-concept",
            PlanQuery::RecentChanges { .. } => "recent-changes",
        }
    }
}

/// The queries a `pixel plan` invocation runs: the explicit `--query` when
/// given, else the ones its prompt classifies to.
pub fn plan_queries(
    prompt: Option<&str>,
    query: Option<&str>,
    tag: Option<&str>,
    limit: Option<usize>,
) -> Result<Vec<PlanQuery>, String> {
    if let Some(q) = query {
        return explicit_query(q, prompt, tag, limit);
    }
    let prompt = prompt.ok_or_else(|| "missing prompt (or pass --query)".to_string())?;
    Ok(classify_prompt(prompt))
}

fn explicit_query(
    q: &str,
    prompt: Option<&str>,
    tag: Option<&str>,
    limit: Option<usize>,
) -> Result<Vec<PlanQuery>, String> {
    match q {
        "dead-interactive" => Ok(vec![PlanQuery::DeadInteractive {
            tag_filter: tag.map(str::to_string),
        }]),
        "dead-code" => Ok(vec![PlanQuery::DeadCode]),
        "hotspots" => Ok(vec![PlanQuery::Hotspots {
            limit: limit.unwrap_or(10),
        }]),
        "recent-changes" => Ok(vec![PlanQuery::RecentChanges {
            max_files: limit.unwrap_or(20),
        }]),
        "by-concept" => {
            let query = prompt.ok_or_else(|| "by-concept requires a prompt".to_string())?;
            Ok(vec![PlanQuery::ByConcept {
                query: query.to_string(),
            }])
        }
        _ => Err(format!(
            "unknown query '{q}' (dead-interactive | dead-code | hotspots | recent-changes | by-concept)"
        )),
    }
}

/// Classify a prompt into plan queries by its words, not its substrings:
/// "unlinked" is not about links, "clicked" is not "click". A word matches
/// its plural too ("buttons"). A prompt no query claims gets a concept match
/// on its own text.
pub fn classify_prompt(prompt: &str) -> Vec<PlanQuery> {
    let words: Vec<String> = prompt
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect();
    let has = |word: &str| {
        words
            .iter()
            .any(|w| w == word || w.strip_suffix('s') == Some(word))
    };
    let phrase = format!(" {} ", words.join(" "));
    let mut queries = Vec::new();

    // Interactive-element queries: push all matching variants so multi-intent
    // prompts ("links or buttons") get full coverage, not just the first hit.
    if has("clickable") || has("interactive") {
        queries.push(PlanQuery::DeadInteractive { tag_filter: None });
    } else {
        let mut tags = Vec::new();
        if has("button") {
            tags.push("button");
        }
        if has("link") || has("navigation") {
            tags.push("a");
            tags.push("Link");
        }
        if has("click") && tags.is_empty() {
            queries.push(PlanQuery::DeadInteractive { tag_filter: None });
        }
        for tag in tags {
            queries.push(PlanQuery::DeadInteractive {
                tag_filter: Some(tag.to_string()),
            });
        }
    }
    if phrase.contains(" dead code ") || has("unused") || has("remove") {
        queries.push(PlanQuery::DeadCode);
    }
    if has("refactor") || has("hotspot") || has("priority") {
        queries.push(PlanQuery::Hotspots { limit: 10 });
    }
    if has("recent") || has("bug") || has("regression") {
        queries.push(PlanQuery::RecentChanges { max_files: 20 });
    }
    if queries.is_empty() {
        queries.push(PlanQuery::ByConcept {
            query: prompt.to_string(),
        });
    }
    queries
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
        if is_entry_point(&name, &file) {
            continue;
        }
        // Impact envelope: if unresolved same-name call sites exist, the
        // resolver gave up — the symbol may have callers we couldn't link.
        // Don't flag it as dead; that would be a false positive.
        let envelope = store.envelope_for_name(&name)?;
        if envelope.lower_bound {
            continue;
        }
        let fi = *fan_in.get(&file).unwrap_or(&0);
        // "No callers found", never "unused": static extraction misses
        // dynamic dispatch, trait impls called through the trait, and
        // framework entry points.
        let label = format!(
            "No callers found for {kind} `{name}`: confirm it is unused before removing (qualified: {qualified})"
        );
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

/// A function nothing in the repository calls by design: a program entry
/// point (`main`, Go's `init`) or a test the harness runs. Reporting one as
/// dead code is noise at best and a deleted test at worst.
fn is_entry_point(name: &str, path: &str) -> bool {
    if name == "main" || (name == "init" && path.ends_with(".go")) {
        return true;
    }
    let file = path.rsplit('/').next().unwrap_or(path);
    let in_test_dir = path
        .split('/')
        .any(|dir| matches!(dir, "test" | "tests" | "__tests__" | "spec"));
    in_test_dir
        || file.ends_with("_test.go")
        || file.ends_with("_test.py")
        || (file.starts_with("test_") && file.ends_with(".py"))
        || file.contains(".test.")
        || file.contains(".spec.")
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
         ORDER BY fan_in DESC, f.path ASC",
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
    // A month of history on a busy repository exceeds the default 1 MiB
    // cap; the enumeration cap applies, and hitting it is an error, not an
    // empty answer.
    let runner = runner.with_max_output_bytes(Some(pixel_git::ENUMERATION_MAX_OUTPUT_BYTES));
    let output = match runner.run(&[
        "-c",
        "core.quotepath=false",
        "log",
        "--since=30.days",
        "--name-only",
        "--pretty=format:",
    ]) {
        Ok(output) => output,
        // Outside a repository or before the first commit nothing is recent.
        Err(pixel_git::GitError::NonZeroExit { .. }) => return Ok(Vec::new()),
        Err(e) => return Err(Box::new(e)),
    };
    let mut ranked = Vec::new();
    for (path, commits) in paths_by_churn(&String::from_utf8_lossy(&output)) {
        if ranked.len() == max_files {
            break;
        }
        if store.file_by_path(&path)?.is_some() {
            ranked.push((path, commits));
        }
    }
    let paths: Vec<String> = ranked.iter().map(|(p, _)| p.clone()).collect();
    let fan_in = fan_in_for_file_paths(store, &paths)?;
    Ok(ranked
        .into_iter()
        .map(|(p, commits)| {
            let fi = *fan_in.get(&p).unwrap_or(&0);
            PlanFinding {
                label: format!("Review recent changes in {p} ({commits} commit(s) in 30 days)"),
                file: p,
                line: 1,
                fan_in: fi,
                severity: Severity::from_fan_in(fi),
            }
        })
        .collect())
}

/// Paths of a `git log --name-only --pretty=format:` output with the number
/// of commits that touched each, most-touched first, ties by path.
fn paths_by_churn(log: &str) -> Vec<(String, usize)> {
    let mut commits: HashMap<&str, usize> = HashMap::new();
    for path in log.lines().map(str::trim).filter(|l| !l.is_empty()) {
        *commits.entry(path).or_default() += 1;
    }
    let mut ranked: Vec<(String, usize)> = commits
        .into_iter()
        .map(|(path, n)| (path.to_string(), n))
        .collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked
}

/// SQLite's default bound on host parameters is 32 766; one query per chunk
/// keeps `IN (…)` lists well under it on repositories of any size.
const FAN_IN_CHUNK: usize = 500;

fn fan_in_for_files(
    store: &GraphStore,
    file_ids: &[i64],
) -> Result<HashMap<String, u32>, Box<dyn std::error::Error + Send + Sync>> {
    let mut ids = file_ids.to_vec();
    ids.sort_unstable();
    ids.dedup();
    let mut out = HashMap::new();
    for chunk in ids.chunks(FAN_IN_CHUNK) {
        let params: Vec<Box<dyn rusqlite::ToSql>> = chunk
            .iter()
            .map(|id| Box::new(*id) as Box<dyn rusqlite::ToSql>)
            .collect();
        out.extend(fan_in_where(store, "f.id", &params)?);
    }
    Ok(out)
}

fn fan_in_for_file_paths(
    store: &GraphStore,
    paths: &[String],
) -> Result<HashMap<String, u32>, Box<dyn std::error::Error + Send + Sync>> {
    let mut unique = paths.to_vec();
    unique.sort_unstable();
    unique.dedup();
    let mut out = HashMap::new();
    for chunk in unique.chunks(FAN_IN_CHUNK) {
        let params: Vec<Box<dyn rusqlite::ToSql>> = chunk
            .iter()
            .map(|p| Box::new(p.clone()) as Box<dyn rusqlite::ToSql>)
            .collect();
        out.extend(fan_in_where(store, "f.path", &params)?);
    }
    Ok(out)
}

/// Distinct calling files per target file, for the files whose `column` is
/// one of `params`.
fn fan_in_where(
    store: &GraphStore,
    column: &str,
    params: &[Box<dyn rusqlite::ToSql>],
) -> Result<HashMap<String, u32>, Box<dyn std::error::Error + Send + Sync>> {
    let placeholders = vec!["?"; params.len()].join(",");
    let sql = format!(
        "SELECT f.path, COUNT(DISTINCT f2.id) AS fan_in \
         FROM edges e \
         JOIN symbols s ON e.dst_id = s.id \
         JOIN files f ON s.file_id = f.id \
         JOIN symbols s2 ON e.src_id = s2.id \
         JOIN files f2 ON s2.file_id = f2.id \
         WHERE e.kind = 'calls' AND f2.id != f.id AND {column} IN ({placeholders}) \
         GROUP BY f.path"
    );
    let mut stmt = store.conn().prepare(&sql)?;
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

    #[test]
    fn fan_in_counts_distinct_calling_files_by_id_and_by_path() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let fa = file(&mut store, "src/a.ts");
        let fb = file(&mut store, "src/b.ts");
        let fc = file(&mut store, "src/c.ts");
        let fd = file(&mut store, "src/d.ts");
        let a1 = sym(&mut store, fa, "a1", SymbolKind::Function, 1);
        let a2 = sym(&mut store, fa, "a2", SymbolKind::Function, 5);
        let b1 = sym(&mut store, fb, "b1", SymbolKind::Function, 1);
        let c1 = sym(&mut store, fc, "c1", SymbolKind::Function, 1);
        // b and c call into a (two edges from b: still one file); d calls nobody
        // and nobody calls d.
        call(&mut store, b1, a1);
        call(&mut store, b1, a2);
        call(&mut store, c1, a1);
        // A self-call within a must not count as fan-in.
        call(&mut store, a2, a1);

        let by_id = fan_in_for_files(&store, &[fa, fd]).unwrap();
        assert_eq!(by_id.get("src/a.ts"), Some(&2), "{by_id:?}");
        assert_eq!(by_id.get("src/d.ts"), None, "no callers, no row: {by_id:?}");
        assert_eq!(by_id.len(), 1);
        assert!(fan_in_for_files(&store, &[]).unwrap().is_empty());

        let by_path =
            fan_in_for_file_paths(&store, &["src/a.ts".to_string(), "src/d.ts".to_string()])
                .unwrap();
        assert_eq!(by_path.get("src/a.ts"), Some(&2), "{by_path:?}");
        assert_eq!(by_path.get("src/d.ts"), None, "{by_path:?}");
        assert_eq!(by_path.len(), 1);
        assert!(fan_in_for_file_paths(&store, &[]).unwrap().is_empty());
    }

    #[test]
    fn dead_interactive_reports_handlerless_interactive_elements_with_fan_in() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let fa = file(&mut store, "src/Form.tsx");
        let fb = file(&mut store, "src/App.tsx");
        let save = sym(&mut store, fa, "save", SymbolKind::Function, 1);
        let app = sym(&mut store, fb, "App", SymbolKind::Function, 1);
        call(&mut store, app, save);
        // A wired button, a dead button, a dead div (not interactive).
        store
            .insert_jsx_element(fa, "button", true, "Save", 3, 3)
            .unwrap();
        store
            .insert_jsx_element(fa, "button", false, "Cancel", 4, 4)
            .unwrap();
        store
            .insert_jsx_element(fa, "div", false, "", 5, 5)
            .unwrap();

        let dead = store.jsx_elements_dead(None, Some("button")).unwrap();
        assert_eq!(dead.len(), 1, "{dead:?}");
        assert_eq!(dead[0].text_content, "Cancel");
        assert_eq!(store.jsx_elements_dead(Some(fb), None).unwrap().len(), 0);
        assert_eq!(store.jsx_elements_dead(None, None).unwrap().len(), 2);

        let findings = dead_interactive(&store, None).unwrap();
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].file, "src/Form.tsx");
        assert_eq!(findings[0].line, 4);
        assert_eq!(findings[0].fan_in, 1, "App.tsx calls into Form.tsx");
        assert_eq!(dead_interactive(&store, Some("div")).unwrap().len(), 1);
        assert!(dead_interactive(&store, Some("form")).unwrap().is_empty());
    }

    fn git(root: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    fn commit(root: &Path, files: &[&str], msg: &str) {
        for p in files {
            let path = root.join(p);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let previous = std::fs::read_to_string(&path).unwrap_or_default();
            std::fs::write(&path, format!("{previous}{msg}\n")).unwrap();
        }
        git(root, &["add", "."]);
        git(root, &["commit", "-qm", msg]);
    }

    /// Recent churn points at the likely bug area: the files most commits
    /// touched come first, only files the graph knows are findings, and the
    /// cap keeps the most-touched ones rather than the first names.
    #[test]
    fn recent_changes_ranks_graph_files_by_commits_in_the_window() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git(root, &["init", "-q"]);
        commit(root, &["README.md", "src/a.ts", "src/b.ts"], "one");
        commit(root, &["README.md", "src/b.ts"], "two");
        commit(root, &["README.md", "src/b.ts", "src/c.ts"], "three");
        commit(root, &["src/c.ts"], "four");

        let mut store = GraphStore::open_in_memory().unwrap();
        let fa = file(&mut store, "src/a.ts");
        let fb = file(&mut store, "src/b.ts");
        file(&mut store, "src/c.ts");
        let a1 = sym(&mut store, fa, "a1", SymbolKind::Function, 1);
        let b1 = sym(&mut store, fb, "b1", SymbolKind::Function, 1);
        call(&mut store, a1, b1);
        let runner = pixel_git::GitRunner::new(root);

        let findings = recent_changes(&store, root, &runner, 10).unwrap();
        let files: Vec<&str> = findings.iter().map(|f| f.file.as_str()).collect();
        assert_eq!(
            files,
            ["src/b.ts", "src/c.ts", "src/a.ts"],
            "3 commits, then 2, then 1; README.md is not in the graph"
        );
        assert_eq!(
            findings[0].label,
            "Review recent changes in src/b.ts (3 commit(s) in 30 days)"
        );
        assert_eq!(findings[0].fan_in, 1, "a.ts calls into b.ts");
        assert_eq!(findings[0].line, 1);

        let capped = recent_changes(&store, root, &runner, 2).unwrap();
        let files: Vec<&str> = capped.iter().map(|f| f.file.as_str()).collect();
        assert_eq!(
            files,
            ["src/b.ts", "src/c.ts"],
            "the cap counts findings, after README.md was skipped"
        );
        assert!(recent_changes(&store, root, &runner, 0).unwrap().is_empty());

        // Outside a repository there is nothing recent.
        let empty = tempfile::tempdir().unwrap();
        let runner = pixel_git::GitRunner::new(empty.path());
        assert!(
            recent_changes(&store, empty.path(), &runner, 10)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn paths_by_churn_counts_commits_per_path_most_touched_first() {
        let log = [
            "",
            "src/b.ts",
            "src/a.ts",
            "",
            "src/b.ts",
            "  src/c.ts  ",
            "",
            "src/c.ts",
        ]
        .join("\n");
        assert_eq!(
            paths_by_churn(&log),
            [
                ("src/b.ts".to_string(), 2),
                ("src/c.ts".to_string(), 2),
                ("src/a.ts".to_string(), 1)
            ]
        );
        assert!(paths_by_churn("").is_empty());
    }

    #[test]
    fn prompts_classify_by_whole_words() {
        use PlanQuery::*;
        assert_eq!(
            classify_prompt("fix all clickable elements"),
            [DeadInteractive { tag_filter: None }]
        );
        assert_eq!(
            classify_prompt("wire the Buttons and navigation Links"),
            [
                DeadInteractive {
                    tag_filter: Some("button".into())
                },
                DeadInteractive {
                    tag_filter: Some("a".into())
                },
                DeadInteractive {
                    tag_filter: Some("Link".into())
                },
            ]
        );
        assert_eq!(
            classify_prompt("nothing happens on click"),
            [DeadInteractive { tag_filter: None }]
        );
        assert_eq!(
            classify_prompt("delete dead code, refactor by priority; recent regression"),
            [
                DeadCode,
                Hotspots { limit: 10 },
                RecentChanges { max_files: 20 }
            ]
        );
        assert_eq!(classify_prompt("remove unused helpers"), [DeadCode]);
        // Each intent word stands on its own.
        assert_eq!(classify_prompt("remove the legacy flag"), [DeadCode]);
        assert_eq!(classify_prompt("list unused exports"), [DeadCode]);
        assert_eq!(
            classify_prompt("refactor the parser"),
            [Hotspots { limit: 10 }]
        );
        assert_eq!(classify_prompt("show hotspots"), [Hotspots { limit: 10 }]);
        assert_eq!(
            classify_prompt("broken links"),
            [
                DeadInteractive {
                    tag_filter: Some("a".into())
                },
                DeadInteractive {
                    tag_filter: Some("Link".into())
                },
            ]
        );
        assert_eq!(
            classify_prompt("recent work"),
            [RecentChanges { max_files: 20 }]
        );
        assert_eq!(
            classify_prompt("a bug in billing"),
            [RecentChanges { max_files: 20 }]
        );
        // Substrings of other words are not intents.
        for prompt in [
            "unlinked invoices",
            "debugging the clicked handler",
            "codebase deadline",
        ] {
            assert_eq!(
                classify_prompt(prompt),
                [ByConcept {
                    query: prompt.to_string()
                }],
                "{prompt}"
            );
        }
    }

    #[test]
    fn plan_queries_prefer_the_explicit_query_and_name_every_query() {
        use PlanQuery::*;
        let q = |query, prompt, tag, limit| plan_queries(prompt, Some(query), tag, limit);
        assert_eq!(
            q("dead-interactive", None, Some("Link"), None).unwrap(),
            [DeadInteractive {
                tag_filter: Some("Link".into())
            }]
        );
        assert_eq!(
            q("dead-code", Some("links"), None, None).unwrap(),
            [DeadCode]
        );
        assert_eq!(
            q("hotspots", None, None, None).unwrap(),
            [Hotspots { limit: 10 }]
        );
        assert_eq!(
            q("hotspots", None, None, Some(3)).unwrap(),
            [Hotspots { limit: 3 }]
        );
        assert_eq!(
            q("recent-changes", None, None, None).unwrap(),
            [RecentChanges { max_files: 20 }]
        );
        assert_eq!(
            q("recent-changes", None, None, Some(4)).unwrap(),
            [RecentChanges { max_files: 4 }]
        );
        assert_eq!(
            q("by-concept", Some("invoice totals"), None, None).unwrap(),
            [ByConcept {
                query: "invoice totals".into()
            }]
        );
        assert_eq!(
            q("by-concept", None, None, None).unwrap_err(),
            "by-concept requires a prompt"
        );
        assert!(
            q("dead", None, None, None)
                .unwrap_err()
                .starts_with("unknown query 'dead'")
        );
        assert_eq!(
            plan_queries(Some("remove unused"), None, None, None).unwrap(),
            [DeadCode]
        );
        assert_eq!(
            plan_queries(None, None, None, None).unwrap_err(),
            "missing prompt (or pass --query)"
        );

        let names: Vec<&str> = [
            DeadInteractive { tag_filter: None },
            DeadCode,
            Hotspots { limit: 1 },
            ByConcept { query: "x".into() },
            RecentChanges { max_files: 1 },
        ]
        .iter()
        .map(PlanQuery::name)
        .collect();
        assert_eq!(
            names,
            [
                "dead-interactive",
                "dead-code",
                "hotspots",
                "by-concept",
                "recent-changes"
            ]
        );
        for name in names {
            let parsed = plan_queries(Some("p"), Some(name), None, None).unwrap();
            assert_eq!(parsed[0].name(), name, "--query {name} round-trips");
        }
    }

    #[test]
    fn severity_names_are_the_rendered_labels() {
        assert_eq!(Severity::High.as_str(), "HIGH");
        assert_eq!(Severity::Medium.as_str(), "MEDIUM");
        assert_eq!(Severity::Low.as_str(), "LOW");
    }

    #[test]
    fn entry_points_and_tests_are_never_dead_code() {
        for (name, path) in [
            ("main", "src/main.rs"),
            ("main", "cmd/tool/main.go"),
            ("init", "pkg/db.go"),
            ("test_login", "tests/test_auth.py"),
            ("it_works", "crates/x/tests/all/smoke.rs"),
            ("renders", "src/__tests__/App.tsx"),
            ("helper", "src/login.test.ts"),
            ("helper", "src/login.spec.ts"),
            ("TestLogin", "auth/login_test.go"),
            ("check", "auth/login_test.py"),
            ("check", "auth/test_login.py"),
            ("example", "spec/models/user_spec.rb"),
        ] {
            assert!(is_entry_point(name, path), "{name} in {path}");
        }
        for (name, path) in [
            ("init", "src/init.rs"),
            ("maintain", "src/main.rs"),
            ("helper", "src/testing.ts"),
            ("helper", "src/contest/score.py"),
            ("latest", "src/latest.go"),
        ] {
            assert!(!is_entry_point(name, path), "{name} in {path}");
        }
    }

    #[test]
    fn dead_code_says_no_callers_found_and_skips_entry_points() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let fa = file(&mut store, "src/main.rs");
        let ft = file(&mut store, "src/app.test.ts");
        sym(&mut store, fa, "main", SymbolKind::Function, 1);
        sym(&mut store, fa, "orphan", SymbolKind::Function, 9);
        sym(&mut store, ft, "helper", SymbolKind::Function, 1);
        let findings = dead_code(&store).unwrap();
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].file, "src/main.rs");
        assert_eq!(findings[0].line, 9);
        assert_eq!(
            findings[0].label,
            "No callers found for function `orphan`: confirm it is unused before removing (qualified: orphan)"
        );
    }

    /// Which files make the cut must not depend on row order: equal fan-in
    /// ranks by path, and exactly `limit` rows come back.
    #[test]
    fn hotspots_break_ties_by_path_and_stop_at_the_limit() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let caller_file = file(&mut store, "src/app.ts");
        let caller = sym(&mut store, caller_file, "app", SymbolKind::Function, 1);
        for path in ["src/z.ts", "src/m.ts", "src/b.ts"] {
            let fid = file(&mut store, path);
            let target = sym(
                &mut store,
                fid,
                &path.replace(['/', '.'], "_"),
                SymbolKind::Function,
                1,
            );
            call(&mut store, caller, target);
        }
        let files = |limit| -> Vec<String> {
            hotspots(&store, limit)
                .unwrap()
                .into_iter()
                .map(|f| f.file)
                .collect()
        };
        assert_eq!(files(2), ["src/b.ts", "src/m.ts"]);
        assert_eq!(files(3), ["src/b.ts", "src/m.ts", "src/z.ts"]);
        assert!(files(0).is_empty());
        let first = &hotspots(&store, 1).unwrap()[0];
        assert_eq!(first.label, "Refactor hotspot file src/b.ts (1 dependents)");
        assert_eq!(first.line, 1);
    }

    /// A plan over thousands of dead symbols asks for each file's fan-in
    /// once, in chunks SQLite accepts, and loses no file on a chunk edge.
    #[test]
    fn fan_in_deduplicates_and_chunks_large_id_and_path_lists() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let fa = file(&mut store, "src/a.ts");
        let fb = file(&mut store, "src/b.ts");
        let a1 = sym(&mut store, fa, "a1", SymbolKind::Function, 1);
        let b1 = sym(&mut store, fb, "b1", SymbolKind::Function, 1);
        call(&mut store, b1, a1);

        let mut ids: Vec<i64> = (1_000..34_000).collect();
        ids.extend(std::iter::repeat_n(fa, 40_000));
        let by_id = fan_in_for_files(&store, &ids).unwrap();
        assert_eq!(by_id.get("src/a.ts"), Some(&1), "{by_id:?}");
        assert_eq!(by_id.len(), 1);

        let mut paths: Vec<String> = (0..1_200).map(|i| format!("src/x{i}.ts")).collect();
        paths.push("src/a.ts".to_string());
        paths.push("src/a.ts".to_string());
        let by_path = fan_in_for_file_paths(&store, &paths).unwrap();
        assert_eq!(by_path.get("src/a.ts"), Some(&1), "{by_path:?}");
        assert_eq!(by_path.len(), 1);
    }

    #[test]
    fn run_plan_queries_orders_by_severity_then_fan_in_then_location() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let hot = file(&mut store, "src/hot.ts");
        let warm = file(&mut store, "src/warm.ts");
        let cold = file(&mut store, "src/cold.ts");
        // Dead symbols: two in hot.ts (fan-in 8: HIGH), one in warm.ts
        // (fan-in 3: MEDIUM), one in cold.ts (fan-in 0: LOW).
        sym(&mut store, cold, "cold_dead", SymbolKind::Function, 1);
        sym(&mut store, hot, "hot_dead_late", SymbolKind::Function, 20);
        sym(&mut store, hot, "hot_dead_early", SymbolKind::Function, 10);
        sym(&mut store, warm, "warm_dead", SymbolKind::Function, 5);
        let hot_target = sym(&mut store, hot, "hot_used", SymbolKind::Function, 1);
        let warm_target = sym(&mut store, warm, "warm_used", SymbolKind::Function, 1);
        for i in 0..8 {
            let fid = file(&mut store, &format!("src/callers/c{i}.ts"));
            let c = sym(&mut store, fid, &format!("c{i}"), SymbolKind::Function, 1);
            call(&mut store, c, hot_target);
            if i < 3 {
                call(&mut store, c, warm_target);
            }
        }
        let root = Path::new(".");
        let runner = pixel_git::GitRunner::new(root);
        let findings = run_plan_queries(
            &store,
            root,
            &runner,
            &[PlanQuery::DeadCode, PlanQuery::DeadCode],
        )
        .unwrap();
        let order: Vec<(&str, u32, u32)> = findings
            .iter()
            .filter(|f| !f.file.starts_with("src/callers/"))
            .map(|f| (f.file.as_str(), f.line, f.fan_in))
            .collect();
        assert_eq!(
            order,
            [
                ("src/hot.ts", 10, 8),
                ("src/hot.ts", 20, 8),
                ("src/warm.ts", 5, 3),
                ("src/cold.ts", 1, 0),
            ],
            "{findings:?}"
        );
        let first_low = findings
            .iter()
            .position(|f| f.severity == Severity::Low)
            .unwrap();
        assert!(
            findings[first_low..]
                .iter()
                .all(|f| f.severity == Severity::Low)
        );
    }

    #[test]
    fn by_concept_turns_concept_matches_into_findings() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("billing.ts"),
            "export function calculateInvoiceTotal(lines: number[]) { return lines.length; }\n",
        )
        .unwrap();
        let db = dir.path().join("graph.db");
        crate::build::build_graph(dir.path(), &db).unwrap();
        let store = GraphStore::open(&db).unwrap();
        let findings = by_concept(&store, "invoice total").unwrap();
        let hit = findings
            .iter()
            .find(|f| f.label.contains("calculateInvoiceTotal"))
            .unwrap_or_else(|| panic!("{findings:?}"));
        assert_eq!(hit.file, "billing.ts");
        assert_eq!(hit.line, 1);
        assert_eq!(hit.severity, Severity::Low);
        assert!(by_concept(&store, "zzqx unrelated").unwrap().is_empty());
    }
}

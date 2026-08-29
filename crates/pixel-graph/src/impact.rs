//! Impact analysis — BFS over `calls` edges, depth-bucketed blast radius with
//! an epistemic envelope so "0 callers" is distinguishable from "resolver gave up".

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::str::FromStr;

use rusqlite::params;
use serde::Serialize;

use crate::store::{EdgeKind, Envelope, GraphStore, StoreError, SymbolRow, Tier};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Upstream,
    Downstream,
}

impl Direction {
    pub fn as_str(&self) -> &'static str {
        match self {
            Direction::Upstream => "upstream",
            Direction::Downstream => "downstream",
        }
    }
}

impl FromStr for Direction {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "upstream" | "up" | "callers" => Ok(Direction::Upstream),
            "downstream" | "down" | "callees" => Ok(Direction::Downstream),
            other => Err(format!(
                "unknown direction: {other} (expected upstream|downstream)"
            )),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ImpactItem {
    pub uid: String,
    pub name: String,
    pub path: String,
    pub line: u32,
    pub tier: String,
    pub processes: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImpactReport {
    pub target: String,
    pub direction: String,
    /// "LOW" | "MEDIUM" | "HIGH" | "CRITICAL"
    pub risk: String,
    pub summary: String,
    pub d1_will_break: Vec<ImpactItem>,
    pub d2_likely_affected: Vec<ImpactItem>,
    pub d3_may_need_tests: Vec<ImpactItem>,
    pub counts_by_depth: [u64; 3],
    pub affected_files: u64,
    pub affected_processes: Vec<String>,
    pub envelope: Envelope,
}

/// Fetch a symbol row by rowid via ad-hoc SQL (store exposes uid/name lookups only).
pub(crate) fn symbol_by_id(store: &GraphStore, id: i64) -> Result<Option<SymbolRow>, StoreError> {
    use rusqlite::OptionalExtension;
    let row = store
        .conn()
        .query_row(
            "SELECT id, uid, file_id, name, qualified, kind, start_line, end_line, sig
             FROM symbols WHERE id = ?1",
            params![id],
            |r| {
                Ok(SymbolRow {
                    id: r.get(0)?,
                    uid: r.get(1)?,
                    file_id: r.get(2)?,
                    name: r.get(3)?,
                    qualified: r.get(4)?,
                    kind: crate::store::SymbolKind::parse(&r.get::<_, String>(5)?),
                    start_line: r.get(6)?,
                    end_line: r.get(7)?,
                    sig: r.get(8)?,
                })
            },
        )
        .optional()?;
    Ok(row)
}

pub(crate) fn file_path_by_id(store: &GraphStore, file_id: i64) -> Result<String, StoreError> {
    use rusqlite::OptionalExtension;
    Ok(store
        .conn()
        .query_row(
            "SELECT path FROM files WHERE id = ?1",
            params![file_id],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .unwrap_or_default())
}

/// Distinct process labels a symbol participates in.
pub(crate) fn processes_for_symbol(
    store: &GraphStore,
    symbol_id: i64,
) -> Result<Vec<String>, StoreError> {
    let mut stmt = store.conn().prepare(
        "SELECT DISTINCT p.label FROM processes p
         JOIN process_steps ps ON ps.process_id = p.id
         WHERE ps.symbol_id = ?1 ORDER BY p.label",
    )?;
    let rows = stmt.query_map(params![symbol_id], |r| r.get::<_, String>(0))?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

fn risk_label(mut level: u8, lower_bound: bool) -> String {
    if lower_bound && level < 3 {
        level += 1;
    }
    match level {
        0 => "LOW",
        1 => "MEDIUM",
        2 => "HIGH",
        _ => "CRITICAL",
    }
    .to_string()
}

pub fn impact(
    store: &GraphStore,
    uid: &str,
    direction: Direction,
    max_depth: u32,
    limit_per_depth: usize,
) -> Result<ImpactReport, StoreError> {
    let target = store
        .symbol_by_uid(uid)?
        .ok_or_else(|| StoreError::Sql(rusqlite::Error::QueryReturnedNoRows))?;
    let max_depth = max_depth.clamp(1, 3);

    let mut visited: HashSet<i64> = HashSet::new();
    visited.insert(target.id);
    // (symbol_id, depth, tier of the edge that reached it)
    let mut queue: VecDeque<(i64, u32, Tier)> = VecDeque::new();
    queue.push_back((target.id, 0, Tier::Exact));

    let mut buckets: [Vec<ImpactItem>; 3] = [Vec::new(), Vec::new(), Vec::new()];
    let mut counts: [u64; 3] = [0, 0, 0];
    let mut files: HashSet<i64> = HashSet::new();
    let mut proc_set: BTreeSet<String> = BTreeSet::new();

    while let Some((id, depth, _)) = queue.pop_front() {
        if depth >= max_depth {
            continue;
        }
        let edges = match direction {
            Direction::Upstream => store.edges_to(id, Some(EdgeKind::Calls))?,
            Direction::Downstream => store.edges_from(id, Some(EdgeKind::Calls))?,
        };
        for e in edges {
            let nbr = match direction {
                Direction::Upstream => e.src_id,
                Direction::Downstream => e.dst_id,
            };
            if !visited.insert(nbr) {
                continue;
            }
            let d = depth + 1;
            let bucket = (d - 1) as usize;
            counts[bucket] += 1;
            if let Some(sym) = symbol_by_id(store, nbr)? {
                files.insert(sym.file_id);
                let procs = processes_for_symbol(store, sym.id)?;
                for p in &procs {
                    proc_set.insert(p.clone());
                }
                if buckets[bucket].len() < limit_per_depth {
                    buckets[bucket].push(ImpactItem {
                        uid: sym.uid,
                        name: sym.name,
                        path: file_path_by_id(store, sym.file_id)?,
                        line: sym.start_line,
                        tier: e.tier.as_str().to_string(),
                        processes: procs,
                    });
                }
            }
            queue.push_back((nbr, d, e.tier));
        }
    }

    let envelope = store.envelope_for_name(&target.name)?;
    let affected_processes: Vec<String> = proc_set.into_iter().collect();
    let d1 = counts[0];
    let nproc = affected_processes.len();
    let base = if d1 > 50 || nproc > 20 {
        3
    } else if d1 > 15 || nproc > 8 {
        2
    } else if d1 > 3 {
        1
    } else {
        0
    };
    let risk = risk_label(base, envelope.lower_bound);
    let summary = format!(
        "{}: {} at depth 1, {} at depth 2, {} at depth 3 ({}) across {} files, {} processes; risk {}{}",
        target.name,
        counts[0],
        counts[1],
        counts[2],
        direction.as_str(),
        files.len(),
        nproc,
        risk,
        if envelope.lower_bound {
            format!(
                " (lower bound: {} unresolved same-name call sites)",
                envelope.unresolved_same_name
            )
        } else {
            String::new()
        }
    );

    let [d1_will_break, d2_likely_affected, d3_may_need_tests] = buckets;
    Ok(ImpactReport {
        target: target.uid,
        direction: direction.as_str().to_string(),
        risk,
        summary,
        d1_will_break,
        d2_likely_affected,
        d3_may_need_tests,
        counts_by_depth: counts,
        affected_files: files.len() as u64,
        affected_processes,
        envelope,
    })
}

/// Word-frequency helper shared by cluster/process labeling: split camelCase +
/// snake_case identifiers into lowercase words.
pub fn split_ident_words(name: &str) -> Vec<String> {
    let mut words = Vec::new();
    for chunk in name.split(['_', '-', '.', ':', '#']) {
        if chunk.is_empty() {
            continue;
        }
        let mut cur = String::new();
        let chars: Vec<char> = chunk.chars().collect();
        for (i, &c) in chars.iter().enumerate() {
            let boundary = c.is_uppercase()
                && i > 0
                && (chars[i - 1].is_lowercase()
                    || (i + 1 < chars.len() && chars[i + 1].is_lowercase()));
            if boundary && !cur.is_empty() {
                words.push(cur.to_lowercase());
                cur = String::new();
            }
            cur.push(c);
        }
        if !cur.is_empty() {
            words.push(cur.to_lowercase());
        }
    }
    words
}

#[allow(dead_code)]
pub(crate) fn word_counts<'a, I: Iterator<Item = &'a str>>(names: I) -> HashMap<String, u64> {
    let mut m = HashMap::new();
    for n in names {
        for w in split_ident_words(n) {
            *m.entry(w).or_insert(0) += 1;
        }
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{EdgeRow, GraphStore, SymbolKind, Tier};
    use crate::trace;

    fn sym(store: &GraphStore, file_id: i64, name: &str, start: u32, end: u32) -> i64 {
        store
            .insert_symbol(
                file_id,
                &format!("src/a.ts#{name}#function"),
                name,
                name,
                SymbolKind::Function,
                start,
                end,
                "",
            )
            .unwrap()
    }

    fn call(store: &GraphStore, src: i64, dst: i64) {
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
    fn impact_buckets_envelope_and_trace() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let fid = store.replace_file("src/a.ts", "oid", "ts").unwrap();
        let a = sym(&store, fid, "alpha", 1, 5);
        let b = sym(&store, fid, "beta", 10, 15);
        let c = sym(&store, fid, "gamma", 20, 25);
        // c -> b -> a
        call(&store, b, a);
        call(&store, c, b);
        // unresolved same-name call site for "alpha" → lower bound
        store
            .insert_unresolved_call(fid, "alpha", None, 42, None)
            .unwrap();

        let report = impact(
            &store,
            "src/a.ts#alpha#function",
            Direction::Upstream,
            3,
            100,
        )
        .unwrap();
        assert_eq!(report.counts_by_depth, [1, 1, 0]);
        assert_eq!(report.d1_will_break.len(), 1);
        assert_eq!(report.d1_will_break[0].name, "beta");
        assert_eq!(report.d2_likely_affected.len(), 1);
        assert_eq!(report.d2_likely_affected[0].name, "gamma");
        assert!(report.envelope.lower_bound);
        assert_eq!(report.envelope.unresolved_same_name, 1);
        // LOW bumped one level by lower-bound envelope
        assert_eq!(report.risk, "MEDIUM");

        // trace c -> a found through b
        let t = trace::trace(
            &store,
            "src/a.ts#gamma#function",
            "src/a.ts#alpha#function",
            10,
        )
        .unwrap();
        assert!(t.found);
        assert_eq!(t.hops.len(), 3);
        assert_eq!(t.hops[0].name, "gamma");
        assert_eq!(t.hops[2].name, "alpha");
    }
}

//! Tiered call-graph resolution.
//!
//! Tiers (a name NEVER fans out to multiple definition sites as edges):
//! - T0: callee defined in the same file → `Exact`
//! - T1: callee defined in exactly one file the caller imports → `Exact`
//! - T2: callee name defined in exactly one file repo-wide → `Probable`
//! - otherwise → `unresolved_calls` row (feeds the epistemic envelope)
//!
//! Receiver honesty: a call with a real receiver expression (`x.parse()`,
//! `SymbolKind::parse`) can never be `Exact` from name-only resolution — the
//! receiver's type is not tracked, so linking it to a same-name function/
//! method would be a guess. Such calls are capped at `Probable`. Calls whose
//! receiver is `self`/`Self`/`this` (or absent) keep the normal tier, since
//! those resolve against the enclosing type's own methods.
//!
//! Known limitation: T1 still matches at file granularity (an import resolves
//! to a file, not to specific exported bindings). Refining this to per-name
//! import tracking requires recording imported binding names, which is left
//! for a follow-up; the receiver downgrade above already removes the cited
//! false-positive (`x.parse()` → `SymbolKind::parse`).

use std::collections::{HashMap, HashSet};

use rusqlite::params;

use crate::store::{EdgeKind, EdgeRow, GraphStore, StoreError, SymbolKind, Tier};

#[derive(Debug, Default, Clone)]
pub struct ResolveStats {
    pub exact: u64,
    pub probable: u64,
    pub unresolved: u64,
}

/// One extracted call site awaiting resolution (symbol ids already assigned).
#[derive(Debug, Clone)]
pub struct PendingCall {
    pub callee_name: String,
    pub enclosing_symbol_id: Option<i64>,
    pub site_line: u32,
    /// Receiver expression text if this is a method/field call (`x.m()`,
    /// `a::b()`), else `None` for a plain call (`m()`). Used to cap
    /// non-`self` receiver calls at `Probable`.
    pub receiver: Option<String>,
}

/// All pending calls of one file.
#[derive(Debug, Clone)]
pub struct FileCalls {
    pub file_id: i64,
    pub calls: Vec<PendingCall>,
}

/// Per-call resolution decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Exact(i64),
    Probable(i64),
    Unresolved,
}

#[derive(Clone, Copy)]
struct Candidate {
    file_id: i64,
    symbol_id: i64,
    kind: SymbolKind,
    start_line: u32,
}

/// Symbol-name index + import graph snapshot used for tier decisions.
pub struct ResolveIndex {
    by_name: HashMap<String, Vec<Candidate>>,
    /// file_id → set of imported file_ids (for file-level fallback).
    imports_of: HashMap<i64, HashSet<i64>>,
    /// (file_id, binding_name) → set of imported file_ids. When non-empty,
    /// T1 requires the callee name to be an imported binding from that file,
    /// not just any definition in an imported file. Empty binding sets fall
    /// back to file-level matching (the pre-fix behavior).
    import_bindings: HashMap<(i64, String), HashSet<i64>>,
}

fn callable(kind: SymbolKind) -> bool {
    matches!(
        kind,
        SymbolKind::Function | SymbolKind::Method | SymbolKind::Class | SymbolKind::Struct
    )
}

fn kind_priority(kind: SymbolKind) -> u8 {
    match kind {
        SymbolKind::Function => 0,
        SymbolKind::Method => 1,
        SymbolKind::Class => 2,
        SymbolKind::Struct => 3,
        _ => 9,
    }
}

fn best(cands: &[Candidate]) -> Option<i64> {
    cands
        .iter()
        .min_by_key(|c| (kind_priority(c.kind), c.start_line, c.symbol_id))
        .map(|c| c.symbol_id)
}

impl ResolveIndex {
    pub fn build(store: &GraphStore) -> Result<Self, StoreError> {
        let conn = store.conn();
        let mut by_name: HashMap<String, Vec<Candidate>> = HashMap::new();
        {
            let mut stmt =
                conn.prepare("SELECT name, file_id, id, kind, start_line FROM symbols")?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    Candidate {
                        file_id: r.get(1)?,
                        symbol_id: r.get(2)?,
                        kind: SymbolKind::parse(&r.get::<_, String>(3)?),
                        start_line: r.get(4)?,
                    },
                ))
            })?;
            for row in rows {
                let (name, cand) = row?;
                if callable(cand.kind) {
                    by_name.entry(name).or_default().push(cand);
                }
            }
        }
        let mut imports_of: HashMap<i64, HashSet<i64>> = HashMap::new();
        let mut import_bindings: HashMap<(i64, String), HashSet<i64>> = HashMap::new();
        {
            let mut stmt = conn.prepare(
                "SELECT file_id, resolved_file_id, bindings FROM imports WHERE resolved_file_id IS NOT NULL",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Option<i64>>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?;
            for row in rows {
                let (fid, dst_opt, bindings_csv) = row?;
                if let Some(dst) = dst_opt {
                    imports_of.entry(fid).or_default().insert(dst);
                    // Parse comma-separated binding names. Empty string means
                    // wildcard or unknown and grants no T1 Exact confidence.
                    for b in bindings_csv.split(',') {
                        let b = b.trim();
                        if !b.is_empty() {
                            import_bindings
                                .entry((fid, b.to_string()))
                                .or_default()
                                .insert(dst);
                        }
                    }
                }
            }
        }
        Ok(Self {
            by_name,
            imports_of,
            import_bindings,
        })
    }

    /// The tier decision for one call from `caller_file_id` to `name`.
    /// `receiver` is the receiver expression text (if any) of the call site;
    /// a real receiver (not `self`/`Self`/`this`) caps the result at
    /// `Probable` because the receiver's type is unknown to the resolver.
    pub fn decide(&self, caller_file_id: i64, name: &str, receiver: Option<&str>) -> Decision {
        let raw = self.decide_raw(caller_file_id, name);
        if matches!(raw, Decision::Exact(_)) && has_real_receiver(receiver) {
            // Downgrade: a non-self receiver means we cannot confirm the
            // callee is the same definition the receiver's type resolves to.
            return match raw {
                Decision::Exact(id) => Decision::Probable(id),
                _ => raw,
            };
        }
        raw
    }

    /// Tier decision ignoring receiver type (the original name-only logic).
    fn decide_raw(&self, caller_file_id: i64, name: &str) -> Decision {
        let Some(cands) = self.by_name.get(name) else {
            return Decision::Unresolved;
        };
        // T0: same file.
        let same_file: Vec<Candidate> = cands
            .iter()
            .copied()
            .filter(|c| c.file_id == caller_file_id)
            .collect();
        if let Some(id) = best(&same_file) {
            return Decision::Exact(id);
        }
        // T1: defined in exactly one file that explicitly imported this name.
        // File-level or wildcard imports cannot prove an unqualified binding,
        // so they remain eligible only for repo-wide T2 Probable resolution.
        if let Some(imported) = self.imports_of.get(&caller_file_id) {
            let binding_files = self
                .import_bindings
                .get(&(caller_file_id, name.to_string()));
            let mut effective_imported: HashSet<i64> = binding_files
                .into_iter()
                .flat_map(|files| files.iter().copied())
                .collect();
            effective_imported.retain(|file_id| imported.contains(file_id));
            let hits: Vec<Candidate> = cands
                .iter()
                .copied()
                .filter(|c| effective_imported.contains(&c.file_id))
                .collect();
            let files: HashSet<i64> = hits.iter().map(|c| c.file_id).collect();
            if files.len() == 1
                && let Some(id) = best(&hits)
            {
                return Decision::Exact(id);
            }
            if files.len() > 1 {
                return Decision::Unresolved; // ambiguous — never fan out
            }
        }
        // T2: unique definition file repo-wide.
        let files: HashSet<i64> = cands.iter().map(|c| c.file_id).collect();
        if files.len() == 1
            && let Some(id) = best(cands)
        {
            return Decision::Probable(id);
        }
        Decision::Unresolved
    }
}

/// True iff `receiver` is a real receiver expression (not absent and not one
/// of the self-pseudo-receivers). `self`/`Self`/`this`/`crate`/`super` resolve
/// against the enclosing type/module, so they keep the normal tier.
fn has_real_receiver(receiver: Option<&str>) -> bool {
    match receiver {
        None => false,
        Some(r) => {
            let r = r.trim();
            !r.is_empty()
                && !matches!(
                    r,
                    "self" | "Self" | "this" | "crate" | "super" | "Self::" | "self."
                )
        }
    }
}

/// Resolve the given in-memory pending calls, writing edges / unresolved
/// rows into the store. Used by `build::build_graph` after extraction.
pub fn resolve_calls(
    store: &GraphStore,
    pending: &[FileCalls],
) -> Result<ResolveStats, StoreError> {
    let idx = ResolveIndex::build(store)?;
    let mut stats = ResolveStats::default();
    for fc in pending {
        for call in &fc.calls {
            let Some(src_id) = call.enclosing_symbol_id else {
                // Top-level call site: no source symbol to hang an edge on.
                store.insert_unresolved_call(
                    fc.file_id,
                    &call.callee_name,
                    None,
                    call.site_line,
                    call.receiver.as_deref(),
                )?;
                stats.unresolved += 1;
                continue;
            };
            match idx.decide(fc.file_id, &call.callee_name, call.receiver.as_deref()) {
                Decision::Exact(dst) => {
                    store.insert_edge(&EdgeRow {
                        src_id,
                        dst_id: dst,
                        kind: EdgeKind::Calls,
                        tier: Tier::Exact,
                        site_line: call.site_line,
                        receiver: call.receiver.clone(),
                    })?;
                    stats.exact += 1;
                }
                Decision::Probable(dst) => {
                    store.insert_edge(&EdgeRow {
                        src_id,
                        dst_id: dst,
                        kind: EdgeKind::Calls,
                        tier: Tier::Probable,
                        site_line: call.site_line,
                        receiver: call.receiver.clone(),
                    })?;
                    stats.probable += 1;
                }
                Decision::Unresolved => {
                    store.insert_unresolved_call(
                        fc.file_id,
                        &call.callee_name,
                        Some(src_id),
                        call.site_line,
                        call.receiver.as_deref(),
                    )?;
                    stats.unresolved += 1;
                }
            }
        }
    }
    Ok(stats)
}

/// Re-attempt resolution of every stored `unresolved_calls` row against the
/// current index. Rows that resolve become edges and are deleted; the rest
/// stay (keeping the epistemic envelope honest). Used after incremental
/// updates so callers into a rebuilt file re-link. The stored `receiver` is
/// replayed so the receiver downgrade stays consistent across re-resolutions.
pub fn resolve_all(store: &mut GraphStore) -> Result<ResolveStats, StoreError> {
    let idx = ResolveIndex::build(store)?;
    struct Row {
        id: i64,
        file_id: i64,
        name: String,
        enclosing: i64,
        site_line: u32,
        receiver: Option<String>,
    }
    let rows: Vec<Row> = {
        let mut stmt = store.conn().prepare(
            "SELECT u.id, u.file_id, u.name, u.enclosing_symbol_id, u.site_line, u.receiver
               FROM unresolved_calls u
               JOIN symbols s ON s.id = u.enclosing_symbol_id
              WHERE u.enclosing_symbol_id IS NOT NULL",
        )?;
        let mapped = stmt.query_map([], |r| {
            Ok(Row {
                id: r.get(0)?,
                file_id: r.get(1)?,
                name: r.get(2)?,
                enclosing: r.get(3)?,
                site_line: r.get(4)?,
                receiver: r.get(5)?,
            })
        })?;
        mapped.collect::<Result<_, _>>()?
    };
    let mut stats = ResolveStats::default();
    for row in &rows {
        let decision = idx.decide(row.file_id, &row.name, row.receiver.as_deref());
        let (dst, tier) = match decision {
            Decision::Exact(d) => (d, Tier::Exact),
            Decision::Probable(d) => (d, Tier::Probable),
            Decision::Unresolved => {
                stats.unresolved += 1;
                continue;
            }
        };
        store.insert_edge(&EdgeRow {
            src_id: row.enclosing,
            dst_id: dst,
            kind: EdgeKind::Calls,
            tier,
            site_line: row.site_line,
            receiver: row.receiver.clone(),
        })?;
        store.conn().execute(
            "DELETE FROM unresolved_calls WHERE id = ?1",
            params![row.id],
        )?;
        match tier {
            Tier::Exact => stats.exact += 1,
            Tier::Probable => stats.probable += 1,
        }
    }
    Ok(stats)
}

/// Reconsider resolved calls whose target names were defined by a changed
/// file. Adding a same-name definition can make a previously unique target
/// ambiguous; unrelated call edges remain untouched.
pub fn reconsider_resolved_calls(
    store: &mut GraphStore,
    changed_names: &HashSet<String>,
) -> Result<(), StoreError> {
    struct ResolvedCall {
        file_id: i64,
        name: String,
        enclosing: i64,
        site_line: u32,
        receiver: Option<String>,
    }
    let mut calls = Vec::new();
    for name in changed_names {
        let found: Vec<ResolvedCall> = {
            let mut stmt = store.conn().prepare(
                "SELECT src.file_id, dst.name, e.src_id, e.site_line, e.receiver
                   FROM edges e
                   JOIN symbols src ON src.id = e.src_id
                   JOIN symbols dst ON dst.id = e.dst_id
                  WHERE e.kind = 'calls' AND dst.name = ?1",
            )?;
            let rows = stmt.query_map(params![name], |row| {
                Ok(ResolvedCall {
                    file_id: row.get(0)?,
                    name: row.get(1)?,
                    enclosing: row.get(2)?,
                    site_line: row.get(3)?,
                    receiver: row.get(4)?,
                })
            })?;
            rows.collect::<Result<_, _>>()?
        };
        calls.extend(found);
        store.conn().execute(
            "DELETE FROM edges
              WHERE kind = 'calls'
                AND dst_id IN (SELECT id FROM symbols WHERE name = ?1)",
            params![name],
        )?;
    }
    for call in calls {
        store.insert_unresolved_call(
            call.file_id,
            &call.name,
            Some(call.enclosing),
            call.site_line,
            call.receiver.as_deref(),
        )?;
    }
    Ok(())
}

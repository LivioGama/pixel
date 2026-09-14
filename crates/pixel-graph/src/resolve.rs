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
//! A real receiver whose callee name is also defined in the caller's own file
//! is `Unresolved` instead: T0 would link the qualified call
//! (`pixel_graph::build::build_graph` inside `api.rs`) to the caller's own
//! same-name symbol — a shadow, not the callee. The unresolved row keeps the
//! envelope honest (`lower_bound`, `unresolved_same_name`) instead of an edge
//! to the wrong definition.
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

/// One extracted callback/reference site awaiting resolution (symbol ids
/// already assigned). A symbol passed as an argument to a call (e.g.
/// `schema.plugin(tenantScopePlugin)`). Resolves to a `References` edge,
/// which is weaker than `Calls` — it means "may be invoked", not "directly
/// called". All resolved references use `Tier::Probable`.
#[derive(Debug, Clone)]
pub struct PendingReference {
    pub name: String,
    pub enclosing_symbol_id: Option<i64>,
    pub site_line: u32,
    /// The callee that received this argument, when known.
    pub arg_of: Option<String>,
}

/// All pending references of one file.
#[derive(Debug, Clone)]
pub struct FileReferences {
    pub file_id: i64,
    pub references: Vec<PendingReference>,
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

    /// True iff some symbol in the graph is named `name`.
    pub fn defines(&self, name: &str) -> bool {
        self.by_name.contains_key(name)
    }

    /// The tier decision for one call from `caller_file_id` to `name`.
    /// `receiver` is the receiver expression text (if any) of the call site;
    /// a real receiver (not `self`/`Self`/`this`) caps the result at
    /// `Probable` because the receiver's type is unknown to the resolver.
    /// A real receiver whose name is also defined in the caller's own file is
    /// `Unresolved`: T0 would otherwise point the qualified call at the
    /// caller's same-name symbol (the shadow) instead of the named module's.
    pub fn decide(&self, caller_file_id: i64, name: &str, receiver: Option<&str>) -> Decision {
        if has_real_receiver(receiver) && self.defines_in_file(caller_file_id, name) {
            return Decision::Unresolved;
        }
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

    /// True iff `name` has a callable definition in `file_id` — the T0 case
    /// `decide` must not use for a call with a real receiver.
    fn defines_in_file(&self, file_id: i64, name: &str) -> bool {
        self.by_name
            .get(name)
            .is_some_and(|cands| cands.iter().any(|c| c.file_id == file_id))
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
                    "calls",
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
                        "calls",
                    )?;
                    stats.unresolved += 1;
                }
            }
        }
    }
    Ok(stats)
}

/// Resolve the given in-memory pending references (symbols passed as
/// arguments to calls), writing `References` edges / unresolved rows into
/// the store. Used by `build::build_graph` after extraction. Mirrors
/// `resolve_calls` but inserts `EdgeKind::References` edges and always
/// uses `Tier::Probable` (we don't know if the callee actually invokes
/// the arg). A reference whose name no symbol carries is a plain value
/// (`g(x)`), not a callback, and is dropped; only a named function the
/// resolver could not pick goes to `unresolved_calls`, where the epistemic
/// envelope counts it.
pub fn resolve_references(
    store: &GraphStore,
    pending: &[FileReferences],
) -> Result<ResolveStats, StoreError> {
    let idx = ResolveIndex::build(store)?;
    let mut stats = ResolveStats::default();
    for fr in pending {
        for r#ref in &fr.references {
            if !idx.defines(&r#ref.name) {
                continue;
            }
            let Some(src_id) = r#ref.enclosing_symbol_id else {
                // Top-level reference site: no source symbol to hang an edge on.
                store.insert_unresolved_call(
                    fr.file_id,
                    &r#ref.name,
                    None,
                    r#ref.site_line,
                    r#ref.arg_of.as_deref(),
                    "references",
                )?;
                stats.unresolved += 1;
                continue;
            };
            // References resolve against the same T0/T1/T2 index. A real
            // receiver is irrelevant here (the arg is an identifier, not a
            // method call), so pass `None`.
            match idx.decide(fr.file_id, &r#ref.name, None) {
                Decision::Exact(dst) | Decision::Probable(dst) => {
                    store.insert_edge(&EdgeRow {
                        src_id,
                        dst_id: dst,
                        kind: EdgeKind::References,
                        tier: Tier::Probable,
                        site_line: r#ref.site_line,
                        receiver: r#ref.arg_of.clone(),
                    })?;
                    stats.probable += 1;
                }
                Decision::Unresolved => {
                    store.insert_unresolved_call(
                        fr.file_id,
                        &r#ref.name,
                        Some(src_id),
                        r#ref.site_line,
                        r#ref.arg_of.as_deref(),
                        "references",
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
    struct Row {
        id: i64,
        file_id: i64,
        name: String,
        enclosing: i64,
        site_line: u32,
        receiver: Option<String>,
        kind: String,
    }

    let idx = ResolveIndex::build(store)?;
    let rows: Vec<Row> = {
        let mut stmt = store.conn().prepare(
            "SELECT u.id, u.file_id, u.name, u.enclosing_symbol_id, u.site_line, u.receiver, u.kind
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
                kind: r
                    .get::<_, Option<String>>(6)?
                    .unwrap_or_else(|| "calls".to_string()),
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
        let edge_kind = if row.kind == "references" {
            EdgeKind::References
        } else {
            EdgeKind::Calls
        };
        // References are always Probable — we don't know if the callee
        // actually invokes the passed arg.
        let tier = if edge_kind == EdgeKind::References {
            Tier::Probable
        } else {
            tier
        };
        store.insert_edge(&EdgeRow {
            src_id: row.enclosing,
            dst_id: dst,
            kind: edge_kind,
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
/// Both `Calls` and `References` edges are reconsidered — a reference to a
/// previously-unique `handler` is just as stale when a second definition
/// appears.
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
        kind: String,
    }
    let mut calls = Vec::new();
    for name in changed_names {
        let found: Vec<ResolvedCall> = {
            let mut stmt = store.conn().prepare(
                "SELECT src.file_id, dst.name, e.src_id, e.site_line, e.receiver, e.kind
                   FROM edges e
                   JOIN symbols src ON src.id = e.src_id
                   JOIN symbols dst ON dst.id = e.dst_id
                  WHERE e.kind IN ('calls', 'references') AND dst.name = ?1",
            )?;
            let rows = stmt.query_map(params![name], |row| {
                Ok(ResolvedCall {
                    file_id: row.get(0)?,
                    name: row.get(1)?,
                    enclosing: row.get(2)?,
                    site_line: row.get(3)?,
                    receiver: row.get(4)?,
                    kind: row.get(5)?,
                })
            })?;
            rows.collect::<Result<_, _>>()?
        };
        calls.extend(found);
        store.conn().execute(
            "DELETE FROM edges
              WHERE kind IN ('calls', 'references')
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
            &call.kind,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::GraphStore;

    /// Two Rust files that both define `f`: the caller's own `src/local.rs`
    /// and the `src/remote.rs` a qualified `other_crate::f()` names. `g` is
    /// defined in `src/remote.rs` only.
    fn fixture() -> (GraphStore, i64, i64, i64, i64) {
        let mut store = GraphStore::open_in_memory().unwrap();
        let local_file = store
            .replace_file("src/local.rs", "oid-local", "rust")
            .unwrap();
        let remote_file = store
            .replace_file("src/remote.rs", "oid-remote", "rust")
            .unwrap();
        let local_f = insert(&store, local_file, "src/local.rs", "f");
        let remote_f = insert(&store, remote_file, "src/remote.rs", "f");
        let remote_g = insert(&store, remote_file, "src/remote.rs", "g");
        (store, local_file, local_f, remote_f, remote_g)
    }

    fn insert(store: &GraphStore, file_id: i64, path: &str, name: &str) -> i64 {
        store
            .insert_symbol(
                file_id,
                &format!("{path}#{name}#function"),
                name,
                name,
                SymbolKind::Function,
                1,
                3,
                "",
            )
            .unwrap()
    }

    #[test]
    fn qualified_call_with_a_locally_defined_name_is_unresolved() {
        let (store, caller_file, _local_f, _remote_f, _remote_g) = fixture();
        let idx = ResolveIndex::build(&store).unwrap();
        // `other_crate::f()` inside `src/local.rs` names another module; T0
        // must not link it to the caller's own `f` (the shadow).
        assert_eq!(
            idx.decide(caller_file, "f", Some("other_crate")),
            Decision::Unresolved
        );
    }

    #[test]
    fn unqualified_and_self_receiver_calls_keep_the_t0_definition() {
        let (store, caller_file, local_f, _remote_f, _remote_g) = fixture();
        let idx = ResolveIndex::build(&store).unwrap();
        // No receiver: the plain `f()` is T0 Exact, unchanged.
        assert_eq!(idx.decide(caller_file, "f", None), Decision::Exact(local_f));
        // `self::f()` / `Self::f()` / `this.f()` keep their exemption.
        for receiver in ["self", "Self", "this"] {
            assert_eq!(
                idx.decide(caller_file, "f", Some(receiver)),
                Decision::Exact(local_f),
                "receiver {receiver:?} keeps the T0 definition"
            );
        }
    }

    #[test]
    fn real_receiver_with_a_name_defined_elsewhere_stays_probable() {
        let (store, caller_file, _local_f, _remote_f, remote_g) = fixture();
        let idx = ResolveIndex::build(&store).unwrap();
        // `x.g()`: `g` has no local definition to shadow, so the unique
        // repo-wide definition stays the (receiver-capped) Probable answer.
        assert_eq!(
            idx.decide(caller_file, "g", Some("x")),
            Decision::Probable(remote_g)
        );
    }
}

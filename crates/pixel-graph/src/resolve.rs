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
//! Two receiver-shaped tiebreaks extend the receiver rules, both capped at
//! `Probable`. A receiver path whose last segment names the type of exactly
//! one candidate (`pixel_git::GitRunner::new` ↔ `GitRunner::new`) links to
//! it where the name tiers would otherwise stay unresolved or shadow-vetoed;
//! the receiver names the implementing type, so a trait-impl candidate counts
//! here (`Options::default()`). And when the graph holds exactly one callable
//! definition of the name, it sits in the caller's own file as an inherent
//! method, and the receiver is a value/path (`w.push_call()`, `idx.decide()`),
//! that sole candidate is returned — no other definition exists to shadow it,
//! and a value receiver never names a trait implementor.
//!
//! A real receiver whose callee name is also defined in the caller's own file
//! is otherwise `Unresolved`: T0 would link the call
//! (`pixel_graph::build::build_graph` inside `api.rs`) to the caller's own
//! same-name symbol — a shadow, not the callee. The unresolved row keeps the
//! envelope honest (`lower_bound`, `unresolved_same_name`) instead of an edge
//! to the wrong definition.
//!
//! T1 matches on the names an import binds (`imports.bindings`), not on the
//! file it resolves to: a wildcard or file-level import proves no binding.
//! Under an alias the call site writes the importer's local name while the
//! candidate carries the source name (`use a::push as leased;` →
//! `leased()` calls `push`), so T1 looks the local name up and matches
//! candidates on the source; the source name alone is not in scope there.

use std::collections::{HashMap, HashSet};

use rusqlite::params;

use crate::store::{EdgeKind, EdgeRow, GraphStore, StoreError, SymbolKind, Tier, decode_bindings};

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
    /// Method declared in a trait impl, so an implementor (or a std type the
    /// graph never sees) may be the real callee. Excluded from the receiver
    /// relaxation below.
    trait_impl: bool,
}

/// Symbol-name index + import graph snapshot used for tier decisions.
pub struct ResolveIndex {
    by_name: HashMap<String, Vec<Candidate>>,
    ruby_files: HashSet<i64>,
    /// symbol_id → qualified name, for the type-qualified receiver tiebreak
    /// (`pixel_git::GitRunner` + `new` ↔ `GitRunner::new`). Kept beside the
    /// `Copy` candidate rows so the tier code stays copy-based.
    qualified_of: HashMap<i64, String>,
    /// (file_id, local name) → the (imported file_id, source name) pairs an
    /// import binds under that name. T1 requires the callee to be one of
    /// them: a definition named `source` in that file, not just any
    /// definition in an imported file.
    import_bindings: HashMap<(i64, String), Vec<(i64, String)>>,
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
        let ruby_files = {
            let mut stmt = conn.prepare("SELECT id FROM files WHERE lang = 'ruby'")?;
            let rows = stmt.query_map([], |row| row.get::<_, i64>(0))?;
            rows.collect::<Result<HashSet<_>, _>>()?
        };
        let mut by_name: HashMap<String, Vec<Candidate>> = HashMap::new();
        let mut qualified_of: HashMap<i64, String> = HashMap::new();
        {
            let mut stmt = conn.prepare(
                "SELECT name, file_id, id, kind, start_line, trait_impl, qualified FROM symbols",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    Candidate {
                        file_id: r.get(1)?,
                        symbol_id: r.get(2)?,
                        kind: SymbolKind::parse(&r.get::<_, String>(3)?),
                        start_line: r.get(4)?,
                        trait_impl: r.get(5)?,
                    },
                    r.get::<_, String>(6)?,
                ))
            })?;
            for row in rows {
                let (name, cand, qualified) = row?;
                qualified_of.insert(cand.symbol_id, qualified);
                if callable(cand.kind) {
                    by_name.entry(name).or_default().push(cand);
                }
            }
        }
        let mut import_bindings: HashMap<(i64, String), Vec<(i64, String)>> = HashMap::new();
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
                    // An empty column means wildcard or unknown and grants
                    // no T1 Exact confidence.
                    for b in decode_bindings(&bindings_csv) {
                        import_bindings
                            .entry((fid, b.local))
                            .or_default()
                            .push((dst, b.source));
                    }
                }
            }
        }
        Ok(Self {
            by_name,
            ruby_files,
            qualified_of,
            import_bindings,
        })
    }

    /// True iff some symbol in the graph is named `name`.
    pub fn defines(&self, name: &str) -> bool {
        self.by_name.contains_key(name)
    }

    /// True iff `name` can name a symbol from `file_id`: a symbol carries
    /// it, or an import of that file binds it (an alias names no symbol).
    fn names_a_symbol(&self, file_id: i64, name: &str) -> bool {
        self.defines(name)
            || self
                .import_bindings
                .contains_key(&(file_id, name.to_string()))
    }

    /// The tier decision for one call from `caller_file_id` to `name`.
    /// `receiver` is the receiver expression text (if any) of the call site;
    /// a real receiver (not `self`/`Self`/`this`) caps the result at
    /// `Probable` because the receiver's type is unknown to the resolver.
    /// A real receiver whose name is also defined in the caller's own file is
    /// `Unresolved`: T0 would otherwise point the call at the caller's
    /// same-name symbol (the shadow) instead of the receiver's own callee.
    ///
    /// Two receiver-shaped exceptions fire before that veto:
    ///
    /// - `receiver-type-match`: the receiver is a value/path whose last
    ///   segment is the type of exactly one same-name candidate
    ///   (`pixel_git::GitRunner::new` → `GitRunner::new`). That candidate is
    ///   returned as `Probable` — the type identity is still a guess, but a
    ///   better-evidenced one than the caller's own same-name symbol. A
    ///   trait-impl candidate counts here: the receiver names the implementing
    ///   type, so `Options::default()` can only call `Options`'s `Default`
    ///   impl. Outside the shadow case it only fires where the name tiers
    ///   refused (`Unresolved`), so an import-resolved `Exact` target is never
    ///   second-guessed.
    /// - `sole-local-method`: when the graph holds exactly one callable
    ///   definition of `name`, it sits in the caller's own file, it is an
    ///   inherent method, and the receiver text is a value/path rather than a
    ///   chained expression, there is no competing definition T0 could shadow
    ///   and no trait implementor the graph cannot see. The call gets that
    ///   sole candidate as `Probable` — never `Exact`.
    ///
    /// A same-file free function (`path.exists()`), a trait-impl method on a
    /// value receiver (`x.clone()` next to `Box::clone`), a chained receiver
    /// (`words.iter().count()`), two same-name methods in one file
    /// (`A::walk` beside `B::walk`), and any name with a definition in
    /// another file keep the shadow veto.
    pub fn decide(&self, caller_file_id: i64, name: &str, receiver: Option<&str>) -> Decision {
        self.decide_from(caller_file_id, None, name, receiver)
    }

    fn decide_from(
        &self,
        caller_file_id: i64,
        caller_symbol_id: Option<i64>,
        name: &str,
        receiver: Option<&str>,
    ) -> Decision {
        if self.ruby_files.contains(&caller_file_id)
            && self.ambiguous_local_name(caller_file_id, name)
        {
            match receiver.map(str::trim) {
                None => return Decision::Unresolved,
                Some("self") => {
                    return caller_symbol_id
                        .and_then(|id| self.ruby_self_target(caller_file_id, id, name))
                        .map_or(Decision::Unresolved, Decision::Exact);
                }
                Some(_) => {}
            }
        }
        if has_real_receiver(receiver) && self.defines_in_file(caller_file_id, name) {
            if let Some(r) = receiver
                && let Some(id) = self.qualified_match(r, name)
            {
                return Decision::Probable(id);
            }
            if let Some(r) = receiver
                && is_value_receiver(r)
                && let Some(id) = self.sole_inherent_method(name)
            {
                return Decision::Probable(id);
            }
            return Decision::Unresolved;
        }
        let raw = self.decide_raw(caller_file_id, name);
        if has_real_receiver(receiver) {
            if let Decision::Exact(id) = raw {
                // Downgrade: a non-self receiver means we cannot confirm the
                // callee is the same definition the receiver's type resolves
                // to.
                return Decision::Probable(id);
            }
            // The name tiers refused (several files define the name); the
            // receiver still names a type the graph knows, so that candidate
            // is better evidence than nothing.
            if matches!(raw, Decision::Unresolved)
                && let Some(r) = receiver
                && let Some(id) = self.qualified_match(r, name)
            {
                return Decision::Probable(id);
            }
        }
        raw
    }

    fn ambiguous_local_name(&self, caller_file_id: i64, name: &str) -> bool {
        let Some(candidates) = self.by_name.get(name) else {
            return false;
        };
        let local_count = candidates
            .iter()
            .filter(|candidate| candidate.file_id == caller_file_id)
            .count();
        local_count > 1
            || (local_count == 1
                && candidates
                    .iter()
                    .any(|candidate| candidate.file_id != caller_file_id))
    }

    fn ruby_self_target(
        &self,
        caller_file_id: i64,
        caller_symbol_id: i64,
        name: &str,
    ) -> Option<i64> {
        let caller_owner = ruby_owner(self.qualified_of.get(&caller_symbol_id)?)?;
        let matches: Vec<Candidate> = self
            .by_name
            .get(name)?
            .iter()
            .copied()
            .filter(|candidate| candidate.file_id == caller_file_id)
            .filter(|candidate| {
                self.qualified_of
                    .get(&candidate.symbol_id)
                    .and_then(|qualified| ruby_owner(qualified))
                    == Some(caller_owner)
            })
            .collect();
        best(&matches)
    }

    /// The sole callable candidate of `name` whose qualified name starts with
    /// the receiver path's last segment (`pixel_git::GitRunner` + `new` →
    /// `GitRunner::new`). The receiver names the implementing type, so a
    /// trait-impl candidate matches too: `Options::default()` can only call
    /// `Options`'s `Default` impl, unlike `opts.default()`, which
    /// `sole_inherent_method_in` refuses. More than one matching candidate —
    /// the same type name in two files, or an inherent method beside a
    /// trait-impl one — is ambiguous and returns `None`, as does a receiver
    /// that is not a plain value/path (`get_store().open()`).
    fn qualified_match(&self, receiver: &str, name: &str) -> Option<i64> {
        if !is_value_receiver(receiver) {
            return None;
        }
        let segment = receiver.trim().rsplit("::").next()?;
        let prefix = format!("{segment}::");
        let mut hit: Option<i64> = None;
        for cand in self.by_name.get(name)? {
            let Some(qualified) = self.qualified_of.get(&cand.symbol_id) else {
                continue;
            };
            if qualified.starts_with(&prefix) {
                if hit.is_some() {
                    return None;
                }
                hit = Some(cand.symbol_id);
            }
        }
        hit
    }

    /// The symbol id of the graph's sole callable definition of `name`, when
    /// it is an inherent method. `None` when the name has no definition, or
    /// when the one definition is a trait-impl method or a free function.
    /// `decide` only calls this under the shadow veto — the name is already
    /// known to be defined in the caller's own file — so the sole definition
    /// is that file's. Two definitions (`A::walk` beside `B::walk`, a
    /// competing file) never reach the single-candidate slice.
    fn sole_inherent_method(&self, name: &str) -> Option<i64> {
        let [candidate] = self.by_name.get(name)?.as_slice() else {
            return None;
        };
        (!candidate.trait_impl && candidate.kind == SymbolKind::Method)
            .then_some(candidate.symbol_id)
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
        let cands = self.by_name.get(name).map_or(&[][..], Vec::as_slice);
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
        if let Some(decision) = self.import_tier(caller_file_id, name) {
            return decision;
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

    /// T1: the definitions the imports binding `name` in `caller_file_id`
    /// point at — each a symbol named after the binding's source in the file
    /// the import resolved to. `None` when no import binds `name` or none of
    /// them lands on a definition (T2 decides); `Unresolved` when they land
    /// in several files, since a name never fans out.
    fn import_tier(&self, caller_file_id: i64, name: &str) -> Option<Decision> {
        let targets = self
            .import_bindings
            .get(&(caller_file_id, name.to_string()))?;
        let mut hits: Vec<Candidate> = Vec::new();
        for (file_id, source) in targets {
            if let Some(cands) = self.by_name.get(source) {
                hits.extend(cands.iter().copied().filter(|c| c.file_id == *file_id));
            }
        }
        let files: HashSet<i64> = hits.iter().map(|c| c.file_id).collect();
        match files.len() {
            0 => None,
            1 => best(&hits).map(Decision::Exact),
            _ => Some(Decision::Unresolved),
        }
    }
}

/// True iff `receiver` is a real receiver expression (not absent and not one
/// of the self-pseudo-receivers). `self`/`Self`/`this`/`crate`/`super` resolve
/// against the enclosing type/module, so they keep the normal tier.
fn ruby_owner(qualified: &str) -> Option<(&str, char)> {
    let separator = qualified.rfind(['#', '.'])?;
    Some((
        &qualified[..separator],
        qualified[separator..].chars().next()?,
    ))
}

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

/// True iff `receiver` is a plain identifier (`w`, `idx`, `Walker`) or a
/// `::`-separated path (`crate::store`). A chained expression
/// (`words.iter().filter(..)`) names a value produced elsewhere, so the
/// sole-local-method relaxation in `decide` must not treat it as that file's
/// method call.
fn is_value_receiver(receiver: &str) -> bool {
    let r = receiver.trim();
    !r.is_empty() && r.split("::").all(is_plain_ident)
}

fn is_plain_ident(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && text.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
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
            match idx.decide_from(
                fc.file_id,
                Some(src_id),
                &call.callee_name,
                call.receiver.as_deref(),
            ) {
                Decision::Exact(dst) => {
                    store.insert_resolved_edge(
                        &EdgeRow {
                            src_id,
                            dst_id: dst,
                            kind: EdgeKind::Calls,
                            tier: Tier::Exact,
                            site_line: call.site_line,
                            receiver: call.receiver.clone(),
                        },
                        &call.callee_name,
                    )?;
                    stats.exact += 1;
                }
                Decision::Probable(dst) => {
                    store.insert_resolved_edge(
                        &EdgeRow {
                            src_id,
                            dst_id: dst,
                            kind: EdgeKind::Calls,
                            tier: Tier::Probable,
                            site_line: call.site_line,
                            receiver: call.receiver.clone(),
                        },
                        &call.callee_name,
                    )?;
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
            if !idx.names_a_symbol(fr.file_id, &r#ref.name) {
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
                    store.insert_resolved_edge(
                        &EdgeRow {
                            src_id,
                            dst_id: dst,
                            kind: EdgeKind::References,
                            tier: Tier::Probable,
                            site_line: r#ref.site_line,
                            receiver: r#ref.arg_of.clone(),
                        },
                        &r#ref.name,
                    )?;
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
        let decision = idx.decide_from(
            row.file_id,
            Some(row.enclosing),
            &row.name,
            row.receiver.as_deref(),
        );
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
        store.insert_resolved_edge(
            &EdgeRow {
                src_id: row.enclosing,
                dst_id: dst,
                kind: edge_kind,
                tier,
                site_line: row.site_line,
                receiver: row.receiver.clone(),
            },
            &row.name,
        )?;
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
                "SELECT src.file_id, COALESCE(e.callee, dst.name), e.src_id, e.site_line, e.receiver, e.kind
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
    use crate::extract::ImportBinding;
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
    fn ruby_unqualified_t0_is_unresolved_when_other_files_define_the_name() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let local = store
            .replace_file("lib/local.rb", "oid-local", "ruby")
            .unwrap();
        let remote = store
            .replace_file("lib/remote.rb", "oid-remote", "ruby")
            .unwrap();
        insert(&store, local, "lib/local.rb", "application");
        insert(&store, remote, "lib/remote.rb", "application");
        let idx = ResolveIndex::build(&store).unwrap();
        assert_eq!(idx.decide(local, "application", None), Decision::Unresolved);
    }

    #[test]
    fn ruby_self_call_resolves_only_with_matching_local_owner() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let local = store
            .replace_file("lib/local.rb", "oid-local", "ruby")
            .unwrap();
        let remote = store
            .replace_file("lib/remote.rb", "oid-remote", "ruby")
            .unwrap();
        let caller = store
            .insert_symbol(
                local,
                "local#App#run#method",
                "run",
                "App#run",
                SymbolKind::Method,
                1,
                3,
                "run",
            )
            .unwrap();
        let unrelated_caller = store
            .insert_symbol(
                local,
                "local#Admin#run#method",
                "run",
                "Admin#run",
                SymbolKind::Method,
                5,
                7,
                "run",
            )
            .unwrap();
        let local_target = store
            .insert_symbol(
                local,
                "local#App#application#method",
                "application",
                "App#application",
                SymbolKind::Method,
                9,
                11,
                "application",
            )
            .unwrap();
        store
            .insert_symbol(
                remote,
                "remote#Other#application#method",
                "application",
                "Other#application",
                SymbolKind::Method,
                1,
                3,
                "application",
            )
            .unwrap();
        let idx = ResolveIndex::build(&store).unwrap();
        assert_eq!(
            idx.decide_from(local, Some(caller), "application", Some("self")),
            Decision::Exact(local_target)
        );
        assert_eq!(
            idx.decide_from(local, Some(unrelated_caller), "application", Some("self")),
            Decision::Unresolved
        );
    }

    #[test]
    fn ruby_same_file_duplicate_owners_are_ambiguous_without_remote_candidates() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let file = store.replace_file("lib/local.rb", "oid", "ruby").unwrap();
        let caller = store
            .insert_symbol(
                file,
                "local#App#run#method",
                "run",
                "App#run",
                SymbolKind::Method,
                1,
                3,
                "run",
            )
            .unwrap();
        let app_target = store
            .insert_symbol(
                file,
                "local#App#application#method",
                "application",
                "App#application",
                SymbolKind::Method,
                5,
                7,
                "application",
            )
            .unwrap();
        store
            .insert_symbol(
                file,
                "local#Admin#application#method",
                "application",
                "Admin#application",
                SymbolKind::Method,
                9,
                11,
                "application",
            )
            .unwrap();

        let idx = ResolveIndex::build(&store).unwrap();
        assert_eq!(
            idx.decide_from(file, Some(caller), "application", Some("self")),
            Decision::Exact(app_target)
        );
        assert_eq!(idx.decide(file, "application", None), Decision::Unresolved);
    }

    #[test]
    fn ruby_unique_unqualified_t0_stays_exact() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let file = store.replace_file("lib/local.rb", "oid", "ruby").unwrap();
        let local = insert(&store, file, "lib/local.rb", "helper");
        let idx = ResolveIndex::build(&store).unwrap();
        assert_eq!(idx.decide(file, "helper", None), Decision::Exact(local));
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

    /// Single-file store for the sole-local-method relaxation: `src/local.rs`
    /// is the only file, so every name defined there has exactly one file.
    fn local_only() -> (GraphStore, i64) {
        let mut store = GraphStore::open_in_memory().unwrap();
        let file = store.replace_file("src/local.rs", "oid", "rust").unwrap();
        (store, file)
    }

    /// Insert a symbol with an explicit kind and trait-impl flag; `line`
    /// disambiguates duplicate names in one file (the uid is per line).
    fn insert_at(
        store: &GraphStore,
        file_id: i64,
        name: &str,
        kind: SymbolKind,
        line: u32,
        trait_impl: bool,
    ) -> i64 {
        let id = store
            .insert_symbol(
                file_id,
                &format!("f{file_id}#{name}#{line}"),
                name,
                name,
                kind,
                line,
                line + 1,
                "",
            )
            .unwrap();
        if trait_impl {
            store.mark_trait_impl(id).unwrap();
        }
        id
    }

    #[test]
    fn sole_inherent_method_resolves_a_value_receiver_at_probable_never_exact() {
        let (store, file) = local_only();
        let method = insert_at(&store, file, "push_call", SymbolKind::Method, 2, false);
        let idx = ResolveIndex::build(&store).unwrap();
        // `w.push_call()`: the only definition of the name is this file's
        // method, so the shadow veto relaxes to the sole candidate — capped
        // at Probable, since the receiver's type is still unknown.
        assert_eq!(
            idx.decide(file, "push_call", Some("w")),
            Decision::Probable(method)
        );
        assert_eq!(
            idx.decide(file, "push_call", Some("crate::extract")),
            Decision::Probable(method)
        );
    }

    #[test]
    fn two_same_named_methods_in_one_file_keep_the_shadow_veto() {
        let (store, file) = local_only();
        // `A::walk` and `B::walk` are two candidates for an unknown receiver:
        // either could be the callee, so neither is picked.
        let late = insert_at(&store, file, "walk", SymbolKind::Method, 9, false);
        let early = insert_at(&store, file, "walk", SymbolKind::Method, 4, false);
        assert_ne!(early, late);
        let idx = ResolveIndex::build(&store).unwrap();
        assert_eq!(idx.decide(file, "walk", Some("w")), Decision::Unresolved);
    }

    #[test]
    fn a_free_function_with_a_receiver_keeps_the_shadow_veto() {
        let (store, file) = local_only();
        insert_at(&store, file, "exists", SymbolKind::Function, 2, false);
        let idx = ResolveIndex::build(&store).unwrap();
        // `path.exists()` is `Path::exists`, not this file's free `fn
        // exists`: a function has no receiver, so the call cannot target it.
        assert_eq!(
            idx.decide(file, "exists", Some("path")),
            Decision::Unresolved
        );
    }

    #[test]
    fn a_trait_impl_method_keeps_the_shadow_veto() {
        let (store, file) = local_only();
        insert_at(&store, file, "clone", SymbolKind::Method, 2, true);
        let idx = ResolveIndex::build(&store).unwrap();
        // `path.clone()` is `Clone::clone` for a std type the graph never
        // sees; the file's `Box::clone` is not evidence the call targets it.
        assert_eq!(
            idx.decide(file, "clone", Some("path")),
            Decision::Unresolved
        );
    }

    #[test]
    fn a_trait_impl_among_several_local_candidates_keeps_the_shadow_veto() {
        let (store, file) = local_only();
        insert_at(&store, file, "dims", SymbolKind::Method, 2, false);
        insert_at(&store, file, "dims", SymbolKind::Method, 8, true);
        let idx = ResolveIndex::build(&store).unwrap();
        // The inherent `dims` cannot be told apart from the trait impl's
        // `dims` on an unknown receiver, so the call stays unresolved.
        assert_eq!(
            idx.decide(file, "dims", Some("embedder")),
            Decision::Unresolved
        );
    }

    #[test]
    fn a_chained_receiver_keeps_the_shadow_veto() {
        let (store, file) = local_only();
        insert_at(&store, file, "count", SymbolKind::Method, 2, false);
        let idx = ResolveIndex::build(&store).unwrap();
        // `words.iter().filter(..).count()` is `Iterator::count`, not the
        // file's own method: the receiver is an expression, not a value.
        assert_eq!(
            idx.decide(file, "count", Some("words.iter().filter(..)")),
            Decision::Unresolved
        );
    }

    #[test]
    fn a_second_file_definition_keeps_the_shadow_veto() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let local = store.replace_file("src/local.rs", "oid", "rust").unwrap();
        let remote = store.replace_file("src/remote.rs", "oid", "rust").unwrap();
        insert_at(&store, local, "run", SymbolKind::Method, 2, false);
        insert_at(&store, remote, "run", SymbolKind::Method, 2, false);
        let idx = ResolveIndex::build(&store).unwrap();
        // `x.run()` with a competing definition elsewhere is ambiguous, so
        // the shadow veto stays (the cited `graph::build::f()` regression).
        assert_eq!(idx.decide(local, "run", Some("x")), Decision::Unresolved);
    }

    #[test]
    fn value_receiver_shapes() {
        assert!(is_value_receiver("w"));
        assert!(is_value_receiver("a1"));
        assert!(is_value_receiver("_private"));
        assert!(is_value_receiver("Store"));
        assert!(is_value_receiver("crate::store"));
        assert!(is_value_receiver("  idx  "));
        assert!(!is_value_receiver("m.path"));
        assert!(!is_value_receiver("words.iter().filter(..)"));
        assert!(!is_value_receiver("response[\"text\"]"));
        assert!(!is_value_receiver(""));
        assert!(!is_value_receiver("1bad"));
    }

    /// Two files, each with its own `open`: `src/local.rs` has `Other::open`
    /// (the caller's file) and `src/remote.rs` has `Store::open`.
    fn two_opens() -> (GraphStore, i64, i64, i64, i64) {
        let mut store = GraphStore::open_in_memory().unwrap();
        let local = store.replace_file("src/local.rs", "oid", "rust").unwrap();
        let remote = store.replace_file("src/remote.rs", "oid", "rust").unwrap();
        let other_open = insert_named(&store, local, "Other", "open");
        let store_open = insert_named(&store, remote, "Store", "open");
        (store, local, remote, other_open, store_open)
    }

    /// Insert an inherent method `Type::name` whose qualified name carries the
    /// type, mirroring what the extractor writes for an `impl` block.
    fn insert_named(store: &GraphStore, file_id: i64, ty: &str, name: &str) -> i64 {
        store
            .insert_symbol(
                file_id,
                &format!("f{file_id}#{ty}::{name}#method"),
                name,
                &format!("{ty}::{name}"),
                SymbolKind::Method,
                1,
                3,
                "",
            )
            .unwrap()
    }

    /// `insert_named` for a method declared in a trait impl.
    fn insert_trait_named(store: &GraphStore, file_id: i64, ty: &str, name: &str) -> i64 {
        let id = insert_named(store, file_id, ty, name);
        store.mark_trait_impl(id).unwrap();
        id
    }

    #[test]
    fn receiver_path_type_matches_the_unique_qualified_candidate() {
        let (store, local, _remote, other_open, store_open) = two_opens();
        let idx = ResolveIndex::build(&store).unwrap();
        // `pixel_git::GitRunner::new`-shaped: the receiver names `Store`, so
        // the call links to `Store::open` even though the caller's own file
        // defines `Other::open` (which the shadow veto would otherwise
        // refuse to resolve at all).
        assert_eq!(
            idx.decide(local, "open", Some("pixel_remote::Store")),
            Decision::Probable(store_open)
        );
        // A single-segment receiver does the same.
        assert_eq!(
            idx.decide(local, "open", Some("Store")),
            Decision::Probable(store_open)
        );
        // Without the type path the call stays shadowed to the local
        // `Other::open`, never guessed at the other one.
        assert_eq!(idx.decide(local, "open", Some("o")), Decision::Unresolved);
        assert_ne!(other_open, store_open);
    }

    #[test]
    fn a_type_qualified_trait_method_links_to_the_implementor() {
        let (store, file) = local_only();
        let default = insert_trait_named(&store, file, "Options", "default");
        let idx = ResolveIndex::build(&store).unwrap();
        // `Options::default()` names the implementing type, so the trait impl
        // is the only possible callee — unlike `opts.default()`, where the
        // receiver's type (and therefore the implementor) is unknown.
        assert_eq!(
            idx.decide(file, "default", Some("Options")),
            Decision::Probable(default)
        );
        assert_eq!(
            idx.decide(file, "default", Some("opts")),
            Decision::Unresolved
        );
    }

    #[test]
    fn two_same_named_types_make_the_path_match_ambiguous() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let a = store.replace_file("src/a.rs", "oid", "rust").unwrap();
        let b = store.replace_file("src/b.rs", "oid", "rust").unwrap();
        insert_named(&store, a, "Store", "open");
        insert_named(&store, b, "Store", "open");
        let idx = ResolveIndex::build(&store).unwrap();
        // `x::Store::open` with two `Store::open` candidates is ambiguous:
        // no edge, no fan-out.
        assert_eq!(
            idx.decide(a, "open", Some("x::Store")),
            Decision::Unresolved
        );
    }

    #[test]
    fn path_prefix_must_be_the_whole_type_segment() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let a = store.replace_file("src/a.rs", "oid", "rust").unwrap();
        let b = store.replace_file("src/b.rs", "oid", "rust").unwrap();
        insert_named(&store, a, "WalkerHelper", "walk");
        insert_named(&store, b, "Other", "walk");
        let idx = ResolveIndex::build(&store).unwrap();
        // `Walker` is a prefix of `WalkerHelper` but not the type: no match,
        // and the two-file ambiguity keeps the call unresolved.
        assert_eq!(idx.decide(a, "walk", Some("Walker")), Decision::Unresolved);
    }

    #[test]
    fn a_chained_receiver_never_path_matches() {
        let (store, local, _remote, _other_open, _store_open) = two_opens();
        let idx = ResolveIndex::build(&store).unwrap();
        // `get_store().open()` is not a receiver path even though the text
        // ends in `::Store`: the expression is a call, not a name.
        assert_eq!(idx.qualified_match("get_store()::Store", "open"), None);
        // The same call therefore falls through to the shadow veto (the
        // caller's file defines `Other::open`).
        assert_eq!(
            idx.decide(local, "open", Some("get_store()::Store")),
            Decision::Unresolved
        );
    }

    #[test]
    fn an_import_resolved_exact_target_is_not_second_guessed_by_a_path() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let local = store.replace_file("src/local.rs", "oid", "rust").unwrap();
        let a = store.replace_file("src/a.rs", "oid", "rust").unwrap();
        let b = store.replace_file("src/b.rs", "oid", "rust").unwrap();
        let a_open = insert_named(&store, a, "A", "open");
        insert_named(&store, b, "B", "open");
        // `local.rs` imports the binding `open` from `a.rs`; T1 resolves
        // `open()` to `A::open` as Exact. A receiver naming `B` must not
        // swap that for a Probable guess at `B::open`.
        store
            .insert_import(
                local,
                "crate::a::open",
                Some(a),
                &[crate::extract::ImportBinding::named("open")],
            )
            .unwrap();
        let idx = ResolveIndex::build(&store).unwrap();
        assert_eq!(
            idx.decide(local, "open", Some("B")),
            Decision::Probable(a_open),
            "Exact(A::open) downgraded to Probable, never swapped for B::open"
        );
    }

    /// T1 under an alias: the call writes the local name, the candidate
    /// carries the source name. Two imports binding one name to definitions
    /// in two files never fan out; an import whose file holds no definition
    /// of the source leaves the decision to T2.
    #[test]
    fn import_tier_matches_the_local_name_against_the_source_definition() {
        let mut store = GraphStore::open_in_memory().unwrap();
        let local = store.replace_file("src/local.rs", "oid", "rust").unwrap();
        let a = store.replace_file("src/a.rs", "oid", "rust").unwrap();
        let b = store.replace_file("src/b.rs", "oid", "rust").unwrap();
        let c = store.replace_file("src/c.rs", "oid", "rust").unwrap();
        let a_push = insert(&store, a, "src/a.rs", "push");
        insert(&store, b, "src/b.rs", "push");
        let c_open = insert(&store, c, "src/c.rs", "open");
        store
            .insert_import(
                local,
                "crate::a::push as leased",
                Some(a),
                &[ImportBinding::aliased("push", "leased")],
            )
            .unwrap();
        store
            .insert_import(
                local,
                "crate::a::push as both",
                Some(a),
                &[ImportBinding::aliased("push", "both")],
            )
            .unwrap();
        store
            .insert_import(
                local,
                "crate::b::push as both",
                Some(b),
                &[ImportBinding::aliased("push", "both")],
            )
            .unwrap();
        // `open` is bound to a.rs, which does not define it: the import
        // proves nothing, and c.rs's sole `open` is T2's Probable.
        store
            .insert_import(
                local,
                "crate::a::open",
                Some(a),
                &[ImportBinding::named("open")],
            )
            .unwrap();
        let idx = ResolveIndex::build(&store).unwrap();
        assert_eq!(idx.decide(local, "leased", None), Decision::Exact(a_push));
        assert_eq!(
            idx.decide(local, "push", None),
            Decision::Unresolved,
            "only aliases are bound; two files define push"
        );
        assert_eq!(idx.decide(local, "both", None), Decision::Unresolved);
        assert_eq!(idx.decide(local, "open", None), Decision::Probable(c_open));
    }
}

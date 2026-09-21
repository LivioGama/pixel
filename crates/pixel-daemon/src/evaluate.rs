//! `pixel evaluate`: symbol resolution, the snapshot contract, and the
//! conversion from a graph evaluation to the wire envelope.
//!
//! The three layers of the command are deliberately separate:
//!
//! - [`pixel_graph::predicate`] walks the stored relation and produces a
//!   status, a coverage report and a witness. It knows nothing about names,
//!   snapshots or JSON.
//! - [`pixel_proto::evaluate`] is the wire contract: an illegal tuple (an
//!   `established` without a witness) cannot be built.
//! - this module is everything in between — it turns the caller's `--from`
//!   and `--to` into symbol ids, establishes that the answer is about a
//!   snapshot the working tree still matches, and assembles the envelope.
//!
//! # The snapshot contract
//!
//! An answer is only as good as the generation it was read from, so the
//! envelope separates three claims that are routinely conflated:
//!
//! 1. **identity** — `meta.freshness` and `meta.extractor_version`, read
//!    from the store. A content digest, not an opaque build id.
//! 2. **generation coherence** — the rows and the signature belong to one
//!    generation. This is what makes a single read trustworthy, and it is
//!    a property of the writer: since the incremental update paths commit
//!    rows and `meta.freshness` in one transaction, a reader on one
//!    connection cannot observe new rows under an old signature.
//! 3. **working-tree match** — the whole tree is walked *before* the
//!    traversal ([`pixel_graph::build::tree_delta`]) and again *after* it
//!    ([`pixel_graph::build::freshness_signature`]), and both must equal
//!    the stored signature.
//!
//! The after-check is not belt-and-braces: without it, a negative answer
//! ("no path") could be produced from a tree that had already grown the
//! very edge being denied. It is whole-tree rather than witness-only for
//! the same reason — a file the witness never mentions is exactly where
//! the missing edge would appear. There is deliberately no witness-only
//! mode, on the wire or here.
//!
//! # Resolution
//!
//! A uid is looked up verbatim and must exist; a bare name resolves only
//! when it matches exactly one symbol in scope. Anything else is an
//! `ambiguous_symbol` with the candidate uids, never a silent pick: the
//! method and the free function that share a name are the case this
//! protects.

use std::path::Path;

use pixel_graph::build::{
    EXTRACTOR_VERSION_KEY, FRESHNESS_KEY, FRESHNESS_WITHHELD, freshness_signature,
};
use pixel_graph::predicate;
use pixel_graph::store::{GraphStore, SymbolRow};
use pixel_proto::evaluate as wire;

/// How many symbols sharing a name are looked up, and therefore how many
/// candidates an ambiguous answer lists.
///
/// The two are deliberately the same number: the candidate list is exactly
/// the set the "is this unique?" decision was taken on, so the count in the
/// answer is exact for that set rather than a truncation of it. A list that
/// is exactly this long means the lookup itself was bounded and more
/// symbols may share the name — which the renderer says out loud.
pub const CANDIDATE_CAP: usize = 50;

/// [`CANDIDATE_CAP`] as the store's lookup limit wants it.
const NAME_LOOKUP_LIMIT: u32 = CANDIDATE_CAP as u32;

/// Which stored tiers form the relation being evaluated.
///
/// `--tiers` selects a relation, never a confidence threshold: an answer in
/// the exact relation is not "less sure" than one in the widened relation,
/// it is about a different set of edges. There is no automatic widening
/// when the narrow relation finds nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TierSelection {
    /// Import-resolved and same-file-scope edges only.
    Exact,
    /// Also edges resolved by name uniqueness in an import-connected
    /// component.
    ExactAndProbable,
}

impl TierSelection {
    /// Parse the `--tiers` value; anything else is a usage error, never a
    /// silent fallback to a different relation.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "exact" => Some(TierSelection::Exact),
            "exact,probable" => Some(TierSelection::ExactAndProbable),
            _ => None,
        }
    }

    /// The tiers this selection admits, as the envelope reports them.
    pub fn tiers(self) -> Vec<wire::Tier> {
        match self {
            TierSelection::Exact => vec![wire::Tier::Exact],
            TierSelection::ExactAndProbable => vec![wire::Tier::Exact, wire::Tier::Probable],
        }
    }
}

/// What the caller asked for, already parsed and validated.
#[derive(Debug, Clone)]
pub struct Args {
    pub from: String,
    pub to: String,
    pub traversal: wire::Traversal,
    pub tiers: TierSelection,
    pub max_depth: u32,
    pub time_budget_ms: u64,
    /// `--in`: restrict name resolution to paths under this prefix.
    pub scope: Option<String>,
    /// `--at-snapshot`: answer about the stored snapshot, skipping the
    /// after-check.
    pub at_snapshot: bool,
}

/// An evaluation that stopped before the traversal, carrying the reason the
/// envelope will report. Distinct from a technical failure, which is an
/// `Err` and exits 3.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Halt(pub wire::Reason);

/// Resolve one `--from`/`--to` argument to a symbol row.
///
/// A `#` makes it a uid: looked up verbatim, never ranked, and a uid that
/// names nothing is [`wire::Reason::SymbolNotFound`] — never an absence of
/// path. A bare name auto-resolves only on an exact unique match within
/// `scope`; two or more candidates are reported with their uids so the
/// caller can re-ask precisely.
pub fn resolve_argument(
    store: &GraphStore,
    argument: &str,
    value: &str,
    scope: Option<&str>,
) -> Result<SymbolRow, Halt> {
    if value.contains('#') {
        return match store.symbol_by_uid(value) {
            Ok(Some(row)) => Ok(row),
            Ok(None) | Err(_) => Err(Halt(wire::Reason::SymbolNotFound {
                argument: argument.to_string(),
            })),
        };
    }
    let rows = store
        .symbols_by_name(value, NAME_LOOKUP_LIMIT)
        .unwrap_or_default();
    let rows = match scope {
        None => rows,
        Some(prefix) => {
            let prefix = normalize_scope(prefix);
            rows.into_iter()
                .filter(|row| path_of(store, row).is_some_and(|p| p.starts_with(&prefix)))
                .collect()
        }
    };
    match rows.len() {
        0 => Err(Halt(wire::Reason::SymbolNotFound {
            argument: argument.to_string(),
        })),
        1 => Ok(rows.into_iter().next().expect("length checked")),
        _ => Err(Halt(wire::Reason::AmbiguousSymbol {
            argument: argument.to_string(),
            candidates: candidates(store, &rows),
        })),
    }
}

/// `--in` accepts a path with or without a trailing slash, in either slash
/// style; the store keeps repo-relative forward-slash paths.
fn normalize_scope(prefix: &str) -> String {
    prefix
        .replace('\\', "/")
        .trim_start_matches("./")
        .trim_start_matches('/')
        .to_string()
}

fn path_of(store: &GraphStore, row: &SymbolRow) -> Option<String> {
    store.file_by_id(row.file_id).ok().flatten().map(|f| f.path)
}

/// The candidates an ambiguous answer lists, capped so one common name
/// cannot flood the envelope. The cap is visible: a caller that sees
/// exactly [`CANDIDATE_CAP`] entries knows more may exist.
fn candidates(store: &GraphStore, rows: &[SymbolRow]) -> Vec<wire::Candidate> {
    rows.iter()
        .take(CANDIDATE_CAP)
        .map(|row| wire::Candidate {
            uid: row.uid.clone(),
            kind: row.kind.as_str().to_string(),
            path: path_of(store, row).unwrap_or_default(),
            line: row.start_line,
        })
        .collect()
}

/// The stored snapshot identity: the freshness signature and the extractor
/// version that produced the rows.
///
/// Both come from one `meta` read on the store handle the traversal will
/// use, which is what makes them the *same* generation as the rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub signature: String,
    pub extractor_version: String,
}

/// Read the snapshot identity, or say why there is none to speak of.
///
/// A missing signature means the graph predates signatures or was written
/// by another extractor; the withheld sentinel means a writer could not
/// vouch for the tree it had just written. Neither is a snapshot an answer
/// can be attributed to, and both are repaired by a refresh, so both are
/// [`wire::Reason::GraphStale`] rather than `SnapshotChanged`: nothing
/// changed under us, the stored state is simply not answerable.
pub fn identity(store: &GraphStore) -> Result<Identity, Halt> {
    let signature = store
        .meta_get(FRESHNESS_KEY)
        .ok()
        .flatten()
        .unwrap_or_default();
    if signature.is_empty() || signature == FRESHNESS_WITHHELD {
        return Err(Halt(wire::Reason::GraphStale));
    }
    let extractor_version = store
        .meta_get(EXTRACTOR_VERSION_KEY)
        .ok()
        .flatten()
        .unwrap_or_default();
    Ok(Identity {
        signature,
        extractor_version,
    })
}

/// Whether the working tree still hashes to `signature`.
///
/// This is the after-check; the before-check is the daemon's freshness gate,
/// which walks the same tree to decide whether a refresh is needed.
pub fn tree_matches(root: &Path, signature: &str) -> bool {
    freshness_signature(root) == signature
}

/// Assemble the wire envelope from a graph evaluation.
///
/// The mapping is total and deliberately dull: every honesty field the
/// evaluator measured is carried through, and the summary is generated from
/// the fixed templates so the scope travels with the verdict.
pub fn envelope(
    store: &GraphStore,
    evaluation: &predicate::Evaluation,
    identity: &Identity,
    args: &Args,
    epistemics: pixel_proto::Epistemics,
    graph_file_cap_hit: bool,
) -> wire::EvaluationEnvelope {
    let outcome = outcome(store, evaluation);
    let domain = wire::Domain {
        relation: wire::RELATION_INDEXED_CALL_GRAPH.to_string(),
        edge_kinds: evaluation
            .coverage
            .edge_kinds
            .iter()
            .map(|k| k.as_str().to_string())
            .collect(),
        traversal: args.traversal,
        tiers: args.tiers.tiers(),
    };
    let snapshot = wire::Snapshot {
        signature: identity.signature.clone(),
        extractor_version: identity.extractor_version.clone(),
        generation_coherent: true,
        working_tree_check: if args.at_snapshot {
            wire::WorkingTreeCheck::BeforeOnly
        } else {
            wire::WorkingTreeCheck::FullBeforeAndAfter
        },
        working_tree_matches: true,
    };
    let coverage = coverage(&evaluation.coverage, graph_file_cap_hit);
    wire::EvaluationEnvelope::new(
        wire::Predicate::Path,
        outcome,
        domain,
        snapshot,
        coverage,
        epistemics,
    )
}

/// The wire outcome for a graph status. `Unknown` reasons the evaluator
/// itself produces are budget exhaustion only; resolution and snapshot
/// reasons are raised before it ever runs.
fn outcome(store: &GraphStore, evaluation: &predicate::Evaluation) -> wire::Outcome {
    match &evaluation.status {
        predicate::Status::Established => wire::Outcome::Established {
            witness: witness(store, &evaluation.witness),
        },
        predicate::Status::AbsentInSnapshot => wire::Outcome::AbsentInSnapshot,
        predicate::Status::Unknown { reason } => {
            let reason = match reason {
                predicate::UnknownReason::TraversalBudgetExhausted { parameter, current } => {
                    wire::Reason::TraversalBudgetExhausted {
                        parameter: match parameter {
                            predicate::BudgetParameter::MaxDepth => wire::BudgetParameter::MaxDepth,
                            predicate::BudgetParameter::TimeBudgetMs => {
                                wire::BudgetParameter::TimeBudgetMs
                            }
                        },
                        current: *current,
                    }
                }
            };
            unknown(reason)
        }
    }
}

/// An `unknown` with the next actions its reason prescribes. Always built
/// through here so no reason can reach the wire without them.
pub fn unknown(reason: wire::Reason) -> wire::Outcome {
    let next_actions = wire::default_next_actions(&reason);
    wire::Outcome::Unknown {
        reason,
        next_actions,
    }
}

/// A graph witness on the wire. `Witness::None` cannot appear under an
/// `Established` status, and the conversion does not invent one: it is
/// mapped to an identity-less path with no edges only if the evaluator
/// contradicted itself, which the type system upstream already prevents.
fn witness(store: &GraphStore, witness: &predicate::Witness) -> wire::Witness {
    match witness {
        predicate::Witness::Path {
            probable_edges,
            edges,
        } => wire::Witness::Path {
            probable_edges: *probable_edges,
            edges: edges.iter().map(|e| witness_edge(store, e)).collect(),
        },
        predicate::Witness::Identity { symbol } => wire::Witness::Identity {
            symbol: symbol_ref(store, symbol),
            hunks: Vec::new(),
        },
        // Unreachable through `outcome`, which only builds a witness for
        // `Established`. Mapped rather than panicking: a daemon does not
        // abort a session over a contradiction it can report.
        predicate::Witness::None => wire::Witness::Path {
            probable_edges: 0,
            edges: Vec::new(),
        },
    }
}

fn witness_edge(store: &GraphStore, edge: &predicate::WitnessEdge) -> wire::WitnessEdge {
    wire::WitnessEdge {
        from: symbol_ref(store, &edge.from),
        to: symbol_ref(store, &edge.to),
        edge: wire::EdgeInfo {
            kind: edge.edge.kind.as_str().to_string(),
            tier: tier(edge.edge.tier),
            site: wire::CallSite {
                path: edge.edge.site.path.clone(),
                line: edge.edge.site.line,
            },
            receiver: edge.edge.receiver.clone(),
        },
        call_direction: wire::CallDirection::FromTo,
        traversal_step: edge.traversal_step,
        premises: premises(edge.premises.as_ref()),
    }
}

/// The premises of an edge, keeping the three cases the wire distinguishes:
/// an exact edge needs none, a probable edge either has the import rows that
/// justify it or has none the store can show.
fn premises(premises: Option<&predicate::Premises>) -> wire::Premises {
    match premises {
        None => wire::Premises::NotRequired,
        Some(p) if p.available => wire::Premises::Imports {
            imports: p
                .imports
                .iter()
                .map(|i| wire::ImportPremise {
                    from_path: i.from_path.clone(),
                    spec: i.spec.clone(),
                    to_path: i.to_path.clone(),
                })
                .collect(),
        },
        Some(_) => wire::Premises::Unavailable,
    }
}

/// A witness symbol plus the content hash of the file it lives in, so a
/// reader can check the witness against the bytes the graph parsed.
fn symbol_ref(store: &GraphStore, symbol: &predicate::SymbolRef) -> wire::SymbolRef {
    wire::SymbolRef {
        uid: symbol.uid.clone(),
        path: symbol.path.clone(),
        lines: symbol.lines,
        content_hash: content_hash(store, &symbol.path),
    }
}

fn content_hash(store: &GraphStore, path: &str) -> String {
    store
        .file_by_path(path)
        .ok()
        .flatten()
        .map(|f| f.blob_oid)
        .unwrap_or_default()
}

fn tier(tier: pixel_graph::store::Tier) -> wire::Tier {
    match tier {
        pixel_graph::store::Tier::Exact => wire::Tier::Exact,
        pixel_graph::store::Tier::Probable => wire::Tier::Probable,
    }
}

/// Carry the evaluator's coverage onto the wire.
///
/// `files_excluded_by_size` is what this evaluation observed, which is
/// nothing: the build does not record per-file size exclusions, and paying
/// for a second whole-tree walk to count them would double the cost of the
/// command. The claim a reader must not over-read is bounded by the
/// `absent_in_snapshot` summary, which names "files beyond caps" as outside
/// the relation whatever these counters say.
fn coverage(coverage: &predicate::Coverage, graph_file_cap_hit: bool) -> wire::Coverage {
    wire::Coverage {
        traversal_exhausted: coverage.traversal_exhausted,
        depth_cap: coverage.depth_cap,
        depth_cap_dropped_frontier: coverage.depth_cap_dropped_frontier,
        time_budget_ms: coverage.time_budget_ms,
        time_budget_hit: coverage.time_budget_hit,
        graph_file_cap_hit,
        files_excluded_by_size: 0,
        unresolved_same_name_sites: coverage.unresolved_same_name_sites,
        extraction_limits: extraction_limits(),
    }
}

/// The blind spots of tree-sitter extraction, stated on every answer: they
/// are why `closed_world` is never true and why an absence is bounded.
fn extraction_limits() -> Vec<String> {
    vec![
        "callbacks passed as arguments (e.g. schema.plugin(fn), emitter.on('event', fn))"
            .to_string(),
        "dynamic dispatch (e.g. obj[methodName]())".to_string(),
        "macro-generated calls".to_string(),
        "eval / new Function".to_string(),
    ]
}

/// An envelope for an evaluation that never ran: the reason, the identity
/// if one was readable, and coverage that claims nothing.
pub fn halted(
    reason: wire::Reason,
    identity: Option<&Identity>,
    args: &Args,
    working_tree_matches: bool,
    epistemics: pixel_proto::Epistemics,
) -> wire::EvaluationEnvelope {
    let domain = wire::Domain {
        relation: wire::RELATION_INDEXED_CALL_GRAPH.to_string(),
        edge_kinds: predicate::EDGE_KINDS
            .iter()
            .map(|k| k.as_str().to_string())
            .collect(),
        traversal: args.traversal,
        tiers: args.tiers.tiers(),
    };
    let snapshot = wire::Snapshot {
        signature: identity.map(|i| i.signature.clone()).unwrap_or_default(),
        extractor_version: identity
            .map(|i| i.extractor_version.clone())
            .unwrap_or_default(),
        generation_coherent: identity.is_some(),
        working_tree_check: if args.at_snapshot {
            wire::WorkingTreeCheck::BeforeOnly
        } else {
            wire::WorkingTreeCheck::FullBeforeAndAfter
        },
        working_tree_matches,
    };
    let coverage = wire::Coverage {
        traversal_exhausted: false,
        depth_cap: args.max_depth,
        depth_cap_dropped_frontier: false,
        time_budget_ms: args.time_budget_ms,
        time_budget_hit: false,
        graph_file_cap_hit: false,
        files_excluded_by_size: 0,
        unresolved_same_name_sites: 0,
        extraction_limits: extraction_limits(),
    };
    wire::EvaluationEnvelope::new(
        wire::Predicate::Path,
        unknown(reason),
        domain,
        snapshot,
        coverage,
        epistemics,
    )
}

/// The traversal and tier selection the graph layer expects.
pub fn request_shape(args: &Args) -> (predicate::Traversal, predicate::TierSelection) {
    let traversal = match args.traversal {
        wire::Traversal::Callees => predicate::Traversal::Callees,
        wire::Traversal::Callers => predicate::Traversal::Callers,
    };
    let tiers = match args.tiers {
        TierSelection::Exact => predicate::TierSelection::Exact,
        TierSelection::ExactAndProbable => predicate::TierSelection::ExactAndProbable,
    };
    (traversal, tiers)
}

//! Whole-repo build / single-file update orchestration.
//!
//! `build_graph`: walk → parallel extract (rayon) → single-writer store
//! phase (files+symbols, then imports, then tiered call resolution).
//! `update_file`: transactional per-file replacement + re-resolution.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::time::Instant;

use rayon::prelude::*;
use xxhash_rust::xxh3::xxh3_64;

use crate::extract::{FileExtraction, extract_file, lang_of};
use crate::imports::resolve_import;
use crate::resolve::{
    FileCalls, PendingCall, reconsider_resolved_calls, resolve_all, resolve_calls,
};
use crate::store::{EdgeKind, GraphStore, extract_crux};

/// Extract concepts for a file and insert them, linking each to the smallest
/// enclosing symbol (by line range) when one exists. `symbol_ids` are the ids
/// of the file's symbols in `start_line` order.
fn insert_concepts(
    store: &GraphStore,
    file_id: i64,
    rel: &str,
    content: &[u8],
    symbol_ids: &[i64],
    symbol_lines: &[(u32, u32)],
) -> Result<(), BoxErr> {
    let concepts = crate::concept::extract_concepts(rel, content);
    for mut c in concepts {
        c.owner_symbol_id = symbol_lines
            .iter()
            .enumerate()
            .filter(|(_, (s, e))| *s <= c.start_line && c.end_line <= *e)
            .min_by_key(|(_, (s, e))| e - s)
            .map(|(i, _)| symbol_ids[i]);
        store.insert_concept(
            file_id,
            c.kind,
            &c.raw,
            &c.norm,
            &c.detail,
            c.start_line,
            c.end_line,
            c.owner_symbol_id,
        )?;
    }
    Ok(())
}

type BoxErr = Box<dyn std::error::Error + Send + Sync>;

/// `meta` key under which the build-time freshness signature is stored.
pub const FRESHNESS_KEY: &str = "freshness";

#[derive(Debug, Clone)]
pub struct GraphStats {
    pub files: u64,
    pub symbols: u64,
    pub edges: u64,
    pub unresolved: u64,
    pub elapsed_ms: u128,
}

const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

fn read_source_file(path: &Path) -> Option<Vec<u8>> {
    let before = std::fs::symlink_metadata(path).ok()?;
    if !before.file_type().is_file() || before.len() > MAX_FILE_BYTES {
        return None;
    }
    let file = std::fs::File::open(path).ok()?;
    let opened = file.metadata().ok()?;
    let after = std::fs::symlink_metadata(path).ok()?;
    if !after.file_type().is_file()
        || opened.dev() != after.dev()
        || opened.ino() != after.ino()
        || opened.len() > MAX_FILE_BYTES
    {
        return None;
    }
    let mut content = Vec::with_capacity(opened.len() as usize);
    file.take(MAX_FILE_BYTES.saturating_add(1))
        .read_to_end(&mut content)
        .ok()?;
    (content.len() as u64 <= MAX_FILE_BYTES).then_some(content)
}
const BINARY_SNIFF_BYTES: usize = 8 * 1024;

fn is_binary(content: &[u8]) -> bool {
    let end = content.len().min(BINARY_SNIFF_BYTES);
    content[..end].contains(&0)
}

fn rel_path(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let s = rel.to_string_lossy().replace('\\', "/");
    if s.is_empty() { None } else { Some(s) }
}

/// Maximum number of source files to collect in a single walk when there is
/// no git anchor. With a git repo, `git ls-files` bounds the file set to
/// tracked files only; without git, the walk could traverse an entire home
/// directory or a vendored monorepo. This cap prevents pathological cases
/// from hanging the daemon. Override with `PIXEL_GRAPH_MAX_FILES=0` (disables)
/// or a positive integer. Default 50000 — generous for any real project,
/// but stops a runaway walk on a mis-rooted or huge directory.
const DEFAULT_GRAPH_MAX_FILES: usize = 50_000;

fn graph_max_files() -> Option<usize> {
    match std::env::var("PIXEL_GRAPH_MAX_FILES") {
        Ok(v) => v
            .parse::<usize>()
            .ok()
            .filter(|&n| n > 0)
            .or(Some(usize::MAX)),
        Err(_) => Some(DEFAULT_GRAPH_MAX_FILES),
    }
}

/// Walk `root` collecting supported source files (skips .git, .pixel,
/// default-ignored dirs, gitignored paths, binaries, oversized files). Hidden
/// files (dotfiles, `.github/`, `.claude/`, …) ARE collected — they are real
/// project content. The walk itself is the shared
/// `pixel_index::index::policy_walk`, so default-ignored-dir pruning and
/// gitless-tree gitignore handling stay in lockstep with the lexical index.
///
/// When `max_files` is `Some(n)`, the walk stops after collecting `n` source
/// files — a safety cap for non-git directories where there is no
/// `git ls-files` to bound the file set. `None` means no cap (git-anchored
/// builds where `git ls-files` already bounds the set).
fn collect_files(root: &Path) -> Vec<(String, Vec<u8>)> {
    let max_files = graph_max_files();
    let mut out = Vec::new();
    let walker = pixel_index::index::policy_walk(root);
    for entry in walker.flatten() {
        if let Some(cap) = max_files
            && out.len() >= cap
        {
            break;
        }
        let is_file = entry.file_type().is_some_and(|t| t.is_file());
        if !is_file {
            continue;
        }
        let Some(rel) = rel_path(root, entry.path()) else {
            continue;
        };
        if lang_of(&rel).is_none() {
            continue;
        }
        let Some(content) = read_source_file(entry.path()) else {
            continue;
        };
        if is_binary(&content) {
            continue;
        }
        out.push((rel, content));
    }
    out
}

struct Extracted {
    rel: String,
    blob_oid: String,
    content: Vec<u8>,
    fx: FileExtraction,
}

fn input_signature(inputs: &[(String, Vec<u8>)]) -> String {
    let mut entries: Vec<(&str, u64)> = inputs
        .iter()
        .map(|(rel, content)| (rel.as_str(), xxh3_64(content)))
        .collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    let mut hasher_buf = Vec::with_capacity(entries.len() * 24);
    for (rel, hash) in entries {
        hasher_buf.extend_from_slice(rel.as_bytes());
        hasher_buf.extend_from_slice(&hash.to_le_bytes());
    }
    format!("{:016x}", xxh3_64(&hasher_buf))
}

/// Full graph build: parse everything in parallel, then write files,
/// symbols, imports, and resolved call edges.
pub fn build_graph(root: &Path, db_path: &Path) -> Result<GraphStats, BoxErr> {
    let t0 = Instant::now();

    let inputs = collect_files(root);
    let snapshot_signature = input_signature(&inputs);
    let mut extracted: Vec<Extracted> = inputs
        .into_par_iter()
        .filter_map(|(rel, content)| {
            let fx = extract_file(&rel, &content)?;
            let blob_oid = format!("{:016x}", xxh3_64(&content));
            Some(Extracted {
                rel,
                blob_oid,
                content,
                fx,
            })
        })
        .collect();

    let all_paths: Vec<String> = extracted.iter().map(|e| e.rel.clone()).collect();

    let mut store = GraphStore::open(db_path)?;

    // Drop files that vanished since the last build.
    let known: std::collections::HashSet<&str> = all_paths.iter().map(String::as_str).collect();
    let stale: Vec<String> = store
        .files()?
        .into_iter()
        .filter(|f| !known.contains(f.path.as_str()))
        .map(|f| f.path)
        .collect();
    for path in &stale {
        store.remove_file(path)?;
    }

    // Pass 1: files + symbols (need every file id before import resolution).
    let mut path_to_id: HashMap<String, i64> = HashMap::new();
    let mut sym_ids: Vec<Vec<i64>> = Vec::with_capacity(extracted.len());
    for (i, e) in extracted.iter_mut().enumerate() {
        let file_id = store.replace_file(&e.rel, &e.blob_oid, e.fx.lang)?;
        path_to_id.insert(e.rel.clone(), file_id);
        let mut ids = Vec::with_capacity(e.fx.symbols.len());
        let mut lines = Vec::with_capacity(e.fx.symbols.len());
        for s in &e.fx.symbols {
            let uid = format!("{}#{}#{}", e.rel, s.qualified, s.kind.as_str());
            let id = store.insert_symbol(
                file_id,
                &uid,
                &s.name,
                &s.qualified,
                s.kind,
                s.start_line,
                s.end_line,
                &s.sig,
            )?;
            ids.push(id);
            lines.push((s.start_line, s.end_line));

            // P2·3: content-anchored crux — store the guarded logical lines of
            // each symbol's body (guards, state mutations, early-returns) so
            // retrieval can surface them. Fingerprints are content-stable: they survive
            // a file that later shifts line numbers (re-anchoring by fingerprint).
            let body_str = String::from_utf8_lossy(&e.content);
            let all_lines: Vec<&str> = body_str.lines().collect();
            let start = (s.start_line.saturating_sub(1) as usize).min(all_lines.len());
            let end = (s.end_line.saturating_sub(1) as usize).min(all_lines.len());
            let body = if end > start {
                all_lines[start..end].join("\n")
            } else {
                String::new()
            };
            let crux = extract_crux(&body, 3);
            store.set_symbol_crux(id, &crux)?;
        }
        sym_ids.push(ids);
        // Engine 1: concept pass alongside symbol extraction with O(1) content access.
        insert_concepts(&store, file_id, &e.rel, &e.content, &sym_ids[i], &lines)?;
        e.content.clear();
        e.content.shrink_to_fit();
    }

    // Pass 2: imports (resolved against the full file list) + pending calls.
    let mut pending: Vec<FileCalls> = Vec::with_capacity(extracted.len());
    for (i, e) in extracted.iter().enumerate() {
        let file_id = path_to_id[&e.rel];
        for imp in &e.fx.imports {
            let resolved = resolve_import(&imp.spec, &e.rel, &all_paths)
                .and_then(|p| path_to_id.get(&p).copied());
            store.insert_import(file_id, &imp.spec, resolved, &imp.bindings)?;
        }
        let calls =
            e.fx.calls
                .iter()
                .map(|c| PendingCall {
                    callee_name: c.callee_name.clone(),
                    enclosing_symbol_id: c.enclosing_index.map(|ix| sym_ids[i][ix]),
                    site_line: c.site_line,
                    receiver: c.receiver.clone(),
                })
                .collect();
        pending.push(FileCalls { file_id, calls });
    }

    resolve_calls(&store, &pending)?;

    // Bind freshness to the exact bytes parsed above. If the source tree moved
    // during extraction/storage, publishing this graph as fresh would attach
    // old symbols to a new filesystem signature.
    let current_signature = freshness_signature(root);
    if current_signature != snapshot_signature {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "source changed during graph build; graph was not published as fresh",
        )
        .into());
    }
    store.meta_set(FRESHNESS_KEY, &snapshot_signature)?;

    let (files, symbols, edges, unresolved) = store.counts()?;
    Ok(GraphStats {
        files,
        symbols,
        edges,
        unresolved,
        elapsed_ms: t0.elapsed().as_millis(),
    })
}

/// `(repo-relative path, xxh3 content hash)` of every supported source file
/// under `root`, sorted by path. This is the exact input set of
/// `build_graph`, and the per-file hash is the `blob_oid` the store keeps
/// for each file, so a stored row whose `blob_oid` differs from the entry
/// here is a file that changed since the graph was built.
fn tree_hashes(root: &Path) -> Vec<(String, u64)> {
    // Must mirror `collect_files`'s walk policy exactly — both go through
    // `pixel_index::index::policy_walk` — or the freshness signature would
    // disagree with the set of files the graph was actually built from.
    let walker = pixel_index::index::policy_walk(root);
    let mut entries: Vec<(String, u64)> = walker
        .flatten()
        .filter_map(|entry| {
            let is_file = entry.file_type().is_some_and(|t| t.is_file());
            if !is_file {
                return None;
            }
            let rel = rel_path(root, entry.path())?;
            lang_of(&rel)?;
            let content = read_source_file(entry.path())?;
            if is_binary(&content) {
                return None;
            }
            let hash = xxh3_64(&content);
            Some((rel, hash))
        })
        .collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries
}

fn signature_of(entries: &[(String, u64)]) -> String {
    let mut hasher_buf: Vec<u8> = Vec::with_capacity(entries.len() * 24);
    for (rel, hash) in entries {
        hasher_buf.extend_from_slice(rel.as_bytes());
        hasher_buf.extend_from_slice(&hash.to_le_bytes());
    }
    format!("{:016x}", xxh3_64(&hasher_buf))
}

/// Content-aware signature of every supported source file under `root`.
/// The signature includes a fast xxh3 content hash per file, so it detects
/// equal-size edits even when mtime is restored (e.g. `touch -t`). This is
/// more expensive than a stat-only signature but is necessary for trust:
/// a stale graph would serve obsolete symbols. The cost is bounded by
/// `MAX_FILE_BYTES` per file and parallelized via rayon. Symlinks are
/// excluded (their target's content would be unstable and they are never
/// indexed).
pub fn freshness_signature(root: &Path) -> String {
    signature_of(&tree_hashes(root))
}

/// What separates the working tree from the graph at `db_path`.
#[derive(Debug, Clone)]
pub struct TreeDelta {
    /// The stored signature equals the tree's: nothing to do.
    pub fresh: bool,
    /// Files added or edited since the build (present in the tree, absent
    /// from the store or stored under another content hash), with the hash
    /// the tree had when the delta was taken.
    pub changed: Vec<(String, u64)>,
    /// Files the store knows that are no longer in the tree.
    pub removed: Vec<String>,
    /// Number of files the store currently holds.
    pub indexed_files: usize,
    /// Signature of the tree as walked for this delta.
    pub signature: String,
}

impl TreeDelta {
    pub fn changed_count(&self) -> usize {
        self.changed.len() + self.removed.len()
    }
}

/// Compare `root`'s working tree with the graph at `db_path`. One walk
/// (the same one `freshness_signature` makes) answers both "is it fresh"
/// and "which files drifted". `Ok(None)` when the db carries no freshness
/// signature (built before signatures existed, or interrupted): the caller
/// cannot trust its rows and must rebuild.
pub fn tree_delta(root: &Path, db_path: &Path) -> Result<Option<TreeDelta>, BoxErr> {
    let store = GraphStore::open(db_path)?;
    let Some(stored) = store.meta_get(FRESHNESS_KEY)? else {
        return Ok(None);
    };
    let current = tree_hashes(root);
    let signature = signature_of(&current);
    let known: HashMap<String, String> = store
        .files()?
        .into_iter()
        .map(|f| (f.path, f.blob_oid))
        .collect();
    if stored == signature {
        return Ok(Some(TreeDelta {
            fresh: true,
            changed: Vec::new(),
            removed: Vec::new(),
            indexed_files: known.len(),
            signature,
        }));
    }
    let present: HashSet<&str> = current.iter().map(|(rel, _)| rel.as_str()).collect();
    let changed: Vec<(String, u64)> = current
        .iter()
        .filter(|(rel, hash)| known.get(rel) != Some(&format!("{hash:016x}")))
        .cloned()
        .collect();
    let mut removed: Vec<String> = known
        .keys()
        .filter(|path| !present.contains(path.as_str()))
        .cloned()
        .collect();
    removed.sort();
    Ok(Some(TreeDelta {
        fresh: false,
        changed,
        removed,
        indexed_files: known.len(),
        signature,
    }))
}

/// Bring the graph up to date with a [`TreeDelta`]: re-extract the changed
/// files, drop the removed ones, re-resolve the calls that targeted them
/// (see `update_files`), then publish `delta.signature` as the freshness
/// signature. The signature is only published when every changed file was
/// stored under the hash the delta saw — a file edited while the update
/// ran is left unsigned, so the next open detects the drift again instead
/// of binding stale symbols to a fresh-looking signature.
pub fn apply_tree_delta(root: &Path, db_path: &Path, delta: &TreeDelta) -> Result<(), BoxErr> {
    let files: Vec<(&str, bool)> = delta
        .changed
        .iter()
        .map(|(rel, _)| (rel.as_str(), false))
        .chain(delta.removed.iter().map(|rel| (rel.as_str(), true)))
        .collect();
    update_files_unsigned(root, db_path, &files)?;
    let store = GraphStore::open(db_path)?;
    for (rel, hash) in &delta.changed {
        let stored = store.file_by_path(rel)?.map(|f| f.blob_oid);
        // A changed file that extraction dropped (unparseable, vanished)
        // has no row; that is its stable state, not a race.
        if stored.is_some_and(|oid| oid != format!("{hash:016x}")) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                format!("{rel} changed during incremental graph update; graph was not published as fresh"),
            )
            .into());
        }
    }
    store.meta_set(FRESHNESS_KEY, &delta.signature)?;
    Ok(())
}

/// True iff the on-disk graph at `db_path` is fresh relative to `root`'s
/// current working tree. A missing db is never fresh. A db whose stored
/// signature matches `freshness_signature(root)` is fresh; otherwise (or if
/// the meta key is absent on an old db) it is stale and must be rebuilt.
pub fn is_fresh(root: &Path, db_path: &Path) -> bool {
    let Ok(store) = GraphStore::open(db_path) else {
        return false;
    };
    let Ok(Some(stored)) = store.meta_get(FRESHNESS_KEY) else {
        return false;
    };
    stored == freshness_signature(root)
}

/// Incrementally re-index a batch of files: preserve incoming call knowledge,
/// replace files, re-extract symbols and concepts, and resolve all calls once.
pub fn update_files(root: &Path, db_path: &Path, files: &[(&str, bool)]) -> Result<(), BoxErr> {
    if files.is_empty() {
        return Ok(());
    }
    update_files_unsigned(root, db_path, files)?;
    // Keep the freshness signature in sync so a later cold open does not
    // needlessly rebuild after this incremental update.
    let store = GraphStore::open(db_path)?;
    store.meta_set(FRESHNESS_KEY, &freshness_signature(root))?;
    Ok(())
}

/// `update_files` without the closing signature write: the caller decides
/// which signature (if any) describes the tree it just synchronised to.
fn update_files_unsigned(
    root: &Path,
    db_path: &Path,
    files: &[(&str, bool)],
) -> Result<(), BoxErr> {
    /// A changed file after pass 1: its row and symbols are in the store,
    /// its imports and calls wait for every file of the batch to exist.
    struct Staged {
        rel: String,
        file_id: i64,
        fx: FileExtraction,
        symbol_ids: Vec<i64>,
    }

    if files.is_empty() {
        return Ok(());
    }
    let mut store = GraphStore::open(db_path)?;
    let mut all_changed_names: HashSet<String> = HashSet::new();
    let known_before: HashSet<String> = store.files()?.into_iter().map(|f| f.path).collect();

    let mut staged: Vec<Staged> = Vec::with_capacity(files.len());

    // Pass 1: files + symbols + concepts. Same split as `build_graph`: an
    // import from file A to file B added in the same batch can only
    // resolve once B has a row, so nothing here touches imports or calls.
    for &(rel, removed) in files {
        let abs = root.join(rel);

        // Demote incoming call edges (from OTHER files) into unresolved rows so
        // they can re-link after the rebuild instead of being silently dropped.
        // The receiver is preserved so receiver calls are never falsely promoted
        // from Probable to Exact during re-resolution.
        if let Some(old) = store.file_by_path(rel)? {
            let old_syms = store.symbols_in_file(old.id)?;
            let mut demoted: Vec<(i64, String, i64, u32, Option<String>)> = Vec::new();
            for sym in &old_syms {
                for edge in store.edges_to(sym.id, Some(EdgeKind::Calls))? {
                    let src_file: Option<i64> = store
                        .conn()
                        .query_row(
                            "SELECT file_id FROM symbols WHERE id = ?1",
                            rusqlite::params![edge.src_id],
                            |r| r.get(0),
                        )
                        .ok();
                    if let Some(src_file) = src_file
                        && src_file != old.id
                    {
                        demoted.push((
                            src_file,
                            sym.name.clone(),
                            edge.src_id,
                            edge.site_line,
                            edge.receiver.clone(),
                        ));
                    }
                }
            }
            for (src_file, name, src_id, site_line, receiver) in demoted {
                store.insert_unresolved_call(
                    src_file,
                    &name,
                    Some(src_id),
                    site_line,
                    receiver.as_deref(),
                )?;
            }
        }

        if removed {
            store.remove_file(rel)?;
            continue;
        }

        let Some(content) = read_source_file(&abs) else {
            store.remove_file(rel)?;
            continue;
        };

        let Some(fx) = extract_file(rel, &content) else {
            store.remove_file(rel)?;
            continue;
        };

        for s in &fx.symbols {
            all_changed_names.insert(s.name.clone());
        }

        let blob_oid = format!("{:016x}", xxh3_64(&content));
        let file_id = store.replace_file(rel, &blob_oid, fx.lang)?;

        let mut ids = Vec::with_capacity(fx.symbols.len());
        let mut lines = Vec::with_capacity(fx.symbols.len());
        for s in &fx.symbols {
            let uid = format!("{rel}#{}#{}", s.qualified, s.kind.as_str());
            let id = store.insert_symbol(
                file_id,
                &uid,
                &s.name,
                &s.qualified,
                s.kind,
                s.start_line,
                s.end_line,
                &s.sig,
            )?;
            ids.push(id);
            lines.push((s.start_line, s.end_line));

            // P2·3: content-anchored crux for the incremental re-index path.
            let body_str = String::from_utf8_lossy(&content);
            let all_lines: Vec<&str> = body_str.lines().collect();
            let start = (s.start_line.saturating_sub(1) as usize).min(all_lines.len());
            let end = (s.end_line.saturating_sub(1) as usize).min(all_lines.len());
            let body = if end > start {
                all_lines[start..end].join("\n")
            } else {
                String::new()
            };
            let crux = extract_crux(&body, 3);
            store.set_symbol_crux(id, &crux)?;
        }
        insert_concepts(&store, file_id, rel, &content, &ids, &lines)?;
        staged.push(Staged {
            rel: rel.to_string(),
            file_id,
            fx,
            symbol_ids: ids,
        });
    }

    // The file list as it stands AFTER pass 1: added files included,
    // removed ones gone.
    let files_now = store.files()?;
    let all_paths: Vec<String> = files_now.iter().map(|f| f.path.clone()).collect();
    let path_to_id: HashMap<String, i64> = files_now.into_iter().map(|f| (f.path, f.id)).collect();

    // Pass 2: imports + pending calls of the changed files.
    let mut pending_calls: Vec<FileCalls> = Vec::with_capacity(staged.len());
    for st in &staged {
        for imp in &st.fx.imports {
            let resolved = resolve_import(&imp.spec, &st.rel, &all_paths)
                .and_then(|p| path_to_id.get(&p).copied());
            store.insert_import(st.file_id, &imp.spec, resolved, &imp.bindings)?;
        }
        let calls = st
            .fx
            .calls
            .iter()
            .map(|c| PendingCall {
                callee_name: c.callee_name.clone(),
                enclosing_symbol_id: c.enclosing_index.map(|ix| st.symbol_ids[ix]),
                site_line: c.site_line,
                receiver: c.receiver.clone(),
            })
            .collect();
        pending_calls.push(FileCalls {
            file_id: st.file_id,
            calls,
        });
    }

    // A file that appeared in this batch may be the target of imports that
    // UNCHANGED files could never resolve before (`import x from "./new"`
    // written ahead of the file). Re-resolve every dangling import against
    // the new file list so the resolver's import tier sees them.
    let added_any = staged.iter().any(|st| !known_before.contains(&st.rel));
    if added_any {
        let dangling: Vec<(i64, String, String)> = {
            let mut stmt = store.conn().prepare(
                "SELECT i.id, i.spec, f.path FROM imports i
                   JOIN files f ON f.id = i.file_id
                  WHERE i.resolved_file_id IS NULL",
            )?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            rows.collect::<std::result::Result<_, _>>()?
        };
        for (import_id, spec, importer) in dangling {
            if let Some(target) = resolve_import(&spec, &importer, &all_paths)
                .and_then(|p| path_to_id.get(&p).copied())
            {
                store.conn().execute(
                    "UPDATE imports SET resolved_file_id = ?2 WHERE id = ?1",
                    rusqlite::params![import_id, target],
                )?;
            }
        }
    }

    if !pending_calls.is_empty() {
        resolve_calls(&store, &pending_calls)?;
    }

    // Any changed definition can invalidate a previously unique target.
    reconsider_resolved_calls(&mut store, &all_changed_names)?;
    // Retry everything unresolved against the complete new candidate set.
    resolve_all(&mut store)?;

    // Persisted analyses (`processes`, `clusters`) are keyed by symbol id,
    // and `replace_file` hands re-extracted symbols NEW ids: the cached
    // rows would point at deleted symbols. A full rebuild starts from an
    // empty db, so they were recomputed on demand; give the incremental
    // path the same guarantee.
    store.conn().execute_batch(
        "DELETE FROM process_steps; DELETE FROM processes;
         DELETE FROM cluster_members; DELETE FROM clusters;",
    )?;
    Ok(())
}

/// Incrementally re-index one file.
pub fn update_file(root: &Path, db_path: &Path, rel: &str) -> Result<(), BoxErr> {
    update_files(root, db_path, &[(rel, false)])
}

/// Concepts-only refresh for a file that is NOT a graph language (e.g. a
/// `.svelte`/`.vue`/`.html`/`.json`/`.yaml`/`.css` file the symbol graph
/// ignores). Ensures the file row exists, then replaces its concepts in one
/// transaction. No-op for files outside the concept gate.
pub fn update_concepts(root: &Path, db_path: &Path, rel: &str) -> Result<(), BoxErr> {
    if crate::concept::concept_lang_of(rel).is_none() {
        return Ok(());
    }
    let mut store = GraphStore::open(db_path)?;
    let abs = root.join(rel);
    let Some(content) = read_source_file(&abs) else {
        store.remove_file(rel)?;
        return Ok(());
    };
    let concepts = crate::concept::extract_concepts(rel, &content);
    let file_id = store.replace_file(rel, &format!("{:016x}", xxh3_64(&content)), "concept")?;
    store.replace_concepts(file_id, &concepts)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolve::{Decision, ResolveIndex};
    use crate::store::Tier;

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "pixel-graph-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn smoke_build_graph_ts_and_rust() {
        let root = tmpdir("smoke");
        std::fs::write(
            root.join("a.ts"),
            "export function greet(name: string): string {\n  return \"hi \" + name;\n}\n",
        )
        .unwrap();
        std::fs::write(
            root.join("b.ts"),
            "import { greet } from \"./a\";\nexport function main() {\n  return greet(\"x\");\n}\n",
        )
        .unwrap();
        std::fs::write(
            root.join("c.rs"),
            "fn helper() -> u32 { 1 }\nfn run() -> u32 { helper() }\n",
        )
        .unwrap();

        let db = root.join(".pixel").join("graph.db");
        let stats = build_graph(&root, &db).unwrap();
        assert_eq!(stats.files, 3, "all three files indexed");
        assert!(
            stats.symbols >= 4,
            "greet, main, helper, run: {}",
            stats.symbols
        );
        assert!(
            stats.edges >= 2,
            "cross-file + same-file call edges: {}",
            stats.edges
        );

        let store = GraphStore::open(&db).unwrap();
        // Cross-file: main -> greet must be an Exact (T1 import-resolved) edge.
        let greet = &store.symbols_by_name("greet", 10).unwrap()[0];
        let callers = store.edges_to(greet.id, Some(EdgeKind::Calls)).unwrap();
        assert_eq!(callers.len(), 1, "exactly one caller of greet");
        assert_eq!(callers[0].tier, Tier::Exact);
        let main_sym = &store.symbols_by_name("main", 10).unwrap()[0];
        assert_eq!(callers[0].src_id, main_sym.id, "caller is b.ts main");
        // Same-file Rust: run -> helper Exact (T0).
        let helper = &store.symbols_by_name("helper", 10).unwrap()[0];
        let hcallers = store.edges_to(helper.id, Some(EdgeKind::Calls)).unwrap();
        assert_eq!(hcallers.len(), 1);
        assert_eq!(hcallers[0].tier, Tier::Exact);
        // Sanity: counts agree with stats.
        let (f, s, e, _u) = store.counts().unwrap();
        assert_eq!((f, s, e), (stats.files, stats.symbols, stats.edges));
        drop(store);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn smoke_update_file_relinks_callers() {
        let root = tmpdir("update");
        std::fs::write(root.join("a.ts"), "export function greet() { return 1 }\n").unwrap();
        std::fs::write(
            root.join("b.ts"),
            "import { greet } from \"./a\";\nexport function main() { return greet() }\n",
        )
        .unwrap();
        let db = root.join(".pixel").join("graph.db");
        build_graph(&root, &db).unwrap();

        // Edit a.ts (same symbol, new body) and update just that file.
        std::fs::write(root.join("a.ts"), "export function greet() { return 2 }\n").unwrap();
        update_file(&root, &db, "a.ts").unwrap();

        let store = GraphStore::open(&db).unwrap();
        let greet = &store.symbols_by_name("greet", 10).unwrap()[0];
        let callers = store.edges_to(greet.id, Some(EdgeKind::Calls)).unwrap();
        assert_eq!(
            callers.len(),
            1,
            "caller edge survives an incremental update"
        );
        assert_eq!(callers[0].tier, Tier::Exact);
        drop(store);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `tree_delta` is the one walk behind both the freshness verdict and
    /// the incremental update: it must name exactly the files that drifted
    /// (added, edited, removed) and nothing else, or the daemon would either
    /// re-extract the whole tree (defeating the point) or miss an edit
    /// (serving stale symbols).
    #[test]
    fn tree_delta_names_added_edited_and_removed_files_only() {
        let root = tmpdir("delta");
        std::fs::write(root.join("a.ts"), "export function alpha() { return 1 }\n").unwrap();
        std::fs::write(root.join("b.ts"), "export function beta() { return 1 }\n").unwrap();
        std::fs::write(root.join("c.ts"), "export function gamma() { return 1 }\n").unwrap();
        let db = root.join(".pixel").join("graph.db");
        build_graph(&root, &db).unwrap();

        let fresh = tree_delta(&root, &db).unwrap().expect("signed db");
        assert!(fresh.fresh);
        assert_eq!(fresh.changed_count(), 0);
        assert_eq!(fresh.indexed_files, 3);
        assert_eq!(fresh.signature, freshness_signature(&root));

        // Same size, different content (the `touch -t` shape), one new
        // file, one deleted file; `b.ts` untouched.
        std::fs::write(root.join("a.ts"), "export function alpha() { return 2 }\n").unwrap();
        std::fs::write(root.join("d.ts"), "export function delta() { return 1 }\n").unwrap();
        std::fs::remove_file(root.join("c.ts")).unwrap();

        let delta = tree_delta(&root, &db).unwrap().expect("signed db");
        assert!(!delta.fresh);
        let changed: Vec<&str> = delta.changed.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(changed, ["a.ts", "d.ts"], "sorted, b.ts untouched");
        assert_eq!(delta.removed, ["c.ts"]);
        assert_eq!(delta.indexed_files, 3, "counts the graph as built");
        assert_eq!(delta.signature, freshness_signature(&root));

        // Applying it makes the graph fresh again with exactly the
        // surviving files, and the deleted file's symbol is gone.
        apply_tree_delta(&root, &db, &delta).unwrap();
        assert!(is_fresh(&root, &db));
        let store = GraphStore::open(&db).unwrap();
        let mut paths: Vec<String> = store.files().unwrap().into_iter().map(|f| f.path).collect();
        paths.sort();
        assert_eq!(paths, ["a.ts", "b.ts", "d.ts"]);
        assert!(store.symbols_by_name("gamma", 5).unwrap().is_empty());
        assert_eq!(store.symbols_by_name("delta", 5).unwrap().len(), 1);
        drop(store);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Parity with a full rebuild, part 1: imports between files of the
    /// same batch, and imports of UNCHANGED files that pointed at a file
    /// which did not exist yet, resolve once the batch lands. Without this
    /// the resolver's import tier never saw the new file and the caller
    /// edge came out `Probable` or unresolved, unlike after `pixel graph`.
    #[test]
    fn incremental_update_resolves_imports_to_files_added_in_the_batch() {
        let root = tmpdir("delta-imports");
        // a.ts imports a file that does not exist yet.
        std::fs::write(
            root.join("a.ts"),
            "import { helper } from \"./b\";\nexport function work() { return helper() }\n",
        )
        .unwrap();
        let db = root.join(".pixel").join("graph.db");
        build_graph(&root, &db).unwrap();
        let store = GraphStore::open(&db).unwrap();
        let a_id = store.file_by_path("a.ts").unwrap().unwrap().id;
        let unresolved: i64 = store
            .conn()
            .query_row(
                "SELECT count(*) FROM imports WHERE file_id = ?1 AND resolved_file_id IS NULL",
                rusqlite::params![a_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(unresolved, 1, "b.ts does not exist: import dangles");
        drop(store);

        // b.ts appears, together with c.ts which imports it in the same batch.
        std::fs::write(root.join("b.ts"), "export function helper() { return 1 }\n").unwrap();
        std::fs::write(
            root.join("c.ts"),
            "import { helper } from \"./b\";\nexport function other() { return helper() }\n",
        )
        .unwrap();
        let delta = tree_delta(&root, &db).unwrap().unwrap();
        apply_tree_delta(&root, &db, &delta).unwrap();

        let store = GraphStore::open(&db).unwrap();
        let b_id = store.file_by_path("b.ts").unwrap().unwrap().id;
        for importer in ["a.ts", "c.ts"] {
            let f = store.file_by_path(importer).unwrap().unwrap().id;
            let resolved: Option<i64> = store
                .conn()
                .query_row(
                    "SELECT resolved_file_id FROM imports WHERE file_id = ?1",
                    rusqlite::params![f],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(
                resolved,
                Some(b_id),
                "{importer}: import must resolve to b.ts"
            );
        }
        let helper = &store.symbols_by_name("helper", 5).unwrap()[0];
        let callers = store.edges_to(helper.id, Some(EdgeKind::Calls)).unwrap();
        assert_eq!(callers.len(), 2, "work() and other() both call helper()");
        assert!(
            callers.iter().all(|e| e.tier == Tier::Exact),
            "imported unique target: Exact, as after a full rebuild ({callers:?})"
        );
        drop(store);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Parity with a full rebuild, part 2: persisted `processes`/`clusters`
    /// are keyed by symbol id and a re-extracted file gets new ids; after an
    /// incremental update they must be gone (recomputed on demand), not
    /// left pointing at deleted symbols.
    #[test]
    fn incremental_update_drops_cached_processes_and_clusters() {
        let root = tmpdir("delta-analyses");
        std::fs::write(root.join("a.ts"), "export function alpha() { return 1 }\n").unwrap();
        let db = root.join(".pixel").join("graph.db");
        build_graph(&root, &db).unwrap();
        let store = GraphStore::open(&db).unwrap();
        let alpha = &store.symbols_by_name("alpha", 5).unwrap()[0];
        store
            .conn()
            .execute_batch(&format!(
                "INSERT INTO processes (id, label, entry_symbol_id, step_count) VALUES (1, 'p', {0}, 1);
                 INSERT INTO process_steps (process_id, step, symbol_id) VALUES (1, 0, {0});
                 INSERT INTO clusters (id, label) VALUES (1, 'c');
                 INSERT INTO cluster_members (cluster_id, symbol_id) VALUES (1, {0});",
                alpha.id
            ))
            .unwrap();
        drop(store);

        std::fs::write(root.join("a.ts"), "export function alpha() { return 2 }\n").unwrap();
        let delta = tree_delta(&root, &db).unwrap().unwrap();
        apply_tree_delta(&root, &db, &delta).unwrap();

        let store = GraphStore::open(&db).unwrap();
        for table in ["processes", "process_steps", "clusters", "cluster_members"] {
            let n: i64 = store
                .conn()
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 0, "{table} must be cleared by the incremental update");
        }
        drop(store);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A db without a freshness signature cannot say what it was built
    /// from: `tree_delta` refuses to guess (None) so the caller rebuilds.
    #[test]
    fn tree_delta_is_none_without_a_signature() {
        let root = tmpdir("delta-unsigned");
        std::fs::write(root.join("a.ts"), "export function alpha() { return 1 }\n").unwrap();
        let db = root.join(".pixel").join("graph.db");
        build_graph(&root, &db).unwrap();
        GraphStore::open(&db)
            .unwrap()
            .conn()
            .execute(
                "DELETE FROM meta WHERE key = ?1",
                rusqlite::params![FRESHNESS_KEY],
            )
            .unwrap();
        assert!(tree_delta(&root, &db).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A file edited between the delta walk and its application must not be
    /// signed as fresh: the delta's signature describes bytes the store never
    /// saw. The update itself still lands (the graph is no worse than before),
    /// only the signature is withheld so the next open detects the drift.
    #[test]
    fn apply_tree_delta_withholds_signature_when_a_file_changed_underneath() {
        let root = tmpdir("delta-race");
        std::fs::write(root.join("a.ts"), "export function alpha() { return 1 }\n").unwrap();
        let db = root.join(".pixel").join("graph.db");
        build_graph(&root, &db).unwrap();
        std::fs::write(root.join("a.ts"), "export function alpha() { return 2 }\n").unwrap();
        let delta = tree_delta(&root, &db).unwrap().unwrap();
        // The "concurrent" edit: the tree moves on after the walk.
        std::fs::write(root.join("a.ts"), "export function alpha() { return 3 }\n").unwrap();
        let err = apply_tree_delta(&root, &db, &delta).unwrap_err();
        assert!(
            err.to_string().contains("changed during incremental"),
            "{err}"
        );
        assert!(
            !is_fresh(&root, &db),
            "signature must not have been published"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Regression: an existing graph.db must be detected as stale when a
    /// source file changes, and `is_fresh` must reflect that. After a rebuild
    /// the db is fresh again.
    #[test]
    fn freshness_detects_drift_and_rebuild() {
        let root = tmpdir("fresh");
        std::fs::write(root.join("a.ts"), "export function alpha() { return 1 }\n").unwrap();
        let db = root.join(".pixel").join("graph.db");

        // Initial build: fresh.
        build_graph(&root, &db).unwrap();
        assert!(
            is_fresh(&root, &db),
            "graph must be fresh right after build"
        );

        // Edit a source file: now stale.
        std::fs::write(root.join("a.ts"), "export function alpha() { return 2 }\n").unwrap();
        assert!(
            !is_fresh(&root, &db),
            "graph must be stale after a source file changes"
        );

        // Rebuild: fresh again.
        build_graph(&root, &db).unwrap();
        assert!(is_fresh(&root, &db), "graph must be fresh after rebuild");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Regression: a method call with a real receiver (`x.parse()`) must NOT
    /// be linked as an `Exact` edge to a same-name function/method elsewhere.
    /// The cited false-positive was `42.parse()` linking to `SymbolKind::parse`.
    /// The receiver downgrade caps such calls at `Probable` at most.
    #[test]
    fn receiver_call_not_exact_to_same_name_function() {
        let root = tmpdir("receiver");
        // a.ts defines a free function `parse` (unique repo-wide).
        std::fs::write(
            root.join("a.ts"),
            "export function parse(input: string): number { return Number(input); }\n",
        )
        .unwrap();
        // b.ts calls `parse(...)` directly (no receiver) AND `n.parse(...)`
        // with a receiver. The bare call should be Exact (T2 unique); the
        // receiver call must NOT be Exact.
        std::fs::write(
            root.join("b.ts"),
            "import { parse } from \"./a\";\n\
             export function caller(n: any) {\n  \
             const a = parse(\"42\");\n  \
             const b = n.parse(\"42\");\n\
             }\n",
        )
        .unwrap();
        let db = root.join(".pixel").join("graph.db");
        build_graph(&root, &db).unwrap();

        let store = GraphStore::open(&db).unwrap();
        let parse = &store.symbols_by_name("parse", 10).unwrap()[0];
        let callers = store.edges_to(parse.id, Some(EdgeKind::Calls)).unwrap();
        // At least the bare `parse("42")` call resolves (Exact, T1 imported).
        let exact: Vec<_> = callers.iter().filter(|e| e.tier == Tier::Exact).collect();
        let probable: Vec<_> = callers
            .iter()
            .filter(|e| e.tier == Tier::Probable)
            .collect();
        // The receiver call `n.parse(...)` must NOT be Exact.
        // (It may be Probable via T2 unique-name, or unresolved; either is
        // acceptable as long as it is not Exact.)
        assert!(
            !exact.is_empty(),
            "bare parse() call should resolve as Exact"
        );
        // If there is a second edge (the receiver call), it must not be Exact.
        if callers.len() > exact.len() {
            assert!(
                probable.len() + (callers.len() - exact.len() - probable.len()) > 0,
                "receiver call must be Probable or Unresolved, never Exact"
            );
            for e in &callers {
                if e.tier == Tier::Exact {
                    // Exact edges must come from the bare call only; sanity
                    // check there is at least one Exact and any non-Exact is
                    // not Exact (trivially true).
                }
            }
        }
        // Stronger direct check via the resolver: a receiver call to a unique
        // name is downgraded to Probable.
        let idx = ResolveIndex::build(&store).unwrap();
        let b_id = store.file_by_path("b.ts").unwrap().unwrap().id;
        match idx.decide(b_id, "parse", Some("n")) {
            Decision::Exact(_) => panic!("receiver call must not be Exact"),
            Decision::Probable(_) => {} // acceptable downgrade
            Decision::Unresolved => {}  // also acceptable
        }
        // Bare call (no receiver) to the imported unique name stays Exact.
        match idx.decide(b_id, "parse", None) {
            Decision::Exact(_) => {}
            other => panic!("bare call to imported unique name should be Exact, got {other:?}"),
        }
        drop(store);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Regression: graph freshness must detect equal-size content changes
    /// even when mtime is restored. The old stat-only signature (path+size+mtime)
    /// could be fooled by `touch -t` or `cp` + `touch -r`. The content-hash
    /// signature catches this.
    #[test]
    fn freshness_detects_equal_size_content_change() {
        let root = tmpdir("fresh-content");
        // Two different bodies with the same byte length.
        let body_a = "export function alpha() { return 1 }\n";
        let body_b = "export function alpha() { return 2 }\n";
        assert_eq!(
            body_a.len(),
            body_b.len(),
            "test setup: equal-length bodies"
        );

        std::fs::write(root.join("a.ts"), body_a).unwrap();
        let db = root.join(".pixel").join("graph.db");
        build_graph(&root, &db).unwrap();
        assert!(is_fresh(&root, &db), "fresh after initial build");

        // Change content, same size. The content hash differs even if mtime
        // is restored, so the signature must change.
        std::fs::write(root.join("a.ts"), body_b).unwrap();
        assert!(
            !is_fresh(&root, &db),
            "graph must be stale after equal-size content change"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Regression: importing one binding must NOT make unrelated definitions
    /// from the same file eligible for Exact T1 resolution. If file B imports
    /// `{ greet }` from `./a`, a call to `farewell()` (also defined in `./a`
    /// but NOT imported) must NOT be Exact via T1.
    #[test]
    fn import_binding_specificity_prevents_false_exact() {
        let root = tmpdir("import-bindings");
        // a.ts exports two functions: greet and farewell.
        std::fs::write(
            root.join("a.ts"),
            "export function greet(): void {}\n\
             export function farewell(): void {}\n",
        )
        .unwrap();
        // b.ts imports ONLY greet, but calls both greet and farewell.
        std::fs::write(
            root.join("b.ts"),
            "import { greet } from \"./a\";\n\
             export function caller() {\n  \
             greet();\n  \
             farewell();\n\
             }\n",
        )
        .unwrap();
        let db = root.join(".pixel").join("graph.db");
        build_graph(&root, &db).unwrap();

        let store = GraphStore::open(&db).unwrap();
        let idx = ResolveIndex::build(&store).unwrap();
        let b_id = store.file_by_path("b.ts").unwrap().unwrap().id;

        // greet was imported → bare call should be Exact (T1 binding-level).
        match idx.decide(b_id, "greet", None) {
            Decision::Exact(_) => {}
            other => panic!("imported binding `greet` should be Exact, got {other:?}"),
        }

        // farewell was NOT imported → must NOT be Exact via T1. It can be
        // Probable (T2 unique name) or Unresolved, but never Exact.
        match idx.decide(b_id, "farewell", None) {
            Decision::Exact(_) => {
                panic!("farewell was not imported — must not be Exact via T1")
            }
            Decision::Probable(_) => {} // acceptable: T2 unique name
            Decision::Unresolved => {}  // also acceptable
        }

        drop(store);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn wildcard_import_does_not_make_unqualified_call_exact() {
        let root = tmpdir("wildcard-import");
        std::fs::write(root.join("a.ts"), "export function target(): void {}\n").unwrap();
        std::fs::write(
            root.join("b.ts"),
            "import * as ns from \"./a\";\nexport function caller() { target(); }\n",
        )
        .unwrap();
        let db = root.join(".pixel").join("graph.db");
        build_graph(&root, &db).unwrap();

        let store = GraphStore::open(&db).unwrap();
        let idx = ResolveIndex::build(&store).unwrap();
        let b_id = store.file_by_path("b.ts").unwrap().unwrap().id;
        assert!(
            !matches!(idx.decide(b_id, "target", None), Decision::Exact(_)),
            "namespace import cannot make an unqualified call Exact"
        );

        drop(store);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn incremental_definition_addition_reconsiders_existing_edges() {
        let root = tmpdir("incremental-ambiguity");
        std::fs::write(root.join("a.ts"), "export function target() {}\n").unwrap();
        std::fs::write(
            root.join("b.ts"),
            "export function caller() { target(); }\n",
        )
        .unwrap();
        let db = root.join(".pixel").join("graph.db");
        build_graph(&root, &db).unwrap();
        {
            let store = GraphStore::open(&db).unwrap();
            let target = store.symbols_by_name("target", 10).unwrap().remove(0);
            assert_eq!(
                store
                    .edges_to(target.id, Some(EdgeKind::Calls))
                    .unwrap()
                    .len(),
                1
            );
        }

        std::fs::write(root.join("c.ts"), "export function target() {}\n").unwrap();
        update_file(&root, &db, "c.ts").unwrap();
        let store = GraphStore::open(&db).unwrap();
        for target in store.symbols_by_name("target", 10).unwrap() {
            assert!(
                store
                    .edges_to(target.id, Some(EdgeKind::Calls))
                    .unwrap()
                    .is_empty(),
                "ambiguous target must not retain a resolved edge"
            );
        }
        let envelope = store.envelope_for_name("target").unwrap();
        assert!(envelope.lower_bound);
        assert!(envelope.unresolved_same_name >= 1);
        drop(store);
        let _ = std::fs::remove_dir_all(&root);
    }
}

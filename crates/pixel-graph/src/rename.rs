//! `pixel rename` — IDE-style symbol rename, graph-driven and tree-sitter
//! verified.
//!
//! The graph says WHERE a symbol is named (its declaration line, every
//! resolved call/reference `site_line`, the import bindings that pull it out
//! of its file). For each candidate site the file is re-parsed and the
//! identifier's *role* is verified before its bytes are touched — a name the
//! graph asserted sits on a line but that the parse finds only inside a
//! comment, a string, or an alias target is skipped, not rewritten. Sites the
//! graph could not resolve (`unresolved_calls` carrying the old name) are
//! reported in the plan so the caller sees the honest boundary of the rename.
//!
//! No regex replaces, no whole-word text scan: two same-named symbols in one
//! file only collide if the graph itself confused them.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Serialize;
use tree_sitter::{Node, Tree};

use crate::extract;
use crate::store::{EdgeKind, GraphStore, SymbolRow};

/// Why a site is being rewritten — the graph evidence that nominated it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SiteKind {
    /// The symbol's own declaration name.
    Definition,
    /// A resolved `Calls` edge's callee identifier.
    Call,
    /// A resolved `References` edge (passed-as-value uses).
    Reference,
    /// The imported name inside a `use`/`import` binding.
    Import,
}

impl SiteKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Definition => "definition",
            Self::Call => "call",
            Self::Reference => "reference",
            Self::Import => "import",
        }
    }
}

/// One verified identifier rewrite: byte range plus the 1-based line the
/// graph pointed at.
#[derive(Debug, Clone, Serialize)]
pub struct RenameEdit {
    pub line: u32,
    pub start_byte: usize,
    pub end_byte: usize,
    pub kind: SiteKind,
}

/// A site the rename refused to guess at.
#[derive(Debug, Clone, Serialize)]
pub struct RenameSkip {
    pub path: String,
    pub line: u32,
    pub reason: String,
}

/// Per-file edit list plus the sites that were reported instead of rewritten.
#[derive(Debug, Default, Serialize)]
pub struct RenamePlan {
    /// path -> verified edits for that file.
    pub files: BTreeMap<String, Vec<RenameEdit>>,
    pub skipped: Vec<RenameSkip>,
    /// Occurrences of the old name the graph never claimed (comments,
    /// strings, macro text, dynamic dispatch): counted per file so the
    /// answer admits they exist instead of looking byte-clean.
    pub unclaimed_text: BTreeMap<String, u32>,
}

/// A candidate site the graph nominated, before parse verification.
struct Site {
    /// Repo-relative path.
    path: String,
    line: u32,
    kind: SiteKind,
    /// For `Import`: the import's spec text, used to find the right
    /// statement when a file has several.
    spec: Option<String>,
}

/// Compute the verified edit set for renaming `sym` to `new_name`.
///
/// `new_name` must already be a plausible identifier (the caller validates);
/// this only checks it differs and does not collide with a sibling symbol of
/// the same name in the same file+kind (which would silently merge two
/// declarations).
pub fn plan(
    store: &GraphStore,
    root: &Path,
    sym: &SymbolRow,
    new_name: &str,
) -> Result<RenamePlan, String> {
    if sym.name == new_name {
        return Err("rename: new name is the old name".to_string());
    }
    let files = file_paths(store)?;
    let def_path = files
        .get(&sym.file_id)
        .cloned()
        .ok_or_else(|| "rename: symbol's file is not in the graph".to_string())?;

    // A same-named symbol in the same file makes the rename self-colliding:
    // after the write both declarations share a name and one file.
    let collision = store
        .symbols_in_file(sym.file_id)
        .map_err(|e| e.to_string())?
        .into_iter()
        .any(|s| s.id != sym.id && s.name == new_name);
    if collision {
        return Err(format!(
            "rename: {def_path} already declares a {new_name:?} — renaming would collide"
        ));
    }

    let mut sites: Vec<Site> = vec![Site {
        path: def_path.clone(),
        line: sym.start_line,
        kind: SiteKind::Definition,
        spec: None,
    }];

    for kind in [EdgeKind::Calls, EdgeKind::References] {
        let site_kind = if kind == EdgeKind::Calls {
            SiteKind::Call
        } else {
            SiteKind::Reference
        };
        for edge in store
            .edges_to(sym.id, Some(kind))
            .map_err(|e| e.to_string())?
        {
            // The edge names the enclosing symbol; its file holds the site.
            let Some(src) = symbol_by_id(store, edge.src_id) else {
                continue;
            };
            let Some(path) = files.get(&src.file_id) else {
                continue;
            };
            sites.push(Site {
                path: path.clone(),
                line: edge.site_line,
                kind: site_kind,
                spec: None,
            });
        }
    }

    for import in store
        .imports_to_file(sym.file_id)
        .map_err(|e| e.to_string())?
    {
        if !import.bindings.iter().any(|b| b == &sym.name) {
            continue;
        }
        let Some(path) = files.get(&import.file_id) else {
            continue;
        };
        sites.push(Site {
            path: path.clone(),
            line: 0, // located by spec text, not a stored line
            kind: SiteKind::Import,
            spec: Some(import.spec),
        });
    }

    let mut plan = RenamePlan::default();
    for u in store
        .unresolved_named(&sym.name)
        .map_err(|e| e.to_string())?
    {
        if let Some(path) = files.get(&u.file_id) {
            plan.skipped.push(RenameSkip {
                path: path.clone(),
                line: u.site_line,
                reason: format!(
                    "unresolved {}: the graph could not prove this is the renamed symbol",
                    u.kind
                ),
            });
        }
    }

    // Group sites by file, then verify each against a fresh parse.
    let mut by_file: BTreeMap<String, Vec<Site>> = BTreeMap::new();
    for site in sites {
        by_file.entry(site.path.clone()).or_default().push(site);
    }
    for (path, sites) in by_file {
        let abs = root.join(&path);
        let content = match std::fs::read(&abs) {
            Ok(c) => c,
            Err(e) => {
                for s in &sites {
                    plan.skipped.push(RenameSkip {
                        path: path.clone(),
                        line: s.line,
                        reason: format!("file unreadable: {e}"),
                    });
                }
                continue;
            }
        };
        let Some(tree) = extract::parse_file(&path, &content) else {
            for s in &sites {
                plan.skipped.push(RenameSkip {
                    path: path.clone(),
                    line: s.line,
                    reason: "file does not parse with its extension's grammar".to_string(),
                });
            }
            continue;
        };
        let mut edits = Vec::new();
        for site in sites {
            verify_site(&tree, &content, &site, sym, &mut edits, &mut plan, &path);
        }
        // Residual same-name bytes the graph never claimed (comments,
        // strings, macro bodies): counted, not touched.
        let leftover = count_unclaimed_text(&content, &sym.name, &edits);
        if leftover > 0 {
            plan.unclaimed_text.insert(path.clone(), leftover);
        }
        if !edits.is_empty() {
            edits.sort_by_key(|e| e.start_byte);
            edits.dedup_by_key(|e| (e.start_byte, e.end_byte));
            plan.files.insert(path, edits);
        }
    }
    Ok(plan)
}

/// Apply a plan: rewrite each file's verified byte ranges, ascending file
/// order and descending byte order inside a file so earlier rewrites never
/// shift later offsets. Returns the paths written.
pub fn apply(
    root: &Path,
    plan: &RenamePlan,
    old_name: &str,
    new_name: &str,
) -> Result<Vec<String>, String> {
    let mut written = Vec::new();
    for (path, edits) in &plan.files {
        let abs = root.join(path);
        let mut content =
            std::fs::read(&abs).map_err(|e| format!("rename: cannot read {path}: {e}"))?;
        for edit in edits.iter().rev() {
            // Re-verify before touching bytes: the file may have changed
            // between plan and apply (watcher-driven reindex, a user edit).
            if content.get(edit.start_byte..edit.end_byte) != Some(old_name.as_bytes()) {
                return Err(format!(
                    "rename: {path}:{} no longer holds {old_name:?} — file changed under the rename",
                    edit.line
                ));
            }
            content.splice(
                edit.start_byte..edit.end_byte,
                new_name.as_bytes().iter().copied(),
            );
        }
        std::fs::write(&abs, &content).map_err(|e| format!("rename: cannot write {path}: {e}"))?;
        written.push(path.clone());
    }
    Ok(written)
}

/// Verify one nominated site against the fresh parse and push its edits.
fn verify_site(
    tree: &Tree,
    content: &[u8],
    site: &Site,
    sym: &SymbolRow,
    edits: &mut Vec<RenameEdit>,
    plan: &mut RenamePlan,
    path: &str,
) {
    let skip = |plan: &mut RenamePlan, reason: &str| {
        plan.skipped.push(RenameSkip {
            path: path.to_string(),
            line: site.line,
            reason: reason.to_string(),
        });
    };
    match site.kind {
        SiteKind::Import => {
            let Some(spec) = &site.spec else { return };
            let found = import_name_nodes(tree, content, spec, &sym.name);
            if found.is_empty() {
                skip(
                    plan,
                    "import binding not found in a matching statement (stale graph?)",
                );
                return;
            }
            for node in found {
                edits.push(edit_of(node, site));
            }
        }
        _ => {
            let candidates = identifier_nodes_on_line(tree, content, site.line, &sym.name);
            let chosen = match site.kind {
                SiteKind::Definition => pick_definition(&candidates),
                SiteKind::Call => pick_callee(&candidates),
                SiteKind::Reference => pick_reference(&candidates),
                SiteKind::Import => unreachable!(),
            };
            if chosen.is_empty() {
                skip(
                    plan,
                    "no verified identifier for the asserted site (comment/string/stale graph)",
                );
                return;
            }
            for node in chosen {
                edits.push(edit_of(node, site));
            }
        }
    }
}

fn edit_of(node: Node, site: &Site) -> RenameEdit {
    RenameEdit {
        line: site.line,
        start_byte: node.start_byte(),
        end_byte: node.end_byte(),
        kind: site.kind,
    }
}

/// A node names the symbol textually: named, identifier-ish, exact text.
fn is_identifier_named(node: Node, name: &str, content: &[u8]) -> bool {
    if !node.is_named() {
        return false;
    }
    let kind = node.kind();
    let identifierish = kind.ends_with("identifier")
        || matches!(
            kind,
            "constant" | "simple_identifier" | "alias" | "name" | "property_identifier_pattern"
        );
    identifierish && node.utf8_text(content).is_ok_and(|t| t == name)
}

/// True when an ancestor is a comment or string — bytes that are text, not
/// code, and must never be rewritten as a reference.
fn in_text_node(node: Node) -> bool {
    let mut cur = node;
    loop {
        let kind = cur.kind();
        if kind.contains("comment")
            || kind.contains("string")
            || kind == "interpreted_string_literal"
        {
            return true;
        }
        match cur.parent() {
            Some(p) => cur = p,
            None => return false,
        }
    }
}

/// Every named identifier-ish node on `line` (1-based) whose text is `name`,
/// excluding comment/string interiors.
fn identifier_nodes_on_line<'t>(
    tree: &'t Tree,
    content: &[u8],
    line: u32,
    name: &str,
) -> Vec<Node<'t>> {
    let row = line.saturating_sub(1) as usize;
    let mut out = Vec::new();
    collect_line_nodes(tree.root_node(), content, row, name, &mut out);
    out
}

fn collect_line_nodes<'t>(
    node: Node<'t>,
    content: &[u8],
    row: usize,
    name: &str,
    out: &mut Vec<Node<'t>>,
) {
    // Prune: a node whose line span excludes `row` cannot contain a site.
    if node.start_position().row > row || node.end_position().row < row {
        return;
    }
    if is_identifier_named(node, name, content)
        && node.start_position().row == row
        && !in_text_node(node)
    {
        out.push(node);
    }
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            collect_line_nodes(child, content, row, name, out);
        }
    }
}

/// The declaration's own name: the node that is the `name`/`declarator`
/// field of a declaration-shaped ancestor. Falls back to a single
/// candidate inside a declaration-shaped parent when grammars use other
/// field names.
fn pick_definition<'t>(candidates: &[Node<'t>]) -> Vec<Node<'t>> {
    for &node in candidates {
        if let Some(parent) = node.parent() {
            for field in ["name", "declarator", "left"] {
                if parent.child_by_field_name(field) == Some(node) {
                    return vec![node];
                }
            }
        }
    }
    let decl_like: Vec<Node> = candidates
        .iter()
        .copied()
        .filter(|n| {
            n.parent().is_some_and(|p| {
                let k = p.kind();
                k.contains("declaration")
                    || k.contains("definition")
                    || k.contains("_item")
                    || k.contains("declarator")
                    || k.contains("spec")
                    || k.contains("assignment")
            })
        })
        .collect();
    if decl_like.len() == 1 {
        decl_like
    } else {
        Vec::new()
    }
}

/// The callee of a call on this line: a node that IS its parent's
/// function/name/field/property/attribute/method field, or the first named
/// child of a call-shaped parent.
fn pick_callee<'t>(candidates: &[Node<'t>]) -> Vec<Node<'t>> {
    let callees: Vec<Node> = candidates
        .iter()
        .copied()
        .filter(|n| is_callee_position(*n))
        .collect();
    if !callees.is_empty() {
        return callees;
    }
    // The graph asserted a call here; a lone same-name identifier is that
    // call (covers grammars whose callee field is not in the table).
    if candidates.len() == 1 {
        return vec![candidates[0]];
    }
    Vec::new()
}

fn is_callee_position(node: Node) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    for field in [
        "function",
        "name",
        "field",
        "property",
        "attribute",
        "method",
        "type_arguments",
    ] {
        if parent.child_by_field_name(field) == Some(node) {
            return true;
        }
    }
    let pk = parent.kind();
    if (pk.contains("call") || pk.contains("invocation")) && parent.named_child(0) == Some(node) {
        return true;
    }
    // `recv.name(...)`: the field/property half of a member access whose
    // own parent is the call's function.
    if (pk.contains("member") || pk.contains("selector") || pk.contains("field_expression"))
        && parent
            .parent()
            .is_some_and(|gp| gp.child_by_field_name("function") == Some(parent))
        && parent.named_children(&mut parent.walk()).last() == Some(node)
    {
        return true;
    }
    false
}

/// A passed-as-value reference: identifier in expression position that is
/// neither a declaration name nor the callee the Calls edge already owns.
/// Every same-named identifier on the asserted line in use position is a
/// reference to the renamed symbol.
fn pick_reference<'t>(candidates: &[Node<'t>]) -> Vec<Node<'t>> {
    let refs: Vec<Node> = candidates
        .iter()
        .copied()
        .filter(|n| !is_declaration_name(*n))
        .collect();
    if refs.len() == candidates.len() && !refs.is_empty() {
        return refs;
    }
    // Mixed line (declaration + use of the same name): keep only uses.
    refs
}

fn is_declaration_name(node: Node) -> bool {
    node.parent().is_some_and(|p| {
        p.child_by_field_name("name") == Some(node)
            && (p.kind().contains("declaration")
                || p.kind().contains("definition")
                || p.kind().contains("_item"))
    })
}

/// Inside the file's import statements that carry `spec`, the identifier
/// nodes naming `name` in binding position — never the `as` alias, which is
/// the importer's local name and stays valid as-is.
fn import_name_nodes<'t>(tree: &'t Tree, content: &[u8], spec: &str, name: &str) -> Vec<Node<'t>> {
    let mut out = Vec::new();
    collect_import_nodes(tree.root_node(), content, spec, name, &mut out);
    out
}

fn collect_import_nodes<'t>(
    node: Node<'t>,
    content: &[u8],
    spec: &str,
    name: &str,
    out: &mut Vec<Node<'t>>,
) {
    let k = node.kind();
    let is_import = k.contains("import")
        || k == "use_declaration"
        || k.contains("using_directive")
        || (k == "export_statement" && node.utf8_text(content).is_ok_and(|t| t.contains("from")));
    if is_import && node.utf8_text(content).is_ok_and(|t| t.contains(spec)) {
        // Within this statement, rewrite name-position identifiers matching
        // the old name; alias-position nodes (the `as X` target) keep the
        // importer's local name.
        collect_binding_names(node, content, name, out);
        return;
    }
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            collect_import_nodes(child, content, spec, name, out);
        }
    }
}

fn collect_binding_names<'t>(node: Node<'t>, content: &[u8], name: &str, out: &mut Vec<Node<'t>>) {
    if is_identifier_named(node, name, content) && !in_text_node(node) && !is_alias_position(node) {
        out.push(node);
    }
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            collect_binding_names(child, content, name, out);
        }
    }
}

/// `foo as bar` — `bar` is the alias the importer chose; it is a different
/// name, not a reference to the renamed symbol.
fn is_alias_position(node: Node) -> bool {
    node.parent().is_some_and(|p| {
        p.child_by_field_name("alias") == Some(node)
            || (p.kind().contains("alias") && p.named_children(&mut p.walk()).last() == Some(node))
    })
}

/// Whole-word occurrences of `name` in `content` outside the verified edit
/// ranges — comments, doc text, macro bodies. Byte-level and honest.
fn count_unclaimed_text(content: &[u8], name: &str, edits: &[RenameEdit]) -> u32 {
    let is_word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut count = 0u32;
    let mut i = 0usize;
    let n = name.as_bytes();
    while i + n.len() <= content.len() {
        if &content[i..i + n.len()] == n
            && (i == 0 || !is_word(content[i - 1]))
            && (i + n.len() == content.len() || !is_word(content[i + n.len()]))
            && !edits
                .iter()
                .any(|e| i >= e.start_byte && i + n.len() <= e.end_byte)
        {
            count += 1;
            i += n.len();
        } else {
            i += 1;
        }
    }
    count
}

fn file_paths(store: &GraphStore) -> Result<BTreeMap<i64, String>, String> {
    Ok(store
        .files()
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|f| (f.id, f.path))
        .collect())
}

fn symbol_by_id(store: &GraphStore, id: i64) -> Option<SymbolRow> {
    store
        .conn()
        .query_row(
            "SELECT id, uid, file_id, name, qualified, kind, start_line, end_line, sig
             FROM symbols WHERE id = ?1",
            rusqlite::params![id],
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
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Word-boundary counting must not match inside a longer identifier.
    #[test]
    fn unclaimed_text_respects_word_boundaries() {
        let content = b"foo foo_bar bar_foo foo\n".as_slice();
        assert_eq!(count_unclaimed_text(content, "foo", &[]), 2);
        let edits = vec![RenameEdit {
            line: 1,
            start_byte: 0,
            end_byte: 3,
            kind: SiteKind::Definition,
        }];
        assert_eq!(count_unclaimed_text(content, "foo", &edits), 1);
    }

    /// The verified edit ranges are applied back-to-front so a same-line
    /// earlier rewrite cannot shift a later one.
    #[test]
    fn apply_writes_ranges_descending() {
        let dir = std::env::temp_dir().join(format!("pixel-rename-apply-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.rs"), b"fn foo() {}\nfn bar() { foo() }\n").unwrap();
        let mut plan = RenamePlan::default();
        plan.files.insert(
            "a.rs".to_string(),
            vec![
                RenameEdit {
                    line: 1,
                    start_byte: 3,
                    end_byte: 6,
                    kind: SiteKind::Definition,
                },
                RenameEdit {
                    line: 2,
                    start_byte: 23,
                    end_byte: 26,
                    kind: SiteKind::Call,
                },
            ],
        );
        let written = apply(&dir, &plan, "foo", "baz").unwrap();
        assert_eq!(written, vec!["a.rs".to_string()]);
        assert_eq!(
            std::fs::read(dir.join("a.rs")).unwrap(),
            b"fn baz() {}\nfn bar() { baz() }\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A file changed between plan and apply is refused, not half-rewritten.
    #[test]
    fn apply_refuses_a_file_that_moved() {
        let dir = std::env::temp_dir().join(format!("pixel-rename-moved-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.rs"), b"fn foo() {}\n").unwrap();
        let mut plan = RenamePlan::default();
        plan.files.insert(
            "a.rs".to_string(),
            vec![RenameEdit {
                line: 1,
                start_byte: 3,
                end_byte: 6,
                kind: SiteKind::Definition,
            }],
        );
        std::fs::write(dir.join("a.rs"), b"fn qux() {}\n").unwrap();
        assert!(apply(&dir, &plan, "foo", "baz").is_err());
        assert_eq!(std::fs::read(dir.join("a.rs")).unwrap(), b"fn qux() {}\n");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

//! Per-file tree-sitter extraction: symbols, call sites, import specs.
//!
//! Pragmatic node-kind walks per language family. Any grammar failure
//! degrades gracefully to `None` — a file we cannot parse simply
//! contributes nothing to the graph.

use std::panic::AssertUnwindSafe;

use tree_sitter::{Language, Node, Parser};

use crate::store::SymbolKind;

#[derive(Debug, Clone)]
pub struct RawSymbol {
    pub name: String,
    pub qualified: String,
    pub kind: SymbolKind,
    pub start_line: u32,
    pub end_line: u32,
    pub sig: String,
}

#[derive(Debug, Clone)]
pub struct RawCall {
    pub callee_name: String,
    pub receiver: Option<String>,
    pub site_line: u32,
    /// Index into `FileExtraction::symbols` of the smallest enclosing symbol.
    pub enclosing_index: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct RawImport {
    pub spec: String,
    /// Named bindings imported from this spec (e.g. `["greet", "farewell"]`
    /// for `import { greet, farewell } from "./a"`). Empty for wildcard
    /// imports (`import * as x`) or when bindings cannot be extracted. Empty
    /// bindings never grant Exact import-tier confidence.
    pub bindings: Vec<String>,
}

#[derive(Debug)]
pub struct FileExtraction {
    pub lang: &'static str,
    pub symbols: Vec<RawSymbol>,
    pub calls: Vec<RawCall>,
    pub imports: Vec<RawImport>,
}

/// Language tag for a repo-relative path, or `None` if unsupported.
pub fn lang_of(path: &str) -> Option<&'static str> {
    let file = path.rsplit('/').next().unwrap_or(path);
    let ext = file.rsplit_once('.')?.1;
    match ext {
        "ts" | "mts" | "cts" => Some("ts"),
        "tsx" => Some("tsx"),
        "js" | "jsx" | "mjs" | "cjs" => Some("js"),
        "rs" => Some("rust"),
        "go" => Some("go"),
        "java" => Some("java"),
        "py" => Some("python"),
        _ => None,
    }
}

/// Extract symbols/calls/imports from one file. `None` on unsupported
/// language or any parse/grammar failure.
pub fn extract_file(path_rel: &str, content: &[u8]) -> Option<FileExtraction> {
    let lang = lang_of(path_rel)?;
    std::panic::catch_unwind(AssertUnwindSafe(|| extract_inner(lang, content)))
        .ok()
        .flatten()
}

fn language_for(lang: &str) -> Option<Language> {
    Some(match lang {
        "ts" => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        "tsx" => tree_sitter_typescript::LANGUAGE_TSX.into(),
        "js" => tree_sitter_javascript::LANGUAGE.into(),
        "rust" => tree_sitter_rust::LANGUAGE.into(),
        "go" => tree_sitter_go::LANGUAGE.into(),
        "java" => tree_sitter_java::LANGUAGE.into(),
        "python" => tree_sitter_python::LANGUAGE.into(),
        _ => return None,
    })
}

fn extract_inner(lang: &'static str, content: &[u8]) -> Option<FileExtraction> {
    let language = language_for(lang)?;
    let mut parser = Parser::new();
    parser.set_language(&language).ok()?;
    let tree = parser.parse(content, None)?;
    let mut w = Walker {
        src: content,
        symbols: Vec::new(),
        calls: Vec::new(),
        imports: Vec::new(),
        stack: Vec::new(),
    };
    let root = tree.root_node();
    match lang {
        "ts" | "tsx" | "js" => walk_ts(&mut w, root, 0),
        "rust" => walk_rust(&mut w, root, 0),
        "go" => walk_go(&mut w, root, 0),
        "java" => walk_java(&mut w, root, 0),
        "python" => walk_python(&mut w, root, 0),
        _ => return None,
    }
    let mut fx = FileExtraction {
        lang,
        symbols: w.symbols,
        calls: w.calls,
        imports: w.imports,
    };
    assign_enclosing(&mut fx);
    Some(fx)
}

/// Smallest symbol whose line range contains the call site.
fn assign_enclosing(fx: &mut FileExtraction) {
    for call in &mut fx.calls {
        let mut best: Option<(usize, u32)> = None;
        for (i, s) in fx.symbols.iter().enumerate() {
            if s.start_line <= call.site_line && call.site_line <= s.end_line {
                let span = s.end_line - s.start_line;
                if best.map(|(_, b)| span < b).unwrap_or(true) {
                    best = Some((i, span));
                }
            }
        }
        call.enclosing_index = best.map(|(i, _)| i);
    }
}

const MAX_DEPTH: usize = 512;
const SIG_CAP: usize = 200;

struct Walker<'a> {
    src: &'a [u8],
    symbols: Vec<RawSymbol>,
    calls: Vec<RawCall>,
    imports: Vec<RawImport>,
    /// Enclosing type names (class/impl/trait) for qualification.
    stack: Vec<String>,
}

impl<'a> Walker<'a> {
    fn text(&self, n: Node) -> String {
        String::from_utf8_lossy(&self.src[n.byte_range()]).into_owned()
    }

    fn sig(&self, n: Node) -> String {
        let raw = &self.src[n.byte_range()];
        let first = raw.split(|&b| b == b'\n').next().unwrap_or(raw);
        let s = String::from_utf8_lossy(first);
        let t = s.trim();
        if t.len() > SIG_CAP {
            let mut cut = SIG_CAP;
            while cut > 0 && !t.is_char_boundary(cut) {
                cut -= 1;
            }
            t[..cut].to_string()
        } else {
            t.to_string()
        }
    }

    fn qualify(&self, name: &str, sep: &str) -> String {
        if self.stack.is_empty() {
            name.to_string()
        } else {
            format!("{}{}{}", self.stack.join(sep), sep, name)
        }
    }

    fn push_symbol(&mut self, name: String, qualified: String, kind: SymbolKind, node: Node) {
        if name.is_empty() {
            return;
        }
        self.symbols.push(RawSymbol {
            sig: self.sig(node),
            start_line: line_start(node),
            end_line: line_end(node),
            name,
            qualified,
            kind,
        });
    }

    fn push_call(&mut self, callee: String, receiver: Option<String>, node: Node) {
        if callee.is_empty() {
            return;
        }
        self.calls.push(RawCall {
            callee_name: callee,
            receiver,
            site_line: line_start(node),
            enclosing_index: None,
        });
    }

    fn push_import(&mut self, spec: String, bindings: Vec<String>) {
        if !spec.is_empty() {
            self.imports.push(RawImport { spec, bindings });
        }
    }
}

fn line_start(n: Node) -> u32 {
    n.start_position().row as u32 + 1
}
fn line_end(n: Node) -> u32 {
    n.end_position().row as u32 + 1
}

fn field_text(w: &Walker, n: Node, field: &str) -> Option<String> {
    n.child_by_field_name(field).map(|c| w.text(c))
}

fn strip_quotes(s: &str) -> String {
    s.trim_matches(|c| c == '"' || c == '\'' || c == '`')
        .to_string()
}

fn each_child<'t>(n: Node<'t>) -> Vec<Node<'t>> {
    let mut cursor = n.walk();
    n.children(&mut cursor).collect()
}

// --- TypeScript / TSX / JavaScript ---------------------------------------

fn walk_ts(w: &mut Walker, node: Node, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    let mut pushed = false;
    match node.kind() {
        "function_declaration" | "generator_function_declaration" => {
            if let Some(name) = field_text(w, node, "name") {
                let q = w.qualify(&name, ".");
                w.push_symbol(name, q, SymbolKind::Function, node);
            }
        }
        "class_declaration" | "abstract_class_declaration" => {
            if let Some(name) = field_text(w, node, "name") {
                let q = w.qualify(&name, ".");
                w.push_symbol(name.clone(), q, SymbolKind::Class, node);
                w.stack.push(name);
                pushed = true;
            }
        }
        "interface_declaration" => {
            if let Some(name) = field_text(w, node, "name") {
                let q = w.qualify(&name, ".");
                w.push_symbol(name, q, SymbolKind::Interface, node);
            }
        }
        "enum_declaration" => {
            if let Some(name) = field_text(w, node, "name") {
                let q = w.qualify(&name, ".");
                w.push_symbol(name, q, SymbolKind::Enum, node);
            }
        }
        "method_definition" => {
            if let Some(name) = field_text(w, node, "name") {
                let q = w.qualify(&name, ".");
                w.push_symbol(name, q, SymbolKind::Method, node);
            }
        }
        "variable_declarator" => {
            let is_fn = node
                .child_by_field_name("value")
                .map(|v| {
                    matches!(
                        v.kind(),
                        "arrow_function" | "function_expression" | "function"
                    )
                })
                .unwrap_or(false);
            if is_fn
                && let Some(name) = field_text(w, node, "name")
                && !name.contains(['{', '['])
            {
                let q = w.qualify(&name, ".");
                w.push_symbol(name, q, SymbolKind::Function, node);
            }
        }
        "call_expression" => {
            if let Some(f) = node.child_by_field_name("function") {
                match f.kind() {
                    "identifier" => {
                        let name = w.text(f);
                        w.push_call(name, None, node);
                    }
                    "member_expression" => {
                        if let Some(prop) = field_text(w, f, "property") {
                            let recv = field_text(w, f, "object");
                            w.push_call(prop, recv, node);
                        }
                    }
                    _ => {}
                }
            }
        }
        "new_expression" => {
            if let Some(c) = node.child_by_field_name("constructor")
                && c.kind() == "identifier"
            {
                let name = w.text(c);
                w.push_call(name, None, node);
            }
        }
        "import_statement" | "export_statement" => {
            if let Some(src) = node.child_by_field_name("source") {
                let spec = strip_quotes(&w.text(src));
                let bindings = ts_import_bindings(w, node);
                w.push_import(spec, bindings);
            }
        }
        _ => {}
    }
    for child in each_child(node) {
        walk_ts(w, child, depth + 1);
    }
    if pushed {
        w.stack.pop();
    }
}

/// Extract named import bindings from a TS/JS `import_statement` or
/// `export_statement ... from "..."`. Handles:
/// - `import { greet, farewell } from "./a"` → `["greet", "farewell"]`
/// - `import greet from "./a"` → `["greet"]` (default import)
/// - `import * as ns from "./a"` → `[]` (wildcard — no tracked bindings)
/// - `import greet, { helper } from "./a"` → `["greet", "helper"]`
///
/// Returns empty for wildcard imports and unparseable forms; T1 then falls
/// back to file-level matching (the safe, pre-fix behavior).
fn ts_import_bindings(w: &Walker, node: Node) -> Vec<String> {
    let mut bindings = Vec::new();
    for child in each_child(node) {
        match child.kind() {
            // Named imports: `import { greet, farewell as f } from "./a"`
            "import_clause" => {
                for sub in each_child(child) {
                    match sub.kind() {
                        "named_imports" => {
                            for spec in each_child(sub) {
                                if spec.kind() == "import_specifier"
                                    && let Some(name) = sub_field_text(w, spec, "name")
                                {
                                    bindings.push(name);
                                }
                            }
                        }
                        // Default import: `import greet from "./a"`
                        "identifier" => {
                            let name = w.text(sub);
                            if !name.is_empty() {
                                bindings.push(name);
                            }
                        }
                        // Wildcard: `import * as ns` — no tracked bindings.
                        "namespace_import" | "import_namespace_clause" => {
                            return Vec::new();
                        }
                        _ => {}
                    }
                }
            }
            // Re-export: `export { greet } from "./a"`
            "export_clause" => {
                for spec in each_child(child) {
                    if spec.kind() == "export_specifier"
                        && let Some(name) = sub_field_text(w, spec, "name")
                    {
                        bindings.push(name);
                    }
                }
            }
            _ => {}
        }
    }
    bindings
}

fn sub_field_text(w: &Walker, node: Node, field: &str) -> Option<String> {
    let child = node.child_by_field_name(field)?;
    let text = w.text(child);
    if text.is_empty() { None } else { Some(text) }
}

// --- Rust ----------------------------------------------------------------

fn walk_rust(w: &mut Walker, node: Node, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    if rust_is_test_container(w, node) {
        return;
    }
    let mut pushed = false;
    match node.kind() {
        "function_item" => {
            if let Some(name) = field_text(w, node, "name") {
                let (kind, q) = if w.stack.is_empty() {
                    (SymbolKind::Function, name.clone())
                } else {
                    (SymbolKind::Method, w.qualify(&name, "::"))
                };
                w.push_symbol(name, q, kind, node);
            }
        }
        "impl_item" => {
            if let Some(ty) = field_text(w, node, "type") {
                let base = ty.split('<').next().unwrap_or(&ty).trim().to_string();
                w.stack.push(base);
                pushed = true;
            }
        }
        "struct_item" => {
            if let Some(name) = field_text(w, node, "name") {
                w.push_symbol(name.clone(), name, SymbolKind::Struct, node);
            }
        }
        "enum_item" => {
            if let Some(name) = field_text(w, node, "name") {
                w.push_symbol(name.clone(), name, SymbolKind::Enum, node);
            }
        }
        "trait_item" => {
            if let Some(name) = field_text(w, node, "name") {
                w.push_symbol(name.clone(), name.clone(), SymbolKind::Trait, node);
                w.stack.push(name);
                pushed = true;
            }
        }
        "mod_item" => {
            if let Some(name) = field_text(w, node, "name") {
                w.push_symbol(name.clone(), name, SymbolKind::Module, node);
            }
        }
        "const_item" | "static_item" => {
            if let Some(name) = field_text(w, node, "name") {
                w.push_symbol(name.clone(), name, SymbolKind::Const, node);
            }
        }
        "call_expression" => {
            if let Some(f) = node.child_by_field_name("function") {
                rust_callee(w, node, f);
            }
        }
        "use_declaration" => {
            if let Some(arg) = node.child_by_field_name("argument") {
                let spec = w.text(arg);
                w.push_import(spec, Vec::new());
            }
        }
        _ => {}
    }
    for child in each_child(node) {
        walk_rust(w, child, depth + 1);
    }
    if pushed {
        w.stack.pop();
    }
}

fn rust_is_test_container(w: &Walker, node: Node) -> bool {
    if !matches!(node.kind(), "function_item" | "mod_item") {
        return false;
    }
    let range = node.byte_range();
    let end = range.end.min(range.start.saturating_add(512));
    let prefix = String::from_utf8_lossy(&w.src[range.start..end]);
    let header = prefix.split('{').next().unwrap_or(&prefix);
    let mut attributes = String::new();
    let mut sibling = node.prev_named_sibling();
    while let Some(previous) = sibling {
        if previous.kind() != "attribute_item" {
            break;
        }
        attributes.push_str(&w.text(previous));
        sibling = previous.prev_named_sibling();
    }
    let markers = format!("{attributes}{header}");
    markers.contains("#[test]")
        || markers.contains("::test]")
        || markers.contains("::test(")
        || (node.kind() == "mod_item" && markers.contains("#[cfg(test)]"))
}

fn rust_callee(w: &mut Walker, call: Node, f: Node) {
    match f.kind() {
        "identifier" => {
            let name = w.text(f);
            w.push_call(name, None, call);
        }
        "scoped_identifier" => {
            if let Some(name) = field_text(w, f, "name") {
                let recv = field_text(w, f, "path");
                w.push_call(name, recv, call);
            }
        }
        "field_expression" => {
            if let Some(name) = field_text(w, f, "field") {
                let recv = field_text(w, f, "value");
                w.push_call(name, recv, call);
            }
        }
        "generic_function" => {
            if let Some(inner) = f.child_by_field_name("function") {
                rust_callee(w, call, inner);
            }
        }
        _ => {}
    }
}

// --- Go ------------------------------------------------------------------

fn walk_go(w: &mut Walker, node: Node, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    match node.kind() {
        "function_declaration" => {
            if let Some(name) = field_text(w, node, "name") {
                w.push_symbol(name.clone(), name, SymbolKind::Function, node);
            }
        }
        "method_declaration" => {
            if let Some(name) = field_text(w, node, "name") {
                let recv = node
                    .child_by_field_name("receiver")
                    .and_then(|r| first_descendant_of_kind(r, "type_identifier"))
                    .map(|n| w.text(n));
                let q = match &recv {
                    Some(r) => format!("{r}.{name}"),
                    None => name.clone(),
                };
                w.push_symbol(name, q, SymbolKind::Method, node);
            }
        }
        "type_spec" => {
            if let (Some(name), Some(ty)) = (
                field_text(w, node, "name"),
                node.child_by_field_name("type"),
            ) {
                match ty.kind() {
                    "struct_type" => w.push_symbol(name.clone(), name, SymbolKind::Struct, node),
                    "interface_type" => {
                        w.push_symbol(name.clone(), name, SymbolKind::Interface, node)
                    }
                    _ => {}
                }
            }
        }
        "call_expression" => {
            if let Some(f) = node.child_by_field_name("function") {
                match f.kind() {
                    "identifier" => {
                        let name = w.text(f);
                        w.push_call(name, None, node);
                    }
                    "selector_expression" => {
                        if let Some(name) = field_text(w, f, "field") {
                            let recv = field_text(w, f, "operand");
                            w.push_call(name, recv, node);
                        }
                    }
                    _ => {}
                }
            }
        }
        "import_spec" => {
            if let Some(path) = node.child_by_field_name("path") {
                let spec = strip_quotes(&w.text(path));
                w.push_import(spec, Vec::new());
            }
        }
        _ => {}
    }
    for child in each_child(node) {
        walk_go(w, child, depth + 1);
    }
}

fn first_descendant_of_kind<'t>(n: Node<'t>, kind: &str) -> Option<Node<'t>> {
    if n.kind() == kind {
        return Some(n);
    }
    for child in each_child(n) {
        if let Some(found) = first_descendant_of_kind(child, kind) {
            return Some(found);
        }
    }
    None
}

// --- Java ----------------------------------------------------------------

fn walk_java(w: &mut Walker, node: Node, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    let mut pushed = false;
    match node.kind() {
        "class_declaration" => {
            if let Some(name) = field_text(w, node, "name") {
                let q = w.qualify(&name, ".");
                w.push_symbol(name.clone(), q, SymbolKind::Class, node);
                w.stack.push(name);
                pushed = true;
            }
        }
        "interface_declaration" => {
            if let Some(name) = field_text(w, node, "name") {
                let q = w.qualify(&name, ".");
                w.push_symbol(name.clone(), q, SymbolKind::Interface, node);
                w.stack.push(name);
                pushed = true;
            }
        }
        "enum_declaration" => {
            if let Some(name) = field_text(w, node, "name") {
                let q = w.qualify(&name, ".");
                w.push_symbol(name, q, SymbolKind::Enum, node);
            }
        }
        "method_declaration" | "constructor_declaration" => {
            if let Some(name) = field_text(w, node, "name") {
                let q = w.qualify(&name, ".");
                w.push_symbol(name, q, SymbolKind::Method, node);
            }
        }
        "method_invocation" => {
            if let Some(name) = field_text(w, node, "name") {
                let recv = field_text(w, node, "object");
                w.push_call(name, recv, node);
            }
        }
        "object_creation_expression" => {
            if let Some(ty) = field_text(w, node, "type") {
                let base = ty.split('<').next().unwrap_or(&ty);
                let name = base.rsplit('.').next().unwrap_or(base).trim().to_string();
                w.push_call(name, None, node);
            }
        }
        "import_declaration" => {
            let mut spec = String::new();
            let mut star = false;
            for child in each_child(node) {
                match child.kind() {
                    "scoped_identifier" | "identifier" => spec = w.text(child),
                    "asterisk" => star = true,
                    _ => {}
                }
            }
            if star && !spec.is_empty() {
                spec.push_str(".*");
            }
            w.push_import(spec, Vec::new());
        }
        _ => {}
    }
    for child in each_child(node) {
        walk_java(w, child, depth + 1);
    }
    if pushed {
        w.stack.pop();
    }
}

// --- Python --------------------------------------------------------------

fn walk_python(w: &mut Walker, node: Node, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    let mut pushed = false;
    match node.kind() {
        "class_definition" => {
            if let Some(name) = field_text(w, node, "name") {
                let q = w.qualify(&name, ".");
                w.push_symbol(name.clone(), q, SymbolKind::Class, node);
                w.stack.push(name);
                pushed = true;
            }
        }
        "function_definition" => {
            if let Some(name) = field_text(w, node, "name") {
                let (kind, q) = if w.stack.is_empty() {
                    (SymbolKind::Function, name.clone())
                } else {
                    (SymbolKind::Method, w.qualify(&name, "."))
                };
                w.push_symbol(name, q, kind, node);
            }
        }
        "call" => {
            if let Some(f) = node.child_by_field_name("function") {
                match f.kind() {
                    "identifier" => {
                        let name = w.text(f);
                        w.push_call(name, None, node);
                    }
                    "attribute" => {
                        if let Some(name) = field_text(w, f, "attribute") {
                            let recv = field_text(w, f, "object");
                            w.push_call(name, recv, node);
                        }
                    }
                    _ => {}
                }
            }
        }
        "import_statement" => {
            for child in each_child(node) {
                match child.kind() {
                    "dotted_name" => {
                        let spec = w.text(child);
                        w.push_import(spec, Vec::new());
                    }
                    "aliased_import" => {
                        if let Some(name) = child.child_by_field_name("name") {
                            let spec = w.text(name);
                            w.push_import(spec, Vec::new());
                        }
                    }
                    _ => {}
                }
            }
        }
        "import_from_statement" => {
            if let Some(m) = node.child_by_field_name("module_name") {
                let spec = w.text(m);
                w.push_import(spec, Vec::new());
            }
        }
        _ => {}
    }
    for child in each_child(node) {
        walk_python(w, child, depth + 1);
    }
    if pushed {
        w.stack.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::extract_file;

    #[test]
    fn rust_test_containers_do_not_enter_runtime_graph() {
        let source = br#"
fn production_entry() { production_helper(); }
fn production_helper() {}

#[test]
fn top_level_test() { production_entry(); }

#[cfg(test)]
mod tests {
    #[test]
    fn nested_test() { super::production_entry(); }
}
"#;
        let extraction = extract_file("src/lib.rs", source).unwrap();
        let names: Vec<_> = extraction
            .symbols
            .iter()
            .map(|symbol| symbol.name.as_str())
            .collect();
        assert!(names.contains(&"production_entry"));
        assert!(names.contains(&"production_helper"));
        assert!(!names.contains(&"top_level_test"));
        assert!(!names.contains(&"nested_test"));
        assert!(!names.contains(&"tests"));
    }
}

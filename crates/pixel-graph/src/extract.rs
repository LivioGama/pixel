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
    /// A method of a trait implementation (`impl Display for X { fn fmt }`):
    /// called through the trait, so a missing direct caller proves nothing.
    pub trait_impl: bool,
    /// An external Rust module declaration (`mod foo;`): it names the file
    /// that holds the module's code instead of defining code in this one, so
    /// an exact-name probe of `foo` must not promote the declaring file (see
    /// `targets::symbol_hits`). Inline modules (`mod foo { … }`) are false.
    pub module_decl: bool,
}

#[derive(Debug, Clone)]
pub struct RawCall {
    pub callee_name: String,
    pub receiver: Option<String>,
    pub site_line: u32,
    /// Index into `FileExtraction::symbols` of the smallest enclosing symbol.
    pub enclosing_index: Option<usize>,
}

/// A symbol passed as an argument to a call (callback / plugin / handler
/// registration). The `arg_of` field is the callee that received the
/// argument (e.g. `"plugin"` in `schema.plugin(tenantScopePlugin)`).
#[derive(Debug, Clone)]
pub struct RawReference {
    pub name: String,
    /// Index into `FileExtraction::symbols` of the smallest enclosing symbol.
    pub enclosing_index: Option<usize>,
    pub site_line: u32,
    /// The callee that received this argument, when known.
    pub arg_of: Option<String>,
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

#[derive(Debug, Clone)]
pub struct RawJsxElement {
    pub tag: String,
    pub has_handler: bool,
    pub text_content: String,
    pub start_line: u32,
    pub end_line: u32,
}

#[derive(Debug)]
pub struct FileExtraction {
    pub lang: &'static str,
    pub symbols: Vec<RawSymbol>,
    pub calls: Vec<RawCall>,
    pub references: Vec<RawReference>,
    pub imports: Vec<RawImport>,
    pub jsx_elements: Vec<RawJsxElement>,
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
        "cs" => Some("csharp"),
        "rb" | "rake" | "gemspec" | "ru" => Some("ruby"),
        // Perfect-expansion languages driven by the generic node-kind walker.
        "php" => Some("php"),
        "c" | "h" => Some("c"),
        "swift" => Some("swift"),
        "ex" | "exs" => Some("elixir"),
        "lua" => Some("lua"),
        _ => None,
    }
}

/// Parse one file into a tree-sitter tree for the language its extension
/// maps to. `None` on unsupported language or any parse/grammar failure.
/// Shared by extraction and the rename verifier, which re-parses a file to
/// confirm each candidate identifier's role before rewriting it.
pub fn parse_file(path_rel: &str, content: &[u8]) -> Option<tree_sitter::Tree> {
    let lang = lang_of(path_rel)?;
    let language = language_for(lang)?;
    std::panic::catch_unwind(AssertUnwindSafe(|| {
        let mut parser = Parser::new();
        parser.set_language(&language).ok()?;
        parser.parse(content, None)
    }))
    .ok()
    .flatten()
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
        "csharp" => tree_sitter_c_sharp::LANGUAGE.into(),
        "ruby" => tree_sitter_ruby::LANGUAGE.into(),
        "php" => tree_sitter_php::LANGUAGE_PHP.into(),
        "c" => tree_sitter_c::LANGUAGE.into(),
        "swift" => tree_sitter_swift::LANGUAGE.into(),
        "elixir" => tree_sitter_elixir::LANGUAGE.into(),
        "lua" => tree_sitter_lua::LANGUAGE.into(),
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
        references: Vec::new(),
        imports: Vec::new(),
        jsx_elements: Vec::new(),
        stack: Vec::new(),
        in_trait_impl: false,
    };
    let root = tree.root_node();
    match lang {
        "ts" | "tsx" | "js" => walk_ts(&mut w, lang, root, 0),
        "rust" => walk_rust(&mut w, root, 0),
        "go" => walk_go(&mut w, root, 0),
        "java" => walk_java(&mut w, root, 0),
        "python" => walk_python(&mut w, root, 0),
        "csharp" => walk_csharp(&mut w, root, 0),
        "ruby" => walk_ruby(&mut w, root, 0),
        // Any other wired language — or any future language added to the lang
        // map — falls back to the heuristic node-kind walker. Coarse but better
        // than absent.
        _ => walk_generic(&mut w, root, 0),
    }
    let mut fx = FileExtraction {
        lang,
        symbols: w.symbols,
        calls: w.calls,
        references: w.references,
        imports: w.imports,
        jsx_elements: w.jsx_elements,
    };
    assign_enclosing(&mut fx);
    Some(fx)
}

/// Smallest symbol whose line range contains the call/reference site.
fn assign_enclosing(fx: &mut FileExtraction) {
    let best = |line: u32| -> Option<usize> {
        let mut best: Option<(usize, u32)> = None;
        for (i, s) in fx.symbols.iter().enumerate() {
            if s.start_line <= line && line <= s.end_line {
                let span = s.end_line - s.start_line;
                if best.is_none_or(|(_, b)| span < b) {
                    best = Some((i, span));
                }
            }
        }
        best.map(|(i, _)| i)
    };
    for call in &mut fx.calls {
        call.enclosing_index = best(call.site_line);
    }
    for r#ref in &mut fx.references {
        r#ref.enclosing_index = best(r#ref.site_line);
    }
}

const MAX_DEPTH: usize = 512;
const SIG_CAP: usize = 200;

struct Walker<'a> {
    src: &'a [u8],
    symbols: Vec<RawSymbol>,
    calls: Vec<RawCall>,
    references: Vec<RawReference>,
    imports: Vec<RawImport>,
    jsx_elements: Vec<RawJsxElement>,
    /// Enclosing type names (class/impl/trait) for qualification.
    stack: Vec<String>,
    /// Inside the body of a trait implementation (`impl Trait for Type`).
    in_trait_impl: bool,
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
        self.push_symbol_full(name, qualified, kind, node, false);
    }

    /// An external `mod foo;` declaration: unlike every other symbol, it names
    /// another file instead of defining code in this one.
    fn push_module_decl(&mut self, name: String, node: Node) {
        self.push_symbol_full(name.clone(), name, SymbolKind::Module, node, true);
    }

    fn push_symbol_full(
        &mut self,
        name: String,
        qualified: String,
        kind: SymbolKind,
        node: Node,
        module_decl: bool,
    ) {
        if name.is_empty() {
            return;
        }
        self.symbols.push(RawSymbol {
            trait_impl: self.in_trait_impl && kind == SymbolKind::Method,
            module_decl,
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

    /// Record a symbol passed as an argument to a call (a callback / plugin /
    /// handler reference). `call_node` is the enclosing call expression; its
    /// start line becomes the reference site line. `arg_of` is the callee
    /// that received the argument, when known.
    fn push_reference(&mut self, name: String, call_node: Node, arg_of: Option<String>) {
        if name.is_empty() {
            return;
        }
        self.references.push(RawReference {
            name,
            enclosing_index: None,
            site_line: line_start(call_node),
            arg_of,
        });
    }

    fn push_import(&mut self, spec: String, bindings: Vec<String>) {
        if !spec.is_empty() {
            self.imports.push(RawImport { spec, bindings });
        }
    }

    fn push_jsx_element(
        &mut self,
        tag: String,
        has_handler: bool,
        text_content: String,
        start_line: u32,
        end_line: u32,
    ) {
        if tag.is_empty() {
            return;
        }
        self.jsx_elements.push(RawJsxElement {
            tag,
            has_handler,
            text_content,
            start_line,
            end_line,
        });
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

/// Words that parse as identifiers in some grammars but never name a
/// function: literal keywords and the self pseudo-receivers.
const NON_REFERENCE_WORDS: &[&str] = &[
    "undefined",
    "null",
    "true",
    "false",
    "None",
    "nil",
    "self",
    "this",
];

/// The receiver texts a member argument may start from and still name a
/// function of the enclosing type (`this.onClick`, `self.handler`).
const SELF_RECEIVERS: &[&str] = &["this", "self", "Self"];

/// Walk the `arguments` field of a call/invocation node and record each
/// argument that may name a function as a `RawReference`. The callee name
/// (`arg_of`) is the method/function that received the argument.
///
/// - a bare identifier (`schema.plugin(tenantScopePlugin)`), literal
///   keywords and self pseudo-receivers excepted;
/// - a path (`Self::helper`, `module::func`), which names an item;
/// - a member access only on a self receiver (`this.onClick`). A member of
///   any other value (`user.name`) is data: resolving its property name
///   against every function of that name linked unrelated code.
fn walk_call_arguments(w: &mut Walker, call: Node, arg_of: Option<String>) {
    let Some(args) = call.child_by_field_name("arguments") else {
        return;
    };
    let mut cursor = args.walk();
    for arg in args.children(&mut cursor) {
        let name = match arg.kind() {
            "identifier" | "simple_identifier" | "variable" => Some(w.text(arg)),
            "scoped_identifier" => field_text(w, arg, "name"),
            "member_expression" | "field_expression" | "attribute" | "member_access_expression" => {
                self_member_name(w, arg)
            }
            _ => None,
        };
        if let Some(name) = name
            && !NON_REFERENCE_WORDS.contains(&name.as_str())
        {
            w.push_reference(name, call, arg_of.clone());
        }
    }
}

/// The property of a member access whose receiver is `this`/`self`/`Self`,
/// or `None` for a member of any other value.
fn self_member_name(w: &Walker, member: Node) -> Option<String> {
    let receiver = ["object", "value", "expression"]
        .iter()
        .find_map(|f| member.child_by_field_name(f))
        .map(|c| w.text(c))?;
    if !SELF_RECEIVERS.contains(&receiver.as_str()) {
        return None;
    }
    ["property", "field", "attribute", "name"]
        .iter()
        .find_map(|f| member.child_by_field_name(f))
        .map(|c| w.text(c))
}

// --- TypeScript / TSX / JavaScript ---------------------------------------

fn walk_ts(w: &mut Walker, lang: &'static str, node: Node, depth: usize) {
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
            let is_fn = node.child_by_field_name("value").is_some_and(|v| {
                matches!(
                    v.kind(),
                    "arrow_function" | "function_expression" | "function"
                )
            });
            if is_fn
                && let Some(name) = field_text(w, node, "name")
                && !name.contains(['{', '['])
            {
                let q = w.qualify(&name, ".");
                w.push_symbol(name, q, SymbolKind::Function, node);
            }
        }
        "call_expression" => {
            let mut callee_name: Option<String> = None;
            if let Some(f) = node.child_by_field_name("function") {
                match f.kind() {
                    "identifier" => {
                        let name = w.text(f);
                        callee_name = Some(name.clone());
                        w.push_call(name, None, node);
                    }
                    "member_expression" => {
                        if let Some(prop) = field_text(w, f, "property") {
                            let recv = field_text(w, f, "object");
                            callee_name = Some(prop.clone());
                            w.push_call(prop, recv, node);
                        }
                    }
                    _ => {}
                }
            }
            // After extracting the callee, check arguments for identifier
            // references (callbacks / plugins / handlers passed as args).
            walk_call_arguments(w, node, callee_name);
        }
        "new_expression" => {
            if let Some(c) = node.child_by_field_name("constructor")
                && c.kind() == "identifier"
            {
                let name = w.text(c);
                w.push_call(name, None, node);
            }
            // `new Foo(handler)` — constructor args can also be callbacks.
            walk_call_arguments(w, node, None);
        }
        "import_statement" | "export_statement" => {
            if let Some(src) = node.child_by_field_name("source") {
                let spec = strip_quotes(&w.text(src));
                let bindings = ts_import_bindings(w, node);
                w.push_import(spec, bindings);
            }
        }
        // Only the tsx and js grammars produce this node; the ts grammar
        // has no JSX, so no language guard is needed here.
        "jsx_element" => {
            if let Some(opening) = node.child_by_field_name("open_tag")
                && let Some(tag_node) = opening.child_by_field_name("name")
            {
                let tag = w.text(tag_node);
                let has_handler = jsx_has_handler(w, opening);
                let text_content = jsx_text_content(w, node, opening, &tag);
                jsx_handler_refs(w, opening, &tag);
                if let Some((name, receiver)) = jsx_component_call(&tag) {
                    w.push_call(name, receiver, node);
                }
                w.push_jsx_element(
                    tag,
                    has_handler,
                    text_content,
                    line_start(node),
                    line_end(node),
                );
            }
        }
        "jsx_self_closing_element" if matches!(lang, "tsx" | "js") => {
            if let Some(tag_node) = node.child_by_field_name("name") {
                let tag = w.text(tag_node);
                let has_handler = jsx_has_handler(w, node);
                let text_content = jsx_attr_text(w, node);
                jsx_handler_refs(w, node, &tag);
                if let Some((name, receiver)) = jsx_component_call(&tag) {
                    w.push_call(name, receiver, node);
                }
                w.push_jsx_element(
                    tag,
                    has_handler,
                    text_content,
                    line_start(node),
                    line_end(node),
                );
            }
        }
        _ => {}
    }
    for child in each_child(node) {
        walk_ts(w, lang, child, depth + 1);
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

// --- JSX helpers ---------------------------------------------------------

/// The call a JSX tag compiles to, as `(name, receiver)`: `<Button/>` renders
/// the `Button` component, `<Menu.Item>` the `Item` member of `Menu`. A
/// lowercase or namespaced tag (`div`, `svg:rect`) is an intrinsic element
/// and renders no symbol. Without this edge every component that is only
/// rendered, never called, had no callers.
fn jsx_component_call(tag: &str) -> Option<(String, Option<String>)> {
    if tag.contains(':') {
        return None;
    }
    match tag.rsplit_once('.') {
        Some((receiver, name)) if !receiver.is_empty() && !name.is_empty() => {
            Some((name.to_string(), Some(receiver.to_string())))
        }
        Some(_) => None,
        None => tag
            .starts_with(|c: char| c.is_ascii_uppercase())
            .then(|| (tag.to_string(), None)),
    }
}

fn jsx_attr_name(w: &Walker, attr: Node) -> Option<String> {
    // jsx_attribute has no named fields and names like `aria-label` are parsed
    // as jsx_namespace_name. Use the raw attribute text and split on the first `=`.
    let raw = w.text(attr);
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    raw.split('=').next().map(|s| s.trim().to_string())
}

fn jsx_attr_value(w: &Walker, attr: Node) -> Option<String> {
    let raw = w.text(attr);
    let raw = raw.trim();
    if let Some((_, value)) = raw.split_once('=') {
        let value = strip_quotes(value.trim());
        if value.is_empty() { None } else { Some(value) }
    } else {
        None
    }
}

fn jsx_has_handler(w: &Walker, element: Node) -> bool {
    for child in each_child(element) {
        if child.kind() == "jsx_attribute"
            && let Some(name) = jsx_attr_name(w, child)
        {
            let n = name.to_lowercase();
            if n.starts_with("on") || n == "href" || n == "to" {
                return true;
            }
        }
    }
    false
}

/// Emit `references` edges for JSX event-handler props: `onClick={handler}`,
/// `onSubmit={this.save}`. Without this, a handler like `handleSubmit` has
/// zero edges and reads as dead code even though the element wires it.
/// Only bare identifiers and member expressions produce edges — inline
/// arrows (`onClick={() => f()}`) are walked as ordinary code, so their
/// inner calls are already extracted.
fn jsx_handler_refs(w: &mut Walker, element: Node, tag: &str) {
    for child in each_child(element) {
        if child.kind() != "jsx_attribute" {
            continue;
        }
        let Some(attr) = jsx_attr_name(w, child) else {
            continue;
        };
        if !attr.to_lowercase().starts_with("on") {
            continue;
        }
        let arg_of = Some(format!("{tag}.{attr}"));
        let mut cursor = child.walk();
        for part in child.children(&mut cursor) {
            if part.kind() != "jsx_expression" {
                continue;
            }
            let mut inner = part.walk();
            for expr in part.children(&mut inner) {
                match expr.kind() {
                    "identifier" => {
                        let name = w.text(expr);
                        w.push_reference(name, child, arg_of.clone());
                    }
                    "member_expression" => {
                        if let Some(prop) = expr.child_by_field_name("property") {
                            let name = w.text(prop);
                            w.push_reference(name, child, arg_of.clone());
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

fn jsx_attr_text(w: &Walker, element: Node) -> String {
    for child in each_child(element) {
        if child.kind() == "jsx_attribute"
            && let Some(name) = jsx_attr_name(w, child)
        {
            let n = name.to_lowercase();
            if (n == "aria-label" || n == "title")
                && let Some(v) = jsx_attr_value(w, child)
                && !v.is_empty()
            {
                return v;
            }
        }
    }
    String::new()
}

fn jsx_text_content(w: &Walker, element: Node, opening: Node, tag: &str) -> String {
    let mut parts = Vec::new();
    for child in each_child(element) {
        if child.kind() == "jsx_text" {
            let t = w.text(child);
            if !t.trim().is_empty() {
                parts.push(t.trim().to_string());
            }
        }
    }
    if !parts.is_empty() {
        return parts.join(" ").trim().to_string();
    }
    // Fallback to aria-label or title attribute for interactive tags.
    if matches!(tag.to_lowercase().as_str(), "button" | "a" | "link") {
        let fallback = jsx_attr_text(w, opening);
        if !fallback.is_empty() {
            return fallback;
        }
    }
    String::new()
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
    let outer_trait_impl = w.in_trait_impl;
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
            w.in_trait_impl = node.child_by_field_name("trait").is_some();
        }
        "struct_item" => {
            if let Some(name) = field_text(w, node, "name") {
                w.push_symbol(name.clone(), name, SymbolKind::Struct, node);
            }
        }
        "enum_item" => {
            if let Some(name) = field_text(w, node, "name") {
                w.push_symbol(name.clone(), name.clone(), SymbolKind::Enum, node);
                // Qualify each variant as `Enum::Variant` for the subtree.
                w.stack.push(name);
                pushed = true;
            }
        }
        "enum_variant" => {
            if let Some(name) = field_text(w, node, "name") {
                let q = w.qualify(&name, "::");
                w.push_symbol(name, q, SymbolKind::Variant, node);
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
                // `mod foo;` names the file that holds the module's code;
                // `mod foo { … }` defines it in this file.
                if node.child_by_field_name("body").is_some() {
                    w.push_symbol(name.clone(), name, SymbolKind::Module, node);
                } else {
                    w.push_module_decl(name, node);
                }
            }
        }
        "const_item" | "static_item" => {
            if let Some(name) = field_text(w, node, "name") {
                w.push_symbol(name.clone(), name, SymbolKind::Const, node);
            }
        }
        "call_expression" => {
            let mut callee_name: Option<String> = None;
            if let Some(f) = node.child_by_field_name("function") {
                callee_name = rust_callee(w, node, f);
            }
            // After extracting the callee, check arguments for identifier
            // references (callbacks / closures passed as args).
            walk_call_arguments(w, node, callee_name);
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
    w.in_trait_impl = outer_trait_impl;
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

fn rust_callee(w: &mut Walker, call: Node, f: Node) -> Option<String> {
    match f.kind() {
        "identifier" => {
            let name = w.text(f);
            w.push_call(name.clone(), None, call);
            Some(name)
        }
        "scoped_identifier" => {
            if let Some(name) = field_text(w, f, "name") {
                let recv = field_text(w, f, "path");
                w.push_call(name.clone(), recv, call);
                Some(name)
            } else {
                None
            }
        }
        "field_expression" => {
            if let Some(name) = field_text(w, f, "field") {
                let recv = field_text(w, f, "value");
                w.push_call(name.clone(), recv, call);
                Some(name)
            } else {
                None
            }
        }
        "generic_function" => {
            if let Some(inner) = f.child_by_field_name("function") {
                rust_callee(w, call, inner)
            } else {
                None
            }
        }
        _ => None,
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
            let mut callee_name: Option<String> = None;
            if let Some(f) = node.child_by_field_name("function") {
                match f.kind() {
                    "identifier" => {
                        let name = w.text(f);
                        callee_name = Some(name.clone());
                        w.push_call(name, None, node);
                    }
                    "selector_expression" => {
                        if let Some(name) = field_text(w, f, "field") {
                            let recv = field_text(w, f, "operand");
                            callee_name = Some(name.clone());
                            w.push_call(name, recv, node);
                        }
                    }
                    _ => {}
                }
            }
            // After extracting the callee, check arguments for identifier
            // references (callbacks / handlers passed as args).
            walk_call_arguments(w, node, callee_name);
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
            let mut callee_name: Option<String> = None;
            if let Some(name) = field_text(w, node, "name") {
                let recv = field_text(w, node, "object");
                callee_name = Some(name.clone());
                w.push_call(name, recv, node);
            }
            // After extracting the callee, check arguments for identifier
            // references (callbacks / handlers passed as args).
            walk_call_arguments(w, node, callee_name);
        }
        "object_creation_expression" => {
            if let Some(ty) = field_text(w, node, "type") {
                let base = ty.split('<').next().unwrap_or(&ty);
                let name = base.rsplit('.').next().unwrap_or(base).trim().to_string();
                w.push_call(name, None, node);
            }
            // `new Foo(handler)` — constructor args can also be callbacks.
            walk_call_arguments(w, node, None);
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
            let mut callee_name: Option<String> = None;
            if let Some(f) = node.child_by_field_name("function") {
                match f.kind() {
                    "identifier" => {
                        let name = w.text(f);
                        callee_name = Some(name.clone());
                        w.push_call(name, None, node);
                    }
                    "attribute" => {
                        if let Some(name) = field_text(w, f, "attribute") {
                            let recv = field_text(w, f, "object");
                            callee_name = Some(name.clone());
                            w.push_call(name, recv, node);
                        }
                    }
                    _ => {}
                }
            }
            // After extracting the callee, check arguments for identifier
            // references (callbacks / handlers passed as args).
            walk_call_arguments(w, node, callee_name);
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

// --- C# -------------------------------------------------------------------

fn walk_csharp(w: &mut Walker, node: Node, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    let mut pushed = false;
    match node.kind() {
        "namespace_declaration" => {
            if let Some(name) = field_text(w, node, "name") {
                let q = w.qualify(&name, ".");
                w.push_symbol(name.clone(), q, SymbolKind::Module, node);
                w.stack.push(name);
                pushed = true;
            }
        }
        "class_declaration" | "record_declaration" | "struct_declaration" => {
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
        "delegate_declaration" => {
            if let Some(name) = field_text(w, node, "name") {
                let q = w.qualify(&name, ".");
                w.push_symbol(name, q, SymbolKind::Method, node);
            }
        }
        "method_declaration" | "constructor_declaration" | "local_function_statement" => {
            if let Some(name) = field_text(w, node, "name") {
                let q = w.qualify(&name, ".");
                w.push_symbol(name, q, SymbolKind::Method, node);
            }
        }
        "property_declaration" => {
            if let Some(name) = field_text(w, node, "name") {
                let q = w.qualify(&name, ".");
                w.push_symbol(name, q, SymbolKind::Method, node);
            }
        }
        "invocation_expression" => {
            let mut callee_name: Option<String> = None;
            if let Some(f) = node.child_by_field_name("function") {
                match f.kind() {
                    "identifier" => {
                        let name = w.text(f);
                        callee_name = Some(name.clone());
                        w.push_call(name, None, node);
                    }
                    "member_access_expression" => {
                        if let Some(name) = field_text(w, f, "name") {
                            let recv = field_text(w, f, "expression");
                            callee_name = Some(name.clone());
                            w.push_call(name, recv, node);
                        }
                    }
                    "generic_name" => {
                        if let Some(inner) = f.child_by_field_name("name") {
                            let name = w.text(inner);
                            callee_name = Some(name.clone());
                            w.push_call(name, None, node);
                        }
                    }
                    _ => {}
                }
            }
            // After extracting the callee, check arguments for identifier
            // references (callbacks / handlers passed as args).
            walk_call_arguments(w, node, callee_name);
        }
        "object_creation_expression" => {
            if let Some(ty) = field_text(w, node, "type") {
                let base = ty.split('<').next().unwrap_or(&ty);
                let name = base.rsplit('.').next().unwrap_or(base).trim().to_string();
                w.push_call(name, None, node);
            }
            // `new Foo(handler)` — constructor args can also be callbacks.
            walk_call_arguments(w, node, None);
        }
        "using_directive" => {
            // `name` field only exists for alias usings (`using Foo = X;`)
            // and holds the alias — not the imported namespace. The qualified
            // namespace is a plain child (`qualified_name` / `identifier`).
            for child in each_child(node) {
                match child.kind() {
                    "qualified_name" | "identifier" => {
                        let spec = w.text(child);
                        w.push_import(spec, Vec::new());
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    for child in each_child(node) {
        walk_csharp(w, child, depth + 1);
    }
    if pushed {
        w.stack.pop();
    }
}

// --- Ruby -----------------------------------------------------------------

/// `call` methods that load another file rather than invoke behaviour. Their
/// first string argument becomes an import spec instead of a call edge.
const RUBY_REQUIRE_METHODS: &[&str] =
    &["require", "require_relative", "require_dependency", "load"];

fn walk_ruby(w: &mut Walker, node: Node, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    let mut pushed = false;
    match node.kind() {
        "module" => {
            if let Some(name) = field_text(w, node, "name") {
                let q = w.qualify(&name, "::");
                w.push_symbol(name.clone(), q, SymbolKind::Module, node);
                w.stack.push(name);
                pushed = true;
            }
        }
        "class" => {
            // `name` may be a `scope_resolution` (`Admin::User`); keep the
            // full text so reopened namespaced classes qualify consistently.
            if let Some(name) = field_text(w, node, "name") {
                let q = w.qualify(&name, "::");
                w.push_symbol(name.clone(), q, SymbolKind::Class, node);
                w.stack.push(name);
                pushed = true;
            }
        }
        "method" => {
            if let Some(name) = field_text(w, node, "name") {
                let (kind, q) = if w.stack.is_empty() {
                    (SymbolKind::Function, name.clone())
                } else {
                    // Ruby convention: `Klass#instance_method`.
                    (
                        SymbolKind::Method,
                        format!("{}#{}", w.stack.join("::"), name),
                    )
                };
                w.push_symbol(name, q, kind, node);
            }
        }
        "singleton_method" => {
            // `def self.foo` — Ruby convention: `Klass.class_method`. The
            // `object` is not pushed on the stack; the enclosing type is.
            if let Some(name) = field_text(w, node, "name") {
                let q = if w.stack.is_empty() {
                    name.clone()
                } else {
                    format!("{}.{}", w.stack.join("::"), name)
                };
                w.push_symbol(name, q, SymbolKind::Method, node);
            }
        }
        "call" => {
            let mut callee_name: Option<String> = None;
            if let Some(name) = field_text(w, node, "method") {
                let recv = field_text(w, node, "receiver");
                callee_name = Some(name.clone());
                if recv.is_none() && RUBY_REQUIRE_METHODS.contains(&name.as_str()) {
                    if let Some(spec) = ruby_first_string_argument(w, node) {
                        w.push_import(spec, Vec::new());
                    }
                } else {
                    // Covers paren-less Rails DSL (`has_many :spots`,
                    // `before_action :auth`) and receiver calls (`user.save`).
                    w.push_call(name, recv, node);
                }
            }
            // After extracting the callee, check arguments for identifier
            // references (callbacks / handlers passed as args). Skip require
            // methods — their string args are imports, not references.
            if !matches!(callee_name.as_deref(), Some(n) if RUBY_REQUIRE_METHODS.contains(&n)) {
                walk_call_arguments(w, node, callee_name);
            }
        }
        _ => {}
    }
    for child in each_child(node) {
        walk_ruby(w, child, depth + 1);
    }
    if pushed {
        w.stack.pop();
    }
}

/// Literal text of the first `string` argument of a Ruby `call`, or `None`
/// when the first argument is missing, non-literal, or interpolated.
fn ruby_first_string_argument(w: &Walker, call: Node) -> Option<String> {
    let args = call.child_by_field_name("arguments")?;
    let first = each_child(args)
        .into_iter()
        .find(tree_sitter::Node::is_named)?;
    if first.kind() != "string" {
        return None;
    }
    let mut spec = String::new();
    for part in each_child(first) {
        match part.kind() {
            "string_content" => spec.push_str(&w.text(part)),
            "interpolation" => return None,
            _ => {}
        }
    }
    if spec.is_empty() { None } else { Some(spec) }
}

// --- Generic heuristic walker -------------------------------------------------
//
// Languages without a hand-written walker (php, c, swift, elixir, lua,
// and any future grammar wired into `language_for`) fall back to a node-kind
// heuristic pass. We match node kinds ending in `_declaration`/`_definition`
// for symbols、 call/invocation node kinds for call sites, and import/use/require
// node kinds for import specs. It is deliberately coarse:s a less precise graph
// beats an absent one. Field names and node kinds degrade gracefully to `None`.

fn walk_generic(w: &mut Walker, node: Node, depth: usize) {
    if depth > MAX_DEPTH {
        return;
    }
    let kind = node.kind();
    let mut pushed = false;

    // Symbols: declarations and definitions carry a name we can qualify.
    if (kind.ends_with("_declaration") || kind.ends_with("_definition"))
        && let Some((sym_kind, is_container)) = generic_symbol_kind(kind)
        && let Some(name) = generic_name(w, node)
    {
        let q = w.qualify(&name, ".");
        if is_container {
            w.push_symbol(name.clone(), q, sym_kind, node);
            w.stack.push(name);
            pushed = true;
        } else {
            w.push_symbol(name, q, sym_kind, node);
        }
    }

    // Calls: any node kind mentioning call/invocation is a candidate site.
    if kind.contains("call") || kind.contains("invocation") {
        generic_call(w, node);
    }

    // Imports: import/use/require node kinds.
    if kind.contains("import") || kind.starts_with("use_") || kind.starts_with("require") {
        generic_import(w, node);
    }

    for child in each_child(node) {
        walk_generic(w, child, depth + 1);
    }
    if pushed {
        w.stack.pop();
    }
}

/// Classify a `_declaration`/`_definition` node kind into a `SymbolKind`,
/// plus whether the declared item nests further symbols (containers: class,
/// struct, interface, trait, protocol, namespace, module, package). Bare
/// declaration kinds (import/use/attribute/parameter/preproc/deinit/typealias…)
/// are filtered out —— they inject neither symbols nor qualification.
fn generic_symbol_kind(kind: &str) -> Option<(SymbolKind, bool)> {
    if !(kind.ends_with("_declaration") || kind.ends_with("_definition")) {
        return None;
    }
    if kind.contains("import")
        || kind.contains("use")
        || kind.starts_with("namespace_use")
        || kind.contains("attribute")
        || kind.contains("parameter")
        || kind.contains("argument")
        || kind.starts_with("preproc")
        || kind.contains("deinit")
        || kind.contains("typealias")
        || kind.contains("associatedtype")
        || kind.contains("operator")
    {
        return None;
    }
    if kind.contains("function")
        || kind.contains("method")
        || kind.contains("lambda")
        || kind.contains("closure")
    {
        return Some((SymbolKind::Function, false));
    }
    if kind.contains("namespace")
        || kind.contains("module")
        || kind.contains("package")
        || kind.contains("library")
    {
        return Some((SymbolKind::Module, true));
    }
    if kind.contains("class")
        || kind.contains("struct")
        || kind.contains("record")
        || kind.contains("actor")
    {
        return Some((SymbolKind::Class, true));
    }
    if kind.contains("interface") {
        return Some((SymbolKind::Interface, true));
    }
    if kind.contains("trait") || kind.contains("protocol") {
        return Some((SymbolKind::Trait, true));
    }
    if kind.contains("enum") {
        return Some((SymbolKind::Enum, false));
    }
    None
}

/// Best-effort symbol name for a declaration node. Prefers a `name` field,
/// then a `declarator` field (C function definitions nested type declarator),
/// then the first identifier-like named child (kotlin simple_identifier etc.).
fn generic_name(w: &Walker, node: Node) -> Option<String> {
    if let Some(name) = node.child_by_field_name("name") {
        let t = w.text(name);
        if !t.is_empty() && !t.contains(['(', ')', ',']) {
            return Some(t);
        }
    }
    if let Some(decl) = node.child_by_field_name("declarator") {
        for child in each_child(decl) {
            if let Some(n) = generic_name(w, child) {
                return Some(n);
            }
        }
    }
    for child in each_child(node) {
        match child.kind() {
            "simple_identifier" | "identifier" | "type_identifier" | "name" => {
                let t = w.text(child);
                if !t.is_empty() && !t.contains(['(', ')', ',']) {
                    return Some(t);
                }
            }
            _ => {}
        }
    }
    None
}

/// Extract a callee + optional receiver from a generic call/invocation node.
/// Handles direct identifiers (`function`/`callee`/`name`/`method`/`target`
/// fieldsh and member accesses (php `member_call_expression`, C `field_expression`
/// member of a join, etc.).
fn generic_call(w: &mut Walker, node: Node) {
    // Elixir `call` nodes whose body is a `do_block` are function definitions,
    // not invocation sites —— skip them.
    if node.kind() == "call" && node.child_by_field_name("do_block").is_some() {
        return;
    }
    let mut callee: Option<String> = None;
    let mut receiver: Option<String> = None;
    let expr = ["function", "callee", "name", "method", "target"]
        .iter()
        .find_map(|f| node.child_by_field_name(f));
    if let Some(e) = expr {
        match e.kind() {
            "identifier" | "simple_identifier" | "name" | "type_identifier" | "dotted_name"
            | "qualified_name" | "namespace_name" | "escaped_identifier" | "variable" => {
                callee = Some(w.text(e));
                receiver = ["receiver", "object", "scope", "target"]
                    .iter()
                    .find_map(|f| node.child_by_field_name(f))
                    .map(|r| w.text(r));
            }
            k if k.ends_with("_expression")
                || k.ends_with("_selector")
                || k.contains("member")
                || k.contains("attribute")
                || k.contains("index")
                || k.contains("access") =>
            {
                let pos_name = ["property", "field", "name", "attribute", "member"]
                    .iter()
                    .find_map(|f| e.child_by_field_name(f));
                let pos_recv = [
                    "object",
                    "operand",
                    "scope",
                    "expression",
                    "value",
                    "target",
                ]
                .iter()
                .find_map(|f| e.child_by_field_name(f));
                callee = pos_name.map(|n| w.text(n));
                receiver = pos_recv.map(|r| w.text(r));
            }
            _ => {}
        }
    }
    // php `scoped_call_expression` members a namelessqualified path; fall back
    // to dots in member name.
    if callee.is_none()
        && let Some(t) = node.child_by_field_name("target")
    {
        callee = Some(w.text(t));
    }
    if let (Some(c), r) = (callee, receiver) {
        w.push_call(c.clone(), r, node);
        // After extracting the callee, check arguments for identifier
        // references (callbacks / handlers passed as args).
        walk_call_arguments(w, node, Some(c));
    }
}

/// Best-effort import spec from import/use/require node kinds. Prefers source-like
/// fields, then string/identifier children (php `require_expression`, kotlin
/// `import_header`, swift `import_declaration`).
fn generic_import(w: &mut Walker, node: Node) {
    for field in ["source", "path", "module_name", "import_string", "name"] {
        if let Some(src) = node.child_by_field_name(field) {
            let spec = strip_quotes(&w.text(src));
            if !spec.is_empty() {
                w.push_import(spec, Vec::new());
                return;
            }
        }
    }
    for child in each_child(node) {
        match child.kind() {
            "string" => {
                let spec = strip_quotes(&w.text(child));
                if !spec.is_empty() {
                    w.push_import(spec, Vec::new());
                    return;
                }
            }
            "identifier" | "dotted_name" | "qualified_name" | "namespace_name" => {
                let spec = w.text(child);
                if !spec.is_empty() {
                    w.push_import(spec, Vec::new());
                    return;
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        FileExtraction, RawCall, RawSymbol, assign_enclosing, extract_file, jsx_component_call,
    };
    use crate::store::SymbolKind;

    fn sym(name: &str, start_line: u32, end_line: u32) -> RawSymbol {
        RawSymbol {
            name: name.to_string(),
            qualified: name.to_string(),
            kind: SymbolKind::Function,
            start_line,
            end_line,
            sig: String::new(),
            trait_impl: false,
            module_decl: false,
        }
    }

    fn call(site_line: u32) -> RawCall {
        RawCall {
            callee_name: "f".to_string(),
            receiver: None,
            site_line,
            enclosing_index: None,
        }
    }

    /// A call site belongs to the smallest symbol whose line range holds it;
    /// among equal spans the first declared wins; outside every symbol it
    /// has no owner.
    #[test]
    fn assign_enclosing_picks_the_smallest_containing_symbol() {
        let mut fx = FileExtraction {
            lang: "rs",
            symbols: vec![
                sym("outer", 90, 100),
                sym("inner", 95, 96),
                sym("a", 1, 10),
                sym("b", 5, 14),
            ],
            calls: vec![call(95), call(98), call(7), call(50)],
            imports: vec![],
            jsx_elements: vec![],
            references: vec![],
        };
        assign_enclosing(&mut fx);
        let owners: Vec<Option<usize>> = fx.calls.iter().map(|c| c.enclosing_index).collect();
        assert_eq!(owners, vec![Some(1), Some(0), Some(2), None]);
    }

    #[test]
    fn typescript_arrow_and_function_expression_declarators_are_symbols() {
        let source =
            b"const arrow = () => 1;\nconst expr = function () { return 2; };\nconst value = 3;\n";
        let extraction = extract_file("src/a.ts", source).unwrap();
        let names: Vec<&str> = extraction.symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"arrow"), "{names:?}");
        assert!(names.contains(&"expr"), "{names:?}");
        assert!(
            !names.contains(&"value"),
            "a plain value is not a symbol: {names:?}"
        );
    }

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

    /// `mod foo;` names the file that holds the module's code; `mod foo { … }`
    /// defines it here. Only the first must not grant this file an exact-name
    /// probe (see `targets::symbol_hits`).
    #[test]
    fn rust_module_declaration_distinguishes_external_from_inline() {
        let source = br#"
pub mod external;
mod inline {
    pub fn body() {}
}
"#;
        let extraction = extract_file("src/lib.rs", source).unwrap();
        let find = |name: &str| {
            extraction
                .symbols
                .iter()
                .find(|s| s.name == name)
                .unwrap_or_else(|| panic!("module `{name}` is a symbol"))
        };
        assert!(
            find("external").module_decl,
            "`mod foo;` must be marked as naming another file"
        );
        assert!(
            !find("inline").module_decl,
            "`mod foo {{ … }}` defines code in this file"
        );
    }

    /// Enum variants are definitions the ident tier must find. Before
    /// `EXTRACTOR_VERSION` 4, `walk_rust` emitted only the enum, so an exact
    /// query for `SelfUpdate` fell through to string-concept noise.
    #[test]
    fn rust_enum_variants_are_symbols() {
        let source = br#"
enum Command {
    SelfUpdate { force: bool },
    RepoState,
    DryRun,
    ListErrors(usize),
}
"#;
        let extraction = extract_file("src/main.rs", source).unwrap();
        let variants: Vec<(&str, &str)> = extraction
            .symbols
            .iter()
            .filter(|s| s.kind.as_str() == "variant")
            .map(|s| (s.name.as_str(), s.qualified.as_str()))
            .collect();
        assert_eq!(
            variants,
            [
                ("SelfUpdate", "Command::SelfUpdate"),
                ("RepoState", "Command::RepoState"),
                ("DryRun", "Command::DryRun"),
                ("ListErrors", "Command::ListErrors"),
            ],
            "every enum variant is a symbol qualified by its enum"
        );
        assert!(
            extraction
                .symbols
                .iter()
                .any(|s| s.kind == SymbolKind::Enum && s.qualified == "Command"),
            "the enum itself stays a symbol"
        );
    }

    #[test]
    fn csharp_extracts_symbols_calls_and_imports() {
        let source = br#"
using System.Collections.Generic;

namespace MyApp.Services {
    interface IGreeter {
        string Greet(string who);
    }

    enum Status { Open, Closed }

    public struct Point { public int X; }

    public class Greeter : IGreeter {
        public Greeter() { }

        public string Greet(string who) {
            var list = new List<string>();
            list.Add(who);
            return $"Hello {who}";
        }

        public int Add(int a, int b) => a + b;
    }

    public delegate bool Predicate(int x);
}
"#;
        let extraction = extract_file("Greeter.cs", source).unwrap();
        assert_eq!(extraction.lang, "csharp");
        assert_eq!(extraction.imports.len(), 1);
        assert_eq!(extraction.imports[0].spec, "System.Collections.Generic");

        let names: Vec<_> = extraction.symbols.iter().map(|s| s.name.as_str()).collect();
        for expected in [
            "MyApp.Services",
            "IGreeter",
            "Status",
            "Point",
            "Greeter",
            "Greet",
            "Add",
            "Predicate",
        ] {
            assert!(
                names.contains(&expected),
                "missing symbol {expected}: {names:?}"
            );
        }

        // Call into List<string>.Add via member access: receiver `list`.
        let member_calls: Vec<_> = extraction
            .calls
            .iter()
            .filter(|c| c.receiver.is_some())
            .collect();
        assert!(
            member_calls
                .iter()
                .any(|c| c.callee_name == "Add" && c.receiver.as_deref() == Some("list")),
            "expected member call Add() on receiver `list`: {:?}",
            extraction.calls
        );
        // Constructor invocation of List.
        assert!(
            extraction.calls.iter().any(|c| c.callee_name == "List"),
            "expected object creation of List: {:?}",
            extraction.calls
        );

        // Symbols inside the class carry qualified names (both the
        // interface method and the class method have the same simple name).
        let greet_in_greeter = extraction
            .symbols
            .iter()
            .find(|s| s.qualified == "MyApp.Services.Greeter.Greet")
            .expect("class method Greet should exist with full qualification");
        assert_eq!(greet_in_greeter.name, "Greet");
        let greet_in_interface = extraction
            .symbols
            .iter()
            .find(|s| s.qualified == "MyApp.Services.IGreeter.Greet")
            .expect("interface method Greet should exist with full qualification");
        assert_eq!(greet_in_interface.name, "Greet");
    }

    #[test]
    fn ruby_extracts_rails_symbols_calls_and_imports() {
        let source = br#"
require "json"
require_relative "../lib/pricing"

module Admin
  class UsersController < ApplicationController
    before_action :authenticate!

    def index
      @users = User.where(active: true)
      render json: @users
    end

    def self.permitted_params
    end

    private

    def authenticate!
    end
  end
end

def helper
end
"#;
        let extraction = extract_file("app/controllers/admin/users_controller.rb", source).unwrap();
        assert_eq!(extraction.lang, "ruby");

        // `require` loads files: they must land in imports, in source order,
        // so import-tier resolution can link the spec to a repo file.
        let specs: Vec<_> = extraction.imports.iter().map(|i| i.spec.as_str()).collect();
        assert_eq!(
            specs,
            vec!["json", "../lib/pricing"],
            "require strings become import specs in order"
        );
        assert!(
            !extraction
                .calls
                .iter()
                .any(|c| c.callee_name == "require" || c.callee_name == "require_relative"),
            "require must not double as a call edge — it would resolve to nothing and pollute impact: {:?}",
            extraction.calls
        );

        let find = |q: &str| extraction.symbols.iter().find(|s| s.qualified == q);
        let module =
            find("Admin").expect("module Admin is a symbol so namespaced targets qualify under it");
        assert_eq!(module.kind, SymbolKind::Module);
        let class =
            find("Admin::UsersController").expect("class qualifies under its module with `::`");
        assert_eq!(class.kind, SymbolKind::Class);
        assert_eq!(class.name, "UsersController");
        // `#` vs `.` distinguishes instance from class methods so impact
        // lookups never merge `User#save` with `User.save`.
        let index =
            find("Admin::UsersController#index").expect("instance method qualifies with `#`");
        assert_eq!(index.kind, SymbolKind::Method);
        let params = find("Admin::UsersController.permitted_params")
            .expect("`def self.` qualifies with `.`");
        assert_eq!(params.kind, SymbolKind::Method);
        assert!(
            find("Admin::UsersController#authenticate!").is_some(),
            "bang methods keep their `!` — it is part of the Ruby name: {:?}",
            extraction
                .symbols
                .iter()
                .map(|s| &s.qualified)
                .collect::<Vec<_>>()
        );
        let helper =
            find("helper").expect("top-level def is a bare Function, not a method of anything");
        assert_eq!(helper.kind, SymbolKind::Function);

        let call = |name: &str| extraction.calls.iter().find(|c| c.callee_name == name);
        let before = call("before_action")
            .expect("paren-less Rails DSL is a call — hooks are how Rails wires behaviour");
        assert_eq!(before.receiver, None);
        let wher = call("where").expect("receiver call `User.where` is a call");
        assert_eq!(
            wher.receiver.as_deref(),
            Some("User"),
            "receiver is kept so class-method calls can resolve to `User.where`"
        );
        let render = call("render").expect("keyword-arg call without parens is still a call");
        assert_eq!(render.receiver, None);
        assert_eq!(
            wher.enclosing_index
                .map(|i| extraction.symbols[i].qualified.as_str()),
            Some("Admin::UsersController#index"),
            "call sites attach to the smallest enclosing method so impact walks method-to-method"
        );
    }

    #[test]
    fn jsx_button_with_onclick_is_wired() {
        let source = br#"function App() { return <button onClick={() => {}}>Save</button>; }"#;
        let extraction = extract_file("App.tsx", source).unwrap();
        assert_eq!(extraction.jsx_elements.len(), 1);
        let e = &extraction.jsx_elements[0];
        assert_eq!(e.tag, "button");
        assert!(e.has_handler);
        assert_eq!(e.text_content, "Save");
    }

    #[test]
    fn jsx_element_without_a_handler_prop_is_not_wired() {
        // `className` is an attribute but not a handler; `href` alone is.
        // (A wrong comparison used to let any non-href attribute count.)
        let source = br#"function App() { return <button className="x">Save</button>; }"#;
        let extraction = extract_file("App.tsx", source).unwrap();
        assert_eq!(extraction.jsx_elements.len(), 1);
        assert!(
            !extraction.jsx_elements[0].has_handler,
            "{:?}",
            extraction.jsx_elements
        );
        let source = br#"function App() { return <a href="/x">link</a>; }"#;
        let extraction = extract_file("App.tsx", source).unwrap();
        assert!(extraction.jsx_elements[0].has_handler);
        let source = br#"function App() { return <button>Save</button>; }"#;
        let extraction = extract_file("App.tsx", source).unwrap();
        assert!(
            !extraction.jsx_elements[0].has_handler,
            "no attributes at all"
        );
    }

    #[test]
    fn plain_ts_source_yields_no_jsx_elements() {
        let source = br#"function App() { return <button onClick={f}>Save</button>; }"#;
        let extraction = extract_file("App.ts", source).unwrap();
        assert!(
            extraction.jsx_elements.is_empty(),
            "{:?}",
            extraction.jsx_elements
        );
    }

    #[test]
    fn jsx_link_href_and_to_are_handlers() {
        let source = br#"
function App() {
  return (
    <>
      <a href="/home" className="x">Home</a>
      <Link to="/profile">Profile</Link>
    </>
  );
}
"#;
        let extraction = extract_file("App.tsx", source).unwrap();
        let tags: Vec<_> = extraction
            .jsx_elements
            .iter()
            .map(|e| (e.tag.as_str(), e.has_handler))
            .collect();
        assert!(tags.contains(&("a", true)));
        assert!(tags.contains(&("Link", true)));
    }

    #[test]
    fn jsx_aria_label_and_title_fallback() {
        let source = br#"
function App() {
  return (
    <>
      <button aria-label="Close" />
      <button title="Submit form" />
      <button aria-label="Dismiss" title="X">x</button>
    </>
  );
}
"#;
        let extraction = extract_file("App.tsx", source).unwrap();
        let texts: Vec<_> = extraction
            .jsx_elements
            .iter()
            .map(|e| e.text_content.as_str())
            .collect();
        assert!(texts.contains(&"Close"));
        assert!(texts.contains(&"Submit form"));
        // Visible text wins over title/aria-label when present.
        assert!(texts.contains(&"x"));
    }

    #[test]
    fn jsx_self_closing_button_is_extracted() {
        let source = br#"function App() { return <button onClick={handle} className="ok" aria-label="OK" />; }"#;
        let extraction = extract_file("App.tsx", source).unwrap();
        let e = extraction
            .jsx_elements
            .iter()
            .find(|e| e.tag == "button")
            .expect("button");
        assert!(e.has_handler);
        assert_eq!(e.text_content, "OK");
    }

    #[test]
    fn jsx_malformed_does_not_crash() {
        let source = br#"function App() { return <button onClick={}>  ; }"#;
        let extraction = extract_file("App.tsx", source);
        // Extraction should return a result even if the JSX is broken.
        assert!(extraction.is_some());
    }

    #[test]
    fn ts_callback_arg_extracted_as_reference() {
        // `schema.plugin(tenantScopePlugin)` — tenantScopePlugin is passed
        // as an argument to the `plugin` call, so it should be a RawReference
        // with arg_of = "plugin".
        let source = br#"
export function tenantScopePlugin(schema: any) { return schema; }
export function setup(schema: any) {
  schema.plugin(tenantScopePlugin);
}
"#;
        let extraction = extract_file("src/schema.ts", source).unwrap();
        let refs: Vec<_> = extraction
            .references
            .iter()
            .filter(|r| r.name == "tenantScopePlugin")
            .collect();
        assert_eq!(
            refs.len(),
            1,
            "tenantScopePlugin passed as arg: {:?}",
            extraction.references
        );
        assert_eq!(refs[0].name, "tenantScopePlugin");
        assert_eq!(refs[0].arg_of.as_deref(), Some("plugin"));
        // The reference is enclosed by `setup`, not `tenantScopePlugin`.
        let enclosing = refs[0]
            .enclosing_index
            .map(|i| extraction.symbols[i].name.as_str());
        assert_eq!(enclosing, Some("setup"));
    }

    #[test]
    fn ts_emitter_on_handler_extracted_as_reference() {
        // `emitter.on('event', handler)` — handler is passed as an argument
        // to the `on` call, so it should be a RawReference with arg_of = "on".
        let source = br#"
export function handler() {}
export function wire(emitter: any) {
  emitter.on('event', handler);
}
"#;
        let extraction = extract_file("src/emitter.ts", source).unwrap();
        let refs: Vec<_> = extraction
            .references
            .iter()
            .filter(|r| r.name == "handler")
            .collect();
        assert_eq!(
            refs.len(),
            1,
            "handler passed as arg: {:?}",
            extraction.references
        );
        assert_eq!(refs[0].arg_of.as_deref(), Some("on"));
    }

    fn reference_names(path: &str, source: &[u8]) -> Vec<String> {
        let extraction = extract_file(path, source).unwrap();
        extraction.references.into_iter().map(|r| r.name).collect()
    }

    /// A member argument names a function only on a self receiver:
    /// `user.name` is data, and resolving `name` against every function of
    /// that name linked unrelated code.
    #[test]
    fn member_arguments_are_references_only_on_a_self_receiver() {
        assert_eq!(
            reference_names(
                "src/a.ts",
                b"export class C { go(user: any) { consume(user.name, this.onClick, handler); } }\n",
            ),
            ["onClick", "handler"]
        );
        assert_eq!(
            reference_names(
                "src/a.py",
                b"class C:\n    def go(self, obj):\n        register(self, self.handler, obj.attr, None)\n",
            ),
            ["handler"]
        );
        assert_eq!(
            reference_names(
                "src/a.rs",
                b"impl C { fn go(&self, cfg: Cfg) { run(cfg.field, self.handler, Self::helper, self); } }\n",
            ),
            ["handler", "helper"]
        );
    }

    #[test]
    fn jsx_component_call_names_rendered_components_only() {
        assert_eq!(
            jsx_component_call("Button"),
            Some(("Button".to_string(), None))
        );
        assert_eq!(
            jsx_component_call("Menu.Item"),
            Some(("Item".to_string(), Some("Menu".to_string())))
        );
        assert_eq!(
            jsx_component_call("ui.menu.item"),
            Some(("item".to_string(), Some("ui.menu".to_string())))
        );
        for intrinsic in ["div", "button", "svg:rect", "Svg:Rect", ".x", "x.", ""] {
            assert_eq!(jsx_component_call(intrinsic), None, "{intrinsic:?}");
        }
    }

    #[test]
    fn rendered_jsx_components_are_calls_and_intrinsic_tags_are_not() {
        let source = b"export function App() { return <div><Button label=\"x\" /><Menu.Item>go</Menu.Item></div>; }\n";
        let extraction = extract_file("src/App.tsx", source).unwrap();
        let calls: Vec<(&str, Option<&str>)> = extraction
            .calls
            .iter()
            .map(|c| (c.callee_name.as_str(), c.receiver.as_deref()))
            .collect();
        assert_eq!(calls, [("Button", None), ("Item", Some("Menu"))]);
        let app = extraction
            .symbols
            .iter()
            .position(|s| s.name == "App")
            .unwrap();
        assert!(
            extraction
                .calls
                .iter()
                .all(|c| c.enclosing_index == Some(app)),
            "the renderer encloses every component call: {:?}",
            extraction.calls
        );
    }

    /// `impl Display for X { fn fmt }` is called through the trait: its
    /// methods are marked, inherent and trait-definition methods are not,
    /// and the mark ends with the impl block, nested impls included.
    #[test]
    fn rust_trait_impl_methods_are_marked() {
        let source = br#"
struct X;
impl std::fmt::Display for X {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { Ok(()) }
}
impl X {
    fn own(&self) {
        struct Y;
        impl Default for Y { fn default() -> Self { Y } }
    }
    fn after(&self) {}
}
trait Greet { fn hello(&self) {} }
fn free() {}
"#;
        let extraction = extract_file("src/lib.rs", source).unwrap();
        let marked: Vec<(&str, bool)> = extraction
            .symbols
            .iter()
            .filter(|s| matches!(s.kind, SymbolKind::Method | SymbolKind::Function))
            .map(|s| (s.name.as_str(), s.trait_impl))
            .collect();
        assert_eq!(
            marked,
            [
                ("fmt", true),
                ("own", false),
                ("default", true),
                ("after", false),
                ("hello", false),
                ("free", false),
            ]
        );
    }

    #[test]
    fn ts_literal_args_not_extracted_as_references() {
        // `foo(undefined, null, true, false, "x", 42)` — none of the literal
        // args should become references.
        let source = b"export function f() { foo(undefined, null, true, false, \"x\", 42); }\n";
        let extraction = extract_file("src/f.ts", source).unwrap();
        assert!(
            extraction.references.is_empty(),
            "literal args must not be references: {:?}",
            extraction.references
        );
    }
}

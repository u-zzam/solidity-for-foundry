//! Engine B (live navigation): an error-tolerant tree-sitter parse of each open
//! buffer, re-run on every keystroke.
//!
//! Unlike the solc-AST index — accurate, but built only from the last
//! successful full compile — this works the instant a file opens, survives
//! syntax errors, updates while typing, and needs no `foundry.toml`. It resolves
//! symbols by name (no type inference), so the solc index is preferred whenever
//! it has a valid answer; this fills the gaps it cannot: cold start, mid-edit,
//! and config-less single files.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use tower_lsp::lsp_types::{
    CompletionItem, CompletionItemKind, DocumentHighlight, DocumentHighlightKind, DocumentSymbol,
    Hover, HoverContents, InlayHint, InlayHintKind, InlayHintLabel, Location, MarkupContent,
    MarkupKind, Position, Range, SymbolInformation, SymbolKind, Url,
};
use tree_sitter::{Node, Parser};

use crate::diagnostics::PositionMapper;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DefKind {
    Contract,
    Interface,
    Library,
    Function,
    Modifier,
    Event,
    Error,
    Struct,
    Field,
    Enum,
    EnumValue,
    StateVar,
    Local,
    Param,
    UserType,
}

impl DefKind {
    /// A type scope whose body's declarations become `.`-accessible members.
    fn is_container(self) -> bool {
        matches!(
            self,
            DefKind::Contract | DefKind::Interface | DefKind::Library | DefKind::Struct | DefKind::Enum
        )
    }

    /// Accessible via `Container.` member completion (excludes locals/params and
    /// the container kinds themselves).
    fn is_member(self) -> bool {
        matches!(
            self,
            DefKind::Function
                | DefKind::Event
                | DefKind::Error
                | DefKind::Struct
                | DefKind::Field
                | DefKind::Enum
                | DefKind::EnumValue
                | DefKind::StateVar
                | DefKind::UserType
        )
    }

    /// Worth offering as a bare identifier (top-level/contract names, callables,
    /// state vars) — locals, params, fields and enum values are not.
    fn is_global(self) -> bool {
        !matches!(self, DefKind::Local | DefKind::Param | DefKind::Field | DefKind::EnumValue)
    }

    /// Carries an ordered parameter list a call site can label with names.
    fn takes_args(self) -> bool {
        matches!(
            self,
            DefKind::Function | DefKind::Modifier | DefKind::Event | DefKind::Error | DefKind::Struct
        )
    }

    fn symbol_kind(self) -> SymbolKind {
        match self {
            DefKind::Contract | DefKind::Library => SymbolKind::CLASS,
            DefKind::Interface => SymbolKind::INTERFACE,
            DefKind::Function => SymbolKind::FUNCTION,
            DefKind::Modifier => SymbolKind::FUNCTION,
            DefKind::Event => SymbolKind::EVENT,
            DefKind::Error => SymbolKind::OBJECT,
            DefKind::Struct | DefKind::UserType => SymbolKind::STRUCT,
            DefKind::Field | DefKind::StateVar => SymbolKind::FIELD,
            DefKind::Enum => SymbolKind::ENUM,
            DefKind::EnumValue => SymbolKind::ENUM_MEMBER,
            DefKind::Local | DefKind::Param => SymbolKind::VARIABLE,
        }
    }

    fn completion_kind(self) -> CompletionItemKind {
        match self {
            DefKind::Contract | DefKind::Library => CompletionItemKind::CLASS,
            DefKind::Interface => CompletionItemKind::INTERFACE,
            DefKind::Function | DefKind::Modifier => CompletionItemKind::FUNCTION,
            DefKind::Event => CompletionItemKind::EVENT,
            DefKind::Error => CompletionItemKind::CONSTRUCTOR,
            DefKind::Struct | DefKind::UserType => CompletionItemKind::STRUCT,
            DefKind::Field | DefKind::StateVar => CompletionItemKind::FIELD,
            DefKind::Enum => CompletionItemKind::ENUM,
            DefKind::EnumValue => CompletionItemKind::ENUM_MEMBER,
            DefKind::Local | DefKind::Param => CompletionItemKind::VARIABLE,
        }
    }
}

/// A named declaration found in the buffer.
struct Def {
    name: String,
    kind: DefKind,
    name_start: usize,
    name_end: usize,
    full_start: usize,
    full_end: usize,
    /// Index into the file's `defs` of the enclosing container
    /// (contract/interface/library/struct/enum), for member completion and
    /// outline nesting. `None` at file level.
    container: Option<usize>,
    /// A rendered header (`function f(uint a) public`) for hover/completion.
    detail: String,
    /// Ordered parameter names for callables/structs, for call-site inlay hints.
    /// Empty for non-callables; an unnamed parameter is an empty string.
    params: Vec<String>,
    /// For a variable/parameter/field of a user-defined type, that type's name,
    /// so `<var>.` can complete the type's members. `None` for elementary types,
    /// mappings and arrays.
    type_name: Option<String>,
    /// For a contract/interface, the names it inherits from, so member completion
    /// can include inherited members.
    bases: Vec<String>,
}

/// One identifier occurrence (declaration site or reference), for cursor
/// resolution, find-references and document highlight.
struct Ident {
    name: String,
    start: usize,
    end: usize,
    /// True when this occurrence is a declaration's own name.
    is_def: bool,
}

/// A call site (`f(a, b)`, `emit E(x)`, `mod(y)`) and its positional arguments,
/// for call-site parameter-name inlay hints.
struct Call {
    callee: String,
    args: Vec<Arg>,
}

/// One positional argument: where its expression starts, and the argument's own
/// identifier when it is a bare name (so a hint that would merely repeat the
/// parameter name can be suppressed).
struct Arg {
    byte: usize,
    ident: Option<String>,
}

/// An import path literal and the span of the quoted string, so clicking the
/// path can open the imported file.
struct ImportPath {
    /// The path as written, without the surrounding quotes (`./X.sol`).
    path: String,
    start: usize,
    end: usize,
}

/// A single parsed buffer.
pub struct File {
    pub text: String,
    defs: Vec<Def>,
    idents: Vec<Ident>,
    calls: Vec<Call>,
    imports: Vec<ImportPath>,
}

thread_local! {
    static PARSER: RefCell<Parser> = RefCell::new(make_parser());
}

fn make_parser() -> Parser {
    let mut p = Parser::new();
    p.set_language(&tree_sitter_solidity::LANGUAGE.into())
        .expect("load solidity grammar");
    p
}

/// Parse `text` into a symbol view. Always succeeds (tree-sitter is error
/// tolerant); a totally unparseable buffer yields an empty view.
pub fn parse(text: &str) -> File {
    let tree = PARSER.with(|p| p.borrow_mut().parse(text, None));
    let mut defs = Vec::new();
    let mut idents = Vec::new();
    let mut calls = Vec::new();
    let mut imports = Vec::new();
    if let Some(tree) = tree {
        let src = text.as_bytes();
        walk(tree.root_node(), src, None, &mut defs, &mut idents, &mut calls, &mut imports);
        // Mark identifier occurrences that coincide with a declaration's name.
        let def_spans: HashSet<(usize, usize)> =
            defs.iter().map(|d| (d.name_start, d.name_end)).collect();
        for id in &mut idents {
            id.is_def = def_spans.contains(&(id.start, id.end));
        }
    }
    File { text: text.to_string(), defs, idents, calls, imports }
}

/// Recursive traversal: record declarations (carrying their enclosing type
/// scope) and every identifier occurrence.
fn walk<'a>(
    node: Node<'a>,
    src: &[u8],
    container: Option<usize>,
    defs: &mut Vec<Def>,
    idents: &mut Vec<Ident>,
    calls: &mut Vec<Call>,
    imports: &mut Vec<ImportPath>,
) {
    // Import path: record the quoted source so clicking it can open the file.
    // Descend anyway — a named import (`import {X} from …`) has identifiers we
    // still want indexed.
    if node.kind() == "import_directive" {
        if let Some(s) = node.child_by_field_name("source") {
            if let Ok(raw) = s.utf8_text(src) {
                imports.push(ImportPath {
                    path: raw.trim_matches(['"', '\'']).to_string(),
                    start: s.start_byte(),
                    end: s.end_byte(),
                });
            }
        }
    }

    if matches!(node.kind(), "identifier" | "enum_value") {
        if let Ok(name) = node.utf8_text(src) {
            idents.push(Ident {
                name: name.to_string(),
                start: node.start_byte(),
                end: node.end_byte(),
                is_def: false,
            });
        }
        // enum_value is also a declaration; fall through to record it.
        if node.kind() == "identifier" {
            return;
        }
    }

    if let Some(call) = call_of(node, src) {
        calls.push(call);
        // Fall through to descend into the arguments (nested calls, identifiers).
    }

    if let Some((name, kind, ns, ne)) = def_of(node, src) {
        let detail = header(node, src);
        let index = defs.len();
        defs.push(Def {
            name,
            kind,
            name_start: ns,
            name_end: ne,
            full_start: node.start_byte(),
            full_end: node.end_byte(),
            container,
            detail,
            params: param_names(node, src),
            type_name: var_type_name(node, src),
            bases: base_names(node, src),
        });
        // Descend with this def as the container if it is a type scope. Nesting
        // by index (not name) stops a member sharing its container's name
        // (`contract A { struct A {} }`) from re-matching itself and recursing
        // forever in `nested`.
        let child_container = if kind.is_container() { Some(index) } else { container };
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            walk(child, src, child_container, defs, idents, calls, imports);
        }
        return;
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk(child, src, container, defs, idents, calls, imports);
    }
}

/// If `node` is a named declaration, return `(name, kind, name_start, name_end)`.
fn def_of(node: Node, src: &[u8]) -> Option<(String, DefKind, usize, usize)> {
    let kind = match node.kind() {
        "contract_declaration" => DefKind::Contract,
        "interface_declaration" => DefKind::Interface,
        "library_declaration" => DefKind::Library,
        "function_definition" => DefKind::Function,
        "modifier_definition" => DefKind::Modifier,
        "event_definition" => DefKind::Event,
        "error_declaration" => DefKind::Error,
        "struct_declaration" => DefKind::Struct,
        "struct_member" => DefKind::Field,
        "enum_declaration" => DefKind::Enum,
        "enum_value" => DefKind::EnumValue,
        "state_variable_declaration" | "constant_variable_declaration" => DefKind::StateVar,
        "user_defined_type_definition" => DefKind::UserType,
        "variable_declaration" => DefKind::Local,
        "parameter" | "event_parameter" | "error_parameter" => DefKind::Param,
        _ => return None,
    };
    // enum_value has no `name` field — the node itself is the identifier.
    let name_node = if kind == DefKind::EnumValue {
        node
    } else {
        node.child_by_field_name("name")?
    };
    let name = name_node.utf8_text(src).ok()?.to_string();
    if name.is_empty() {
        return None;
    }
    Some((name, kind, name_node.start_byte(), name_node.end_byte()))
}

/// A one-line header for hover/completion: the declaration up to its body (or
/// the whole node when bodyless), with interior whitespace collapsed.
fn header(node: Node, src: &[u8]) -> String {
    let start = node.start_byte();
    let stop = node
        .child_by_field_name("body")
        .map(|b| b.start_byte())
        .unwrap_or_else(|| node.end_byte());
    let raw = std::str::from_utf8(&src[start..stop.min(src.len())]).unwrap_or("");
    // Drop a trailing initializer and the terminator, then collapse whitespace.
    let raw = raw.split('=').next().unwrap_or(raw);
    let raw = raw.trim().trim_end_matches([';', '{']).trim();
    raw.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Ordered parameter names of a callable or struct, so a call site can label
/// each positional argument. An unnamed parameter yields an empty string.
fn param_names(node: Node, src: &[u8]) -> Vec<String> {
    let child_kind = match node.kind() {
        "function_definition" | "modifier_definition" => "parameter",
        "event_definition" => "event_parameter",
        "error_declaration" => "error_parameter",
        "struct_declaration" => "struct_member",
        _ => return Vec::new(),
    };
    let mut out = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() != child_kind {
            continue;
        }
        let name = child
            .child_by_field_name("name")
            .and_then(|n| n.utf8_text(src).ok())
            .unwrap_or("")
            .to_string();
        out.push(name);
    }
    out
}

/// Unwrap the grammar's `expression` supertype wrapper to the concrete node it
/// holds (`(expression (identifier))` -> the identifier).
fn inner_expr(mut node: Node) -> Node {
    while node.kind() == "expression" {
        match node.named_child(0) {
            Some(child) => node = child,
            None => break,
        }
    }
    node
}

/// The user-defined type of a variable/parameter/field, so `<var>.` can complete
/// that type's members. `None` for elementary types, mappings and arrays — which
/// have no single container of members.
fn var_type_name(node: Node, src: &[u8]) -> Option<String> {
    if !matches!(
        node.kind(),
        "variable_declaration"
            | "state_variable_declaration"
            | "constant_variable_declaration"
            | "parameter"
            | "struct_member"
            | "event_parameter"
            | "error_parameter"
    ) {
        return None;
    }
    let type_field = node.child_by_field_name("type")?;
    // A plain user type's `type_name` wraps a `user_defined_type`; a primitive,
    // mapping or array wraps something else and yields no member container.
    let mut cursor = type_field.walk();
    let first = type_field.named_children(&mut cursor).next()?;
    (first.kind() == "user_defined_type")
        .then(|| last_identifier(first, src))
        .flatten()
}

/// The names a contract/interface inherits from (each `inheritance_specifier`'s
/// `ancestor`), for walking inherited members.
fn base_names(node: Node, src: &[u8]) -> Vec<String> {
    if !matches!(node.kind(), "contract_declaration" | "interface_declaration") {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() != "inheritance_specifier" {
            continue;
        }
        if let Some(name) =
            child.child_by_field_name("ancestor").and_then(|a| last_identifier(a, src))
        {
            out.push(name);
        }
    }
    out
}

/// The last `identifier` under a node (`A.B` qualified type -> `B`).
fn last_identifier(node: Node, src: &[u8]) -> Option<String> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|n| n.kind() == "identifier")
        .last()
        .and_then(|id| id.utf8_text(src).ok())
        .map(String::from)
}

/// The callee name of a call: a bare `identifier`, or the rightmost name of a
/// `member_expression` (`Lib.add` -> `add`). `None` for any other expression.
fn callee_name(node: Node, src: &[u8]) -> Option<String> {
    let node = inner_expr(node);
    match node.kind() {
        "identifier" => node.utf8_text(src).ok().map(String::from),
        "member_expression" => node
            .child_by_field_name("property")
            .and_then(|p| p.utf8_text(src).ok())
            .map(String::from),
        _ => None,
    }
}

/// Extract a call site from `node` if it is one we hint: `f(...)` / `Lib.f(...)`
/// (`call_expression`), `emit E(...)` (`emit_statement`), or a modifier use
/// (`modifier_invocation`). Returns `None` for named-argument calls (`f({a: 1})`,
/// whose names are already in source) and argument-less invocations.
fn call_of(node: Node, src: &[u8]) -> Option<Call> {
    let callee = match node.kind() {
        "call_expression" => callee_name(node.child_by_field_name("function")?, src)?,
        "emit_statement" => callee_name(node.child_by_field_name("name")?, src)?,
        "modifier_invocation" => {
            let mut cursor = node.walk();
            let id = node.named_children(&mut cursor).find(|c| c.kind() == "identifier")?;
            id.utf8_text(src).ok()?.to_string()
        }
        _ => return None,
    };
    let mut args = Vec::new();
    let mut cursor = node.walk();
    for ca in node.named_children(&mut cursor) {
        if ca.kind() != "call_argument" {
            continue;
        }
        let mut inner_cursor = ca.walk();
        let Some(inner) = ca.named_children(&mut inner_cursor).next() else {
            continue;
        };
        // `f({a: 1})` carries its names in source already — don't hint this call.
        if inner.kind() == "call_struct_argument" {
            return None;
        }
        let expr = inner_expr(inner);
        let ident = (expr.kind() == "identifier")
            .then(|| expr.utf8_text(src).ok().map(String::from))
            .flatten();
        args.push(Arg { byte: expr.start_byte(), ident });
    }
    (!args.is_empty()).then_some(Call { callee, args })
}

/// The identifier occurrence under `offset`, if any (smallest containing span).
fn ident_at(file: &File, offset: usize) -> Option<&Ident> {
    file.idents
        .iter()
        .filter(|i| i.start <= offset && offset <= i.end)
        .min_by_key(|i| i.end - i.start)
}

fn location(uri: &Url, text: &str, start: usize, end: usize) -> Location {
    let m = PositionMapper::new(text);
    Location::new(uri.clone(), Range::new(m.position(start), m.position(end)))
}

/// Go-to-definition on an import path literal: open the imported file. Resolves
/// only relative paths (`./`, `../`), which are pure filesystem lookups; remapped
/// paths (`@oz/…`) need solc's resolution and are served from the index. Returns
/// `None` when the cursor is not on a path or the target file does not exist.
pub fn import_definition(file: Option<&File>, uri: &Url, pos: Position) -> Option<Location> {
    let file = file?;
    let offset = PositionMapper::new(&file.text).offset(pos);
    let imp = file.imports.iter().find(|i| i.start <= offset && offset <= i.end)?;
    if !imp.path.starts_with('.') {
        return None;
    }
    let dir = uri.to_file_path().ok()?;
    let target = std::fs::canonicalize(dir.parent()?.join(&imp.path)).ok()?;
    let top = Position::new(0, 0);
    Some(Location::new(Url::from_file_path(target).ok()?, Range::new(top, top)))
}

/// Go-to-definition by name: declarations named like the identifier under the
/// cursor, preferring the current file, then any other parsed buffer.
pub fn definition(files: &HashMap<Url, File>, uri: &Url, pos: Position) -> Vec<Location> {
    let Some(file) = files.get(uri) else {
        return Vec::new();
    };
    let offset = PositionMapper::new(&file.text).offset(pos);
    let Some(name) = ident_at(file, offset).map(|i| i.name.clone()) else {
        return Vec::new();
    };
    let same: Vec<Location> = file
        .defs
        .iter()
        .filter(|d| d.name == name)
        .map(|d| location(uri, &file.text, d.name_start, d.name_end))
        .collect();
    if !same.is_empty() {
        return same;
    }
    files
        .iter()
        .filter(|(u, _)| *u != uri)
        .flat_map(|(u, f)| {
            f.defs
                .iter()
                .filter(|d| d.name == name)
                .map(move |d| location(u, &f.text, d.name_start, d.name_end))
        })
        .collect()
}

/// Every occurrence of the identifier under the cursor across all buffers.
pub fn references(
    files: &HashMap<Url, File>,
    uri: &Url,
    pos: Position,
    include_decl: bool,
) -> Vec<Location> {
    let Some(file) = files.get(uri) else {
        return Vec::new();
    };
    let offset = PositionMapper::new(&file.text).offset(pos);
    let Some(name) = ident_at(file, offset).map(|i| i.name.clone()) else {
        return Vec::new();
    };
    files
        .iter()
        .flat_map(|(u, f)| {
            f.idents
                .iter()
                .filter(|i| i.name == name && (include_decl || !i.is_def))
                .map(move |i| location(u, &f.text, i.start, i.end))
        })
        .collect()
}

/// Same-file highlights of the identifier under the cursor (declaration sites
/// flagged WRITE, uses READ), for `textDocument/documentHighlight`.
pub fn highlights(file: &File, pos: Position) -> Vec<DocumentHighlight> {
    let m = PositionMapper::new(&file.text);
    let offset = m.offset(pos);
    let Some(name) = ident_at(file, offset).map(|i| i.name.clone()) else {
        return Vec::new();
    };
    file.idents
        .iter()
        .filter(|i| i.name == name)
        .map(|i| DocumentHighlight {
            range: Range::new(m.position(i.start), m.position(i.end)),
            kind: Some(if i.is_def {
                DocumentHighlightKind::WRITE
            } else {
                DocumentHighlightKind::READ
            }),
        })
        .collect()
}

/// Hover: the rendered header of the declaration the cursor's name resolves to.
pub fn hover(files: &HashMap<Url, File>, uri: &Url, pos: Position) -> Option<Hover> {
    let file = files.get(uri)?;
    let offset = PositionMapper::new(&file.text).offset(pos);
    let name = ident_at(file, offset).map(|i| i.name.clone())?;
    // Prefer a declaration in this file; else any buffer.
    let detail = file
        .defs
        .iter()
        .find(|d| d.name == name)
        .or_else(|| files.values().flat_map(|f| f.defs.iter()).find(|d| d.name == name))
        .map(|d| d.detail.clone())
        .filter(|d| !d.is_empty())?;
    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: format!("```solidity\n{detail}\n```"),
        }),
        range: None,
    })
}

/// Outline for one file: top-level declarations, contract/struct/enum members
/// nested beneath them.
pub fn document_symbols(file: &File) -> Vec<DocumentSymbol> {
    let m = PositionMapper::new(&file.text);
    nested(&file.defs, None, &m)
}

fn nested(defs: &[Def], parent: Option<usize>, m: &PositionMapper) -> Vec<DocumentSymbol> {
    defs.iter()
        .enumerate()
        .filter(|(_, d)| d.container == parent)
        .map(|(i, d)| {
            let children = d
                .kind
                .is_container()
                .then(|| nested(defs, Some(i), m))
                .filter(|c| !c.is_empty());
            let selection = Range::new(m.position(d.name_start), m.position(d.name_end));
            let mut range = Range::new(m.position(d.full_start), m.position(d.full_end));
            if selection.start < range.start {
                range.start = selection.start;
            }
            #[allow(deprecated)]
            DocumentSymbol {
                name: d.name.clone(),
                detail: (!d.detail.is_empty()).then(|| d.detail.clone()),
                kind: d.kind.symbol_kind(),
                tags: None,
                deprecated: None,
                range,
                selection_range: selection,
                children,
            }
        })
        .collect()
}

/// Workspace symbols across all buffers matching `query` (case-insensitive
/// substring), skipping locals and parameters.
pub fn workspace_symbols(files: &HashMap<Url, File>, query: &str) -> Vec<SymbolInformation> {
    let q = query.to_lowercase();
    let mut out = Vec::new();
    for (uri, f) in files {
        for d in &f.defs {
            if matches!(d.kind, DefKind::Local | DefKind::Param) {
                continue;
            }
            if !q.is_empty() && !d.name.to_lowercase().contains(&q) {
                continue;
            }
            #[allow(deprecated)]
            out.push(SymbolInformation {
                name: d.name.clone(),
                kind: d.kind.symbol_kind(),
                tags: None,
                deprecated: None,
                location: location(uri, &f.text, d.name_start, d.name_end),
                container_name: d.container.map(|i| f.defs[i].name.clone()),
            });
        }
    }
    out
}

/// Members for `<container>.` completion. `container` may be an instance (a
/// variable/parameter/field), which resolves to its declared type, or a type /
/// contract / library / struct / enum name used directly. Inherited members are
/// included by walking base contracts; a derived member shadows a base's.
pub fn member_completions(files: &HashMap<Url, File>, container: &str) -> Vec<CompletionItem> {
    // Resolve an instance to its declared type; otherwise use the name directly.
    let target = files
        .values()
        .flat_map(|f| f.defs.iter())
        .find(|d| {
            d.name == container
                && d.type_name.is_some()
                && matches!(
                    d.kind,
                    DefKind::StateVar | DefKind::Local | DefKind::Param | DefKind::Field
                )
        })
        .and_then(|d| d.type_name.clone())
        .unwrap_or_else(|| container.to_string());

    // Inheritance edges by type name.
    let mut bases: HashMap<&str, &[String]> = HashMap::new();
    for d in files.values().flat_map(|f| f.defs.iter()) {
        if !d.bases.is_empty() {
            bases.insert(d.name.as_str(), &d.bases);
        }
    }

    // The target type then its bases, breadth-first (most-derived first).
    let mut order = Vec::new();
    let mut queued: HashSet<String> = HashSet::new();
    let mut queue = std::collections::VecDeque::new();
    queued.insert(target.clone());
    queue.push_back(target);
    while let Some(name) = queue.pop_front() {
        if let Some(bs) = bases.get(name.as_str()) {
            for b in *bs {
                if queued.insert(b.clone()) {
                    queue.push_back(b.clone());
                }
            }
        }
        order.push(name);
    }

    // Members of each type in order, deduped so a derived member shadows a base's.
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for name in &order {
        for f in files.values() {
            for d in &f.defs {
                if d.container.map(|i| f.defs[i].name.as_str()) == Some(name.as_str())
                    && d.kind.is_member()
                    && seen.insert(d.name.clone())
                {
                    out.push(completion_for(d));
                }
            }
        }
    }
    out
}

/// Top-level / contract-level names across all buffers, deduped, for bare
/// identifier completion.
pub fn global_completions(files: &HashMap<Url, File>) -> Vec<CompletionItem> {
    let mut seen = HashSet::new();
    files
        .values()
        .flat_map(|f| f.defs.iter())
        .filter(|d| d.kind.is_global())
        .filter(|d| seen.insert(d.name.clone()))
        .map(completion_for)
        .collect()
}

fn completion_for(d: &Def) -> CompletionItem {
    CompletionItem {
        label: d.name.clone(),
        kind: Some(d.kind.completion_kind()),
        detail: (!d.detail.is_empty()).then(|| d.detail.clone()),
        ..Default::default()
    }
}

/// Call-site parameter-name inlay hints within `range`, resolved by name across
/// the open buffers. Name-based and best-effort (no type inference): a uniquely
/// named callee is used directly, overloads are matched by arity so a hint is
/// never the wrong signature. The caller prefers the accurate, cross-file index;
/// this keeps hints live while typing, before the index is in sync, and through
/// compile errors.
pub fn call_hints(files: &HashMap<Url, File>, uri: &Url, range: Range) -> Vec<InlayHint> {
    let Some(file) = files.get(uri) else {
        return Vec::new();
    };
    // Parameter lists by callee name, gathered from every open buffer.
    let mut by_name: HashMap<&str, Vec<&Vec<String>>> = HashMap::new();
    for f in files.values() {
        for d in &f.defs {
            if d.kind.takes_args() {
                by_name.entry(d.name.as_str()).or_default().push(&d.params);
            }
        }
    }

    let m = PositionMapper::new(&file.text);
    let mut out = Vec::new();
    for call in &file.calls {
        let Some(cands) = by_name.get(call.callee.as_str()) else {
            continue;
        };
        let params = if cands.len() == 1 {
            Some(cands[0])
        } else {
            cands.iter().copied().find(|p| p.len() == call.args.len())
        };
        let Some(params) = params else {
            continue;
        };
        for (i, arg) in call.args.iter().enumerate() {
            let pos = m.position(arg.byte);
            if pos < range.start || pos > range.end {
                continue;
            }
            let Some(name) = params.get(i).filter(|n| !n.is_empty()) else {
                continue;
            };
            // An argument that already spells the parameter needs no hint.
            if arg.ident.as_deref() == Some(name.as_str()) {
                continue;
            }
            out.push(InlayHint {
                position: pos,
                label: InlayHintLabel::String(format!("{name}:")),
                kind: Some(InlayHintKind::PARAMETER),
                text_edits: None,
                tooltip: None,
                padding_left: Some(false),
                padding_right: Some(true),
                data: None,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = r#"
// SPDX-License-Identifier: MIT
pragma solidity 0.8.20;

library MathLib {
    function add(uint a, uint b) internal pure returns (uint) {
        return a + b;
    }
}

contract Counter {
    uint256 public count;
    event Bumped(uint256 newCount);

    function bump(uint256 by) public {
        count = MathLib.add(count, by);
        emit Bumped(count);
    }
}
"#;

    fn pos_of(text: &str, needle: &str) -> Position {
        let byte = text.find(needle).unwrap();
        PositionMapper::new(text).position(byte)
    }

    fn store() -> (HashMap<Url, File>, Url) {
        let uri = Url::parse("file:///Counter.sol").unwrap();
        let mut files = HashMap::new();
        files.insert(uri.clone(), parse(SRC));
        (files, uri)
    }

    #[test]
    fn finds_definitions_and_outline() {
        let file = parse(SRC);
        // The outline has the library and the contract at the top level.
        let syms = document_symbols(&file);
        let names: Vec<_> = syms.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"MathLib"));
        assert!(names.contains(&"Counter"));
        // Counter nests its members.
        let counter = syms.iter().find(|s| s.name == "Counter").unwrap();
        let members: Vec<_> = counter
            .children
            .as_ref()
            .unwrap()
            .iter()
            .map(|c| c.name.as_str())
            .collect();
        assert!(members.contains(&"count"));
        assert!(members.contains(&"bump"));
        assert!(members.contains(&"Bumped"));
    }

    #[test]
    fn go_to_definition_by_name() {
        let (files, uri) = store();
        // Cursor on the `add` call resolves to MathLib.add's declaration.
        let at = pos_of(SRC, "add(count, by)");
        let defs = definition(&files, &uri, at);
        assert_eq!(defs.len(), 1);
        let def_pos = pos_of(SRC, "add(uint a");
        assert_eq!(defs[0].range.start, def_pos);
    }

    #[test]
    fn references_span_uses_and_decl() {
        let (files, uri) = store();
        let at = pos_of(SRC, "count;"); // the state variable declaration
        let with_decl = references(&files, &uri, at, true);
        let without = references(&files, &uri, at, false);
        // `count` appears: decl + count=MathLib.add(count,by) (x2) + emit Bumped(count).
        assert!(with_decl.len() >= 4);
        assert_eq!(without.len(), with_decl.len() - 1);
    }

    #[test]
    fn member_completion_lists_container_members() {
        let (files, _) = store();
        let items = member_completions(&files, "MathLib");
        assert!(items.iter().any(|i| i.label == "add"));
        let counter = member_completions(&files, "Counter");
        assert!(counter.iter().any(|i| i.label == "count"));
        assert!(counter.iter().any(|i| i.label == "bump"));
        // Locals/params never leak into member completion.
        assert!(!counter.iter().any(|i| i.label == "by"));
    }

    #[test]
    fn hover_renders_header() {
        let (files, uri) = store();
        let at = pos_of(SRC, "add(count, by)");
        let h = hover(&files, &uri, at).unwrap();
        let HoverContents::Markup(mc) = h.contents else { panic!() };
        assert!(mc.value.contains("function add(uint a, uint b) internal pure returns (uint)"));
    }

    #[test]
    fn call_hints_label_arguments_by_name() {
        let (files, uri) = store();
        let m = PositionMapper::new(SRC);
        let full = Range::new(m.position(0), m.position(SRC.len()));
        let labels: Vec<String> = call_hints(&files, &uri, full)
            .iter()
            .map(|h| match &h.label {
                InlayHintLabel::String(s) => s.clone(),
                _ => String::new(),
            })
            .collect();
        // MathLib.add(count, by) -> a:, b:  and  emit Bumped(count) -> newCount:
        assert!(labels.contains(&"a:".to_string()), "{labels:?}");
        assert!(labels.contains(&"b:".to_string()), "{labels:?}");
        assert!(labels.contains(&"newCount:".to_string()), "{labels:?}");
    }

    #[test]
    fn member_completion_resolves_instance_type_and_inheritance() {
        const SRC: &str = r#"
struct Point { uint256 x; uint256 y; }

contract Base {
    uint256 public baseVar;
    function baseFn() public {}
}

contract Token is Base {
    Point public p;
    function f(Point memory pt) public {
        uint256 z = pt.x;
    }
}
"#;
        let uri = Url::parse("file:///T.sol").unwrap();
        let mut files = HashMap::new();
        files.insert(uri, parse(SRC));
        let labels = |v: &[CompletionItem]| v.iter().map(|i| i.label.clone()).collect::<Vec<_>>();

        // A parameter of struct type completes that struct's fields.
        let pt = labels(&member_completions(&files, "pt"));
        assert!(pt.contains(&"x".to_string()), "{pt:?}");
        assert!(pt.contains(&"y".to_string()), "{pt:?}");
        // A state variable of struct type, likewise.
        assert!(labels(&member_completions(&files, "p")).contains(&"x".to_string()));
        // A contract name lists its own and inherited members.
        let token = labels(&member_completions(&files, "Token"));
        for m in ["f", "p", "baseFn", "baseVar"] {
            assert!(token.contains(&m.to_string()), "missing {m}: {token:?}");
        }
    }

    #[test]
    fn import_path_opens_the_relative_file() {
        // A sibling file on disk; clicking inside the import path resolves to it.
        let dir = std::env::temp_dir().join(format!("sfi-import-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let b = dir.join("B.sol");
        std::fs::write(&b, "contract B {}").unwrap();

        let src = "import { Token } from \"./B.sol\";\ncontract A {}";
        let file = parse(src);
        // The path literal (sans quotes) was recorded.
        assert_eq!(file.imports.len(), 1, "import not recorded");
        assert_eq!(file.imports[0].path, "./B.sol");

        let uri = Url::from_file_path(dir.join("A.sol")).unwrap();
        let mut files = HashMap::new();
        files.insert(uri.clone(), file);

        // Cursor on the path string jumps to the top of B.sol.
        let at = pos_of(src, "B.sol");
        let loc = import_definition(files.get(&uri), &uri, at).unwrap();
        let want = Url::from_file_path(std::fs::canonicalize(&b).unwrap()).unwrap();
        assert_eq!(loc.uri, want);
        assert_eq!(loc.range.start, Position::new(0, 0));

        // The imported symbol (`Token`) is not a path — name-based def handles it.
        assert!(import_definition(files.get(&uri), &uri, pos_of(src, "Token }")).is_none());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn document_symbols_survive_name_cycles() {
        // A member sharing its container's name used to recurse forever (nesting
        // matched by name); nesting by identity terminates.
        let direct = parse("contract A { struct A { uint256 x; } }");
        let syms = document_symbols(&direct);
        let a = syms.iter().find(|s| s.name == "A").unwrap();
        assert!(a.children.as_ref().unwrap().iter().any(|c| c.name == "A"));

        // The indirect cycle across two contracts, likewise.
        let indirect =
            parse("contract A { struct B { uint256 x; } } contract B { struct A { uint256 y; } }");
        let syms = document_symbols(&indirect);
        assert_eq!(syms.iter().filter(|s| s.name == "A" || s.name == "B").count(), 2);
    }

    #[test]
    fn tolerates_syntax_errors() {
        // A half-typed function must still index the surrounding declarations.
        let broken = "contract C {\n  uint x;\n  function f(  \n}";
        let file = parse(broken);
        let syms = document_symbols(&file);
        assert!(syms.iter().any(|s| s.name == "C"));
    }
}

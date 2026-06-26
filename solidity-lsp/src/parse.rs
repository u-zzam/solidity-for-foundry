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
    Hover, HoverContents, Location, MarkupContent, MarkupKind, Position, Range, SymbolInformation,
    SymbolKind, Url,
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
    /// Enclosing contract/interface/library/struct/enum, for member completion
    /// and outline nesting.
    container: Option<String>,
    /// A rendered header (`function f(uint a) public`) for hover/completion.
    detail: String,
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

/// A single parsed buffer.
pub struct File {
    pub text: String,
    defs: Vec<Def>,
    idents: Vec<Ident>,
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
    if let Some(tree) = tree {
        let src = text.as_bytes();
        walk(tree.root_node(), src, None, &mut defs, &mut idents);
        // Mark identifier occurrences that coincide with a declaration's name.
        let def_spans: HashSet<(usize, usize)> =
            defs.iter().map(|d| (d.name_start, d.name_end)).collect();
        for id in &mut idents {
            id.is_def = def_spans.contains(&(id.start, id.end));
        }
    }
    File { text: text.to_string(), defs, idents }
}

/// Recursive traversal: record declarations (carrying their enclosing type
/// scope) and every identifier occurrence.
fn walk<'a>(
    node: Node<'a>,
    src: &[u8],
    container: Option<&str>,
    defs: &mut Vec<Def>,
    idents: &mut Vec<Ident>,
) {
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

    if let Some((name, kind, ns, ne)) = def_of(node, src) {
        let detail = header(node, src);
        defs.push(Def {
            name: name.clone(),
            kind,
            name_start: ns,
            name_end: ne,
            full_start: node.start_byte(),
            full_end: node.end_byte(),
            container: container.map(str::to_string),
            detail,
        });
        // Descend with this node as the container if it is a type scope.
        let inner = kind.is_container().then_some(name);
        let child_container = inner.as_deref().or(container);
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            walk(child, src, child_container, defs, idents);
        }
        return;
    }

    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        walk(child, src, container, defs, idents);
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

fn nested(defs: &[Def], parent: Option<&str>, m: &PositionMapper) -> Vec<DocumentSymbol> {
    defs.iter()
        .filter(|d| d.container.as_deref() == parent)
        .map(|d| {
            let children = d
                .kind
                .is_container()
                .then(|| nested(defs, Some(&d.name), m))
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
                container_name: d.container.clone(),
            });
        }
    }
    out
}

/// Members of a named container (`Lib.` / `Contract.` / struct / enum), deduped.
pub fn member_completions(files: &HashMap<Url, File>, container: &str) -> Vec<CompletionItem> {
    let mut seen = HashSet::new();
    files
        .values()
        .flat_map(|f| f.defs.iter())
        .filter(|d| d.container.as_deref() == Some(container) && d.kind.is_member())
        .filter(|d| seen.insert(d.name.clone()))
        .map(completion_for)
        .collect()
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
    fn tolerates_syntax_errors() {
        // A half-typed function must still index the surrounding declarations.
        let broken = "contract C {\n  uint x;\n  function f(  \n}";
        let file = parse(broken);
        let syms = document_symbols(&file);
        assert!(syms.iter().any(|s| s.name == "C"));
    }
}

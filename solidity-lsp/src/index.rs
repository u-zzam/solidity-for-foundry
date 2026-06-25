//! Engine B (navigation): a symbol index built from the typed solc AST that the
//! diagnostics compile already produces.
//!
//! solc tags every identifier with `referencedDeclaration` (the id of the
//! declaration it resolves to) and every expression with `typeDescriptions`, so
//! go-to-definition, find-references and hover are mostly "consume the AST",
//! not "build a type checker". We walk each source's AST generically: any node
//! with a `nameLocation` is a declaration; any node with a
//! `referencedDeclaration` is a reference.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use tower_lsp::lsp_types::{
    DocumentSymbol, Location, Position, Range, SymbolInformation, SymbolKind, TextEdit, Url,
    WorkspaceEdit,
};

use crate::diagnostics::PositionMapper;
use crate::project::SourceAst;

/// A declaration solc resolved an identifier to.
struct Decl {
    src_index: usize,
    name_start: usize,
    name_end: usize,
    name: String,
    kind: String,
    type_string: Option<String>,
    /// Pre-rendered signature for callables (functions/events/errors/modifiers).
    signature: Option<String>,
    doc: Option<String>,
}

impl Decl {
    fn signature_text(&self) -> String {
        if let Some(s) = &self.signature {
            return s.clone();
        }
        match self.kind.as_str() {
            "VariableDeclaration" => match &self.type_string {
                Some(t) => format!("{t} {}", self.name),
                None => self.name.clone(),
            },
            "ContractDefinition" => format!("contract {}", self.name),
            "StructDefinition" => format!("struct {}", self.name),
            "EnumDefinition" => format!("enum {}", self.name),
            "UserDefinedValueTypeDefinition" => format!("type {}", self.name),
            "EnumValue" => self.name.clone(),
            _ => format!("{} {}", self.kind, self.name),
        }
    }
}

/// A reference token (where an identifier appears), used to find references.
struct RefSpan {
    src_index: usize,
    start: usize,
    end: usize,
}

/// One indexed source file.
struct FileEntry {
    uri: Url,
    text: String,
    /// Clickable spans (references + declaration names) mapping to a declaration.
    spans: Vec<Span>,
    symbols: Vec<DocumentSymbol>,
}

struct Span {
    start: usize,
    end: usize,
    decl: i64,
}

pub struct Index {
    files: HashMap<usize, FileEntry>,
    path_to_index: HashMap<PathBuf, usize>,
    decls: HashMap<i64, Decl>,
    refs_by_decl: HashMap<i64, Vec<RefSpan>>,
}

impl Index {
    pub fn build(sources: &[SourceAst]) -> Self {
        let mut files = HashMap::new();
        let mut path_to_index = HashMap::new();
        let mut decls: HashMap<i64, Decl> = HashMap::new();
        let mut refs_by_decl: HashMap<i64, Vec<RefSpan>> = HashMap::new();

        for s in sources {
            let Ok(text) = std::fs::read_to_string(&s.path) else {
                continue;
            };
            let canon = std::fs::canonicalize(&s.path).unwrap_or_else(|_| s.path.clone());
            let Ok(uri) = Url::from_file_path(&canon) else {
                continue;
            };
            path_to_index.insert(canon, s.index);

            let mapper = PositionMapper::new(&text);
            let symbols = doc_symbols(&s.ast, &mapper);

            let mut spans = Vec::new();
            walk(&s.ast, &mut |map| {
                // Declaration: has a name location.
                if let (Some(id), Some(name), Some((start, len))) = (
                    geti(map, "id"),
                    gets(map, "name"),
                    gets(map, "nameLocation").and_then(parse_src),
                ) {
                    if !name.is_empty() {
                        spans.push(Span { start, end: start + len, decl: id });
                        decls.entry(id).or_insert_with(|| Decl {
                            src_index: s.index,
                            name_start: start,
                            name_end: start + len,
                            name: name.to_string(),
                            kind: gets(map, "nodeType").unwrap_or_default().to_string(),
                            type_string: type_string(map),
                            signature: signature(map),
                            doc: documentation(map),
                        });
                        return;
                    }
                }
                // Reference: points at a declaration.
                if let Some(refid) = geti(map, "referencedDeclaration") {
                    if refid >= 0 {
                        let loc = gets(map, "memberLocation")
                            .and_then(parse_src)
                            .or_else(|| gets(map, "src").and_then(parse_src));
                        if let Some((start, len)) = loc {
                            if len > 0 {
                                spans.push(Span { start, end: start + len, decl: refid });
                                refs_by_decl.entry(refid).or_default().push(RefSpan {
                                    src_index: s.index,
                                    start,
                                    end: start + len,
                                });
                            }
                        }
                    }
                }
            });

            files.insert(s.index, FileEntry { uri, text, spans, symbols });
        }

        Self { files, path_to_index, decls, refs_by_decl }
    }

    pub fn definition(&self, path: &Path, pos: Position) -> Option<Location> {
        let id = self.resolve(path, pos)?;
        self.decl_location(id)
    }

    pub fn references(&self, path: &Path, pos: Position, include_decl: bool) -> Vec<Location> {
        let Some(id) = self.resolve(path, pos) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        if let Some(refs) = self.refs_by_decl.get(&id) {
            out.extend(refs.iter().filter_map(|r| self.location(r.src_index, r.start, r.end)));
        }
        if include_decl {
            if let Some(loc) = self.decl_location(id) {
                out.push(loc);
            }
        }
        out
    }

    /// Markdown hover: a signature code block plus any NatSpec.
    pub fn hover(&self, path: &Path, pos: Position) -> Option<String> {
        let id = self.resolve(path, pos)?;
        let d = self.decls.get(&id)?;
        let mut md = format!("```solidity\n{}\n```", d.signature_text());
        if let Some(doc) = &d.doc {
            let doc = doc.trim();
            if !doc.is_empty() {
                md.push_str("\n\n");
                md.push_str(doc);
            }
        }
        Some(md)
    }

    /// Rename the declaration under the cursor everywhere it is referenced.
    pub fn rename(&self, path: &Path, pos: Position, new_name: &str) -> Option<WorkspaceEdit> {
        let id = self.resolve(path, pos)?;
        let mut changes: HashMap<Url, Vec<TextEdit>> = HashMap::new();
        let mut push = |loc: Location| {
            changes.entry(loc.uri).or_default().push(TextEdit {
                range: loc.range,
                new_text: new_name.to_string(),
            });
        };
        if let Some(refs) = self.refs_by_decl.get(&id) {
            for r in refs {
                if let Some(loc) = self.location(r.src_index, r.start, r.end) {
                    push(loc);
                }
            }
        }
        if let Some(loc) = self.decl_location(id) {
            push(loc);
        }
        (!changes.is_empty()).then(|| WorkspaceEdit {
            changes: Some(changes),
            ..Default::default()
        })
    }

    pub fn document_symbols(&self, path: &Path) -> Vec<DocumentSymbol> {
        self.slot_for(path)
            .and_then(|i| self.files.get(&i))
            .map(|f| f.symbols.clone())
            .unwrap_or_default()
    }

    pub fn workspace_symbols(&self, query: &str) -> Vec<SymbolInformation> {
        let q = query.to_lowercase();
        let mut out = Vec::new();
        for (id, d) in &self.decls {
            if !q.is_empty() && !d.name.to_lowercase().contains(&q) {
                continue;
            }
            if let Some(location) = self.decl_location(*id) {
                #[allow(deprecated)]
                out.push(SymbolInformation {
                    name: d.name.clone(),
                    kind: symbol_kind(&d.kind),
                    tags: None,
                    deprecated: None,
                    location,
                    container_name: None,
                });
            }
        }
        out
    }

    fn slot_for(&self, path: &Path) -> Option<usize> {
        let canon = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        self.path_to_index
            .get(&canon)
            .or_else(|| self.path_to_index.get(path))
            .copied()
    }

    /// Resolve the cursor to the declaration id of the innermost span under it.
    fn resolve(&self, path: &Path, pos: Position) -> Option<i64> {
        let idx = self.slot_for(path)?;
        let f = self.files.get(&idx)?;
        let offset = PositionMapper::new(&f.text).offset(pos);
        f.spans
            .iter()
            .filter(|s| s.start <= offset && offset < s.end)
            .min_by_key(|s| s.end - s.start)
            .map(|s| s.decl)
    }

    fn decl_location(&self, id: i64) -> Option<Location> {
        let d = self.decls.get(&id)?;
        self.location(d.src_index, d.name_start, d.name_end)
    }

    fn location(&self, src_index: usize, start: usize, end: usize) -> Option<Location> {
        let f = self.files.get(&src_index)?;
        let m = PositionMapper::new(&f.text);
        Some(Location::new(
            f.uri.clone(),
            Range::new(m.position(start), m.position(end)),
        ))
    }
}

/// Recursively visit every AST object that looks like a node (`nodeType` + `src`).
fn walk(value: &Value, visit: &mut impl FnMut(&Map<String, Value>)) {
    match value {
        Value::Object(map) => {
            if map.contains_key("nodeType") && map.contains_key("src") {
                visit(map);
            }
            for v in map.values() {
                walk(v, visit);
            }
        }
        Value::Array(arr) => {
            for v in arr {
                walk(v, visit);
            }
        }
        _ => {}
    }
}

/// Build a hierarchical document outline: top-level items, with contract members nested.
fn doc_symbols(ast: &Value, mapper: &PositionMapper) -> Vec<DocumentSymbol> {
    ast.get("nodes")
        .and_then(|n| n.as_array())
        .map(|nodes| nodes.iter().filter_map(|n| node_symbol(n, mapper)).collect())
        .unwrap_or_default()
}

fn node_symbol(node: &Value, mapper: &PositionMapper) -> Option<DocumentSymbol> {
    let map = node.as_object()?;
    let kind = gets(map, "nodeType")?;
    let name = gets(map, "name")?;
    if name.is_empty() {
        return None;
    }
    let (ns, nl) = gets(map, "nameLocation").and_then(parse_src)?;
    let (fs, fl) = gets(map, "src").and_then(parse_src)?;
    let selection = Range::new(mapper.position(ns), mapper.position(ns + nl));
    let mut range = Range::new(mapper.position(fs), mapper.position(fs + fl));
    // selection_range must be contained in range.
    if selection.start < range.start {
        range.start = selection.start;
    }

    let children = (kind == "ContractDefinition").then(|| {
        node.get("nodes")
            .and_then(|n| n.as_array())
            .map(|arr| arr.iter().filter_map(|c| node_symbol(c, mapper)).collect())
            .unwrap_or_default()
    });

    #[allow(deprecated)]
    Some(DocumentSymbol {
        name: name.to_string(),
        detail: type_string(map),
        kind: symbol_kind(kind),
        tags: None,
        deprecated: None,
        range,
        selection_range: selection,
        children,
    })
}

fn symbol_kind(node_type: &str) -> SymbolKind {
    match node_type {
        "ContractDefinition" => SymbolKind::CLASS,
        "FunctionDefinition" => SymbolKind::METHOD,
        "ModifierDefinition" => SymbolKind::FUNCTION,
        "VariableDeclaration" => SymbolKind::FIELD,
        "EventDefinition" => SymbolKind::EVENT,
        "ErrorDefinition" => SymbolKind::OBJECT,
        "StructDefinition" => SymbolKind::STRUCT,
        "EnumDefinition" => SymbolKind::ENUM,
        "EnumValue" => SymbolKind::ENUM_MEMBER,
        "UserDefinedValueTypeDefinition" => SymbolKind::STRUCT,
        _ => SymbolKind::VARIABLE,
    }
}

/// Render a callable's signature from its AST node, e.g.
/// `function increment(uint256 by) public`.
fn signature(map: &Map<String, Value>) -> Option<String> {
    let kind = gets(map, "nodeType")?;
    let name = gets(map, "name").unwrap_or_default();
    match kind {
        "FunctionDefinition" => {
            let fn_kind = gets(map, "kind").unwrap_or("function");
            let mut s = match fn_kind {
                "constructor" => "constructor".to_string(),
                "fallback" => "fallback".to_string(),
                "receive" => "receive".to_string(),
                _ => format!("function {name}"),
            };
            s.push_str(&format!("({})", params(map, "parameters")));
            if let Some(v) = gets(map, "visibility").filter(|v| !v.is_empty()) {
                s.push(' ');
                s.push_str(v);
            }
            if let Some(m) = gets(map, "stateMutability").filter(|m| *m != "nonpayable") {
                s.push(' ');
                s.push_str(m);
            }
            let rets = params(map, "returnParameters");
            if !rets.is_empty() {
                s.push_str(&format!(" returns ({rets})"));
            }
            Some(s)
        }
        "ModifierDefinition" => Some(format!("modifier {name}({})", params(map, "parameters"))),
        "EventDefinition" => Some(format!("event {name}({})", params(map, "parameters"))),
        "ErrorDefinition" => Some(format!("error {name}({})", params(map, "parameters"))),
        _ => None,
    }
}

/// Render a `ParameterList` field as `type name, type name`.
fn params(map: &Map<String, Value>, key: &str) -> String {
    let Some(list) = map.get(key).and_then(|p| p.get("parameters")).and_then(|a| a.as_array())
    else {
        return String::new();
    };
    list.iter()
        .map(|p| {
            let ty = p
                .get("typeDescriptions")
                .and_then(|t| t.get("typeString"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            match p.get("name").and_then(|v| v.as_str()).filter(|n| !n.is_empty()) {
                Some(n) => format!("{ty} {n}"),
                None => ty.to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn gets<'a>(map: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    map.get(key).and_then(|v| v.as_str())
}

fn geti(map: &Map<String, Value>, key: &str) -> Option<i64> {
    map.get(key).and_then(|v| v.as_i64())
}

fn type_string(map: &Map<String, Value>) -> Option<String> {
    map.get("typeDescriptions")
        .and_then(|t| t.get("typeString"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from)
}

fn documentation(map: &Map<String, Value>) -> Option<String> {
    match map.get("documentation") {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Object(o)) => o.get("text").and_then(|v| v.as_str()).map(String::from),
        _ => None,
    }
}

/// Parse a solc `start:length:index` location, returning `(start, length)` when
/// the length is present (>= 0).
fn parse_src(s: &str) -> Option<(usize, usize)> {
    let mut it = s.split(':');
    let start: usize = it.next()?.parse().ok()?;
    let len: isize = it.next()?.parse().ok()?;
    (len >= 0).then_some((start, len as usize))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_src_locations() {
        assert_eq!(parse_src("12:5:0"), Some((12, 5)));
        assert_eq!(parse_src("12:-1:-1"), None);
        assert_eq!(parse_src("bad"), None);
    }
}

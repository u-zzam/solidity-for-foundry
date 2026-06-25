//! Engine B (navigation): a symbol index built from the typed solc AST that the
//! diagnostics compile already produces.
//!
//! solc tags every identifier with `referencedDeclaration` (the id of the
//! declaration it resolves to) and every expression with `typeDescriptions`, so
//! go-to-definition, find-references and hover are mostly "consume the AST",
//! not "build a type checker". We walk each source's AST generically: any node
//! with a `nameLocation` is a declaration; any node with a
//! `referencedDeclaration` is a reference.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use tower_lsp::lsp_types::{
    CompletionItem, CompletionItemKind, Documentation, DocumentSymbol, InlayHint, InlayHintKind,
    InlayHintLabel, Location, ParameterInformation, ParameterLabel, Position, Range, SemanticToken,
    SemanticTokenType, SignatureHelp, SignatureInformation, SymbolInformation, SymbolKind, TextEdit,
    Url, WorkspaceEdit,
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
    /// Parameter labels (`type name`) for callables, for signature help.
    params: Vec<String>,
    /// Parameter names only (callables' params or a struct's fields, in order),
    /// for call-site inlay hints. Empty string for an unnamed parameter.
    param_names: Vec<String>,
    doc: Option<String>,
}

/// A member of a contract / library / struct / enum, for `.` completion.
struct Member {
    name: String,
    kind: String,
    detail: String,
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
    /// Pre-resolved call-site parameter-name inlay hints (position + label).
    hints: Vec<(Position, String)>,
    /// Delta-encoded semantic tokens for the whole file.
    tokens: Vec<SemanticToken>,
}

struct Span {
    start: usize,
    end: usize,
    decl: i64,
}

/// A call argument awaiting its callee's parameter name (resolved after the
/// whole project is indexed, since the callee may be declared elsewhere).
struct PendingHint {
    src_index: usize,
    byte: usize,
    /// The callee declaration the call resolved to.
    callee: i64,
    arg_index: usize,
    /// Number of written arguments at the call site. For a `using for` bound
    /// call (`x.f(a)`) the callee's parameter list includes the receiver, so it
    /// has one more entry than this; the resolver shifts the index to match.
    arg_count: usize,
    /// If the argument is a bare identifier, its name — used to drop hints that
    /// would just echo the argument (`transfer(to, amount)`).
    arg_ident: Option<String>,
}

pub struct Index {
    files: HashMap<usize, FileEntry>,
    path_to_index: HashMap<PathBuf, usize>,
    decls: HashMap<i64, Decl>,
    refs_by_decl: HashMap<i64, Vec<RefSpan>>,
    /// Container name (contract/library/struct/enum) -> its members.
    containers: HashMap<String, Vec<Member>>,
    /// Callable name -> declaration ids, for signature help (overloads included).
    callables: HashMap<String, Vec<i64>>,
    /// Declaration ids that are top-level (direct children of a source unit),
    /// i.e. importable by name. Used to suggest imports for unresolved symbols.
    top_level: HashSet<i64>,
}

impl Index {
    pub fn build(sources: &[SourceAst]) -> Self {
        let mut files = HashMap::new();
        let mut path_to_index = HashMap::new();
        let mut decls: HashMap<i64, Decl> = HashMap::new();
        let mut refs_by_decl: HashMap<i64, Vec<RefSpan>> = HashMap::new();
        let mut containers: HashMap<String, Vec<Member>> = HashMap::new();
        let mut callables: HashMap<String, Vec<i64>> = HashMap::new();
        let mut pending: Vec<PendingHint> = Vec::new();
        // Declaration ids that are function/event/error/modifier parameters, so
        // their tokens (and references) color as parameters, not variables.
        let mut param_ids: HashSet<i64> = HashSet::new();
        let mut top_level: HashSet<i64> = HashSet::new();

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
                        let kind = gets(map, "nodeType").unwrap_or_default();
                        if matches!(
                            kind,
                            "FunctionDefinition"
                                | "EventDefinition"
                                | "ErrorDefinition"
                                | "ModifierDefinition"
                        ) {
                            callables.entry(name.to_string()).or_default().push(id);
                            collect_param_ids(map, &mut param_ids);
                        }
                        decls.entry(id).or_insert_with(|| Decl {
                            src_index: s.index,
                            name_start: start,
                            name_end: start + len,
                            name: name.to_string(),
                            kind: kind.to_string(),
                            type_string: type_string(map),
                            signature: signature(map),
                            params: param_list(map, "parameters"),
                            param_names: decl_param_names(map),
                            doc: documentation(map),
                        });
                        return;
                    }
                }
                // Call site: queue a parameter-name hint per positional argument.
                if gets(map, "nodeType") == Some("FunctionCall") {
                    collect_call_hints(map, s.index, &mut pending);
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

            collect_containers(&s.ast, &mut containers);
            // Top-level (source-unit) declarations are the ones importable by name.
            if let Some(nodes) = s.ast.get("nodes").and_then(|n| n.as_array()) {
                for n in nodes {
                    if let (Some(id), Some(name)) = (
                        n.get("id").and_then(|v| v.as_i64()),
                        n.get("name").and_then(|v| v.as_str()),
                    ) {
                        if !name.is_empty() {
                            top_level.insert(id);
                        }
                    }
                }
            }
            files.insert(
                s.index,
                FileEntry { uri, text, spans, symbols, hints: Vec::new(), tokens: Vec::new() },
            );
        }

        // Color every clickable span by what it resolves to (declarations and
        // references alike). Needs the full `decls` map, so it runs after the
        // per-file loop.
        for f in files.values_mut() {
            let mapper = PositionMapper::new(&f.text);
            f.tokens = encode_tokens(&f.spans, &decls, &param_ids, &mapper);
        }

        // Resolve queued call-site hints now that every declaration is known
        // (a call's callee may be declared in any indexed file).
        for h in pending {
            let Some(decl) = decls.get(&h.callee) else {
                continue;
            };
            // `using for` calls inject the receiver as the first parameter, so
            // the parameter list is one longer than the written arguments; the
            // offset realigns argument N with its real parameter.
            let offset = decl.param_names.len().saturating_sub(h.arg_count);
            let Some(label) =
                param_hint(&decl.param_names, h.arg_index + offset, h.arg_ident.as_deref())
            else {
                continue;
            };
            if let Some(f) = files.get_mut(&h.src_index) {
                let pos = PositionMapper::new(&f.text).position(h.byte);
                f.hints.push((pos, label));
            }
        }

        Self { files, path_to_index, decls, refs_by_decl, containers, callables, top_level }
    }

    /// Files declaring a top-level symbol named `name`, for import suggestions.
    pub fn import_candidates(&self, name: &str) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = Vec::new();
        for (id, d) in &self.decls {
            if d.name == name && self.top_level.contains(id) {
                if let Some(p) = self.files.get(&d.src_index).and_then(|f| f.uri.to_file_path().ok())
                {
                    if !out.contains(&p) {
                        out.push(p);
                    }
                }
            }
        }
        out
    }

    /// Call-site parameter-name inlay hints within `range`.
    pub fn inlay_hints(&self, path: &Path, range: Range) -> Vec<InlayHint> {
        let Some(f) = self.slot_for(path).and_then(|i| self.files.get(&i)) else {
            return Vec::new();
        };
        f.hints
            .iter()
            .filter(|(pos, _)| range.start <= *pos && *pos <= range.end)
            .map(|(pos, label)| InlayHint {
                position: *pos,
                label: InlayHintLabel::String(label.clone()),
                kind: Some(InlayHintKind::PARAMETER),
                text_edits: None,
                tooltip: None,
                padding_left: Some(false),
                padding_right: Some(true),
                data: None,
            })
            .collect()
    }

    /// Whole-file semantic tokens (delta-encoded), matching `token_legend`.
    pub fn semantic_tokens(&self, path: &Path) -> Vec<SemanticToken> {
        self.slot_for(path)
            .and_then(|i| self.files.get(&i))
            .map(|f| f.tokens.clone())
            .unwrap_or_default()
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

    /// Members of a named container (`Lib.` / `Contract.` / struct / enum).
    pub fn member_completions(&self, container: &str) -> Vec<CompletionItem> {
        self.containers
            .get(container)
            .map(|members| {
                members
                    .iter()
                    .map(|m| CompletionItem {
                        label: m.name.clone(),
                        kind: Some(completion_kind(&m.kind)),
                        detail: (!m.detail.is_empty()).then(|| m.detail.clone()),
                        ..Default::default()
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Every declared symbol name (deduped) for in-scope-ish completion.
    pub fn global_completions(&self) -> Vec<CompletionItem> {
        let mut seen = HashSet::new();
        self.decls
            .values()
            .filter(|d| seen.insert(d.name.as_str()))
            .map(|d| CompletionItem {
                label: d.name.clone(),
                kind: Some(completion_kind(&d.kind)),
                detail: d.signature.clone().or_else(|| d.type_string.clone()),
                ..Default::default()
            })
            .collect()
    }

    /// Signature help for a callable by name (all overloads).
    pub fn signatures(&self, callee: &str, active: u32) -> Option<SignatureHelp> {
        let ids = self.callables.get(callee)?;
        let signatures: Vec<SignatureInformation> = ids
            .iter()
            .filter_map(|id| self.decls.get(id))
            .map(|d| SignatureInformation {
                label: d.signature.clone().unwrap_or_else(|| d.name.clone()),
                documentation: d.doc.clone().map(Documentation::String),
                parameters: Some(
                    d.params
                        .iter()
                        .map(|p| ParameterInformation {
                            label: ParameterLabel::Simple(p.clone()),
                            documentation: None,
                        })
                        .collect(),
                ),
                active_parameter: Some(active),
            })
            .collect();
        (!signatures.is_empty()).then_some(SignatureHelp {
            signatures,
            active_signature: Some(0),
            active_parameter: Some(active),
        })
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
    param_list(map, key).join(", ")
}

/// Each parameter of a `ParameterList` field as a `type name` label.
fn param_list(map: &Map<String, Value>, key: &str) -> Vec<String> {
    let Some(list) = map.get(key).and_then(|p| p.get("parameters")).and_then(|a| a.as_array())
    else {
        return Vec::new();
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
        .collect()
}

/// Ordered parameter names for a callable (its `parameters`) or a struct (its
/// `members`), so a call site can label each positional argument. Unnamed
/// parameters yield an empty string.
fn decl_param_names(map: &Map<String, Value>) -> Vec<String> {
    let arr = match gets(map, "nodeType") {
        Some("StructDefinition") => map.get("members").and_then(|m| m.as_array()),
        _ => map
            .get("parameters")
            .and_then(|p| p.get("parameters"))
            .and_then(|a| a.as_array()),
    };
    arr.map(|list| {
        list.iter()
            .map(|p| p.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string())
            .collect()
    })
    .unwrap_or_default()
}

/// Queue a parameter-name hint for each positional argument of a call. Skips
/// named-argument calls (`f({a: 1})`) and type conversions (`uint8(x)`), which
/// carry no parameter names worth surfacing.
fn collect_call_hints(map: &Map<String, Value>, src_index: usize, out: &mut Vec<PendingHint>) {
    match gets(map, "kind") {
        Some("functionCall") | Some("structConstructorCall") => {}
        _ => return,
    }
    // Named arguments already show their names in source.
    if map.get("names").and_then(|n| n.as_array()).is_some_and(|a| !a.is_empty()) {
        return;
    }
    let Some(callee) = map
        .get("expression")
        .and_then(|e| e.as_object())
        .and_then(|e| geti(e, "referencedDeclaration"))
        .filter(|id| *id >= 0)
    else {
        return;
    };
    let Some(args) = map.get("arguments").and_then(|a| a.as_array()) else {
        return;
    };
    let arg_count = args.len();
    for (i, arg) in args.iter().enumerate() {
        let Some((byte, _)) = arg.get("src").and_then(|s| s.as_str()).and_then(parse_src) else {
            continue;
        };
        let arg_ident = (arg.get("nodeType").and_then(|t| t.as_str()) == Some("Identifier"))
            .then(|| arg.get("name").and_then(|v| v.as_str()).map(String::from))
            .flatten();
        out.push(PendingHint { src_index, byte, callee, arg_index: i, arg_count, arg_ident });
    }
}

/// The inlay label for one argument, or `None` if no hint should show: the
/// callee has no name for that position, the parameter is unnamed, or the
/// argument is just the parameter's name spelled out.
fn param_hint(param_names: &[String], arg_index: usize, arg_ident: Option<&str>) -> Option<String> {
    let name = param_names.get(arg_index).filter(|n| !n.is_empty())?;
    if arg_ident == Some(name.as_str()) {
        return None;
    }
    Some(format!("{name}:"))
}

/// The semantic-token legend, in the order `token_index` encodes. The server
/// advertises this in its capabilities so the editor can decode the tokens.
pub fn token_legend() -> Vec<SemanticTokenType> {
    vec![
        SemanticTokenType::CLASS,       // 0: contracts / libraries / interfaces
        SemanticTokenType::STRUCT,      // 1
        SemanticTokenType::ENUM,        // 2
        SemanticTokenType::ENUM_MEMBER, // 3
        SemanticTokenType::EVENT,       // 4
        SemanticTokenType::FUNCTION,    // 5: functions / modifiers
        SemanticTokenType::TYPE,        // 6: errors / user-defined value types
        SemanticTokenType::PARAMETER,   // 7
        SemanticTokenType::VARIABLE,    // 8: state variables / locals
    ]
}

/// Legend index for a declaration kind, or `None` for kinds we do not color.
fn token_index(kind: &str, is_param: bool) -> Option<u32> {
    Some(match kind {
        "ContractDefinition" => 0,
        "StructDefinition" => 1,
        "EnumDefinition" => 2,
        "EnumValue" => 3,
        "EventDefinition" => 4,
        "FunctionDefinition" | "ModifierDefinition" => 5,
        "ErrorDefinition" | "UserDefinedValueTypeDefinition" => 6,
        "VariableDeclaration" if is_param => 7,
        "VariableDeclaration" => 8,
        _ => return None,
    })
}

/// Record the declaration ids of a callable's parameters (and named returns).
fn collect_param_ids(map: &Map<String, Value>, out: &mut HashSet<i64>) {
    for key in ["parameters", "returnParameters"] {
        if let Some(list) = map.get(key).and_then(|p| p.get("parameters")).and_then(|a| a.as_array())
        {
            for p in list {
                if let Some(id) = p.get("id").and_then(|v| v.as_i64()) {
                    out.insert(id);
                }
            }
        }
    }
}

/// Delta-encode the file's spans into LSP semantic tokens, coloring each by the
/// kind of the declaration it resolves to. Spans are single-line identifiers;
/// overlapping/duplicate spans are dropped so tokens stay strictly increasing.
fn encode_tokens(
    spans: &[Span],
    decls: &HashMap<i64, Decl>,
    param_ids: &HashSet<i64>,
    mapper: &PositionMapper,
) -> Vec<SemanticToken> {
    let mut toks: Vec<(u32, u32, u32, u32)> = Vec::new();
    for s in spans {
        let Some(d) = decls.get(&s.decl) else {
            continue;
        };
        let Some(ty) = token_index(&d.kind, param_ids.contains(&s.decl)) else {
            continue;
        };
        let start = mapper.position(s.start);
        let end = mapper.position(s.end);
        if start.line != end.line {
            continue;
        }
        let len = end.character.saturating_sub(start.character);
        if len > 0 {
            toks.push((start.line, start.character, len, ty));
        }
    }
    toks.sort_unstable_by_key(|t| (t.0, t.1));

    let mut data = Vec::new();
    let (mut prev_line, mut prev_char) = (0u32, 0u32);
    let mut last_end: Option<(u32, u32)> = None;
    for (line, ch, len, ty) in toks {
        if let Some((ll, le)) = last_end {
            if line == ll && ch < le {
                continue; // overlaps the previous token
            }
        }
        let delta_line = line - prev_line;
        let delta_start = if delta_line == 0 { ch - prev_char } else { ch };
        data.push(SemanticToken {
            delta_line,
            delta_start,
            length: len,
            token_type: ty,
            token_modifiers_bitset: 0,
        });
        (prev_line, prev_char) = (line, ch);
        last_end = Some((line, ch + len));
    }
    data
}

/// Record the members of each container (contract/library/struct/enum) by name.
fn collect_containers(ast: &Value, containers: &mut HashMap<String, Vec<Member>>) {
    let Some(nodes) = ast.get("nodes").and_then(|n| n.as_array()) else {
        return;
    };
    for node in nodes {
        let Some(map) = node.as_object() else {
            continue;
        };
        let kind = gets(map, "nodeType").unwrap_or_default();
        if !matches!(
            kind,
            "ContractDefinition" | "StructDefinition" | "EnumDefinition"
        ) {
            continue;
        }
        let Some(name) = gets(map, "name").filter(|n| !n.is_empty()) else {
            continue;
        };
        // Contracts keep members in `nodes`; structs/enums in `members`.
        let children = node
            .get("nodes")
            .and_then(|n| n.as_array())
            .or_else(|| node.get("members").and_then(|n| n.as_array()));
        let members: Vec<Member> = children
            .map(|arr| arr.iter().filter_map(member_of).collect())
            .unwrap_or_default();
        containers.entry(name.to_string()).or_default().extend(members);
    }
}

fn member_of(node: &Value) -> Option<Member> {
    let map = node.as_object()?;
    let name = gets(map, "name").filter(|n| !n.is_empty())?;
    let kind = gets(map, "nodeType")?;
    let detail = signature(map).or_else(|| type_string(map)).unwrap_or_default();
    Some(Member { name: name.to_string(), kind: kind.to_string(), detail })
}

fn completion_kind(node_type: &str) -> CompletionItemKind {
    match node_type {
        "ContractDefinition" => CompletionItemKind::CLASS,
        "FunctionDefinition" => CompletionItemKind::FUNCTION,
        "ModifierDefinition" => CompletionItemKind::FUNCTION,
        "VariableDeclaration" => CompletionItemKind::FIELD,
        "EventDefinition" => CompletionItemKind::EVENT,
        "ErrorDefinition" => CompletionItemKind::CONSTRUCTOR,
        "StructDefinition" => CompletionItemKind::STRUCT,
        "EnumDefinition" => CompletionItemKind::ENUM,
        "EnumValue" => CompletionItemKind::ENUM_MEMBER,
        "UserDefinedValueTypeDefinition" => CompletionItemKind::STRUCT,
        _ => CompletionItemKind::VARIABLE,
    }
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

    #[test]
    fn param_hints_label_only_when_useful() {
        let names = vec!["to".to_string(), "amount".to_string(), String::new()];
        // A literal/expression argument gets the parameter name.
        assert_eq!(param_hint(&names, 0, None), Some("to:".into()));
        assert_eq!(param_hint(&names, 1, Some("value")), Some("amount:".into()));
        // An argument that already spells the parameter is not re-labeled.
        assert_eq!(param_hint(&names, 0, Some("to")), None);
        // Unnamed parameter and out-of-range index produce nothing.
        assert_eq!(param_hint(&names, 2, None), None);
        assert_eq!(param_hint(&names, 9, None), None);
    }
}

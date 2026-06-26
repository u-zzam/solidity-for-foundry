//! Completions that need no compile and no parse: Solidity keywords, the global
//! builtins (`require`, `keccak256`, …), the members of the magic globals
//! (`msg.`, `block.`, `tx.`, `abi.`), snippets, and import-path suggestions.
//! These are available the instant a file opens, before the index — or even the
//! tree-sitter parse — has anything project-specific to offer.

use std::path::Path;

use tower_lsp::lsp_types::{CompletionItem, CompletionItemKind, InsertTextFormat};

fn item(label: &str, kind: CompletionItemKind, detail: &str) -> CompletionItem {
    CompletionItem {
        label: label.to_string(),
        kind: Some(kind),
        detail: (!detail.is_empty()).then(|| detail.to_string()),
        ..Default::default()
    }
}

const KEYWORDS: &[&str] = &[
    "abstract", "address", "anonymous", "as", "assembly", "assert", "bool", "break", "bytes",
    "calldata", "constant", "constructor", "continue", "contract", "delete", "do", "else", "emit",
    "enum", "error", "event", "external", "fallback", "for", "from", "function", "if", "immutable",
    "import", "indexed", "interface", "internal", "is", "library", "mapping", "memory", "modifier",
    "new", "override", "payable", "pragma", "private", "public", "pure", "receive", "return",
    "returns", "revert", "storage", "string", "struct", "try", "type", "uint256", "unchecked",
    "using", "view", "virtual", "while",
];

/// Solidity keyword and elementary-type completions.
pub fn keywords() -> Vec<CompletionItem> {
    KEYWORDS.iter().map(|k| item(k, CompletionItemKind::KEYWORD, "")).collect()
}

/// Whether `word` is a keyword, reserved word or builtin literal, so it can't be
/// the new name in a rename.
pub fn is_reserved(word: &str) -> bool {
    KEYWORDS.contains(&word)
        || matches!(
            word,
            "true" | "false" | "wei" | "gwei" | "ether" | "seconds" | "minutes" | "hours" | "days"
                | "weeks" | "years" | "this" | "super" | "now" | "msg" | "block" | "tx" | "abi"
        )
}

/// Global builtin functions and magic objects (`require`, `keccak256`, `msg`, …).
pub fn global_builtins() -> Vec<CompletionItem> {
    const FUNCS: &[(&str, &str)] = &[
        ("require", "require(bool condition, string memory message)"),
        ("assert", "assert(bool condition)"),
        ("revert", "revert(string memory reason)"),
        ("keccak256", "keccak256(bytes memory) returns (bytes32)"),
        ("sha256", "sha256(bytes memory) returns (bytes32)"),
        ("ripemd160", "ripemd160(bytes memory) returns (bytes20)"),
        ("ecrecover", "ecrecover(bytes32 hash, uint8 v, bytes32 r, bytes32 s) returns (address)"),
        ("addmod", "addmod(uint x, uint y, uint k) returns (uint)"),
        ("mulmod", "mulmod(uint x, uint y, uint k) returns (uint)"),
        ("selfdestruct", "selfdestruct(address payable recipient)"),
        ("blockhash", "blockhash(uint blockNumber) returns (bytes32)"),
        ("blobhash", "blobhash(uint index) returns (bytes32)"),
        ("gasleft", "gasleft() returns (uint256)"),
        ("type", "type(C) — type information"),
    ];
    const OBJECTS: &[(&str, &str)] = &[
        ("msg", "current message"),
        ("block", "current block"),
        ("tx", "current transaction"),
        ("abi", "ABI encoding / decoding"),
        ("this", "the current contract"),
        ("super", "the base contract(s)"),
    ];
    FUNCS
        .iter()
        .map(|(n, d)| item(n, CompletionItemKind::FUNCTION, d))
        .chain(OBJECTS.iter().map(|(n, d)| item(n, CompletionItemKind::VARIABLE, d)))
        .collect()
}

/// Members of a magic global, for `msg.` / `block.` / `tx.` / `abi.` completion.
/// Empty for anything else (user containers are handled by the index/parser).
pub fn member_builtins(container: &str) -> Vec<CompletionItem> {
    let members: &[(&str, &str)] = match container {
        "msg" => &[
            ("sender", "address"),
            ("value", "uint256"),
            ("data", "bytes calldata"),
            ("sig", "bytes4"),
        ],
        "block" => &[
            ("timestamp", "uint256"),
            ("number", "uint256"),
            ("chainid", "uint256"),
            ("coinbase", "address payable"),
            ("basefee", "uint256"),
            ("blobbasefee", "uint256"),
            ("gaslimit", "uint256"),
            ("prevrandao", "uint256"),
            ("difficulty", "uint256"),
        ],
        "tx" => &[("origin", "address"), ("gasprice", "uint256")],
        "abi" => {
            return [
                ("encode", "abi.encode(...) returns (bytes memory)"),
                ("encodePacked", "abi.encodePacked(...) returns (bytes memory)"),
                ("encodeWithSelector", "abi.encodeWithSelector(bytes4, ...) returns (bytes memory)"),
                ("encodeWithSignature", "abi.encodeWithSignature(string, ...) returns (bytes memory)"),
                ("encodeCall", "abi.encodeCall(function, (...)) returns (bytes memory)"),
                ("decode", "abi.decode(bytes memory, (...)) returns (...)"),
            ]
            .iter()
            .map(|(n, d)| item(n, CompletionItemKind::FUNCTION, d))
            .collect();
        }
        _ => return Vec::new(),
    };
    members.iter().map(|(n, d)| item(n, CompletionItemKind::FIELD, d)).collect()
}

fn snippet(label: &str, body: &str, detail: &str) -> CompletionItem {
    CompletionItem {
        label: label.to_string(),
        kind: Some(CompletionItemKind::SNIPPET),
        detail: Some(detail.to_string()),
        insert_text: Some(body.to_string()),
        insert_text_format: Some(InsertTextFormat::SNIPPET),
        ..Default::default()
    }
}

/// A handful of structural snippets (LSP placeholder syntax).
pub fn snippets() -> Vec<CompletionItem> {
    vec![
        snippet("contract", "contract ${1:Name} {\n\t$0\n}", "contract skeleton"),
        snippet("interface", "interface ${1:IName} {\n\t$0\n}", "interface skeleton"),
        snippet("function", "function ${1:name}(${2}) ${3:public} {\n\t$0\n}", "function"),
        snippet("constructor", "constructor(${1}) {\n\t$0\n}", "constructor"),
        snippet("modifier", "modifier ${1:name}(${2}) {\n\t$0\n\t_;\n}", "modifier"),
        snippet("event", "event ${1:Name}(${2});", "event"),
        snippet("error", "error ${1:Name}(${2});", "error"),
        snippet("struct", "struct ${1:Name} {\n\t$0\n}", "struct"),
        snippet(
            "mapping",
            "mapping(${1:address} => ${2:uint256}) ${3:public} ${4:name};",
            "state mapping",
        ),
        snippet("require", "require(${1:condition}, \"${2:message}\");", "require check"),
        snippet(
            "for",
            "for (uint256 ${1:i} = 0; ${1:i} < ${2:n}; ${1:i}++) {\n\t$0\n}",
            "for loop",
        ),
        snippet("spdx", "// SPDX-License-Identifier: ${1:MIT}", "license header"),
        snippet("pragma", "pragma solidity ${1:^0.8.20};", "version pragma"),
    ]
}

/// If the cursor sits inside an unterminated import string, return the path
/// typed so far (`./`, `../lib/`, `@oz/`). Otherwise `None`.
pub fn import_path_context(text: &str, offset: usize) -> Option<String> {
    let offset = offset.min(text.len());
    let line_start = text[..offset].rfind('\n').map_or(0, |i| i + 1);
    let line = &text[line_start..offset];
    if !line.trim_start().starts_with("import") {
        return None;
    }
    // An odd number of quotes before the cursor means we're inside an open
    // string; the path typed so far follows the last (opening) quote.
    if line.bytes().filter(|&b| b == b'"' || b == b'\'').count() % 2 == 0 {
        return None;
    }
    let q = line.rfind(['"', '\''])?;
    Some(line[q + 1..].to_string())
}

/// Import-path completions for the partial path under the cursor: sibling `.sol`
/// files and subdirectories (resolved relative to the importing file), plus the
/// project's remapping prefixes when the path isn't relative.
pub fn import_completions(file_dir: &Path, prefix: &str, remappings: &[String]) -> Vec<CompletionItem> {
    let mut out = Vec::new();

    // Remapping prefixes (e.g. `@openzeppelin/`) for a bare, non-relative path.
    if !prefix.starts_with('.') {
        let frag = prefix;
        for r in remappings {
            if r.starts_with(frag) {
                out.push(item(r, CompletionItemKind::MODULE, "remapping"));
            }
        }
    }

    // Filesystem entries relative to the importing file.
    let (dir_part, _) = prefix.rsplit_once('/').unwrap_or(("", prefix));
    let base = file_dir.join(dir_part);
    if let Ok(entries) = std::fs::read_dir(&base) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            if is_dir {
                out.push(item(&format!("{name}/"), CompletionItemKind::FOLDER, ""));
            } else if name.ends_with(".sol") {
                out.push(item(&name, CompletionItemKind::FILE, ""));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn import_context_detects_unterminated_string() {
        assert_eq!(import_path_context("import \"./", 9), Some(".".to_string()));
        assert_eq!(import_path_context("import {A} from \"../lib/", 23), Some("../lib".to_string()));
        // A closed string, or a non-import line, is not import context.
        assert_eq!(import_path_context("import \"./A.sol\";", 16), None);
        assert_eq!(import_path_context("uint x = 1;", 11), None);
    }

    #[test]
    fn member_builtins_cover_the_magic_globals() {
        assert!(member_builtins("msg").iter().any(|i| i.label == "sender"));
        assert!(member_builtins("block").iter().any(|i| i.label == "timestamp"));
        assert!(member_builtins("tx").iter().any(|i| i.label == "origin"));
        assert!(member_builtins("abi").iter().any(|i| i.label == "encode"));
        assert!(member_builtins("Foo").is_empty());
    }
}

//! Map solc compiler errors to LSP diagnostics.
//!
//! The only fiddly part is the position model: solc reports byte offsets into
//! the (UTF-8) source, while LSP positions are UTF-16 code units per line.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use foundry_compilers::artifacts::{Error as SolcError, Severity};
use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, NumberOrString, Position, Range, Url};

/// Converts byte offsets within a single source file into LSP positions.
pub struct PositionMapper<'a> {
    text: &'a str,
    /// Byte offset of the first character of each line.
    line_starts: Vec<usize>,
}

impl<'a> PositionMapper<'a> {
    pub fn new(text: &'a str) -> Self {
        let mut line_starts = vec![0usize];
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(i + 1);
            }
        }
        Self { text, line_starts }
    }

    /// Byte offset -> 0-based UTF-16 (line, character), clamped to the document.
    pub fn position(&self, byte: usize) -> Position {
        let byte = byte.min(self.text.len());
        let line = self.line_starts.partition_point(|&s| s <= byte) - 1;
        let line_start = self.line_starts[line];
        // Walk back to a char boundary in case solc points mid-codepoint.
        let mut end = byte;
        while end > line_start && !self.text.is_char_boundary(end) {
            end -= 1;
        }
        let character: usize = self.text[line_start..end]
            .chars()
            .map(char::len_utf16)
            .sum();
        Position::new(line as u32, character as u32)
    }

    pub fn range(&self, start: i32, end: i32) -> Range {
        let s = start.max(0) as usize;
        let e = (end.max(0) as usize).max(s);
        Range::new(self.position(s), self.position(e))
    }
}

fn severity(s: Severity) -> DiagnosticSeverity {
    match s {
        Severity::Error => DiagnosticSeverity::ERROR,
        Severity::Warning => DiagnosticSeverity::WARNING,
        Severity::Info => DiagnosticSeverity::INFORMATION,
    }
}

/// Build an LSP diagnostic for `err`, positioned via `mapper` (the file the
/// error points into). The caller supplies the matching source text.
pub fn to_diagnostic(err: &SolcError, mapper: &PositionMapper) -> Diagnostic {
    let range = err
        .source_location
        .as_ref()
        .map(|l| mapper.range(l.start, l.end))
        .unwrap_or_default();
    Diagnostic {
        range,
        severity: Some(severity(err.severity)),
        code: err.error_code.map(|c| NumberOrString::Number(c as i32)),
        source: Some("solc".to_string()),
        message: err.message.trim().to_string(),
        ..Default::default()
    }
}

/// Group solc errors by file URI and map each into an LSP diagnostic. Source
/// text is read from disk (what solc compiled). Errors without a usable file
/// location are attached to `fallback` at the top of the document.
pub fn group(errors: &[SolcError], root: &Path, fallback: &Url) -> HashMap<Url, Vec<Diagnostic>> {
    // Read each distinct error file once.
    let mut texts: HashMap<PathBuf, Option<String>> = HashMap::new();
    for err in errors {
        if let Some(loc) = &err.source_location {
            let p = root.join(&loc.file);
            texts
                .entry(p.clone())
                .or_insert_with(|| std::fs::read_to_string(&p).ok());
        }
    }

    let mut out: HashMap<Url, Vec<Diagnostic>> = HashMap::new();
    for err in errors {
        let abs = err.source_location.as_ref().map(|loc| root.join(&loc.file));
        let uri = abs
            .as_ref()
            .and_then(|p| Url::from_file_path(p).ok())
            .unwrap_or_else(|| fallback.clone());
        let text = abs
            .as_ref()
            .and_then(|p| texts.get(p))
            .and_then(|o| o.as_deref())
            .unwrap_or("");
        let mapper = PositionMapper::new(text);
        out.entry(uri).or_default().push(to_diagnostic(err, &mapper));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_positions() {
        // 'é' = 2 bytes / 1 UTF-16 unit, '→' = 3 bytes / 1 UTF-16 unit.
        let text = "let aé→b = 1;\nsecond line";
        let m = PositionMapper::new(text);

        // Start of file.
        assert_eq!(m.position(0), Position::new(0, 0));

        // 'b' starts at byte 10: "let a"(5) + é(2) + →(3). The 7 preceding
        // chars (l e t ' ' a é →) are each 1 UTF-16 unit, so character == 7.
        assert_eq!(m.position(10), Position::new(0, 7));
        assert_eq!(m.position(11), Position::new(0, 8)); // the space after 'b'

        // Second line starts after "....;\n". Compute its byte start.
        let nl = text.find('\n').unwrap();
        assert_eq!(m.position(nl + 1), Position::new(1, 0));
        assert_eq!(m.position(nl + 4), Position::new(1, 3)); // "sec"
    }

    #[test]
    fn out_of_bounds_clamps() {
        let m = PositionMapper::new("abc");
        assert_eq!(m.position(999), Position::new(0, 3));
        // negative solc offsets clamp to start of file.
        assert_eq!(m.range(-1, -1), Range::new(Position::new(0, 0), Position::new(0, 0)));
    }
}

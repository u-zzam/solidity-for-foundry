//! Map solc compiler errors to LSP diagnostics.
//!
//! The only fiddly part is the position model: solc reports byte offsets into
//! the (UTF-8) source, while LSP positions are UTF-16 code units per line.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use foundry_compilers::artifacts::{Error as SolcError, Severity};
use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, NumberOrString, Position, Range, Url};

use crate::project::LintFinding;

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

    /// LSP position -> byte offset, clamped to the document. Inverse of `position`.
    pub fn offset(&self, pos: Position) -> usize {
        let line = pos.line as usize;
        let Some(&line_start) = self.line_starts.get(line) else {
            return self.text.len();
        };
        let line_end = self
            .line_starts
            .get(line + 1)
            .copied()
            .unwrap_or(self.text.len());
        let mut utf16 = 0u32;
        let mut byte = line_start;
        for c in self.text[line_start..line_end].chars() {
            if utf16 >= pos.character {
                break;
            }
            utf16 += c.len_utf16() as u32;
            byte += c.len_utf8();
        }
        byte.min(self.text.len())
    }

    pub fn range(&self, start: i32, end: i32) -> Range {
        let s = start.max(0) as usize;
        let e = (end.max(0) as usize).max(s);
        Range::new(self.position(s), self.position(e))
    }
}

/// The range covering the whole document, for full-text replacement edits.
pub fn full_range(text: &str) -> Range {
    let end = PositionMapper::new(text).position(text.len());
    Range::new(Position::new(0, 0), end)
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

/// Diagnostics for the single edited file, mapped against the in-memory
/// `buffer` rather than disk. Live checks compile the unsaved buffer, so solc's
/// byte offsets only line up with the buffer — using the on-disk text would
/// misplace every squiggle once the buffer and file diverge. Only errors
/// located in `target` are returned (that's all a live check publishes).
pub fn for_buffer(errors: &[SolcError], root: &Path, target: &Url, buffer: &str) -> Vec<Diagnostic> {
    let mapper = PositionMapper::new(buffer);
    errors
        .iter()
        .filter(|err| match &err.source_location {
            // Errors with no location (e.g. a fatal compiler run failure) attach
            // to the edited file at the top, rather than vanishing.
            None => true,
            Some(loc) => {
                Url::from_file_path(root.join(&loc.file)).ok().is_some_and(|uri| uri == *target)
            }
        })
        .map(|err| to_diagnostic(err, &mapper))
        .collect()
}

/// Group `forge lint` findings by file URI into LSP diagnostics, tagged with
/// source `forge lint`. The caller merges these into the same per-file publish
/// as the solc diagnostics so both kinds of squiggle coexist.
pub fn group_lints(findings: &[LintFinding]) -> HashMap<Url, Vec<Diagnostic>> {
    let mut texts: HashMap<PathBuf, Option<String>> = HashMap::new();
    for f in findings {
        texts
            .entry(f.file.clone())
            .or_insert_with(|| std::fs::read_to_string(&f.file).ok());
    }

    let mut out: HashMap<Url, Vec<Diagnostic>> = HashMap::new();
    for f in findings {
        let Ok(uri) = Url::from_file_path(&f.file) else {
            continue;
        };
        let text = texts.get(&f.file).and_then(|o| o.as_deref()).unwrap_or("");
        let mapper = PositionMapper::new(text);
        let severity = match f.level.as_str() {
            "error" => DiagnosticSeverity::ERROR,
            "warning" => DiagnosticSeverity::WARNING,
            "help" => DiagnosticSeverity::HINT,
            _ => DiagnosticSeverity::INFORMATION,
        };
        out.entry(uri).or_default().push(Diagnostic {
            range: Range::new(
                mapper.position(f.byte_start),
                mapper.position(f.byte_end.max(f.byte_start)),
            ),
            severity: Some(severity),
            code: f.code.clone().map(NumberOrString::String),
            source: Some("forge lint".to_string()),
            message: f.message.clone(),
            ..Default::default()
        });
    }
    out
}

/// A quick-fix derived from a `forge lint` suggestion.
#[derive(Clone)]
pub struct LintFix {
    pub range: Range,
    pub title: String,
    pub new_text: String,
}

/// Build quick-fixes for lint findings that carry a suggested replacement,
/// grouped by file URI.
pub fn lint_fixes(findings: &[LintFinding]) -> HashMap<Url, Vec<LintFix>> {
    let mut texts: HashMap<PathBuf, Option<String>> = HashMap::new();
    let mut out: HashMap<Url, Vec<LintFix>> = HashMap::new();
    for f in findings {
        let Some(s) = &f.suggestion else {
            continue;
        };
        let Ok(uri) = Url::from_file_path(&f.file) else {
            continue;
        };
        let text = texts
            .entry(f.file.clone())
            .or_insert_with(|| std::fs::read_to_string(&f.file).ok())
            .clone();
        let mapper = PositionMapper::new(text.as_deref().unwrap_or(""));
        let title = match &f.code {
            Some(c) => format!("{c}: replace with `{}`", s.text),
            None => format!("Replace with `{}`", s.text),
        };
        out.entry(uri).or_default().push(LintFix {
            range: Range::new(mapper.position(s.byte_start), mapper.position(s.byte_end)),
            title,
            new_text: s.text.clone(),
        });
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
    fn offset_roundtrips_position() {
        let text = "let aé→b = 1;\nsecond line";
        let m = PositionMapper::new(text);
        for byte in [0usize, 4, 5, 7, 10, 11, text.find('\n').unwrap() + 1, text.len()] {
            assert!(text.is_char_boundary(byte));
            let pos = m.position(byte);
            assert_eq!(m.offset(pos), byte, "byte {byte} via {pos:?}");
        }
    }

    #[test]
    fn out_of_bounds_clamps() {
        let m = PositionMapper::new("abc");
        assert_eq!(m.position(999), Position::new(0, 3));
        // negative solc offsets clamp to start of file.
        assert_eq!(m.range(-1, -1), Range::new(Position::new(0, 0), Position::new(0, 0)));
    }
}

//! Source files, spans and diagnostics.

use std::fmt;
use std::sync::Arc;

/// Identifies a source file within a [`SourceMap`].
pub type FileId = u32;

/// A byte range within a source file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct Span {
    pub file: FileId,
    pub lo: u32,
    pub hi: u32,
}

impl Span {
    pub fn new(file: FileId, lo: u32, hi: u32) -> Span {
        Span { file, lo, hi }
    }
    pub fn to(self, other: Span) -> Span {
        Span { file: self.file, lo: self.lo.min(other.lo), hi: self.hi.max(other.hi) }
    }
    pub fn empty_at(self, pos: u32) -> Span {
        Span { file: self.file, lo: pos, hi: pos }
    }
}

#[derive(Debug)]
pub struct SourceFile {
    pub name: String,
    pub text: Arc<str>,
    line_starts: Vec<u32>,
    /// The number of characters before each multiple of [`CHAR_MARK`] bytes,
    /// so that columns are found in constant time on long lines.
    char_marks: Vec<u32>,
}

/// Bytes between the entries of `SourceFile::char_marks`.
const CHAR_MARK: usize = 256;

/// Whether `b` starts a character (is not a UTF-8 continuation byte).
fn starts_char(b: u8) -> bool {
    b & 0xC0 != 0x80
}

impl SourceFile {
    pub fn new(name: impl Into<String>, text: impl Into<Arc<str>>) -> SourceFile {
        let text: Arc<str> = text.into();
        let mut line_starts = vec![0];
        let mut char_marks = Vec::with_capacity(text.len() / CHAR_MARK + 1);
        let mut chars = 0u32;
        for (i, b) in text.bytes().enumerate() {
            if i.is_multiple_of(CHAR_MARK) {
                char_marks.push(chars);
            }
            chars += starts_char(b) as u32;
            if b == b'\n' {
                line_starts.push(i as u32 + 1);
            }
        }
        if text.len().is_multiple_of(CHAR_MARK) {
            char_marks.push(chars);
        }
        SourceFile { name: name.into(), text, line_starts, char_marks }
    }

    /// The number of characters before byte offset `pos`.
    fn chars_before(&self, pos: usize) -> usize {
        let pos = pos.min(self.text.len());
        let mark = pos / CHAR_MARK;
        let before = self.char_marks[mark];
        before as usize + self.text.as_bytes()[mark * CHAR_MARK..pos].iter().filter(|&&b| starts_char(b)).count()
    }

    /// 1-based line and column (in characters) of a byte offset.
    pub fn line_col(&self, pos: u32) -> (usize, usize) {
        let line = match self.line_starts.binary_search(&pos) {
            Ok(i) => i,
            Err(i) => i - 1,
        };
        let start = self.line_starts[line] as usize;
        let col = self.chars_before(pos as usize).saturating_sub(self.chars_before(start));
        (line + 1, col + 1)
    }

    pub fn line_text(&self, line: usize) -> &str {
        let start = self.line_starts[line - 1] as usize;
        let end = self.line_starts.get(line).map(|&e| e as usize).unwrap_or(self.text.len());
        self.text[start..end].trim_end_matches(['\n', '\r'])
    }
}

/// All source files known to a compilation.
#[derive(Debug, Default)]
pub struct SourceMap {
    pub files: Vec<SourceFile>,
}

impl SourceMap {
    pub fn add(&mut self, name: impl Into<String>, text: impl Into<Arc<str>>) -> FileId {
        self.files.push(SourceFile::new(name, text));
        (self.files.len() - 1) as FileId
    }
    pub fn file(&self, id: FileId) -> &SourceFile {
        &self.files[id as usize]
    }
    pub fn text(&self, id: FileId) -> &str {
        &self.files[id as usize].text
    }
    pub fn resolve(&self, d: &Diag) -> Diagnostic {
        let f = self.file(d.span.file);
        let (line, column) = f.line_col(d.span.lo);
        let (end_line, end_col) = f.line_col(d.span.hi);
        let width = if end_line == line { end_col.saturating_sub(column).max(1) } else { 1 };
        Diagnostic {
            severity: d.severity,
            code: d.code.to_string(),
            message: d.message.clone(),
            file: f.name.clone(),
            line,
            column,
            width,
            notes: d.notes.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

/// An unresolved diagnostic (span-based), produced by the front end.
#[derive(Clone, Debug)]
pub struct Diag {
    pub severity: Severity,
    pub code: &'static str,
    pub message: String,
    pub span: Span,
    pub notes: Vec<String>,
}

impl Diag {
    pub fn error(span: Span, message: impl Into<String>) -> Diag {
        Diag { severity: Severity::Error, code: "E", message: message.into(), span, notes: vec![] }
    }
    pub fn warning(span: Span, code: &'static str, message: impl Into<String>) -> Diag {
        Diag { severity: Severity::Warning, code, message: message.into(), span, notes: vec![] }
    }
    pub fn with_code(mut self, code: &'static str) -> Diag {
        self.code = code;
        self
    }
    pub fn note(mut self, n: impl Into<String>) -> Diag {
        self.notes.push(n.into());
        self
    }
}

/// A diagnostic resolved to file, line and column.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub severity: Severity,
    pub code: String,
    pub message: String,
    pub file: String,
    pub line: usize,
    pub column: usize,
    pub width: usize,
    pub notes: Vec<String>,
}

impl Diagnostic {
    /// Renders the diagnostic, with a source excerpt if `source` is the text of `self.file`.
    pub fn render(&self, source: Option<&str>) -> String {
        let kind = match self.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
        };
        let mut out = if self.code.is_empty() || self.code == "E" {
            format!("{kind}: {}\n", self.message)
        } else {
            format!("{kind}[{}]: {}\n", self.code, self.message)
        };
        out.push_str(&format!("  --> {}:{}:{}\n", self.file, self.line, self.column));
        if let Some(src) = source
            && let Some(text) = src.lines().nth(self.line - 1)
        {
            let num = self.line.to_string();
            let pad = " ".repeat(num.len());
            out.push_str(&format!("{pad} |\n{num} | {text}\n"));
            let prefix: String = text.chars().take(self.column - 1).map(|c| if c == '\t' { '\t' } else { ' ' }).collect();
            out.push_str(&format!("{pad} | {prefix}{}\n", "^".repeat(self.width)));
        }
        for n in &self.notes {
            out.push_str(&format!("  = note: {n}\n"));
        }
        out
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
        };
        write!(f, "{}:{}:{}: {kind}: {}", self.file, self.line, self.column, self.message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns_count_characters_on_long_lines() {
        let text = format!("ab\n{}x\né{}y\n", "é".repeat(300), "a".repeat(1000));
        let f = SourceFile::new("t", text.as_str());
        let col = |needle: char| f.line_col(text.find(needle).unwrap() as u32);
        assert_eq!(col('b'), (1, 2));
        assert_eq!(col('x'), (2, 301));
        assert_eq!(col('y'), (3, 1002));
        assert_eq!(f.line_col(text.len() as u32), (4, 1));
        for len in [0, 255, 256, 257, 512] {
            let f = SourceFile::new("t", "a".repeat(len));
            assert_eq!(f.line_col(len as u32), (1, len + 1));
        }
        for (pos, _) in text.char_indices() {
            let (line, col) = f.line_col(pos as u32);
            let start = text[..pos].rfind('\n').map_or(0, |i| i + 1);
            assert_eq!((line, col), (text[..pos].matches('\n').count() + 1, text[start..pos].chars().count() + 1));
        }
    }
}

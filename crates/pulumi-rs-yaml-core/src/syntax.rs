use crate::source::FileId;
use std::fmt;

/// A byte-offset span within a source file.
///
/// Spans are cheap to copy and compare. They reference positions
/// within the source text owned by `SourceArena`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Span {
    pub file: FileId,
    pub start: u32,
    pub end: u32,
}

impl Span {
    /// Creates a new span.
    pub fn new(file: FileId, start: u32, end: u32) -> Self {
        debug_assert!(start <= end);
        Self { file, start, end }
    }

    /// Returns the length of the span in bytes.
    pub fn len(&self) -> u32 {
        self.end - self.start
    }

    /// Returns true if the span is empty.
    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }

    /// Merges two spans into one covering both, assuming they are in the same file.
    pub fn merge(self, other: Span) -> Span {
        debug_assert_eq!(self.file, other.file);
        Span {
            file: self.file,
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }
}

/// Position information computed from a span and its source text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineCol {
    /// 1-based line number.
    pub line: u32,
    /// 1-based column number (byte offset within line).
    pub col: u32,
}

/// A lookup table for converting byte offsets to line/column positions.
#[derive(Clone)]
pub struct LineIndex {
    /// Byte offsets of line starts (including offset 0 for line 1).
    line_starts: Vec<u32>,
}

impl LineIndex {
    /// Builds a line index from source text.
    pub fn new(text: &str) -> Self {
        let mut line_starts = vec![0u32];
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(i as u32 + 1);
            }
        }
        Self { line_starts }
    }

    /// Converts a byte offset to a 1-based line and column.
    pub fn line_col(&self, offset: u32) -> LineCol {
        let line = match self.line_starts.binary_search(&offset) {
            Ok(line) => line,
            Err(line) => line - 1,
        };
        let col = offset - self.line_starts[line];
        LineCol {
            line: line as u32 + 1,
            col: col + 1,
        }
    }

    /// Returns the number of lines.
    pub fn line_count(&self) -> usize {
        self.line_starts.len()
    }
}

/// Metadata that can be attached to any AST node.
///
/// Contains an optional span for source location tracking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ExprMeta {
    pub span: Option<Span>,
}

impl ExprMeta {
    /// Creates metadata with a span.
    pub fn with_span(span: Span) -> Self {
        Self { span: Some(span) }
    }

    /// Creates metadata with no span (synthetic nodes).
    pub fn no_span() -> Self {
        Self { span: None }
    }
}

impl fmt::Display for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}..{}", self.start, self.end)
    }
}

impl fmt::Display for LineCol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.line, self.col)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::FileId;

    #[test]
    fn test_span_basics() {
        let span = Span::new(FileId(0), 5, 10);
        assert_eq!(span.len(), 5);
        assert!(!span.is_empty());
    }

    #[test]
    fn test_span_empty() {
        let span = Span::new(FileId(0), 5, 5);
        assert_eq!(span.len(), 0);
        assert!(span.is_empty());
    }

    #[test]
    fn test_span_merge() {
        let a = Span::new(FileId(0), 5, 10);
        let b = Span::new(FileId(0), 8, 15);
        let merged = a.merge(b);
        assert_eq!(merged.start, 5);
        assert_eq!(merged.end, 15);
    }

    #[test]
    fn test_line_index_simple() {
        let text = "hello\nworld\n";
        let idx = LineIndex::new(text);
        assert_eq!(idx.line_col(0), LineCol { line: 1, col: 1 });
        assert_eq!(idx.line_col(5), LineCol { line: 1, col: 6 }); // the \n
        assert_eq!(idx.line_col(6), LineCol { line: 2, col: 1 }); // 'w'
        assert_eq!(idx.line_col(11), LineCol { line: 2, col: 6 }); // the \n
    }

    #[test]
    fn test_line_index_single_line() {
        let text = "no newline";
        let idx = LineIndex::new(text);
        assert_eq!(idx.line_col(0), LineCol { line: 1, col: 1 });
        assert_eq!(idx.line_col(9), LineCol { line: 1, col: 10 });
    }

    #[test]
    fn test_line_index_empty() {
        let text = "";
        let idx = LineIndex::new(text);
        assert_eq!(idx.line_count(), 1);
        assert_eq!(idx.line_col(0), LineCol { line: 1, col: 1 });
    }

    #[test]
    fn test_line_index_multiple_newlines() {
        let text = "a\n\nb\n";
        let idx = LineIndex::new(text);
        assert_eq!(idx.line_col(0), LineCol { line: 1, col: 1 }); // 'a'
        assert_eq!(idx.line_col(2), LineCol { line: 2, col: 1 }); // empty line
        assert_eq!(idx.line_col(3), LineCol { line: 3, col: 1 }); // 'b'
    }

    #[test]
    fn test_expr_meta() {
        let meta = ExprMeta::with_span(Span::new(FileId(0), 0, 5));
        assert!(meta.span.is_some());

        let meta = ExprMeta::no_span();
        assert!(meta.span.is_none());

        let meta = ExprMeta::default();
        assert!(meta.span.is_none());
    }
}

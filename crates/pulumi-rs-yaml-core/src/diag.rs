use crate::source::{FileId, SourceArena};
use crate::syntax::{LineIndex, Span};
use std::fmt;

/// Severity level for diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Severity {
    Warning = 0,
    Error = 1,
}

/// A diagnostic message associated with a source location.
#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub severity: Severity,
    pub span: Option<Span>,
    pub summary: String,
    pub detail: String,
    /// Whether the diagnostic has been shown to the user.
    pub shown: bool,
}

impl Diagnostic {
    /// Creates a new error diagnostic.
    pub fn error(
        span: Option<Span>,
        summary: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            severity: Severity::Error,
            span,
            summary: summary.into(),
            detail: detail.into(),
            shown: false,
        }
    }

    /// Creates a new warning diagnostic.
    pub fn warning(
        span: Option<Span>,
        summary: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            severity: Severity::Warning,
            span,
            summary: summary.into(),
            detail: detail.into(),
            shown: false,
        }
    }

    /// Returns true if this is an error-level diagnostic.
    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let prefix = match self.severity {
            Severity::Warning => "warning",
            Severity::Error => "error",
        };
        if self.detail.is_empty() {
            write!(f, "{}: {}", prefix, self.summary)
        } else {
            write!(f, "{}: {}; {}", prefix, self.summary, self.detail)
        }
    }
}

/// A collection of diagnostics.
#[derive(Debug, Clone, Default)]
pub struct Diagnostics {
    diags: Vec<Diagnostic>,
}

impl Diagnostics {
    /// Creates an empty diagnostics collection.
    pub fn new() -> Self {
        Self { diags: Vec::new() }
    }

    /// Adds a diagnostic.
    pub fn add(&mut self, diag: Diagnostic) {
        self.diags.push(diag);
    }

    /// Adds an error diagnostic.
    pub fn error(
        &mut self,
        span: Option<Span>,
        summary: impl Into<String>,
        detail: impl Into<String>,
    ) {
        self.add(Diagnostic::error(span, summary, detail));
    }

    /// Adds a warning diagnostic.
    pub fn warning(
        &mut self,
        span: Option<Span>,
        summary: impl Into<String>,
        detail: impl Into<String>,
    ) {
        self.add(Diagnostic::warning(span, summary, detail));
    }

    /// Extends with another collection of diagnostics.
    pub fn extend(&mut self, other: Diagnostics) {
        self.diags.extend(other.diags);
    }

    /// Extends from an iterator of diagnostics.
    pub fn extend_iter(&mut self, iter: impl IntoIterator<Item = Diagnostic>) {
        self.diags.extend(iter);
    }

    /// Returns true if any error-level diagnostics are present.
    pub fn has_errors(&self) -> bool {
        self.diags.iter().any(|d| d.is_error())
    }

    /// Returns true if any warning-level diagnostics are present.
    pub fn has_warnings(&self) -> bool {
        self.diags.iter().any(|d| d.severity == Severity::Warning)
    }

    /// Returns true if the collection is empty.
    pub fn is_empty(&self) -> bool {
        self.diags.is_empty()
    }

    /// Returns the number of diagnostics.
    pub fn len(&self) -> usize {
        self.diags.len()
    }

    /// Returns an iterator over the diagnostics.
    pub fn iter(&self) -> impl Iterator<Item = &Diagnostic> {
        self.diags.iter()
    }

    /// Returns a mutable iterator over the diagnostics.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut Diagnostic> {
        self.diags.iter_mut()
    }

    /// Returns diagnostics that have not been shown yet.
    pub fn unshown(&self) -> impl Iterator<Item = &Diagnostic> {
        self.diags.iter().filter(|d| !d.shown)
    }

    /// Consumes self and returns the inner Vec.
    pub fn into_vec(self) -> Vec<Diagnostic> {
        self.diags
    }
}

impl IntoIterator for Diagnostics {
    type Item = Diagnostic;
    type IntoIter = std::vec::IntoIter<Diagnostic>;

    fn into_iter(self) -> Self::IntoIter {
        self.diags.into_iter()
    }
}

impl<'a> IntoIterator for &'a Diagnostics {
    type Item = &'a Diagnostic;
    type IntoIter = std::slice::Iter<'a, Diagnostic>;

    fn into_iter(self) -> Self::IntoIter {
        self.diags.iter()
    }
}

impl fmt::Display for Diagnostics {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut sorted: Vec<_> = self.diags.iter().collect();
        sorted.sort_by_key(|d| d.severity);
        for diag in sorted {
            writeln!(f, "{}", diag)?;
        }
        Ok(())
    }
}

/// A table for looking up file names and computing line/column positions.
pub struct FileTable<'a> {
    arena: &'a SourceArena,
    indices: Vec<Option<LineIndex>>,
}

impl<'a> FileTable<'a> {
    /// Creates a new file table from a source arena.
    pub fn new(arena: &'a SourceArena) -> Self {
        let count = arena.file_count();
        let mut indices = Vec::with_capacity(count);
        indices.resize_with(count, || None);
        Self { arena, indices }
    }

    /// Returns the file name for a file ID.
    pub fn file_name(&self, file: FileId) -> &str {
        self.arena.name(file)
    }

    /// Computes or retrieves the line index for a file.
    fn line_index(&mut self, file: FileId) -> &LineIndex {
        let idx = file.0 as usize;
        if self.indices[idx].is_none() {
            self.indices[idx] = Some(LineIndex::new(self.arena.text(file)));
        }
        self.indices[idx].as_ref().unwrap()
    }

    /// Formats a span as "filename:line:col".
    pub fn format_span(&mut self, span: Span) -> String {
        let lc = self.line_index(span.file).line_col(span.start);
        format!("{}:{}:{}", self.arena.name(span.file), lc.line, lc.col)
    }

    /// Formats a diagnostic with source location.
    pub fn format_diagnostic(&mut self, diag: &Diagnostic) -> String {
        let prefix = match diag.severity {
            Severity::Warning => "warning",
            Severity::Error => "error",
        };
        let location = match diag.span {
            Some(span) => format!("{}: ", self.format_span(span)),
            None => String::new(),
        };
        if diag.detail.is_empty() {
            format!("{}{}: {}", location, prefix, diag.summary)
        } else {
            format!("{}{}: {}; {}", location, prefix, diag.summary, diag.detail)
        }
    }
}

/// Computes the edit distance between two strings (Levenshtein distance).
pub fn edit_distance(a: &str, b: &str) -> usize {
    let a_bytes = a.as_bytes();
    let b_bytes = b.as_bytes();
    let m = a_bytes.len();
    let n = b_bytes.len();

    let mut prev = (0..=n).collect::<Vec<_>>();
    let mut curr = vec![0; n + 1];

    for i in 1..=m {
        curr[0] = i;
        for j in 1..=n {
            let cost = if a_bytes[i - 1] == b_bytes[j - 1] {
                0
            } else {
                1
            };
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }

    prev[n]
}

/// Sorts a list of strings by edit distance from a target, ascending.
pub fn sort_by_edit_distance(candidates: &[String], target: &str) -> Vec<String> {
    let mut with_distance: Vec<_> = candidates
        .iter()
        .map(|c| (edit_distance(c, target), c.clone()))
        .collect();
    with_distance.sort_by_key(|(d, _)| *d);
    with_distance.into_iter().map(|(_, c)| c).collect()
}

/// Warn about miscapitalization. Returns `None` if `expected == found`.
pub fn unexpected_casing(span: Option<Span>, expected: &str, found: &str) -> Option<Diagnostic> {
    if expected == found {
        return None;
    }
    let summary = format!(
        "'{}' looks like a miscapitalization of '{}'",
        found, expected
    );
    let detail = "A future version of Pulumi YAML will enforce camelCase fields. See https://github.com/pulumi/pulumi-yaml/issues/355 for details.";
    Some(Diagnostic::warning(span, summary, detail))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::SourceArena;

    #[test]
    fn test_diagnostic_error() {
        let d = Diagnostic::error(None, "something broke", "details here");
        assert!(d.is_error());
        assert_eq!(d.severity, Severity::Error);
        assert_eq!(d.to_string(), "error: something broke; details here");
    }

    #[test]
    fn test_diagnostic_warning() {
        let d = Diagnostic::warning(None, "be careful", "");
        assert!(!d.is_error());
        assert_eq!(d.to_string(), "warning: be careful");
    }

    #[test]
    fn test_diagnostics_has_errors() {
        let mut diags = Diagnostics::new();
        assert!(!diags.has_errors());

        diags.warning(None, "warn", "");
        assert!(!diags.has_errors());

        diags.error(None, "err", "");
        assert!(diags.has_errors());
    }

    #[test]
    fn test_diagnostics_extend() {
        let mut a = Diagnostics::new();
        a.error(None, "a1", "");
        let mut b = Diagnostics::new();
        b.warning(None, "b1", "");
        a.extend(b);
        assert_eq!(a.len(), 2);
    }

    #[test]
    fn test_edit_distance() {
        assert_eq!(edit_distance("kitten", "sitting"), 3);
        assert_eq!(edit_distance("", "abc"), 3);
        assert_eq!(edit_distance("abc", ""), 3);
        assert_eq!(edit_distance("abc", "abc"), 0);
        assert_eq!(edit_distance("a", "b"), 1);
    }

    #[test]
    fn test_sort_by_edit_distance() {
        let candidates = vec![
            "protect".to_string(),
            "provider".to_string(),
            "parent".to_string(),
        ];
        let sorted = sort_by_edit_distance(&candidates, "provder");
        assert_eq!(sorted[0], "provider");
    }

    #[test]
    fn test_file_table_format_span() {
        let mut arena = SourceArena::new();
        let id = arena.add_file("test.yaml".to_string(), "hello\nworld\n".to_string());
        let mut table = FileTable::new(&arena);
        let span = Span::new(id, 6, 11);
        assert_eq!(table.format_span(span), "test.yaml:2:1");
    }

    #[test]
    fn test_file_table_format_diagnostic() {
        let mut arena = SourceArena::new();
        let id = arena.add_file("main.yaml".to_string(), "line1\nline2\n".to_string());
        let mut table = FileTable::new(&arena);
        let diag = Diagnostic::error(Some(Span::new(id, 6, 11)), "bad thing", "");
        assert_eq!(
            table.format_diagnostic(&diag),
            "main.yaml:2:1: error: bad thing"
        );
    }

    #[test]
    fn test_unexpected_casing_match() {
        assert!(unexpected_casing(None, "dependsOn", "dependsOn").is_none());
    }

    #[test]
    fn test_unexpected_casing_mismatch() {
        let diag = unexpected_casing(None, "dependsOn", "DependsOn").unwrap();
        assert!(!diag.is_error());
        assert!(diag.summary.contains("miscapitalization"));
    }

    #[test]
    fn test_diagnostics_display() {
        let mut diags = Diagnostics::new();
        diags.error(None, "err1", "detail1");
        diags.warning(None, "warn1", "");
        let output = diags.to_string();
        assert!(output.contains("warning: warn1"));
        assert!(output.contains("error: err1; detail1"));
    }

    #[test]
    fn test_diagnostics_unshown() {
        let mut diags = Diagnostics::new();
        diags.add(Diagnostic {
            severity: Severity::Error,
            span: None,
            summary: "shown".into(),
            detail: String::new(),
            shown: true,
        });
        diags.add(Diagnostic::error(None, "unshown", ""));
        let unshown: Vec<_> = diags.unshown().collect();
        assert_eq!(unshown.len(), 1);
        assert_eq!(unshown[0].summary, "unshown");
    }
}

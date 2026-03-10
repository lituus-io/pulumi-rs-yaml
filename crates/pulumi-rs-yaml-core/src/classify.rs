//! Error classification for structured diagnostic reporting.
//!
//! Classifies diagnostics into machine-readable categories by inspecting
//! the existing structured data (not regex on message text).

/// Machine-readable error categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCategory {
    MissingField,
    TypeMismatch,
    SyntaxError,
    JinjaError,
    MissingConfig,
    InvalidReference,
    CircularDep,
    InvalidResource,
    DuplicateName,
    ReservedName,
    UnknownProperty,
    MissingRequired,
    Unknown,
}

impl ErrorCategory {
    pub fn as_str(&self) -> &'static str {
        match self {
            ErrorCategory::MissingField => "missing_field",
            ErrorCategory::TypeMismatch => "type_mismatch",
            ErrorCategory::SyntaxError => "syntax_error",
            ErrorCategory::JinjaError => "jinja_error",
            ErrorCategory::MissingConfig => "missing_config",
            ErrorCategory::InvalidReference => "invalid_reference",
            ErrorCategory::CircularDep => "circular_dep",
            ErrorCategory::InvalidResource => "invalid_resource",
            ErrorCategory::DuplicateName => "duplicate_name",
            ErrorCategory::ReservedName => "reserved_name",
            ErrorCategory::UnknownProperty => "unknown_property",
            ErrorCategory::MissingRequired => "missing_required",
            ErrorCategory::Unknown => "unknown",
        }
    }
}

/// A classified diagnostic with structured data extracted from the original.
#[derive(Debug, Clone)]
pub struct ClassifiedDiagnostic {
    pub category: ErrorCategory,
    pub message: String,
    pub detail: String,
    pub suggestions: Vec<String>,
    pub bad_ref: Option<String>,
    pub best_match: Option<String>,
    pub cycle_path: Option<Vec<String>>,
}

/// Classify a diagnostic message into a category with structured data.
pub fn classify_diagnostic(summary: &str, detail: &str) -> ClassifiedDiagnostic {
    let msg_lower = summary.to_lowercase();

    let (category, suggestions, bad_ref, best_match, cycle_path) =
        if msg_lower.contains("circular dependency") {
            // Extract cycle path from "circular dependency: a -> b -> a"
            let cycle = extract_cycle_path(summary);
            (ErrorCategory::CircularDep, Vec::new(), None, None, Some(cycle))
        } else if msg_lower.contains("is not defined") {
            // "resource or variable 'X' ... is not defined; did you mean 'Y'?"
            let bad = extract_quoted_name(summary, "referenced");
            let suggestion = extract_did_you_mean(summary);
            let suggestions = suggestion.clone().into_iter().collect();
            (
                ErrorCategory::InvalidReference,
                suggestions,
                bad,
                suggestion,
                None,
            )
        } else if msg_lower.contains("duplicate node name")
            || msg_lower.contains("defined in both")
        {
            let name = extract_quoted_name(summary, "'");
            (ErrorCategory::DuplicateName, Vec::new(), name, None, None)
        } else if msg_lower.contains("reserved name") {
            (ErrorCategory::ReservedName, Vec::new(), None, None, None)
        } else if msg_lower.contains("runtime")
            && (msg_lower.contains("missing") || msg_lower.contains("required"))
        {
            (ErrorCategory::MissingField, Vec::new(), None, None, None)
        } else if msg_lower.contains("type")
            && (msg_lower.contains("mismatch") || msg_lower.contains("expected"))
        {
            (ErrorCategory::TypeMismatch, Vec::new(), None, None, None)
        } else if msg_lower.contains("jinja") || msg_lower.contains("template") {
            (ErrorCategory::JinjaError, Vec::new(), None, None, None)
        } else if msg_lower.contains("syntax")
            || msg_lower.contains("indent")
            || msg_lower.contains("mapping")
        {
            (ErrorCategory::SyntaxError, Vec::new(), None, None, None)
        } else if msg_lower.contains("config")
            && (msg_lower.contains("missing") || msg_lower.contains("undeclared"))
        {
            (ErrorCategory::MissingConfig, Vec::new(), None, None, None)
        } else if msg_lower.contains("unknown property") || msg_lower.contains("did you mean") {
            let suggestion = extract_did_you_mean(summary);
            let suggestions = suggestion.clone().into_iter().collect();
            (
                ErrorCategory::UnknownProperty,
                suggestions,
                None,
                suggestion,
                None,
            )
        } else if msg_lower.contains("required") && msg_lower.contains("missing") {
            (ErrorCategory::MissingRequired, Vec::new(), None, None, None)
        } else {
            (ErrorCategory::Unknown, Vec::new(), None, None, None)
        };

    ClassifiedDiagnostic {
        category,
        message: summary.to_string(),
        detail: detail.to_string(),
        suggestions,
        bad_ref,
        best_match,
        cycle_path,
    }
}

/// Classify all diagnostics from a validation result.
pub fn classify_all(diags: &crate::diag::Diagnostics) -> Vec<ClassifiedDiagnostic> {
    diags
        .iter()
        .map(|d| classify_diagnostic(&d.summary, &d.detail))
        .collect()
}

// --- Extraction helpers ---

fn extract_cycle_path(msg: &str) -> Vec<String> {
    // "circular dependency: a -> b -> a" or
    // "circular dependency: a (file.yaml) -> b (file.yaml) -> a (file.yaml)"
    if let Some(colon_pos) = msg.find(':') {
        let path_str = &msg[colon_pos + 1..].trim();
        return path_str
            .split(" -> ")
            .map(|s| {
                // Strip "(filename)" suffix if present
                if let Some(paren_pos) = s.find(" (") {
                    s[..paren_pos].trim().to_string()
                } else {
                    s.trim().to_string()
                }
            })
            .collect();
    }
    Vec::new()
}

fn extract_quoted_name(msg: &str, _context: &str) -> Option<String> {
    // Extract first 'name' from message
    let mut chars = msg.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\'' {
            let name: String = chars.by_ref().take_while(|&c| c != '\'').collect();
            if !name.is_empty() {
                return Some(name);
            }
        }
    }
    None
}

fn extract_did_you_mean(msg: &str) -> Option<String> {
    // "did you mean 'storageBucket'?"
    if let Some(pos) = msg.find("did you mean '") {
        let after = &msg[pos + 14..];
        if let Some(end) = after.find('\'') {
            return Some(after[..end].to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify_circular_dep() {
        let c = classify_diagnostic("circular dependency: a -> b -> a", "");
        assert_eq!(c.category, ErrorCategory::CircularDep);
        assert_eq!(c.cycle_path.as_ref().unwrap(), &["a", "b", "a"]);
    }

    #[test]
    fn test_classify_invalid_reference() {
        let c = classify_diagnostic(
            "resource or variable 'myRes' is not defined; did you mean 'myResource'?",
            "",
        );
        assert_eq!(c.category, ErrorCategory::InvalidReference);
        assert_eq!(c.bad_ref.as_deref(), Some("myRes"));
        assert_eq!(c.best_match.as_deref(), Some("myResource"));
    }

    #[test]
    fn test_classify_duplicate() {
        let c = classify_diagnostic("duplicate node name 'bucket'", "");
        assert_eq!(c.category, ErrorCategory::DuplicateName);
        assert_eq!(c.bad_ref.as_deref(), Some("bucket"));
    }

    #[test]
    fn test_classify_unknown() {
        let c = classify_diagnostic("something completely different", "");
        assert_eq!(c.category, ErrorCategory::Unknown);
    }

    #[test]
    fn test_classify_syntax() {
        let c = classify_diagnostic("syntax error in YAML", "bad indentation");
        assert_eq!(c.category, ErrorCategory::SyntaxError);
    }

    #[test]
    fn test_extract_cycle_path_with_files() {
        let path = extract_cycle_path("circular dependency: a (main.yaml) -> b (net.yaml) -> a (main.yaml)");
        assert_eq!(path, vec!["a", "b", "a"]);
    }

    #[test]
    fn test_classify_all() {
        let mut diags = crate::diag::Diagnostics::new();
        diags.error(None, "circular dependency: x -> y -> x", "");
        diags.warning(None, "something else", "");
        let classified = classify_all(&diags);
        assert_eq!(classified.len(), 2);
        assert_eq!(classified[0].category, ErrorCategory::CircularDep);
        assert_eq!(classified[1].category, ErrorCategory::Unknown);
    }
}

use crate::ast::property::{parse_property_access, PropertyAccess};
use crate::diag::Diagnostics;
use crate::syntax::Span;
use std::borrow::Cow;

/// A single part of an interpolated string.
///
/// Interpolations have the form `"text ${property.access} more text"`.
/// Each part has a text prefix and an optional property access reference.
#[derive(Debug, Clone, PartialEq)]
pub struct InterpolationPart<'src> {
    /// Literal text before the property access (or the trailing text).
    pub text: Cow<'src, str>,
    /// If present, the property access for this interpolation part.
    pub value: Option<PropertyAccess<'src>>,
}

/// Parses an interpolated string into its constituent parts.
///
/// Syntax:
/// - `$$` is an escaped dollar sign (produces a single `$`)
/// - `${...}` is a property access expression
/// - Everything else is literal text
pub fn parse_interpolation<'src>(
    input: &'src str,
    span: Option<Span>,
    diags: &mut Diagnostics,
) -> Vec<InterpolationPart<'src>> {
    let mut parts: Vec<InterpolationPart<'src>> = Vec::new();
    let mut current_text = String::new();
    let bytes = input.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() {
            match bytes[i + 1] {
                b'$' => {
                    // Escaped dollar sign
                    current_text.push('$');
                    i += 2;
                }
                b'{' => {
                    // Property access interpolation
                    let after_brace = &input[i + 2..];
                    let (rest, access) = parse_property_access(after_brace, span, diags);

                    if let Some(access) = access {
                        let text = if current_text.is_empty() {
                            Cow::Borrowed("")
                        } else {
                            Cow::Owned(std::mem::take(&mut current_text))
                        };
                        parts.push(InterpolationPart {
                            text,
                            value: Some(access),
                        });
                    }

                    // Calculate new position: input[i+2..] -> rest means we consumed
                    let consumed = after_brace.len() - rest.len();
                    i = i + 2 + consumed;
                }
                _ => {
                    current_text.push('$');
                    i += 1;
                }
            }
        } else {
            current_text.push(input[i..].chars().next().unwrap());
            i += input[i..].chars().next().unwrap().len_utf8();
        }
    }

    // Trailing text
    if !current_text.is_empty() {
        parts.push(InterpolationPart {
            text: Cow::Owned(current_text),
            value: None,
        });
    }

    parts
}

/// Returns true if the string contains any `${...}` interpolation markers.
pub fn has_interpolations(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'$' {
            if bytes[i + 1] == b'{' {
                return true;
            }
            if bytes[i + 1] == b'$' {
                i += 2;
                continue;
            }
        }
        i += 1;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(input: &str) -> Vec<InterpolationPart<'_>> {
        let mut diags = Diagnostics::new();
        let parts = parse_interpolation(input, None, &mut diags);
        assert!(!diags.has_errors(), "unexpected errors: {}", diags);
        parts
    }

    #[test]
    fn test_plain_text() {
        let parts = parse_ok("hello world");
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].text.as_ref(), "hello world");
        assert!(parts[0].value.is_none());
    }

    #[test]
    fn test_empty_string() {
        let parts = parse_ok("");
        assert!(parts.is_empty());
    }

    #[test]
    fn test_single_interpolation() {
        let parts = parse_ok("${resource.prop}");
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].text.as_ref(), "");
        let access = parts[0].value.as_ref().unwrap();
        assert_eq!(access.to_string(), "resource.prop");
    }

    #[test]
    fn test_text_with_interpolation() {
        let parts = parse_ok("prefix ${resource.prop} suffix");
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].text.as_ref(), "prefix ");
        assert!(parts[0].value.is_some());
        assert_eq!(parts[1].text.as_ref(), " suffix");
        assert!(parts[1].value.is_none());
    }

    #[test]
    fn test_multiple_interpolations() {
        let parts = parse_ok("${a.b}:${c.d}");
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].text.as_ref(), "");
        assert_eq!(parts[0].value.as_ref().unwrap().to_string(), "a.b");
        assert_eq!(parts[1].text.as_ref(), ":");
        assert_eq!(parts[1].value.as_ref().unwrap().to_string(), "c.d");
    }

    #[test]
    fn test_escaped_dollar() {
        let parts = parse_ok("cost is $$100");
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].text.as_ref(), "cost is $100");
        assert!(parts[0].value.is_none());
    }

    #[test]
    fn test_escaped_dollar_before_brace() {
        let parts = parse_ok("$${not.interp}");
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].text.as_ref(), "${not.interp}");
        assert!(parts[0].value.is_none());
    }

    #[test]
    fn test_has_interpolations() {
        assert!(has_interpolations("${foo}"));
        assert!(has_interpolations("hello ${foo} world"));
        assert!(!has_interpolations("hello world"));
        assert!(!has_interpolations("$${escaped}"));
        assert!(!has_interpolations("$100"));
        assert!(!has_interpolations(""));
    }

    #[test]
    fn test_just_symbol() {
        let parts = parse_ok("${myResource}");
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].text.as_ref(), "");
        let access = parts[0].value.as_ref().unwrap();
        assert_eq!(access.root_name(), "myResource");
    }

    #[test]
    fn test_index_access() {
        let parts = parse_ok("${arr[0]}");
        assert_eq!(parts.len(), 1);
        let access = parts[0].value.as_ref().unwrap();
        assert_eq!(access.to_string(), "arr[0]");
    }

    #[test]
    fn test_bracket_string_access() {
        let parts = parse_ok("${obj[\"key\"]}");
        assert_eq!(parts.len(), 1);
        let access = parts[0].value.as_ref().unwrap();
        assert_eq!(access.to_string(), "obj[\"key\"]");
    }
}

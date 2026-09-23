// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

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

/// Appends a borrowed run of `input` to the part being built.
///
/// A segment with no escape in it is one run, so it is borrowed and never
/// allocated. The first escape is what forces a copy, and from then on the
/// buffer grows in place.
#[inline]
fn push_run<'src>(text: &mut Cow<'src, str>, run: &'src str) {
    if run.is_empty() {
        return;
    }
    match text {
        Cow::Borrowed("") => *text = Cow::Borrowed(run),
        Cow::Borrowed(prev) => {
            let mut owned = String::with_capacity(prev.len() + run.len());
            owned.push_str(prev);
            owned.push_str(run);
            *text = Cow::Owned(owned);
        }
        Cow::Owned(owned) => owned.push_str(run),
    }
}

/// Appends the single `$` an escape collapses to.
#[inline]
fn push_dollar(text: &mut Cow<'_, str>) {
    match text {
        Cow::Borrowed(prev) => {
            let mut owned = String::with_capacity(prev.len() + 1);
            owned.push_str(prev);
            owned.push('$');
            *text = Cow::Owned(owned);
        }
        Cow::Owned(owned) => owned.push('$'),
    }
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
    // The part being built. Borrowed from `input` until an escape collapses,
    // so a string whose only marker is `${...}` copies no text at all.
    let mut text: Cow<'src, str> = Cow::Borrowed("");
    // Start of the run of bytes that can still be handed over as a borrow.
    let mut run_start = 0usize;
    let bytes = input.as_bytes();
    let mut i = 0usize;

    // Scanning bytes rather than chars is safe here and needs no boundary
    // check: `input` is `&str`, so it is valid UTF-8, and `$` (0x24) is ASCII,
    // so it can never appear inside a multi-byte sequence -- every byte that
    // stops the scan is therefore a char boundary, and so is every index this
    // loop slices at. Multi-byte text inside a run is copied as bytes, which
    // is what it already was.
    while i < bytes.len() {
        if bytes[i] != b'$' {
            i += 1;
            continue;
        }
        match bytes.get(i + 1) {
            // `$$` -- an escaped dollar sign.
            Some(b'$') => {
                push_run(&mut text, &input[run_start..i]);
                push_dollar(&mut text);
                i += 2;
                run_start = i;
            }
            // `${...}` -- a property access.
            Some(b'{') => {
                push_run(&mut text, &input[run_start..i]);
                let after_brace = &input[i + 2..];
                let (rest, access) = parse_property_access(after_brace, span, diags);

                if let Some(access) = access {
                    parts.push(InterpolationPart {
                        text: std::mem::replace(&mut text, Cow::Borrowed("")),
                        value: Some(access),
                    });
                }

                // input[i+2..] -> rest means this much was consumed.
                let consumed = after_brace.len() - rest.len();
                i = i + 2 + consumed;
                run_start = i;
            }
            // A lone `$`, including one at the very end: literal text, so it
            // stays inside the run and costs nothing.
            _ => i += 1,
        }
    }

    // Trailing text
    push_run(&mut text, &input[run_start..]);
    if !text.is_empty() {
        parts.push(InterpolationPart { text, value: None });
    }

    parts
}

/// Returns true when `s` has to go through [`parse_interpolation`].
///
/// Two markers require the pass, and both are the parser's own syntax: `${` in
/// front of a property access, which resolves to a value, and `$$`, which
/// collapses to one `$`. A string carrying only the escape has nothing to
/// resolve, but the text it stands for is not the text that was written, so it
/// is not a literal either.
///
/// Asking for `${` alone is what made `"FROM $${data()}"` a literal: the
/// predicate stepped over the escape looking for an interpolation, found none,
/// and the caller handed both dollars to a provider verbatim.
pub fn needs_interpolation_pass(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'$' && (bytes[i + 1] == b'{' || bytes[i + 1] == b'$') {
            return true;
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
    fn a_property_access_needs_the_pass() {
        assert!(needs_interpolation_pass("${foo}"));
        assert!(needs_interpolation_pass("hello ${foo} world"));
    }

    #[test]
    fn an_escape_needs_the_pass_with_no_interpolation_in_sight() {
        // The regression: each of these carries an escape and nothing to
        // resolve. Answering false here left both dollars in the value.
        assert!(needs_interpolation_pass("$${escaped}"));
        assert!(needs_interpolation_pass("FROM $${data()}"));
        assert!(needs_interpolation_pass("cost is $$100"));
        assert!(needs_interpolation_pass("$$"));
    }

    #[test]
    fn text_with_no_marker_skips_the_pass() {
        assert!(!needs_interpolation_pass("hello world"));
        assert!(!needs_interpolation_pass("$100"));
        assert!(!needs_interpolation_pass("trailing $"));
        assert!(!needs_interpolation_pass("$"));
        assert!(!needs_interpolation_pass(""));
    }

    #[test]
    fn test_just_symbol() {
        let parts = parse_ok("${myResource}");
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].text.as_ref(), "");
        let access = parts[0].value.as_ref().unwrap();
        assert_eq!(access.root_name().unwrap(), "myResource");
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

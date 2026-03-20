//! GAT-based property resolver (Phase B5).
//!
//! Provides a `PropertyResolver` trait with two implementations:
//! - `BorrowedResolver`: returns `&'v Value<'src>` (zero-copy for simple `${res.prop}`)
//! - `OwnedResolver`: returns `Value<'src>` (for computed/interpolated chains)
//!
//! This eliminates the leaf clone in `eval_property_access` for the common case
//! where the caller only needs to read the value, not own it.

use crate::ast::property::PropertyAccessor;
use crate::diag::Diagnostics;
use crate::eval::value::Value;

// ---------------------------------------------------------------------------
// GAT-based trait (B5)
// ---------------------------------------------------------------------------

/// GAT-based trait for resolving property access chains on `Value`.
///
/// The associated type `Output<'v>` carries the borrow lifetime, enabling
/// zero-copy resolution when the value tree contains no secrets.
pub trait PropertyResolver<'src> {
    /// The output type, parameterized by the borrow lifetime.
    /// - `BorrowedResolver`: `&'v Value<'src>`
    /// - `OwnedResolver`: `Value<'src>`
    type Output<'v>
    where
        Self: 'v;

    /// Resolve a chain of property accessors against this resolver's root value.
    ///
    /// Returns `None` on error (diagnostic emitted) or if the resolver cannot
    /// handle the access chain (e.g., borrowed resolver encountering a Secret).
    fn resolve<'v>(
        &'v self,
        accessors: &[PropertyAccessor<'_>],
        diags: &mut Diagnostics,
    ) -> Option<Self::Output<'v>>;
}

// ---------------------------------------------------------------------------
// BorrowedResolver — zero-copy, no secrets
// ---------------------------------------------------------------------------

/// Zero-copy property resolver that returns references into the value tree.
///
/// Returns `None` (without emitting diagnostics) if a `Secret` wrapper is
/// encountered, signaling the caller to fall back to `OwnedResolver`.
pub struct BorrowedResolver<'src, 'a> {
    root: &'a Value<'src>,
}

impl<'src, 'a> BorrowedResolver<'src, 'a> {
    /// Create a new borrowed resolver rooted at the given value.
    #[inline]
    pub fn new(root: &'a Value<'src>) -> Self {
        Self { root }
    }
}

impl<'src, 'a> PropertyResolver<'src> for BorrowedResolver<'src, 'a> {
    type Output<'v>
        = &'v Value<'src>
    where
        Self: 'v;

    fn resolve<'v>(
        &'v self,
        accessors: &[PropertyAccessor<'_>],
        diags: &mut Diagnostics,
    ) -> Option<&'v Value<'src>> {
        let mut current: &Value<'src> = self.root;

        for accessor in accessors {
            match accessor {
                PropertyAccessor::Name(name) | PropertyAccessor::StringSubscript(name) => {
                    match current {
                        Value::Object(entries) => {
                            match entries.iter().find(|(k, _)| k.as_ref() == name.as_ref()) {
                                Some((_, v)) => current = v,
                                None => return Some(&NULL_VALUE),
                            }
                        }
                        // Cannot resolve through secrets without cloning — signal fallback
                        Value::Secret(_) => return None,
                        Value::Null | Value::Unknown => return Some(current),
                        _ => {
                            diags.error(
                                None,
                                format!(
                                    "cannot access .{} on {}",
                                    name.as_ref(),
                                    current.type_name()
                                ),
                                "",
                            );
                            return None;
                        }
                    }
                }
                PropertyAccessor::IntSubscript(idx) => {
                    let i = *idx;
                    match current {
                        Value::List(items) => {
                            if i < 0 || (i as usize) >= items.len() {
                                diags.error(
                                    None,
                                    format!(
                                        "index {} out of bounds for list of length {}",
                                        i,
                                        items.len()
                                    ),
                                    "",
                                );
                                return None;
                            }
                            current = &items[i as usize];
                        }
                        // Cannot resolve through secrets without cloning — signal fallback
                        Value::Secret(_) => return None,
                        Value::Null | Value::Unknown => return Some(current),
                        _ => {
                            diags.error(
                                None,
                                format!("cannot index into {}", current.type_name()),
                                "",
                            );
                            return None;
                        }
                    }
                }
            }
        }

        Some(current)
    }
}

/// Static null value for returning references to Null from the borrowed resolver.
static NULL_VALUE: Value<'static> = Value::Null;

// ---------------------------------------------------------------------------
// OwnedResolver — handles secrets, returns owned values
// ---------------------------------------------------------------------------

/// Owned property resolver that handles `Secret` wrappers by unwrapping,
/// resolving the inner value, and re-wrapping the result.
///
/// This is the fallback path used when `BorrowedResolver` encounters a Secret.
pub struct OwnedResolver<'src, 'a> {
    root: &'a Value<'src>,
}

impl<'src, 'a> OwnedResolver<'src, 'a> {
    /// Create a new owned resolver rooted at the given value.
    #[inline]
    pub fn new(root: &'a Value<'src>) -> Self {
        Self { root }
    }
}

impl<'src, 'a> PropertyResolver<'src> for OwnedResolver<'src, 'a> {
    type Output<'v>
        = Value<'src>
    where
        Self: 'v;

    fn resolve<'v>(
        &'v self,
        accessors: &[PropertyAccessor<'_>],
        diags: &mut Diagnostics,
    ) -> Option<Value<'src>> {
        resolve_owned(self.root, accessors, diags)
    }
}

/// Inner implementation for owned resolution (supports recursion for secrets).
fn resolve_owned<'src>(
    value: &Value<'src>,
    accessors: &[PropertyAccessor<'_>],
    diags: &mut Diagnostics,
) -> Option<Value<'src>> {
    let mut current: &Value<'src> = value;

    for (i, accessor) in accessors.iter().enumerate() {
        match accessor {
            PropertyAccessor::Name(name) | PropertyAccessor::StringSubscript(name) => {
                match current {
                    Value::Object(entries) => {
                        match entries.iter().find(|(k, _)| k.as_ref() == name.as_ref()) {
                            Some((_, v)) => current = v,
                            None => return Some(Value::Null),
                        }
                    }
                    Value::Secret(inner) => {
                        let result =
                            resolve_owned(inner, std::slice::from_ref(accessor), diags)?;
                        // Continue resolving the remaining accessors on the unwrapped result
                        let remaining = &accessors[i + 1..];
                        if remaining.is_empty() {
                            return Some(Value::Secret(Box::new(result)));
                        }
                        let continued = resolve_owned(&result, remaining, diags)?;
                        return Some(Value::Secret(Box::new(continued)));
                    }
                    Value::Null | Value::Unknown => return Some(current.clone()),
                    _ => {
                        diags.error(
                            None,
                            format!(
                                "cannot access .{} on {}",
                                name.as_ref(),
                                current.type_name()
                            ),
                            "",
                        );
                        return None;
                    }
                }
            }
            PropertyAccessor::IntSubscript(idx) => {
                let i_val = *idx;
                match current {
                    Value::List(items) => {
                        if i_val < 0 || (i_val as usize) >= items.len() {
                            diags.error(
                                None,
                                format!(
                                    "index {} out of bounds for list of length {}",
                                    i_val,
                                    items.len()
                                ),
                                "",
                            );
                            return None;
                        }
                        current = &items[i_val as usize];
                    }
                    Value::Secret(inner) => {
                        let result =
                            resolve_owned(inner, std::slice::from_ref(accessor), diags)?;
                        let remaining = &accessors[i + 1..];
                        if remaining.is_empty() {
                            return Some(Value::Secret(Box::new(result)));
                        }
                        let continued = resolve_owned(&result, remaining, diags)?;
                        return Some(Value::Secret(Box::new(continued)));
                    }
                    Value::Null | Value::Unknown => return Some(current.clone()),
                    _ => {
                        diags.error(
                            None,
                            format!("cannot index into {}", current.type_name()),
                            "",
                        );
                        return None;
                    }
                }
            }
        }
    }

    Some(current.clone())
}

// ---------------------------------------------------------------------------
// Convenience: try-borrowed-then-owned resolution
// ---------------------------------------------------------------------------

/// Attempt zero-copy resolution first; fall back to owned resolution only
/// when a `Secret` wrapper is encountered.
///
/// This is the recommended entry point for callers that need an owned `Value`
/// but want to avoid cloning in the common (non-secret) case.
#[inline]
pub fn resolve_property<'src>(
    value: &Value<'src>,
    accessors: &[PropertyAccessor<'_>],
    diags: &mut Diagnostics,
) -> Option<Value<'src>> {
    // Fast path: try borrowed resolution (no clone unless we succeed)
    let borrowed = BorrowedResolver::new(value);
    // We need a fresh diagnostics to avoid emitting errors from the borrowed
    // path when we'll retry with the owned path (Secret case).
    let mut borrow_diags = Diagnostics::new();
    if let Some(val) = borrowed.resolve(accessors, &mut borrow_diags) {
        return Some(val.clone());
    }

    // If the borrowed resolver failed without errors, it hit a Secret.
    // Fall back to the owned resolver which handles Secret unwrapping.
    if !borrow_diags.has_errors() {
        let owned = OwnedResolver::new(value);
        return owned.resolve(accessors, diags);
    }

    // The borrowed resolver emitted real errors — replay them.
    diags.extend(borrow_diags);
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(val: &str) -> Value<'static> {
        Value::String(Cow::Owned(val.to_string()))
    }

    fn n(val: f64) -> Value<'static> {
        Value::Number(val)
    }

    // -----------------------------------------------------------------------
    // BorrowedResolver tests
    // -----------------------------------------------------------------------

    #[test]
    fn borrowed_simple_name() {
        let mut diags = Diagnostics::new();
        let val = Value::Object(vec![
            (Cow::Owned("name".into()), s("test")),
            (Cow::Owned("count".into()), n(42.0)),
        ]);
        let resolver = BorrowedResolver::new(&val);
        let result = resolver
            .resolve(&[PropertyAccessor::Name(Cow::Borrowed("name"))], &mut diags)
            .unwrap();
        assert_eq!(result.as_str(), Some("test"));
        assert!(!diags.has_errors());
    }

    #[test]
    fn borrowed_chain() {
        let mut diags = Diagnostics::new();
        let val = Value::Object(vec![(
            Cow::Owned("outer".into()),
            Value::Object(vec![(Cow::Owned("inner".into()), s("deep"))]),
        )]);
        let resolver = BorrowedResolver::new(&val);
        let result = resolver
            .resolve(
                &[
                    PropertyAccessor::Name(Cow::Borrowed("outer")),
                    PropertyAccessor::Name(Cow::Borrowed("inner")),
                ],
                &mut diags,
            )
            .unwrap();
        assert_eq!(result.as_str(), Some("deep"));
    }

    #[test]
    fn borrowed_index() {
        let mut diags = Diagnostics::new();
        let val = Value::List(vec![s("first"), s("second")]);
        let resolver = BorrowedResolver::new(&val);
        let result = resolver
            .resolve(&[PropertyAccessor::IntSubscript(1)], &mut diags)
            .unwrap();
        assert_eq!(result.as_str(), Some("second"));
    }

    #[test]
    fn borrowed_missing_returns_null() {
        let mut diags = Diagnostics::new();
        let val = Value::Object(vec![]);
        let resolver = BorrowedResolver::new(&val);
        let result = resolver
            .resolve(
                &[PropertyAccessor::Name(Cow::Borrowed("missing"))],
                &mut diags,
            )
            .unwrap();
        assert!(result.is_null());
    }

    #[test]
    fn borrowed_null_passthrough() {
        let mut diags = Diagnostics::new();
        let val = Value::Null;
        let resolver = BorrowedResolver::new(&val);
        let result = resolver
            .resolve(
                &[PropertyAccessor::Name(Cow::Borrowed("x"))],
                &mut diags,
            )
            .unwrap();
        assert!(result.is_null());
    }

    #[test]
    fn borrowed_returns_none_on_secret() {
        let mut diags = Diagnostics::new();
        let val = Value::Secret(Box::new(Value::Object(vec![(
            Cow::Owned("key".into()),
            s("hidden"),
        )])));
        let resolver = BorrowedResolver::new(&val);
        let result = resolver.resolve(
            &[PropertyAccessor::Name(Cow::Borrowed("key"))],
            &mut diags,
        );
        // Should return None (signal fallback) without emitting errors
        assert!(result.is_none());
        assert!(!diags.has_errors());
    }

    // -----------------------------------------------------------------------
    // OwnedResolver tests
    // -----------------------------------------------------------------------

    #[test]
    fn owned_simple_name() {
        let mut diags = Diagnostics::new();
        let val = Value::Object(vec![(Cow::Owned("name".into()), s("test"))]);
        let resolver = OwnedResolver::new(&val);
        let result = resolver
            .resolve(&[PropertyAccessor::Name(Cow::Borrowed("name"))], &mut diags)
            .unwrap();
        assert_eq!(result.as_str(), Some("test"));
    }

    #[test]
    fn owned_through_secret() {
        let mut diags = Diagnostics::new();
        let val = Value::Secret(Box::new(Value::Object(vec![(
            Cow::Owned("key".into()),
            s("secret-val"),
        )])));
        let resolver = OwnedResolver::new(&val);
        let result = resolver
            .resolve(&[PropertyAccessor::Name(Cow::Borrowed("key"))], &mut diags)
            .unwrap();
        match &result {
            Value::Secret(inner) => assert_eq!(inner.as_str(), Some("secret-val")),
            _ => panic!("expected secret wrapping, got {:?}", result),
        }
    }

    #[test]
    fn owned_index_oob() {
        let mut diags = Diagnostics::new();
        let val = Value::List(vec![s("only")]);
        let resolver = OwnedResolver::new(&val);
        let result = resolver.resolve(&[PropertyAccessor::IntSubscript(5)], &mut diags);
        assert!(diags.has_errors());
        assert!(result.is_none());
    }

    // -----------------------------------------------------------------------
    // resolve_property (try-borrowed-then-owned) tests
    // -----------------------------------------------------------------------

    #[test]
    fn resolve_property_non_secret() {
        let mut diags = Diagnostics::new();
        let val = Value::Object(vec![(Cow::Owned("x".into()), n(1.0))]);
        let result = resolve_property(
            &val,
            &[PropertyAccessor::Name(Cow::Borrowed("x"))],
            &mut diags,
        )
        .unwrap();
        assert_eq!(result, n(1.0));
    }

    #[test]
    fn resolve_property_through_secret() {
        let mut diags = Diagnostics::new();
        let val = Value::Secret(Box::new(Value::Object(vec![(
            Cow::Owned("key".into()),
            s("secret-val"),
        )])));
        let result = resolve_property(
            &val,
            &[PropertyAccessor::Name(Cow::Borrowed("key"))],
            &mut diags,
        )
        .unwrap();
        match &result {
            Value::Secret(inner) => assert_eq!(inner.as_str(), Some("secret-val")),
            _ => panic!("expected secret wrapping"),
        }
    }

    #[test]
    fn resolve_property_error_propagates() {
        let mut diags = Diagnostics::new();
        let val = Value::Number(42.0);
        let result = resolve_property(
            &val,
            &[PropertyAccessor::Name(Cow::Borrowed("x"))],
            &mut diags,
        );
        assert!(result.is_none());
        assert!(diags.has_errors());
    }
}

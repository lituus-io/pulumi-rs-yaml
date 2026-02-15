use crate::diag::Diagnostics;
use crate::syntax::Span;
use std::borrow::Cow;
use std::fmt;

/// A chain of property accesses (e.g. `resource.nested[0].prop`).
#[derive(Debug, Clone, PartialEq)]
pub struct PropertyAccess<'src> {
    pub accessors: Vec<PropertyAccessor<'src>>,
}

/// A single step in a property access chain.
#[derive(Debug, Clone, PartialEq)]
pub enum PropertyAccessor<'src> {
    /// A named property access (e.g. `.name` or the root `name`).
    Name(Cow<'src, str>),
    /// A subscript access with a string key (e.g. `["key"]`).
    StringSubscript(Cow<'src, str>),
    /// A subscript access with an integer index (e.g. `[0]`).
    IntSubscript(i64),
}

impl PropertyAccess<'_> {
    /// Returns the root name of the access chain.
    pub fn root_name(&self) -> &str {
        match &self.accessors[0] {
            PropertyAccessor::Name(n) => n.as_ref(),
            PropertyAccessor::StringSubscript(n) => n.as_ref(),
            PropertyAccessor::IntSubscript(_) => panic!("root cannot be integer subscript"),
        }
    }
}

impl fmt::Display for PropertyAccess<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, accessor) in self.accessors.iter().enumerate() {
            match accessor {
                PropertyAccessor::Name(name) => {
                    if i != 0 {
                        write!(f, ".")?;
                    }
                    write!(f, "{}", name)?;
                }
                PropertyAccessor::StringSubscript(key) => {
                    let escaped = key.replace('"', "\\\"");
                    write!(f, "[\"{}\"]", escaped)?;
                }
                PropertyAccessor::IntSubscript(idx) => {
                    write!(f, "[{}]", idx)?;
                }
            }
        }
        Ok(())
    }
}

/// Regex pattern for valid property names: `[a-zA-Z_$][a-zA-Z0-9_$]*`.
pub fn is_valid_property_name(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '$' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

/// Parses a property access string, consuming up to the closing `}` for interpolation.
///
/// Returns `(remaining_input, parsed_access)`.
///
/// The input is expected to start after the `${` of an interpolation expression.
/// The parser consumes through the matching `}`.
pub fn parse_property_access<'src>(
    input: &'src str,
    span: Option<Span>,
    diags: &mut Diagnostics,
) -> (&'src str, Option<PropertyAccess<'src>>) {
    let mut accessors: Vec<PropertyAccessor<'src>> = Vec::new();
    let mut remaining = input;

    while !remaining.is_empty() {
        let first = remaining.as_bytes()[0];
        match first {
            b'}' => {
                // End of interpolation
                return (&remaining[1..], Some(PropertyAccess { accessors }));
            }
            b'.' => {
                remaining = &remaining[1..];
            }
            b'[' => {
                // Check for string key: ["..."]
                if remaining.len() > 1 && remaining.as_bytes()[1] == b'"' {
                    let mut key = Vec::new();
                    let mut i = 2;
                    let bytes = remaining.as_bytes();
                    loop {
                        if i >= bytes.len() {
                            diags.error(span, "missing closing quote in property name", "");
                            return ("", None);
                        } else if bytes[i] == b'"' {
                            i += 1;
                            break;
                        } else if bytes[i] == b'\\' && i + 1 < bytes.len() && bytes[i + 1] == b'"' {
                            key.push(b'"');
                            i += 2;
                        } else {
                            key.push(bytes[i]);
                            i += 1;
                        }
                    }
                    if i >= bytes.len() || bytes[i] != b']' {
                        diags.error(span, "missing closing bracket in property access", "");
                        return ("", None);
                    }
                    let key_str = String::from_utf8(key).unwrap_or_default();
                    accessors.push(PropertyAccessor::StringSubscript(Cow::Owned(key_str)));
                    remaining = &remaining[i + 1..];
                } else {
                    // Numeric index: [N]
                    let rbracket = match remaining.find(']') {
                        Some(pos) => pos,
                        None => {
                            diags.error(span, "missing closing bracket in list index", "");
                            return ("", None);
                        }
                    };
                    let index_str = &remaining[1..rbracket];
                    let index: i64 = match index_str.parse() {
                        Ok(v) => v,
                        Err(_) => {
                            diags.error(span, "invalid list index", "");
                            return ("", None);
                        }
                    };
                    if accessors.is_empty() {
                        diags.error(
                            span,
                            "the root property must be a string subscript or a name",
                            "",
                        );
                        return ("", None);
                    }
                    accessors.push(PropertyAccessor::IntSubscript(index));
                    remaining = &remaining[rbracket + 1..];
                }
            }
            _ => {
                // Read a property name
                let end = remaining.find(['.', '[', '}']).unwrap_or(remaining.len());
                let name = &remaining[..end];
                accessors.push(PropertyAccessor::Name(Cow::Borrowed(name)));
                remaining = &remaining[end..];
            }
        }
    }

    diags.error(span, "unterminated interpolation", "");
    ("", None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(input: &str) -> (String, PropertyAccess<'_>) {
        let mut diags = Diagnostics::new();
        let (rest, access) = parse_property_access(input, None, &mut diags);
        assert!(!diags.has_errors(), "unexpected errors: {}", diags);
        (rest.to_string(), access.unwrap())
    }

    #[test]
    fn test_simple_name() {
        let (rest, access) = parse_ok("root}");
        assert_eq!(rest, "");
        assert_eq!(access.root_name(), "root");
        assert_eq!(access.to_string(), "root");
    }

    #[test]
    fn test_nested_property() {
        let (rest, access) = parse_ok("root.nested}");
        assert_eq!(rest, "");
        assert_eq!(access.to_string(), "root.nested");
    }

    #[test]
    fn test_double_nested() {
        let (rest, access) = parse_ok("root.double.nest}");
        assert_eq!(rest, "");
        assert_eq!(access.to_string(), "root.double.nest");
    }

    #[test]
    fn test_bracket_string() {
        let (rest, access) = parse_ok("root[\"nested\"]}");
        assert_eq!(rest, "");
        assert_eq!(access.to_string(), "root[\"nested\"]");
    }

    #[test]
    fn test_array_index() {
        let (rest, access) = parse_ok("root.array[0]}");
        assert_eq!(rest, "");
        assert_eq!(access.to_string(), "root.array[0]");
    }

    #[test]
    fn test_complex_chain() {
        let (rest, access) = parse_ok("root.nested.array[0].double[1]}");
        assert_eq!(rest, "");
        assert_eq!(access.to_string(), "root.nested.array[0].double[1]");
    }

    #[test]
    fn test_escaped_quotes() {
        let (rest, access) = parse_ok("root[\"key with \\\"escaped\\\" quotes\"]}");
        assert_eq!(rest, "");
        assert_eq!(
            access.to_string(),
            "root[\"key with \\\"escaped\\\" quotes\"]"
        );
    }

    #[test]
    fn test_remaining_input() {
        let (rest, access) = parse_ok("root.prop} more text");
        assert_eq!(rest, " more text");
        assert_eq!(access.to_string(), "root.prop");
    }

    #[test]
    fn test_root_bracket_string() {
        let (rest, access) = parse_ok("[\"root key\"]}");
        assert_eq!(rest, "");
        assert_eq!(access.to_string(), "[\"root key\"]");
    }

    #[test]
    fn test_root_int_subscript_error() {
        let mut diags = Diagnostics::new();
        let (_, access) = parse_property_access("[0]}", None, &mut diags);
        assert!(diags.has_errors());
        assert!(access.is_none());
    }

    #[test]
    fn test_unterminated_error() {
        let mut diags = Diagnostics::new();
        let (_, access) = parse_property_access("root.prop", None, &mut diags);
        assert!(diags.has_errors());
        assert!(access.is_none());
    }

    #[test]
    fn test_missing_closing_quote() {
        let mut diags = Diagnostics::new();
        let (_, access) = parse_property_access("root[\"unclosed}", None, &mut diags);
        assert!(diags.has_errors());
        assert!(access.is_none());
    }

    #[test]
    fn test_is_valid_property_name() {
        assert!(is_valid_property_name("foo"));
        assert!(is_valid_property_name("_bar"));
        assert!(is_valid_property_name("$baz"));
        assert!(is_valid_property_name("foo123"));
        assert!(!is_valid_property_name(""));
        assert!(!is_valid_property_name("123foo"));
        assert!(!is_valid_property_name("foo-bar"));
    }
}

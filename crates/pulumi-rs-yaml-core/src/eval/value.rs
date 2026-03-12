use std::borrow::Cow;
use std::fmt;

/// Runtime value during evaluation. Replaces Go's `interface{}`.
///
/// Uses `Cow<'src, str>` so values can borrow from source text or own data
/// from gRPC responses.
#[derive(Clone, PartialEq)]
pub enum Value<'src> {
    Null,
    Bool(bool),
    Number(f64),
    String(Cow<'src, str>),
    List(Vec<Value<'src>>),
    Object(Vec<(Cow<'src, str>, Value<'src>)>),
    Secret(Box<Value<'src>>),
    /// Reference to a registered resource by index.
    Resource(ResourceRef),
    /// An asset value.
    Asset(Asset<'src>),
    /// An archive value.
    Archive(Archive<'src>),
    /// Unknown value (preview mode).
    Unknown,
}

/// Reference to a resource by index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ResourceRef(pub u32);

/// An asset value.
#[derive(Debug, Clone, PartialEq)]
pub enum Asset<'src> {
    String(Cow<'src, str>),
    File(Cow<'src, str>),
    Remote(Cow<'src, str>),
}

/// An archive value.
#[derive(Debug, Clone, PartialEq)]
pub enum Archive<'src> {
    File(Cow<'src, str>),
    Remote(Cow<'src, str>),
    Assets(Vec<(Cow<'src, str>, Value<'src>)>),
}

impl fmt::Debug for Value<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => write!(f, "Null"),
            Value::Bool(b) => f.debug_tuple("Bool").field(b).finish(),
            Value::Number(n) => f.debug_tuple("Number").field(n).finish(),
            Value::String(s) => f.debug_tuple("String").field(s).finish(),
            Value::List(items) => f.debug_tuple("List").field(items).finish(),
            Value::Object(entries) => f.debug_tuple("Object").field(entries).finish(),
            Value::Secret(_) => write!(f, "Secret([REDACTED])"),
            Value::Resource(r) => f.debug_tuple("Resource").field(r).finish(),
            Value::Asset(a) => f.debug_tuple("Asset").field(a).finish(),
            Value::Archive(a) => f.debug_tuple("Archive").field(a).finish(),
            Value::Unknown => write!(f, "Unknown"),
        }
    }
}

impl<'src> Value<'src> {
    /// Returns true if this is a null value.
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// Returns true if this is a secret value.
    pub fn is_secret(&self) -> bool {
        matches!(self, Value::Secret(_))
    }

    /// Returns true if this is an unknown value.
    pub fn is_unknown(&self) -> bool {
        matches!(self, Value::Unknown)
    }

    /// Tries to get the value as a string slice.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s.as_ref()),
            _ => None,
        }
    }

    /// Tries to get the value as a bool.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// Tries to get the value as a f64.
    pub fn as_number(&self) -> Option<f64> {
        match self {
            Value::Number(n) => Some(*n),
            _ => None,
        }
    }

    /// Unwraps secrets, returning the inner value.
    pub fn unwrap_secret(&self) -> &Value<'src> {
        match self {
            Value::Secret(inner) => inner.unwrap_secret(),
            other => other,
        }
    }

    /// Converts all borrowed strings to owned, producing a `Value<'static>`.
    pub fn into_owned(self) -> Value<'static> {
        match self {
            Value::Null => Value::Null,
            Value::Bool(b) => Value::Bool(b),
            Value::Number(n) => Value::Number(n),
            Value::String(s) => Value::String(Cow::Owned(s.into_owned())),
            Value::List(items) => Value::List(items.into_iter().map(|v| v.into_owned()).collect()),
            Value::Object(entries) => Value::Object(
                entries
                    .into_iter()
                    .map(|(k, v)| (Cow::Owned(k.into_owned()), v.into_owned()))
                    .collect(),
            ),
            Value::Secret(inner) => Value::Secret(Box::new(inner.into_owned())),
            Value::Resource(r) => Value::Resource(r),
            Value::Asset(a) => Value::Asset(match a {
                Asset::String(s) => Asset::String(Cow::Owned(s.into_owned())),
                Asset::File(s) => Asset::File(Cow::Owned(s.into_owned())),
                Asset::Remote(s) => Asset::Remote(Cow::Owned(s.into_owned())),
            }),
            Value::Archive(a) => Value::Archive(match a {
                Archive::File(s) => Archive::File(Cow::Owned(s.into_owned())),
                Archive::Remote(s) => Archive::Remote(Cow::Owned(s.into_owned())),
                Archive::Assets(entries) => Archive::Assets(
                    entries
                        .into_iter()
                        .map(|(k, v)| (Cow::Owned(k.into_owned()), v.into_owned()))
                        .collect(),
                ),
            }),
            Value::Unknown => Value::Unknown,
        }
    }

    /// Converts a `serde_json::Value` to a `Value<'static>`.
    pub fn from_json(v: &serde_json::Value) -> Value<'static> {
        match v {
            serde_json::Value::Null => Value::Null,
            serde_json::Value::Bool(b) => Value::Bool(*b),
            serde_json::Value::Number(n) => Value::Number(n.as_f64().unwrap_or(0.0)),
            serde_json::Value::String(s) => Value::String(Cow::Owned(s.clone())),
            serde_json::Value::Array(arr) => {
                Value::List(arr.iter().map(Value::from_json).collect())
            }
            serde_json::Value::Object(obj) => Value::Object(
                obj.iter()
                    .map(|(k, v)| (Cow::Owned(k.clone()), Value::from_json(v)))
                    .collect(),
            ),
        }
    }

    /// Converts this value to a `serde_json::Value`.
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Value::Null => serde_json::Value::Null,
            Value::Bool(b) => serde_json::Value::Bool(*b),
            Value::Number(n) => serde_json::Number::from_f64(*n)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null),
            Value::String(s) => serde_json::Value::String(s.to_string()),
            Value::List(items) => {
                serde_json::Value::Array(items.iter().map(|v| v.to_json()).collect())
            }
            Value::Object(entries) => {
                let map: serde_json::Map<String, serde_json::Value> = entries
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_json()))
                    .collect();
                serde_json::Value::Object(map)
            }
            Value::Secret(inner) => inner.to_json(),
            Value::Unknown => serde_json::Value::Null,
            _ => serde_json::Value::Null,
        }
    }

    /// Returns a type name for error messages.
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Bool(_) => "bool",
            Value::Number(_) => "number",
            Value::String(_) => "string",
            Value::List(_) => "list",
            Value::Object(_) => "object",
            Value::Secret(_) => "secret",
            Value::Resource(_) => "resource",
            Value::Asset(_) => "asset",
            Value::Archive(_) => "archive",
            Value::Unknown => "unknown",
        }
    }
}

impl fmt::Display for Value<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => write!(f, "null"),
            Value::Bool(b) => write!(f, "{}", b),
            Value::Number(n) => write!(f, "{}", n),
            Value::String(s) => write!(f, "{}", s),
            Value::List(items) => {
                write!(f, "[")?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", item)?;
                }
                write!(f, "]")
            }
            Value::Object(entries) => {
                write!(f, "{{")?;
                for (i, (k, v)) in entries.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}: {}", k, v)?;
                }
                write!(f, "}}")
            }
            Value::Secret(_) => write!(f, "[secret]"),
            Value::Resource(r) => write!(f, "resource({})", r.0),
            Value::Asset(_) => write!(f, "[asset]"),
            Value::Archive(_) => write!(f, "[archive]"),
            Value::Unknown => write!(f, "[unknown]"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_value_null() {
        let v = Value::Null;
        assert!(v.is_null());
        assert!(!v.is_secret());
        assert!(!v.is_unknown());
        assert_eq!(v.type_name(), "null");
    }

    #[test]
    fn test_value_string() {
        let v = Value::String(Cow::Borrowed("hello"));
        assert_eq!(v.as_str(), Some("hello"));
        assert_eq!(v.type_name(), "string");
    }

    #[test]
    fn test_value_secret() {
        let v = Value::Secret(Box::new(Value::String(Cow::Borrowed("pw"))));
        assert!(v.is_secret());
        assert_eq!(v.unwrap_secret().as_str(), Some("pw"));
    }

    #[test]
    fn test_value_nested_secret_unwrap() {
        let v = Value::Secret(Box::new(Value::Secret(Box::new(Value::Bool(true)))));
        assert_eq!(v.unwrap_secret().as_bool(), Some(true));
    }

    #[test]
    fn test_value_into_owned() {
        let v = Value::Object(vec![(
            Cow::Borrowed("key"),
            Value::String(Cow::Borrowed("val")),
        )]);
        let owned: Value<'static> = v.into_owned();
        match &owned {
            Value::Object(entries) => {
                assert_eq!(entries[0].0.as_ref(), "key");
                assert_eq!(entries[0].1.as_str(), Some("val"));
            }
            _ => panic!("expected object"),
        }
    }

    #[test]
    fn test_debug_does_not_leak_secret() {
        let secret = Value::Secret(Box::new(Value::String(Cow::Borrowed("super-secret-pw"))));
        let debug_str = format!("{:?}", secret);
        assert!(
            !debug_str.contains("super-secret-pw"),
            "Debug output leaked secret: {}",
            debug_str
        );
        assert!(debug_str.contains("REDACTED"));
    }

    #[test]
    fn test_display_masks_secret() {
        let secret = Value::Secret(Box::new(Value::String(Cow::Borrowed("super-secret-pw"))));
        let display_str = format!("{}", secret);
        assert!(
            !display_str.contains("super-secret-pw"),
            "Display output leaked secret: {}",
            display_str
        );
        assert!(display_str.contains("[secret]"));
    }

    #[test]
    fn test_debug_non_secret_values() {
        // Ensure non-secret values still show correctly in Debug
        assert!(format!("{:?}", Value::Null).contains("Null"));
        assert!(format!("{:?}", Value::Bool(true)).contains("true"));
        assert!(format!("{:?}", Value::Number(42.0)).contains("42"));
        assert!(format!("{:?}", Value::String(Cow::Borrowed("hi"))).contains("hi"));
        assert!(format!("{:?}", Value::Unknown).contains("Unknown"));
    }

    #[test]
    fn test_value_display() {
        assert_eq!(Value::Null.to_string(), "null");
        assert_eq!(Value::Bool(true).to_string(), "true");
        assert_eq!(Value::Number(42.0).to_string(), "42");
        assert_eq!(Value::String(Cow::Borrowed("hi")).to_string(), "hi");
        assert_eq!(Value::Unknown.to_string(), "[unknown]");
    }
}

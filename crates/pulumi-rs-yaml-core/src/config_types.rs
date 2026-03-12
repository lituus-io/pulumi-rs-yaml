use std::fmt;

/// Represents the type of a configuration parameter.
///
/// Matches the Go `config.Type` interface with known config types:
/// String, Number, Int, Boolean, Object, and List variants of each.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ConfigType {
    String,
    Number,
    Int,
    Boolean,
    Object,
    StringList,
    NumberList,
    IntList,
    BooleanList,
    ObjectList,
}

impl ConfigType {
    /// Parses a config type string like "string", "int", "List<Boolean>", etc.
    ///
    /// Returns `None` if the string doesn't match a known config type.
    pub fn parse(s: &str) -> Option<Self> {
        // Normalize: trim whitespace, lowercase for comparison
        let s = s.trim();

        // Check list types first: "List<T>" or "list<T>"
        if let Some(inner) = s.strip_prefix("List<").or_else(|| s.strip_prefix("list<")) {
            let inner = inner.strip_suffix('>')?.trim();
            return match inner.to_lowercase().as_str() {
                "string" => Some(ConfigType::StringList),
                "number" => Some(ConfigType::NumberList),
                "int" | "integer" => Some(ConfigType::IntList),
                "boolean" | "bool" => Some(ConfigType::BooleanList),
                "object" => Some(ConfigType::ObjectList),
                _ => None,
            };
        }

        match s.to_lowercase().as_str() {
            "string" => Some(ConfigType::String),
            "number" => Some(ConfigType::Number),
            "int" | "integer" => Some(ConfigType::Int),
            "boolean" | "bool" => Some(ConfigType::Boolean),
            "object" => Some(ConfigType::Object),
            _ => None,
        }
    }

    /// Returns true if this is a primitive (non-list) type.
    pub fn is_primitive(&self) -> bool {
        matches!(
            self,
            ConfigType::String
                | ConfigType::Number
                | ConfigType::Int
                | ConfigType::Boolean
                | ConfigType::Object
        )
    }

    /// Returns true if this is a list type.
    pub fn is_list(&self) -> bool {
        !self.is_primitive()
    }

    /// Returns the element type for list types, or `None` for primitives.
    pub fn element_type(&self) -> Option<ConfigType> {
        match self {
            ConfigType::StringList => Some(ConfigType::String),
            ConfigType::NumberList => Some(ConfigType::Number),
            ConfigType::IntList => Some(ConfigType::Int),
            ConfigType::BooleanList => Some(ConfigType::Boolean),
            ConfigType::ObjectList => Some(ConfigType::Object),
            _ => None,
        }
    }

    /// Returns the list version of a primitive type.
    pub fn as_list(&self) -> Option<ConfigType> {
        match self {
            ConfigType::String => Some(ConfigType::StringList),
            ConfigType::Number => Some(ConfigType::NumberList),
            ConfigType::Int => Some(ConfigType::IntList),
            ConfigType::Boolean => Some(ConfigType::BooleanList),
            ConfigType::Object => Some(ConfigType::ObjectList),
            _ => None,
        }
    }
}

impl fmt::Display for ConfigType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigType::String => write!(f, "String"),
            ConfigType::Number => write!(f, "Number"),
            ConfigType::Int => write!(f, "Int"),
            ConfigType::Boolean => write!(f, "Boolean"),
            ConfigType::Object => write!(f, "Object"),
            ConfigType::StringList => write!(f, "List<String>"),
            ConfigType::NumberList => write!(f, "List<Number>"),
            ConfigType::IntList => write!(f, "List<Int>"),
            ConfigType::BooleanList => write!(f, "List<Boolean>"),
            ConfigType::ObjectList => write!(f, "List<Object>"),
        }
    }
}

/// All known primitive config types.
pub const PRIMITIVES: &[ConfigType] = &[
    ConfigType::String,
    ConfigType::Number,
    ConfigType::Int,
    ConfigType::Boolean,
    ConfigType::Object,
];

/// All known config types (primitives + list variants).
pub const CONFIG_TYPES: &[ConfigType] = &[
    ConfigType::String,
    ConfigType::Number,
    ConfigType::Int,
    ConfigType::Boolean,
    ConfigType::Object,
    ConfigType::StringList,
    ConfigType::NumberList,
    ConfigType::IntList,
    ConfigType::BooleanList,
    ConfigType::ObjectList,
];

/// Attempts to infer the config type of a JSON/YAML value.
#[derive(Debug)]
pub enum TypeInferenceError {
    /// A list contains elements of different types.
    HeterogeneousList,
    /// The value is an empty list (type cannot be inferred).
    EmptyList,
    /// The value type is not a known config type.
    UnexpectedType(String),
}

impl fmt::Display for TypeInferenceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TypeInferenceError::HeterogeneousList => {
                write!(f, "list contains elements of different types")
            }
            TypeInferenceError::EmptyList => {
                write!(f, "cannot infer type of empty list")
            }
            TypeInferenceError::UnexpectedType(t) => {
                write!(f, "unexpected type: {}", t)
            }
        }
    }
}

impl std::error::Error for TypeInferenceError {}

/// Infers the config type from a `serde_json::Value`.
pub fn infer_type(value: &serde_json::Value) -> Result<ConfigType, TypeInferenceError> {
    match value {
        serde_json::Value::Null => Ok(ConfigType::String), // null treated as string
        serde_json::Value::Bool(_) => Ok(ConfigType::Boolean),
        serde_json::Value::Number(n) => {
            if n.is_f64() && n.as_f64().is_some_and(|f| f.fract() != 0.0) {
                Ok(ConfigType::Number)
            } else {
                Ok(ConfigType::Int)
            }
        }
        serde_json::Value::String(_) => Ok(ConfigType::String),
        serde_json::Value::Object(_) => Ok(ConfigType::Object),
        serde_json::Value::Array(arr) => {
            if arr.is_empty() {
                return Err(TypeInferenceError::EmptyList);
            }
            let first_type = infer_type(&arr[0])?;
            for item in &arr[1..] {
                let item_type = infer_type(item)?;
                if item_type != first_type {
                    return Err(TypeInferenceError::HeterogeneousList);
                }
            }
            first_type
                .as_list()
                .ok_or_else(|| TypeInferenceError::UnexpectedType(first_type.to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_primitives() {
        assert_eq!(ConfigType::parse("string"), Some(ConfigType::String));
        assert_eq!(ConfigType::parse("String"), Some(ConfigType::String));
        assert_eq!(ConfigType::parse("number"), Some(ConfigType::Number));
        assert_eq!(ConfigType::parse("int"), Some(ConfigType::Int));
        assert_eq!(ConfigType::parse("integer"), Some(ConfigType::Int));
        assert_eq!(ConfigType::parse("boolean"), Some(ConfigType::Boolean));
        assert_eq!(ConfigType::parse("bool"), Some(ConfigType::Boolean));
        assert_eq!(ConfigType::parse("object"), Some(ConfigType::Object));
    }

    #[test]
    fn test_parse_lists() {
        assert_eq!(
            ConfigType::parse("List<String>"),
            Some(ConfigType::StringList)
        );
        assert_eq!(
            ConfigType::parse("List<Number>"),
            Some(ConfigType::NumberList)
        );
        assert_eq!(ConfigType::parse("List<Int>"), Some(ConfigType::IntList));
        assert_eq!(
            ConfigType::parse("List<Boolean>"),
            Some(ConfigType::BooleanList)
        );
        assert_eq!(
            ConfigType::parse("List<Object>"),
            Some(ConfigType::ObjectList)
        );
    }

    #[test]
    fn test_parse_invalid() {
        assert_eq!(ConfigType::parse("unknown"), None);
        assert_eq!(ConfigType::parse("List<unknown>"), None);
        assert_eq!(ConfigType::parse(""), None);
    }

    #[test]
    fn test_display() {
        assert_eq!(ConfigType::String.to_string(), "String");
        assert_eq!(ConfigType::IntList.to_string(), "List<Int>");
        assert_eq!(ConfigType::BooleanList.to_string(), "List<Boolean>");
    }

    #[test]
    fn test_is_primitive() {
        assert!(ConfigType::String.is_primitive());
        assert!(!ConfigType::StringList.is_primitive());
    }

    #[test]
    fn test_element_type() {
        assert_eq!(
            ConfigType::StringList.element_type(),
            Some(ConfigType::String)
        );
        assert_eq!(ConfigType::String.element_type(), None);
    }

    #[test]
    fn test_as_list() {
        assert_eq!(ConfigType::String.as_list(), Some(ConfigType::StringList));
        assert_eq!(ConfigType::StringList.as_list(), None);
    }

    #[test]
    fn test_infer_type_string() {
        let v = serde_json::json!("hello");
        assert_eq!(infer_type(&v).unwrap(), ConfigType::String);
    }

    #[test]
    fn test_infer_type_bool() {
        let v = serde_json::json!(true);
        assert_eq!(infer_type(&v).unwrap(), ConfigType::Boolean);
    }

    #[test]
    fn test_infer_type_int() {
        let v = serde_json::json!(42);
        assert_eq!(infer_type(&v).unwrap(), ConfigType::Int);
    }

    #[test]
    fn test_infer_type_float() {
        let v = serde_json::json!(2.75);
        assert_eq!(infer_type(&v).unwrap(), ConfigType::Number);
    }

    #[test]
    fn test_infer_type_object() {
        let v = serde_json::json!({"key": "value"});
        assert_eq!(infer_type(&v).unwrap(), ConfigType::Object);
    }

    #[test]
    fn test_infer_type_string_list() {
        let v = serde_json::json!(["a", "b", "c"]);
        assert_eq!(infer_type(&v).unwrap(), ConfigType::StringList);
    }

    #[test]
    fn test_infer_type_empty_list() {
        let v = serde_json::json!([]);
        assert!(matches!(infer_type(&v), Err(TypeInferenceError::EmptyList)));
    }

    #[test]
    fn test_infer_type_heterogeneous_list() {
        let v = serde_json::json!([1, "two"]);
        assert!(matches!(
            infer_type(&v),
            Err(TypeInferenceError::HeterogeneousList)
        ));
    }
}

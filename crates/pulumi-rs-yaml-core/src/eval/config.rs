use std::borrow::Cow;
use std::collections::HashMap;

use crate::config_types::ConfigType;
use crate::diag::Diagnostics;
use crate::eval::value::Value;

/// Raw config values from the engine, keyed by fully-qualified name
/// (e.g. "project:key").
pub type RawConfig = HashMap<String, String>;

/// Raw config values with secret annotations.
pub type SecretKeys = Vec<String>;

/// Resolved config entry after type checking and default application.
#[derive(Debug, Clone)]
pub struct ResolvedConfig<'src> {
    pub value: Value<'src>,
    pub is_secret: bool,
}

/// Resolves a single config entry from the raw config map.
///
/// This function:
/// 1. Looks up the config value by key (with project prefix)
/// 2. Applies the declared type to parse the value
/// 3. Falls back to the default value if the key is missing
/// 4. Wraps the value in Secret if marked as secret
#[allow(clippy::too_many_arguments)]
pub fn resolve_config_entry<'src>(
    key: &str,
    project_name: &str,
    declared_type: Option<ConfigType>,
    default_value: Option<Value<'src>>,
    is_secret_in_config: bool,
    is_secret_in_schema: bool,
    raw_config: &RawConfig,
    diags: &mut Diagnostics,
) -> Option<ResolvedConfig<'src>> {
    let full_key = format!("{}:{}", project_name, key);

    // Look up the raw value
    let raw_value = raw_config.get(&full_key).or_else(|| raw_config.get(key));

    let effective_type = declared_type.clone().unwrap_or_else(|| {
        if let Some(ref default) = default_value {
            infer_type_from_value(default)
        } else {
            ConfigType::String
        }
    });

    // Validate default type matches declared type
    if let (Some(decl_type), Some(ref default)) = (declared_type, &default_value) {
        let default_type = infer_type_from_value(default);
        // Allow Int default for Number type
        if decl_type == ConfigType::Int && default_type == ConfigType::Number {
            if let Value::Number(n) = default {
                if n.fract() == 0.0 {
                    // OK: integer value for Int type
                } else {
                    diags.error(
                        None,
                        format!(
                            "type mismatch: default value of type {} but type {} was specified",
                            default_type, decl_type
                        ),
                        "",
                    );
                    return None;
                }
            }
        } else if default_type != decl_type
            && !(decl_type == ConfigType::Number && default_type == ConfigType::Int)
        {
            diags.error(
                None,
                format!(
                    "type mismatch: default value of type {} but type {} was specified",
                    default_type, decl_type
                ),
                "",
            );
            return None;
        }
    }

    let value = if let Some(raw) = raw_value {
        parse_config_value(raw, effective_type, diags)?
    } else if let Some(default) = default_value {
        default
    } else {
        diags.error(
            None,
            format!("missing required configuration variable '{}'", key),
            "",
        );
        return None;
    };

    let is_secret = is_secret_in_config || is_secret_in_schema;
    let final_value = if is_secret {
        Value::Secret(Box::new(value))
    } else {
        value
    };

    Some(ResolvedConfig {
        value: final_value,
        is_secret,
    })
}

/// Validates that a resolved config value matches its declared type.
///
/// Emits a warning (not error) on mismatch to avoid blocking deployment for
/// loosely-typed configs.
pub fn validate_config_type(value: &Value<'_>, declared: &str, key: &str, diags: &mut Diagnostics) {
    let ok = match declared {
        "string" => matches!(value, Value::String(_)),
        "int" | "integer" => matches!(value, Value::Number(n) if n.fract() == 0.0),
        "number" => matches!(value, Value::Number(_)),
        "boolean" => matches!(value, Value::Bool(_)),
        _ => true, // unknown types pass through
    };
    if !ok {
        diags.warning(
            None,
            format!(
                "config '{}': expected {}, got {}",
                key,
                declared,
                value.type_name()
            ),
            "",
        );
    }
}

/// Parses a raw string config value into a typed Value.
fn parse_config_value<'src>(
    raw: &str,
    expected_type: ConfigType,
    diags: &mut Diagnostics,
) -> Option<Value<'src>> {
    match expected_type {
        ConfigType::String => Some(Value::String(Cow::Owned(raw.to_string()))),
        ConfigType::Number => match raw.parse::<f64>() {
            Ok(n) => Some(Value::Number(n)),
            Err(_) => {
                diags.error(
                    None,
                    format!("config value '{}' is not a valid number", raw),
                    "",
                );
                None
            }
        },
        ConfigType::Int => match raw.parse::<i64>() {
            Ok(n) => Some(Value::Number(n as f64)),
            Err(_) => {
                diags.error(
                    None,
                    format!("config value '{}' is not a valid integer", raw),
                    "",
                );
                None
            }
        },
        ConfigType::Boolean => match raw {
            "true" => Some(Value::Bool(true)),
            "false" => Some(Value::Bool(false)),
            _ => {
                diags.error(
                    None,
                    format!("config value '{}' is not a valid boolean", raw),
                    "",
                );
                None
            }
        },
        ConfigType::Object | ConfigType::ObjectList => {
            // Objects and lists are JSON-encoded in config
            parse_json_config(raw, diags)
        }
        ConfigType::StringList
        | ConfigType::NumberList
        | ConfigType::IntList
        | ConfigType::BooleanList => {
            // Lists are JSON-encoded in config
            parse_json_config(raw, diags)
        }
    }
}

/// Parses a JSON-encoded config value.
fn parse_json_config<'src>(raw: &str, diags: &mut Diagnostics) -> Option<Value<'src>> {
    let json_value: serde_json::Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(e) => {
            diags.error(None, format!("config value is not valid JSON: {}", e), "");
            return None;
        }
    };
    Some(Value::from_json(&json_value))
}

/// Infers the ConfigType from a Value.
fn infer_type_from_value(value: &Value<'_>) -> ConfigType {
    match value {
        Value::String(_) => ConfigType::String,
        Value::Number(n) => {
            if n.fract() == 0.0 {
                ConfigType::Int
            } else {
                ConfigType::Number
            }
        }
        Value::Bool(_) => ConfigType::Boolean,
        Value::List(_) => ConfigType::StringList, // approximate
        Value::Object(_) => ConfigType::Object,
        _ => ConfigType::String,
    }
}

/// Strips the project namespace from a config key.
///
/// e.g. "myproject:myKey" -> "myKey" if project is "myproject"
pub fn strip_config_namespace<'a>(project: &str, key: &'a str) -> &'a str {
    let prefix = format!("{}:", project);
    key.strip_prefix(&prefix).unwrap_or(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_config_namespace() {
        assert_eq!(strip_config_namespace("myproject", "myproject:key"), "key");
        assert_eq!(
            strip_config_namespace("myproject", "other:key"),
            "other:key"
        );
        assert_eq!(strip_config_namespace("myproject", "key"), "key");
    }

    #[test]
    fn test_parse_config_string() {
        let mut diags = Diagnostics::new();
        let val = parse_config_value("hello", ConfigType::String, &mut diags);
        assert!(!diags.has_errors());
        assert_eq!(val.unwrap().as_str(), Some("hello"));
    }

    #[test]
    fn test_parse_config_number() {
        let mut diags = Diagnostics::new();
        let val = parse_config_value("2.75", ConfigType::Number, &mut diags);
        assert!(!diags.has_errors());
        assert_eq!(val.unwrap().as_number(), Some(2.75));
    }

    #[test]
    fn test_parse_config_int() {
        let mut diags = Diagnostics::new();
        let val = parse_config_value("42", ConfigType::Int, &mut diags);
        assert!(!diags.has_errors());
        assert_eq!(val.unwrap().as_number(), Some(42.0));
    }

    #[test]
    fn test_parse_config_bool() {
        let mut diags = Diagnostics::new();
        let val = parse_config_value("true", ConfigType::Boolean, &mut diags);
        assert!(!diags.has_errors());
        assert_eq!(val.unwrap().as_bool(), Some(true));

        let val = parse_config_value("false", ConfigType::Boolean, &mut diags);
        assert!(!diags.has_errors());
        assert_eq!(val.unwrap().as_bool(), Some(false));
    }

    #[test]
    fn test_parse_config_invalid_number() {
        let mut diags = Diagnostics::new();
        let val = parse_config_value("not-a-number", ConfigType::Number, &mut diags);
        assert!(diags.has_errors());
        assert!(val.is_none());
    }

    #[test]
    fn test_parse_config_invalid_bool() {
        let mut diags = Diagnostics::new();
        let val = parse_config_value("yes", ConfigType::Boolean, &mut diags);
        assert!(diags.has_errors());
        assert!(val.is_none());
    }

    #[test]
    fn test_parse_config_json_object() {
        let mut diags = Diagnostics::new();
        let val = parse_config_value(r#"{"key": "value"}"#, ConfigType::Object, &mut diags);
        assert!(!diags.has_errors());
        match val.unwrap() {
            Value::Object(entries) => {
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].0.as_ref(), "key");
            }
            other => panic!("expected object, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_config_json_list() {
        let mut diags = Diagnostics::new();
        let val = parse_config_value(r#"["a", "b", "c"]"#, ConfigType::StringList, &mut diags);
        assert!(!diags.has_errors());
        match val.unwrap() {
            Value::List(items) => assert_eq!(items.len(), 3),
            other => panic!("expected list, got {:?}", other),
        }
    }

    #[test]
    fn test_resolve_config_with_default() {
        let mut diags = Diagnostics::new();
        let raw = HashMap::new();
        let result = resolve_config_entry(
            "myKey",
            "proj",
            Some(ConfigType::String),
            Some(Value::String(Cow::Owned("default-val".to_string()))),
            false,
            false,
            &raw,
            &mut diags,
        );
        assert!(!diags.has_errors());
        let resolved = result.unwrap();
        assert_eq!(resolved.value.as_str(), Some("default-val"));
        assert!(!resolved.is_secret);
    }

    #[test]
    fn test_resolve_config_from_raw() {
        let mut diags = Diagnostics::new();
        let mut raw = HashMap::new();
        raw.insert("proj:myKey".to_string(), "from-config".to_string());
        let result = resolve_config_entry(
            "myKey",
            "proj",
            Some(ConfigType::String),
            None,
            false,
            false,
            &raw,
            &mut diags,
        );
        assert!(!diags.has_errors());
        assert_eq!(result.unwrap().value.as_str(), Some("from-config"));
    }

    #[test]
    fn test_resolve_config_secret() {
        let mut diags = Diagnostics::new();
        let mut raw = HashMap::new();
        raw.insert("proj:secret".to_string(), "pw123".to_string());
        let result = resolve_config_entry(
            "secret",
            "proj",
            Some(ConfigType::String),
            None,
            true,
            false,
            &raw,
            &mut diags,
        );
        assert!(!diags.has_errors());
        let resolved = result.unwrap();
        assert!(resolved.is_secret);
        match &resolved.value {
            Value::Secret(inner) => assert_eq!(inner.as_str(), Some("pw123")),
            _ => panic!("expected secret"),
        }
    }

    #[test]
    fn test_resolve_config_missing_required() {
        let mut diags = Diagnostics::new();
        let raw = HashMap::new();
        let result = resolve_config_entry(
            "missing",
            "proj",
            Some(ConfigType::String),
            None,
            false,
            false,
            &raw,
            &mut diags,
        );
        assert!(diags.has_errors());
        assert!(result.is_none());
    }

    #[test]
    fn test_resolve_config_type_mismatch() {
        let mut diags = Diagnostics::new();
        let raw = HashMap::new();
        let result = resolve_config_entry(
            "key",
            "proj",
            Some(ConfigType::Boolean),
            Some(Value::String(Cow::Owned("not-a-bool".to_string()))),
            false,
            false,
            &raw,
            &mut diags,
        );
        assert!(diags.has_errors());
        assert!(result.is_none());
    }

    #[test]
    fn test_json_to_value_nested() {
        let json: serde_json::Value = serde_json::json!({
            "name": "test",
            "count": 42,
            "enabled": true,
            "tags": ["a", "b"],
            "nested": { "key": "val" }
        });
        let val = Value::from_json(&json);
        match &val {
            Value::Object(entries) => assert_eq!(entries.len(), 5),
            _ => panic!("expected object"),
        }
    }

    #[test]
    fn test_validate_config_type_match() {
        let mut diags = Diagnostics::new();
        validate_config_type(
            &Value::String(Cow::Borrowed("hi")),
            "string",
            "key",
            &mut diags,
        );
        assert!(!diags.has_errors());
        validate_config_type(&Value::Number(42.0), "int", "key", &mut diags);
        assert!(!diags.has_errors());
        validate_config_type(&Value::Number(2.5), "number", "key", &mut diags);
        assert!(!diags.has_errors());
        validate_config_type(&Value::Bool(true), "boolean", "key", &mut diags);
        assert!(!diags.has_errors());
    }

    #[test]
    fn test_validate_config_type_mismatch_warns() {
        let mut diags = Diagnostics::new();
        validate_config_type(
            &Value::String(Cow::Borrowed("hi")),
            "number",
            "myKey",
            &mut diags,
        );
        // Should produce a warning, not an error
        assert!(!diags.has_errors());
        assert!(diags.has_warnings());
    }

    #[test]
    fn test_validate_config_type_unknown_type_passes() {
        let mut diags = Diagnostics::new();
        validate_config_type(
            &Value::String(Cow::Borrowed("hi")),
            "customType",
            "key",
            &mut diags,
        );
        assert!(!diags.has_errors());
        assert!(!diags.has_warnings());
    }

    #[test]
    fn test_infer_type_from_value_variants() {
        assert_eq!(
            infer_type_from_value(&Value::String(Cow::Borrowed("hi"))),
            ConfigType::String
        );
        assert_eq!(infer_type_from_value(&Value::Number(42.0)), ConfigType::Int);
        assert_eq!(
            infer_type_from_value(&Value::Number(2.75)),
            ConfigType::Number
        );
        assert_eq!(
            infer_type_from_value(&Value::Bool(true)),
            ConfigType::Boolean
        );
    }
}

use crate::eval::value::Value;
use std::borrow::Cow;
use std::collections::BTreeMap;

/// Special protobuf string markers used by the Pulumi SDK to encode
/// unknowns and secrets within google.protobuf.Struct values.
pub const UNKNOWN_VALUE: &str = "04da6b54-80e4-46f7-96ec-b56ff0331ba9";
pub const SECRET_SIG: &str = "1b47061264138c4ac30d75fd1eb44270";
pub const RESOURCE_SIG: &str = "5cf8f73096256a8f31e491e813e4eb8e";
pub const OUTPUT_SIG: &str = "d0e6a833031e9bbcd3f4e8bde6ca49a4";
pub const ASSET_SIG: &str = "c44067f5952c0a294b673a41bacd8c17";
pub const ARCHIVE_SIG: &str = "0def7320c3a5731c473e5ecbe6d01bc7";

/// Converts a `Value` into a `prost_types::Value` for gRPC transmission.
pub fn value_to_protobuf(val: &Value<'_>) -> prost_types::Value {
    use prost_types::value::Kind;

    let kind = match val {
        Value::Null => Kind::NullValue(0),
        Value::Bool(b) => Kind::BoolValue(*b),
        Value::Number(n) => Kind::NumberValue(*n),
        Value::String(s) => Kind::StringValue(s.to_string()),
        Value::List(items) => {
            let values: Vec<prost_types::Value> = items.iter().map(value_to_protobuf).collect();
            Kind::ListValue(prost_types::ListValue { values })
        }
        Value::Object(entries) => {
            let fields: BTreeMap<String, prost_types::Value> = entries
                .iter()
                .map(|(k, v)| (k.to_string(), value_to_protobuf(v)))
                .collect();
            Kind::StructValue(prost_types::Struct { fields })
        }
        Value::Secret(inner) => {
            // Encode as a special struct with the secret signature
            let mut fields = BTreeMap::new();
            fields.insert(
                "4dabf18193072939515e22adb298388d".to_string(),
                prost_types::Value {
                    kind: Some(Kind::StringValue(SECRET_SIG.to_string())),
                },
            );
            fields.insert("value".to_string(), value_to_protobuf(inner));
            Kind::StructValue(prost_types::Struct { fields })
        }
        Value::Unknown => Kind::StringValue(UNKNOWN_VALUE.to_string()),
        Value::Resource(r) => Kind::StringValue(format!("resource({})", r.0)),
        Value::Asset(asset) => {
            let mut fields = BTreeMap::new();
            fields.insert(
                "4dabf18193072939515e22adb298388d".to_string(),
                prost_types::Value {
                    kind: Some(Kind::StringValue(ASSET_SIG.to_string())),
                },
            );
            match asset {
                crate::eval::value::Asset::String(s) => {
                    fields.insert(
                        "text".to_string(),
                        prost_types::Value {
                            kind: Some(Kind::StringValue(s.to_string())),
                        },
                    );
                }
                crate::eval::value::Asset::File(s) => {
                    fields.insert(
                        "path".to_string(),
                        prost_types::Value {
                            kind: Some(Kind::StringValue(s.to_string())),
                        },
                    );
                }
                crate::eval::value::Asset::Remote(s) => {
                    fields.insert(
                        "uri".to_string(),
                        prost_types::Value {
                            kind: Some(Kind::StringValue(s.to_string())),
                        },
                    );
                }
            }
            Kind::StructValue(prost_types::Struct { fields })
        }
        Value::Archive(archive) => {
            let mut fields = BTreeMap::new();
            fields.insert(
                "4dabf18193072939515e22adb298388d".to_string(),
                prost_types::Value {
                    kind: Some(Kind::StringValue(ARCHIVE_SIG.to_string())),
                },
            );
            match archive {
                crate::eval::value::Archive::File(s) => {
                    fields.insert(
                        "path".to_string(),
                        prost_types::Value {
                            kind: Some(Kind::StringValue(s.to_string())),
                        },
                    );
                }
                crate::eval::value::Archive::Remote(s) => {
                    fields.insert(
                        "uri".to_string(),
                        prost_types::Value {
                            kind: Some(Kind::StringValue(s.to_string())),
                        },
                    );
                }
                crate::eval::value::Archive::Assets(entries) => {
                    let assets: BTreeMap<String, prost_types::Value> = entries
                        .iter()
                        .map(|(k, v)| (k.to_string(), value_to_protobuf(v)))
                        .collect();
                    fields.insert(
                        "assets".to_string(),
                        prost_types::Value {
                            kind: Some(Kind::StructValue(prost_types::Struct { fields: assets })),
                        },
                    );
                }
            }
            Kind::StructValue(prost_types::Struct { fields })
        }
    };

    prost_types::Value { kind: Some(kind) }
}

/// Converts a `prost_types::Value` back into a `Value<'static>`.
pub fn protobuf_to_value(pv: &prost_types::Value) -> Value<'static> {
    use prost_types::value::Kind;

    let kind = match &pv.kind {
        Some(k) => k,
        None => return Value::Null,
    };

    match kind {
        Kind::NullValue(_) => Value::Null,
        Kind::BoolValue(b) => Value::Bool(*b),
        Kind::NumberValue(n) => Value::Number(*n),
        Kind::StringValue(s) => {
            if s == UNKNOWN_VALUE {
                Value::Unknown
            } else {
                Value::String(Cow::Owned(s.clone()))
            }
        }
        Kind::ListValue(list) => {
            let values: Vec<Value<'static>> = list.values.iter().map(protobuf_to_value).collect();
            Value::List(values)
        }
        Kind::StructValue(obj) => {
            // Check for special Pulumi signatures
            if let Some(sig_val) = obj.fields.get("4dabf18193072939515e22adb298388d") {
                if let Some(Kind::StringValue(sig)) = &sig_val.kind {
                    match sig.as_str() {
                        SECRET_SIG => {
                            if let Some(inner) = obj.fields.get("value") {
                                return Value::Secret(Box::new(protobuf_to_value(inner)));
                            }
                            return Value::Secret(Box::new(Value::Null));
                        }
                        ASSET_SIG => {
                            if let Some(text_val) = obj.fields.get("text") {
                                if let Some(Kind::StringValue(s)) = &text_val.kind {
                                    return Value::Asset(crate::eval::value::Asset::String(
                                        Cow::Owned(s.clone()),
                                    ));
                                }
                            }
                            if let Some(path_val) = obj.fields.get("path") {
                                if let Some(Kind::StringValue(s)) = &path_val.kind {
                                    return Value::Asset(crate::eval::value::Asset::File(
                                        Cow::Owned(s.clone()),
                                    ));
                                }
                            }
                            if let Some(uri_val) = obj.fields.get("uri") {
                                if let Some(Kind::StringValue(s)) = &uri_val.kind {
                                    return Value::Asset(crate::eval::value::Asset::Remote(
                                        Cow::Owned(s.clone()),
                                    ));
                                }
                            }
                        }
                        OUTPUT_SIG => {
                            // Unwrap output: extract known value or return Unknown
                            let is_secret = obj
                                .fields
                                .get("secret")
                                .and_then(|v| match &v.kind {
                                    Some(Kind::BoolValue(b)) => Some(*b),
                                    _ => None,
                                })
                                .unwrap_or(false);
                            let inner = obj
                                .fields
                                .get("value")
                                .map(protobuf_to_value)
                                .unwrap_or(Value::Unknown);
                            if is_secret {
                                return Value::Secret(Box::new(inner));
                            }
                            return inner;
                        }
                        RESOURCE_SIG => {
                            // Build object with urn + id fields
                            let mut entries = Vec::new();
                            if let Some(urn_val) = obj.fields.get("urn") {
                                entries
                                    .push((Cow::Owned("urn".into()), protobuf_to_value(urn_val)));
                            }
                            if let Some(id_val) = obj.fields.get("id") {
                                entries.push((Cow::Owned("id".into()), protobuf_to_value(id_val)));
                            }
                            return if entries.is_empty() {
                                Value::Unknown
                            } else {
                                Value::Object(entries)
                            };
                        }
                        ARCHIVE_SIG => {
                            if let Some(path_val) = obj.fields.get("path") {
                                if let Some(Kind::StringValue(s)) = &path_val.kind {
                                    return Value::Archive(crate::eval::value::Archive::File(
                                        Cow::Owned(s.clone()),
                                    ));
                                }
                            }
                            if let Some(uri_val) = obj.fields.get("uri") {
                                if let Some(Kind::StringValue(s)) = &uri_val.kind {
                                    return Value::Archive(crate::eval::value::Archive::Remote(
                                        Cow::Owned(s.clone()),
                                    ));
                                }
                            }
                            if let Some(assets_val) = obj.fields.get("assets") {
                                if let Some(Kind::StructValue(assets_obj)) = &assets_val.kind {
                                    let entries: Vec<_> = assets_obj
                                        .fields
                                        .iter()
                                        .map(|(k, v)| (Cow::Owned(k.clone()), protobuf_to_value(v)))
                                        .collect();
                                    return Value::Archive(crate::eval::value::Archive::Assets(
                                        entries,
                                    ));
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }

            // Regular object
            let entries: Vec<(Cow<'static, str>, Value<'static>)> = obj
                .fields
                .iter()
                .map(|(k, v)| (Cow::Owned(k.clone()), protobuf_to_value(v)))
                .collect();
            Value::Object(entries)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(val: Value<'static>) -> Value<'static> {
        let pb = value_to_protobuf(&val);
        protobuf_to_value(&pb)
    }

    #[test]
    fn test_null_round_trip() {
        assert_eq!(round_trip(Value::Null), Value::Null);
    }

    #[test]
    fn test_bool_round_trip() {
        assert_eq!(round_trip(Value::Bool(true)), Value::Bool(true));
        assert_eq!(round_trip(Value::Bool(false)), Value::Bool(false));
    }

    #[test]
    fn test_number_round_trip() {
        assert_eq!(round_trip(Value::Number(42.0)), Value::Number(42.0));
        assert_eq!(round_trip(Value::Number(2.75)), Value::Number(2.75));
    }

    #[test]
    fn test_string_round_trip() {
        let v = Value::String(Cow::Owned("hello".to_string()));
        assert_eq!(
            round_trip(v),
            Value::String(Cow::Owned("hello".to_string()))
        );
    }

    #[test]
    fn test_list_round_trip() {
        let v = Value::List(vec![
            Value::Number(1.0),
            Value::String(Cow::Owned("two".to_string())),
            Value::Bool(true),
        ]);
        let result = round_trip(v);
        match &result {
            Value::List(items) => {
                assert_eq!(items.len(), 3);
                assert_eq!(items[0], Value::Number(1.0));
            }
            _ => panic!("expected list"),
        }
    }

    #[test]
    fn test_object_round_trip() {
        let v = Value::Object(vec![(
            Cow::Owned("key".to_string()),
            Value::String(Cow::Owned("value".to_string())),
        )]);
        let result = round_trip(v);
        match &result {
            Value::Object(entries) => {
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].0.as_ref(), "key");
            }
            _ => panic!("expected object"),
        }
    }

    #[test]
    fn test_secret_round_trip() {
        let v = Value::Secret(Box::new(Value::String(Cow::Owned("pw".to_string()))));
        let result = round_trip(v);
        match &result {
            Value::Secret(inner) => {
                assert_eq!(inner.as_str(), Some("pw"));
            }
            _ => panic!("expected secret"),
        }
    }

    #[test]
    fn test_unknown_round_trip() {
        assert_eq!(round_trip(Value::Unknown), Value::Unknown);
    }

    #[test]
    fn test_nested_object_round_trip() {
        let v = Value::Object(vec![(
            Cow::Owned("outer".to_string()),
            Value::Object(vec![(Cow::Owned("inner".to_string()), Value::Number(99.0))]),
        )]);
        let result = round_trip(v);
        match &result {
            Value::Object(entries) => match &entries[0].1 {
                Value::Object(inner) => {
                    assert_eq!(inner[0].1, Value::Number(99.0));
                }
                _ => panic!("expected inner object"),
            },
            _ => panic!("expected outer object"),
        }
    }

    #[test]
    fn test_asset_string_round_trip() {
        let v = Value::Asset(crate::eval::value::Asset::String(Cow::Owned(
            "contents".to_string(),
        )));
        let result = round_trip(v);
        match &result {
            Value::Asset(crate::eval::value::Asset::String(s)) => {
                assert_eq!(s.as_ref(), "contents");
            }
            _ => panic!("expected string asset"),
        }
    }

    #[test]
    fn test_asset_file_round_trip() {
        let v = Value::Asset(crate::eval::value::Asset::File(Cow::Owned(
            "/path/to/file".to_string(),
        )));
        let result = round_trip(v);
        match &result {
            Value::Asset(crate::eval::value::Asset::File(s)) => {
                assert_eq!(s.as_ref(), "/path/to/file");
            }
            _ => panic!("expected file asset"),
        }
    }

    #[test]
    fn test_archive_file_round_trip() {
        let v = Value::Archive(crate::eval::value::Archive::File(Cow::Owned(
            "/path/to/archive".to_string(),
        )));
        let result = round_trip(v);
        match &result {
            Value::Archive(crate::eval::value::Archive::File(s)) => {
                assert_eq!(s.as_ref(), "/path/to/archive");
            }
            _ => panic!("expected file archive"),
        }
    }

    #[test]
    fn test_unknown_round_trip_is_stable() {
        // Verify Unknown survives multiple round trips
        let v = Value::Unknown;
        let pb1 = value_to_protobuf(&v);
        let v2 = protobuf_to_value(&pb1);
        assert_eq!(v2, Value::Unknown);
        let pb2 = value_to_protobuf(&v2);
        let v3 = protobuf_to_value(&pb2);
        assert_eq!(v3, Value::Unknown);
    }

    #[test]
    fn test_nested_secret_list_round_trip() {
        let v = Value::Secret(Box::new(Value::List(vec![
            Value::String(Cow::Owned("visible".to_string())),
            Value::Unknown,
        ])));
        let result = round_trip(v);
        match &result {
            Value::Secret(inner) => match inner.as_ref() {
                Value::List(items) => {
                    assert_eq!(items.len(), 2);
                    assert_eq!(items[0].as_str(), Some("visible"));
                    assert_eq!(items[1], Value::Unknown);
                }
                other => panic!("expected list inside secret, got {:?}", other),
            },
            other => panic!("expected secret, got {:?}", other),
        }
    }
}

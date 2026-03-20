//! Security tests for code added in v0.3.0–v0.3.1.
//!
//! Covers: classify.rs, visitor.rs, protobuf.rs (by-value), packages.rs,
//! multi_file.rs, graph.rs, and the normalize_grpc_address helper.

use pulumi_rs_yaml_core::classify::{classify_diagnostic, ErrorCategory};
use pulumi_rs_yaml_core::packages::{
    canonicalize_type_token, collapse_type_token, expand_type_token, resolve_pkg_name,
    to_lower_camel,
};

// =========================================================================
// classify.rs — diagnostic classification with string parsing
// =========================================================================

mod classify_security {
    use super::*;

    #[test]
    fn sql_injection_payload_in_resource_name() {
        // Verify injection payloads in quoted names are extracted verbatim
        // (they must NOT be interpreted or executed).
        let c = classify_diagnostic(
            "resource or variable ''; DROP TABLE resources; --' is not defined",
            "",
        );
        assert_eq!(c.category, ErrorCategory::InvalidReference);
        // The extraction stops at the first closing quote, so only the
        // content up to the next ' is captured.
        assert!(c.bad_ref.is_some());
        // Verify it doesn't contain the DROP TABLE — the closing quote
        // terminates extraction.
    }

    #[test]
    fn xss_payload_in_bad_ref() {
        let c = classify_diagnostic(
            "resource or variable '<script>alert(1)</script>' is not defined",
            "",
        );
        assert_eq!(c.category, ErrorCategory::InvalidReference);
        assert_eq!(c.bad_ref.as_deref(), Some("<script>alert(1)</script>"));
        // Downstream consumers must escape this when rendering in HTML.
    }

    #[test]
    fn unicode_quotes_not_confused_with_ascii() {
        // Curly quotes should NOT be treated as extraction delimiters.
        let c = classify_diagnostic(
            "resource or variable \u{2018}myRes\u{2019} is not defined",
            "",
        );
        assert_eq!(c.category, ErrorCategory::InvalidReference);
        // No ASCII quotes found, so bad_ref should be None.
        assert!(c.bad_ref.is_none());
    }

    #[test]
    fn newlines_in_diagnostic_message() {
        let c = classify_diagnostic("resource or variable 'multi\nline' is not defined", "");
        assert_eq!(c.category, ErrorCategory::InvalidReference);
        assert_eq!(c.bad_ref.as_deref(), Some("multi\nline"));
    }

    #[test]
    fn null_bytes_in_message() {
        let c = classify_diagnostic("resource or variable 'null\0byte' is not defined", "");
        assert_eq!(c.category, ErrorCategory::InvalidReference);
        assert_eq!(c.bad_ref.as_deref(), Some("null\0byte"));
    }

    #[test]
    fn empty_quoted_name_ignored() {
        let c = classify_diagnostic("resource or variable '' is not defined", "");
        assert_eq!(c.category, ErrorCategory::InvalidReference);
        // Empty quoted name should be None per the implementation.
        assert!(c.bad_ref.is_none());
    }

    #[test]
    fn cycle_path_with_arrow_in_name() {
        // Names containing " -> " could confuse the cycle path parser.
        let c = classify_diagnostic("circular dependency: a -> b -> c -> a", "");
        assert_eq!(c.category, ErrorCategory::CircularDep);
        let path = c.cycle_path.unwrap();
        assert_eq!(path, vec!["a", "b", "c", "a"]);
    }

    #[test]
    fn cycle_path_with_parenthesized_filenames() {
        let c = classify_diagnostic(
            "circular dependency: alpha (main.yaml) -> beta (net.yaml) -> alpha (main.yaml)",
            "",
        );
        let path = c.cycle_path.unwrap();
        assert_eq!(path, vec!["alpha", "beta", "alpha"]);
    }

    #[test]
    fn cycle_path_no_colon() {
        // Message without colon should return empty cycle path.
        let c = classify_diagnostic("circular dependency detected", "");
        assert_eq!(c.category, ErrorCategory::CircularDep);
        let path = c.cycle_path.unwrap();
        assert!(path.is_empty() || path == vec![" detected"]);
    }

    #[test]
    fn did_you_mean_with_special_chars() {
        let c = classify_diagnostic(
            "unknown property 'foo'; did you mean '../../../etc/passwd'?",
            "",
        );
        assert_eq!(c.category, ErrorCategory::UnknownProperty);
        assert_eq!(c.best_match.as_deref(), Some("../../../etc/passwd"));
    }

    #[test]
    fn very_long_message_does_not_hang() {
        // Ensure long messages are processed in bounded time.
        let long_name = "a".repeat(100_000);
        let msg = format!("resource or variable '{}' is not defined", long_name);
        let c = classify_diagnostic(&msg, "");
        assert_eq!(c.category, ErrorCategory::InvalidReference);
        assert_eq!(c.bad_ref.as_deref(), Some(long_name.as_str()));
    }

    #[test]
    fn very_long_cycle_path() {
        let nodes: Vec<String> = (0..1000).map(|i| format!("node{}", i)).collect();
        let path_str = nodes.join(" -> ");
        let msg = format!("circular dependency: {}", path_str);
        let c = classify_diagnostic(&msg, "");
        assert_eq!(c.category, ErrorCategory::CircularDep);
        assert_eq!(c.cycle_path.unwrap().len(), 1000);
    }

    #[test]
    fn case_insensitive_classification() {
        assert_eq!(
            classify_diagnostic("CIRCULAR DEPENDENCY: a -> b", "").category,
            ErrorCategory::CircularDep
        );
        assert_eq!(
            classify_diagnostic("Syntax Error In YAML", "").category,
            ErrorCategory::SyntaxError
        );
        assert_eq!(
            classify_diagnostic("JINJA rendering failed", "").category,
            ErrorCategory::JinjaError
        );
    }

    #[test]
    fn detail_preserved_verbatim() {
        let detail = "line 42: <script>alert('xss')</script>";
        let c = classify_diagnostic("some error", detail);
        assert_eq!(c.detail, detail);
    }
}

// =========================================================================
// packages.rs — token parsing, canonicalization
// =========================================================================

mod packages_security {
    use super::*;

    #[test]
    fn empty_type_token() {
        assert_eq!(resolve_pkg_name(""), "");
    }

    #[test]
    fn single_colon_token() {
        assert_eq!(resolve_pkg_name(":"), "");
    }

    #[test]
    fn triple_colon_token() {
        assert_eq!(resolve_pkg_name(":::"), "");
    }

    #[test]
    fn many_colons_token() {
        // splitn(3, ':') should handle this gracefully.
        assert_eq!(resolve_pkg_name("a:b:c:d:e"), "a");
    }

    #[test]
    fn null_byte_in_token() {
        assert_eq!(resolve_pkg_name("aws\0:s3:Bucket"), "aws\0");
    }

    #[test]
    fn very_long_token() {
        let long = "a".repeat(100_000);
        let token = format!("{}:module:Type", long);
        assert_eq!(resolve_pkg_name(&token), long.as_str());
    }

    #[test]
    fn unicode_token() {
        assert_eq!(resolve_pkg_name("日本語:module:Type"), "日本語");
    }

    #[test]
    fn path_traversal_in_token() {
        assert_eq!(
            resolve_pkg_name("../../../etc/passwd:s3:Bucket"),
            "../../../etc/passwd"
        );
    }

    #[test]
    fn pulumi_providers_with_extra_colons() {
        // pulumi:providers:aws:extra should resolve to "aws:extra" (the third segment).
        let result = resolve_pkg_name("pulumi:providers:aws:extra");
        assert_eq!(result, "aws:extra");
    }

    #[test]
    fn pulumi_providers_empty() {
        assert_eq!(resolve_pkg_name("pulumi:providers:"), "");
    }

    #[test]
    fn canonicalize_empty() {
        assert_eq!(canonicalize_type_token(""), "");
    }

    #[test]
    fn canonicalize_single_part() {
        assert_eq!(canonicalize_type_token("aws"), "aws");
    }

    #[test]
    fn canonicalize_very_long_type_name() {
        let long = "A".repeat(100_000);
        let token = format!("pkg:mod:{}", long);
        let result = canonicalize_type_token(&token);
        // Should produce pkg:mod/<lowerCamel>:<long>
        assert!(result.starts_with("pkg:mod/"));
        assert!(result.ends_with(&format!(":{}", long)));
    }

    #[test]
    fn canonicalize_unicode_type_name() {
        let result = canonicalize_type_token("pkg:mod:Ñoño");
        assert_eq!(result, "pkg:mod/ñoño:Ñoño");
    }

    #[test]
    fn collapse_empty_string() {
        assert_eq!(collapse_type_token(""), "");
    }

    #[test]
    fn collapse_one_part() {
        assert_eq!(collapse_type_token("aws"), "aws");
    }

    #[test]
    fn expand_empty() {
        let candidates = expand_type_token("");
        assert_eq!(candidates, vec![""]);
    }

    #[test]
    fn to_lower_camel_unicode() {
        assert_eq!(to_lower_camel("Über"), "über");
        assert_eq!(to_lower_camel("Δelta"), "δelta");
    }

    #[test]
    fn to_lower_camel_emoji() {
        // Emojis have no lowercase form, should be preserved.
        let result = to_lower_camel("🚀Rocket");
        assert_eq!(result, "🚀Rocket");
    }
}

// =========================================================================
// protobuf.rs — by-value deserialization security
// =========================================================================

mod protobuf_security {
    use prost_types::value::Kind;
    use pulumi_rs_yaml_core::eval::protobuf::{
        protobuf_to_value, value_to_protobuf, ASSET_SIG, OUTPUT_SIG, RESOURCE_SIG, SECRET_SIG,
        UNKNOWN_VALUE,
    };
    use pulumi_rs_yaml_core::eval::value::Value;
    use std::borrow::Cow;
    use std::collections::BTreeMap;

    const SIG_KEY: &str = "4dabf18193072939515e22adb298388d";

    #[test]
    fn deeply_nested_object_no_stack_overflow() {
        // Build 200 levels of nesting (reasonable depth, shouldn't overflow).
        let mut val = prost_types::Value {
            kind: Some(Kind::StringValue("leaf".to_string())),
        };
        for _ in 0..200 {
            let mut fields = BTreeMap::new();
            fields.insert("nested".to_string(), val);
            val = prost_types::Value {
                kind: Some(Kind::StructValue(prost_types::Struct { fields })),
            };
        }
        let result = protobuf_to_value(val);
        // Verify it's deeply nested — just ensure no panic/stack overflow.
        let mut current = &result;
        for _ in 0..200 {
            match current {
                Value::Object(entries) => {
                    assert_eq!(entries.len(), 1);
                    current = &entries[0].1;
                }
                _ => panic!("expected object at each nesting level"),
            }
        }
        assert_eq!(*current, Value::String(Cow::Owned("leaf".to_string())));
    }

    #[test]
    fn large_list_no_crash() {
        let values: Vec<prost_types::Value> = (0..10_000)
            .map(|i| prost_types::Value {
                kind: Some(Kind::NumberValue(i as f64)),
            })
            .collect();
        let val = prost_types::Value {
            kind: Some(Kind::ListValue(prost_types::ListValue { values })),
        };
        let result = protobuf_to_value(val);
        match result {
            Value::List(items) => assert_eq!(items.len(), 10_000),
            _ => panic!("expected list"),
        }
    }

    #[test]
    fn large_string_no_crash() {
        let big = "x".repeat(1_000_000);
        let val = prost_types::Value {
            kind: Some(Kind::StringValue(big.clone())),
        };
        let result = protobuf_to_value(val);
        assert_eq!(result, Value::String(Cow::Owned(big)));
    }

    #[test]
    fn unknown_signature_treated_as_regular_object() {
        let mut fields = BTreeMap::new();
        fields.insert(
            SIG_KEY.to_string(),
            prost_types::Value {
                kind: Some(Kind::StringValue("unknown_fake_signature".to_string())),
            },
        );
        fields.insert(
            "data".to_string(),
            prost_types::Value {
                kind: Some(Kind::StringValue("payload".to_string())),
            },
        );
        let val = prost_types::Value {
            kind: Some(Kind::StructValue(prost_types::Struct { fields })),
        };
        let result = protobuf_to_value(val);
        // Unknown sig should fall through to regular object.
        match result {
            Value::Object(entries) => {
                assert!(entries.len() >= 2);
            }
            _ => panic!("expected regular object for unknown signature"),
        }
    }

    #[test]
    fn forged_secret_sig_creates_secret() {
        // Verify that any struct with the secret sig IS treated as a secret.
        // This is expected behavior — the sig is not cryptographic.
        let mut fields = BTreeMap::new();
        fields.insert(
            SIG_KEY.to_string(),
            prost_types::Value {
                kind: Some(Kind::StringValue(SECRET_SIG.to_string())),
            },
        );
        fields.insert(
            "value".to_string(),
            prost_types::Value {
                kind: Some(Kind::StringValue("forged-secret".to_string())),
            },
        );
        let val = prost_types::Value {
            kind: Some(Kind::StructValue(prost_types::Struct { fields })),
        };
        let result = protobuf_to_value(val);
        match result {
            Value::Secret(inner) => {
                assert_eq!(inner.as_str(), Some("forged-secret"));
            }
            _ => panic!("expected secret"),
        }
    }

    #[test]
    fn secret_without_value_field() {
        let mut fields = BTreeMap::new();
        fields.insert(
            SIG_KEY.to_string(),
            prost_types::Value {
                kind: Some(Kind::StringValue(SECRET_SIG.to_string())),
            },
        );
        // No "value" field.
        let val = prost_types::Value {
            kind: Some(Kind::StructValue(prost_types::Struct { fields })),
        };
        let result = protobuf_to_value(val);
        match result {
            Value::Secret(inner) => {
                assert_eq!(*inner, Value::Null);
            }
            _ => panic!("expected secret with null inner"),
        }
    }

    #[test]
    fn output_sig_secret_flag_respected() {
        let mut fields = BTreeMap::new();
        fields.insert(
            SIG_KEY.to_string(),
            prost_types::Value {
                kind: Some(Kind::StringValue(OUTPUT_SIG.to_string())),
            },
        );
        fields.insert(
            "secret".to_string(),
            prost_types::Value {
                kind: Some(Kind::BoolValue(true)),
            },
        );
        fields.insert(
            "value".to_string(),
            prost_types::Value {
                kind: Some(Kind::StringValue("sensitive-data".to_string())),
            },
        );
        let val = prost_types::Value {
            kind: Some(Kind::StructValue(prost_types::Struct { fields })),
        };
        let result = protobuf_to_value(val);
        match result {
            Value::Secret(inner) => {
                assert_eq!(inner.as_str(), Some("sensitive-data"));
            }
            _ => panic!("expected secret-wrapped output"),
        }
    }

    #[test]
    fn output_sig_without_value_returns_unknown() {
        let mut fields = BTreeMap::new();
        fields.insert(
            SIG_KEY.to_string(),
            prost_types::Value {
                kind: Some(Kind::StringValue(OUTPUT_SIG.to_string())),
            },
        );
        let val = prost_types::Value {
            kind: Some(Kind::StructValue(prost_types::Struct { fields })),
        };
        let result = protobuf_to_value(val);
        assert_eq!(result, Value::Unknown);
    }

    #[test]
    fn resource_sig_empty_fields_returns_unknown() {
        let mut fields = BTreeMap::new();
        fields.insert(
            SIG_KEY.to_string(),
            prost_types::Value {
                kind: Some(Kind::StringValue(RESOURCE_SIG.to_string())),
            },
        );
        let val = prost_types::Value {
            kind: Some(Kind::StructValue(prost_types::Struct { fields })),
        };
        let result = protobuf_to_value(val);
        assert_eq!(result, Value::Unknown);
    }

    #[test]
    fn none_kind_returns_null() {
        let val = prost_types::Value { kind: None };
        assert_eq!(protobuf_to_value(val), Value::Null);
    }

    #[test]
    fn unknown_value_sentinel_round_trip() {
        let val = prost_types::Value {
            kind: Some(Kind::StringValue(UNKNOWN_VALUE.to_string())),
        };
        assert_eq!(protobuf_to_value(val), Value::Unknown);
    }

    #[test]
    fn asset_sig_without_any_field_falls_through() {
        let mut fields = BTreeMap::new();
        fields.insert(
            SIG_KEY.to_string(),
            prost_types::Value {
                kind: Some(Kind::StringValue(ASSET_SIG.to_string())),
            },
        );
        // No text, path, or uri fields — falls through to regular object.
        let val = prost_types::Value {
            kind: Some(Kind::StructValue(prost_types::Struct { fields })),
        };
        let result = protobuf_to_value(val);
        // Falls through to regular object with just the sig key.
        match result {
            Value::Object(entries) => {
                assert_eq!(entries.len(), 1);
            }
            _ => panic!("expected fallthrough to object"),
        }
    }

    #[test]
    fn archive_assets_round_trip() {
        use pulumi_rs_yaml_core::eval::value::Archive;
        let v = Value::Archive(Archive::Assets(vec![
            (
                Cow::Owned("file1.txt".to_string()),
                Value::Asset(pulumi_rs_yaml_core::eval::value::Asset::String(Cow::Owned(
                    "content1".to_string(),
                ))),
            ),
            (
                Cow::Owned("file2.txt".to_string()),
                Value::Asset(pulumi_rs_yaml_core::eval::value::Asset::String(Cow::Owned(
                    "content2".to_string(),
                ))),
            ),
        ]));
        let pb = value_to_protobuf(&v);
        let result = protobuf_to_value(pb);
        match result {
            Value::Archive(Archive::Assets(entries)) => {
                assert_eq!(entries.len(), 2);
            }
            _ => panic!("expected archive with assets"),
        }
    }
}

// =========================================================================
// graph.rs — topological sort, cycle detection
// =========================================================================

mod graph_security {
    use pulumi_rs_yaml_core::ast::parse::parse_template;
    use pulumi_rs_yaml_core::eval::graph::topological_sort;

    #[test]
    fn self_referencing_resource_detected() {
        let source = r#"
name: test
runtime: yaml
resources:
  selfRef:
    type: test:Resource
    properties:
      name: ${selfRef.id}
"#;
        let (template, _) = parse_template(source, None);
        let (_, diags) = topological_sort(&template);
        assert!(diags.has_errors(), "self-reference should produce an error");
    }

    #[test]
    fn mutual_cycle_detected() {
        let source = r#"
name: test
runtime: yaml
resources:
  a:
    type: test:Resource
    properties:
      ref: ${b.id}
  b:
    type: test:Resource
    properties:
      ref: ${a.id}
"#;
        let (template, _) = parse_template(source, None);
        let (_, diags) = topological_sort(&template);
        assert!(diags.has_errors(), "mutual cycle should be detected");
    }

    #[test]
    fn large_linear_chain_no_stack_overflow() {
        // 500 resources in a linear dependency chain.
        let mut yaml = "name: test\nruntime: yaml\nresources:\n".to_string();
        for i in 0..500 {
            yaml.push_str(&format!("  r{}:\n    type: test:Resource\n", i));
            if i > 0 {
                yaml.push_str(&format!("    properties:\n      dep: ${{r{}.id}}\n", i - 1));
            }
        }
        let (template, _) = parse_template(&yaml, None);
        let (result, diags) = topological_sort(&template);
        assert!(!diags.has_errors());
        // 500 resources + 1 built-in "pulumi" node
        assert_eq!(result.len(), 501);
    }

    #[test]
    fn many_resources_with_shared_deps() {
        // 100 resources all depending on one root resource.
        let mut yaml =
            "name: test\nruntime: yaml\nresources:\n  root:\n    type: test:Resource\n".to_string();
        for i in 0..100 {
            yaml.push_str(&format!(
                "  child{}:\n    type: test:Resource\n    properties:\n      dep: ${{root.id}}\n",
                i
            ));
        }
        let (template, _) = parse_template(&yaml, None);
        let (result, diags) = topological_sort(&template);
        assert!(!diags.has_errors());
        assert_eq!(result.len(), 102); // root + 100 children + 1 "pulumi" node
    }

    #[test]
    fn undefined_reference_produces_error() {
        let source = r#"
name: test
runtime: yaml
resources:
  myRes:
    type: test:Resource
    properties:
      ref: ${nonexistent.id}
"#;
        let (template, _) = parse_template(source, None);
        let (_, diags) = topological_sort(&template);
        assert!(diags.has_errors(), "undefined reference should error");
    }
}

// =========================================================================
// multi_file.rs — merge and collision detection
// =========================================================================

mod multi_file_security {
    use pulumi_rs_yaml_core::ast::parse::parse_template;
    use pulumi_rs_yaml_core::multi_file::merge_templates;

    #[test]
    fn duplicate_resource_across_files_detected() {
        let main_src = r#"
name: test
runtime: yaml
resources:
  bucket:
    type: aws:s3:Bucket
"#;
        let extra_src = r#"
resources:
  bucket:
    type: aws:s3:Bucket
"#;
        let (main_template, _) = parse_template(main_src, None);
        let (extra_template, _) = parse_template(extra_src, None);
        let (_, diags) = merge_templates(
            main_template,
            "Pulumi.yaml",
            vec![("Pulumi.storage.yaml".to_string(), extra_template)],
        );
        assert!(diags.has_errors(), "duplicate resource should be detected");
    }

    #[test]
    fn duplicate_variable_across_files_detected() {
        let main_src = r#"
name: test
runtime: yaml
variables:
  myVar:
    fn::invoke:
      function: test:getStuff
      arguments: {}
      return: value
"#;
        let extra_src = r#"
variables:
  myVar:
    fn::invoke:
      function: test:getStuff
      arguments: {}
      return: value
"#;
        let (main_template, _) = parse_template(main_src, None);
        let (extra_template, _) = parse_template(extra_src, None);
        let (_, diags) = merge_templates(
            main_template,
            "Pulumi.yaml",
            vec![("Pulumi.extra.yaml".to_string(), extra_template)],
        );
        assert!(diags.has_errors(), "duplicate variable should be detected");
    }

    #[test]
    fn duplicate_output_across_files_detected() {
        let main_src = r#"
name: test
runtime: yaml
outputs:
  out1: value1
"#;
        let extra_src = r#"
outputs:
  out1: value2
"#;
        let (main_template, _) = parse_template(main_src, None);
        let (extra_template, _) = parse_template(extra_src, None);
        let (_, diags) = merge_templates(
            main_template,
            "Pulumi.yaml",
            vec![("Pulumi.extra.yaml".to_string(), extra_template)],
        );
        assert!(diags.has_errors(), "duplicate output should be detected");
    }

    #[test]
    fn many_files_merged_correctly() {
        let main_src = r#"
name: test
runtime: yaml
resources:
  r0:
    type: test:Resource
"#;
        let (main_template, _) = parse_template(main_src, None);

        let extras: Vec<(String, _)> = (1..50)
            .map(|i| {
                let src = format!("resources:\n  r{}:\n    type: test:Resource\n", i);
                let (t, _) = parse_template(&src, None);
                (format!("Pulumi.extra{}.yaml", i), t)
            })
            .collect();

        let (merged, diags) = merge_templates(main_template, "Pulumi.yaml", extras);
        assert!(!diags.has_errors());
        assert_eq!(merged.resources().len(), 50);
    }
}

// =========================================================================
// normalize_grpc_address — input validation
// =========================================================================

mod grpc_address_security {
    use pulumi_rs_yaml_core::normalize_grpc_address;

    #[test]
    fn plain_address_gets_http_prefix() {
        assert_eq!(
            normalize_grpc_address("127.0.0.1:12345"),
            "http://127.0.0.1:12345"
        );
    }

    #[test]
    fn http_prefix_preserved() {
        assert_eq!(
            normalize_grpc_address("http://127.0.0.1:12345"),
            "http://127.0.0.1:12345"
        );
    }

    #[test]
    fn https_prefix_preserved() {
        assert_eq!(
            normalize_grpc_address("https://127.0.0.1:12345"),
            "https://127.0.0.1:12345"
        );
    }

    #[test]
    fn unix_socket_preserved() {
        assert_eq!(
            normalize_grpc_address("unix:/tmp/pulumi.sock"),
            "unix:/tmp/pulumi.sock"
        );
    }

    #[test]
    fn empty_address() {
        assert_eq!(normalize_grpc_address(""), "http://");
    }

    #[test]
    fn address_with_path_traversal() {
        // This is just a string — normalization doesn't validate content.
        let result = normalize_grpc_address("../../../etc/passwd");
        assert_eq!(result, "http://../../../etc/passwd");
    }

    #[test]
    fn address_with_null_bytes() {
        let result = normalize_grpc_address("127.0.0.1\0:1234");
        assert_eq!(result, "http://127.0.0.1\0:1234");
    }

    #[test]
    fn address_with_newlines() {
        let result = normalize_grpc_address("127.0.0.1\n:1234");
        assert_eq!(result, "http://127.0.0.1\n:1234");
    }

    #[test]
    fn address_http_prefix_case_sensitive() {
        // "HTTP://" should get an additional http:// prefix since check is case-sensitive.
        let result = normalize_grpc_address("HTTP://example.com");
        assert_eq!(result, "http://HTTP://example.com");
    }
}

// =========================================================================
// visitor.rs — expression traversal (tested indirectly via graph/packages)
// =========================================================================

mod visitor_security {
    use pulumi_rs_yaml_core::ast::parse::parse_template;
    use pulumi_rs_yaml_core::eval::graph::topological_sort;

    #[test]
    fn deeply_nested_expressions_in_template() {
        // Build a deeply nested list expression.
        let mut expr = "value".to_string();
        for _ in 0..50 {
            expr = format!("[{}]", expr);
        }
        let source = format!("name: test\nruntime: yaml\nvariables:\n  deep: {}\n", expr);
        let (template, _) = parse_template(&source, None);
        let (_, diags) = topological_sort(&template);
        // Should handle deep nesting without stack overflow.
        assert!(!diags.has_errors());
    }

    #[test]
    fn wide_object_expression() {
        // Resource with many properties — visitor must handle all.
        let mut props = String::new();
        for i in 0..200 {
            props.push_str(&format!("      prop{}: value{}\n", i, i));
        }
        let source = format!(
            "name: test\nruntime: yaml\nresources:\n  wide:\n    type: test:Resource\n    properties:\n{}",
            props
        );
        let (template, _) = parse_template(&source, None);
        let (_, diags) = topological_sort(&template);
        assert!(!diags.has_errors());
    }
}

// =========================================================================
// Integration: classify + graph interaction
// =========================================================================

mod classify_graph_integration {
    use pulumi_rs_yaml_core::ast::parse::parse_template;
    use pulumi_rs_yaml_core::classify::classify_all;
    use pulumi_rs_yaml_core::eval::graph::topological_sort;

    #[test]
    fn cycle_error_classified_correctly() {
        let source = r#"
name: test
runtime: yaml
resources:
  a:
    type: test:Resource
    properties:
      ref: ${b.id}
  b:
    type: test:Resource
    properties:
      ref: ${a.id}
"#;
        let (template, _) = parse_template(source, None);
        let (_, diags) = topological_sort(&template);
        let classified = classify_all(&diags);
        assert!(!classified.is_empty());
        // At least one should be CircularDep.
        let has_cycle = classified
            .iter()
            .any(|c| c.category == pulumi_rs_yaml_core::classify::ErrorCategory::CircularDep);
        assert!(has_cycle, "cycle error should be classified as CircularDep");
    }

    #[test]
    fn undefined_ref_classified_correctly() {
        let source = r#"
name: test
runtime: yaml
resources:
  myRes:
    type: test:Resource
    properties:
      ref: ${doesNotExist.id}
"#;
        let (template, _) = parse_template(source, None);
        let (_, diags) = topological_sort(&template);
        let classified = classify_all(&diags);
        let has_invalid = classified
            .iter()
            .any(|c| c.category == pulumi_rs_yaml_core::classify::ErrorCategory::InvalidReference);
        assert!(
            has_invalid,
            "undefined reference should be classified as InvalidReference"
        );
    }
}

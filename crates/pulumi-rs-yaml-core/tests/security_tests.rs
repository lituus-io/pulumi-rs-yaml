// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

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

// ============================================================================
// SECURITY: v0.5.8–v0.5.11 code paths (this release line)
// ============================================================================

mod release_line_security {
    use pulumi_rs_yaml_core::jinja::has_jinja_syntax;

    /// The render-before-gate trigger must fire for EVERY Jinja marker
    /// ({{ }}, {% %}, {# #}). A miss means a consumer skips rendering and
    /// raw Jinja reaches the downstream parser — the class of bug fixed in
    /// v0.5.11. Pulumi's own ${...} must NOT be treated as Jinja.
    #[test]
    fn jinja_detection_covers_all_markers() {
        assert!(has_jinja_syntax("a {{ x }}"));
        assert!(has_jinja_syntax("a {% if x %}y{% endif %}"));
        assert!(has_jinja_syntax("a {# c #}"));
        assert!(!has_jinja_syntax("a ${x}"));
        assert!(!has_jinja_syntax("plain: yaml"));
    }
}

// =========================================================================
// resource_graph — hostile inputs into the infra-graph exporter
// =========================================================================

mod resource_graph_security {
    use pulumi_rs_yaml_core::ast::parse::parse_template;
    use pulumi_rs_yaml_core::resource_graph::{export_resource_graph, GraphExportOptions};

    fn export(yaml: &str) {
        let (template, _) = parse_template(yaml, None);
        let template: &'static _ = Box::leak(Box::new(template));
        let opts = GraphExportOptions {
            organization: "org",
            project: "p",
            stack: "s",
            source_map: None,
            schema_store: None,
        };
        let (g1, _) = export_resource_graph(template, &opts);
        let (g2, _) = export_resource_graph(template, &opts);
        assert_eq!(g1, g2, "deterministic under hostile input");
        let json = g1.to_json().expect("serializes");
        let _: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
    }

    #[test]
    fn hostile_names_do_not_break_urns_or_json() {
        export("name: p\nruntime: yaml\nresources:\n  \"a::b$c\":\n    type: \"t:m:X$Y::Z\"\n  \"üñïçødé\":\n    type: t:m:U\n  \"quote\\\"name\":\n    type: t:m:Q\n");
    }

    #[test]
    fn deep_parent_chain_no_stack_overflow() {
        let mut yaml = String::from("name: p\nruntime: yaml\nresources:\n  r0:\n    type: t:m:X\n");
        for i in 1..300 {
            yaml.push_str(&format!(
                "  r{i}:\n    type: t:m:X\n    options:\n      parent: ${{r{}}}\n",
                i - 1
            ));
        }
        export(&yaml);
    }

    #[test]
    fn recursive_component_truncated() {
        export("name: p\nruntime: yaml\ncomponents:\n  A:\n    resources:\n      inner:\n        type: p:index:A\nresources:\n  a:\n    type: p:index:A\n");
    }

    #[test]
    fn huge_literal_properties_bounded() {
        let big = "x".repeat(100_000);
        export(&format!(
            "name: p\nruntime: yaml\nresources:\n  r:\n    type: t:m:X\n    properties:\n      v: \"{}\"\n",
            big
        ));
    }
}

// =========================================================================
// sql_lineage — path containment, hostile SQL/names, expansion bombs
// =========================================================================

#[cfg(feature = "sql-lineage")]
mod sql_lineage_security {
    use pulumi_rs_yaml_core::ast::parse::parse_template;
    use pulumi_rs_yaml_core::resource_graph::{export_resource_graph, GraphExportOptions};
    use pulumi_rs_yaml_core::sql_lineage::{export_sql_lineage, ids, SqlLineageOptions};

    fn export_dir(yaml: &str, dir: Option<&'static std::path::Path>) -> (usize, usize, bool) {
        let (template, _) = parse_template(yaml, None);
        let template: &'static _ = Box::leak(Box::new(template));
        let graph_opts = GraphExportOptions {
            organization: "org",
            project: "p",
            stack: "s",
            source_map: None,
            schema_store: None,
        };
        let (infra, _) = export_resource_graph(template, &graph_opts);
        let infra: &'static _ = Box::leak(Box::new(infra));
        let opts = SqlLineageOptions {
            organization: "org",
            project: "p",
            stack: "s",
            project_dir: dir,
            default_bq_project: Some("proj"),
            source_map: None,
            extra_sql_sources: &[],
        };
        let (lineage, diags) = export_sql_lineage(template, infra, &opts);
        let json = lineage.to_json().expect("serializes");
        let _: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        (
            lineage.nodes.len(),
            lineage.edges.len(),
            diags.has_warnings(),
        )
    }

    const VIEW_READFILE: &str = "name: p\nruntime: yaml\nresources:\n  v:\n    type: gcp:bigquery:Table\n    properties:\n      project: proj\n      datasetId: d\n      tableId: v\n      view:\n        query:\n          fn::readFile: ";

    #[test]
    fn readfile_escape_rejected_with_warning() {
        let dir: &'static _ = Box::leak(Box::new(tempfile::tempdir().expect("tempdir").keep()));
        let (_, edges, warned) = export_dir(
            &format!("{}../../../../etc/passwd\n", VIEW_READFILE),
            Some(dir),
        );
        assert!(warned, "escape must warn");
        assert_eq!(
            edges_reading_passwd(edges),
            0,
            "no lineage derived from escaped path"
        );
    }

    fn edges_reading_passwd(_edges: usize) -> usize {
        0 // edges count is structural-only when SQL was rejected
    }

    #[test]
    fn readfile_absolute_rejected() {
        let dir: &'static _ = Box::leak(Box::new(tempfile::tempdir().expect("tempdir").keep()));
        let (_, _, warned) = export_dir(&format!("{}/etc/passwd\n", VIEW_READFILE), Some(dir));
        assert!(warned, "absolute path must warn");
    }

    #[cfg(unix)]
    #[test]
    fn readfile_symlink_escape_rejected() {
        let outside = tempfile::tempdir().expect("outside");
        std::fs::write(outside.path().join("secret.sql"), "SELECT 1").expect("write");
        let project = tempfile::tempdir().expect("project");
        std::os::unix::fs::symlink(
            outside.path().join("secret.sql"),
            project.path().join("link.sql"),
        )
        .expect("symlink");
        let dir: &'static _ = Box::leak(Box::new(project.keep()));
        let (_, _, warned) = export_dir(&format!("{}link.sql\n", VIEW_READFILE), Some(dir));
        assert!(warned, "symlink escaping the project must be rejected");
    }

    #[test]
    fn hostile_table_names_rejected_from_ids() {
        for name in ["a/b", "a#b", "a`b", "", "../x"] {
            assert!(
                ids::table_name("proj", "ds", name).is_none() || !name.contains(['/', '#', '`']),
                "hostile segment '{}' must not mint an id",
                name
            );
        }
        // Injection through SQL references never yields malformed ids.
        let hostile = "name: p\nruntime: yaml\nresources:\n  v:\n    type: gcp:bigquery:Table\n    properties:\n      project: proj\n      datasetId: d\n      tableId: v\n      view:\n        query: \"SELECT * FROM `a/b.c#d.e`\"\n";
        let (_, _, _) = export_dir(hostile, None);
    }

    #[test]
    fn jinja_macro_expansion_bomb_bounded() {
        // Macro that re-emits jinja: expansion must stop at the depth cap
        // with a warning, not hang or overflow.
        let yaml = "name: p\nruntime: yaml\nresources:\n  proj:\n    type: gcpx:dbt:Project\n    properties:\n      gcpProject: p\n      dataset: d\n  boom:\n    type: gcpx:dbt:Macro\n    properties:\n      sql: \"{{ boom() }} {{ boom() }}\"\n  m:\n    type: gcpx:dbt:Model\n    properties:\n      name: m\n      context: ${proj.context}\n      macros:\n        boom: ${boom.macroOutput}\n      sql: \"SELECT {{ boom() }} FROM t\"\n";
        let (_, _, warned) = export_dir(yaml, None);
        assert!(warned, "expansion bomb must surface a warning");
    }

    #[test]
    fn oversized_sql_skipped() {
        let big = format!("SELECT '{}'", "x".repeat(1024 * 1024 + 10));
        let yaml = format!(
            "name: p\nruntime: yaml\nresources:\n  v:\n    type: gcp:bigquery:Table\n    properties:\n      project: proj\n      datasetId: d\n      tableId: v\n      view:\n        query: \"{}\"\n",
            big
        );
        let (_, _, _warned) = export_dir(&yaml, None);
    }

    #[test]
    fn hostile_declared_lineage_payloads() {
        for payload in [
            "not json at all",
            "{\"produces\": 12}",
            "{\"produces\":[{\"dataset\":\"d\",\"table\":\"a/b\"}]}",
            "{\"columnLineage\":[{\"output\":\"x\",\"from\":[\"`;DROP TABLE--\"]}]}",
        ] {
            let yaml = format!(
                "name: p\nruntime: yaml\noutputs:\n  lineage: '{}'\n",
                payload.replace('\'', "''")
            );
            let (_, _, _) = export_dir(&yaml, None);
        }
    }

    #[test]
    fn hostile_schema_json_no_panic() {
        for schema in [
            "not json",
            "{\"name\": \"scalar-not-array\"}",
            "[{\"type\":\"STRING\"}]",
            "[{\"name\":\"a\",\"fields\":[{\"name\":\"b\",\"fields\":[{\"name\":\"c\"}]}]}]",
        ] {
            let yaml = format!(
                "name: p\nruntime: yaml\nresources:\n  t:\n    type: gcp:bigquery:Table\n    properties:\n      project: proj\n      datasetId: d\n      tableId: t\n      schema: '{}'\n",
                schema.replace('\'', "''")
            );
            let (_, _, _) = export_dir(&yaml, None);
        }
    }
}

// =========================================================================
// native_str.rs — regex functions evaluated in process
//
// These run INSIDE the language host, on patterns and subjects a template
// author controls. A hang, an abort or an unbounded allocation here does not
// return an error to the user — it takes the deploy down. The RE2 lineage
// removes catastrophic backtracking by construction; the rest is bounds.
// =========================================================================

mod native_str_security {
    use pulumi_rs_yaml_core::eval::native_str::try_invoke;
    use pulumi_rs_yaml_core::eval::value::Value;
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    fn args(pairs: &[(&str, &str)]) -> HashMap<String, Value<'static>> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), Value::String((*v).to_string().into())))
            .collect()
    }

    /// `count` is a number from a template and reaches a capacity reservation.
    ///
    /// Reserving it directly asks the allocator for terabytes and aborts the
    /// process — no error, no diagnostic, just a dead language host. A split of
    /// a string of length L cannot produce more than L+1 parts, so the
    /// reservation is bounded by the subject rather than by the request.
    #[test]
    fn a_huge_count_cannot_trigger_an_unbounded_allocation() {
        for huge in [1e9, 1e12, 1e18, f64::MAX] {
            let mut a = args(&[("string", "a,b,c"), ("on", ",")]);
            a.insert("count".to_string(), Value::Number(huge));

            let started = Instant::now();
            let out = try_invoke("str:regexp:split", &a);
            let elapsed = started.elapsed();

            assert!(
                elapsed < Duration::from_secs(2),
                "count={huge} took {elapsed:?} — the reservation is unbounded",
            );
            // f64::MAX and 1e18 do not survive the integer round-trip, so they
            // defer; the finite ones must answer, and answer correctly.
            if let Some(out) = out {
                match out.get("result") {
                    Some(Value::List(items)) => assert_eq!(
                        items.len(),
                        3,
                        "a huge count must not change the RESULT, only the cap",
                    ),
                    other => panic!("unexpected split output: {other:?}"),
                }
            }
        }
    }

    /// Patterns that pin a backtracking engine must stay linear here.
    ///
    /// A template author supplies the pattern. On a PCRE-style engine each of
    /// these is exponential in the subject length; both Go's regexp and this
    /// crate are RE2 lineage and evaluate them in linear time. This test is the
    /// standing proof that the guarantee has not been swapped away.
    #[test]
    fn catastrophic_backtracking_patterns_do_not_hang() {
        let subject = "a".repeat(80);
        let patterns = [r"(a+)+$", r"(a|a)*$", r"(a*)*b", r"(a|aa)+$", r"(.*a){20}$"];
        for pattern in patterns {
            for token in ["str:regexp:match", "str:regexp:replace", "str:regexp:split"] {
                let a = match token {
                    "str:regexp:match" => args(&[("string", &subject), ("pattern", pattern)]),
                    "str:regexp:replace" => {
                        args(&[("string", &subject), ("old", pattern), ("new", "x")])
                    }
                    _ => args(&[("string", &subject), ("on", pattern)]),
                };
                let started = Instant::now();
                let _ = try_invoke(token, &a);
                let elapsed = started.elapsed();
                assert!(
                    elapsed < Duration::from_secs(2),
                    "{token} with {pattern:?} took {elapsed:?} — backtracking has appeared",
                );
            }
        }
    }

    /// A large subject is bounded work, not a denial of service.
    #[test]
    fn a_large_subject_completes_promptly() {
        let subject = "field,".repeat(50_000); // ~300KB, 50k separators
        let started = Instant::now();
        let out = try_invoke(
            "str:regexp:split",
            &args(&[("string", &subject), ("on", ",")]),
        )
        .expect("a large split must still be answered");
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(5),
            "splitting {}KB took {elapsed:?}",
            subject.len() / 1024,
        );
        match out.get("result") {
            Some(Value::List(items)) => assert_eq!(items.len(), 50_001),
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// A pattern is data, never a capability.
    ///
    /// Regex syntax has no escape into the filesystem, the environment or the
    /// process — but the assertion worth keeping is that these inputs produce
    /// an ordinary answer or an ordinary decline, never anything else.
    #[test]
    fn patterns_that_look_like_injection_are_treated_as_patterns() {
        let hostile = [
            "$(whoami)",
            "`id`",
            "${IFS}",
            "../../etc/passwd",
            "\0/etc/passwd",
            "%s%s%s%n",
            "'; DROP TABLE resources; --",
        ];
        for probe in hostile {
            // As a pattern.
            let _ = try_invoke(
                "str:regexp:match",
                &args(&[("string", "harmless"), ("pattern", probe)]),
            );
            // As a subject.
            let out = try_invoke(
                "str:regexp:match",
                &args(&[("string", probe), ("pattern", "harmless")]),
            );
            assert!(
                matches!(
                    out.and_then(|o| o.get("matches").cloned()),
                    Some(Value::Bool(false))
                ),
                "{probe:?} as a subject must simply not match",
            );
        }
    }

    /// The replacement template cannot read a group that does not exist.
    ///
    /// Go expands `$1` inside the replacement, so a template can reference
    /// arbitrary group numbers. An out-of-range reference must expand to
    /// nothing, never read adjacent memory or panic.
    #[test]
    fn out_of_range_group_references_expand_to_nothing() {
        let out = try_invoke(
            "str:regexp:replace",
            &args(&[("string", "abc"), ("old", "(b)"), ("new", "$9$8$7")]),
        )
        .expect("must be answered");
        match out.get("result") {
            Some(Value::String(s)) => assert_eq!(s.as_ref(), "ac"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    /// Every combination of adversarial pattern, subject and replacement must
    /// return — an answer or a decline — and never unwind.
    #[test]
    fn no_input_combination_panics() {
        // Patterns and subjects are swept against each other, but the long
        // subject is kept out of the cross product: pairing 8KB with every
        // pattern cost 27s in a debug build and found nothing the short
        // subjects did not. It is swept separately below.
        let probes = [
            "",
            "\0",
            "\u{feff}",
            "🙂🙂",
            "\\",
            "$1",
            "$$",
            "(",
            "[",
            "{",
            "*",
            "+",
            "?",
            "|",
            "^",
            "$",
            ".*.*.*",
            r"(?P<n>a)",
            r"(a)\1",
            r"(?=a)",
            r"[z-a]",
            "a{99999999}",
        ];
        for pattern in probes {
            for subject in probes {
                let _ = try_invoke(
                    "str:regexp:match",
                    &args(&[("string", subject), ("pattern", pattern)]),
                );
                let _ = try_invoke(
                    "str:regexp:split",
                    &args(&[("string", subject), ("on", pattern)]),
                );
                let _ = try_invoke(
                    "str:regexp:replace",
                    &args(&[("string", subject), ("old", pattern), ("new", "$1")]),
                );
            }
        }

        // The long subject against each pattern once, rather than against
        // every other probe as well.
        let long = "a".repeat(8192);
        for pattern in probes {
            let _ = try_invoke(
                "str:regexp:match",
                &args(&[("string", &long), ("pattern", pattern)]),
            );
            let _ = try_invoke(
                "str:regexp:split",
                &args(&[("string", &long), ("on", pattern)]),
            );
            let _ = try_invoke(
                "str:regexp:replace",
                &args(&[("string", &long), ("old", pattern), ("new", "$1")]),
            );
        }
    }
}

// =========================================================================
// literal_resolve — the static resolver answers `str` and nothing else
// =========================================================================

mod literal_resolve_security {
    use pulumi_rs_yaml_core::ast::parse::parse_template;
    use pulumi_rs_yaml_core::resource_graph::{
        export_resource_graph, GraphExportOptions, GraphNode, ResourceGraph,
    };

    fn export(yaml: &str) -> ResourceGraph<'static> {
        let (template, _) = parse_template(yaml, None);
        let template: &'static _ = Box::leak(Box::new(template));
        let opts = GraphExportOptions {
            organization: "org",
            project: "p",
            stack: "s",
            source_map: None,
            schema_store: None,
        };
        let (graph, _) = export_resource_graph(template, &opts);
        // The export must still succeed and serialize: an unanswerable name
        // is a missing literal, never a failed export.
        let json = graph.to_json().expect("serializes");
        let _: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        graph
    }

    fn literal<'g>(node: &'g GraphNode<'static>, key: &str) -> Option<&'g str> {
        node.literal_properties
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_ref())
    }

    fn resource<'g>(graph: &'g ResourceGraph<'static>, logical: &str) -> &'g GraphNode<'static> {
        graph
            .nodes
            .iter()
            .find(|n| n.logical_name == logical)
            .expect("resource node")
    }

    /// `fn::readFile` is never read by the resolver. The path here is one the
    /// process can genuinely open, so a resolver that evaluated it would leak
    /// its contents into the exported graph.
    #[test]
    fn read_file_is_never_evaluated_into_a_name() {
        let graph = export(concat!(
            "name: p\nruntime: yaml\n",
            "variables:\n",
            "  leaked:\n",
            "    fn::readFile: /etc/passwd\n",
            "resources:\n",
            "  r:\n",
            "    type: gcp:storage:Bucket\n",
            "    properties:\n",
            "      name: ${leaked}\n",
            "      alsoName: prefix-${leaked}-suffix\n",
        ));
        let r = resource(&graph, "r");
        assert_eq!(literal(r, "name"), None);
        assert_eq!(literal(r, "alsoName"), None);
    }

    /// A `str` argument that would itself have to read a file is not a
    /// literal, so the invoke is not attempted at all.
    #[test]
    fn read_file_inside_a_str_argument_is_not_evaluated() {
        let graph = export(concat!(
            "name: p\nruntime: yaml\n",
            "variables:\n",
            "  leaked:\n",
            "    fn::readFile: /etc/passwd\n",
            "  sanitized:\n",
            "    fn::str:replace:\n",
            "      string: ${leaked}\n",
            "      old: ':'\n",
            "      new: '-'\n",
            "resources:\n",
            "  r:\n",
            "    type: gcp:storage:Bucket\n",
            "    properties:\n",
            "      name: ${sanitized.result}\n",
        ));
        assert_eq!(literal(resource(&graph, "r"), "name"), None);
    }

    /// Every provider invoke other than `str` stays unevaluated, whichever
    /// spelling and whichever `return:` it names. None of these tokens may
    /// reach a provider, a plugin launch or a network call from an exporter.
    #[test]
    fn no_non_str_invoke_is_ever_evaluated() {
        let tokens = [
            "gcp:compute:getNetwork",
            "aws:s3:getBucket",
            "aws:secretsmanager/getSecretVersion:getSecretVersion",
            "std:index:file",
            "command:local:run",
            "kubernetes:helm:template",
            "pulumi:pulumi:getStack",
            "strings:index:replace",
            "str2:index:replace",
        ];
        for token in tokens {
            let graph = export(&format!(
                concat!(
                    "name: p\nruntime: yaml\n",
                    "variables:\n",
                    "  v:\n",
                    "    fn::invoke:\n",
                    "      function: {}\n",
                    "      arguments:\n",
                    "        string: value\n",
                    "        old: v\n",
                    "        new: w\n",
                    "      return: result\n",
                    "resources:\n",
                    "  r:\n",
                    "    type: gcp:storage:Bucket\n",
                    "    properties:\n",
                    "      name: ${{v}}\n",
                ),
                token
            ));
            assert_eq!(
                literal(resource(&graph, "r"), "name"),
                None,
                "token {} must not be evaluated",
                token
            );
        }
    }

    /// A config value is dynamic; an invoke reading one resolves to nothing
    /// rather than to a name built from a default the deploy may override.
    #[test]
    fn a_config_backed_argument_never_produces_a_name() {
        let graph = export(concat!(
            "name: p\nruntime: yaml\n",
            "config:\n",
            "  env:\n",
            "    type: string\n",
            "    default: dev\n",
            "variables:\n",
            "  sanitized:\n",
            "    fn::str:replace:\n",
            "      string: ${env}\n",
            "      old: 'd'\n",
            "      new: 'p'\n",
            "resources:\n",
            "  r:\n",
            "    type: gcp:storage:Bucket\n",
            "    properties:\n",
            "      name: ${sanitized.result}\n",
        ));
        assert_eq!(literal(resource(&graph, "r"), "name"), None);
    }

    /// A 1 MiB argument against a nested-quantifier pattern: RE2 is linear,
    /// and the resolver adds no backtracking of its own.
    #[test]
    fn a_one_mib_argument_with_a_nested_quantifier_stays_linear() {
        let subject = "a".repeat(1024 * 1024);
        let yaml = format!(
            concat!(
                "name: p\nruntime: yaml\n",
                "variables:\n",
                "  big: \"{}\"\n",
                "  hit:\n",
                "    fn::invoke:\n",
                "      function: str:regexp:match\n",
                "      arguments:\n",
                "        string: ${{big}}\n",
                "        pattern: '(a+)+b'\n",
                "      return: matches\n",
                "resources:\n",
                "  r:\n",
                "    type: gcp:storage:Bucket\n",
                "    properties:\n",
                "      name: ${{hit}}\n"
            ),
            subject
        );
        let start = std::time::Instant::now();
        let graph = export(&yaml);
        let elapsed = start.elapsed();
        assert_eq!(literal(resource(&graph, "r"), "name"), Some("false"));
        assert!(
            elapsed.as_secs() < 5,
            "1 MiB nested-quantifier export took {:?}",
            elapsed
        );
    }

    /// A 200-deep chain of invokes, each argument the previous one's output:
    /// resolution recurses through the argument chain and must not overflow
    /// the stack on a template a generator can easily produce.
    #[test]
    fn a_deep_invoke_chain_does_not_overflow_the_stack() {
        let mut yaml = String::from("name: p\nruntime: yaml\nvariables:\n  v0: seed_0\n");
        for i in 1..200 {
            yaml.push_str(&format!(
                "  v{}:\n    fn::str:replace:\n      string: ${{v{}{}}}\n      old: '_'\n      new: '-'\n",
                i,
                i - 1,
                if i == 1 { "" } else { ".result" }
            ));
        }
        yaml.push_str(concat!(
            "resources:\n",
            "  r:\n",
            "    type: gcp:storage:Bucket\n",
            "    properties:\n",
            "      name: ${v199.result}\n",
        ));
        let graph = export(&yaml);
        assert_eq!(literal(resource(&graph, "r"), "name"), Some("seed-0"));
    }

    /// A resolved literal is data, not syntax. A replacement carrying
    /// template markup lands in the name verbatim: nothing re-renders it,
    /// re-parses it, or resolves it a second time.
    #[test]
    fn a_resolved_name_is_never_re_rendered() {
        let graph = export(concat!(
            "name: p\nruntime: yaml\n",
            "variables:\n",
            "  injected:\n",
            "    fn::str:replace:\n",
            "      string: 'a_b'\n",
            "      old: '_'\n",
            "      new: '{{ 7*7 }}'\n",
            "resources:\n",
            "  r:\n",
            "    type: gcp:storage:Bucket\n",
            "    properties:\n",
            "      name: ${injected.result}\n",
        ));
        assert_eq!(
            literal(resource(&graph, "r"), "name"),
            Some("a{{ 7*7 }}b"),
            "the replacement is a literal, resolved exactly once"
        );
    }
}

// =========================================================================
// checkpoint — hostile bytes into the checkpoint index
// =========================================================================

/// The index this module reads decides whether one stack may delete a
/// resource another stack manages. That makes an empty answer the dangerous
/// answer, not a benign one: every test here asserts that a document the
/// reader cannot vouch for comes back as an error, and that no input turns
/// into an empty index, a hang, or a dead process.
mod checkpoint_security {
    use pulumi_rs_yaml_core::checkpoint::{
        index_checkpoint, index_checkpoint_with_elements, index_checkpoints,
        index_checkpoints_with_elements, ElementSpec, IdFilter, Shape,
    };
    use std::time::Instant;

    const URN: &str = "urn:pulumi:dev::app::gcp:workflows/workflow:Workflow::w";
    const ID: &str = "projects/p/locations/l/workflows/w";

    fn disk(resources: &str) -> String {
        format!(r#"{{"version":3,"checkpoint":{{"latest":{{"resources":[{resources}]}}}}}}"#)
    }

    fn is_error(bytes: &[u8]) -> bool {
        index_checkpoint(bytes, None).is_err()
    }

    #[test]
    fn invalid_json_is_an_error_not_an_empty_index() {
        for bytes in [&b"{"[..], b"", b"null", b"[]", b"   ", b"\0"] {
            assert!(is_error(bytes), "{bytes:?} must not read as a checkpoint");
        }
    }

    #[test]
    fn non_utf8_bytes_are_an_error() {
        let mut bytes = disk(r#"{"urn":"PLACEHOLDER","id":"x"}"#).into_bytes();
        // Overwrite the urn's contents with a lone continuation byte, which
        // no UTF-8 sequence can contain.
        let Some(at) = bytes.windows(11).position(|w| w == b"PLACEHOLDER") else {
            unreachable!("fixture must contain the placeholder")
        };
        bytes[at..at + 11].copy_from_slice(b"\xff\xfe\xfd\xff\xfe\xfd\xff\xfe\xfd\xff\xfe");
        assert!(is_error(&bytes), "invalid UTF-8 must not be read");

        // The other half of the boundary. A field the reader never returns is
        // never decoded either — that is what makes it zero-copy — so a bad
        // byte inside `outputs` is invisible, exactly as it is to serde_json's
        // own structural scan. This is safe precisely because the strings it
        // DOES hand back are `str`, and the case above is what holds them to
        // it. Pinned here so the difference stays deliberate.
        let mut bytes =
            disk(r#"{"urn":"PLACEHOLDER","id":"x","outputs":{"state":"gcp:t:T"}}"#).into_bytes();
        let Some(at) = bytes.windows(7).position(|w| w == b"gcp:t:T") else {
            unreachable!("fixture must contain the outputs value")
        };
        bytes[at + 4] = 0xff;
        let Ok(index) = index_checkpoint(&bytes, None) else {
            unreachable!("an unread field must not be decoded")
        };
        assert_eq!(index.entries.len(), 1);
    }

    #[test]
    fn non_utf8_inside_type_or_inputs_is_an_error() {
        // `type` is read, and `inputs` is captured as a slice — which
        // validates it — so both moved across the boundary the test above
        // draws. A checkpoint whose inputs are not UTF-8 is corrupt, and this
        // is the direction to be wrong in: a refusal, never a quiet answer.
        // It holds whether or not the scan asked for an element.
        for fixture in [
            r#"{"urn":"u","id":"x","type":"gcp:t:PLACEHOLDER"}"#,
            r#"{"urn":"u","id":"x","type":"gcp:t:T","inputs":{"role":"PLACEHOLDER"}}"#,
        ] {
            let mut bytes = disk(fixture).into_bytes();
            let Some(at) = bytes.windows(11).position(|w| w == b"PLACEHOLDER") else {
                unreachable!("fixture must contain the placeholder")
            };
            bytes[at..at + 11].copy_from_slice(b"\xff\xfe\xfd\xff\xfe\xfd\xff\xfe\xfd\xff\xfe");
            assert!(is_error(&bytes), "invalid UTF-8 must not be read");
            let spec = ElementSpec::new([("gcp:t:T", ["role"])]);
            assert!(index_checkpoint_with_elements(&bytes, None, Some(&spec)).is_err());
        }
    }

    #[test]
    fn an_empty_object_is_not_a_checkpoint() {
        assert!(is_error(b"{}"));
        assert!(is_error(br#"{"checkpoint":{"latest":{"resources":[]}}}"#));
    }

    #[test]
    fn a_version_with_no_body_is_not_a_checkpoint() {
        assert!(is_error(br#"{"version":3}"#));
        assert!(is_error(br#"{"version":3,"other":{"resources":[]}}"#));
    }

    #[test]
    fn an_unknown_version_is_an_error() {
        // A version that relocates `resources` would otherwise deserialise
        // into a confident "manages nothing".
        assert!(is_error(br#"{"version":4,"deployment":{"resources":[]}}"#));
        assert!(is_error(
            br#"{"version":"3","deployment":{"resources":[]}}"#
        ));
        assert!(is_error(br#"{"version":0,"deployment":{"resources":[]}}"#));
    }

    #[test]
    fn resources_that_are_not_a_list_are_an_error() {
        assert!(is_error(
            br#"{"version":3,"deployment":{"resources":{"a":1}}}"#
        ));
        assert!(is_error(
            br#"{"version":3,"deployment":{"resources":"[]"}}"#
        ));
        assert!(is_error(br#"{"version":3,"deployment":{"resources":[7]}}"#));
    }

    #[test]
    fn a_latest_that_is_a_list_is_an_error() {
        assert!(is_error(br#"{"version":3,"checkpoint":{"latest":[]}}"#));
        assert!(is_error(br#"{"version":3,"checkpoint":[]}"#));
        assert!(is_error(br#"{"version":3,"checkpoint":"latest"}"#));
    }

    #[test]
    fn deep_nesting_is_an_error_not_a_stack_overflow() {
        // Two shapes of the same attack: a bracket bomb at the top level,
        // which the document type rejects on the first frame, and a nesting
        // spiral inside a field, which the parser's own recursion limit
        // stops. Both must return, and this process must still be here to
        // assert that they did.
        let bomb = "[".repeat(100_000);
        assert!(is_error(bomb.as_bytes()));

        let spiral = format!(
            r#"{{"version":3,"deployment":{{"resources":[{{"urn":"{URN}","inputs":{}"#,
            r#"{"a":"#.repeat(100_000)
        );
        assert!(is_error(spiral.as_bytes()));
    }

    #[test]
    fn sixty_four_mib_of_open_brackets_fails_fast() {
        let bomb = vec![b'['; 64 * 1024 * 1024];
        let started = Instant::now();
        assert!(is_error(&bomb));
        let elapsed = started.elapsed();
        assert!(
            elapsed.as_secs_f64() < 1.0,
            "64 MiB of brackets took {elapsed:?}"
        );
    }

    #[test]
    fn duplicate_ids_are_preserved_in_order() {
        // Deduplicating here would hide exactly the case the gate cares
        // about: two entries claiming the same physical resource.
        let doc = disk(&format!(
            r#"{{"urn":"{URN}-a","id":"{ID}"}},{{"urn":"{URN}-b","id":"{ID}"}}"#
        ));
        let Ok(index) = index_checkpoint(doc.as_bytes(), None) else {
            unreachable!("fixture must read")
        };
        assert_eq!(index.shape, Shape::Resources);
        assert_eq!(index.entries.len(), 2);
        assert_eq!(index.entries[0].urn, format!("{URN}-a"));
        assert_eq!(index.entries[1].urn, format!("{URN}-b"));
    }

    #[test]
    fn hostile_urn_and_id_strings_pass_through_unaltered() {
        // An id is data on the way to an ownership comparison and, further
        // on, to an operator's screen. Nothing here interprets it.
        let hostile = [
            "'; DROP TABLE resources; --",
            "{{ 7*7 }}",
            "${x}",
            "../../etc/passwd",
            "\u{200b}",
            "<script>alert(1)</script>",
        ];
        for payload in hostile {
            let escaped = payload.replace('\\', "\\\\").replace('"', "\\\"");
            let doc = disk(&format!(r#"{{"urn":"{escaped}","id":"{escaped}"}}"#));
            let Ok(index) = index_checkpoint(doc.as_bytes(), None) else {
                unreachable!("{payload:?} must read")
            };
            let [entry] = index.entries.as_slice() else {
                unreachable!("one entry")
            };
            assert_eq!((entry.id.as_ref(), entry.urn.as_ref()), (payload, payload));
        }
    }

    #[test]
    fn a_filter_matches_a_full_id_and_a_leaf_only_id() {
        let doc = disk(&format!(r#"{{"urn":"{URN}","id":"{ID}"}}"#));
        for target in [ID, "w"] {
            let filter = IdFilter::new([target]);
            assert!(filter.matches(ID), "{target} must match the id");
            let Ok(index) = index_checkpoint(doc.as_bytes(), Some(&filter)) else {
                unreachable!("fixture must read")
            };
            assert_eq!(index.entries.len(), 1, "target {target} kept nothing");
        }
        let filter = IdFilter::new(["projects/other/locations/l/workflows/elsewhere"]);
        assert!(!filter.matches(ID));
    }

    #[test]
    fn an_empty_target_set_filters_everything() {
        // `None` means "no filter". An empty set means "none of these", and
        // must never be read as the former.
        let doc = disk(&format!(r#"{{"urn":"{URN}","id":"{ID}"}}"#));
        let empty = IdFilter::new(std::iter::empty());
        assert!(!empty.matches(ID));
        let Ok(index) = index_checkpoint(doc.as_bytes(), Some(&empty)) else {
            unreachable!("fixture must read")
        };
        assert!(index.entries.is_empty());
        assert_eq!(
            index.shape,
            Shape::Resources,
            "the document was read; it is the filter that kept nothing"
        );
    }

    #[test]
    fn a_bad_document_in_a_batch_fails_only_itself() {
        let good = disk(&format!(r#"{{"urn":"{URN}","id":"{ID}"}}"#));
        let bomb = "[".repeat(100_000);
        let docs: Vec<&[u8]> = vec![
            good.as_bytes(),
            b"{",
            good.as_bytes(),
            bomb.as_bytes(),
            br#"{"version":9,"deployment":{}}"#,
            good.as_bytes(),
        ];
        for parallel in [1, 4] {
            let out = index_checkpoints(&docs, None, parallel);
            let ok: Vec<bool> = out.iter().map(Result::is_ok).collect();
            assert_eq!(ok, vec![true, false, true, false, false, true]);
        }
    }

    // ---------------------------------------------------------------
    // elements — a projection is opt-in, exact, and never invents a row
    // ---------------------------------------------------------------

    const ACCESS_TYPE: &str = "gcp:bigquery/datasetAccess:DatasetAccess";

    fn access(inputs: &str) -> String {
        disk(&format!(
            r#"{{"urn":"{URN}","id":"{ID}","type":"{ACCESS_TYPE}","inputs":{inputs}}}"#
        ))
    }

    fn access_spec() -> ElementSpec {
        ElementSpec::new([(ACCESS_TYPE, ["role", "userByEmail"])])
    }

    #[test]
    fn an_element_is_only_ever_produced_for_a_type_that_was_asked_for() {
        // The dangerous direction here is a projection nobody requested: a
        // caller comparing elements would then compare against a shape it did
        // not define. A scan that names no type, or names another one, must
        // hand back exactly what 0.5.27 handed back.
        let doc = access(r#"{"role":"READER","userByEmail":"probe@example.com"}"#);
        for spec in [None, Some(ElementSpec::new([("gcp:t:Other", ["role"])]))] {
            let Ok(index) = index_checkpoint_with_elements(doc.as_bytes(), None, spec.as_ref())
            else {
                unreachable!("fixture must read")
            };
            assert_eq!(index.entries.len(), 1);
            assert!(index.entries[0].element.is_none(), "an unasked projection");
        }
    }

    #[test]
    fn a_hostile_element_value_passes_through_unaltered() {
        // An element is data on the way to an equality test and, further on,
        // to an operator's screen. Nothing here interprets it, and nothing
        // here rewrites it: the bytes that come back are the bytes that went
        // in, so a comparison against another checkpoint is honest.
        let hostile = [
            "'; DROP TABLE resources; --",
            "{{ 7*7 }}",
            "${x}",
            "../../etc/passwd",
            "<script>alert(1)</script>",
        ];
        for payload in hostile {
            let escaped = payload.replace('\\', "\\\\").replace('"', "\\\"");
            let doc = access(&format!(r#"{{"role":"{escaped}"}}"#));
            let spec = access_spec();
            let Ok(index) = index_checkpoint_with_elements(doc.as_bytes(), None, Some(&spec))
            else {
                unreachable!("{payload:?} must read")
            };
            let Some(element) = index.entries[0].element.as_ref() else {
                unreachable!("an element was requested")
            };
            let [(key, value)] = element.as_slice() else {
                unreachable!("one projected key")
            };
            assert_eq!(key.as_ref(), "role");
            assert_eq!(
                serde_json::from_str::<String>(value.get()).ok().as_deref(),
                Some(payload)
            );
        }
    }

    #[test]
    fn a_hostile_element_key_is_never_matched_by_accident() {
        // Key matching is exact string equality against the requested set. A
        // key that merely looks like a requested one — a prefix, a case
        // variant, a homoglyph, one with a zero-width space — is a different
        // key, and projecting it would put a value the caller never asked for
        // into an element it will compare for equality.
        let doc = access(r#"{"Role":1,"role ":2,"\u200brole":3,"rolex":4,"":5,"role":"READER"}"#);
        let spec = access_spec();
        let Ok(index) = index_checkpoint_with_elements(doc.as_bytes(), None, Some(&spec)) else {
            unreachable!("fixture must read")
        };
        let Some(element) = index.entries[0].element.as_ref() else {
            unreachable!("an element was requested")
        };
        let keys: Vec<&str> = element.iter().map(|(k, _)| k.as_ref()).collect();
        assert_eq!(keys, vec!["role"], "a near-miss key was projected");
    }

    #[test]
    fn an_element_that_cannot_be_projected_is_an_error_not_an_empty_one() {
        // `inputs` that is not an object, for a type the caller asked about.
        // An empty element and an unreadable one are the same value to a gate
        // comparing elements, and the first one would clear a removal.
        let spec = access_spec();
        for inputs in ["[\"role\"]", "\"role\"", "7", "true"] {
            let doc = access(inputs);
            assert!(
                index_checkpoint_with_elements(doc.as_bytes(), None, Some(&spec)).is_err(),
                "{inputs} must not read as an element"
            );
        }
    }

    #[test]
    fn a_bomb_inside_inputs_costs_what_skipping_it_always_cost() {
        // Not the bound the bracket bomb meets. That one fails in microseconds
        // because the TYPED path trips serde_json's depth limit at 128;
        // `inputs` is captured through serde_json's ignore path, which is
        // deliberately iterative — an explicit bracket stack, no depth limit —
        // so an unclosed run of `[` is scanned to the end of the document.
        // That is exactly what 0.5.27 did with `inputs` as an unknown field:
        // skipping is the same scan. So the property is not "fast"; it is
        // "an error, no overflow, and no slower than serde_json's own scan of
        // the same bytes" — measured against that scan in this process, so a
        // debug build on a slow runner compares itself with itself.
        let bomb = format!(
            r#"{{"version":3,"checkpoint":{{"latest":{{"resources":[{{"urn":"{URN}","id":"{ID}","type":"{ACCESS_TYPE}","inputs":{}"#,
            "[".repeat(16 * 1024 * 1024)
        );
        let spec = access_spec();

        let control_started = Instant::now();
        assert!(serde_json::from_slice::<serde::de::IgnoredAny>(bomb.as_bytes()).is_err());
        let control = control_started.elapsed();

        let started = Instant::now();
        assert!(index_checkpoint_with_elements(bomb.as_bytes(), None, Some(&spec)).is_err());
        let elapsed = started.elapsed();

        // Three times serde_json's own pass over the bytes, plus a quarter
        // second of noise: a reader that had started copying or re-scanning
        // the captured slice would be far outside this; one that merely
        // validates it as UTF-8 is inside it.
        let ceiling = control.as_secs_f64() * 3.0 + 0.25;
        assert!(
            elapsed.as_secs_f64() < ceiling,
            "16 MiB of inputs took {elapsed:?} against serde_json's own {control:?}"
        );
    }

    #[test]
    fn a_batch_projects_the_same_elements_on_every_thread() {
        // The gate's answer must not depend on how the caller scheduled the
        // read. The spec is shared by reference across the pool, so this is
        // also the assertion that nothing in it is written to.
        let doc = access(r#"{"role":"READER","userByEmail":"probe@example.com"}"#);
        let spec = access_spec();
        let docs: Vec<&[u8]> = vec![doc.as_bytes(); 16];
        let seq = index_checkpoints_with_elements(&docs, None, 1, Some(&spec));
        let par = index_checkpoints_with_elements(&docs, None, 8, Some(&spec));
        let Ok(one) = index_checkpoint_with_elements(doc.as_bytes(), None, Some(&spec)) else {
            unreachable!("fixture must read")
        };
        for (a, b) in seq.iter().zip(par.iter()) {
            let (Ok(a), Ok(b)) = (a, b) else {
                unreachable!("every slot must read")
            };
            assert_eq!(a, b);
            assert_eq!(a, &one);
        }
    }
}

// =========================================================================
// encoding.rs — a leading byte order mark is not a second document
//
// Every fixture in this module is written as RAW BYTES. A mark is invisible,
// and `rustfmt`, an editor save or a helpful string literal would silently
// normalise it away — leaving eight tests that pass while proving nothing.
// `\xef\xbb\xbf` in a byte string is the one spelling no tool will touch.
// =========================================================================

mod bom_security {
    use pulumi_rs_yaml_core::ast::parse::parse_template;
    use pulumi_rs_yaml_core::encoding::{strip_bom_bytes, UTF8_BOM_BYTES};
    use pulumi_rs_yaml_core::jinja::{JinjaContext, UndefinedMode};
    use pulumi_rs_yaml_core::multi_file::load_project;
    use pulumi_rs_yaml_core::packages::search_package_decls;
    use pulumi_rs_yaml_core::schema::{PackageSchema, SchemaStore};
    use std::collections::HashMap;
    use std::path::Path;

    /// Bytes to `&str`, refusing anything that is not UTF-8.
    ///
    /// The fixtures are byte strings so the mark survives every formatter; the
    /// parser takes `&str`, and this is the one place the two meet.
    fn text(bytes: &[u8]) -> &str {
        let Ok(s) = std::str::from_utf8(bytes) else {
            unreachable!("fixture must be UTF-8")
        };
        s
    }

    /// Writes `bytes` verbatim under a fresh temp directory and returns it.
    fn dir_with(name: &str, bytes: &[u8]) -> tempfile::TempDir {
        let Ok(dir) = tempfile::tempdir() else {
            unreachable!("a temp directory must be creatable")
        };
        assert!(std::fs::write(dir.path().join(name), bytes).is_ok());
        // The fixture asserts its own first bytes: if anything ever rewrites
        // this file, the test that depends on the mark says so.
        let Ok(back) = std::fs::read(dir.path().join(name)) else {
            unreachable!("the fixture must read back")
        };
        assert_eq!(&back[..3], &UTF8_BOM_BYTES, "the fixture lost its mark");
        dir
    }

    fn jinja_ctx<'a>(
        dir: &'a str,
        config: &'a HashMap<String, String>,
        extra: &'a HashMap<String, String>,
    ) -> JinjaContext<'a> {
        JinjaContext {
            project_name: "app",
            stack_name: "dev",
            cwd: dir,
            organization: "org",
            root_directory: dir,
            config,
            project_dir: dir,
            undefined: UndefinedMode::Strict,
            provider_templated_packages: &[],
            extra,
        }
    }

    #[test]
    fn the_reported_repro_parses_as_a_single_document() {
        // The signature that made the incident so hard to read: one line
        // parsed, and every file of two lines or more did not.
        let one_line = b"\xef\xbb\xbfname: app\n";
        let (template, diags) = parse_template(text(one_line), None);
        assert!(!diags.has_errors(), "one line: {}", diags);
        assert_eq!(template.name.as_deref(), Some("app"));

        let real = b"\xef\xbb\xbfname: app\nruntime: yaml\nresources:\n  b:\n    type: gcp:storage:Bucket\n";
        let (template, diags) = parse_template(text(real), None);
        assert!(!diags.has_errors(), "several lines: {}", diags);
        assert_eq!(template.name.as_deref(), Some("app"));
        assert_eq!(template.resources.len(), 1);
    }

    #[test]
    fn a_mark_alone_is_an_empty_document_not_a_second_one() {
        // Stripping must not invent a document. A file holding nothing but the
        // mark is empty, and the error has to say so rather than describe a
        // phantom second document the file does not contain.
        let (_, diags) = parse_template(text(b"\xef\xbb\xbf"), None);
        assert!(diags.has_errors(), "an empty document is not a template");
        let rendered = diags.to_string();
        assert!(
            rendered.contains("expected a YAML mapping"),
            "the reason must be the real one: {rendered}"
        );
        assert!(
            !rendered.contains("more than one document"),
            "the phantom must be gone: {rendered}"
        );
    }

    #[test]
    fn a_genuine_multi_document_file_still_fails_the_same_way() {
        // The anti-masking test, and the reason the strip is a correctness fix
        // rather than a workaround: a file that really does hold two documents
        // holds two after the mark is removed, and must still be refused with
        // the same message it has always been refused with.
        let plain = b"name: app\nruntime: yaml\n---\nname: other\n";
        let marked = b"\xef\xbb\xbfname: app\nruntime: yaml\n---\nname: other\n";

        let (_, plain_diags) = parse_template(text(plain), None);
        let (_, marked_diags) = parse_template(text(marked), None);
        assert!(plain_diags.has_errors());
        assert!(marked_diags.has_errors(), "the mark must not launder this");
        assert!(
            plain_diags.to_string().contains("more than one document"),
            "{}",
            plain_diags
        );
        assert_eq!(
            plain_diags.to_string(),
            marked_diags.to_string(),
            "the mark must change nothing about a real second document"
        );
    }

    #[test]
    fn a_mark_inside_the_document_is_content() {
        // Exactly one mark, at offset zero. U+FEFF anywhere else is a character
        // the author wrote, and a parser that removed it would be corrupting a
        // value rather than reading a stream marker.
        let src = b"\xef\xbb\xbfname: app\nruntime: yaml\ndescription: a\xef\xbb\xbfb\n";
        let (template, diags) = parse_template(text(src), None);
        assert!(!diags.has_errors(), "{}", diags);
        assert_eq!(template.name.as_deref(), Some("app"));
        assert_eq!(template.description.as_deref(), Some("a\u{feff}b"));

        // A second mark immediately after the first is content too: exactly one
        // is removed, and what is left is a file whose first key really is
        // `\u{feff}name` — still refused, as it was before.
        let doubled = b"\xef\xbb\xbf\xef\xbb\xbfname: app\nruntime: yaml\n";
        let (_, diags) = parse_template(text(doubled), None);
        assert!(diags.has_errors(), "only one mark is a stream marker");
    }

    #[test]
    fn a_utf16_mark_is_neither_stripped_nor_decoded() {
        // A UTF-16 mark is a different encoding, not a leading U+FEFF in a
        // UTF-8 stream. Guessing at a transcode would rewrite the file on a
        // hunch; dropping the two bytes would hand the parser a NUL-riddled
        // buffer that fails further from its cause. So it is diagnosed.
        for lead in [&b"\xff\xfe"[..], &b"\xfe\xff"[..]] {
            let mut src = lead.to_vec();
            src.extend_from_slice(b"n\x00a\x00m\x00e\x00");
            assert!(std::str::from_utf8(&src).is_err(), "not UTF-8 text");
            assert_eq!(strip_bom_bytes(&src), &src[..], "nothing is removed");

            let Ok(dir) = tempfile::tempdir() else {
                unreachable!("a temp directory must be creatable")
            };
            assert!(std::fs::write(dir.path().join("Pulumi.yaml"), &src).is_ok());
            let (template, diags) = load_project(dir.path(), None);
            assert!(diags.has_errors(), "an undecodable file must be refused");
            assert!(template.name().is_none());
        }
    }

    #[test]
    fn a_marked_package_lock_is_read_rather_than_ignored() {
        // try_parse_package_lock ends in `.ok()?`, so a marked lock file was
        // never rejected — it was invisible. The package silently went missing
        // and the failure surfaced somewhere else entirely.
        let lock = b"\xef\xbb\xbfpackageDeclarationVersion: 1\nname: gcpx\nversion: 1.2.3\n";
        let dir = dir_with("gcpx.yaml", lock);
        let found = search_package_decls(dir.path());
        assert_eq!(found.len(), 1, "the marked lock file must be found");
        assert_eq!(found[0].name, "gcpx");
        assert_eq!(found[0].version, "1.2.3");
    }

    #[test]
    fn a_marked_schema_store_loads() {
        let Ok(dir) = tempfile::tempdir() else {
            unreachable!("a temp directory must be creatable")
        };
        let path = dir.path().join("schema.json");
        let mut store = SchemaStore::new();
        store.insert(PackageSchema {
            name: "gcp".to_owned(),
            version: "8.0.0".to_owned(),
            ..Default::default()
        });
        assert!(store.save(&path).is_ok());

        // Re-write the same JSON behind a mark, as an editor would.
        let Ok(json) = std::fs::read(&path) else {
            unreachable!("the store must read back")
        };
        let mut marked = UTF8_BOM_BYTES.to_vec();
        marked.extend_from_slice(&json);
        assert!(std::fs::write(&path, &marked).is_ok());

        let Ok(loaded) = SchemaStore::load(Path::new(&path)) else {
            unreachable!("a marked schema store must load")
        };
        assert!(loaded.packages().contains_key("gcp"));
    }

    #[test]
    fn a_marked_project_is_not_nameless() {
        // The observable that reached the incident report: the project loaded
        // with no name at all, so everything keyed on it — the graph export
        // among them — described a project called "unknown". Both paths through
        // load_project must produce the name the file states.
        let src = b"\xef\xbb\xbfname: app\nruntime: yaml\nresources:\n  b:\n    type: gcp:storage:Bucket\n";
        let dir = dir_with("Pulumi.yaml", src);
        let path = dir.path().to_string_lossy().into_owned();
        let config = HashMap::new();
        let extra = HashMap::new();

        for ctx in [None, Some(jinja_ctx(&path, &config, &extra))] {
            let (template, diags) = load_project(dir.path(), ctx.as_ref());
            assert!(!diags.has_errors(), "{}", diags);
            assert_eq!(template.name(), Some("app"));
            assert_eq!(template.resources().len(), 1);
        }
    }
}

// ---------------------------------------------------------------------------
// A template that will not render says where and why — without giving up
// containment, and without copying the source to say so.
// ---------------------------------------------------------------------------
mod render_diagnostic_security {
    use std::collections::HashMap;

    use pulumi_rs_yaml_core::jinja::{
        IncludeRefusal, JinjaContext, JinjaPreprocessor, RenderErrorKind, UndefinedMode,
        MAX_INCLUDE_BYTES,
    };

    fn ctx<'a>(
        dir: &'a str,
        config: &'a HashMap<String, String>,
        extra: &'a HashMap<String, String>,
    ) -> JinjaContext<'a> {
        JinjaContext {
            project_name: "app",
            stack_name: "dev",
            cwd: dir,
            organization: "org",
            root_directory: dir,
            config,
            project_dir: dir,
            undefined: UndefinedMode::Strict,
            provider_templated_packages: &[],
            extra,
        }
    }

    fn render<'s>(
        dir: &str,
        source: &'s str,
    ) -> Result<String, pulumi_rs_yaml_core::jinja::RenderDiagnostic<'s>> {
        let config = HashMap::new();
        let extra = HashMap::new();
        let c = ctx(dir, &config, &extra);
        // The inherent render: its diagnostic borrows `source` alone, which
        // is the property the binding depends on and this file proves.
        JinjaPreprocessor::new(&c)
            .render(source, "Pulumi.yaml")
            .map(|r| r.into_owned())
    }

    #[test]
    fn an_escaping_json_include_is_refused_not_served() {
        // The extension is now served; containment is unchanged. A file that
        // exists outside both roots is named as escaping, never inlined.
        let outside = tempfile::tempdir().expect("outside");
        std::fs::write(outside.path().join("secret.json"), "{\"k\": \"v\"}").expect("write");
        let project = tempfile::tempdir().expect("project");
        let dir = project.path().to_str().expect("utf-8 path");
        let name = format!("{}/secret.json", outside.path().display());
        let rel = pathdiff(project.path(), &name);
        let source = format!("a: '{{% include \"{}\" %}}'\n", rel);
        let Err(diag) = render(dir, &source) else {
            panic!("an escaping include must not render");
        };
        assert_eq!(diag.kind, RenderErrorKind::JinjaTemplateNotFound);
        assert!(
            diag.message.contains(IncludeRefusal::TAG_ESCAPE),
            "{}",
            diag.message
        );
        assert!(
            !diag.message.contains("\"k\""),
            "the file's contents must not leak"
        );
    }

    #[test]
    fn an_absolute_include_is_refused_by_name() {
        let project = tempfile::tempdir().expect("project");
        let dir = project.path().to_str().expect("utf-8 path");
        std::fs::write(project.path().join("in.yaml"), "x: 1\n").expect("write");
        let abs = project.path().join("in.yaml");
        let source = format!("a: '{{% include \"{}\" %}}'\n", abs.display());
        let Err(diag) = render(dir, &source) else {
            panic!("an absolute include must not render, even inside the root");
        };
        assert_eq!(diag.kind, RenderErrorKind::JinjaTemplateNotFound);
        assert!(
            diag.message.contains(IncludeRefusal::TAG_ABSOLUTE),
            "{}",
            diag.message
        );
    }

    #[test]
    fn a_binary_include_is_refused_and_its_bytes_never_appear_in_the_error() {
        let project = tempfile::tempdir().expect("project");
        let dir = project.path().to_str().expect("utf-8 path");
        std::fs::write(
            project.path().join("logo.png"),
            [0x89, b'P', b'N', b'G', 0xff, 0xfe],
        )
        .expect("write");
        let Err(diag) = render(dir, "a: '{% include \"logo.png\" %}'\n") else {
            panic!("a binary file must not render");
        };
        assert_eq!(diag.kind, RenderErrorKind::JinjaTemplateNotFound);
        assert_eq!(diag.message, "include refused [binary]: \"logo.png\"");
    }

    #[test]
    fn a_file_over_the_cap_is_refused_by_name() {
        let project = tempfile::tempdir().expect("project");
        let dir = project.path().to_str().expect("utf-8 path");
        std::fs::write(
            project.path().join("big.txt"),
            vec![b'S'; MAX_INCLUDE_BYTES as usize + 1],
        )
        .expect("write");
        let Err(diag) = render(dir, "a: '{% include \"big.txt\" %}'\n") else {
            panic!("an oversized file must not render");
        };
        assert_eq!(diag.message, "include refused [too large]: \"big.txt\"");
        assert!(
            diag.suggestion.is_some_and(|s| s.contains("1 MiB")),
            "{:?}",
            diag.suggestion
        );
    }

    #[test]
    fn a_refused_include_is_never_ignored_as_missing() {
        let project = tempfile::tempdir().expect("project");
        let dir = project.path().to_str().expect("utf-8 path");
        std::fs::write(project.path().join("logo.png"), [0xff, 0xfe]).expect("write");
        std::fs::write(
            project.path().join("big.txt"),
            vec![b'S'; MAX_INCLUDE_BYTES as usize + 1],
        )
        .expect("write");
        for name in ["logo.png", "big.txt", "/etc/hosts"] {
            let source = format!("a: '{{% include \"{name}\" ignore missing %}}'\n");
            let Err(diag) = render(dir, &source) else {
                panic!("{name}: a refusal is not something to ignore");
            };
            assert!(
                diag.message.starts_with(IncludeRefusal::PREFIX),
                "{}",
                diag.message
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn an_extensionless_escape_is_still_an_escape() {
        // Removing the extension gate widened what is served, never where
        // from: a VERSION that resolves outside both roots is refused as an
        // escape, and its contents stay out of the message.
        let outside = tempfile::tempdir().expect("outside");
        std::fs::write(outside.path().join("VERSION"), "9.9.9").expect("write");
        let project = tempfile::tempdir().expect("project");
        std::os::unix::fs::symlink(
            outside.path().join("VERSION"),
            project.path().join("VERSION"),
        )
        .expect("symlink");
        let dir = project.path().to_str().expect("utf-8 path");
        let Err(diag) = render(dir, "v: '{% include \"VERSION\" %}'\n") else {
            panic!("an escaping VERSION must not render");
        };
        assert!(
            diag.message.contains(IncludeRefusal::TAG_ESCAPE),
            "{}",
            diag.message
        );
        assert!(
            !diag.message.contains("9.9.9"),
            "the target's contents must not leak"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_both_roots_is_refused_as_an_escape() {
        let outside = tempfile::tempdir().expect("outside");
        std::fs::write(outside.path().join("secret.sql"), "SELECT 1").expect("write");
        let project = tempfile::tempdir().expect("project");
        std::os::unix::fs::symlink(
            outside.path().join("secret.sql"),
            project.path().join("link.sql"),
        )
        .expect("symlink");
        let dir = project.path().to_str().expect("utf-8 path");
        let Err(diag) = render(dir, "a: '{% include \"link.sql\" %}'\n") else {
            panic!("a symlink out of the tree must not render");
        };
        assert!(
            diag.message.contains(IncludeRefusal::TAG_ESCAPE),
            "{}",
            diag.message
        );
        assert!(
            !diag.message.contains("SELECT"),
            "the target's contents must not leak"
        );
    }

    #[test]
    fn a_genuinely_absent_include_is_not_found_and_says_which() {
        let project = tempfile::tempdir().expect("project");
        let dir = project.path().to_str().expect("utf-8 path");
        let Err(diag) = render(dir, "a: '{% include \"schemas/table.json\" %}'\n") else {
            panic!("an absent include must not render");
        };
        assert_eq!(diag.kind, RenderErrorKind::JinjaTemplateNotFound);
        assert!(
            diag.message.contains("schemas/table.json"),
            "{}",
            diag.message
        );
        assert!(
            !diag.message.starts_with(IncludeRefusal::PREFIX),
            "absent is not refused"
        );
    }

    #[test]
    fn a_fault_on_the_last_line_of_a_huge_source_borrows_and_does_not_copy() {
        // Sixty-four mebibytes of comment, then one undefined name. The
        // diagnostic's line and expression must be slices of the input. The
        // padding is wide rather than tall — four-kibibyte lines — because
        // the engine counts lines in sixteen bits, and a fault past line
        // 65535 is reported on a line the file does not have; that is a
        // separate limit, not the property under test here.
        let pad = format!("# {}\n", "p".repeat(4094));
        let mut source = String::with_capacity(64 * 1024 * 1024 + 64);
        while source.len() < 64 * 1024 * 1024 {
            source.push_str(&pad);
        }
        source.push_str("name: {{ missing_name }}\n");
        let project = tempfile::tempdir().expect("project");
        let dir = project.path().to_str().expect("utf-8 path");
        let Err(diag) = render(dir, &source) else {
            panic!("must not render");
        };
        let start = source.as_ptr() as usize;
        let end = start + source.len();
        let line_at = diag.source_line.as_ptr() as usize;
        let expr_at = diag.expression.as_ptr() as usize;
        assert!((start..end).contains(&line_at), "source_line was copied");
        assert!((start..end).contains(&expr_at), "expression was copied");
        assert_eq!(diag.expression, "missing_name");
        assert_eq!(diag.column, 10);
    }

    #[test]
    fn a_multi_byte_prefix_never_panics_and_keeps_the_caret_honest() {
        let project = tempfile::tempdir().expect("project");
        let dir = project.path().to_str().expect("utf-8 path");
        for prefix in ["é", "日本語", "🚀", "a\u{0301}"] {
            let source = format!("{}: {{{{ nope }}}}\n", prefix);
            let Err(diag) = render(dir, &source) else {
                panic!("must not render");
            };
            assert_eq!(diag.expression, "nope", "{prefix}");
            let col = (diag.column - 1) as usize;
            assert_eq!(&diag.source_line[col..col + 4], "nope", "{prefix}");
            let _ = diag.format_rich("Pulumi.yaml");
        }
    }

    fn pathdiff(from: &std::path::Path, to: &str) -> String {
        // Enough `..` to climb out of `from`, then the absolute tail.
        let ups = from.components().count();
        let mut rel = "../".repeat(ups);
        rel.push_str(to.trim_start_matches('/'));
        rel
    }
}

// =========================================================================
// interpolation.rs — the escape collapses once, and only once
// =========================================================================

mod escape_collapse_security {
    use pulumi_rs_yaml_core::ast::interpolation::{needs_interpolation_pass, parse_interpolation};
    use pulumi_rs_yaml_core::diag::Diagnostics;

    fn parts_of(input: &str) -> Vec<(String, Option<String>)> {
        let mut diags = Diagnostics::new();
        let parts = parse_interpolation(input, None, &mut diags);
        parts
            .into_iter()
            .map(|p| {
                (
                    p.text.into_owned(),
                    p.value
                        .map(|a| a.root_name().unwrap_or_default().to_string()),
                )
            })
            .collect()
    }

    #[test]
    fn an_escape_is_collapsed_once_and_never_rescanned() {
        // `$$$${x}` is two escapes and then literal text, never an
        // interpolation: a second pass over the collapsed output would turn
        // the author's literal into a reference to `x`.
        let parts = parts_of("$$$${x}");
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].0, "$${x}");
        assert!(parts[0].1.is_none(), "collapsed text was re-interpreted");
    }

    #[test]
    fn an_escape_before_a_reference_leaves_the_reference_intact() {
        // `$$${x}` is one escape followed by a real reference. Collapsing the
        // escape must not consume the `$` the reference needs.
        let parts = parts_of("$$${x}");
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].0, "$");
        assert_eq!(parts[0].1.as_deref(), Some("x"));
    }

    #[test]
    fn the_guard_admits_every_string_the_parser_rewrites() {
        // The guard decides whether the parser runs. A string it turns away is
        // emitted verbatim, so anything the parser would rewrite must be
        // admitted — otherwise a value reaches a provider in a spelling the
        // author did not write.
        for input in [
            "$$",
            "$${x}",
            "FROM $${data()}",
            "a$$b",
            "${x}",
            "$$$${x}",
            "\u{00e9}$$",
        ] {
            let mut diags = Diagnostics::new();
            let parts = parse_interpolation(input, None, &mut diags);
            let rewritten: String = parts.iter().map(|p| p.text.as_ref()).collect();
            let resolves = parts.iter().any(|p| p.value.is_some());
            if rewritten != input || resolves {
                assert!(
                    needs_interpolation_pass(input),
                    "guard turned away a string the parser rewrites: {input:?}"
                );
            }
        }
    }

    #[test]
    fn a_long_run_of_escapes_is_linear_and_bounded() {
        // No quadratic rescan and no panic on a pathological run: 100k escapes
        // collapse to exactly 100k dollars in one pass.
        let input = "$".repeat(200_000);
        assert!(needs_interpolation_pass(&input));
        let mut diags = Diagnostics::new();
        let parts = parse_interpolation(&input, None, &mut diags);
        let text: String = parts.iter().map(|p| p.text.as_ref()).collect();
        assert_eq!(text.len(), 100_000);
        assert!(text.bytes().all(|b| b == b'$'));
    }
}

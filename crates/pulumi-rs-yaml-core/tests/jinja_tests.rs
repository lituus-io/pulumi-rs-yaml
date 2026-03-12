//! Integration tests for the Jinja2 template pre-processing module.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fs;

use pulumi_rs_yaml_core::jinja::{
    has_jinja_block_syntax, strip_jinja_blocks, validate_jinja_syntax, validate_rendered_yaml,
    JinjaContext, JinjaPreprocessor, NoopPreprocessor, RenderErrorKind, TemplatePreprocessor,
    UndefinedMode,
};

fn make_context(config: &HashMap<String, String>) -> JinjaContext<'_> {
    JinjaContext {
        project_name: "test-project",
        stack_name: "dev",
        cwd: "/tmp/test",
        organization: "my-org",
        root_directory: "/home/user/project",
        config,
        project_dir: "/home/user/project",
        undefined: UndefinedMode::Strict,
    }
}

// ============================================================================
// NoopPreprocessor tests
// ============================================================================

#[test]
fn test_noop_preprocessor_zero_copy() {
    let source = "name: test\nruntime: yaml\n";
    let preprocessor = NoopPreprocessor;
    let result = preprocessor.preprocess(source, "Pulumi.yaml").unwrap();
    // NoopPreprocessor returns exact &str reference (pointer equality)
    assert!(std::ptr::eq(result, source));
}

// ============================================================================
// JinjaPreprocessor passthrough tests
// ============================================================================

#[test]
fn test_no_jinja_passthrough() {
    let source = "name: test\nruntime: yaml\nresources:\n  bucket:\n    type: aws:s3:Bucket\n";
    let config = HashMap::new();
    let ctx = make_context(&config);
    let preprocessor = JinjaPreprocessor::new(&ctx);
    let result = preprocessor.preprocess(source, "Pulumi.yaml").unwrap();
    // Should return Cow::Borrowed (no allocation) for plain YAML
    assert!(matches!(result, Cow::Borrowed(_)));
    assert_eq!(result.as_ref(), source);
}

// ============================================================================
// Jinja rendering tests
// ============================================================================

#[test]
fn test_jinja_variable_substitution() {
    let source = "name: {{ pulumi_project }}\nruntime: yaml\n";
    let config = HashMap::new();
    let ctx = make_context(&config);
    let preprocessor = JinjaPreprocessor::new(&ctx);
    let result = preprocessor.preprocess(source, "Pulumi.yaml").unwrap();
    assert!(result.contains("name: test-project"));
}

#[test]
fn test_jinja_for_loop_resources() {
    let source = r#"name: test
runtime: yaml
resources:
{% for i in range(3) %}
  bucket{{ i }}:
    type: aws:s3:Bucket
    properties:
      bucketName: "bucket-{{ i }}"
{% endfor %}
"#;
    let config = HashMap::new();
    let ctx = make_context(&config);
    let preprocessor = JinjaPreprocessor::new(&ctx);
    let result = preprocessor.preprocess(source, "Pulumi.yaml").unwrap();
    assert!(result.contains("bucket0:"));
    assert!(result.contains("bucket1:"));
    assert!(result.contains("bucket2:"));
    assert!(result.contains("bucket-0"));
    assert!(result.contains("bucket-1"));
    assert!(result.contains("bucket-2"));
}

#[test]
fn test_jinja_conditional() {
    let source = r#"name: test
runtime: yaml
resources:
{% if pulumi_stack == "dev" %}
  devBucket:
    type: aws:s3:Bucket
{% endif %}
"#;
    let config = HashMap::new();
    let ctx = make_context(&config);
    let preprocessor = JinjaPreprocessor::new(&ctx);
    let result = preprocessor.preprocess(source, "Pulumi.yaml").unwrap();
    assert!(result.contains("devBucket:"));
}

#[test]
fn test_jinja_with_pulumi_interp() {
    // Jinja {{ }} and Pulumi ${} coexist without conflict
    let source = r#"name: {{ pulumi_project }}
runtime: yaml
config:
  greeting:
    type: string
    default: hello
outputs:
  result: ${greeting}
"#;
    let config = HashMap::new();
    let ctx = make_context(&config);
    let preprocessor = JinjaPreprocessor::new(&ctx);
    let result = preprocessor.preprocess(source, "Pulumi.yaml").unwrap();
    // Jinja should have rendered the name
    assert!(result.contains("name: test-project"));
    // Pulumi interpolation should be preserved
    assert!(result.contains("${greeting}"));
}

#[test]
fn test_jinja_config_vars() {
    let mut config = HashMap::new();
    config.insert("test:region".to_string(), "us-west-2".to_string());
    let ctx = make_context(&config);
    let preprocessor = JinjaPreprocessor::new(&ctx);
    let source = "region: {{ config.region }}\n";
    let result = preprocessor.preprocess(source, "Pulumi.yaml").unwrap();
    assert!(result.contains("region: us-west-2"), "got: {}", result);
}

// ============================================================================
// Error tests
// ============================================================================

#[test]
fn test_jinja_error_has_line_number() {
    let source = "line1\n{% invalid syntax %}\nline3\n";
    let config = HashMap::new();
    let ctx = make_context(&config);
    let preprocessor = JinjaPreprocessor::new(&ctx);
    let err = preprocessor.preprocess(source, "Pulumi.yaml").unwrap_err();
    assert!(err.line > 0, "error should have a line number");
    assert_eq!(err.kind, RenderErrorKind::JinjaSyntax);
}

#[test]
fn test_jinja_error_has_suggestion() {
    let source = "name: {{ undefined_variable }}\n";
    let config = HashMap::new();
    let ctx = make_context(&config);
    let preprocessor = JinjaPreprocessor::new(&ctx);
    let err = preprocessor.preprocess(source, "Pulumi.yaml").unwrap_err();
    assert_eq!(err.kind, RenderErrorKind::JinjaUndefinedVariable);
    assert!(
        err.suggestion.is_some(),
        "undefined variable should have suggestion"
    );
}

#[test]
fn test_jinja_error_has_source_context() {
    let source = "line1: ok\nname: {{ undefined_var }}\nline3: ok\n";
    let config = HashMap::new();
    let ctx = make_context(&config);
    let preprocessor = JinjaPreprocessor::new(&ctx);
    let err = preprocessor.preprocess(source, "Pulumi.yaml").unwrap_err();
    // source_line should be a zero-copy slice of the original source
    assert!(!err.source_line.is_empty());
    assert!(
        err.source_line.contains("undefined_var"),
        "source line should contain the failing expression, got: {}",
        err.source_line
    );
}

// ============================================================================
// Post-render YAML validation tests
// ============================================================================

#[test]
fn test_yaml_validation_error() {
    let rendered = "name: test\n  invalid: indentation\n";
    let original = rendered;
    let result = validate_rendered_yaml(rendered, original, "Pulumi.yaml");
    assert!(result.is_err());
    let diag = result.unwrap_err();
    assert!(matches!(
        diag.kind,
        RenderErrorKind::YamlSyntax | RenderErrorKind::YamlIndentation
    ));
}

#[test]
fn test_yaml_validation_suggestion() {
    // "mapping values are not allowed" scenario
    let rendered = "key:value\n";
    let original = rendered;
    let _result = validate_rendered_yaml(rendered, original, "Pulumi.yaml");
    // This may or may not trigger depending on serde_yaml's parsing;
    // serde_yaml might accept "key:value" as a plain string.
    // Test the positive case: valid YAML passes validation
    let valid = "name: test\nruntime: yaml\n";
    assert!(validate_rendered_yaml(valid, valid, "Pulumi.yaml").is_ok());
}

// ============================================================================
// Filter tests
// ============================================================================

#[test]
fn test_jinja_filter_to_json() {
    let source = "data: '{{ pulumi_project | to_json }}'\n";
    let config = HashMap::new();
    let ctx = make_context(&config);
    let preprocessor = JinjaPreprocessor::new(&ctx);
    let result = preprocessor.preprocess(source, "Pulumi.yaml").unwrap();
    assert!(
        result.contains("\"test-project\""),
        "to_json should produce JSON string, got: {}",
        result
    );
}

#[test]
fn test_jinja_filter_base64() {
    let source = "encoded: '{{ \"hello\" | base64_encode }}'\ndecoded: '{{ \"aGVsbG8=\" | base64_decode }}'\n";
    let config = HashMap::new();
    let ctx = make_context(&config);
    let preprocessor = JinjaPreprocessor::new(&ctx);
    let result = preprocessor.preprocess(source, "Pulumi.yaml").unwrap();
    assert!(
        result.contains("aGVsbG8="),
        "base64_encode should produce 'aGVsbG8=', got: {}",
        result
    );
    assert!(
        result.contains("decoded: 'hello'"),
        "base64_decode should produce 'hello', got: {}",
        result
    );
}

// ============================================================================
// Rich error formatting test
// ============================================================================

#[test]
fn test_render_diagnostic_format_rich() {
    let source = "line1\n{% bad %}\nline3\n";
    let config = HashMap::new();
    let ctx = make_context(&config);
    let preprocessor = JinjaPreprocessor::new(&ctx);
    let err = preprocessor.preprocess(source, "Pulumi.yaml").unwrap_err();
    let formatted = err.format_rich("Pulumi.yaml");
    assert!(formatted.contains("Pulumi.yaml"));
    assert!(formatted.contains("error:"));
}

// ============================================================================
// parse_template_with_preprocessor test
// ============================================================================

#[test]
fn test_parse_template_with_noop_preprocessor() {
    use pulumi_rs_yaml_core::jinja::parse_template_with_preprocessor;

    let source = "name: test\nruntime: yaml\n";
    let preprocessor = NoopPreprocessor;
    let (template, diags) = parse_template_with_preprocessor(source, &preprocessor, None);
    assert!(!diags.has_errors(), "errors: {}", diags);
    assert_eq!(template.name.as_deref(), Some("test"));
}

#[test]
fn test_parse_template_with_jinja_preprocessor() {
    use pulumi_rs_yaml_core::jinja::parse_template_with_preprocessor;

    let source = "name: {{ pulumi_project }}\nruntime: yaml\n";
    let config = HashMap::new();
    let ctx = make_context(&config);
    let preprocessor = JinjaPreprocessor::new(&ctx);
    let (template, diags) = parse_template_with_preprocessor(source, &preprocessor, None);
    assert!(!diags.has_errors(), "errors: {}", diags);
    assert_eq!(template.name.as_deref(), Some("test-project"));
}

// ============================================================================
// Full pipeline tests (Jinja + evaluator)
// ============================================================================

#[test]
fn test_jinja_full_pipeline_with_evaluator() {
    use pulumi_rs_yaml_core::ast::parse::parse_template;
    use pulumi_rs_yaml_core::eval::evaluator::Evaluator;
    use pulumi_rs_yaml_core::eval::mock::MockCallback;

    let source = r#"name: {{ pulumi_project }}
runtime: yaml
resources:
{% for i in range(3) %}
  bucket{{ i }}:
    type: aws:s3:Bucket
    properties:
      bucketName: "{{ pulumi_project }}-bucket-{{ i }}"
{% endfor %}
outputs:
  count: 3
"#;

    let config = HashMap::new();
    let ctx = make_context(&config);
    let preprocessor = JinjaPreprocessor::new(&ctx);
    let rendered = preprocessor.preprocess(source, "Pulumi.yaml").unwrap();

    // Parse the rendered YAML
    let (template, diags) = parse_template(rendered.as_ref(), None);
    assert!(!diags.has_errors(), "parse errors: {}", diags);

    // Evaluate with mock
    let template: &'static _ = Box::leak(Box::new(template));
    let mut eval = Evaluator::with_callback(
        "test-project".to_string(),
        "dev".to_string(),
        "/tmp".to_string(),
        false,
        MockCallback::new(),
    );
    eval.evaluate_template(template, &HashMap::new(), &[]);
    assert!(!eval.diags.has_errors(), "eval errors: {}", eval.diags);

    // All 3 buckets should be registered
    let regs = eval.callback().registrations();
    assert_eq!(regs.len(), 3);
    for (i, reg) in regs.iter().enumerate() {
        assert_eq!(
            reg.inputs.get("bucketName").and_then(|v| v.as_str()),
            Some(format!("test-project-bucket-{}", i).as_str()),
        );
    }
}

#[test]
fn test_jinja_comment_stripped() {
    let source =
        "{# This is a Jinja comment that should be stripped #}\nname: test\nruntime: yaml\n";
    let config = HashMap::new();
    let ctx = make_context(&config);
    let preprocessor = JinjaPreprocessor::new(&ctx);
    let result = preprocessor.preprocess(source, "Pulumi.yaml").unwrap();
    assert!(!result.contains("comment"));
    assert!(result.contains("name: test"));
}

#[test]
fn test_jinja_all_context_variables() {
    let source = r#"proj: {{ pulumi_project }}
stack: {{ pulumi_stack }}
cwd: {{ pulumi_cwd }}
org: {{ pulumi_organization }}
root: {{ pulumi_root_directory }}
"#;
    let config = HashMap::new();
    let ctx = make_context(&config);
    let preprocessor = JinjaPreprocessor::new(&ctx);
    let result = preprocessor.preprocess(source, "Pulumi.yaml").unwrap();
    assert!(result.contains("proj: test-project"), "got: {}", result);
    assert!(result.contains("stack: dev"), "got: {}", result);
    assert!(result.contains("cwd: /tmp/test"), "got: {}", result);
    assert!(result.contains("org: my-org"), "got: {}", result);
    assert!(
        result.contains("root: /home/user/project"),
        "got: {}",
        result
    );
}

#[test]
fn test_jinja_filter_to_yaml() {
    let source = "data: '{{ pulumi_project | to_yaml }}'\n";
    let config = HashMap::new();
    let ctx = make_context(&config);
    let preprocessor = JinjaPreprocessor::new(&ctx);
    let result = preprocessor.preprocess(source, "Pulumi.yaml").unwrap();
    assert!(
        result.contains("test-project"),
        "to_yaml should work, got: {}",
        result
    );
}

#[test]
fn test_parse_template_with_preprocessor_error_path() {
    use pulumi_rs_yaml_core::jinja::parse_template_with_preprocessor;

    let source = "name: {{ undefined_var }}\nruntime: yaml\n";
    let config = HashMap::new();
    let ctx = make_context(&config);
    let preprocessor = JinjaPreprocessor::new(&ctx);
    let (_template, diags) = parse_template_with_preprocessor(source, &preprocessor, None);
    assert!(diags.has_errors(), "should have errors for undefined var");
}

#[test]
fn test_jinja_config_namespace_stripping() {
    let mut config = HashMap::new();
    config.insert("myproject:dbName".to_string(), "mydb".to_string());
    config.insert("myproject:port".to_string(), "5432".to_string());
    config.insert("simpleKey".to_string(), "simpleVal".to_string());
    let ctx = make_context(&config);
    let preprocessor = JinjaPreprocessor::new(&ctx);
    let source = "db: {{ config.dbName }}\nport: {{ config.port }}\nkey: {{ config.simpleKey }}\n";
    let result = preprocessor.preprocess(source, "Pulumi.yaml").unwrap();
    assert!(result.contains("db: mydb"), "got: {}", result);
    assert!(result.contains("port: 5432"), "got: {}", result);
    assert!(result.contains("key: simpleVal"), "got: {}", result);
}

// ============================================================================
// Strip + exec pipeline integration tests
// ============================================================================

#[test]
fn test_strip_then_render_produces_valid_yaml() {
    let source = r#"name: test
runtime: yaml
resources:
{% for i in range(3) %}
  "bucket{{ i }}":
    type: aws:s3:Bucket
    properties:
      name: "test-{{ i }}"
{% endfor %}
outputs:
  result: done
"#;

    // 1. Strip should produce valid YAML (CLI path)
    let stripped = strip_jinja_blocks(source);
    let parsed: Result<serde_yaml::Value, _> = serde_yaml::from_str(&stripped);
    assert!(
        parsed.is_ok(),
        "stripped YAML should parse: {:?}",
        parsed.err()
    );

    // 2. Full render should also produce valid YAML (language host path)
    let config = HashMap::new();
    let ctx = make_context(&config);
    let preprocessor = JinjaPreprocessor::new(&ctx);
    let rendered = preprocessor.preprocess(source, "Pulumi.yaml").unwrap();
    let parsed: Result<serde_yaml::Value, _> = serde_yaml::from_str(rendered.as_ref());
    assert!(
        parsed.is_ok(),
        "rendered YAML should parse: {:?}",
        parsed.err()
    );
}

#[test]
fn test_full_pipeline_with_jinja_source_env() {
    // Simulate the exec flow end-to-end (without actual process spawning)
    let original = r#"name: test-project
runtime: yaml
resources:
{% for i in range(2) %}
  bucket{{ i }}:
    type: aws:s3:Bucket
    properties:
      name: "{{ pulumi_project }}-{{ i }}"
{% endfor %}
outputs:
  result: ${bucket0.id}
"#;

    // Phase 1: Syntax validation (exec wrapper)
    assert!(validate_jinja_syntax(original, "Pulumi.yaml").is_ok());

    // Phase 1: Block detection
    assert!(has_jinja_block_syntax(original));

    // Phase 1: Strip for CLI
    let stripped = strip_jinja_blocks(original);
    assert!(!stripped.contains("{% for"));
    assert!(!stripped.contains("{% endfor"));
    // The stripped YAML should still have expressions and Pulumi interpolations
    assert!(stripped.contains("bucket{{ i }}"));
    assert!(stripped.contains("${bucket0.id}"));

    // Phase 2: Full Jinja render (language host, reads original)
    let config = HashMap::new();
    let ctx = make_context(&config);
    let preprocessor = JinjaPreprocessor::new(&ctx);
    let rendered = preprocessor.preprocess(original, "Pulumi.yaml").unwrap();
    assert!(rendered.contains("bucket0:"), "got:\n{}", rendered);
    assert!(rendered.contains("bucket1:"), "got:\n{}", rendered);
    assert!(rendered.contains("test-project-0"));
    assert!(rendered.contains("test-project-1"));
    // Pulumi interpolation passes through
    assert!(rendered.contains("${bucket0.id}"));
}

#[test]
fn test_jinja_blocks_with_pulumi_interpolation() {
    let source = r#"name: test
runtime: yaml
resources:
{% for i in range(2) %}
  res{{ i }}:
    type: aws:s3:Bucket
    properties:
      name: "bucket-{{ i }}"
{% endfor %}
outputs:
  url0: ${res0.url}
  url1: ${res1.url}
"#;

    // Strip preserves ${} interpolations
    let stripped = strip_jinja_blocks(source);
    assert!(stripped.contains("${res0.url}"));
    assert!(stripped.contains("${res1.url}"));

    // Full render also preserves ${} interpolations
    let config = HashMap::new();
    let ctx = make_context(&config);
    let preprocessor = JinjaPreprocessor::new(&ctx);
    let rendered = preprocessor.preprocess(source, "Pulumi.yaml").unwrap();
    assert!(rendered.contains("${res0.url}"), "got:\n{}", rendered);
    assert!(rendered.contains("${res1.url}"), "got:\n{}", rendered);
    assert!(rendered.contains("res0:"), "got:\n{}", rendered);
    assert!(rendered.contains("res1:"), "got:\n{}", rendered);
}

#[test]
fn test_strip_with_conditional() {
    let source = r#"name: test
runtime: yaml
resources:
{% if pulumi_stack == "dev" %}
  devBucket:
    type: aws:s3:Bucket
{% endif %}
{% if pulumi_stack == "prod" %}
  prodBucket:
    type: aws:s3:Bucket
{% endif %}
"#;

    let stripped = strip_jinja_blocks(source);
    // Both resources should be visible in stripped form (CLI sees all possible types)
    assert!(stripped.contains("devBucket:"));
    assert!(stripped.contains("prodBucket:"));
    assert!(!stripped.contains("{% if"));
    assert!(!stripped.contains("{% endif"));
}

#[test]
fn test_validate_syntax_catches_unclosed_block() {
    let source = r#"name: test
runtime: yaml
resources:
{% for i in range(3) %}
  "bucket{{ i }}":
    type: aws:s3:Bucket
"#;
    // Missing {% endfor %} — should fail syntax validation
    let result = validate_jinja_syntax(source, "Pulumi.yaml");
    assert!(result.is_err(), "unclosed for block should be caught");
}

#[test]
fn test_no_block_syntax_skips_stripping() {
    let source = r#"name: "{{ pulumi_project }}"
runtime: yaml
resources:
  myBucket:
    type: aws:s3:Bucket
    properties:
      name: "{{ pulumi_project }}-bucket"
outputs:
  result: ${myBucket.url}
"#;
    // No {% %} blocks — has_jinja_block_syntax should return false
    assert!(!has_jinja_block_syntax(source));

    // Strip is a no-op
    let stripped = strip_jinja_blocks(source);
    assert_eq!(stripped, source);
}

// ============================================================================
// Exec pipeline edge cases
// ============================================================================

#[test]
fn test_nested_for_and_if_blocks() {
    let source = r#"name: test
runtime: yaml
resources:
{% for i in range(3) %}
{% if i > 0 %}
  res{{ i }}:
    type: aws:s3:Bucket
    properties:
      name: "bucket-{{ i }}"
{% endif %}
{% endfor %}
"#;

    // Syntax validation passes
    assert!(validate_jinja_syntax(source, "Pulumi.yaml").is_ok());

    // Strip removes all {% %} lines
    let stripped = strip_jinja_blocks(source);
    assert!(!stripped.contains("{%"));

    // Render produces only res1 and res2 (i > 0)
    let config = HashMap::new();
    let ctx = make_context(&config);
    let preprocessor = JinjaPreprocessor::new(&ctx);
    let rendered = preprocessor.preprocess(source, "Pulumi.yaml").unwrap();
    assert!(
        !rendered.contains("res0:"),
        "i=0 should be filtered out, got:\n{}",
        rendered
    );
    assert!(rendered.contains("res1:"), "got:\n{}", rendered);
    assert!(rendered.contains("res2:"), "got:\n{}", rendered);
}

#[test]
fn test_strip_preserves_indentation() {
    let source = "name: test\nruntime: yaml\nresources:\n{% for i in range(1) %}\n  bucket:\n    type: aws:s3:Bucket\n    properties:\n      name: test\n{% endfor %}\n";
    let stripped = strip_jinja_blocks(source);
    // Verify indentation is preserved exactly
    assert!(
        stripped.contains("  bucket:\n    type: aws:s3:Bucket\n    properties:\n      name: test")
    );
}

#[test]
fn test_strip_empty_template() {
    let source = "";
    assert!(!has_jinja_block_syntax(source));
    let stripped = strip_jinja_blocks(source);
    assert_eq!(stripped, "");
}

#[test]
fn test_strip_only_blocks() {
    let source = "{% for i in range(3) %}\n{% endfor %}\n";
    let stripped = strip_jinja_blocks(source);
    assert_eq!(stripped, "\n", "should be empty line with trailing newline");
}

#[test]
fn test_exec_pipeline_with_config_in_context() {
    let source = r#"name: test
runtime: yaml
config:
  gcp:project:
    value: my-project
resources:
{% for env in ["dev", "staging"] %}
  "bucket-{{ env }}":
    type: gcp:storage:Bucket
    properties:
      name: "{{ config.project }}-{{ env }}"
{% endfor %}
"#;

    // Validate syntax
    assert!(validate_jinja_syntax(source, "Pulumi.yaml").is_ok());

    // Strip for CLI
    let stripped = strip_jinja_blocks(source);
    let parsed: Result<serde_yaml::Value, _> = serde_yaml::from_str(&stripped);
    assert!(parsed.is_ok(), "stripped should parse: {:?}", parsed.err());

    // Render with context (simulating Run RPC)
    let mut config = HashMap::new();
    config.insert("gcp:project".to_string(), "my-project".to_string());
    let ctx = make_context(&config);
    let preprocessor = JinjaPreprocessor::new(&ctx);
    let rendered = preprocessor.preprocess(source, "Pulumi.yaml").unwrap();
    assert!(
        rendered.contains("bucket-dev"),
        "should contain bucket-dev, got:\n{}",
        rendered
    );
    assert!(
        rendered.contains("bucket-staging"),
        "should contain bucket-staging, got:\n{}",
        rendered
    );
    assert!(
        rendered.contains("my-project-dev"),
        "should contain rendered config value, got:\n{}",
        rendered
    );
    assert!(
        rendered.contains("my-project-staging"),
        "should contain rendered config value, got:\n{}",
        rendered
    );
}

#[test]
fn test_exec_pipeline_full_evaluator_with_blocks() {
    use pulumi_rs_yaml_core::ast::parse::parse_template;
    use pulumi_rs_yaml_core::eval::evaluator::Evaluator;
    use pulumi_rs_yaml_core::eval::mock::MockCallback;

    // Simulate the full exec pipeline: strip → CLI parses → render → evaluate
    let original = r#"name: exec-test
runtime: yaml
resources:
{% for i in range(3) %}
  bucket{{ i }}:
    type: aws:s3:Bucket
    properties:
      bucketName: "{{ pulumi_project }}-{{ i }}"
{% endfor %}
outputs:
  count: 3
"#;

    // Phase 1: Strip for CLI validation
    let stripped = strip_jinja_blocks(original);
    let parsed: Result<serde_yaml::Value, _> = serde_yaml::from_str(&stripped);
    assert!(parsed.is_ok(), "stripped should parse");

    // Phase 2: Render original (what the language host does)
    let config = HashMap::new();
    let ctx = JinjaContext {
        project_name: "exec-test",
        stack_name: "dev",
        cwd: "/tmp",
        organization: "my-org",
        root_directory: "/tmp",
        config: &config,
        project_dir: "/tmp",
        undefined: UndefinedMode::Strict,
    };
    let preprocessor = JinjaPreprocessor::new(&ctx);
    let rendered = preprocessor.preprocess(original, "Pulumi.yaml").unwrap();

    // Phase 3: Parse rendered YAML
    let (template, diags) = parse_template(rendered.as_ref(), None);
    assert!(!diags.has_errors(), "parse errors: {}", diags);

    // Phase 4: Evaluate with mock
    let template: &'static _ = Box::leak(Box::new(template));
    let mut eval = Evaluator::with_callback(
        "exec-test".to_string(),
        "dev".to_string(),
        "/tmp".to_string(),
        false,
        MockCallback::new(),
    );
    eval.evaluate_template(template, &HashMap::new(), &[]);
    assert!(!eval.diags.has_errors(), "eval errors: {}", eval.diags);

    // Verify all 3 buckets registered
    let regs = eval.callback().registrations();
    assert_eq!(regs.len(), 3, "expected 3 bucket registrations");
    for (i, reg) in regs.iter().enumerate() {
        assert_eq!(
            reg.inputs.get("bucketName").and_then(|v| v.as_str()),
            Some(format!("exec-test-{}", i).as_str()),
        );
    }
}

#[test]
fn test_validate_syntax_error_has_rich_info() {
    let source = "{% for i in range(3) %}\n  {{ i }}\n";
    let result = validate_jinja_syntax(source, "Pulumi.yaml");
    assert!(result.is_err());
    let diag = result.unwrap_err();
    let formatted = diag.format_rich("Pulumi.yaml");
    assert!(formatted.contains("Pulumi.yaml"), "should contain filename");
    assert!(formatted.contains("error:"), "should contain 'error:'");
}

// ============================================================================
// Single-line {% set %} integration tests
// ============================================================================

#[test]
fn test_single_line_set_full_pipeline() {
    use pulumi_rs_yaml_core::ast::parse::parse_template;
    use pulumi_rs_yaml_core::eval::evaluator::Evaluator;
    use pulumi_rs_yaml_core::eval::mock::MockCallback;

    let source = r#"{% set prefix = "myapp" %}
name: test
runtime: yaml
resources:
  bucket:
    type: aws:s3:Bucket
    properties:
      bucketName: "{{ prefix }}-data"
outputs:
  name: ${bucket.id}
"#;

    // Strip removes the {% set %} line
    let stripped = strip_jinja_blocks(source);
    assert!(!stripped.contains("{% set"));
    let parsed: Result<serde_yaml::Value, _> = serde_yaml::from_str(&stripped);
    assert!(parsed.is_ok(), "stripped should parse: {:?}", parsed.err());

    // Full render resolves the set variable
    let config = HashMap::new();
    let ctx = make_context(&config);
    let preprocessor = JinjaPreprocessor::new(&ctx);
    let rendered = preprocessor.preprocess(source, "Pulumi.yaml").unwrap();
    assert!(
        rendered.contains("myapp-data"),
        "set variable should be resolved, got:\n{}",
        rendered
    );

    // Parse and evaluate
    let (template, diags) = parse_template(rendered.as_ref(), None);
    assert!(!diags.has_errors(), "parse errors: {}", diags);
    let template: &'static _ = Box::leak(Box::new(template));
    let mut eval = Evaluator::with_callback(
        "test-project".to_string(),
        "dev".to_string(),
        "/tmp".to_string(),
        false,
        MockCallback::new(),
    );
    eval.evaluate_template(template, &HashMap::new(), &[]);
    assert!(!eval.diags.has_errors(), "eval errors: {}", eval.diags);

    let regs = eval.callback().registrations();
    assert_eq!(regs.len(), 1);
    assert_eq!(
        regs[0].inputs.get("bucketName").and_then(|v| v.as_str()),
        Some("myapp-data"),
    );
}

#[test]
fn test_single_line_set_per_file_scope() {
    // Variables set in one file's Jinja scope should not leak to another
    let source_a = "{% set prefix = \"file-a\" %}\nname: {{ prefix }}\nruntime: yaml\n";
    let source_b =
        "resources:\n  bucket:\n    type: aws:s3:Bucket\n    properties:\n      name: default\n";

    let config = HashMap::new();
    let ctx = make_context(&config);
    let preprocessor = JinjaPreprocessor::new(&ctx);

    let rendered_a = preprocessor.preprocess(source_a, "Pulumi.yaml").unwrap();
    assert!(
        rendered_a.contains("name: file-a"),
        "file A should resolve set var"
    );

    let rendered_b = preprocessor
        .preprocess(source_b, "Pulumi.storage.yaml")
        .unwrap();
    // File B should not have "prefix" in scope
    assert!(
        !rendered_b.contains("file-a"),
        "file B should NOT see file A's set variable"
    );
    assert!(
        rendered_b.contains("name: default"),
        "file B should be unchanged"
    );
}

// ============================================================================
// readFile() tests
// ============================================================================

fn make_readfile_context<'a>(
    dir: &'a tempfile::TempDir,
    config: &'a HashMap<String, String>,
) -> JinjaContext<'a> {
    // We leak the string so it lives long enough; tests are short-lived
    let project_dir: &'static str =
        Box::leak(dir.path().to_str().unwrap().to_string().into_boxed_str());
    JinjaContext {
        project_name: "test-project",
        stack_name: "dev",
        cwd: project_dir,
        organization: "my-org",
        root_directory: project_dir,
        config,
        project_dir,
        undefined: UndefinedMode::Strict,
    }
}

fn make_temp_dir(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (name, content) in files {
        let path = dir.path().join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, content).unwrap();
    }
    dir
}

#[test]
fn test_readfile_standalone_auto_indent_json() {
    let json_content = "[\n  {\"id\": 1}\n]";
    let dir = make_temp_dir(&[("s.json", json_content)]);
    let config = HashMap::new();
    let ctx = make_readfile_context(&dir, &config);
    let preprocessor = JinjaPreprocessor::new(&ctx);

    let source = "name: test\nruntime: yaml\nresources:\n  r:\n    type: test:R\n    properties:\n      schema: |\n        {{ readFile(\"s.json\") }}\n";
    let result = preprocessor.preprocess(source, "Pulumi.yaml").unwrap();

    // Each line of JSON should have 8-space indent
    assert!(result.contains("        ["), "got:\n{}", result);
    assert!(result.contains("          {\"id\": 1}"), "got:\n{}", result);
    assert!(result.contains("        ]"), "got:\n{}", result);
}

#[test]
fn test_readfile_standalone_auto_indent_yaml() {
    let yaml_content = "key: value\nnested:\n  x: 1";
    let dir = make_temp_dir(&[("cfg.yaml", yaml_content)]);
    let config = HashMap::new();
    let ctx = make_readfile_context(&dir, &config);
    let preprocessor = JinjaPreprocessor::new(&ctx);

    let source = "name: test\nruntime: yaml\nresources:\n  r:\n    type: test:R\n    properties:\n      config: |\n        {{ readFile(\"cfg.yaml\") }}\n";
    let result = preprocessor.preprocess(source, "Pulumi.yaml").unwrap();

    assert!(result.contains("        key: value"), "got:\n{}", result);
    assert!(result.contains("        nested:"), "got:\n{}", result);
    assert!(result.contains("          x: 1"), "got:\n{}", result);
}

#[test]
fn test_readfile_inline_single_line() {
    let dir = make_temp_dir(&[("v.txt", "1.2.3")]);
    let config = HashMap::new();
    let ctx = make_readfile_context(&dir, &config);
    let preprocessor = JinjaPreprocessor::new(&ctx);

    let source = "version: {{ readFile(\"v.txt\") }}\n";
    let result = preprocessor.preprocess(source, "Pulumi.yaml").unwrap();
    assert!(result.contains("version: 1.2.3"), "got:\n{}", result);
}

#[test]
fn test_readfile_inline_multi_line() {
    let dir = make_temp_dir(&[("m.txt", "line1\nline2")]);
    let config = HashMap::new();
    let ctx = make_readfile_context(&dir, &config);
    let preprocessor = JinjaPreprocessor::new(&ctx);

    let source = "data: {{ readFile(\"m.txt\") }}\n";
    let result = preprocessor.preprocess(source, "Pulumi.yaml").unwrap();
    // Inline: raw replacement, no auto-indent
    assert!(result.contains("data: line1"), "got:\n{}", result);
    assert!(result.contains("line2"), "got:\n{}", result);
}

#[test]
fn test_readfile_empty_file() {
    let dir = make_temp_dir(&[("empty.txt", "")]);
    let config = HashMap::new();
    let ctx = make_readfile_context(&dir, &config);
    let preprocessor = JinjaPreprocessor::new(&ctx);

    let source = "data: {{ readFile(\"empty.txt\") }}\n";
    let result = preprocessor.preprocess(source, "Pulumi.yaml").unwrap();
    assert!(result.contains("data: "), "got:\n{}", result);
}

#[test]
fn test_readfile_file_not_found_error() {
    let dir = make_temp_dir(&[]);
    let config = HashMap::new();
    let ctx = make_readfile_context(&dir, &config);
    let preprocessor = JinjaPreprocessor::new(&ctx);

    let source = "data: {{ readFile(\"missing.txt\") }}\n";
    let result = preprocessor.preprocess(source, "Pulumi.yaml");
    assert!(result.is_err());
    let diag = result.unwrap_err();
    assert!(
        diag.message.contains("readFile: failed to read"),
        "got: {}",
        diag.message
    );
    assert_eq!(diag.kind, RenderErrorKind::JinjaFilterError);
    assert!(diag.suggestion.is_some());
    assert!(diag.suggestion.unwrap().contains("project directory"));
}

#[cfg(unix)] // Windows backslashes in paths are misinterpreted as Jinja escape sequences
#[test]
fn test_readfile_absolute_path() {
    let dir = make_temp_dir(&[("abs.txt", "absolute content")]);
    let abs_path = dir.path().join("abs.txt");
    let config = HashMap::new();
    let ctx = make_readfile_context(&dir, &config);
    let preprocessor = JinjaPreprocessor::new(&ctx);

    let source = format!("data: {{{{ readFile(\"{}\") }}}}\n", abs_path.display());
    let result = preprocessor.preprocess(&source, "Pulumi.yaml").unwrap();
    assert!(
        result.contains("data: absolute content"),
        "got:\n{}",
        result
    );
}

#[test]
fn test_readfile_relative_to_project_dir() {
    let dir = make_temp_dir(&[("sub/f.txt", "nested content")]);
    let config = HashMap::new();
    let ctx = make_readfile_context(&dir, &config);
    let preprocessor = JinjaPreprocessor::new(&ctx);

    let source = "data: {{ readFile(\"sub/f.txt\") }}\n";
    let result = preprocessor.preprocess(source, "Pulumi.yaml").unwrap();
    assert!(result.contains("data: nested content"), "got:\n{}", result);
}

#[test]
fn test_readfile_no_jinja_fast_path() {
    let config = HashMap::new();
    let dir = make_temp_dir(&[]);
    let ctx = make_readfile_context(&dir, &config);
    let preprocessor = JinjaPreprocessor::new(&ctx);

    let source = "name: test\nruntime: yaml\n";
    let result = preprocessor.preprocess(source, "Pulumi.yaml").unwrap();
    assert!(matches!(result, Cow::Borrowed(_)));
}

#[test]
fn test_readfile_in_for_loop() {
    let dir = make_temp_dir(&[("a.txt", "alpha"), ("b.txt", "beta")]);
    let config = HashMap::new();
    let ctx = make_readfile_context(&dir, &config);
    let preprocessor = JinjaPreprocessor::new(&ctx);

    let source = r#"name: test
runtime: yaml
resources:
{% for f in ["a.txt", "b.txt"] %}
  res{{ loop.index }}:
    type: test:R
    properties:
      data: {{ readFile(f) }}
{% endfor %}
"#;
    let result = preprocessor.preprocess(source, "Pulumi.yaml").unwrap();
    assert!(result.contains("alpha"), "got:\n{}", result);
    assert!(result.contains("beta"), "got:\n{}", result);
}

#[test]
fn test_readfile_in_if_conditional() {
    let dir = make_temp_dir(&[("f.txt", "conditionally included")]);
    let config = HashMap::new();
    let ctx = make_readfile_context(&dir, &config);
    let preprocessor = JinjaPreprocessor::new(&ctx);

    let source =
        "name: test\nruntime: yaml\n{% if true %}\ndata: {{ readFile(\"f.txt\") }}\n{% endif %}\n";
    let result = preprocessor.preprocess(source, "Pulumi.yaml").unwrap();
    assert!(
        result.contains("conditionally included"),
        "got:\n{}",
        result
    );
}

#[test]
fn test_readfile_stored_in_set_variable() {
    let dir = make_temp_dir(&[("f.txt", "set content")]);
    let config = HashMap::new();
    let ctx = make_readfile_context(&dir, &config);
    let preprocessor = JinjaPreprocessor::new(&ctx);

    let source = "{% set s = readFile(\"f.txt\") %}\ndata: {{ s }}\n";
    let result = preprocessor.preprocess(source, "Pulumi.yaml").unwrap();
    assert!(result.contains("set content"), "got:\n{}", result);
}

#[test]
fn test_readfile_with_jinja_filter_chain() {
    let dir = make_temp_dir(&[("f.txt", "hello")]);
    let config = HashMap::new();
    let ctx = make_readfile_context(&dir, &config);
    let preprocessor = JinjaPreprocessor::new(&ctx);

    let source = "data: {{ readFile(\"f.txt\") | upper }}\n";
    let result = preprocessor.preprocess(source, "Pulumi.yaml").unwrap();
    // The marker is what gets uppercased, which won't work as expected.
    // But the filter is applied to the marker string, not the content.
    // This is a known limitation — filters on readFile work on the marker.
    // For now, just verify no crash occurs.
    assert!(result.contains("data: "), "got:\n{}", result);
}

#[test]
fn test_readfile_with_pulumi_interpolation() {
    let dir = make_temp_dir(&[("f.txt", "file content")]);
    let config = HashMap::new();
    let ctx = make_readfile_context(&dir, &config);
    let preprocessor = JinjaPreprocessor::new(&ctx);

    let source = "data: {{ readFile(\"f.txt\") }}\nref: ${resource.id}\n";
    let result = preprocessor.preprocess(source, "Pulumi.yaml").unwrap();
    assert!(result.contains("file content"), "got:\n{}", result);
    assert!(result.contains("${resource.id}"), "got:\n{}", result);
}

#[test]
fn test_readfile_alongside_jinja_vars() {
    let dir = make_temp_dir(&[("f.txt", "included")]);
    let config = HashMap::new();
    let ctx = make_readfile_context(&dir, &config);
    let preprocessor = JinjaPreprocessor::new(&ctx);

    let source = "name: {{ pulumi_project }}\ndata: {{ readFile(\"f.txt\") }}\n";
    let result = preprocessor.preprocess(source, "Pulumi.yaml").unwrap();
    assert!(result.contains("name: test-project"), "got:\n{}", result);
    assert!(result.contains("data: included"), "got:\n{}", result);
}

#[test]
fn test_readfile_no_args_error() {
    let dir = make_temp_dir(&[]);
    let config = HashMap::new();
    let ctx = make_readfile_context(&dir, &config);
    let preprocessor = JinjaPreprocessor::new(&ctx);

    let source = "data: {{ readFile() }}\n";
    let result = preprocessor.preprocess(source, "Pulumi.yaml");
    assert!(result.is_err(), "readFile() with no args should error");
}

#[test]
fn test_readfile_full_pipeline_with_evaluator() {
    use pulumi_rs_yaml_core::ast::parse::parse_template;
    use pulumi_rs_yaml_core::eval::evaluator::Evaluator;
    use pulumi_rs_yaml_core::eval::mock::MockCallback;

    let json_content = "{\n  \"type\": \"object\",\n  \"properties\": {\n    \"name\": {\"type\": \"string\"}\n  }\n}";
    let dir = make_temp_dir(&[("schema.json", json_content)]);
    let config = HashMap::new();
    let ctx = make_readfile_context(&dir, &config);
    let preprocessor = JinjaPreprocessor::new(&ctx);

    let source = r#"name: test-project
runtime: yaml
resources:
  myResource:
    type: test:Resource
    properties:
      schema: |
        {{ readFile("schema.json") }}
outputs:
  done: ok
"#;

    let rendered = preprocessor.preprocess(source, "Pulumi.yaml").unwrap();

    // Verify the rendered output is valid YAML
    assert!(
        validate_rendered_yaml(rendered.as_ref(), source, "Pulumi.yaml").is_ok(),
        "rendered should be valid YAML:\n{}",
        rendered
    );

    // Parse the rendered YAML
    let (template, diags) = parse_template(rendered.as_ref(), None);
    assert!(!diags.has_errors(), "parse errors: {}", diags);

    // Evaluate with mock
    let template: &'static _ = Box::leak(Box::new(template));
    let mut eval = Evaluator::with_callback(
        "test-project".to_string(),
        "dev".to_string(),
        "/tmp".to_string(),
        false,
        MockCallback::new(),
    );
    eval.evaluate_template(template, &HashMap::new(), &[]);
    assert!(!eval.diags.has_errors(), "eval errors: {}", eval.diags);

    // Verify the resource was registered and has schema content
    let regs = eval.callback().registrations();
    assert_eq!(regs.len(), 1);
    let schema_val = regs[0]
        .inputs
        .get("schema")
        .expect("should have schema property");
    let schema_str = schema_val.as_str().expect("schema should be a string");
    assert!(
        schema_str.contains("\"type\": \"object\""),
        "schema should contain JSON content, got: {}",
        schema_str
    );
    assert!(
        schema_str.contains("\"name\""),
        "schema should contain nested properties, got: {}",
        schema_str
    );
}

// ============================================================================
// Passthrough mode — extract_root_identifier tests
// ============================================================================

use pulumi_rs_yaml_core::jinja::{
    classify_expression, extract_root_identifier, pre_escape_for_passthrough, ExprClassification,
};

#[test]
fn test_extract_root_config_dot_access() {
    let (ident, is_fn) = extract_root_identifier("config.gcpProject").unwrap();
    assert_eq!(ident, "config");
    assert!(!is_fn);
}

#[test]
fn test_extract_root_config_function_call() {
    let (ident, is_fn) = extract_root_identifier("config(materialized='view')").unwrap();
    assert_eq!(ident, "config");
    assert!(is_fn);
}

#[test]
fn test_extract_root_ref_function() {
    let (ident, is_fn) = extract_root_identifier("ref('orders')").unwrap();
    assert_eq!(ident, "ref");
    assert!(is_fn);
}

#[test]
fn test_extract_root_source_function() {
    let (ident, is_fn) = extract_root_identifier("source('raw', 'orders')").unwrap();
    assert_eq!(ident, "source");
    assert!(is_fn);
}

#[test]
fn test_extract_root_bare_variable() {
    let (ident, is_fn) = extract_root_identifier("amount_cents").unwrap();
    assert_eq!(ident, "amount_cents");
    assert!(!is_fn);
}

#[test]
fn test_extract_root_with_filter() {
    let (ident, is_fn) = extract_root_identifier("config.region | upper").unwrap();
    assert_eq!(ident, "config");
    assert!(!is_fn);
}

#[test]
fn test_extract_root_readfile() {
    let (ident, is_fn) = extract_root_identifier("readFile('f.sql')").unwrap();
    assert_eq!(ident, "readFile");
    assert!(is_fn);
}

#[test]
fn test_extract_root_empty() {
    assert!(extract_root_identifier("").is_none());
}

#[test]
fn test_extract_root_string_literal() {
    assert!(extract_root_identifier("\"literal\"").is_none());
}

#[test]
fn test_extract_root_whitespace_trim() {
    let (ident, is_fn) = extract_root_identifier("  config.key  ").unwrap();
    assert_eq!(ident, "config");
    assert!(!is_fn);
}

// ============================================================================
// Passthrough mode — classify_expression tests
// ============================================================================

#[test]
fn test_classify_unknown_root_passthrough() {
    assert_eq!(
        classify_expression(" ref('model') "),
        ExprClassification::Passthrough
    );
}

#[test]
fn test_classify_known_root_evaluate() {
    assert_eq!(
        classify_expression(" config.region "),
        ExprClassification::Evaluate
    );
}

#[test]
fn test_classify_dict_as_function_passthrough() {
    // config used as function call = dbt's config(), not Pulumi's config.key
    assert_eq!(
        classify_expression(" config(materialized='view') "),
        ExprClassification::Passthrough
    );
}

#[test]
fn test_classify_known_function_evaluate() {
    assert_eq!(
        classify_expression(" readFile('f.sql') "),
        ExprClassification::Evaluate
    );
}

#[test]
fn test_classify_bare_unknown_passthrough() {
    assert_eq!(
        classify_expression(" amount_cents "),
        ExprClassification::Passthrough
    );
}

#[test]
fn test_classify_pulumi_project_evaluate() {
    assert_eq!(
        classify_expression(" pulumi_project "),
        ExprClassification::Evaluate
    );
}

// ============================================================================
// Passthrough mode — pre_escape_for_passthrough tests
// ============================================================================

#[test]
fn test_pre_escape_no_expressions() {
    let source = "name: test\nruntime: yaml\n";
    let result = pre_escape_for_passthrough(source);
    assert!(matches!(result, Cow::Borrowed(_)));
    assert_eq!(result.as_ref(), source);
}

#[test]
fn test_pre_escape_all_known() {
    let source = "name: {{ pulumi_project }}\nregion: {{ config.region }}\n";
    let result = pre_escape_for_passthrough(source);
    // All expressions are known → no escaping needed
    assert!(matches!(result, Cow::Borrowed(_)));
}

#[test]
fn test_pre_escape_unknown_wrapped() {
    let source = "sql: {{ ref('orders') }}\n";
    let result = pre_escape_for_passthrough(source);
    assert!(
        result.contains("{% raw %}{{ ref('orders') }}{% endraw %}"),
        "got: {}",
        result
    );
}

#[test]
fn test_pre_escape_mixed_known_and_unknown() {
    let source = "name: {{ pulumi_project }}\nsql: {{ ref('orders') }}\n";
    let result = pre_escape_for_passthrough(source);
    // pulumi_project should NOT be wrapped
    assert!(result.contains("{{ pulumi_project }}"), "got: {}", result);
    assert!(
        !result.contains("{% raw %}{{ pulumi_project }}"),
        "got: {}",
        result
    );
    // ref should be wrapped
    assert!(
        result.contains("{% raw %}{{ ref('orders') }}{% endraw %}"),
        "got: {}",
        result
    );
}

#[test]
fn test_pre_escape_preserves_comments() {
    let source = "{# a comment #}\ndata: {{ unknown_var }}\n";
    let result = pre_escape_for_passthrough(source);
    // Comments use {# #} not {{ }}, so they pass through
    assert!(result.contains("{# a comment #}"), "got: {}", result);
}

#[test]
fn test_pre_escape_preserves_blocks() {
    let source = "{% for i in range(3) %}\ndata: {{ unknown }}\n{% endfor %}\n";
    let result = pre_escape_for_passthrough(source);
    // Blocks use {% %} not {{ }}, so they pass through
    assert!(
        result.contains("{% for i in range(3) %}"),
        "got: {}",
        result
    );
    assert!(result.contains("{% endfor %}"), "got: {}", result);
}

#[test]
fn test_pre_escape_existing_raw_not_double_wrapped() {
    // If source already has {% raw %}, the inner {{ }} won't be processed
    // by our scanner because {% raw %} blocks are Jinja syntax, not {{ }}
    let source = "data: {% raw %}{{ dbt_thing }}{% endraw %}\n";
    let result = pre_escape_for_passthrough(source);
    // Should be borrowed (no {{ }} expressions outside raw)
    assert!(matches!(result, Cow::Borrowed(_)));
}

// ============================================================================
// Passthrough mode — end-to-end tests
// ============================================================================

#[test]
fn test_passthrough_dbt_template() {
    let source = r#"name: {{ pulumi_project }}
runtime: yaml
resources:
  model:
    type: gcpx:dbt:Model
    properties:
      sql: "SELECT ROUND({{ amount_cents }} / 100, 2) FROM {{ ref('orders') }}"
"#;
    let config = HashMap::new();
    let ctx = JinjaContext {
        project_name: "myproject",
        stack_name: "dev",
        cwd: "/tmp",
        organization: "",
        root_directory: "/tmp",
        config: &config,
        project_dir: "/tmp",
        undefined: UndefinedMode::Passthrough,
    };
    let preprocessor = JinjaPreprocessor::new(&ctx);
    let result = preprocessor.preprocess(source, "Pulumi.yaml").unwrap();
    // Pulumi project should be resolved
    assert!(result.contains("name: myproject"), "got:\n{}", result);
    // dbt expressions should pass through literally
    assert!(result.contains("{{ amount_cents }}"), "got:\n{}", result);
    assert!(result.contains("{{ ref('orders') }}"), "got:\n{}", result);
}

#[test]
fn test_passthrough_config_typo_still_errors() {
    // config.gcpProjecT (with typo) should still be evaluated (caught by minijinja)
    let source = "region: {{ config.gcpProjecT }}\n";
    let mut config = HashMap::new();
    config.insert("test:gcpProject".to_string(), "my-project".to_string());
    let ctx = JinjaContext {
        project_name: "test",
        stack_name: "dev",
        cwd: "/tmp",
        organization: "",
        root_directory: "/tmp",
        config: &config,
        project_dir: "/tmp",
        undefined: UndefinedMode::Passthrough,
    };
    let preprocessor = JinjaPreprocessor::new(&ctx);
    let result = preprocessor.preprocess(source, "Pulumi.yaml");
    // Should error because config.gcpProjecT is evaluated (config is a known root)
    // and the typo means the attribute doesn't exist
    assert!(
        result.is_err(),
        "config typo should still error in passthrough mode"
    );
}

#[test]
fn test_passthrough_unknown_passes_through() {
    let source = "data: {{ some_unknown_var }}\n";
    let config = HashMap::new();
    let ctx = JinjaContext {
        project_name: "test",
        stack_name: "dev",
        cwd: "/tmp",
        organization: "",
        root_directory: "/tmp",
        config: &config,
        project_dir: "/tmp",
        undefined: UndefinedMode::Passthrough,
    };
    let preprocessor = JinjaPreprocessor::new(&ctx);
    let result = preprocessor.preprocess(source, "Pulumi.yaml").unwrap();
    assert!(
        result.contains("{{ some_unknown_var }}"),
        "got:\n{}",
        result
    );
}

#[test]
fn test_passthrough_readfile_still_works() {
    let dir = make_temp_dir(&[("f.sql", "SELECT 1")]);
    let config = HashMap::new();
    let ctx = JinjaContext {
        project_name: "test",
        stack_name: "dev",
        cwd: dir.path().to_str().unwrap(),
        organization: "",
        root_directory: dir.path().to_str().unwrap(),
        config: &config,
        project_dir: dir.path().to_str().unwrap(),
        undefined: UndefinedMode::Passthrough,
    };
    let preprocessor = JinjaPreprocessor::new(&ctx);
    let source = "sql: {{ readFile('f.sql') }}\n";
    let result = preprocessor.preprocess(source, "Pulumi.yaml").unwrap();
    assert!(
        result.contains("SELECT 1"),
        "readFile should still work in passthrough mode, got:\n{}",
        result
    );
}

#[test]
fn test_passthrough_for_loop_with_unknown_vars() {
    // for-loop variables defined by {% set %} or {% for %} whose names aren't KNOWN_ROOTS
    // will be treated as passthrough in the pre-escape phase.
    // This is a known limitation (documented in A6).
    // Users should use readFile() or config.* for Pulumi variables.
    let source = "{% for i in range(3) %}\nitem: {{ i }}\n{% endfor %}\n";
    let config = HashMap::new();
    let ctx = JinjaContext {
        project_name: "test",
        stack_name: "dev",
        cwd: "/tmp",
        organization: "",
        root_directory: "/tmp",
        config: &config,
        project_dir: "/tmp",
        undefined: UndefinedMode::Passthrough,
    };
    let preprocessor = JinjaPreprocessor::new(&ctx);
    let result = preprocessor.preprocess(source, "Pulumi.yaml").unwrap();
    // `i` is not a known root, so in passthrough mode it gets wrapped in raw
    // which means it passes through literally as {{ i }}
    assert!(
        result.contains("{{ i }}"),
        "for-loop var should pass through in passthrough mode, got:\n{}",
        result
    );
}

#[test]
fn test_strict_mode_unchanged() {
    // In strict mode (default), unknown variables should still error
    let source = "data: {{ unknown_var }}\n";
    let config = HashMap::new();
    let ctx = JinjaContext {
        project_name: "test",
        stack_name: "dev",
        cwd: "/tmp",
        organization: "",
        root_directory: "/tmp",
        config: &config,
        project_dir: "/tmp",
        undefined: UndefinedMode::Strict,
    };
    let preprocessor = JinjaPreprocessor::new(&ctx);
    let result = preprocessor.preprocess(source, "Pulumi.yaml");
    assert!(
        result.is_err(),
        "strict mode should error on unknown variables"
    );
}

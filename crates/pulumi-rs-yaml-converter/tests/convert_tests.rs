use pretty_assertions::assert_eq;
use pulumi_rs_yaml_converter::yaml_to_pcl;

/// Runs a golden-file test: reads input YAML, converts to PCL, compares with expected.
fn golden_test(fixture: &str) {
    let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("testdata")
        .join(fixture);
    let input = std::fs::read_to_string(base.join("input.yaml")).unwrap_or_else(|e| {
        panic!(
            "failed to read {}: {}",
            base.join("input.yaml").display(),
            e
        )
    });
    let expected = std::fs::read_to_string(base.join("expected.pp")).unwrap_or_else(|e| {
        panic!(
            "failed to read {}: {}",
            base.join("expected.pp").display(),
            e
        )
    });

    let result = yaml_to_pcl(&input);
    assert!(
        !result.diagnostics.has_errors(),
        "conversion produced errors:\n{}",
        result.diagnostics
    );

    assert_eq!(result.pcl_text, expected);
}

#[test]
fn test_basic_resource() {
    golden_test("basic-resource");
}

#[test]
fn test_config_variables() {
    golden_test("config-variables");
}

#[test]
fn test_variables_builtins() {
    golden_test("variables-builtins");
}

#[test]
fn test_outputs() {
    golden_test("outputs");
}

#[test]
fn test_resource_options() {
    golden_test("resource-options");
}

#[test]
fn test_invokes() {
    golden_test("invokes");
}

#[test]
fn test_assets_archives() {
    golden_test("assets-archives");
}

#[test]
fn test_dashes_in_names() {
    golden_test("dashes-in-names");
}

#[test]
fn test_pulumi_variables() {
    golden_test("pulumi-variables");
}

#[test]
fn test_nested_map() {
    golden_test("nested-map");
}

#[test]
fn test_func_name_shadowing() {
    golden_test("func-name-shadowing");
}

#[test]
fn test_complex_interpolation() {
    golden_test("complex-interpolation");
}

#[test]
fn test_invoke_options() {
    golden_test("invoke-options");
}

#[test]
fn test_stack_reference() {
    golden_test("stack-reference");
}

#[test]
fn test_name_collisions() {
    golden_test("name-collisions");
}

// ─── Inline tests matching Go load_test.go ────────────────────

#[test]
fn test_go_complex_resource_options() {
    let yaml = r#"
name: test
runtime: yaml
resources:
  prov:
    type: test:mod:Prov
  bar:
    type: test:mod:Typ
    options:
      provider: ${prov.outputField[0].outputProvider}
"#;
    let result = yaml_to_pcl(yaml);
    let pcl = result.pcl_text;

    assert!(
        pcl.contains("resource bar \"test:mod:Typ\""),
        "got:\n{}",
        pcl
    );
    assert!(
        pcl.contains("resource prov \"test:mod:Prov\""),
        "got:\n{}",
        pcl
    );
    assert!(
        pcl.contains("provider = prov.outputField[0].outputProvider"),
        "got:\n{}",
        pcl
    );
}

#[test]
fn test_go_complex_pulumi_variables() {
    let yaml = r#"
name: test
runtime: yaml
resources:
  bar:
    type: test:mod:Typ
    properties:
      foo: ${pulumi.cwd}
"#;
    let result = yaml_to_pcl(yaml);
    let pcl = result.pcl_text;

    assert!(pcl.contains("foo = cwd()"), "got:\n{}", pcl);
}

#[test]
fn test_go_interpolate_pulumi_variable() {
    let yaml = r#"
name: test
runtime: yaml
outputs:
  foo: ${pulumi.cwd}/folder
"#;
    let result = yaml_to_pcl(yaml);
    let pcl = result.pcl_text;

    assert!(pcl.contains("value = \"${cwd()}/folder\""), "got:\n{}", pcl);
}

#[test]
fn test_select_to_index_expression() {
    let yaml = r#"
name: test
runtime: yaml
variables:
  picked:
    fn::select:
      - 1
      - - a
        - b
        - c
"#;
    let result = yaml_to_pcl(yaml);
    let pcl = result.pcl_text;

    // fn::select → IndexExpression: values[idx]
    assert!(pcl.contains("[1]"), "got:\n{}", pcl);
}

#[test]
fn test_secret_builtin() {
    let yaml = r#"
name: test
runtime: yaml
variables:
  secretVal:
    fn::secret: my-password
"#;
    let result = yaml_to_pcl(yaml);
    let pcl = result.pcl_text;

    assert!(
        pcl.contains("secretVal = secret(\"my-password\")"),
        "got:\n{}",
        pcl
    );
}

#[test]
fn test_read_file_builtin() {
    let yaml = r#"
name: test
runtime: yaml
variables:
  content:
    fn::readFile: ./data.txt
"#;
    let result = yaml_to_pcl(yaml);
    let pcl = result.pcl_text;

    assert!(
        pcl.contains("content = readFile(\"./data.txt\")"),
        "got:\n{}",
        pcl
    );
}

#[test]
fn test_split_builtin() {
    let yaml = r#"
name: test
runtime: yaml
variables:
  parts:
    fn::split:
      - ","
      - "a,b,c"
"#;
    let result = yaml_to_pcl(yaml);
    let pcl = result.pcl_text;

    assert!(
        pcl.contains("parts = split(\",\", \"a,b,c\")"),
        "got:\n{}",
        pcl
    );
}

#[test]
fn test_to_json_builtin() {
    let yaml = r#"
name: test
runtime: yaml
variables:
  jsonified:
    fn::toJSON:
      key: value
"#;
    let result = yaml_to_pcl(yaml);
    let pcl = result.pcl_text;

    assert!(pcl.contains("toJSON("), "got:\n{}", pcl);
}

#[test]
fn test_rust_only_builtin_warning() {
    let yaml = r#"
name: test
runtime: yaml
variables:
  absVal:
    fn::abs: -5
"#;
    let result = yaml_to_pcl(yaml);

    // Should produce a warning about unsupported builtin
    let warnings: Vec<_> = result
        .diagnostics
        .iter()
        .filter(|d| !d.is_error())
        .collect();
    assert!(!warnings.is_empty(), "expected a warning for fn::abs");
    assert!(warnings[0].summary.contains("unsupported builtin"));

    // Should emit null placeholder
    assert!(
        result.pcl_text.contains("null /* unsupported builtin */"),
        "got:\n{}",
        result.pcl_text
    );
}

#[test]
fn test_empty_template() {
    let yaml = r#"
name: test
runtime: yaml
"#;
    let result = yaml_to_pcl(yaml);
    assert!(!result.diagnostics.has_errors());
    assert_eq!(result.pcl_text, "");
}

#[test]
fn test_config_secret() {
    let yaml = r#"
name: test
runtime: yaml
config:
  password:
    type: string
    secret: true
"#;
    let result = yaml_to_pcl(yaml);
    let pcl = result.pcl_text;

    assert!(pcl.contains("secret = true"), "got:\n{}", pcl);
}

#[test]
fn test_asset_archive() {
    let yaml = r#"
name: test
runtime: yaml
resources:
  layer:
    type: aws:lambda:LayerVersion
    properties:
      code:
        fn::assetArchive:
          index.js:
            fn::stringAsset: "exports.handler = () => {}"
          config:
            fn::fileArchive: ./config
"#;
    let result = yaml_to_pcl(yaml);
    let pcl = result.pcl_text;

    assert!(pcl.contains("assetArchive("), "got:\n{}", pcl);
    assert!(pcl.contains("stringAsset("), "got:\n{}", pcl);
    assert!(pcl.contains("fileArchive("), "got:\n{}", pcl);
}

#[test]
fn test_components() {
    golden_test("components");
}

#[test]
fn test_component_inline() {
    let yaml = r#"
name: test
runtime: yaml
components:
  myApp:
    inputs:
      env:
        type: string
        default: prod
"#;
    let result = yaml_to_pcl(yaml);
    let pcl = result.pcl_text;

    assert!(pcl.contains("component myApp"), "got:\n{}", pcl);
    assert!(pcl.contains("__logicalName = \"myApp\""), "got:\n{}", pcl);
    assert!(pcl.contains("env = \"prod\""), "got:\n{}", pcl);
}

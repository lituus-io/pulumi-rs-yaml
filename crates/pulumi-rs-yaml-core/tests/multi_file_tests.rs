//! Integration tests for multi-file Pulumi YAML support.
//!
//! Tests cover: file discovery, merge rules, collision detection,
//! cross-file references, DAG ordering, Jinja + multi-file interactions.

use std::collections::HashMap;
use std::fs;

use pulumi_rs_yaml_core::ast::parse::parse_template;
use pulumi_rs_yaml_core::eval::evaluator::Evaluator;
use pulumi_rs_yaml_core::eval::graph::{topological_sort, topological_sort_with_sources};
use pulumi_rs_yaml_core::eval::mock::MockCallback;
use pulumi_rs_yaml_core::jinja::{
    JinjaContext, JinjaPreprocessor, TemplatePreprocessor, UndefinedMode,
};
use pulumi_rs_yaml_core::multi_file::{discover_project_files, load_project, merge_templates};

fn make_temp_project(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (name, content) in files {
        fs::write(dir.path().join(name), content).unwrap();
    }
    dir
}

// ---- File Discovery ----

#[test]
fn test_discover_single_file() {
    let dir = make_temp_project(&[("Pulumi.yaml", "name: test\nruntime: yaml\n")]);
    let files = discover_project_files(dir.path()).unwrap();
    assert_eq!(files.file_count(), 1);
    assert!(files.additional_files.is_empty());
}

#[test]
fn test_discover_multiple_files() {
    let dir = make_temp_project(&[
        ("Pulumi.yaml", "name: test\nruntime: yaml\n"),
        (
            "Pulumi.buckets.yaml",
            "resources:\n  b:\n    type: test:B\n",
        ),
        ("Pulumi.tables.yaml", "resources:\n  t:\n    type: test:T\n"),
    ]);
    let files = discover_project_files(dir.path()).unwrap();
    assert_eq!(files.file_count(), 3);
    assert_eq!(files.additional_files.len(), 2);
    let names: Vec<String> = files
        .additional_files
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
        .collect();
    assert_eq!(names, vec!["Pulumi.buckets.yaml", "Pulumi.tables.yaml"]);
}

#[test]
fn test_discover_requires_pulumi_yaml() {
    let dir = make_temp_project(&[("Pulumi.extra.yaml", "resources: {}\n")]);
    let err = discover_project_files(dir.path()).unwrap_err();
    assert!(err.contains("no Pulumi.yaml"));
}

// ---- Merge Rules ----

#[test]
fn test_merge_cross_file_reference() {
    let main_src = "name: test\nruntime: yaml\noutputs:\n  url: ${bucket.url}\n";
    let buckets_src =
        "resources:\n  bucket:\n    type: test:Bucket\n    properties:\n      name: my-bucket\n";

    let (main, _) = parse_template(main_src, None);
    let (buckets, _) = parse_template(buckets_src, None);

    let (merged, diags) = merge_templates(
        main,
        "Pulumi.yaml",
        vec![("Pulumi.buckets.yaml".to_string(), buckets)],
    );
    assert!(!diags.has_errors(), "errors: {}", diags);
    assert_eq!(merged.resource_count(), 1);
    assert_eq!(merged.output_count(), 1);
    assert_eq!(merged.source_file("bucket"), Some("Pulumi.buckets.yaml"));
}

#[test]
fn test_merge_name_collision_error() {
    let main_src = "name: test\nruntime: yaml\nresources:\n  bucket:\n    type: test:B1\n";
    let extra_src = "resources:\n  bucket:\n    type: test:B2\n";

    let (main, _) = parse_template(main_src, None);
    let (extra, _) = parse_template(extra_src, None);

    let (_, diags) = merge_templates(
        main,
        "Pulumi.yaml",
        vec![("Pulumi.extra.yaml".to_string(), extra)],
    );
    assert!(diags.has_errors());
    let err = diags.iter().find(|d| d.is_error()).unwrap();
    assert!(err.summary.contains("bucket"));
    assert!(err.summary.contains("Pulumi.yaml"));
    assert!(err.summary.contains("Pulumi.extra.yaml"));
}

#[test]
fn test_merge_config_in_extra_file_error() {
    // Extra file with config AND resources → error (not a stack config file)
    let main_src = "name: test\nruntime: yaml\n";
    let extra_src = "config:\n  myKey:\n    type: string\nresources:\n  r1:\n    type: test:A\n";

    let (main, _) = parse_template(main_src, None);
    let (extra, _) = parse_template(extra_src, None);

    let (_, diags) = merge_templates(
        main,
        "Pulumi.yaml",
        vec![("Pulumi.buckets.yaml".to_string(), extra)],
    );
    assert!(diags.has_errors());
    let err = diags.iter().find(|d| d.is_error()).unwrap();
    assert!(err.summary.contains("config"));
    assert!(err.summary.contains("Pulumi.buckets.yaml"));
}

#[test]
fn test_merge_backward_compat_single_file() {
    let src = r#"
name: test
runtime: yaml
config:
  region:
    default: us-east-1
variables:
  msg: hello
resources:
  bucket:
    type: test:Bucket
    properties:
      name: ${msg}
outputs:
  url: ${bucket.url}
"#;
    let (template, _) = parse_template(src, None);
    let (merged, diags) = merge_templates(template, "Pulumi.yaml", Vec::new());
    assert!(!diags.has_errors());
    let td = merged.as_template_decl();
    assert_eq!(td.resources.len(), 1);
    assert_eq!(td.variables.len(), 1);
    assert_eq!(td.config.len(), 1);
    assert_eq!(td.outputs.len(), 1);
}

#[test]
fn test_merge_outputs_from_multiple_files() {
    let main_src = "name: test\nruntime: yaml\noutputs:\n  main_out: hello\n";
    let extra_src = "outputs:\n  extra_out: world\n";

    let (main, _) = parse_template(main_src, None);
    let (extra, _) = parse_template(extra_src, None);

    let (merged, diags) = merge_templates(
        main,
        "Pulumi.yaml",
        vec![("Pulumi.extra.yaml".to_string(), extra)],
    );
    assert!(!diags.has_errors());
    assert_eq!(merged.output_count(), 2);
}

#[test]
fn test_merge_variables_cross_file() {
    let main_src = "name: test\nruntime: yaml\nvariables:\n  x: hello\n";
    let extra_src = "variables:\n  y: ${x}\n";

    let (main, _) = parse_template(main_src, None);
    let (extra, _) = parse_template(extra_src, None);

    let (merged, diags) = merge_templates(
        main,
        "Pulumi.yaml",
        vec![("Pulumi.extra.yaml".to_string(), extra)],
    );
    assert!(!diags.has_errors());
    assert_eq!(merged.variable_count(), 2);

    // Topological sort should work on merged template
    let td = merged.as_template_decl();
    let (order, sort_diags) = topological_sort(&td);
    assert!(!sort_diags.has_errors());
    let x_pos = order.iter().position(|n| n == "x").unwrap();
    let y_pos = order.iter().position(|n| n == "y").unwrap();
    assert!(x_pos < y_pos, "x should come before y: {:?}", order);
}

#[test]
fn test_source_map_tracks_origin() {
    let main_src =
        "name: test\nruntime: yaml\nresources:\n  a:\n    type: test:A\nvariables:\n  v1: hello\n";
    let extra_src = "resources:\n  b:\n    type: test:B\nvariables:\n  v2: world\n";

    let (main, _) = parse_template(main_src, None);
    let (extra, _) = parse_template(extra_src, None);

    let (merged, diags) = merge_templates(
        main,
        "Pulumi.yaml",
        vec![("Pulumi.extra.yaml".to_string(), extra)],
    );
    assert!(!diags.has_errors());
    assert_eq!(merged.source_file("a"), Some("Pulumi.yaml"));
    assert_eq!(merged.source_file("v1"), Some("Pulumi.yaml"));
    assert_eq!(merged.source_file("b"), Some("Pulumi.extra.yaml"));
    assert_eq!(merged.source_file("v2"), Some("Pulumi.extra.yaml"));
    assert_eq!(merged.source_file("nonexistent"), None);
}

// ---- DAG Ordering ----

#[test]
fn test_dependency_order_cross_file() {
    let main_src = "name: test\nruntime: yaml\noutputs:\n  tableId: ${table.id}\n";
    let buckets_src = "resources:\n  bucket:\n    type: test:Bucket\n";
    let tables_src =
        "resources:\n  table:\n    type: test:Table\n    properties:\n      ref: ${bucket.id}\n";

    let (main, _) = parse_template(main_src, None);
    let (buckets, _) = parse_template(buckets_src, None);
    let (tables, _) = parse_template(tables_src, None);

    let (merged, merge_diags) = merge_templates(
        main,
        "Pulumi.yaml",
        vec![
            ("Pulumi.buckets.yaml".to_string(), buckets),
            ("Pulumi.tables.yaml".to_string(), tables),
        ],
    );
    assert!(!merge_diags.has_errors());

    let td = merged.as_template_decl();
    let (order, sort_diags) = topological_sort(&td);
    assert!(!sort_diags.has_errors());

    let bucket_pos = order.iter().position(|n| n == "bucket").unwrap();
    let table_pos = order.iter().position(|n| n == "table").unwrap();
    assert!(
        bucket_pos < table_pos,
        "bucket should come before table: {:?}",
        order
    );
}

#[test]
fn test_cross_file_cycle_error() {
    let main_src = "name: test\nruntime: yaml\n";
    let a_src = "resources:\n  a:\n    type: test:A\n    properties:\n      ref: ${b.id}\n";
    let b_src = "resources:\n  b:\n    type: test:B\n    properties:\n      ref: ${a.id}\n";

    let (main, _) = parse_template(main_src, None);
    let (a, _) = parse_template(a_src, None);
    let (b, _) = parse_template(b_src, None);

    let (merged, merge_diags) = merge_templates(
        main,
        "Pulumi.yaml",
        vec![
            ("Pulumi.a.yaml".to_string(), a),
            ("Pulumi.b.yaml".to_string(), b),
        ],
    );
    assert!(!merge_diags.has_errors());

    let td = merged.as_template_decl();
    let (_, sort_diags) = topological_sort(&td);
    assert!(sort_diags.has_errors(), "should detect cycle across files");
}

// ---- load_project Integration ----

#[test]
fn test_load_project_single_file() {
    let dir = make_temp_project(&[(
        "Pulumi.yaml",
        "name: test\nruntime: yaml\nresources:\n  b:\n    type: test:Bucket\n",
    )]);
    let (merged, diags) = load_project(dir.path(), None);
    assert!(!diags.has_errors(), "errors: {}", diags);
    assert_eq!(merged.resource_count(), 1);
}

#[test]
fn test_load_project_multi_file() {
    let dir = make_temp_project(&[
        (
            "Pulumi.yaml",
            "name: test\nruntime: yaml\noutputs:\n  url: ${bucket.url}\n",
        ),
        (
            "Pulumi.buckets.yaml",
            "resources:\n  bucket:\n    type: test:Bucket\n",
        ),
    ]);
    let (merged, diags) = load_project(dir.path(), None);
    assert!(!diags.has_errors(), "errors: {}", diags);
    assert_eq!(merged.resource_count(), 1);
    assert_eq!(merged.output_count(), 1);
}

// ---- Evaluation with Multi-File ----

#[test]
fn test_multi_file_evaluation_with_mock() {
    let main_src = r#"
name: test
runtime: yaml
variables:
  prefix: my
outputs:
  bucketName: ${bucket.name}
"#;
    let buckets_src = r#"
resources:
  bucket:
    type: test:Bucket
    properties:
      name: ${prefix}-bucket
"#;

    let (main, _) = parse_template(main_src, None);
    let (buckets, _) = parse_template(buckets_src, None);

    let (merged, merge_diags) = merge_templates(
        main,
        "Pulumi.yaml",
        vec![("Pulumi.buckets.yaml".to_string(), buckets)],
    );
    assert!(!merge_diags.has_errors());

    let template = merged.as_template_decl();
    let template: &'static _ = Box::leak(Box::new(template));

    let mock = MockCallback::new();
    let mut eval = Evaluator::with_callback(
        "test".to_string(),
        "dev".to_string(),
        ".".to_string(),
        false,
        mock,
    );

    let raw_config = HashMap::new();
    eval.evaluate_template(template, &raw_config, &[]);
    assert!(!eval.diags.has_errors(), "eval errors: {}", eval.diags);

    // Check that the resource was registered
    let registrations = eval.callback().registrations.lock().unwrap();
    assert_eq!(registrations.len(), 1);
    assert_eq!(registrations[0].name, "bucket");
}

// ---- Jinja + Multi-File ----

#[test]
fn test_jinja_expressions_in_multi_file() {
    let dir = make_temp_project(&[
        ("Pulumi.yaml", "name: test\nruntime: yaml\n"),
        (
            "Pulumi.buckets.yaml",
            "resources:\n  bucket:\n    type: test:Bucket\n    properties:\n      name: \"{{ pulumi_project }}-bucket\"\n",
        ),
    ]);

    let config = HashMap::new();
    let ctx = JinjaContext {
        project_name: "myproj",
        stack_name: "dev",
        cwd: "/tmp",
        organization: "",
        root_directory: "",
        config: &config,
        project_dir: dir.path().to_str().unwrap(),
        undefined: UndefinedMode::Strict,
    };

    let (merged, diags) = load_project(dir.path(), Some(&ctx));
    assert!(!diags.has_errors(), "errors: {}", diags);
    assert_eq!(merged.resource_count(), 1);
}

#[test]
fn test_jinja_per_file_isolation() {
    let dir = make_temp_project(&[
        ("Pulumi.yaml", "name: test\nruntime: yaml\n"),
        (
            "Pulumi.a.yaml",
            "variables:\n  a: \"{{ pulumi_project }}\"\n",
        ),
        ("Pulumi.b.yaml", "variables:\n  b: \"{{ pulumi_stack }}\"\n"),
    ]);

    let config = HashMap::new();
    let ctx = JinjaContext {
        project_name: "myproj",
        stack_name: "dev",
        cwd: "/tmp",
        organization: "",
        root_directory: "",
        config: &config,
        project_dir: dir.path().to_str().unwrap(),
        undefined: UndefinedMode::Strict,
    };

    let (merged, diags) = load_project(dir.path(), Some(&ctx));
    assert!(!diags.has_errors(), "errors: {}", diags);
    assert_eq!(merged.variable_count(), 2);
}

#[test]
fn test_jinja_error_in_one_file_reports_correct_filename() {
    let dir = make_temp_project(&[
        ("Pulumi.yaml", "name: test\nruntime: yaml\n"),
        ("Pulumi.good.yaml", "resources:\n  a:\n    type: test:A\n"),
        (
            "Pulumi.bad.yaml",
            "resources:\n  b:\n    type: test:B\n    properties:\n      name: \"{{ undefined_var }}\"\n",
        ),
    ]);

    let config = HashMap::new();
    let ctx = JinjaContext {
        project_name: "test",
        stack_name: "dev",
        cwd: "/tmp",
        organization: "",
        root_directory: "",
        config: &config,
        project_dir: dir.path().to_str().unwrap(),
        undefined: UndefinedMode::Strict,
    };

    let (_, diags) = load_project(dir.path(), Some(&ctx));
    assert!(diags.has_errors(), "should have errors from bad.yaml");
    let err = diags.iter().find(|d| d.is_error()).unwrap();
    assert!(
        err.summary.contains("Pulumi.bad.yaml"),
        "error should mention the file: {}",
        err.summary
    );
}

#[test]
fn test_jinja_for_loop_generates_resources_cross_file_deps() {
    let config = HashMap::new();
    let ctx = JinjaContext {
        project_name: "test",
        stack_name: "dev",
        cwd: "/tmp",
        organization: "",
        root_directory: "",
        config: &config,
        project_dir: "/tmp",
        undefined: UndefinedMode::Strict,
    };
    let preprocessor = JinjaPreprocessor::new(&ctx);

    let buckets_jinja = r#"resources:
{% for i in range(3) %}
  "bucket{{ i }}":
    type: test:Bucket
    properties:
      name: "test-bucket-{{ i }}"
{% endfor %}
"#;
    let rendered_buckets = preprocessor
        .preprocess(buckets_jinja, "Pulumi.buckets.yaml")
        .unwrap();

    let tables_src =
        "resources:\n  table:\n    type: test:Table\n    properties:\n      ref: ${bucket0.id}\n";
    let main_src = "name: test\nruntime: yaml\n";

    let (main, _) = parse_template(main_src, None);
    let (buckets, _) = parse_template(rendered_buckets.as_ref(), None);
    let (tables, _) = parse_template(tables_src, None);

    let (merged, merge_diags) = merge_templates(
        main,
        "Pulumi.yaml",
        vec![
            ("Pulumi.buckets.yaml".to_string(), buckets),
            ("Pulumi.tables.yaml".to_string(), tables),
        ],
    );
    assert!(!merge_diags.has_errors(), "merge errors: {}", merge_diags);
    assert_eq!(merged.resource_count(), 4);

    let td = merged.as_template_decl();
    let (order, sort_diags) = topological_sort(&td);
    assert!(!sort_diags.has_errors(), "sort errors: {}", sort_diags);

    let bucket0_pos = order.iter().position(|n| n == "bucket0").unwrap();
    let table_pos = order.iter().position(|n| n == "table").unwrap();
    assert!(
        bucket0_pos < table_pos,
        "bucket0 should come before table: {:?}",
        order
    );
}

#[test]
fn test_jinja_filters_in_multi_file_properties() {
    let dir = make_temp_project(&[
        ("Pulumi.yaml", "name: test\nruntime: yaml\n"),
        (
            "Pulumi.buckets.yaml",
            "resources:\n  bucket:\n    type: test:Bucket\n    properties:\n      name: \"{{ pulumi_project | upper }}\"\n",
        ),
    ]);

    let config = HashMap::new();
    let ctx = JinjaContext {
        project_name: "myproj",
        stack_name: "dev",
        cwd: "/tmp",
        organization: "",
        root_directory: "",
        config: &config,
        project_dir: dir.path().to_str().unwrap(),
        undefined: UndefinedMode::Strict,
    };

    let (merged, diags) = load_project(dir.path(), Some(&ctx));
    assert!(!diags.has_errors(), "errors: {}", diags);
    assert_eq!(merged.resource_count(), 1);
}

#[test]
fn test_multi_file_jinja_full_pipeline() {
    let config = HashMap::new();
    let ctx = JinjaContext {
        project_name: "test",
        stack_name: "dev",
        cwd: "/tmp",
        organization: "",
        root_directory: "",
        config: &config,
        project_dir: "/tmp",
        undefined: UndefinedMode::Strict,
    };
    let preprocessor = JinjaPreprocessor::new(&ctx);

    let main_src = "name: test\nruntime: yaml\noutputs:\n  result: ${bucket0.name}\n";

    let storage_jinja = r#"resources:
{% for i in range(2) %}
  "bucket{{ i }}":
    type: test:Bucket
    properties:
      name: "{{ pulumi_project }}-{{ i }}"
{% endfor %}
"#;
    let rendered_storage = preprocessor
        .preprocess(storage_jinja, "Pulumi.storage.yaml")
        .unwrap();

    let compute_jinja = r#"resources:
{% if pulumi_stack == "dev" %}
  devVm:
    type: test:Vm
    properties:
      ref: ${bucket0.name}
{% endif %}
"#;
    let rendered_compute = preprocessor
        .preprocess(compute_jinja, "Pulumi.compute.yaml")
        .unwrap();

    let (main, _) = parse_template(main_src, None);
    let (storage, _) = parse_template(rendered_storage.as_ref(), None);
    let (compute, _) = parse_template(rendered_compute.as_ref(), None);

    let (merged, merge_diags) = merge_templates(
        main,
        "Pulumi.yaml",
        vec![
            ("Pulumi.storage.yaml".to_string(), storage),
            ("Pulumi.compute.yaml".to_string(), compute),
        ],
    );
    assert!(!merge_diags.has_errors(), "merge errors: {}", merge_diags);
    assert_eq!(merged.resource_count(), 3);

    let template = merged.as_template_decl();
    let template: &'static _ = Box::leak(Box::new(template));

    let mock = MockCallback::new();
    let mut eval = Evaluator::with_callback(
        "test".to_string(),
        "dev".to_string(),
        ".".to_string(),
        false,
        mock,
    );

    let raw_config = HashMap::new();
    eval.evaluate_template(template, &raw_config, &[]);
    assert!(!eval.diags.has_errors(), "eval errors: {}", eval.diags);

    let registrations = eval.callback().registrations.lock().unwrap();
    assert_eq!(registrations.len(), 3);

    let names: Vec<&str> = registrations.iter().map(|r| r.name.as_str()).collect();
    let bucket0_pos = names.iter().position(|n| *n == "bucket0").unwrap();
    let dev_vm_pos = names.iter().position(|n| *n == "devVm").unwrap();
    assert!(
        bucket0_pos < dev_vm_pos,
        "bucket0 should be registered before devVm: {:?}",
        names
    );
}

#[test]
fn test_multi_file_depends_on_cross_file() {
    let main_src = "name: test\nruntime: yaml\n";
    let a_src = "resources:\n  a:\n    type: test:A\n";
    let b_src =
        "resources:\n  b:\n    type: test:B\n    options:\n      dependsOn:\n        - ${a}\n";

    let (main, _) = parse_template(main_src, None);
    let (a, _) = parse_template(a_src, None);
    let (b, _) = parse_template(b_src, None);

    let (merged, merge_diags) = merge_templates(
        main,
        "Pulumi.yaml",
        vec![
            ("Pulumi.a.yaml".to_string(), a),
            ("Pulumi.b.yaml".to_string(), b),
        ],
    );
    assert!(!merge_diags.has_errors());

    let td = merged.as_template_decl();
    let (order, sort_diags) = topological_sort(&td);
    assert!(!sort_diags.has_errors());

    let a_pos = order.iter().position(|n| n == "a").unwrap();
    let b_pos = order.iter().position(|n| n == "b").unwrap();
    assert!(a_pos < b_pos, "a must come before b: {:?}", order);
}

#[test]
fn test_merge_three_files_many_resources() {
    let main_src = "name: test\nruntime: yaml\n";

    let mut file_a = String::from("resources:\n");
    for i in 0..4 {
        file_a.push_str(&format!(
            "  res_a{}:\n    type: test:A\n    properties:\n      idx: {}\n",
            i, i
        ));
    }

    let mut file_b = String::from("resources:\n");
    for i in 0..3 {
        file_b.push_str(&format!(
            "  res_b{}:\n    type: test:B\n    properties:\n      ref: ${{res_a0.id}}\n",
            i
        ));
    }

    let mut file_c = String::from("resources:\n");
    for i in 0..3 {
        file_c.push_str(&format!(
            "  res_c{}:\n    type: test:C\n    properties:\n      ref: ${{res_b0.id}}\n",
            i
        ));
    }

    let (main, _) = parse_template(main_src, None);
    let (a_template, _) = parse_template(&file_a, None);
    let (b_template, _) = parse_template(&file_b, None);
    let (c_template, _) = parse_template(&file_c, None);

    let (merged, merge_diags) = merge_templates(
        main,
        "Pulumi.yaml",
        vec![
            ("Pulumi.a.yaml".to_string(), a_template),
            ("Pulumi.b.yaml".to_string(), b_template),
            ("Pulumi.c.yaml".to_string(), c_template),
        ],
    );
    assert!(!merge_diags.has_errors(), "merge errors: {}", merge_diags);
    assert_eq!(merged.resource_count(), 10);

    let td = merged.as_template_decl();
    let (order, sort_diags) = topological_sort(&td);
    assert!(!sort_diags.has_errors());

    let a0_pos = order.iter().position(|n| n == "res_a0").unwrap();
    let b0_pos = order.iter().position(|n| n == "res_b0").unwrap();
    let c0_pos = order.iter().position(|n| n == "res_c0").unwrap();
    assert!(a0_pos < b0_pos);
    assert!(b0_pos < c0_pos);
}

#[test]
fn test_variable_collision_across_files() {
    let main_src = "name: test\nruntime: yaml\nvariables:\n  dup: hello\n";
    let extra_src = "variables:\n  dup: world\n";

    let (main, _) = parse_template(main_src, None);
    let (extra, _) = parse_template(extra_src, None);

    let (_, diags) = merge_templates(
        main,
        "Pulumi.yaml",
        vec![("Pulumi.extra.yaml".to_string(), extra)],
    );
    assert!(diags.has_errors());
    let err = diags.iter().find(|d| d.is_error()).unwrap();
    assert!(err.summary.contains("dup"));
}

#[test]
fn test_output_collision_across_files() {
    let main_src = "name: test\nruntime: yaml\noutputs:\n  result: hello\n";
    let extra_src = "outputs:\n  result: world\n";

    let (main, _) = parse_template(main_src, None);
    let (extra, _) = parse_template(extra_src, None);

    let (_, diags) = merge_templates(
        main,
        "Pulumi.yaml",
        vec![("Pulumi.extra.yaml".to_string(), extra)],
    );
    assert!(diags.has_errors());
    let err = diags.iter().find(|d| d.is_error()).unwrap();
    assert!(err.summary.contains("result"));
}

#[test]
fn test_load_project_missing_directory() {
    let (_, diags) = load_project(std::path::Path::new("/nonexistent/path"), None);
    assert!(diags.has_errors());
}

// ============================================================================
// Cross-file DAG validation tests
// ============================================================================

#[test]
fn test_cross_file_dag_order_enforced() {
    let main_src = r#"name: test
runtime: yaml
outputs:
  storageName: ${storageBucket.name}
  tableName: ${tableBucket.name}
"#;
    let storage_src = r#"resources:
  storageBucket:
    type: gcp:storage:Bucket
    properties:
      name: storage-bucket
"#;
    let tables_src = r#"resources:
  tableBucket:
    type: gcp:storage:Bucket
    properties:
      name: table-bucket
      label: ${storageBucket.name}
    options:
      dependsOn:
        - ${storageBucket}
"#;

    let (main, _) = parse_template(main_src, None);
    let (storage, _) = parse_template(storage_src, None);
    let (tables, _) = parse_template(tables_src, None);

    let (merged, merge_diags) = merge_templates(
        main,
        "Pulumi.yaml",
        vec![
            ("Pulumi.storage.yaml".to_string(), storage),
            ("Pulumi.tables.yaml".to_string(), tables),
        ],
    );
    assert!(!merge_diags.has_errors(), "merge errors: {}", merge_diags);

    let source_map = merged.source_map().clone();
    let template = merged.as_template_decl();

    let (order, diags) = topological_sort_with_sources(&template, Some(&source_map));
    assert!(!diags.has_errors(), "sort errors: {}", diags);

    let storage_pos = order.iter().position(|x| x == "storageBucket").unwrap();
    let table_pos = order.iter().position(|x| x == "tableBucket").unwrap();
    assert!(
        storage_pos < table_pos,
        "storageBucket should come before tableBucket: {:?}",
        order
    );

    // Verify source map
    assert_eq!(
        source_map.get("storageBucket").map(|s| s.as_str()),
        Some("Pulumi.storage.yaml")
    );
    assert_eq!(
        source_map.get("tableBucket").map(|s| s.as_str()),
        Some("Pulumi.tables.yaml")
    );
}

#[test]
fn test_cross_file_circular_reference_rich_error() {
    let main_src = "name: test\nruntime: yaml\n";
    let a_src = r#"resources:
  resourceA:
    type: test:Resource
    properties:
      dep: ${resourceB.id}
"#;
    let b_src = r#"resources:
  resourceB:
    type: test:Resource
    properties:
      dep: ${resourceA.id}
"#;

    let (main, _) = parse_template(main_src, None);
    let (a, _) = parse_template(a_src, None);
    let (b, _) = parse_template(b_src, None);

    let (merged, merge_diags) = merge_templates(
        main,
        "Pulumi.yaml",
        vec![
            ("Pulumi.a.yaml".to_string(), a),
            ("Pulumi.b.yaml".to_string(), b),
        ],
    );
    assert!(!merge_diags.has_errors(), "merge errors: {}", merge_diags);

    let source_map = merged.source_map().clone();
    let template = merged.as_template_decl();

    let (_, diags) = topological_sort_with_sources(&template, Some(&source_map));
    assert!(diags.has_errors(), "should detect circular dependency");

    let errors: Vec<String> = diags
        .iter()
        .filter(|d| d.is_error())
        .map(|d| d.summary.clone())
        .collect();
    assert!(
        errors.iter().any(|e| e.contains("circular dependency")),
        "should mention 'circular dependency', got: {:?}",
        errors
    );
    assert!(
        errors
            .iter()
            .any(|e| e.contains("resourceA") && e.contains("resourceB")),
        "should mention both resources in cycle path, got: {:?}",
        errors
    );
    assert!(
        errors
            .iter()
            .any(|e| e.contains("Pulumi.a.yaml") && e.contains("Pulumi.b.yaml")),
        "should mention both filenames, got: {:?}",
        errors
    );
}

#[test]
fn test_cross_file_missing_reference_with_suggestion() {
    let main_src = "name: test\nruntime: yaml\n";
    let storage_src = r#"resources:
  storageBucket:
    type: test:Resource
"#;
    let tables_src = r#"resources:
  tableBucket:
    type: test:Resource
    properties:
      dep: ${strageBucket.id}
"#;

    let (main, _) = parse_template(main_src, None);
    let (storage, _) = parse_template(storage_src, None);
    let (tables, _) = parse_template(tables_src, None);

    let (merged, merge_diags) = merge_templates(
        main,
        "Pulumi.yaml",
        vec![
            ("Pulumi.storage.yaml".to_string(), storage),
            ("Pulumi.tables.yaml".to_string(), tables),
        ],
    );
    assert!(!merge_diags.has_errors(), "merge errors: {}", merge_diags);

    let source_map = merged.source_map().clone();
    let template = merged.as_template_decl();

    let (_, diags) = topological_sort_with_sources(&template, Some(&source_map));
    assert!(diags.has_errors(), "should detect missing reference");

    let errors: Vec<String> = diags
        .iter()
        .filter(|d| d.is_error())
        .map(|d| d.summary.clone())
        .collect();
    assert!(
        errors.iter().any(|e| e.contains("not defined")),
        "should say 'not defined', got: {:?}",
        errors
    );
    assert!(
        errors
            .iter()
            .any(|e| e.contains("did you mean 'storageBucket'?")),
        "should suggest 'storageBucket', got: {:?}",
        errors
    );
    assert!(
        errors
            .iter()
            .any(|e| e.contains("defined in Pulumi.storage.yaml")),
        "should mention source file of suggestion, got: {:?}",
        errors
    );
}

#[test]
fn test_cross_file_output_references_validated() {
    let main_src = r#"name: test
runtime: yaml
outputs:
  name: ${unknownBucket.name}
"#;
    let storage_src = r#"resources:
  storageBucket:
    type: test:Resource
"#;

    let (main, _) = parse_template(main_src, None);
    let (storage, _) = parse_template(storage_src, None);

    let (merged, merge_diags) = merge_templates(
        main,
        "Pulumi.yaml",
        vec![("Pulumi.storage.yaml".to_string(), storage)],
    );
    assert!(!merge_diags.has_errors(), "merge errors: {}", merge_diags);

    let source_map = merged.source_map().clone();
    let template = merged.as_template_decl();

    let (_, diags) = topological_sort_with_sources(&template, Some(&source_map));
    assert!(
        diags.has_errors(),
        "should detect missing reference in output"
    );

    let errors: Vec<String> = diags
        .iter()
        .filter(|d| d.is_error())
        .map(|d| d.summary.clone())
        .collect();
    assert!(
        errors
            .iter()
            .any(|e| e.contains("unknownBucket") && e.contains("not defined")),
        "should flag unknownBucket in output, got: {:?}",
        errors
    );
}

// ============================================================================
// readFile() multi-file tests
// ============================================================================

#[test]
fn test_readfile_in_main_yaml() {
    let dir = make_temp_project(&[
        ("Pulumi.yaml", "name: test\nruntime: yaml\nresources:\n  r:\n    type: test:R\n    properties:\n      schema: |\n        {{ readFile(\"s.json\") }}\n"),
        ("s.json", "[\n  {\"id\": 1}\n]"),
    ]);

    let config = HashMap::new();
    let ctx = JinjaContext {
        project_name: "test",
        stack_name: "dev",
        cwd: dir.path().to_str().unwrap(),
        organization: "",
        root_directory: dir.path().to_str().unwrap(),
        config: &config,
        project_dir: dir.path().to_str().unwrap(),
        undefined: UndefinedMode::Strict,
    };

    let (merged, diags) = load_project(dir.path(), Some(&ctx));
    assert!(!diags.has_errors(), "errors: {}", diags);
    assert_eq!(merged.resource_count(), 1);
}

#[test]
fn test_readfile_in_satellite_yaml() {
    let dir = make_temp_project(&[
        ("Pulumi.yaml", "name: test\nruntime: yaml\n"),
        ("Pulumi.storage.yaml", "resources:\n  r:\n    type: test:R\n    properties:\n      config: |\n        {{ readFile(\"s.json\") }}\n"),
        ("s.json", "{\"key\": \"value\"}"),
    ]);

    let config = HashMap::new();
    let ctx = JinjaContext {
        project_name: "test",
        stack_name: "dev",
        cwd: dir.path().to_str().unwrap(),
        organization: "",
        root_directory: dir.path().to_str().unwrap(),
        config: &config,
        project_dir: dir.path().to_str().unwrap(),
        undefined: UndefinedMode::Strict,
    };

    let (merged, diags) = load_project(dir.path(), Some(&ctx));
    assert!(!diags.has_errors(), "errors: {}", diags);
    assert_eq!(merged.resource_count(), 1);
}

#[test]
fn test_readfile_in_multiple_files() {
    let dir = make_temp_project(&[
        ("Pulumi.yaml", "name: test\nruntime: yaml\nresources:\n  mainRes:\n    type: test:R\n    properties:\n      data: {{ readFile(\"main.txt\") }}\n"),
        ("Pulumi.extra.yaml", "resources:\n  extraRes:\n    type: test:R\n    properties:\n      data: {{ readFile(\"extra.txt\") }}\n"),
        ("main.txt", "main-content"),
        ("extra.txt", "extra-content"),
    ]);

    let config = HashMap::new();
    let ctx = JinjaContext {
        project_name: "test",
        stack_name: "dev",
        cwd: dir.path().to_str().unwrap(),
        organization: "",
        root_directory: dir.path().to_str().unwrap(),
        config: &config,
        project_dir: dir.path().to_str().unwrap(),
        undefined: UndefinedMode::Strict,
    };

    let (merged, diags) = load_project(dir.path(), Some(&ctx));
    assert!(!diags.has_errors(), "errors: {}", diags);
    assert_eq!(merged.resource_count(), 2);
}

#[test]
fn test_readfile_cross_file_with_evaluation() {
    let dir = make_temp_project(&[
        ("Pulumi.yaml", "name: test\nruntime: yaml\noutputs:\n  bucketName: ${bucket.name}\n"),
        ("Pulumi.storage.yaml", "resources:\n  bucket:\n    type: test:Bucket\n    properties:\n      name: {{ readFile(\"name.txt\") }}\n"),
        ("name.txt", "my-bucket"),
    ]);

    let config = HashMap::new();
    let ctx = JinjaContext {
        project_name: "test",
        stack_name: "dev",
        cwd: dir.path().to_str().unwrap(),
        organization: "",
        root_directory: dir.path().to_str().unwrap(),
        config: &config,
        project_dir: dir.path().to_str().unwrap(),
        undefined: UndefinedMode::Strict,
    };

    let (merged, diags) = load_project(dir.path(), Some(&ctx));
    assert!(!diags.has_errors(), "errors: {}", diags);

    let template = merged.as_template_decl();
    let template: &'static _ = Box::leak(Box::new(template));

    let mock = MockCallback::new();
    let mut eval = Evaluator::with_callback(
        "test".to_string(),
        "dev".to_string(),
        ".".to_string(),
        false,
        mock,
    );

    let raw_config = HashMap::new();
    eval.evaluate_template(template, &raw_config, &[]);
    assert!(!eval.diags.has_errors(), "eval errors: {}", eval.diags);

    let registrations = eval.callback().registrations.lock().unwrap();
    assert_eq!(registrations.len(), 1);
    assert_eq!(registrations[0].name, "bucket");
    assert_eq!(
        registrations[0].inputs.get("name").and_then(|v| v.as_str()),
        Some("my-bucket"),
    );
}

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use std::collections::HashMap;

use pulumi_rs_yaml_core::ast::parse::parse_template;
use pulumi_rs_yaml_core::eval::evaluator::Evaluator;
use pulumi_rs_yaml_core::eval::mock::MockCallback;
use pulumi_rs_yaml_core::eval::protobuf::{protobuf_to_value, value_to_protobuf};
use pulumi_rs_yaml_core::eval::value::Value;
use pulumi_rs_yaml_core::jinja::{
    has_jinja_block_syntax, strip_jinja_blocks, validate_rendered_yaml, JinjaContext,
    JinjaPreprocessor, NoopPreprocessor, TemplatePreprocessor, UndefinedMode,
};

fn bench_parse_simple(c: &mut Criterion) {
    let source = r#"
name: test
runtime: yaml
resources:
  myBucket:
    type: aws:s3:Bucket
    properties:
      bucketName: my-bucket
outputs:
  bucketArn: ${myBucket.arn}
"#;

    c.bench_function("parse_simple_template", |b| {
        b.iter(|| {
            let (template, _diags) = parse_template(black_box(source), None);
            black_box(template);
        })
    });
}

fn bench_parse_complex(c: &mut Criterion) {
    // Generate a template with 50 resources
    let mut yaml = String::from("name: bench\nruntime: yaml\nresources:\n");
    for i in 0..50 {
        yaml.push_str(&format!(
            "  res{}:\n    type: aws:s3:Bucket\n    properties:\n      bucketName: bucket-{}\n",
            i, i
        ));
    }
    yaml.push_str("outputs:\n");
    for i in 0..50 {
        yaml.push_str(&format!("  out{}: ${{res{}.arn}}\n", i, i));
    }

    c.bench_function("parse_50_resource_template", |b| {
        b.iter(|| {
            let (template, _diags) = parse_template(black_box(&yaml), None);
            black_box(template);
        })
    });
}

fn bench_eval_simple(c: &mut Criterion) {
    let source = r#"
name: bench
runtime: yaml
config:
  greeting:
    default: hello
variables:
  msg:
    fn::join:
      - " "
      - - ${greeting}
        - world
resources:
  myBucket:
    type: aws:s3:Bucket
    properties:
      bucketName: ${msg}
outputs:
  result: ${msg}
"#;

    c.bench_function("eval_simple_template", |b| {
        b.iter(|| {
            let (template, _diags) = parse_template(source, None);
            let template: &'static _ = Box::leak(Box::new(template));
            let mock = MockCallback::new();
            let mut eval = Evaluator::with_callback(
                "bench".to_string(),
                "dev".to_string(),
                ".".to_string(),
                false,
                mock,
            );
            let raw_config = HashMap::new();
            eval.evaluate_template(template, &raw_config, &[]);
            black_box(&eval.outputs);
        })
    });
}

fn bench_protobuf_round_trip(c: &mut Criterion) {
    use std::borrow::Cow;

    let value = Value::Object(vec![
        (Cow::from("name"), Value::String("test".into())),
        (Cow::from("count"), Value::Number(42.0)),
        (Cow::from("enabled"), Value::Bool(true)),
        (
            Cow::from("tags"),
            Value::List(vec![
                Value::String("a".into()),
                Value::String("b".into()),
                Value::String("c".into()),
            ]),
        ),
        (
            Cow::from("nested"),
            Value::Object(vec![
                (Cow::from("key"), Value::String("value".into())),
                (Cow::from("num"), Value::Number(3.15)),
            ]),
        ),
    ]);

    c.bench_function("protobuf_round_trip", |b| {
        b.iter(|| {
            let proto = value_to_protobuf(black_box(&value));
            let back = protobuf_to_value(black_box(&proto));
            black_box(back);
        })
    });
}

fn bench_topological_sort(c: &mut Criterion) {
    // Generate a template with 100 resources in a chain
    let mut yaml = String::from("name: bench\nruntime: yaml\nresources:\n");
    yaml.push_str("  res0:\n    type: aws:s3:Bucket\n    properties:\n      name: base\n");
    for i in 1..100 {
        yaml.push_str(&format!(
            "  res{}:\n    type: aws:s3:Bucket\n    properties:\n      name: ${{res{}.id}}\n",
            i,
            i - 1
        ));
    }

    let (template, _) = parse_template(&yaml, None);
    let template: &'static _ = Box::leak(Box::new(template));

    c.bench_function("topological_sort_100_chain", |b| {
        b.iter(|| {
            let (order, _diags) =
                pulumi_rs_yaml_core::eval::graph::topological_sort(black_box(template));
            black_box(order);
        })
    });
}

fn bench_config_resolution(c: &mut Criterion) {
    let source = r#"
name: bench
runtime: yaml
config:
  str1:
    default: hello
  num1:
    type: integer
    default: 42
  bool1:
    type: boolean
    default: true
  str2:
    default: world
  num2:
    type: number
    default: 3.15
"#;

    let (template, _) = parse_template(source, None);
    let template: &'static _ = Box::leak(Box::new(template));

    let mut raw_config = HashMap::new();
    raw_config.insert("bench:str1".to_string(), "override".to_string());
    raw_config.insert("bench:num1".to_string(), "99".to_string());

    c.bench_function("config_resolution_5_entries", |b| {
        b.iter(|| {
            let mock = MockCallback::new();
            let mut eval = Evaluator::with_callback(
                "bench".to_string(),
                "dev".to_string(),
                ".".to_string(),
                false,
                mock,
            );
            eval.evaluate_template(template, black_box(&raw_config), &[]);
            black_box(&eval.config);
        })
    });
}

fn bench_noop_preprocessor(c: &mut Criterion) {
    let source = r#"name: test
runtime: yaml
resources:
  myBucket:
    type: aws:s3:Bucket
    properties:
      bucketName: my-bucket
outputs:
  result: ${myBucket.arn}
"#;

    c.bench_function("noop_preprocessor_passthrough", |b| {
        let preprocessor = NoopPreprocessor;
        b.iter(|| {
            let result = preprocessor
                .preprocess(black_box(source), "Pulumi.yaml")
                .unwrap();
            black_box(result);
        })
    });
}

fn bench_jinja_fast_path(c: &mut Criterion) {
    let source = r#"name: test
runtime: yaml
resources:
  myBucket:
    type: aws:s3:Bucket
    properties:
      bucketName: my-bucket
outputs:
  result: ${myBucket.arn}
"#;

    let config = HashMap::new();
    let ctx = JinjaContext {
        project_name: "bench",
        stack_name: "dev",
        cwd: "/tmp",
        organization: "org",
        root_directory: "/home/user",
        config: &config,
        project_dir: "/home/user",
        undefined: UndefinedMode::Strict,
    };

    c.bench_function("jinja_preprocessor_fast_path", |b| {
        let preprocessor = JinjaPreprocessor::new(&ctx);
        b.iter(|| {
            let result = preprocessor
                .preprocess(black_box(source), "Pulumi.yaml")
                .unwrap();
            black_box(result);
        })
    });
}

fn bench_jinja_rendering(c: &mut Criterion) {
    let source = r#"name: {{ pulumi_project }}
runtime: yaml
resources:
{% for i in range(10) %}
  bucket{{ i }}:
    type: aws:s3:Bucket
    properties:
      bucketName: "{{ pulumi_project }}-{{ pulumi_stack }}-{{ i }}"
{% endfor %}
"#;

    let config = HashMap::new();
    let ctx = JinjaContext {
        project_name: "bench",
        stack_name: "dev",
        cwd: "/tmp",
        organization: "org",
        root_directory: "/home/user",
        config: &config,
        project_dir: "/home/user",
        undefined: UndefinedMode::Strict,
    };

    c.bench_function("jinja_preprocessor_render_10_resources", |b| {
        let preprocessor = JinjaPreprocessor::new(&ctx);
        b.iter(|| {
            let result = preprocessor
                .preprocess(black_box(source), "Pulumi.yaml")
                .unwrap();
            black_box(result);
        })
    });
}

fn bench_validate_rendered_yaml(c: &mut Criterion) {
    let yaml = r#"name: test
runtime: yaml
resources:
  bucket0:
    type: aws:s3:Bucket
    properties:
      bucketName: bucket-0
  bucket1:
    type: aws:s3:Bucket
    properties:
      bucketName: bucket-1
  bucket2:
    type: aws:s3:Bucket
    properties:
      bucketName: bucket-2
outputs:
  result: done
"#;

    c.bench_function("validate_rendered_yaml", |b| {
        b.iter(|| {
            let _ = black_box(validate_rendered_yaml(black_box(yaml), yaml, "Pulumi.yaml"));
        })
    });
}

fn bench_strip_jinja_blocks_50_resources(c: &mut Criterion) {
    // Generate a template with 50 resources in a for loop
    let mut source =
        String::from("name: bench\nruntime: yaml\nresources:\n{% for i in range(50) %}\n");
    for _ in 0..50 {
        source.push_str("  \"bucket{{ i }}\":\n    type: aws:s3:Bucket\n    properties:\n      name: \"bench-{{ i }}\"\n");
    }
    source.push_str("{% endfor %}\n");

    c.bench_function("strip_jinja_blocks_50_resources", |b| {
        b.iter(|| {
            let result = strip_jinja_blocks(black_box(&source));
            black_box(result);
        })
    });
}

fn bench_has_jinja_block_syntax(c: &mut Criterion) {
    // Large template with block syntax near the end
    let mut source = String::new();
    for i in 0..100 {
        source.push_str(&format!(
            "  res{}:\n    type: aws:s3:Bucket\n    properties:\n      name: bucket-{}\n",
            i, i
        ));
    }
    source.push_str("{% for i in range(3) %}\n  extra{{ i }}:\n    type: test\n{% endfor %}\n");

    c.bench_function("has_jinja_block_syntax_scan", |b| {
        b.iter(|| {
            let result = has_jinja_block_syntax(black_box(&source));
            black_box(result);
        })
    });
}

fn bench_merge_10_files_50_resources(c: &mut Criterion) {
    use pulumi_rs_yaml_core::multi_file::merge_templates;

    // Generate main template
    let main_src = "name: bench\nruntime: yaml\n";
    let (main_template, _) = parse_template(main_src, None);

    // Generate 10 additional files with 5 resources each
    let mut additional = Vec::new();
    for file_idx in 0..10 {
        let mut yaml = String::from("resources:\n");
        for res_idx in 0..5 {
            yaml.push_str(&format!(
                "  res_f{}_r{}:\n    type: aws:s3:Bucket\n    properties:\n      name: bucket-{}-{}\n",
                file_idx, res_idx, file_idx, res_idx
            ));
        }
        let (template, _) = parse_template(&yaml, None);
        additional.push((format!("Pulumi.file{}.yaml", file_idx), template));
    }

    c.bench_function("merge_10_files_50_resources", |b| {
        b.iter(|| {
            let (merged, _diags) = merge_templates(
                black_box(main_template.clone()),
                "Pulumi.yaml",
                black_box(additional.clone()),
            );
            black_box(merged);
        })
    });
}

fn bench_discover_project_files(c: &mut Criterion) {
    use pulumi_rs_yaml_core::multi_file::discover_project_files;

    // Create a temp directory with 10 Pulumi.*.yaml files
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Pulumi.yaml"),
        "name: bench\nruntime: yaml\n",
    )
    .unwrap();
    for i in 0..10 {
        std::fs::write(
            dir.path().join(format!("Pulumi.file{}.yaml", i)),
            format!("resources:\n  r{}:\n    type: test:R\n", i),
        )
        .unwrap();
    }
    // Add non-matching files to ensure they're filtered
    std::fs::write(dir.path().join("README.md"), "# bench\n").unwrap();
    std::fs::write(dir.path().join("other.yaml"), "data: true\n").unwrap();

    let path = dir.path().to_path_buf();

    c.bench_function("discover_project_files_10_extra", |b| {
        b.iter(|| {
            let files = discover_project_files(black_box(&path)).unwrap();
            black_box(files);
        })
    });
}

fn bench_jinja_preprocess_multi_file(c: &mut Criterion) {
    use pulumi_rs_yaml_core::multi_file::load_project;

    // Create temp project with 10 files, each with Jinja expressions
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Pulumi.yaml"),
        "name: bench\nruntime: yaml\n",
    )
    .unwrap();
    for i in 0..10 {
        let content = format!(
            "resources:\n  \"r{}\":\n    type: test:R\n    properties:\n      name: \"{{{{ pulumi_project }}}}-{}\"\n",
            i, i
        );
        std::fs::write(dir.path().join(format!("Pulumi.file{}.yaml", i)), content).unwrap();
    }

    let config = HashMap::new();
    let ctx = JinjaContext {
        project_name: "bench",
        stack_name: "dev",
        cwd: "/tmp",
        organization: "org",
        root_directory: "/home/user",
        config: &config,
        project_dir: dir.path().to_str().unwrap(),
        undefined: UndefinedMode::Strict,
    };

    let path = dir.path().to_path_buf();

    c.bench_function("jinja_preprocess_multi_file_10", |b| {
        b.iter(|| {
            let (merged, _diags) = load_project(black_box(&path), Some(&ctx));
            black_box(merged);
        })
    });
}

criterion_group!(
    benches,
    bench_parse_simple,
    bench_parse_complex,
    bench_eval_simple,
    bench_protobuf_round_trip,
    bench_topological_sort,
    bench_config_resolution,
    bench_noop_preprocessor,
    bench_jinja_fast_path,
    bench_jinja_rendering,
    bench_validate_rendered_yaml,
    bench_strip_jinja_blocks_50_resources,
    bench_has_jinja_block_syntax,
    bench_merge_10_files_50_resources,
    bench_discover_project_files,
    bench_jinja_preprocess_multi_file,
);
criterion_main!(benches);

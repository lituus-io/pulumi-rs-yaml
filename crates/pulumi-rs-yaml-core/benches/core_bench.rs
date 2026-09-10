// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

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

    // The same template behind a byte order mark. Every parse in the workspace
    // now pays for the check, so the pair has to show that the cost of the
    // marked path is the strip and nothing else — a three-byte comparison and a
    // subslice, not a copy. Benched at the template level rather than on
    // `strip_bom` alone: a nanosecond microbench sits under the CI job's 100 ns
    // floor and could never fail.
    let marked = format!("{}{source}", pulumi_rs_yaml_core::encoding::UTF8_BOM);
    c.bench_function("parse_simple_template_with_bom", |b| {
        b.iter(|| {
            let (template, _diags) = parse_template(black_box(marked.as_str()), None);
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
            let eval = Evaluator::with_callback(
                "bench".to_string(),
                "dev".to_string(),
                ".".to_string(),
                false,
                mock,
            );
            let raw_config = HashMap::new();
            eval.evaluate_template(template, &raw_config, &[]);
            black_box(&eval.state.outputs);
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
            let back = protobuf_to_value(black_box(proto));
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
            let eval = Evaluator::with_callback(
                "bench".to_string(),
                "dev".to_string(),
                ".".to_string(),
                false,
                mock,
            );
            eval.evaluate_template(template, black_box(&raw_config), &[]);
            black_box(&eval.state.config);
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
        provider_templated_packages: &[],
        extra: &HashMap::new(),
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
        provider_templated_packages: &[],
        extra: &HashMap::new(),
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

fn bench_jinja_render_fails(c: &mut Criterion) {
    // A render that fails on its last line, with a hundred lines before it.
    // What this measures is the diagnostic: locating the expression and
    // borrowing the line, which must stay a slice, never a copy — a
    // regression that started copying the source would show here first.
    let mut source = String::new();
    for i in 0..100 {
        source.push_str(&format!("key{i}: value{i}\n"));
    }
    source.push_str("last: {{ undefined_name }}\n");
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
        provider_templated_packages: &[],
        extra: &HashMap::new(),
    };
    c.bench_function("jinja_preprocessor_render_fails_undefined", |b| {
        let preprocessor = JinjaPreprocessor::new(&ctx);
        b.iter(|| {
            let result = preprocessor.preprocess(black_box(&source), "Pulumi.yaml");
            let Err(diag) = result else {
                unreachable!("the fixture must fail")
            };
            black_box((diag.line, diag.column, diag.expression.len()));
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
        provider_templated_packages: &[],
        extra: &HashMap::new(),
    };

    let path = dir.path().to_path_buf();

    c.bench_function("jinja_preprocess_multi_file_10", |b| {
        b.iter(|| {
            let (merged, _diags) = load_project(black_box(&path), Some(&ctx));
            black_box(merged);
        })
    });
}

#[cfg(feature = "sql-lineage")]
fn bench_sql_lineage_export(c: &mut Criterion) {
    use pulumi_rs_yaml_core::resource_graph::{export_resource_graph, GraphExportOptions};
    use pulumi_rs_yaml_core::sql_lineage::{export_sql_lineage, SqlLineageOptions};

    // 20 views, each reading 2 tables with explicit projections.
    let mut yaml = String::from(
        "name: bench\nruntime: yaml\nconfig:\n  gcp:project:\n    value: bench-proj\nresources:\n",
    );
    for i in 0..20 {
        yaml.push_str(&format!(
            "  view{i}:\n    type: gcp:bigquery:Table\n    properties:\n      datasetId: marts\n      tableId: view_{i}\n      view:\n        query: \"SELECT a.id, b.value AS v FROM `bench-proj.raw.t{i}` a JOIN `bench-proj.raw.u{i}` b ON a.id = b.id\"\n",
        ));
    }
    let (template, _) = pulumi_rs_yaml_core::ast::parse::parse_template(&yaml, None);
    let template = Box::leak(Box::new(template));
    let graph_opts = GraphExportOptions {
        organization: "org",
        project: "bench",
        stack: "dev",
        source_map: None,
        schema_store: None,
    };
    let (infra, _) = export_resource_graph(template, &graph_opts);
    let infra = Box::leak(Box::new(infra));

    c.bench_function("sql_lineage_20_views", |b| {
        b.iter(|| {
            let opts = SqlLineageOptions {
                organization: "org",
                project: "bench",
                stack: "dev",
                project_dir: None,
                default_bq_project: None,
                source_map: None,
                extra_sql_sources: &[],
            };
            let (lineage, _) = export_sql_lineage(template, infra, &opts);
            std::hint::black_box(lineage.edges.len())
        })
    });
}

#[cfg(not(feature = "sql-lineage"))]
fn bench_sql_lineage_export(_c: &mut Criterion) {}

/// The `str` regexp functions, answered in process.
///
/// The comparison that matters is not against another regex engine but against
/// what this replaced: a plugin process launch plus a gRPC round trip, tens of
/// milliseconds. These numbers exist so that gap stays visible, and so a
/// change that reintroduces per-call setup shows up as a step rather than as a
/// rumour. Compilation dominates the operation itself, which is why the split
/// and match cases are also measured with the pattern already compiled.
fn bench_native_str_regexp(c: &mut Criterion) {
    use pulumi_rs_yaml_core::eval::native_str::try_invoke;

    fn args(pairs: &[(&str, &str)]) -> HashMap<String, Value<'static>> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), Value::String((*v).to_string().into())))
            .collect()
    }

    // The shape that motivated this: stripping comments from a SQL file.
    let sql = "SELECT a, -- trailing\n/* block */ b FROM t\n".repeat(40);
    let strip = args(&[
        ("string", sql.as_str()),
        ("old", r"(--[^\n]*)|(/\*[\s\S]*?\*/)"),
        ("new", ""),
    ]);
    c.bench_function("native_str_regexp_replace_sql_comments", |b| {
        b.iter(|| black_box(try_invoke("str:regexp:replace", black_box(&strip))))
    });

    let csv = "field,".repeat(500);
    let split = args(&[("string", csv.as_str()), ("on", ",")]);
    c.bench_function("native_str_regexp_split_500_fields", |b| {
        b.iter(|| black_box(try_invoke("str:regexp:split", black_box(&split))))
    });

    let m = args(&[("string", "SELECT 1"), ("pattern", "^SELECT")]);
    c.bench_function("native_str_regexp_match", |b| {
        b.iter(|| black_box(try_invoke("str:regexp:match", black_box(&m))))
    });

    // The non-regex functions, for the contrast: no compilation at all.
    let plain = args(&[("string", "a-b-c-d-e"), ("old", "-"), ("new", "_")]);
    c.bench_function("native_str_index_replace", |b| {
        b.iter(|| black_box(try_invoke("str:index:replace", black_box(&plain))))
    });
}

/// Static literal resolution, measured through the graph exporter that
/// consumes it — `resolve_literal` itself is crate-private, and the exporter
/// is what the cost is actually paid by.
///
/// Three shapes, for three different questions. `plain_variable_chain` is the
/// path that existed before invokes were answered and must not move: it is
/// the regression guard. `invoke_derived_name` is what the new arms cost on a
/// name built the way real templates build one — a `replace` feeding a
/// `regexp:replace`. `wide_template` is the scale case, 1 000 variables of
/// which 200 are invokes, where the memo's absence for invoke outputs would
/// show up if recomputation were ever more than linear.
///
/// Each template is parsed once, outside the measured loop, so the numbers
/// are resolution and export rather than YAML parsing.
///
/// What the invoke cases measure is mostly regex compilation, not string
/// work: 200 plain `replace` evaluations in the wide case cost about 40 us
/// in total, while 20 reads of one `regexp:replace`-derived name cost about
/// 250 us, because invoke outputs are not memoised and each read compiles
/// the pattern again. That is the deliberate trade — the memo holds one
/// scalar per variable name and an invoke has a whole output object — and
/// these two cases are here so its price stays visible rather than assumed.
fn bench_resolve_literal(c: &mut Criterion) {
    use pulumi_rs_yaml_core::ast::template::TemplateDecl;
    use pulumi_rs_yaml_core::resource_graph::{export_resource_graph, GraphExportOptions};

    fn leak(source: &str) -> &'static TemplateDecl<'static> {
        let (template, diags) = parse_template(source, None);
        assert!(!diags.has_errors(), "bench template must parse: {}", diags);
        Box::leak(Box::new(template))
    }

    let opts = GraphExportOptions {
        organization: "org",
        project: "bench",
        stack: "dev",
        source_map: None,
        schema_store: None,
    };

    // A chain of plain variables, each interpolating the previous one, read
    // by 20 resources. No invoke anywhere: this is the untouched path.
    let mut plain = String::from("name: bench\nruntime: yaml\nvariables:\n  v0: seed\n");
    for i in 1..40 {
        plain.push_str(&format!("  v{}: ${{v{}}}-{}\n", i, i - 1, i));
    }
    plain.push_str("resources:\n");
    for i in 0..20 {
        plain.push_str(&format!(
            "  r{}:\n    type: gcp:storage:Bucket\n    properties:\n      name: acme-${{v39}}-{}\n      location: US\n",
            i, i
        ));
    }
    let plain = leak(&plain);
    c.bench_function("resolve_literal_plain_variable_chain", |b| {
        b.iter(|| black_box(export_resource_graph(black_box(plain), black_box(&opts))))
    });

    // The shape this change exists for: a name built from `str:replace`, then
    // reshaped by `str:regexp:replace`, read by 20 resources.
    let mut invoked = String::from(concat!(
        "name: bench\nruntime: yaml\nvariables:\n",
        "  process_nm: geo_fence_service\n",
        "  sanitized:\n    fn::str:replace:\n      string: ${process_nm}\n      old: '_'\n      new: '-'\n",
        "  versioned:\n    fn::str:regexp:replace:\n      string: ${sanitized.result}\n      old: '-(service)$'\n      new: '-$1-v2'\n",
        "resources:\n",
    ));
    for i in 0..20 {
        invoked.push_str(&format!(
            "  r{}:\n    type: gcp:storage:Bucket\n    properties:\n      name: acme-bkt-${{versioned.result}}-{}\n      location: US\n",
            i, i
        ));
    }
    let invoked = leak(&invoked);
    c.bench_function("resolve_literal_invoke_derived_name", |b| {
        b.iter(|| black_box(export_resource_graph(black_box(invoked), black_box(&opts))))
    });

    // 1 000 variables, 200 of them invokes, 50 resources reading them.
    let mut wide = String::from("name: bench\nruntime: yaml\nvariables:\n");
    for i in 0..800 {
        wide.push_str(&format!("  p{}: part_{}_value\n", i, i));
    }
    for i in 0..200 {
        wide.push_str(&format!(
            "  q{}:\n    fn::str:replace:\n      string: ${{p{}}}\n      old: '_'\n      new: '-'\n",
            i, i
        ));
    }
    wide.push_str("resources:\n");
    for i in 0..50 {
        wide.push_str(&format!(
            "  r{}:\n    type: gcp:storage:Bucket\n    properties:\n      name: ${{q{}.result}}\n      other: ${{p{}}}\n",
            i,
            i * 4,
            i * 16
        ));
    }
    let wide = leak(&wide);
    c.bench_function("resolve_literal_wide_template_200_invokes", |b| {
        b.iter(|| black_box(export_resource_graph(black_box(wide), black_box(&opts))))
    });
}

fn bench_checkpoint(c: &mut Criterion) {
    use pulumi_rs_yaml_core::checkpoint::{
        index_checkpoint, index_checkpoint_with_elements, index_checkpoints, ElementSpec, IdFilter,
    };

    /// A checkpoint in the shape a backend stores, padded with the `inputs`
    /// and `outputs` a real resource carries — the bytes the reader has to
    /// skip past are most of the work, so a bare `{urn, id}` fixture would
    /// measure the wrong thing. A hundred resources lands at roughly 37 KiB,
    /// which is the average size of a checkpoint in a shared backend.
    fn checkpoint_doc(resources: usize) -> Vec<u8> {
        let mut doc = String::from(r#"{"version":3,"checkpoint":{"latest":{"resources":["#);
        for i in 0..resources {
            if i > 0 {
                doc.push(',');
            }
            doc.push_str(&format!(
                concat!(
                    r#"{{"urn":"urn:pulumi:dev::app::gcp:workflows/workflow:Workflow::w{i}","#,
                    r#""custom":true,"id":"projects/p/locations/l/workflows/w{i}","#,
                    r#""type":"gcp:workflows/workflow:Workflow","#,
                    r#""inputs":{{"name":"w{i}","region":"a-region-1","project":"p","#,
                    r#""serviceAccount":"projects/p/serviceAccounts/sa@p.iam.example","#,
                    r#""sourceContents":"main:\n  steps:\n    - s{i}:\n        return: ok\n"}},"#,
                    r#""outputs":{{"id":"projects/p/locations/l/workflows/w{i}","#,
                    r#""name":"w{i}","state":"ACTIVE","revisionId":"000001-abc","#,
                    r#""createTime":"2026-01-01T00:00:00.000000Z"}},"#,
                    r#""dependencies":[],"propertyDependencies":{{}}}}"#,
                ),
                i = i
            ));
        }
        doc.push_str("]}}}");
        doc.into_bytes()
    }

    let doc = checkpoint_doc(100);
    c.bench_function("index_checkpoint_37kb", |b| {
        b.iter(|| black_box(index_checkpoint(black_box(&doc), black_box(None))))
    });

    // Ten leaf-only targets, built once: a filter is per scan, not per
    // document, and building it inside the loop would measure the wrong cost.
    let leaves: Vec<String> = (0..10).map(|i| format!("w{}", i * 7)).collect();
    let filter = IdFilter::new(leaves.iter().map(String::as_str));
    c.bench_function("index_checkpoint_filtered_37kb", |b| {
        b.iter(|| black_box(index_checkpoint(black_box(&doc), black_box(Some(&filter)))))
    });

    // The same document, read by a scan that asks for an element. Nothing in
    // this fixture is a type the spec names, so what this measures is the cost
    // every resource pays for the scan to be able to ask: capturing `inputs`
    // as a slice and reading `type`. That is the number that must stay next to
    // the unfiltered one — the projection itself is paid only by the handful
    // of resources that carry an element.
    let spec = ElementSpec::new([(
        "gcp:bigquery/datasetAccess:DatasetAccess",
        ["role", "userByEmail", "view", "authorizedDataset"],
    )]);
    c.bench_function("index_checkpoint_37kb_with_elements", |b| {
        b.iter(|| {
            black_box(index_checkpoint_with_elements(
                black_box(&doc),
                black_box(None),
                black_box(Some(&spec)),
            ))
        })
    });

    // A checkpoint of a hundred resources that DO carry an element, which is
    // the shape a dataset's `access[]` array makes: every id is the parent's,
    // and only the projection tells the rows apart.
    let elements = {
        let mut doc = String::from(r#"{"version":3,"checkpoint":{"latest":{"resources":["#);
        for i in 0..100 {
            if i > 0 {
                doc.push(',');
            }
            doc.push_str(&format!(
                concat!(
                    r#"{{"urn":"urn:pulumi:dev::app::gcp:bigquery/datasetAccess:DatasetAccess::a{i}","#,
                    r#""custom":true,"id":"projects/p/datasets/d","#,
                    r#""type":"gcp:bigquery/datasetAccess:DatasetAccess","#,
                    r#""inputs":{{"__defaults":[],"datasetId":"d","project":"p","#,
                    r#""role":"READER","userByEmail":"probe{i}@example.com"}},"#,
                    r#""outputs":{{"datasetId":"d","project":"p","role":"READER"}},"#,
                    r#""dependencies":[],"propertyDependencies":{{}}}}"#,
                ),
                i = i
            ));
        }
        doc.push_str("]}}}");
        doc.into_bytes()
    };
    c.bench_function("index_checkpoint_100_elements", |b| {
        b.iter(|| {
            black_box(index_checkpoint_with_elements(
                black_box(&elements),
                black_box(None),
                black_box(Some(&spec)),
            ))
        })
    });

    // A whole backend's worth of documents. These are ~2 KiB rather than
    // 37 KiB because 3,000 x 37 KiB is 111 MB of fixture, which measures the
    // allocator more than the reader; the per-document cost above is the
    // number to multiply.
    let small = checkpoint_doc(5);
    let batch: Vec<&[u8]> = vec![small.as_slice(); 3000];
    c.bench_function("index_checkpoints_3000_parallel8", |b| {
        b.iter(|| black_box(index_checkpoints(black_box(&batch), black_box(None), 8)))
    });
    c.bench_function("index_checkpoints_3000_sequential", |b| {
        b.iter(|| black_box(index_checkpoints(black_box(&batch), black_box(None), 1)))
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
    bench_jinja_render_fails,
    bench_validate_rendered_yaml,
    bench_strip_jinja_blocks_50_resources,
    bench_has_jinja_block_syntax,
    bench_merge_10_files_50_resources,
    bench_discover_project_files,
    bench_jinja_preprocess_multi_file,
    bench_sql_lineage_export,
    bench_native_str_regexp,
    bench_resolve_literal,
    bench_checkpoint,
);
criterion_main!(benches);

// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

// Scoping provider-rendered templates out of the runtime's Jinja pass.
//
// The fuzz targets under `fuzz/` carry the weight here — extent detection is a
// property, not a list of examples. What lives in this file is the part that
// reads better as prose: the case the feature exists for, the boundaries it
// must not cross, and a sweep over every YAML file in the tree.

use std::collections::HashMap;

use pulumi_rs_yaml_core::jinja::{
    validate_jinja_syntax, JinjaContext, JinjaPreprocessor, TemplatePreprocessor, UndefinedMode,
};
use pulumi_rs_yaml_core::provider_scope::{packages_from_source, protect, restore, Protected};

fn render(source: &str, packages: &[&str], mode: UndefinedMode) -> Result<String, String> {
    let mut config = HashMap::new();
    config.insert("gcpProject".to_owned(), "probe-project".to_owned());
    let extra = HashMap::new();
    let ctx = JinjaContext {
        project_name: "probe",
        stack_name: "dev",
        cwd: ".",
        organization: "org",
        root_directory: ".",
        config: &config,
        project_dir: ".",
        undefined: mode,
        provider_templated_packages: packages,
        extra: &extra,
    };
    JinjaPreprocessor::new(&ctx)
        .preprocess(source, "Pulumi.yaml")
        .map(|c| c.into_owned())
        .map_err(|e| e.to_string())
}

fn strict(source: &str) -> Result<String, String> {
    render(source, &["gcpx"], UndefinedMode::Strict)
}

// ---------------------------------------------------------------------------
// The case this exists for
// ---------------------------------------------------------------------------

/// Inline dbt SQL, written the way dbt is written, with no `{% raw %}` and no
/// `fn::readFile`. Both the expression form and the statement form survive,
/// in strict mode, which is the default.
#[test]
fn inline_dbt_survives_with_no_escaping() {
    let source = "\
name: probe
runtime: yaml
resources:
  dailyRevenue:
    type: gcpx:dbt/model:Model
    properties:
      project: \"{{ config.gcpProject }}\"
      sql: |
        {{ config(materialized='incremental', unique_key='outage_id') }}
        SELECT * FROM {{ ref('stg_outages') }}
        {% if is_incremental() %}
        WHERE updated_at > (SELECT MAX(updated_at) FROM {{ this }})
        {% endif %}
";
    let out = strict(source).expect("a scoped model must render");

    assert!(out.contains("{{ config(materialized='incremental', unique_key='outage_id') }}"));
    assert!(out.contains("SELECT * FROM {{ ref('stg_outages') }}"));
    assert!(out.contains("{% if is_incremental() %}"));
    assert!(out.contains("{% endif %}"));
    // The property outside the block scalar is still the runtime's to render.
    assert!(out.contains("project: \"probe-project\""));
}

/// Without the package listed, the same file is what it always was: the
/// runtime tries to render dbt syntax and fails. This is the control — the
/// feature has to be the reason the test above passes.
#[test]
fn the_same_file_without_the_package_listed_still_fails() {
    let source = "\
name: probe
runtime: yaml
resources:
  m:
    type: gcpx:dbt/model:Model
    properties:
      sql: |
        SELECT 1
        {% if is_incremental() %}WHERE 1=1{% endif %}
";
    assert!(render(source, &[], UndefinedMode::Strict).is_err());
    assert!(render(source, &[], UndefinedMode::Passthrough).is_err());
    assert!(render(source, &["gcpx"], UndefinedMode::Strict).is_ok());
}

/// The scope is the resource, not the file. A block scalar in a package that
/// was not listed is still the runtime's to render, and still fails.
#[test]
fn protection_does_not_leak_to_the_next_resource() {
    let source = "\
resources:
  scoped:
    type: gcpx:dbt/model:Model
    properties:
      sql: |
        {% if is_incremental() %}ok{% endif %}
  unscoped:
    type: aws:s3/bucket:Bucket
    properties:
      policy: |
        {% if is_incremental() %}not ok{% endif %}
";
    let err = strict(source).expect_err("the aws resource must still be rendered");
    assert!(err.contains("is_incremental"), "unexpected error: {err}");
}

/// Structural Jinja is why rendering has to precede parsing in the first place.
/// The pre-pass must not disturb it.
#[test]
fn structural_jinja_still_generates_resources() {
    let source = "\
resources:
{% for i in range(3) %}
  m{{ i }}:
    type: gcpx:dbt/model:Model
    properties:
      sql: |
        SELECT {{ ref('t') }} AS c
{% endfor %}
";
    let out = strict(source).expect("loop must still run");
    assert_eq!(out.matches("type: gcpx:dbt/model:Model").count(), 3);
    assert_eq!(out.matches("SELECT {{ ref('t') }} AS c").count(), 3);
    assert!(out.contains("m0:") && out.contains("m1:") && out.contains("m2:"));
}

// ---------------------------------------------------------------------------
// Extent detection
// ---------------------------------------------------------------------------

/// Key order is arbitrary in YAML. A single forward pass looking for `type:`
/// before `sql:` would miss this entirely.
#[test]
fn type_may_follow_the_block_scalar() {
    let source = "\
resources:
  m:
    properties:
      sql: |
        {% if is_incremental() %}x{% endif %}
    type: gcpx:dbt/model:Model
";
    assert!(strict(source).is_ok());
}

/// SQL whose content mimics the structure the scanner looks for. None of it
/// may end the extent early or start a phantom resource.
#[test]
fn sql_that_looks_like_yaml_is_still_sql() {
    let source = "\
resources:
  m:
    type: gcpx:dbt/model:Model
    properties:
      sql: |
        -- a comment containing  type: gcpx:dbt/model:Model
        SELECT '  sql: |' AS looks_like_a_key,
               'properties:' AS also_not_a_key
        ---
        ...
        {% if is_incremental() %}
        WHERE 1=1
        {% endif %}
      after: \"{{ config.gcpProject }}\"
";
    let out = strict(source).expect("adversarial SQL must not break the scan");
    assert!(out.contains("-- a comment containing  type: gcpx:dbt/model:Model"));
    assert!(out.contains("SELECT '  sql: |' AS looks_like_a_key,"));
    assert!(
        out.contains("\n        ---\n"),
        "a document marker inside SQL is SQL"
    );
    // The sibling key after the scalar was not swallowed: it still rendered.
    assert!(out.contains("after: \"probe-project\""));
}

/// Every block-scalar style, and the explicit indentation indicator the style
/// exists for.
#[test]
fn every_scalar_style_round_trips() {
    for style in ["|", "|-", "|+", ">", ">-", ">+", "|2", "|-2", "|+2"] {
        let source = format!(
            "\
resources:
  m:
    type: gcpx:dbt/model:Model
    properties:
      sql: {style}
        {{% if is_incremental() %}}x{{% endif %}}
        SELECT 1
      tail: done
"
        );
        let out = strict(&source).unwrap_or_else(|e| panic!("style {style} failed: {e}"));
        assert!(
            out.contains("{% if is_incremental() %}x{% endif %}"),
            "style {style} did not protect its content"
        );
        assert!(
            out.contains("tail: done"),
            "style {style} swallowed a sibling"
        );
    }
}

/// Blank lines and comment-looking lines inside a block scalar are content. A
/// line-based scanner that treats either as a separator truncates the extent.
#[test]
fn blank_lines_and_hashes_inside_a_scalar_are_content() {
    let source = "\
resources:
  m:
    type: gcpx:dbt/model:Model
    properties:
      sql: |
        SELECT 1

        # this is SQL, not a YAML comment
        {% if is_incremental() %}x{% endif %}
      tail: done
";
    let out = strict(source).expect("blank lines must not end the extent");
    assert!(out.contains("# this is SQL, not a YAML comment"));
    assert!(out.contains("{% if is_incremental() %}x{% endif %}"));
    assert!(out.contains("tail: done"));
}

/// Nested maps and sequence entries inside a scoped resource are still inside
/// it.
#[test]
fn the_scope_reaches_nested_structures() {
    let source = "\
resources:
  m:
    type: gcpx:dbt/model:Model
    properties:
      nested:
        body: |
          {% if is_incremental() %}a{% endif %}
      steps:
        - name: first
          script: |
            {% if is_incremental() %}b{% endif %}
";
    let out = strict(source).expect("nested scalars must be scoped too");
    assert!(out.contains("{% if is_incremental() %}a{% endif %}"));
    assert!(out.contains("{% if is_incremental() %}b{% endif %}"));
}

/// Tabs are illegal as YAML indentation and their width is not knowable, so no
/// extent below one can be trusted. Refusing is the point: a silently altered
/// model is worse than a rejected file.
#[test]
fn a_tab_in_indentation_is_refused_not_guessed() {
    let source = "resources:\n  m:\n    type: gcpx:dbt/model:Model\n    properties:\n\tsql: |\n        {% if is_incremental() %}x{% endif %}\n";
    let err = strict(source).expect_err("a tab in indentation must be refused");
    assert!(err.contains("tab"), "unexpected message: {err}");

    // With nothing to render there is nothing to alter, so the fast path skips
    // the pass and the tab is the YAML parser's problem, not ours.
    let no_jinja = "resources:\n  m:\n    type: gcpx:dbt/model:Model\n    properties:\n\tsql: |\n        SELECT 1\n";
    assert!(strict(no_jinja).is_ok());
}

/// A tab *inside* a block scalar is ordinary content — SQL is often
/// tab-aligned — and must not be mistaken for indentation.
#[test]
fn a_tab_inside_content_is_content() {
    let source = "\
resources:
  m:
    type: gcpx:dbt/model:Model
    properties:
      sql: |
        SELECT\t1
        \t-- tab-aligned
";
    let out = strict(source).expect("tabs in content are legal");
    assert!(out.contains("SELECT\t1"));
}

/// CRLF files must stay CRLF. The marker carries the line's own terminator.
#[test]
fn crlf_is_preserved() {
    let source = "resources:\r\n  m:\r\n    type: gcpx:dbt/model:Model\r\n    properties:\r\n      sql: |\r\n        {% if is_incremental() %}x{% endif %}\r\n";
    let out = strict(source).expect("CRLF must render");
    assert!(out.contains("{% if is_incremental() %}x{% endif %}"));
    // No mixed endings: every newline is still part of a CRLF pair. The file's
    // final newline is dropped by minijinja for every rendered file, protected
    // or not, so it is not counted here.
    assert_eq!(
        out.matches('\n').count(),
        out.matches("\r\n").count(),
        "an LF-only line ending appeared: {out:?}"
    );
}

/// The pass is a pair of inverses, and that is checkable directly.
#[test]
fn protect_and_restore_are_inverses() {
    let cases = [
        "",
        "resources:\n",
        "resources:\n  m:\n    type: gcpx:x:Y\n    properties:\n      sql: |\n        a\n        b\n",
        "resources:\n  m:\n    type: gcpx:x:Y\n    properties:\n      sql: |\n        a\n\n        b\n\n  n:\n    type: aws:x:Y\n",
        "a: |\n  x\n--- \nb: 1\n",
        "- type: gcpx:x:Y\n  sql: |\n    q\n- type: gcpx:x:Y\n  sql: |\n    r\n",
    ];
    for case in cases {
        let p = protect(case, &["gcpx"]).unwrap_or_else(|e| panic!("{case:?}: {e}"));
        let back = restore(p.source(case), p.regions()).expect("restore");
        assert_eq!(back.as_ref(), case, "round trip failed for {case:?}");
    }
}

/// The `exec` wrapper validates Jinja syntax on the raw file before anything
/// renders. Text scoped to a provider is not this runtime's to validate either:
/// dbt has tags minijinja has never heard of, and rejecting a file for one would
/// defeat the point of scoping it out.
#[test]
fn syntax_validation_is_scoped_too() {
    let source = "\
resources:
  m:
    type: gcpx:dbt/model:Model
    properties:
      sql: |
        {% snapshot outages_snapshot %}
        SELECT * FROM raw
        {% endsnapshot %}
";
    // minijinja does not know `snapshot`, so validating the raw file fails.
    assert!(
        validate_jinja_syntax(source, "Pulumi.yaml").is_err(),
        "the control must fail, or this test proves nothing"
    );

    // Once the block is set aside, there is nothing left for it to reject.
    let protected = protect(source, &["gcpx"]).expect("protect");
    assert!(
        validate_jinja_syntax(protected.source(source), "Pulumi.yaml").is_ok(),
        "a scoped block scalar must not be validated as this runtime's Jinja"
    );

    // And an unlisted package is still validated, so real mistakes still surface.
    let unlisted = source.replace("gcpx:dbt", "aws:dbt");
    let p2 = protect(&unlisted, &["gcpx"]).expect("protect");
    assert!(validate_jinja_syntax(p2.source(&unlisted), "Pulumi.yaml").is_err());
}

// ---------------------------------------------------------------------------
// The switch
// ---------------------------------------------------------------------------

#[test]
fn the_option_is_read_in_both_sequence_styles() {
    let flow = "name: p\nruntime:\n  name: yaml\n  options:\n    providerTemplatedPackages: [gcpx, other]\n";
    assert_eq!(packages_from_source(flow), vec!["gcpx", "other"]);

    let block = "name: p\nruntime:\n  name: yaml\n  options:\n    providerTemplatedPackages:\n      - gcpx\n      - \"other\"\n";
    assert_eq!(packages_from_source(block), vec!["gcpx", "other"]);
}

#[test]
fn the_option_is_not_read_from_the_wrong_place() {
    // Not under runtime.options.
    assert!(packages_from_source("providerTemplatedPackages: [gcpx]\n").is_empty());
    assert!(packages_from_source("options:\n  providerTemplatedPackages: [gcpx]\n").is_empty());
    // Not under a nested runtime key.
    assert!(
        packages_from_source("resources:\n  r:\n    runtime:\n      options:\n        providerTemplatedPackages: [gcpx]\n")
            .is_empty()
    );
    // Absent entirely.
    assert!(packages_from_source("name: p\nruntime: yaml\n").is_empty());
}

// ---------------------------------------------------------------------------
// Regression over the corpus
// ---------------------------------------------------------------------------

/// Every YAML file in the tree, through the pre-pass, with a generous package
/// list. This is the evidence that matters for a shared runtime: the pass runs
/// over real documents and gives every byte back.
#[test]
fn every_yaml_file_in_the_tree_round_trips() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root");

    let mut checked = 0usize;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if path.is_dir() {
                if !matches!(name.as_ref(), "target" | ".git" | "node_modules") {
                    stack.push(path);
                }
                continue;
            }
            if !name.ends_with(".yaml") && !name.ends_with(".yml") {
                continue;
            }
            let Ok(source) = std::fs::read_to_string(&path) else {
                continue;
            };
            for packages in [
                &["gcpx"][..],
                &["gcpx", "aws", "pulumi"][..],
                &["name", "type"][..],
            ] {
                // A refusal names a line; it never alters the file, so only a
                // successful pass has anything to check.
                if let Ok(p) = protect(&source, packages) {
                    let back = restore(p.source(&source), p.regions())
                        .unwrap_or_else(|e| panic!("{}: restore failed: {e}", path.display()));
                    assert_eq!(
                        back.as_ref(),
                        source,
                        "{} did not round trip with {packages:?}",
                        path.display()
                    );
                }
            }
            // Listing nothing must not even look at the file.
            assert!(
                matches!(protect(&source, &[]).unwrap(), Protected::Unchanged),
                "{} was altered by an empty package list",
                path.display()
            );
            checked += 1;
        }
    }
    assert!(
        checked > 20,
        "expected a real corpus, found {checked} files"
    );
}

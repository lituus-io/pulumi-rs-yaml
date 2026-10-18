// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

//! Byte-for-byte goldens for the two graph exports.
//!
//! Every stack under `tests/acceptance/` is loaded the way the exporters load
//! it -- `multi_file::load_project` with a Jinja context -- and exported to
//! the infrastructure graph and, with the feature, the SQL lineage graph. The
//! output, and the diagnostics, are compared with a stored copy.
//!
//! The point is the default: an optimisation or an opt-in feature may not move
//! a single byte of what a caller who asked for nothing new receives. When a
//! change MEANS to move output -- a correctness fix -- the goldens are
//! re-blessed in the same commit with `PULUMI_RS_YAML_BLESS=1`, and the diff
//! is the review.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use pulumi_rs_yaml_core::jinja::{JinjaContext, UndefinedMode};
use pulumi_rs_yaml_core::multi_file::load_project;
use pulumi_rs_yaml_core::resource_graph::{export_resource_graph, GraphExportOptions};

fn acceptance_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/acceptance")
}

fn golden_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/golden")
}

fn blessing() -> bool {
    std::env::var_os("PULUMI_RS_YAML_BLESS").is_some()
}

/// Absolute paths differ per checkout; everything else must not.
fn normalise(text: &str, dir: &Path) -> String {
    let canonical = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    text.replace(&*canonical.to_string_lossy(), "<STACK>")
        .replace(&*dir.to_string_lossy(), "<STACK>")
}

#[track_caller]
fn check(name: &str, file: &str, actual: &str) {
    let path = golden_dir().join(name).join(file);
    if blessing() {
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(&path, actual).expect("bless");
        return;
    }
    let expected = fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "missing golden {}: {e}; run with PULUMI_RS_YAML_BLESS=1 to create it",
            path.display()
        )
    });
    if expected != actual {
        let line = expected
            .lines()
            .zip(actual.lines())
            .position(|(a, b)| a != b)
            .map(|i| i + 1)
            .unwrap_or_else(|| expected.lines().count().min(actual.lines().count()) + 1);
        panic!(
            "{}/{} differs from its golden at line {line}\n--- expected\n{}\n--- actual\n{}",
            name,
            file,
            expected.lines().nth(line - 1).unwrap_or("<eof>"),
            actual.lines().nth(line - 1).unwrap_or("<eof>"),
        );
    }
}

fn stacks() -> Vec<String> {
    let mut out: Vec<String> = fs::read_dir(acceptance_dir())
        .expect("acceptance dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.path().join("Pulumi.yaml").is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    out.sort();
    out
}

fn export_one(name: &str) {
    let dir = acceptance_dir().join(name);
    let dir_s = dir.to_string_lossy().into_owned();
    let mut config = HashMap::new();
    config.insert("gcp:project".to_string(), "golden-project".to_string());
    let extra = HashMap::new();
    let ctx = JinjaContext {
        project_name: name,
        stack_name: "golden",
        cwd: &dir_s,
        organization: "org",
        root_directory: &dir_s,
        config: &config,
        project_dir: &dir_s,
        undefined: UndefinedMode::Strict,
        provider_templated_packages: &[],
        extra: &extra,
        limits: pulumi_rs_yaml_core::jinja::JinjaLimits::default(),
    };
    let (merged, load_diags) = load_project(&dir, Some(&ctx));
    if load_diags.has_errors() {
        // A stack this pipeline refuses is pinned too: the refusal's wording
        // is part of what a caller sees.
        check(
            name,
            "load_error.txt",
            &normalise(&load_diags.to_string(), &dir),
        );
        return;
    }
    let project = merged.name().unwrap_or("unknown").to_string();
    let template = merged.as_template_decl();
    let source_map = merged.source_map().clone();
    let opts = GraphExportOptions {
        organization: "org",
        project: &project,
        stack: "golden",
        source_map: Some(&source_map),
        schema_store: None,
        ..Default::default()
    };
    let (graph, diags) = export_resource_graph(&template, &opts);
    let json = graph.to_json().expect("graph json");
    check(name, "graph.json", &normalise(&json, &dir));
    check(
        name,
        "graph_diags.txt",
        &normalise(&diags.to_string(), &dir),
    );

    #[cfg(feature = "sql-lineage")]
    {
        use pulumi_rs_yaml_core::sql_lineage::{export_sql_lineage, SqlLineageOptions};
        let lopts = SqlLineageOptions {
            organization: "org",
            project: &project,
            stack: "golden",
            project_dir: Some(&dir),
            default_bq_project: None,
            source_map: Some(&source_map),
            extra_sql_sources: &[],
            ..Default::default()
        };
        let (lineage, ldiags) = export_sql_lineage(&template, &graph, &lopts);
        let ljson = lineage.to_json().expect("lineage json");
        check(name, "lineage.json", &normalise(&ljson, &dir));
        check(
            name,
            "lineage_diags.txt",
            &normalise(&ldiags.to_string(), &dir),
        );

        // The CLI's combined document goes through `serde_json::Value`, whose
        // maps are sorted; that ordering is observable and pinned separately.
        let mut doc = serde_json::to_value(&graph).expect("value");
        doc["sql_lineage"] = serde_json::to_value(&lineage).expect("value");
        let mut combined = serde_json::to_string_pretty(&doc).expect("pretty");
        combined.push('\n');
        check(name, "combined.json", &normalise(&combined, &dir));
    }
}

#[test]
fn every_acceptance_stack_exports_byte_identically() {
    // The graph and lineage stacks read `{{ env.gcp_project }}`, which the
    // renderer takes from `JINJA_VAR_*`. Pinned here so the goldens do not
    // depend on the machine; nothing else in this binary reads the variable.
    std::env::set_var("JINJA_VAR_GCP_PROJECT", "golden-gcp");
    let names = stacks();
    assert!(
        names.len() >= 20,
        "expected the acceptance corpus, found {}",
        names.len()
    );
    for name in &names {
        export_one(name);
    }
}

#[test]
fn every_golden_has_a_stack() {
    // A golden left behind by a removed stack would pass forever.
    if blessing() {
        return;
    }
    let names = stacks();
    for entry in fs::read_dir(golden_dir()).expect("golden dir").flatten() {
        let n = entry.file_name().to_string_lossy().into_owned();
        assert!(names.contains(&n), "golden '{n}' has no acceptance stack");
    }
}

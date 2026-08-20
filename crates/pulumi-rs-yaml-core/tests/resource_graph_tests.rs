// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

//! Integration tests for the resource dependency-graph export.
//!
//! Covers the full loading pipeline (multi-file discovery, Jinja
//! preprocessing) feeding `export_resource_graph`, JSON shape, and the
//! determinism / cross-stack ID contract.

use std::fs;

use pulumi_rs_yaml_core::ast::template::TemplateDecl;
use pulumi_rs_yaml_core::jinja::{JinjaContext, UndefinedMode};
use pulumi_rs_yaml_core::multi_file::load_project;
use pulumi_rs_yaml_core::resource_graph::{
    export_resource_graph, EdgeKind, GraphExportOptions, NodeKind, ResourceGraph,
};

fn make_temp_project(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for (name, content) in files {
        fs::write(dir.path().join(name), content).unwrap();
    }
    dir
}

fn export_project(
    dir: &std::path::Path,
    jinja: Option<&JinjaContext<'_>>,
) -> (
    ResourceGraph<'static>,
    pulumi_rs_yaml_core::diag::Diagnostics,
) {
    let (merged, load_diags) = load_project(dir, jinja);
    assert!(!load_diags.has_errors(), "load failed: {}", load_diags);
    let template: &'static TemplateDecl<'static> = Box::leak(Box::new(merged.as_template_decl()));
    let source_map: &'static _ = Box::leak(Box::new(merged.source_map().clone()));
    let opts = GraphExportOptions {
        organization: "org",
        project: "proj",
        stack: "dev",
        source_map: Some(source_map),
        schema_store: None,
    };
    export_resource_graph(template, &opts)
}

#[test]
fn multi_file_project_source_attribution() {
    let dir = make_temp_project(&[
        (
            "Pulumi.yaml",
            "name: proj\nruntime: yaml\nresources:\n  bucket:\n    type: gcp:storage:Bucket\n",
        ),
        (
            "Pulumi.net.yaml",
            "resources:\n  vpc:\n    type: gcp:compute:Network\n    properties:\n      ref: ${bucket.id}\n",
        ),
    ]);
    let (graph, diags) = export_project(dir.path(), None);
    assert!(!diags.has_errors(), "{}", diags);

    let bucket = graph
        .nodes
        .iter()
        .find(|n| n.logical_name == "bucket")
        .expect("bucket node");
    let vpc = graph
        .nodes
        .iter()
        .find(|n| n.logical_name == "vpc")
        .expect("vpc node");
    assert_eq!(bucket.source_file, Some("Pulumi.yaml"));
    assert_eq!(vpc.source_file, Some("Pulumi.net.yaml"));

    // Cross-file reference produces a typed edge.
    assert!(graph.edges.iter().any(|e| e.source_id == vpc.id
        && e.target_id == bucket.id
        && e.relationship == EdgeKind::References));
}

#[test]
fn jinja_preprocessed_project() {
    let dir = make_temp_project(&[(
        "Pulumi.yaml",
        "name: proj\nruntime: yaml\nresources:\n  bucket:\n    type: gcp:storage:Bucket\n    properties:\n      location: \"{{ pulumi_stack }}\"\n",
    )]);
    let config = std::collections::HashMap::new();
    let extra = std::collections::HashMap::new();
    let ctx = JinjaContext {
        project_name: "proj",
        stack_name: "dev",
        cwd: "/tmp",
        organization: "org",
        root_directory: "/tmp",
        config: &config,
        project_dir: "/tmp",
        undefined: UndefinedMode::Strict,
        provider_templated_packages: &[],
        extra: &extra,
    };
    let (graph, diags) = export_project(dir.path(), Some(&ctx));
    assert!(!diags.has_errors(), "{}", diags);
    let bucket = graph
        .nodes
        .iter()
        .find(|n| n.logical_name == "bucket")
        .expect("bucket node");
    // The Jinja-rendered value is a plain literal for the export.
    assert!(bucket
        .literal_properties
        .iter()
        .any(|(k, v)| k == "location" && v == "dev"));
}

#[test]
fn json_shape() {
    let dir = make_temp_project(&[(
        "Pulumi.yaml",
        "name: proj\nruntime: yaml\nresources:\n  bucket:\n    type: gcp:storage:Bucket\n    properties:\n      location: US\noutputs:\n  id: ${bucket.id}\n",
    )]);
    let (graph, _) = export_project(dir.path(), None);
    let json = graph.to_json().expect("serializes");
    let value: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");

    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["organization"], "org");
    assert_eq!(value["project"], "proj");
    assert_eq!(value["stack"], "dev");

    let nodes = value["nodes"].as_array().expect("nodes array");
    // stack + bucket + output
    assert_eq!(nodes.len(), 3);
    let bucket = nodes
        .iter()
        .find(|n| n["logical_name"] == "bucket")
        .expect("bucket");
    assert_eq!(bucket["kind"], "resource");
    assert_eq!(bucket["type_token"], "gcp:storage/bucket:Bucket");
    assert_eq!(bucket["literal_properties"]["location"], "US");

    let edges = value["edges"].as_array().expect("edges array");
    assert!(edges.iter().all(|e| e["source_id"].is_string()
        && e["target_id"].is_string()
        && e["relationship"].is_string()
        && e["property_paths"].is_array()
        && e["stack"] == "dev"));
    assert!(edges.iter().any(|e| e["relationship"] == "exports"));
    assert!(edges.iter().any(|e| e["relationship"] == "contains"));
}

#[test]
fn export_is_deterministic_across_loads() {
    let files: &[(&str, &str)] = &[
        (
            "Pulumi.yaml",
            "name: proj\nruntime: yaml\nresources:\n  z:\n    type: test:mod:Z\n  a:\n    type: test:mod:A\n    properties:\n      ref: ${z.id}\n",
        ),
        (
            "Pulumi.extra.yaml",
            "resources:\n  m:\n    type: test:mod:M\n    options:\n      dependsOn:\n        - ${a}\n",
        ),
    ];
    let dir1 = make_temp_project(files);
    let dir2 = make_temp_project(files);
    let (g1, _) = export_project(dir1.path(), None);
    let (g2, _) = export_project(dir2.path(), None);
    assert_eq!(
        g1.to_json().expect("json"),
        g2.to_json().expect("json"),
        "independent loads of identical projects export byte-identical JSON"
    );
}

#[test]
fn cross_stack_contract_through_full_pipeline() {
    // Producer project.
    let producer_dir = make_temp_project(&[(
        "Pulumi.yaml",
        "name: producer\nruntime: yaml\nresources:\n  ds:\n    type: gcp:bigquery:Dataset\n    properties:\n      datasetId: analytics\noutputs:\n  datasetId: ${ds.datasetId}\n",
    )]);
    let (producer_merged, _) = load_project(producer_dir.path(), None);
    let producer_template: &'static TemplateDecl<'static> =
        Box::leak(Box::new(producer_merged.as_template_decl()));
    let (producer, _) = export_resource_graph(
        producer_template,
        &GraphExportOptions {
            organization: "org",
            project: "data-platform",
            stack: "prod",
            source_map: None,
            schema_store: None,
        },
    );
    let output_id = &producer
        .nodes
        .iter()
        .find(|n| n.kind == NodeKind::Output)
        .expect("output node")
        .id;

    // Consumer project in a different stack references it.
    let consumer_dir = make_temp_project(&[(
        "Pulumi.yaml",
        "name: consumer\nruntime: yaml\nresources:\n  up:\n    type: pulumi:pulumi:StackReference\n    properties:\n      name: org/data-platform/prod\n  t:\n    type: gcp:bigquery:Table\n    properties:\n      ds: ${up.outputs.datasetId}\n",
    )]);
    let (consumer, diags) = export_project(consumer_dir.path(), None);
    assert!(!diags.has_errors(), "{}", diags);
    assert!(
        consumer
            .edges
            .iter()
            .any(|e| e.relationship == EdgeKind::ConsumesStackOutput && &e.target_id == output_id),
        "consumer cross-stack edge must target the producer's output id exactly"
    );
}

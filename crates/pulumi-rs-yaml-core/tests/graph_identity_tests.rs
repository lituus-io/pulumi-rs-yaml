// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

//! Acceptance fixtures for stack identity, reference backends, cross-stack
//! resolution and the IAM graph with host-supplied config.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use pulumi_rs_yaml_core::cross_stack::resolve_cross_stack;
use pulumi_rs_yaml_core::iam_graph::{export_iam_graph, MemberSource};
use pulumi_rs_yaml_core::multi_file::load_project;
use pulumi_rs_yaml_core::resource_graph::{
    export_resource_graph, GraphExportOptions, ResourceGraph,
};

fn acceptance(dir: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/acceptance")
        .join(dir)
}

/// Loads a stack directory; the template lives for the test.
fn load(dir: &Path) -> &'static pulumi_rs_yaml_core::ast::template::TemplateDecl<'static> {
    let (merged, diags) = load_project(dir, None);
    assert!(!diags.has_errors(), "{}: {diags}", dir.display());
    let (template, _) = merged.into_parts();
    Box::leak(Box::new(template))
}

// ---------------------------------------------------------------------------
// Multi-stack, one GCS prefix per stack directory
// ---------------------------------------------------------------------------

const STACKS: [&str; 3] = ["network", "warehouse", "reporting"];

fn export_layout(exact: bool) -> Vec<ResourceGraph<'static>> {
    let refs: &'static HashMap<String, String> = Box::leak(Box::new(
        [
            ("network/network", "gs://state-bucket/network"),
            ("warehouse/warehouse", "gs://state-bucket/warehouse"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect(),
    ));
    STACKS
        .iter()
        .map(|&name| {
            let template = load(&acceptance("graph-multistack-gcs").join(name));
            let backend: &'static str =
                Box::leak(format!("gs://state-bucket/{name}").into_boxed_str());
            let opts = GraphExportOptions {
                // The program says nothing about identity; the host deploys
                // each directory as project = stack = its name.
                project: template.name.as_deref().unwrap_or(name),
                stack: "dev",
                project_override: Some(name),
                stack_override: Some(name),
                backend_url: Some(backend),
                reference_backends: exact.then_some(refs),
                ..Default::default()
            };
            let (g, diags) = export_resource_graph(template, &opts);
            assert!(!diags.has_errors(), "{diags}");
            g
        })
        .collect()
}

fn node<'g>(
    g: &'g ResourceGraph<'static>,
    logical: &str,
) -> &'g pulumi_rs_yaml_core::resource_graph::GraphNode<'static> {
    g.nodes
        .iter()
        .find(|n| n.logical_name == logical)
        .unwrap_or_else(|| panic!("no node {logical}"))
}

#[test]
fn every_reference_points_at_the_exact_state_object() {
    let graphs = export_layout(true);
    assert_eq!(
        graphs[1].stack_uri.as_deref(),
        Some("gs://state-bucket/warehouse/.pulumi/stacks/warehouse/warehouse.json")
    );
    for (g, logical, want) in [
        (
            &graphs[1],
            "net",
            "gs://state-bucket/network/.pulumi/stacks/network/network.json",
        ),
        (
            &graphs[2],
            "net",
            "gs://state-bucket/network/.pulumi/stacks/network/network.json",
        ),
        (
            &graphs[2],
            "wh",
            "gs://state-bucket/warehouse/.pulumi/stacks/warehouse/warehouse.json",
        ),
    ] {
        let n = node(g, logical);
        assert_eq!(n.referenced_stack_uri.as_deref(), Some(want), "{logical}");
        assert_eq!(n.resolved, Some("exact"));
    }
}

/// The same-bucket guess the export made before `reference_backends`: the
/// consumer's own prefix, which holds no such stack.
#[test]
fn without_the_map_the_guess_is_the_consumers_own_prefix() {
    let graphs = export_layout(false);
    assert_eq!(
        node(&graphs[2], "wh").referenced_stack_uri.as_deref(),
        Some("gs://state-bucket/reporting/.pulumi/stacks/warehouse/warehouse.json")
    );
    assert_eq!(node(&graphs[2], "wh").resolved, None);
}

#[test]
fn cross_stack_links_reach_the_exporting_resources() {
    let graphs = export_layout(true);
    let links = resolve_cross_stack(&graphs);
    let got: Vec<(&str, &str, &str)> = links
        .iter()
        .map(|l| {
            (
                l.consumer_edge.source_id,
                l.exporter_resource_id,
                l.output_key,
            )
        })
        .collect();
    assert_eq!(
        got,
        vec![
            (
                "urn:pulumi:reporting::reporting::gcp:bigquery/table:Table::daily",
                "urn:pulumi:network::network::gcp:bigquery/dataset:Dataset::shared",
                "datasetId"
            ),
            (
                "urn:pulumi:reporting::reporting::gcp:bigquery/table:Table::daily",
                "urn:pulumi:warehouse::warehouse::gcp:bigquery/table:Table::events",
                "tableId"
            ),
            (
                "urn:pulumi:warehouse::warehouse::gcp:bigquery/table:Table::events",
                "urn:pulumi:network::network::gcp:bigquery/dataset:Dataset::shared",
                "datasetId"
            ),
            (
                "urn:pulumi:warehouse::warehouse::gcp:storage/bucketObject:BucketObject::load",
                "urn:pulumi:network::network::gcp:storage/bucket:Bucket::landing",
                "bucketName"
            ),
        ]
    );
}

// ---------------------------------------------------------------------------
// IAM bindings shaped like a data platform's access stack
// ---------------------------------------------------------------------------

/// What a host does with `Pulumi.<stack>.yaml`: keep the plaintext values,
/// drop every `secure:` one.
fn plaintext_stack_config(path: &Path) -> HashMap<String, String> {
    let text = std::fs::read_to_string(path).expect("stack file");
    let doc: serde_yaml::Value = serde_yaml::from_str(&text).expect("yaml");
    let mut out = HashMap::new();
    if let Some(cfg) = doc.get("config").and_then(|c| c.as_mapping()) {
        for (k, v) in cfg {
            if let (Some(k), Some(v)) = (k.as_str(), v.as_str()) {
                out.insert(k.to_string(), v.to_string());
            }
        }
    }
    out
}

fn iam_export(with_config: bool) -> pulumi_rs_yaml_core::iam_graph::IamGraph<'static> {
    let dir = acceptance("graph-iam-bindings");
    let template = load(&dir);
    let config: &'static HashMap<String, String> = Box::leak(Box::new(plaintext_stack_config(
        &dir.join("Pulumi.dev.yaml"),
    )));
    assert!(
        !config.keys().any(|k| k.ends_with("apiToken")),
        "the host drops secure values"
    );
    let opts = GraphExportOptions {
        organization: "org",
        project: "graph-iam-bindings",
        stack: "dev",
        config: with_config.then_some(config),
        ..Default::default()
    };
    let (g, diags) = export_iam_graph(template, &opts);
    assert!(!diags.has_errors(), "{diags}");
    g
}

#[test]
fn every_binding_is_accounted_for_and_coverage_meets_the_bar() {
    let g = iam_export(true);
    let s = g.summary;
    assert_eq!(s.bindings, 67);
    assert_eq!(s.members, 67);
    assert_eq!(s.member_resolved, 63, "48 literal + 15 config");
    assert_eq!(s.member_from_config, 15);
    assert_eq!(s.member_from_resource, 4);
    assert_eq!(s.member_unresolved, 0, "no binding unaccounted for");
    let coverage = s.member_resolved as f64 / s.members as f64;
    assert!(coverage >= 0.94, "{coverage}");
    // The four resource-output members start at the two service accounts.
    let from_resource: Vec<&str> = g
        .edges
        .iter()
        .filter(|e| e.member_source == MemberSource::Resource)
        .map(|e| e.source_id.as_ref())
        .collect();
    assert_eq!(from_resource.len(), 4);
    assert!(from_resource
        .iter()
        .all(|id| id.contains("gcp:serviceaccount/account:Account::")));
    // Emails are canonical: the mixed-case literal joins its lowercase twin.
    assert!(g
        .nodes
        .iter()
        .any(|n| n.id == "principal::user:mixed.case1@example.com"));
    assert!(!g.to_json().expect("json").contains("CIPHERTEXT"));
}

#[test]
fn without_host_config_the_config_members_stay_unresolved() {
    let s = iam_export(false).summary;
    assert_eq!(s.member_resolved, 48);
    assert_eq!(s.member_unresolved, 15);
    assert_eq!(s.member_from_resource, 4);
    assert_eq!(s.member_from_config, 0);
}

// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

//! Integration tests for the SQL lineage export: full pipeline from
//! template text through the infra graph join, cross-stack ID
//! contracts, components, declared lineage, and determinism.
#![cfg(feature = "sql-lineage")]

use std::collections::HashMap;
use std::fs;

use pulumi_rs_yaml_core::ast::parse::parse_template;
use pulumi_rs_yaml_core::ast::template::TemplateDecl;
use pulumi_rs_yaml_core::diag::Diagnostics;
use pulumi_rs_yaml_core::resource_graph::{
    export_resource_graph, GraphExportOptions, ResourceGraph,
};
use pulumi_rs_yaml_core::sql_lineage::{
    export_sql_lineage, DataEdgeKind, DataNodeKind, Resolution, SqlLineageGraph, SqlLineageOptions,
};

struct Export {
    lineage: SqlLineageGraph<'static>,
    #[allow(dead_code)]
    infra: &'static ResourceGraph<'static>,
    #[allow(dead_code)]
    diags: Diagnostics,
}

fn export(yaml: &str, project: &'static str, stack: &'static str) -> Export {
    export_with_dir(yaml, project, stack, None)
}

fn export_with_dir(
    yaml: &str,
    project: &'static str,
    stack: &'static str,
    dir: Option<&'static std::path::Path>,
) -> Export {
    let (template, parse_diags) = parse_template(yaml, None);
    assert!(!parse_diags.has_errors(), "parse failed: {}", parse_diags);
    let template: &'static TemplateDecl<'static> = Box::leak(Box::new(template));
    let graph_opts = GraphExportOptions {
        organization: "org",
        project,
        stack,
        source_map: None,
        schema_store: None,
    };
    let (infra, _) = export_resource_graph(template, &graph_opts);
    let infra: &'static ResourceGraph<'static> = Box::leak(Box::new(infra));
    let opts = SqlLineageOptions {
        organization: "org",
        project,
        stack,
        project_dir: dir,
        default_bq_project: None,
        source_map: None,
        extra_sql_sources: &[],
    };
    let (lineage, diags) = export_sql_lineage(template, infra, &opts);
    Export {
        lineage,
        infra,
        diags,
    }
}

fn node<'g>(
    g: &'g SqlLineageGraph<'static>,
    id: &str,
) -> &'g pulumi_rs_yaml_core::sql_lineage::DataNode<'static> {
    g.nodes.iter().find(|n| n.id == id).unwrap_or_else(|| {
        panic!(
            "no node '{}'; have: {:?}",
            id,
            g.nodes.iter().map(|n| n.id.as_ref()).collect::<Vec<_>>()
        )
    })
}

fn has_edge(g: &SqlLineageGraph<'static>, source: &str, target: &str, kind: DataEdgeKind) -> bool {
    g.edges
        .iter()
        .any(|e| e.source_id == source && e.target_id == target && e.relationship == kind)
}

const PRODUCER: &str = r#"name: data-platform
runtime: yaml
config:
  gcp:project:
    value: data-proj
resources:
  ds:
    type: gcp:bigquery:Dataset
    properties:
      datasetId: analytics
      description: Analytics dataset
  base:
    type: gcp:bigquery:Table
    properties:
      datasetId: analytics
      tableId: orders
      schema: '[{"name":"order_id","type":"STRING","description":"Order key"},{"name":"amount_cents","type":"INTEGER"}]'
  revenueView:
    type: gcp:bigquery:Table
    properties:
      datasetId: analytics
      tableId: revenue_view
      view:
        query: "SELECT o.order_id, o.amount_cents / 100 AS revenue FROM `data-proj.analytics.orders` o"
        useLegacySql: false
"#;

#[test]
fn view_lineage_table_and_column_level() {
    let e = export(PRODUCER, "data-platform", "prod");
    // Entities.
    let view = node(&e.lineage, "bq://data-proj/analytics/revenue_view");
    assert_eq!(view.kind, DataNodeKind::View);
    assert!(view
        .defined_by_urn
        .as_deref()
        .is_some_and(|u| u.contains("revenueView")));
    let base_col = node(&e.lineage, "bq://data-proj/analytics/orders#order_id");
    assert_eq!(base_col.description.as_deref(), Some("Order key"));
    assert_eq!(base_col.data_type.as_deref(), Some("STRING"));
    // Table-level derivation.
    assert!(has_edge(
        &e.lineage,
        "bq://data-proj/analytics/revenue_view",
        "bq://data-proj/analytics/orders",
        DataEdgeKind::DerivesFrom
    ));
    // Column-level derivation.
    assert!(has_edge(
        &e.lineage,
        "bq://data-proj/analytics/revenue_view#revenue",
        "bq://data-proj/analytics/orders#amount_cents",
        DataEdgeKind::ColumnDerivesFrom
    ));
    // Containment chain.
    assert!(has_edge(
        &e.lineage,
        "bq://data-proj",
        "bq://data-proj/analytics",
        DataEdgeKind::Contains
    ));
    // Parsed resolution recorded.
    assert!(e
        .lineage
        .edges
        .iter()
        .any(|edge| edge.relationship == DataEdgeKind::ColumnDerivesFrom
            && edge.resolution == Resolution::Parsed));
}

#[test]
fn cross_stack_ids_join_at_table_and_column_level() {
    let producer = export(PRODUCER, "data-platform", "prod");
    // Consumer in a different pulumi project/stack refines the view by name.
    let consumer_yaml = r#"name: consumer
runtime: yaml
resources:
  refined:
    type: gcp:bigquery:Table
    properties:
      project: data-proj
      datasetId: marts
      tableId: refined_revenue
      view:
        query: "SELECT r.order_id, r.revenue * 2 AS boosted FROM `data-proj.analytics.revenue_view` r"
"#;
    let consumer = export(consumer_yaml, "consumer", "dev");
    let producer_view_id = "bq://data-proj/analytics/revenue_view";
    assert!(producer
        .lineage
        .nodes
        .iter()
        .any(|n| n.id == producer_view_id));
    assert!(has_edge(
        &consumer.lineage,
        "bq://data-proj/marts/refined_revenue",
        producer_view_id,
        DataEdgeKind::DerivesFrom
    ));
    // Column IDs from both stacks are byte-identical.
    let producer_col = "bq://data-proj/analytics/revenue_view#revenue";
    assert!(producer
        .lineage
        .edges
        .iter()
        .any(|e| e.source_id == producer_col));
    assert!(has_edge(
        &consumer.lineage,
        "bq://data-proj/marts/refined_revenue#boosted",
        producer_col,
        DataEdgeKind::ColumnDerivesFrom
    ));
}

#[test]
fn dbt_model_lineage_with_refs_sources_and_macros() {
    let yaml = r#"name: dbt-stack
runtime: yaml
resources:
  proj:
    type: gcpx:dbt:Project
    properties:
      gcpProject: data-proj
      dataset: analytics
      sources:
        raw_src:
          dataset: raw
          tables:
            - orders
  toDollars:
    type: gcpx:dbt:Macro
    properties:
      sql: CAST(amount_cents AS FLOAT64) / 100.0
      args:
        - amount_cents
  stgOrders:
    type: gcpx:dbt:Model
    properties:
      name: stg_orders
      context: ${proj.context}
      sql: "SELECT order_id, amount_cents FROM {{ source('raw_src', 'orders') }}"
  mart:
    type: gcpx:dbt:Model
    properties:
      name: mart_revenue
      context: ${proj.context}
      modelRefs:
        stg_orders: ${stgOrders.modelOutput}
      macros:
        cents_to_dollars: ${toDollars.macroOutput}
      sql: "SELECT order_id, {{ cents_to_dollars('amount_cents') }} AS revenue FROM {{ ref('stg_orders') }}"
"#;
    let e = export(yaml, "dbt-stack", "prod");
    // Structural modelRefs edge AND parsed SQL edge merge (parsed wins).
    let edge = e
        .lineage
        .edges
        .iter()
        .find(|edge| {
            edge.source_id == "bq://data-proj/analytics/mart_revenue"
                && edge.target_id == "bq://data-proj/analytics/stg_orders"
                && edge.relationship == DataEdgeKind::DerivesFrom
        })
        .expect("mart -> stg edge");
    assert_eq!(edge.resolution, Resolution::Parsed);
    // Source table from source() substitution.
    assert!(has_edge(
        &e.lineage,
        "bq://data-proj/analytics/stg_orders",
        "bq://data-proj/raw/orders",
        DataEdgeKind::DerivesFrom
    ));
    // Column lineage through the macro expansion.
    assert!(has_edge(
        &e.lineage,
        "bq://data-proj/analytics/mart_revenue#revenue",
        "bq://data-proj/analytics/stg_orders#amount_cents",
        DataEdgeKind::ColumnDerivesFrom
    ));
}

#[test]
fn component_body_sql_is_extracted() {
    let yaml = r#"name: comp-stack
runtime: yaml
config:
  gcp:project:
    value: data-proj
components:
  ViewWrapper:
    inputs:
      size:
        type: string
    resources:
      wrapped:
        type: gcp:bigquery:Table
        properties:
          datasetId: marts
          tableId: wrapped_view
          view:
            query: "SELECT id FROM `data-proj.analytics.orders`"
    outputs:
      wrapperLineage: '{"produces":[{"project":"data-proj","dataset":"marts","table":"wrapped_view"}],"consumes":[{"project":"data-proj","dataset":"analytics","table":"orders"}]}'
resources:
  w:
    type: comp-stack:index:ViewWrapper
    properties:
      size: large
"#;
    let e = export(yaml, "comp-stack", "dev");
    let view = node(&e.lineage, "bq://data-proj/marts/wrapped_view");
    // defined_by joins the infra component_child URN.
    assert!(view
        .defined_by_urn
        .as_deref()
        .is_some_and(|u| u.contains("ViewWrapper$") && u.ends_with("::wrapped")));
    assert!(has_edge(
        &e.lineage,
        "bq://data-proj/marts/wrapped_view",
        "bq://data-proj/analytics/orders",
        DataEdgeKind::DerivesFrom
    ));
    // Declared lineage from the component output (declared beats parsed
    // on the same edge key).
    let edge = e
        .lineage
        .edges
        .iter()
        .find(|edge| {
            edge.source_id == "bq://data-proj/marts/wrapped_view"
                && edge.target_id == "bq://data-proj/analytics/orders"
        })
        .expect("edge");
    assert_eq!(edge.resolution, Resolution::Declared);
}

#[test]
fn declared_lineage_stack_output_with_columns() {
    let yaml = r#"name: declared
runtime: yaml
outputs:
  pipelineLineage: '{"produces":[{"project":"data-proj","dataset":"marts","table":"daily","columns":[{"name":"dt","type":"DATE","description":"partition day"}]}],"consumes":[{"project":"data-proj","dataset":"raw","table":"events"}],"columnLineage":[{"output":"data-proj.marts.daily.dt","from":["data-proj.raw.events.ts"]}]}'
"#;
    let e = export(yaml, "declared", "prod");
    let col = node(&e.lineage, "bq://data-proj/marts/daily#dt");
    assert_eq!(col.description.as_deref(), Some("partition day"));
    assert!(has_edge(
        &e.lineage,
        "bq://data-proj/marts/daily",
        "bq://data-proj/raw/events",
        DataEdgeKind::DerivesFrom
    ));
    assert!(has_edge(
        &e.lineage,
        "bq://data-proj/marts/daily#dt",
        "bq://data-proj/raw/events#ts",
        DataEdgeKind::ColumnDerivesFrom
    ));
    assert!(e
        .lineage
        .edges
        .iter()
        .all(|edge| edge.relationship != DataEdgeKind::DerivesFrom
            || edge.resolution == Resolution::Declared));
}

#[test]
fn routine_body_writes_and_derives() {
    let yaml = r#"name: routines
runtime: yaml
config:
  gcp:project:
    value: data-proj
resources:
  refresh:
    type: gcp:bigquery:Routine
    properties:
      datasetId: analytics
      routineId: refresh_daily
      routineType: PROCEDURE
      definitionBody: "INSERT INTO `data-proj.marts.daily` SELECT dt, COUNT(*) FROM `data-proj.raw.events` GROUP BY dt"
"#;
    let e = export(yaml, "routines", "prod");
    let routine_id = "bq-routine://data-proj/analytics/refresh_daily";
    assert_eq!(node(&e.lineage, routine_id).kind, DataNodeKind::Routine);
    assert!(has_edge(
        &e.lineage,
        routine_id,
        "bq://data-proj/marts/daily",
        DataEdgeKind::WritesTo
    ));
    // Target derives from source via the routine.
    let edge = e
        .lineage
        .edges
        .iter()
        .find(|edge| {
            edge.source_id == "bq://data-proj/marts/daily"
                && edge.target_id == "bq://data-proj/raw/events"
                && edge.relationship == DataEdgeKind::DerivesFrom
        })
        .expect("derives via routine");
    assert_eq!(edge.via.as_deref(), Some(routine_id));
}

#[test]
fn readfile_sql_resolved_within_project() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("view.sql"),
        "SELECT id FROM `data-proj.analytics.orders`",
    )
    .expect("write");
    let yaml = r#"name: files
runtime: yaml
resources:
  v:
    type: gcp:bigquery:Table
    properties:
      project: data-proj
      datasetId: marts
      tableId: from_file
      view:
        query:
          fn::readFile: view.sql
"#;
    let dir_path: &'static std::path::Path = Box::leak(Box::new(dir.path().to_path_buf()));
    let e = export_with_dir(yaml, "files", "dev", Some(dir_path));
    let edge = e
        .lineage
        .edges
        .iter()
        .find(|edge| {
            edge.source_id == "bq://data-proj/marts/from_file"
                && edge.relationship == DataEdgeKind::DerivesFrom
        })
        .expect("derives edge");
    assert_eq!(
        edge.sql_provenance,
        Some(pulumi_rs_yaml_core::sql_lineage::SqlProvenance::File)
    );
    drop(dir);
}

#[test]
fn deterministic_and_json_serializable() {
    let e1 = export(PRODUCER, "data-platform", "prod");
    let e2 = export(PRODUCER, "data-platform", "prod");
    assert_eq!(e1.lineage, e2.lineage);
    let json = e1.lineage.to_json().expect("json");
    let value: serde_json::Value = serde_json::from_str(&json).expect("valid");
    assert_eq!(value["schema_version"], 1);
    let ids: Vec<&str> = e1.lineage.nodes.iter().map(|n| n.id.as_ref()).collect();
    let mut sorted = ids.clone();
    sorted.sort();
    assert_eq!(ids, sorted, "nodes sorted by id");
}

#[test]
fn dag_failure_returns_empty_graph() {
    let yaml = "name: broken\nruntime: yaml\nresources:\n  a:\n    type: t:m:A\n    properties:\n      x: ${missing}\n";
    let (template, _) = parse_template(yaml, None);
    let template: &'static TemplateDecl<'static> = Box::leak(Box::new(template));
    let (infra, _) = export_resource_graph(
        template,
        &GraphExportOptions {
            organization: "org",
            project: "broken",
            stack: "dev",
            source_map: None,
            schema_store: None,
        },
    );
    let infra: &'static ResourceGraph<'static> = Box::leak(Box::new(infra));
    let (lineage, diags) = export_sql_lineage(
        template,
        infra,
        &SqlLineageOptions {
            organization: "org",
            project: "broken",
            stack: "dev",
            project_dir: None,
            default_bq_project: None,
            source_map: None,
            extra_sql_sources: &[],
        },
    );
    assert!(diags.has_errors());
    assert!(lineage.nodes.is_empty());
    assert!(lineage.edges.is_empty());
}

#[test]
fn multi_file_source_attribution() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(
        dir.path().join("Pulumi.yaml"),
        "name: mf\nruntime: yaml\nconfig:\n  gcp:project:\n    value: data-proj\n",
    )
    .expect("write");
    fs::write(
        dir.path().join("Pulumi.views.yaml"),
        "resources:\n  v:\n    type: gcp:bigquery:Table\n    properties:\n      datasetId: marts\n      tableId: mv\n      view:\n        query: \"SELECT 1 AS one FROM `data-proj.raw.events`\"\n",
    )
    .expect("write");
    let (merged, load_diags) = pulumi_rs_yaml_core::multi_file::load_project(dir.path(), None);
    assert!(!load_diags.has_errors(), "{}", load_diags);
    let template: &'static TemplateDecl<'static> = Box::leak(Box::new(merged.as_template_decl()));
    let source_map: &'static HashMap<String, String> =
        Box::leak(Box::new(merged.source_map().clone()));
    let (infra, _) = export_resource_graph(
        template,
        &GraphExportOptions {
            organization: "org",
            project: "mf",
            stack: "dev",
            source_map: Some(source_map),
            schema_store: None,
        },
    );
    let infra: &'static ResourceGraph<'static> = Box::leak(Box::new(infra));
    let (lineage, diags) = export_sql_lineage(
        template,
        infra,
        &SqlLineageOptions {
            organization: "org",
            project: "mf",
            stack: "dev",
            project_dir: None,
            default_bq_project: None,
            source_map: Some(source_map),
            extra_sql_sources: &[],
        },
    );
    assert!(!diags.has_errors(), "{}", diags);
    let view = lineage
        .nodes
        .iter()
        .find(|n| n.id == "bq://data-proj/marts/mv")
        .expect("view node");
    assert_eq!(view.source_file, Some("Pulumi.views.yaml"));
}

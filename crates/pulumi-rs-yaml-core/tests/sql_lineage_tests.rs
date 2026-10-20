// Author: spicyzhug@gmail.com
// Copyright: lituus-io, all rights reserved

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
        ..Default::default()
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
        ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
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

/// A dataset and a table whose ids are built from `str` functions are
/// addressable in lineage under their real names, and the view that reads
/// them joins to the same entities.
#[test]
fn dataset_and_table_ids_derived_from_str_invokes() {
    const YAML: &str = concat!(
        "name: data-platform\n",
        "runtime: yaml\n",
        "config:\n",
        "  gcp:project:\n",
        "    value: data-proj\n",
        "variables:\n",
        "  domain: sales_eu\n",
        "  dataset_id:\n",
        "    Fn::str:replace:\n",
        "      string: ${domain}\n",
        "      old: '_'\n",
        "      new: '-'\n",
        "  table_id:\n",
        "    fn::str:trimSuffix:\n",
        "      string: orders_raw\n",
        "      suffix: '_raw'\n",
        "resources:\n",
        "  ds:\n",
        "    type: gcp:bigquery:Dataset\n",
        "    properties:\n",
        "      datasetId: ${dataset_id.result}\n",
        "  base:\n",
        "    type: gcp:bigquery:Table\n",
        "    properties:\n",
        "      datasetId: ${dataset_id.result}\n",
        "      tableId: ${table_id.result}\n",
        "      schema: '[{\"name\":\"order_id\",\"type\":\"STRING\"}]'\n",
        "  revenueView:\n",
        "    type: gcp:bigquery:Table\n",
        "    properties:\n",
        "      datasetId: ${dataset_id.result}\n",
        "      tableId: revenue_view\n",
        "      view:\n",
        "        query: \"SELECT o.order_id FROM `data-proj.sales-eu.orders` o\"\n",
        "        useLegacySql: false\n",
    );

    let e = export(YAML, "data-platform", "prod");
    let ids: Vec<&str> = e.lineage.nodes.iter().map(|n| n.id.as_ref()).collect();
    assert!(
        ids.contains(&"bq://data-proj/sales-eu/orders"),
        "invoke-derived table missing; have {:?}",
        ids
    );
    assert!(
        ids.contains(&"bq://data-proj/sales-eu/revenue_view"),
        "view missing; have {:?}",
        ids
    );
    assert!(has_edge(
        &e.lineage,
        "bq://data-proj/sales-eu/revenue_view",
        "bq://data-proj/sales-eu/orders",
        DataEdgeKind::DerivesFrom,
    ));
    assert_eq!(
        node(&e.lineage, "bq://data-proj/sales-eu/orders").kind,
        DataNodeKind::Table
    );
}

// ---------------------------------------------------------------------------
// Statement kinds: what a script statement CHANGES is a write, never a read.
// ---------------------------------------------------------------------------

fn edge_of<'g>(
    g: &'g SqlLineageGraph<'static>,
    source: &str,
    target: &str,
    kind: DataEdgeKind,
) -> Option<&'g pulumi_rs_yaml_core::sql_lineage::DataEdge<'static>> {
    g.edges
        .iter()
        .find(|e| e.source_id == source && e.target_id == target && e.relationship == kind)
}

fn routine(body: &str) -> String {
    format!(
        "name: routines\nruntime: yaml\nresources:\n  r:\n    type: gcp:bigquery:Routine\n    properties:\n      project: data-proj\n      datasetId: ops\n      routineId: r\n      routineType: PROCEDURE\n      definitionBody: {}\n",
        serde_json::to_string(body).expect("quote")
    )
}

const ROUTINE: &str = "bq-routine://data-proj/ops/r";

#[test]
fn drop_truncate_alter_are_writes_not_reads() {
    let e = export(
        &routine(
            "BEGIN DROP TABLE `data-proj.stage.a`; TRUNCATE TABLE `data-proj.stage.b`; ALTER TABLE `data-proj.stage.c` ADD COLUMN x INT64; END",
        ),
        "routines",
        "prod",
    );
    for t in ["a", "b", "c"] {
        let id = format!("bq://data-proj/stage/{t}");
        assert!(
            has_edge(&e.lineage, ROUTINE, &id, DataEdgeKind::WritesTo),
            "{id} must be written"
        );
        assert!(
            !has_edge(&e.lineage, ROUTINE, &id, DataEdgeKind::DerivesFrom),
            "{id} must not be read"
        );
    }
}

#[test]
fn bigquery_load_data_target_is_a_heuristic_write() {
    let e = export(
        &routine(
            "LOAD DATA INTO `data-proj.raw.events` FROM FILES (format = 'CSV', uris = ['gs://landing/events/*.csv'])",
        ),
        "routines",
        "prod",
    );
    let w = edge_of(
        &e.lineage,
        ROUTINE,
        "bq://data-proj/raw/events",
        DataEdgeKind::WritesTo,
    )
    .expect("the load target is written");
    assert_eq!(w.resolution, Resolution::Heuristic);
    assert!(!has_edge(
        &e.lineage,
        ROUTINE,
        "bq://data-proj/raw/events",
        DataEdgeKind::DerivesFrom
    ));
}

#[test]
fn ctas_in_a_routine_writes_and_derives_via_the_routine() {
    let e = export(
        &routine("CREATE OR REPLACE TABLE `data-proj.marts.m` AS SELECT id FROM `data-proj.raw.r`"),
        "routines",
        "prod",
    );
    assert!(has_edge(
        &e.lineage,
        ROUTINE,
        "bq://data-proj/marts/m",
        DataEdgeKind::WritesTo
    ));
    let d = edge_of(
        &e.lineage,
        "bq://data-proj/marts/m",
        "bq://data-proj/raw/r",
        DataEdgeKind::DerivesFrom,
    )
    .expect("target derives from source");
    assert_eq!(d.via.as_deref(), Some(ROUTINE));
}

#[test]
fn merge_target_is_written_and_source_read() {
    let e = export(
        &routine(
            "MERGE `data-proj.marts.t` T USING `data-proj.raw.s` S ON T.id = S.id WHEN MATCHED THEN UPDATE SET v = S.v",
        ),
        "routines",
        "prod",
    );
    assert!(has_edge(
        &e.lineage,
        ROUTINE,
        "bq://data-proj/marts/t",
        DataEdgeKind::WritesTo
    ));
    assert!(has_edge(
        &e.lineage,
        ROUTINE,
        "bq://data-proj/raw/s",
        DataEdgeKind::DerivesFrom
    ));
}

#[test]
fn call_links_the_routine_called() {
    let e = export(
        &routine("CALL `data-proj.ops.refresh`()"),
        "routines",
        "prod",
    );
    assert!(has_edge(
        &e.lineage,
        ROUTINE,
        "bq-routine://data-proj/ops/refresh",
        DataEdgeKind::Calls
    ));
}

// ---------------------------------------------------------------------------
// Jobs and scheduled queries
// ---------------------------------------------------------------------------

#[test]
fn job_query_destination_derives_from_its_reads() {
    let yaml = r#"name: jobs
runtime: yaml
config:
  gcp:project:
    value: data-proj
resources:
  backfill:
    type: gcp:bigquery:Job
    properties:
      jobId: backfill-1
      query:
        query: "SELECT id FROM `data-proj.raw.events`"
        destinationTable:
          projectId: data-proj
          datasetId: marts
          tableId: backfill
"#;
    let e = export(yaml, "jobs", "prod");
    let job = "bq-job://org/jobs/prod/backfill";
    assert!(has_edge(
        &e.lineage,
        job,
        "bq://data-proj/marts/backfill",
        DataEdgeKind::WritesTo
    ));
    assert!(has_edge(
        &e.lineage,
        "bq://data-proj/marts/backfill",
        "bq://data-proj/raw/events",
        DataEdgeKind::DerivesFrom
    ));
}

#[test]
fn scheduled_query_writes_in_its_own_project() {
    let yaml = r#"name: sched
runtime: yaml
config:
  gcp:project:
    value: stack-default
resources:
  nightly:
    type: gcp:bigquery:DataTransferConfig
    properties:
      project: analytics-proj
      displayName: nightly
      dataSourceId: scheduled_query
      destinationDatasetId: marts
      params:
        destination_table_name_template: nightly_rollup
        query: "SELECT id FROM raw.events"
"#;
    let e = export(yaml, "sched", "prod");
    let job = "bq-job://org/sched/prod/nightly";
    assert!(
        has_edge(
            &e.lineage,
            job,
            "bq://analytics-proj/marts/nightly_rollup",
            DataEdgeKind::WritesTo
        ),
        "destination must be in the config's project; edges: {:?}",
        e.lineage
            .edges
            .iter()
            .map(|x| (&x.source_id, &x.target_id))
            .collect::<Vec<_>>()
    );
    // The unqualified `raw.events` resolves in the same project.
    assert!(has_edge(
        &e.lineage,
        job,
        "bq://analytics-proj/raw/events",
        DataEdgeKind::DerivesFrom
    ));
    assert!(!e
        .lineage
        .edges
        .iter()
        .any(|x| x.target_id.contains("stack-default")));
}

#[test]
fn scheduled_query_heuristic_read_stays_heuristic_on_the_destination() {
    // Unparseable SQL: the read is found by the heuristic scanner, and must
    // not reappear on the destination claiming it was parsed.
    let yaml = r#"name: sched
runtime: yaml
config:
  gcp:project:
    value: p
resources:
  nightly:
    type: gcp:bigquery:DataTransferConfig
    properties:
      destinationDatasetId: marts
      params:
        destination_table_name_template: out
        query: "SELECT FROM WHERE `p.raw.events` ((("
"#;
    let e = export(yaml, "sched", "prod");
    let d = edge_of(
        &e.lineage,
        "bq://p/marts/out",
        "bq://p/raw/events",
        DataEdgeKind::DerivesFrom,
    )
    .expect("destination derives from the heuristic read");
    assert_eq!(d.resolution, Resolution::Heuristic);
}

#[test]
fn gcpx_scheduler_sql_job_script_lineage() {
    let yaml = r#"name: sj
runtime: yaml
config:
  gcp:project:
    value: p
resources:
  loader:
    type: gcpx:scheduler:SqlJob
    properties:
      sql: "INSERT INTO `p.marts.t` SELECT * FROM `p.raw.s`; DELETE FROM `p.raw.s` WHERE TRUE"
"#;
    let e = export(yaml, "sj", "prod");
    let job = "bq-job://org/sj/prod/loader";
    assert!(has_edge(
        &e.lineage,
        job,
        "bq://p/marts/t",
        DataEdgeKind::WritesTo
    ));
    assert!(has_edge(
        &e.lineage,
        job,
        "bq://p/raw/s",
        DataEdgeKind::WritesTo
    ));
}

#[test]
fn an_unquoted_qualified_read_in_a_script_keeps_its_project_and_dataset() {
    // `get_table_names` answers the bare `events` for an unquoted
    // `other-proj.raw.events`, and the read used to resolve against the
    // routine's own project and dataset: `data-proj/ops/events`, a table that
    // does not exist, with nothing to say so.
    let e = export(
        &routine("INSERT INTO marts.daily SELECT id FROM `other-proj`.raw.events"),
        "routines",
        "prod",
    );
    assert!(has_edge(
        &e.lineage,
        ROUTINE,
        "bq://data-proj/marts/daily",
        DataEdgeKind::WritesTo
    ));
    assert!(
        has_edge(
            &e.lineage,
            ROUTINE,
            "bq://other-proj/raw/events",
            DataEdgeKind::DerivesFrom
        ),
        "edges: {:?}",
        e.lineage
            .edges
            .iter()
            .map(|x| (&x.source_id, &x.target_id))
            .collect::<Vec<_>>()
    );
    assert!(!e
        .lineage
        .nodes
        .iter()
        .any(|n| n.id == "bq://data-proj/ops/events"));
}

// ---------------------------------------------------------------------------
// The statement layer and the source store
// ---------------------------------------------------------------------------

use pulumi_rs_yaml_core::sql_lineage::{SqlSourceStore, SqlStatement, StatementKind, TextOrigin};

/// Exports with the statement layer on, optionally with a source store.
fn export_statements(
    yaml: &str,
    dir: Option<&'static std::path::Path>,
    with_store: bool,
) -> (
    &'static TemplateDecl<'static>,
    SqlLineageGraph<'static>,
    Option<&'static SqlSourceStore>,
) {
    let (template, parse_diags) = parse_template(yaml, None);
    assert!(!parse_diags.has_errors(), "{parse_diags}");
    let template: &'static TemplateDecl<'static> = Box::leak(Box::new(template));
    let graph_opts = GraphExportOptions {
        organization: "org",
        project: "stmts",
        stack: "prod",
        source_map: None,
        schema_store: None,
        ..Default::default()
    };
    let (infra, _) = export_resource_graph(template, &graph_opts);
    let infra: &'static ResourceGraph<'static> = Box::leak(Box::new(infra));
    let store: Option<&'static SqlSourceStore> = match (with_store, dir) {
        (true, Some(d)) => Some(Box::leak(Box::new(SqlSourceStore::collect(template, d)))),
        _ => None,
    };
    let opts = SqlLineageOptions {
        organization: "org",
        project: "stmts",
        stack: "prod",
        project_dir: dir,
        default_bq_project: Some("p"),
        sql_sources: store,
        include_statements: true,
        ..Default::default()
    };
    let (lineage, _) = export_sql_lineage(template, infra, &opts);
    (template, lineage, store)
}

fn statement<'g>(g: &'g SqlLineageGraph<'static>, text: &str) -> &'g SqlStatement<'static> {
    g.statements
        .iter()
        .find(|s| s.text.as_deref() == Some(text))
        .unwrap_or_else(|| {
            panic!(
                "no statement {text:?}; have {:?}",
                g.statements.iter().map(|s| &s.text).collect::<Vec<_>>()
            )
        })
}

fn points_into(inner: &str, outer: &str) -> bool {
    let (i, o) = (inner.as_ptr() as usize, outer.as_ptr() as usize);
    i >= o && i + inner.len() <= o + outer.len()
}

const VIEW: &str = "name: stmts\nruntime: yaml\nresources:\n  v:\n    type: gcp:bigquery:Table\n    properties:\n      project: p\n      datasetId: marts\n      tableId: v\n      view:\n        query: \"SELECT a.id FROM `p.raw.a` a\"\n";

#[test]
fn statements_are_absent_unless_asked_for() {
    let e = export(VIEW, "stmts", "prod");
    assert!(e.lineage.statements.is_empty());
    assert!(e.lineage.statement_occurrences.is_empty());
    let json = e.lineage.to_json().expect("json");
    assert!(!json.contains("\"statements\""), "{json}");
    assert!(!json.contains("has_statement"), "{json}");
}

#[test]
fn a_view_is_one_statement_that_reads_its_sources_and_writes_the_view() {
    let (template, g, _) = export_statements(VIEW, None, false);
    let st = statement(&g, "SELECT a.id FROM `p.raw.a` a");
    assert_eq!(st.statement_kind, StatementKind::Select);
    assert!(
        st.id.starts_with("sql:") && st.id.len() == 4 + 64,
        "{}",
        st.id
    );
    assert!(has_edge(
        &g,
        "bq://p/marts/v",
        &st.id,
        DataEdgeKind::HasStatement
    ));
    assert!(has_edge(&g, &st.id, "bq://p/raw/a", DataEdgeKind::Reads));
    assert!(has_edge(&g, &st.id, "bq://p/marts/v", DataEdgeKind::Writes));

    // Zero copy: the statement's text is a slice of the parsed template's
    // own string, not a copy of it.
    let text = st.text.as_deref().expect("text");
    let pulumi_rs_yaml_core::ast::template::ResourceProperties::Map(props) =
        &template.resources[0].resource.properties
    else {
        panic!("map");
    };
    let view = props.iter().find(|p| p.key == "view").expect("view");
    let pulumi_rs_yaml_core::ast::expr::Expr::Object(_, obj) = &view.value else {
        panic!("object");
    };
    let pulumi_rs_yaml_core::ast::expr::Expr::String(_, query) = obj[0].value.as_ref() else {
        panic!("string");
    };
    assert!(
        points_into(text, query),
        "statement text must borrow the template literal"
    );
    let occ = &g.statement_occurrences[0];
    assert_eq!(occ.text_origin, TextOrigin::Literal);
    assert_eq!(occ.statement_index, 0);
    assert_eq!(occ.carrier_id, "bq://p/marts/v");
}

#[test]
fn script_statements_have_offsets_kinds_and_their_own_edges() {
    let body = "BEGIN\n  INSERT INTO `p.marts.t` SELECT id FROM `p.raw.s`;\n  DROP TABLE `p.stage.tmp`;\nEND";
    let (_, g, _) = export_statements(&routine(body), None, false);
    let insert = statement(&g, "INSERT INTO `p.marts.t` SELECT id FROM `p.raw.s`");
    let drop = statement(&g, "DROP TABLE `p.stage.tmp`");
    assert_eq!(insert.statement_kind, StatementKind::Insert);
    assert_eq!(drop.statement_kind, StatementKind::Drop);
    assert!(has_edge(
        &g,
        &insert.id,
        "bq://p/marts/t",
        DataEdgeKind::Writes
    ));
    assert!(has_edge(
        &g,
        &insert.id,
        "bq://p/raw/s",
        DataEdgeKind::Reads
    ));
    assert!(has_edge(
        &g,
        &drop.id,
        "bq://p/stage/tmp",
        DataEdgeKind::Writes
    ));
    assert!(!has_edge(
        &g,
        &drop.id,
        "bq://p/stage/tmp",
        DataEdgeKind::Reads
    ));

    // Offsets index the carrier's own text.
    for occ in &g.statement_occurrences {
        let st = g
            .statements
            .iter()
            .find(|s| s.id == occ.statement_id)
            .expect("row");
        let text = st.text.as_deref().expect("text");
        assert_eq!(&body[occ.byte_offset..occ.byte_offset + occ.byte_len], text);
    }
    let indices: Vec<u32> = g
        .statement_occurrences
        .iter()
        .map(|o| o.statement_index)
        .collect();
    assert_eq!(indices, vec![0, 1]);
}

#[test]
fn identical_text_is_one_statement_with_two_occurrences() {
    let yaml = "name: stmts\nruntime: yaml\nresources:\n  a:\n    type: gcp:bigquery:Routine\n    properties:\n      project: p\n      datasetId: ops\n      routineId: a\n      routineType: PROCEDURE\n      definitionBody: \"DELETE FROM `p.raw.s` WHERE TRUE\"\n  b:\n    type: gcp:bigquery:Routine\n    properties:\n      project: p\n      datasetId: ops\n      routineId: b\n      routineType: PROCEDURE\n      definitionBody: \"DELETE FROM `p.raw.s` WHERE TRUE\"\n";
    let (_, g, _) = export_statements(yaml, None, false);
    assert_eq!(g.statements.len(), 1);
    assert_eq!(g.statement_occurrences.len(), 2);
    let carriers: Vec<&str> = g
        .statement_occurrences
        .iter()
        .map(|o| o.carrier_id.as_ref())
        .collect();
    assert_eq!(
        carriers,
        vec!["bq-routine://p/ops/a", "bq-routine://p/ops/b"]
    );
}

#[test]
fn interpolated_sql_is_owned_and_says_so() {
    let yaml = "name: stmts\nruntime: yaml\nvariables:\n  ds: raw\nresources:\n  r:\n    type: gcp:bigquery:Routine\n    properties:\n      project: p\n      datasetId: ops\n      routineId: r\n      routineType: PROCEDURE\n      definitionBody: \"DELETE FROM `p.${ds}.s` WHERE TRUE\"\n";
    let (_, g, _) = export_statements(yaml, None, false);
    assert_eq!(
        g.statement_occurrences[0].text_origin,
        TextOrigin::Interpolated
    );
    let st = statement(&g, "DELETE FROM `p.raw.s` WHERE TRUE");
    assert!(matches!(st.text, Some(std::borrow::Cow::Owned(_))));
}

#[test]
fn export_data_writes_an_external_location() {
    let (_, g, _) = export_statements(
        &routine("EXPORT DATA OPTIONS (uri = 'gs://bucket/out/*.csv', format = 'CSV') AS SELECT id FROM `p.raw.s`"),
        None,
        false,
    );
    let st = &g.statements[0];
    assert_eq!(st.statement_kind, StatementKind::ExportData);
    assert!(has_edge(
        &g,
        &st.id,
        "gs://bucket/out/*.csv",
        DataEdgeKind::Writes
    ));
    assert!(has_edge(&g, &st.id, "bq://p/raw/s", DataEdgeKind::Reads));
    assert_eq!(
        node(&g, "gs://bucket/out/*.csv").kind,
        DataNodeKind::ExternalLocation
    );
}

const READFILE_VIEW: &str = "name: stmts\nruntime: yaml\nresources:\n  v:\n    type: gcp:bigquery:Table\n    properties:\n      project: p\n      datasetId: marts\n      tableId: v\n      view:\n        query:\n          fn::readFile: q.sql\n  w:\n    type: gcp:bigquery:Table\n    properties:\n      project: p\n      datasetId: marts\n      tableId: w\n      view:\n        query:\n          fn::readFile: ./q.sql\n";

fn readfile_dir() -> &'static std::path::Path {
    let dir: &'static _ = Box::leak(Box::new(tempfile::tempdir().expect("tempdir")));
    fs::write(dir.path().join("q.sql"), "SELECT id FROM `p.raw.events`\n").expect("write");
    dir.path()
}

#[test]
fn file_statement_text_is_borrowed_from_the_store() {
    let dir = readfile_dir();
    let (_, g, store) = export_statements(READFILE_VIEW, Some(dir), true);
    let store = store.expect("store");
    // Two literals, one file: read once.
    assert_eq!(store.len(), 1);
    let held = store.get("q.sql").expect("held");
    assert!(std::ptr::eq(held, store.get("./q.sql").expect("same text")));
    let st = statement(&g, "SELECT id FROM `p.raw.events`");
    assert!(points_into(st.text.as_deref().expect("text"), held));
    assert_eq!(g.statement_occurrences.len(), 2);
    for occ in &g.statement_occurrences {
        assert_eq!(occ.text_origin, TextOrigin::File);
        assert!(occ.carrier_file.is_some());
    }
}

#[test]
fn the_store_changes_nothing_but_where_the_text_lives() {
    let dir = readfile_dir();
    let (_, with_store, _) = export_statements(READFILE_VIEW, Some(dir), true);
    let (_, without, _) = export_statements(READFILE_VIEW, Some(dir), false);
    assert_eq!(
        with_store.to_json().expect("json"),
        without.to_json().expect("json")
    );
}

// ---------------------------------------------------------------------------
// Column lineage through scripts
// ---------------------------------------------------------------------------

fn export_script_columns(yaml: &str, on: bool) -> SqlLineageGraph<'static> {
    let (template, diags) = parse_template(yaml, None);
    assert!(!diags.has_errors(), "{diags}");
    let template: &'static TemplateDecl<'static> = Box::leak(Box::new(template));
    let graph_opts = GraphExportOptions {
        organization: "org",
        project: "routines",
        stack: "prod",
        ..Default::default()
    };
    let (infra, _) = export_resource_graph(template, &graph_opts);
    let infra: &'static ResourceGraph<'static> = Box::leak(Box::new(infra));
    let opts = SqlLineageOptions {
        organization: "org",
        project: "routines",
        stack: "prod",
        default_bq_project: Some("p"),
        script_column_lineage: on,
        ..Default::default()
    };
    export_sql_lineage(template, infra, &opts).0
}

#[test]
fn script_column_lineage_follows_ctas_and_insert_via_the_routine() {
    let yaml = routine(
        "BEGIN\n  CREATE OR REPLACE TABLE `p.marts.daily` AS SELECT o.order_id, o.amount_cents / 100 AS revenue FROM `p.raw.orders` o;\n  INSERT INTO `p.marts.audit` (oid) SELECT order_id FROM `p.marts.daily`;\nEND",
    );
    let g = export_script_columns(&yaml, true);
    let e = edge_of(
        &g,
        "bq://p/marts/daily#revenue",
        "bq://p/raw/orders#amount_cents",
        DataEdgeKind::ColumnDerivesFrom,
    )
    .expect("revenue derives from amount_cents");
    assert_eq!(e.via.as_deref(), Some(ROUTINE));
    assert_eq!(e.resolution, Resolution::Parsed);
    assert!(has_edge(
        &g,
        "bq://p/marts/audit#oid",
        "bq://p/marts/daily#order_id",
        DataEdgeKind::ColumnDerivesFrom
    ));
    // Both ends exist as column nodes.
    assert_eq!(
        node(&g, "bq://p/marts/daily#revenue").kind,
        DataNodeKind::Column
    );
    assert_eq!(
        node(&g, "bq://p/raw/orders#amount_cents").kind,
        DataNodeKind::Column
    );
}

#[test]
fn script_column_lineage_is_off_by_default() {
    let yaml = routine("CREATE TABLE `p.marts.t` AS SELECT id FROM `p.raw.s`");
    let off = export_script_columns(&yaml, false);
    assert!(!off
        .edges
        .iter()
        .any(|e| e.relationship == DataEdgeKind::ColumnDerivesFrom));
    // Every table in the routine is fully qualified, so the default project
    // `export` leaves unset changes nothing: the outputs must agree exactly.
    let plain = export(&yaml, "routines", "prod");
    assert_eq!(
        off.to_json().expect("json"),
        plain.lineage.to_json().expect("json")
    );
}

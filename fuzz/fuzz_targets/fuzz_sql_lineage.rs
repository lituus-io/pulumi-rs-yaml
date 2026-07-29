//! Fuzz target: SQL lineage export (full pipeline + hot helpers)
//!
//! Exercises the template → infra graph → lineage pipeline plus the
//! pure helpers most exposed to hostile text: table-reference parsing,
//! statement splitting, and heuristic name scanning.
//!
//! Security targets:
//! - Panics inside the polyglot parser boundary on hostile SQL
//!   (process aborts under panic=abort — any panic is a finding)
//! - Stack overflow from deeply nested SQL or jinja macro expansion
//! - Panics from hostile table names / schema JSON / declared-lineage
//!   payloads
//! - Non-determinism in the exported lineage graph

#![no_main]
use libfuzzer_sys::fuzz_target;

use pulumi_rs_yaml_core::resource_graph::{export_resource_graph, GraphExportOptions};
use pulumi_rs_yaml_core::sql_lineage::{export_sql_lineage, ids, SqlLineageOptions};

fuzz_target!(|data: &[u8]| {
    let Ok(input) = std::str::from_utf8(data) else {
        return;
    };
    if input.len() > 32 * 1024 {
        return;
    }

    // Pure helpers directly on the raw input — must never panic.
    let _ = ids::parse_table_reference(input, Some("proj"), Some("ds"));
    let _ = ids::table_name(input, input, input);

    // Full pipeline: treat the input as a template. No Box::leak — the
    // exporters borrow for any lifetime, so LeakSanitizer stays useful.
    let (template, _diags) = pulumi_rs_yaml_core::ast::parse::parse_template(input, None);
    let graph_opts = GraphExportOptions {
        organization: "org",
        project: "fuzz",
        stack: "dev",
        source_map: None,
        schema_store: None,
    };
    let (infra, _) = export_resource_graph(&template, &graph_opts);
    let opts = SqlLineageOptions {
        organization: "org",
        project: "fuzz",
        stack: "dev",
        project_dir: None,
        default_bq_project: Some("proj"),
        source_map: None,
        extra_sql_sources: &[],
    };
    let (lineage1, _) = export_sql_lineage(&template, &infra, &opts);
    let (lineage2, _) = export_sql_lineage(&template, &infra, &opts);
    assert_eq!(lineage1, lineage2, "lineage export must be deterministic");
    let json = lineage1.to_json().expect("lineage serializes");
    let _: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");

    // ...and also as SQL embedded in a view, driving the parser boundary
    // (analyze/parse/split/heuristic paths) with arbitrary text.
    let view_yaml = format!(
        "name: fuzz\nruntime: yaml\nresources:\n  v:\n    type: gcp:bigquery:Table\n    properties:\n      project: proj\n      datasetId: ds\n      tableId: v\n      view:\n        query: {}\n",
        serde_yaml::to_string(&input).unwrap_or_else(|_| "''".to_string())
    );
    let (view_template, _) = pulumi_rs_yaml_core::ast::parse::parse_template(&view_yaml, None);
    let (view_infra, _) = export_resource_graph(&view_template, &graph_opts);
    let (view_lineage, _) = export_sql_lineage(&view_template, &view_infra, &opts);
    let _ = view_lineage.to_json().expect("view lineage serializes");
});

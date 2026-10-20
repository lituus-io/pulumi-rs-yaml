// Author: spicyzhug@gmail.com
// Copyright: lituus-io, all rights reserved

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
        ..Default::default()
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
        ..Default::default()
    };
    let opts = &opts;
    let (lineage1, _) = export_sql_lineage(&template, &infra, opts);
    let (lineage2, _) = export_sql_lineage(&template, &infra, opts);
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
    let (view_lineage, _) = export_sql_lineage(&view_template, &view_infra, opts);
    let _ = view_lineage.to_json().expect("view lineage serializes");

    // ...and as a routine body, which is SPLIT into statements before it is
    // parsed: the splitter and CALL detection only see author text this way.
    // A view's query never reaches either.
    let routine_yaml = format!(
        "name: fuzz\nruntime: yaml\nresources:\n  r:\n    type: gcp:bigquery:Routine\n    properties:\n      project: proj\n      datasetId: ds\n      routineId: r\n      routineType: PROCEDURE\n      definitionBody: {}\n",
        serde_yaml::to_string(&input).unwrap_or_else(|_| "''".to_string())
    );
    let (routine_template, _) =
        pulumi_rs_yaml_core::ast::parse::parse_template(&routine_yaml, None);
    let (routine_infra, _) = export_resource_graph(&routine_template, &graph_opts);
    let (r1, _) = export_sql_lineage(&routine_template, &routine_infra, opts);
    let (r2, _) = export_sql_lineage(&routine_template, &routine_infra, opts);
    assert_eq!(r1, r2, "routine lineage must be deterministic");
    let _ = r1.to_json().expect("routine lineage serializes");

    // ...and with the statement layer on: deterministic, every id the hash
    // of its own text, every occurrence slicing exactly that text out of the
    // input, and every statement edge starting at a statement that exists.
    // Column lineage through scripts rides along: it regenerates SQL from the
    // parsed statement and analyses it again, on fuzzer-shaped statements.
    let with_statements = SqlLineageOptions {
        include_statements: true,
        script_column_lineage: true,
        organization: opts.organization,
        project: opts.project,
        stack: opts.stack,
        default_bq_project: opts.default_bq_project,
        ..Default::default()
    };
    let (s1, _) = export_sql_lineage(&routine_template, &routine_infra, &with_statements);
    let (s2, _) = export_sql_lineage(&routine_template, &routine_infra, &with_statements);
    assert_eq!(s1, s2, "statement layer must be deterministic");
    for st in &s1.statements {
        if let Some(text) = &st.text {
            let digest = pulumi_rs_yaml_core::sha256::digest(text.as_bytes());
            assert_eq!(
                st.id,
                format!("sql:{}", pulumi_rs_yaml_core::sha256::hex(&digest))
            );
            assert_eq!(st.byte_len, text.len());
        }
    }
    for occ in &s1.statement_occurrences {
        let st = s1
            .statements
            .iter()
            .find(|s| s.id == occ.statement_id)
            .expect("an occurrence names a statement row");
        if let Some(text) = &st.text {
            assert_eq!(occ.byte_len, text.len());
        }
    }
    for edge in &s1.edges {
        if edge.source_id.starts_with("sql:") {
            assert!(
                s1.statements.iter().any(|s| s.id == edge.source_id),
                "edge from a statement with no row"
            );
        }
    }
    let _ = s1.to_json().expect("statement layer serializes");
});

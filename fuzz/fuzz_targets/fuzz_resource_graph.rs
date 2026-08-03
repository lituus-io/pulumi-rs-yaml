// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

//! Fuzz target: infrastructure dependency-graph export
//!
//! Any template that parses must export without panicking, serialize to
//! valid JSON, and be deterministic (two exports byte-equal).
//!
//! Security targets:
//! - Panics from hostile logical names / type tokens / property paths
//! - Stack overflow from deep parent chains or component nesting
//! - Non-determinism (ordering leaks) in the exported graph

#![no_main]
use libfuzzer_sys::fuzz_target;

use pulumi_rs_yaml_core::resource_graph::{export_resource_graph, GraphExportOptions};

fuzz_target!(|data: &[u8]| {
    let Ok(input) = std::str::from_utf8(data) else {
        return;
    };
    if input.len() > 64 * 1024 {
        return;
    }

    // No Box::leak: the exporter borrows for any lifetime, so a local
    // binding suffices and LeakSanitizer stays meaningful.
    let (template, _diags) = pulumi_rs_yaml_core::ast::parse::parse_template(input, None);

    let opts = GraphExportOptions {
        organization: "org",
        project: "fuzz",
        stack: "dev",
        source_map: None,
        schema_store: None,
    };
    let (graph1, _) = export_resource_graph(&template, &opts);
    let (graph2, _) = export_resource_graph(&template, &opts);

    // Determinism: identical inputs produce identical graphs.
    assert_eq!(graph1, graph2, "export must be deterministic");

    // Serialization must never fail and must be valid JSON.
    let json = graph1.to_json().expect("graph serializes");
    let _: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");

    // Node ordering contract: sorted by id.
    let ids: Vec<&str> = graph1.nodes.iter().map(|n| n.id.as_ref()).collect();
    let mut sorted = ids.clone();
    sorted.sort();
    assert_eq!(ids, sorted, "nodes sorted by id");
});

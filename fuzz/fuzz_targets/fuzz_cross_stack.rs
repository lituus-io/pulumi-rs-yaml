// Author: spicyzhug@gmail.com
// Copyright: lituus-io, all rights reserved

//! Fuzz target: cross-stack resolution.
//!
//! A producer and a consumer are generated with fuzzer-chosen output keys,
//! stack names and organizations, exported, and joined; the raw input is also
//! exported and joined with them. Every link must name a real consumer edge
//! and a real exported resource, and the join must be deterministic.

#![no_main]
use libfuzzer_sys::fuzz_target;

use pulumi_rs_yaml_core::ast::parse::parse_template;
use pulumi_rs_yaml_core::cross_stack::resolve_cross_stack;
use pulumi_rs_yaml_core::resource_graph::{
    export_resource_graph, EdgeKind, GraphExportOptions, ResourceGraph,
};

fn q(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

fuzz_target!(|data: &[u8]| {
    let Ok(input) = std::str::from_utf8(data) else {
        return;
    };
    if input.len() > 8 * 1024 {
        return;
    }
    let mut parts = input.splitn(4, '\n');
    let key = parts.next().unwrap_or("k");
    let stack_name = parts.next().unwrap_or("prod");
    let org = parts.next().unwrap_or("");
    let raw = parts.next().unwrap_or("");

    let producer = format!(
        "name: producer\nruntime: yaml\nresources:\n  ds:\n    type: gcp:bigquery:Dataset\noutputs:\n  {}: ${{ds.datasetId}}\n",
        q(key)
    );
    let consumer = format!(
        "name: consumer\nruntime: yaml\nresources:\n  ref:\n    type: pulumi:pulumi:StackReference\n    properties:\n      name: {}\n  t:\n    type: gcp:bigquery:Table\n    properties:\n      a: ${{ref.outputs[{}]}}\n",
        q(&format!("producer/{stack_name}")),
        q(key)
    );
    let sources = [producer, consumer, raw.to_string()];
    let templates: Vec<_> = sources.iter().map(|s| parse_template(s, None).0).collect();
    for backend in [None, Some("gs://s")] {
        let graphs: Vec<ResourceGraph<'_>> = templates
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let opts = GraphExportOptions {
                    organization: if i == 0 { org } else { "" },
                    project: ["producer", "consumer", "raw"][i],
                    stack: stack_name,
                    backend_url: backend,
                    ..Default::default()
                };
                export_resource_graph(t, &opts).0
            })
            .collect();
        let links = resolve_cross_stack(&graphs);
        assert_eq!(links, resolve_cross_stack(&graphs), "deterministic");
        let edges: Vec<_> = graphs.iter().flat_map(|g| g.edges.iter()).collect();
        for l in &links {
            assert!(edges
                .iter()
                .any(|e| e.relationship == EdgeKind::ConsumesStackOutput
                    && e.source_id == l.consumer_edge.source_id
                    && e.target_id == l.consumer_edge.target_id));
            assert!(edges.iter().any(|e| e.relationship == EdgeKind::Exports
                && e.source_id == l.consumer_edge.target_id
                && e.target_id == l.exporter_resource_id));
        }
        let _ = serde_json::to_string(&links).expect("serializes");
    }
});

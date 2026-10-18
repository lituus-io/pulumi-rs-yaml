// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

//! Cross-stack resolution: from a consumer's read of a stack output to the
//! resource behind it.
//!
//! A consuming stack's export ends its edge at a `stackoutput::` id
//! (`consumes_stack_output`); the producing stack's export starts edges at
//! that same id (`exports`) and ends them at the resources the output reads.
//! [`resolve_cross_stack`] joins the two over any set of exported graphs, so
//! a consumer's edge reaches the actual resource. Pure, borrowed and linear:
//! one pass indexes the `exports` edges, one pass joins the consumers.
//!
//! The ids line up only when both stacks were exported with the same
//! organization -- which, on a self-managed backend, the exporter now
//! guarantees by construction (see [`crate::resource_graph::ORG_PLACEHOLDER`]).

use std::collections::HashMap;

use serde::Serialize;

use crate::resource_graph::{EdgeKind, GraphEdge, ResourceGraph};

/// The consumer's edge a link resolves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct ConsumerEdge<'g> {
    pub source_id: &'g str,
    /// The `stackoutput::` id the consumer reads.
    pub target_id: &'g str,
}

/// One consumer edge joined to one resource the output exports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct CrossStackLink<'g> {
    pub consumer_edge: ConsumerEdge<'g>,
    pub exporter_resource_id: &'g str,
    pub output_key: &'g str,
}

/// Joins every `consumes_stack_output` edge in `graphs` to the resources
/// the exporting stack's `exports` edges name. Sorted and deduplicated; an
/// output no graph exports, or one that reads no resource, yields no link.
pub fn resolve_cross_stack<'g>(graphs: &'g [ResourceGraph<'_>]) -> Vec<CrossStackLink<'g>> {
    resolve_cross_stack_edges(graphs.iter().flat_map(|g| g.edges.iter()))
}

/// [`resolve_cross_stack`] over edges from any source (exports read back
/// from JSON, say). Walks `edges` twice.
pub fn resolve_cross_stack_edges<'g, 'a: 'g, I>(edges: I) -> Vec<CrossStackLink<'g>>
where
    I: Iterator<Item = &'g GraphEdge<'a>> + Clone,
{
    let mut exports: HashMap<&'g str, Vec<(&'g str, &'g str)>> = HashMap::new();
    for e in edges.clone() {
        if e.relationship == EdgeKind::Exports {
            let key = e
                .property_paths
                .first()
                .map(|p| p.as_ref())
                .unwrap_or_else(|| output_key_of(&e.source_id));
            exports
                .entry(e.source_id.as_ref())
                .or_default()
                .push((e.target_id.as_ref(), key));
        }
    }
    let mut links = Vec::new();
    if exports.is_empty() {
        return links;
    }
    for e in edges {
        if e.relationship != EdgeKind::ConsumesStackOutput {
            continue;
        }
        let Some(targets) = exports.get(e.target_id.as_ref()) else {
            continue;
        };
        for &(resource, key) in targets {
            links.push(CrossStackLink {
                consumer_edge: ConsumerEdge {
                    source_id: e.source_id.as_ref(),
                    target_id: e.target_id.as_ref(),
                },
                exporter_resource_id: resource,
                output_key: key,
            });
        }
    }
    links.sort_unstable();
    links.dedup();
    links
}

/// The output key of a `stackoutput::{fq}::{key}` id: what follows the
/// second `::`.
fn output_key_of(id: &str) -> &str {
    id.strip_prefix("stackoutput::")
        .and_then(|rest| rest.split_once("::"))
        .map_or("", |(_, key)| key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::parse::parse_template;
    use crate::ast::template::TemplateDecl;
    use crate::resource_graph::{export_resource_graph, GraphExportOptions};

    fn export(
        yaml: &str,
        project: &'static str,
        backend: Option<&'static str>,
        org: &'static str,
    ) -> ResourceGraph<'static> {
        let (template, diags) = parse_template(yaml, None);
        assert!(!diags.has_errors(), "{diags}");
        let template: &'static TemplateDecl<'static> = Box::leak(Box::new(template));
        let opts = GraphExportOptions {
            organization: org,
            project,
            stack: "prod",
            backend_url: backend,
            ..Default::default()
        };
        export_resource_graph(template, &opts).0
    }

    const PRODUCER: &str = "name: producer\nruntime: yaml\nresources:\n  ds:\n    type: gcp:bigquery:Dataset\n    properties:\n      datasetId: d\noutputs:\n  datasetId: ${ds.datasetId}\n  constant: fixed\n";
    const CONSUMER: &str = "name: consumer\nruntime: yaml\nresources:\n  ref:\n    type: pulumi:pulumi:StackReference\n    properties:\n      name: producer/prod\n  t:\n    type: gcp:bigquery:Table\n    properties:\n      datasetId: ${ref.outputs.datasetId}\n      other: ${ref.outputs[\"constant\"]}\n      missing: ${ref.outputs.nowhere}\n";

    #[test]
    fn joins_a_consumer_to_the_exporting_resource() {
        let graphs = [
            export(PRODUCER, "producer", None, "acme"),
            export(CONSUMER, "consumer", None, "acme"),
        ];
        let links = resolve_cross_stack(&graphs);
        assert_eq!(links.len(), 1, "{links:?}");
        let l = links[0];
        assert_eq!(
            l.consumer_edge.source_id,
            "urn:pulumi:prod::consumer::gcp:bigquery/table:Table::t"
        );
        assert_eq!(
            l.consumer_edge.target_id,
            "stackoutput::acme/producer/prod::datasetId"
        );
        assert_eq!(
            l.exporter_resource_id,
            "urn:pulumi:prod::producer::gcp:bigquery/dataset:Dataset::ds"
        );
        assert_eq!(l.output_key, "datasetId");
    }

    #[test]
    fn self_managed_backends_line_up_whatever_org_each_side_passed() {
        // Each side passes a different organization; on a gs:// backend the
        // ids use the one placeholder, so they still join.
        let graphs = [
            export(PRODUCER, "producer", Some("gs://state"), "team-a"),
            export(CONSUMER, "consumer", Some("gs://state"), ""),
        ];
        assert_eq!(resolve_cross_stack(&graphs).len(), 1);
        // On Pulumi Cloud the organization is real, and differing ones do not.
        let graphs = [
            export(
                PRODUCER,
                "producer",
                Some("https://api.pulumi.com"),
                "team-a",
            ),
            export(
                CONSUMER,
                "consumer",
                Some("https://api.pulumi.com"),
                "team-b",
            ),
        ];
        assert!(resolve_cross_stack(&graphs).is_empty());
    }

    #[test]
    fn no_graphs_no_links() {
        assert!(resolve_cross_stack(&[]).is_empty());
        let only_consumer = [export(CONSUMER, "consumer", None, "acme")];
        assert!(resolve_cross_stack(&only_consumer).is_empty());
    }

    #[test]
    fn output_key_parsing() {
        assert_eq!(output_key_of("stackoutput::o/p/s::k"), "k");
        assert_eq!(output_key_of("stackoutput::o/p/s::a::b"), "a::b");
        assert_eq!(output_key_of("nope"), "");
    }
}

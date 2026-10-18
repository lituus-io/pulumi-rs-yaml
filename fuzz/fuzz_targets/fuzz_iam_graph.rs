// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

//! Fuzz target: IAM graph export.
//!
//! The raw input is exported as a template, and its bytes are also used as
//! member, role and config values in a generated binding program. Every
//! member must be accounted for exactly once, every edge must start at a
//! principal or a resource of the infra graph, the export must be
//! deterministic, and a value under a secret-declared key must never appear.

#![no_main]
use libfuzzer_sys::fuzz_target;

use pulumi_rs_yaml_core::ast::parse::parse_template;
use pulumi_rs_yaml_core::iam_graph::export_iam_graph;
use pulumi_rs_yaml_core::resource_graph::{export_resource_graph, GraphExportOptions};

fn q(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

fn check(source: &str, member: &str) {
    let (template, _) = parse_template(source, None);
    let mut config = std::collections::HashMap::new();
    config.insert("fuzz:member".to_string(), member.to_string());
    config.insert("fuzz:token".to_string(), "SECRET-CANARY".to_string());
    let opts = GraphExportOptions {
        organization: "org",
        project: "fuzz",
        stack: "dev",
        config: Some(&config),
        ..Default::default()
    };
    let (g1, _) = export_iam_graph(&template, &opts);
    let (g2, _) = export_iam_graph(&template, &opts);
    assert_eq!(g1, g2, "deterministic");
    let s = g1.summary;
    assert_eq!(
        s.members,
        s.member_resolved + s.member_from_resource + s.member_unresolved
    );
    assert!(s.member_from_config <= s.member_resolved);
    let (infra, _) = export_resource_graph(&template, &opts);
    for e in &g1.edges {
        assert!(
            e.source_id.starts_with("principal::")
                || infra.nodes.iter().any(|n| n.id == e.source_id),
            "{}",
            e.source_id
        );
        assert!(
            infra.nodes.iter().any(|n| n.id == e.target_id),
            "{}",
            e.target_id
        );
    }
    let json = g1.to_json().expect("serializes");
    assert!(!json.contains("SECRET-CANARY"));
}

fuzz_target!(|data: &[u8]| {
    let Ok(input) = std::str::from_utf8(data) else {
        return;
    };
    if input.len() > 16 * 1024 {
        return;
    }
    check(input, input);
    let generated = format!(
        "name: fuzz\nruntime: yaml\nconfig:\n  member:\n    type: string\n  token:\n    type: string\n    secret: true\nresources:\n  sa:\n    type: gcp:serviceaccount:Account\n  b:\n    type: gcp:storage:Bucket\n  one:\n    type: gcp:storage:BucketIAMMember\n    properties:\n      bucket: ${{b.name}}\n      role: {r}\n      member: {m}\n  many:\n    type: gcp:projects:IAMBinding\n    properties:\n      project: {m}\n      role: roles/x\n      members:\n        - ${{member}}\n        - ${{token}}\n        - serviceAccount:${{sa.email}}\n        - {m}\n  access:\n    type: gcp:bigquery:DatasetAccess\n    properties:\n      datasetId: d\n      role: READER\n      userByEmail: ${{member}}\n",
        r = q(input),
        m = q(input)
    );
    check(&generated, input);
});

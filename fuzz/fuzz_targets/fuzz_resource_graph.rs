// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

//! Fuzz target: infrastructure dependency-graph export
//!
//! Any template that parses must export without panicking, serialize to
//! valid JSON, and be deterministic (two exports byte-equal).
//!
//! The input is used twice: verbatim as a template, and as the argument
//! material for a generated template whose resource names are built from
//! `str` invokes. The static literal resolver answers those in process, so
//! fuzzer-chosen patterns, subjects and replacements reach the regex engine
//! and the resolver's argument, cycle and output-selection arms through the
//! exporter rather than only through the evaluator.
//!
//! Security targets:
//! - Panics from hostile logical names / type tokens / property paths
//! - Stack overflow from deep parent chains or component nesting
//! - Non-determinism (ordering leaks) in the exported graph
//! - Panics, hangs or non-determinism from `str`-invoke-derived literals

#![no_main]
use libfuzzer_sys::fuzz_target;

use pulumi_rs_yaml_core::resource_graph::{export_resource_graph, GraphExportOptions};

/// Exports a template twice, asserting determinism, serializability and the
/// node-ordering contract.
fn export_and_check(source: &str) {
    // No Box::leak: the exporter borrows for any lifetime, so a local
    // binding suffices and LeakSanitizer stays meaningful.
    let (template, _diags) = pulumi_rs_yaml_core::ast::parse::parse_template(source, None);

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
}

/// A double-quoted YAML scalar carrying arbitrary text.
///
/// YAML's double-quoted style accepts JSON's escapes, so the JSON encoder is
/// the shortest correct quoter here — and one whose correctness is not this
/// target's to prove.
fn scalar(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_string())
}

/// Builds a template whose every resource name is derived from a `str`
/// invoke over fuzzer-chosen arguments.
///
/// Covers both `fn::` spellings, the long `fn::invoke` form with `return:`,
/// chained invokes (one invoke's output as the next one's argument), a
/// bare invoke read with no `return:`, an argument shape that is an object
/// rather than a string, and — under one mode — a cycle, which the resolver
/// must refuse rather than recurse into.
fn str_invoke_template(input: &str) -> String {
    // Five slices of the input, taken at char boundaries so every piece is
    // valid UTF-8 no matter where the fuzzer put its bytes.
    let bounds: Vec<usize> = input.char_indices().map(|(i, _)| i).collect();
    let piece = |n: usize| -> &str {
        if bounds.is_empty() {
            return "";
        }
        let step = bounds.len().div_ceil(5).max(1);
        let start = bounds.get(n * step).copied().unwrap_or(input.len());
        let end = bounds.get((n + 1) * step).copied().unwrap_or(input.len());
        input.get(start..end).unwrap_or("")
    };

    let mut yaml = String::from("name: fuzz\nruntime: yaml\nvariables:\n");
    yaml.push_str(&format!("  seed: {}\n", scalar(piece(0))));

    // `Fn::` spelling, shorthand, argument read from another variable.
    yaml.push_str(&format!(
        "  a:\n    Fn::str:replace:\n      string: ${{seed}}\n      old: {}\n      new: {}\n",
        scalar(piece(1)),
        scalar(piece(2)),
    ));
    // `fn::` spelling, regexp, chained onto the previous invoke's output.
    yaml.push_str(&format!(
        "  b:\n    fn::str:regexp:replace:\n      string: ${{a.result}}\n      old: {}\n      new: {}\n",
        scalar(piece(3)),
        scalar(piece(2)),
    ));
    // Long form with `return:`, whose output is a bool.
    yaml.push_str(&format!(
        "  c:\n    fn::invoke:\n      function: str:regexp:match\n      arguments:\n        string: ${{b.result}}\n        pattern: {}\n      return: matches\n",
        scalar(piece(3)),
    ));
    // Split answers a list, which is never a literal.
    yaml.push_str(&format!(
        "  d:\n    fn::invoke:\n      function: str:regexp:split\n      arguments:\n        string: ${{seed}}\n        on: {}\n      return: result\n",
        scalar(piece(3)),
    ));
    // An object-valued argument: not a literal, so the invoke is not made.
    yaml.push_str(&format!(
        "  e:\n    fn::str:trimSuffix:\n      string:\n        nested: {}\n      suffix: {}\n",
        scalar(piece(4)),
        scalar(piece(4)),
    ));
    // A mutual cycle through arguments, for the modes that ask for it.
    if input.as_bytes().first().is_some_and(|b| b % 2 == 0) {
        yaml.push_str(&format!(
            "  f:\n    fn::str:replace:\n      string: ${{g.result}}\n      old: {}\n      new: {}\n",
            scalar(piece(1)),
            scalar(piece(2)),
        ));
        yaml.push_str(&format!(
            "  g:\n    fn::str:replace:\n      string: ${{f.result}}\n      old: {}\n      new: {}\n",
            scalar(piece(1)),
            scalar(piece(2)),
        ));
    } else {
        yaml.push_str(&format!(
            "  f:\n    fn::str:trimPrefix:\n      string: ${{b.result}}\n      prefix: {}\n",
            scalar(piece(1)),
        ));
        yaml.push_str(&format!(
            "  g:\n    fn::str:trimSuffix:\n      string: ${{f.result}}\n      suffix: {}\n",
            scalar(piece(4)),
        ));
    }

    yaml.push_str(concat!(
        "resources:\n",
        "  r:\n",
        "    type: gcp:storage:Bucket\n",
        "    properties:\n",
        "      name: ${a.result}\n",
        "      regexped: ${b.result}\n",
        "      matched: ${c}\n",
        "      listed: ${d}\n",
        "      objectArg: ${e.result}\n",
        "      cycled: ${g.result}\n",
        "      bare: ${a}\n",
        "      subscript: ${a[\"result\"]}\n",
        "      deep: ${a.result.inner}\n",
        "      mixed: pre-${a.result}-mid-${b.result}-post\n",
    ));
    yaml
}

fuzz_target!(|data: &[u8]| {
    let Ok(input) = std::str::from_utf8(data) else {
        return;
    };
    if input.len() > 64 * 1024 {
        return;
    }

    export_and_check(input);

    // The generated template embeds the same bytes as `str` arguments. Its
    // regex work is bounded but not free, so it takes the smaller inputs.
    if input.len() <= 4 * 1024 {
        export_and_check(&str_invoke_template(input));
    }
});

// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

//! `truncate`, `center` and `wordwrap`, held to the reference implementation.
//!
//! The three filters are Jinja2's, and all three are easy to get subtly wrong
//! by reading the documentation: `truncate` counts its `end` inside `length`
//! and has a `leeway` that suppresses truncation entirely; `center` pads
//! asymmetrically, and not in the direction one would guess; `wordwrap` treats
//! a hyphen between two alphanumerics as a break opportunity.
//!
//! So none of the expected values here were written by hand. They were captured
//! from the reference implementation across a systematic sweep of the parameter
//! space -- 2,992 cases over 22 subjects including empty, whitespace-only,
//! multi-line, hyphenated, and non-ASCII text -- and stored as a fixture. A
//! case the reference REFUSES is recorded as `null`, and the engine has to
//! refuse it too rather than inventing an answer.

use std::collections::HashMap;
use std::path::Path;

use pulumi_rs_yaml_core::jinja::{
    JinjaContext, JinjaPreprocessor, TemplatePreprocessor, UndefinedMode,
};
use serde::Deserialize;

#[derive(Deserialize)]
struct Case {
    expr: String,
    expected: Option<String>,
}

fn corpus() -> Vec<Case> {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/jinja_string_filters.json");
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    serde_json::from_str(&raw).expect("the corpus is valid JSON")
}

/// Renders through the SAME preprocessor a deploy uses, not a hand-built
/// environment: a filter registered in a test harness and not in the real one
/// would pass every case below and still fail every build.
fn render(expr: &str) -> Result<String, String> {
    let config = HashMap::new();
    let extra = HashMap::new();
    let ctx = JinjaContext {
        project_name: "t",
        stack_name: "dev",
        cwd: "/tmp",
        organization: "org",
        root_directory: "/tmp",
        config: &config,
        project_dir: "/tmp",
        undefined: UndefinedMode::Strict,
        provider_templated_packages: &[],
        extra: &extra,
    };
    // The marker keeps the rendered value on a line of its own, so a filter
    // that emits newlines is compared whole rather than truncated at the first.
    let source = format!("<<<{{{{ {expr} }}}}>>>");
    JinjaPreprocessor::new(&ctx)
        .preprocess(&source, "Pulumi.yaml")
        .map_err(|e| e.to_string())
        .map(|out| {
            // Exactly one marker at each end: trimming every occurrence would
            // eat a subject that starts or ends with the marker itself.
            out.strip_prefix("<<<")
                .and_then(|s| s.strip_suffix(">>>"))
                .unwrap_or(&out)
                .to_owned()
        })
}

#[test]
fn every_case_agrees_with_the_reference() {
    let cases = corpus();
    assert!(
        cases.len() > 2_000,
        "the corpus shrank to {} cases -- was it regenerated with a narrower sweep?",
        cases.len()
    );
    let mut refused_by_reference = 0usize;
    let mut failures = Vec::new();
    for case in &cases {
        if case.expected.is_none() {
            refused_by_reference += 1;
        }
        match (render(&case.expr), case.expected.as_deref()) {
            (Ok(got), Some(want)) if got == want => {}
            (Err(_), None) => {}
            (got, want) => failures.push(format!(
                "  {}\n     expected {want:?}\n     got      {got:?}",
                case.expr
            )),
        }
    }
    assert!(
        refused_by_reference > 0,
        "no case in the corpus is one the reference refuses, so the \
         refuse-rather-than-guess half of this test proves nothing"
    );
    assert!(
        failures.is_empty(),
        "{} of {} cases disagree with the reference ({} of which it refuses):\n{}",
        failures.len(),
        cases.len(),
        refused_by_reference,
        failures
            .iter()
            .take(25)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn the_corpus_covers_all_three_filters_and_the_awkward_inputs() {
    //  A sweep that quietly stopped covering something would still pass above.
    let cases = corpus();
    for filter in ["truncate(", "center(", "wordwrap("] {
        let n = cases.iter().filter(|c| c.expr.contains(filter)).count();
        assert!(n > 100, "only {n} cases exercise {filter}");
    }
    for (what, needle) in [
        ("empty string", "'' |"),
        ("non-ASCII", "héllo"),
        ("multi-line", "\\n"),
        ("hyphenated", "well-known"),
        ("consecutive spaces", "hello  world"),
    ] {
        assert!(
            cases.iter().any(|c| c.expr.contains(needle)),
            "the corpus no longer covers {what}"
        );
    }
}

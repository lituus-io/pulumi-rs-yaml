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
//! space -- 3,112 cases over 22 subjects including empty, whitespace-only,
//! multi-line, hyphenated, and non-ASCII text -- and stored as a fixture. A
//! case the reference REFUSES is recorded as `null`, and the engine has to
//! refuse it too rather than inventing an answer.
//!
//! 120 of those cases spell their arguments as KEYWORDS, because Jinja2 authors
//! do (`truncate(length=60)`, `wordwrap(width=40, break_long_words=False)`) and
//! the generated positional sweep reached none of the eleven keyword branches
//! in the implementation. That was a real gap, found by auditing the code
//! against the corpus rather than by a failure.

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
        cases.len() > 3_000,
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
        ("keyword arguments", "length="),
        ("keyword width", "width="),
        ("keyword wrapstring", "wrapstring="),
        ("mixed positional and keyword", "truncate(6, end="),
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

// ---------------------------------------------------------------------------
// The arguments a corpus generated from positional calls cannot reach, and the
// two places the reference is not worth following.
// ---------------------------------------------------------------------------

#[test]
fn a_non_string_subject_is_coerced_by_center_and_refused_by_the_other_two() {
    // Not symmetry for its own sake -- this is what the reference does, checked
    // against it. `center` is `soft_str(value).center(width)`, so a number
    // centres; `truncate` and `wordwrap` reach `len()` and `.splitlines()` on
    // the value itself and raise. A template writing `{{ count | center(8) }}`
    // works everywhere else and must work here.
    assert_eq!(render("123 | center(5)").as_deref(), Ok(" 123 "));
    assert_eq!(render("none | center(5)").as_deref(), Ok(" None"));
    assert_eq!(render("true | center(6)").as_deref(), Ok(" True "));

    for expr in ["123 | truncate(6, False, '', 0)", "123 | wordwrap(4)"] {
        let err = render(expr).expect_err("the reference raises on these");
        assert!(
            err.contains("expected a string"),
            "should say what it wanted: {err}"
        );
    }
}

#[test]
fn a_container_subject_is_refused_rather_than_imitated() {
    // A documented divergence, not an oversight. The reference ANSWERS
    // `{'a':1} | truncate(6, False, '', 0)` with `{'a': 1}` -- but only because
    // `len()` of a one-key dict is 1, so the leeway check short-circuits before
    // the string operation. Give it a seven-key dict and the same call raises
    // KeyError, because slicing a dict is a key lookup. That is Python's dynamic
    // typing producing an answer, not the filter's semantics, and reproducing it
    // would mean implementing `len()` per type to inherit a crash.
    for expr in ["[1,2] | truncate(6, False, '', 0)", "[1,2] | wordwrap(4)"] {
        assert!(
            render(expr).is_err(),
            "a container should be refused by {expr}"
        );
    }
    // `center` coerces containers too, via minijinja's rendering of the value --
    // which spells a map `{"a": 1}` where CPython spells it `{'a': 1}`. That
    // difference is the engine's everywhere (`{{ {'a':1} }}` renders the same
    // way) and is deliberately not special-cased inside one filter.
    let centred = render("[1,2] | center(8)").expect("center coerces");
    assert!(
        centred.contains('['),
        "expected a rendered sequence: {centred}"
    );
}

#[test]
fn an_unknown_keyword_argument_is_named() {
    // `assert_all_used` is called in all three, and nothing exercised it: a typo
    // silently ignored would render a value the author did not ask for.
    for filter in ["truncate", "center", "wordwrap"] {
        let err = render(&format!("'abc' | {filter}(bogus=1)"))
            .unwrap_or_else(|e| e)
            .to_string();
        assert!(
            err.contains("bogus"),
            "{filter} should name the unknown argument: {err}"
        );
    }
}

#[test]
fn positional_and_keyword_arguments_can_be_mixed() {
    // The form the corpus now covers, asserted once here in the open so the
    // intent is visible rather than buried in 3,154 rows.
    assert_eq!(
        render("'hello world how are you' | truncate(11, end='...', leeway=0)").as_deref(),
        Ok("hello...")
    );
    assert_eq!(
        render("'well-known thing' | wordwrap(6, break_on_hyphens=False)").as_deref(),
        Ok("well-k\nnown\nthing")
    );
}

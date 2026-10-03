// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

//! `fn::deriveString`, held against an independent implementation.
//!
//! The value this builtin produces becomes a resource's name, so it is a
//! compatibility contract: if the algorithm moves, every resource named through
//! it is renamed, which a provider carries out as a delete and a create. The
//! fixture therefore carries answers computed by a separate implementation of
//! the algorithm rather than captured from this one — a recording of our own
//! output would agree with any mistake we made.
//!
//! Every case runs through the real evaluator, not a direct call to the
//! builtin. A function that passed here while unregistered in the parser would
//! satisfy every assertion below and still fail every build.

use std::collections::HashMap;

use pulumi_rs_yaml_core::ast::parse::parse_template;
use pulumi_rs_yaml_core::eval::evaluator::Evaluator;
use pulumi_rs_yaml_core::eval::mock::MockCallback;

#[derive(serde::Deserialize)]
struct Case {
    from: String,
    length: Option<u32>,
    alphabet: Option<String>,
    /// `None` means the arguments must be refused, not answered.
    expected: Option<String>,
}

#[derive(serde::Deserialize)]
struct Corpus {
    cases: Vec<Case>,
}

fn corpus() -> Corpus {
    let raw = include_str!("fixtures/derive_string.json");
    serde_json::from_str(raw).expect("the derive fixture parses")
}

/// Renders one case through the evaluator and returns the derived output, or
/// `Err` with the diagnostics when the program was refused.
fn evaluate(case: &Case) -> Result<String, String> {
    // JSON string syntax is valid YAML double-quoted scalar syntax, so the
    // awkward seeds in the corpus — tabs, newlines, non-ASCII, quotes — reach
    // the parser spelled exactly as the oracle hashed them.
    let q = |s: &str| serde_json::to_string(s).expect("a string serialises");
    let mut args = format!("      from: {}\n", q(&case.from));
    if let Some(n) = case.length {
        args.push_str(&format!("      length: {n}\n"));
    }
    if let Some(a) = &case.alphabet {
        args.push_str(&format!("      alphabet: {}\n", q(a)));
    }
    let source = format!(
        "name: test\nruntime: yaml\nvariables:\n  derived:\n    fn::deriveString:\n{args}\
         outputs:\n  result: ${{derived}}\n"
    );

    let (template, parse_diags) = parse_template(&source, None);
    if parse_diags.has_errors() {
        return Err(format!("{parse_diags}"));
    }
    let template: &'static _ = Box::leak(Box::new(template));
    let eval = Evaluator::with_callback(
        "test".to_string(),
        "dev".to_string(),
        "/tmp".to_string(),
        false,
        MockCallback::new(),
    );
    eval.evaluate_template(template, &HashMap::new(), &[]);
    if eval.has_errors() {
        return Err(eval.diags_display());
    }
    eval.get_output("result")
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .ok_or_else(|| "no result output".to_string())
}

#[test]
fn every_case_agrees_with_the_independent_implementation() {
    let corpus = corpus();
    assert!(
        corpus.cases.len() > 500,
        "the corpus has shrunk to {} cases; it is meant to sweep the parameter space",
        corpus.cases.len()
    );
    assert!(
        corpus.cases.iter().any(|c| c.expected.is_none()),
        "the corpus must include arguments the builtin REFUSES, or it only proves the happy path"
    );

    let mut checked = 0usize;
    for case in &corpus.cases {
        let got = evaluate(case);
        match (&case.expected, &got) {
            (Some(want), Ok(have)) => assert_eq!(
                have, want,
                "from={:?} length={:?} alphabet={:?}",
                case.from, case.length, case.alphabet
            ),
            (None, Err(_)) => {}
            (Some(want), Err(e)) => panic!(
                "from={:?} length={:?} alphabet={:?}: expected {want:?}, refused with {e}",
                case.from, case.length, case.alphabet
            ),
            (None, Ok(have)) => panic!(
                "from={:?} length={:?} alphabet={:?}: must be refused, answered {have:?}",
                case.from, case.length, case.alphabet
            ),
        }
        checked += 1;
    }
    assert_eq!(checked, corpus.cases.len());
}

/// A frozen handful, written out in full.
///
/// The corpus above could in principle be regenerated against a changed
/// algorithm and still agree with itself. These cannot: they are literal
/// expected strings in the source. **If one of these changes, every resource in
/// the fleet whose name comes from that seed is renamed, and a provider carries
/// a rename out as a delete and a create.** Changing them is a breaking change
/// to deployed infrastructure, not a test update.
#[test]
fn the_frozen_values_have_not_moved() {
    for (from, want) in [
        ("", "bwgu8ska"),
        ("a", "m7limr9m"),
        ("abc", "6cmbz1ri"),
    ] {
        let case = Case {
            from: from.to_string(),
            length: None,
            alphabet: None,
            expected: None,
        };
        assert_eq!(evaluate(&case).as_deref(), Ok(want), "seed {from:?}");
    }
}

/// The shorthand and the long form must mean the same thing, or an author who
/// moves between them silently renames a resource.
#[test]
fn the_shorthand_equals_the_explicit_default() {
    let source = r#"
name: test
runtime: yaml
variables:
  short:
    fn::deriveString: voice-usage-egress
  long:
    fn::deriveString:
      from: voice-usage-egress
      length: 8
      alphabet: "0123456789abcdefghijklmnopqrstuvwxyz"
outputs:
  a: ${short}
  b: ${long}
"#;
    let (template, d) = parse_template(source, None);
    assert!(!d.has_errors(), "{d}");
    let template: &'static _ = Box::leak(Box::new(template));
    let eval = Evaluator::with_callback(
        "test".to_string(),
        "dev".to_string(),
        "/tmp".to_string(),
        false,
        MockCallback::new(),
    );
    eval.evaluate_template(template, &HashMap::new(), &[]);
    assert!(!eval.has_errors(), "{}", eval.diags_display());
    let a = eval.get_output("a").and_then(|v| v.as_str().map(String::from));
    let b = eval.get_output("b").and_then(|v| v.as_str().map(String::from));
    assert_eq!(a, b);
    assert_eq!(a.as_deref().map(str::len), Some(8));
}

/// Two evaluations in one process, and the same program twice, must agree.
/// This is the property `fn::randomString` does not have and the reason this
/// builtin exists.
#[test]
fn the_same_program_derives_the_same_value_twice() {
    let case = Case {
        from: "tap_collector".to_string(),
        length: Some(4),
        alphabet: None,
        expected: None,
    };
    let first = evaluate(&case).expect("derives");
    let second = evaluate(&case).expect("derives");
    assert_eq!(first, second);
}

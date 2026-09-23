// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

//! Regression tests: `$$` is an escaped dollar, everywhere a value is written.
//!
//! The guard in front of the interpolation parser asked whether a string held
//! a `${...}` reference. A string whose only marker was the escape answered no,
//! its caller read that as "already a literal", and the source text went
//! through untouched -- so `$${data()}` left the engine as `$${data()}`.
//!
//! These tests hold the shapes a program actually writes, so the composition
//! stays proven rather than the two halves separately. Each one fails against
//! the guard that shipped through 0.5.30.

use std::collections::HashMap;

use pulumi_rs_yaml_core::ast::parse::parse_template;
use pulumi_rs_yaml_core::eval::evaluator::Evaluator;
use pulumi_rs_yaml_core::eval::mock::MockCallback;
use pulumi_rs_yaml_core::eval::value::Value;

fn registered_inputs(source: &str) -> HashMap<String, Value<'static>> {
    let (template, diags) = parse_template(source, None);
    assert!(!diags.has_errors(), "parse errors: {diags}");
    let template: &'static _ = Box::leak(Box::new(template));

    let eval = Evaluator::with_callback(
        "test".to_string(),
        "dev".to_string(),
        "/tmp".to_string(),
        false,
        MockCallback::new(),
    );
    eval.evaluate_template(template, &HashMap::new(), &[]);
    assert!(!eval.has_errors(), "eval errors: {}", eval.diags_display());

    let regs = eval.callback().registrations();
    assert_eq!(regs.len(), 1, "expected exactly one registration");
    regs[0]
        .inputs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone().into_owned()))
        .collect()
}

fn field<'a>(v: &'a Value<'a>, key: &str) -> Option<&'a Value<'a>> {
    match v {
        Value::Object(entries) => entries
            .iter()
            .find(|(k, _)| k.as_ref() == key)
            .map(|(_, val)| val),
        _ => None,
    }
}

fn nth<'a>(v: &'a Value<'a>, index: usize) -> Option<&'a Value<'a>> {
    match v {
        Value::List(items) => items.get(index),
        _ => None,
    }
}

/// The field statement: a freshness rule naming the scanning service's own
/// placeholder, once, as the statement's own `FROM` target.
#[test]
fn the_placeholder_named_once_at_the_top_level() {
    let inputs = registered_inputs(
        r#"
name: t
runtime: yaml
resources:
  scan:
    type: gcp:dataplex:Datascan
    properties:
      dataQualitySpec:
        rules:
          - name: freshness
            dimension: FRESHNESS
            sqlAssertion:
              sqlStatement: |
                SELECT insert_ts
                FROM $${data()}
                WHERE insert_ts > TIMESTAMP_SUB(CURRENT_TIMESTAMP(), INTERVAL 1 HOUR)
"#,
    );
    let statement = field(&inputs["dataQualitySpec"], "rules")
        .and_then(|r| nth(r, 0))
        .and_then(|r| field(r, "sqlAssertion"))
        .and_then(|a| field(a, "sqlStatement"))
        .and_then(|v| v.as_str())
        .expect("sqlStatement");
    assert_eq!(
        statement,
        "SELECT insert_ts\nFROM ${data()}\n\
         WHERE insert_ts > TIMESTAMP_SUB(CURRENT_TIMESTAMP(), INTERVAL 1 HOUR)\n"
    );
}

/// The same placeholder named twice, in two common table expressions. The count
/// was never what mattered: one occurrence failed exactly as two did.
#[test]
fn the_placeholder_named_twice_in_common_table_expressions() {
    let inputs = registered_inputs(
        r#"
name: t
runtime: yaml
resources:
  scan:
    type: gcp:dataplex:Datascan
    properties:
      dataQualitySpec:
        rules:
          - name: freshness
            sqlAssertion:
              sqlStatement: |
                WITH
                  a AS (SELECT MAX(insert_ts) AS t FROM $${data()}),
                  b AS (SELECT MIN(insert_ts) AS t FROM $${data()})
                SELECT a.t, b.t
                FROM a, b
                WHERE a.t < b.t
"#,
    );
    let statement = field(&inputs["dataQualitySpec"], "rules")
        .and_then(|r| nth(r, 0))
        .and_then(|r| field(r, "sqlAssertion"))
        .and_then(|a| field(a, "sqlStatement"))
        .and_then(|v| v.as_str())
        .expect("sqlStatement");
    assert_eq!(
        statement,
        "WITH\n  a AS (SELECT MAX(insert_ts) AS t FROM ${data()}),\n  \
         b AS (SELECT MIN(insert_ts) AS t FROM ${data()})\n\
         SELECT a.t, b.t\nFROM a, b\nWHERE a.t < b.t\n"
    );
}

/// An escape beside a real reference: the escape is text, the reference still
/// resolves, and the resource still depends on what it names.
#[test]
fn an_escape_beside_a_reference_keeps_the_dependency() {
    let inputs = registered_inputs(
        r#"
name: t
runtime: yaml
variables:
  tableId: orders
resources:
  scan:
    type: gcp:dataplex:Datascan
    properties:
      sqlStatement: "SELECT * FROM $${data()} -- ${tableId}"
"#,
    );
    assert_eq!(
        inputs["sqlStatement"].as_str(),
        Some("SELECT * FROM ${data()} -- orders")
    );
}

/// A value that only ever meant a currency amount, and a value with no marker
/// at all: neither is rewritten.
#[test]
fn text_that_is_not_an_escape_is_untouched() {
    let inputs = registered_inputs(
        r#"
name: t
runtime: yaml
resources:
  r:
    type: gcp:storage:Bucket
    properties:
      priceLabel: "costs $100 per month"
      plain: "nothing to see"
      trailing: "ends with $"
"#,
    );
    assert_eq!(inputs["priceLabel"].as_str(), Some("costs $100 per month"));
    assert_eq!(inputs["plain"].as_str(), Some("nothing to see"));
    assert_eq!(inputs["trailing"].as_str(), Some("ends with $"));
}

/// The escape survives a round trip through a builtin that re-serializes the
/// value, which is how a statement reaches a provider that wants JSON.
#[test]
fn an_escape_survives_tojson() {
    let inputs = registered_inputs(
        r#"
name: t
runtime: yaml
resources:
  r:
    type: gcp:storage:Bucket
    properties:
      payload:
        fn::toJSON:
          statement: "FROM $${data()}"
"#,
    );
    let payload = inputs["payload"]
        .as_str()
        .expect("payload is a JSON string");
    assert!(
        payload.contains("FROM ${data()}"),
        "toJSON carried the wrong spelling: {payload}"
    );
    assert!(
        !payload.contains("$$"),
        "toJSON carried an uncollapsed escape: {payload}"
    );
}

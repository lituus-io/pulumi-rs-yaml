// Author: spicyzhug@gmail.com
// Copyright: lituus-io, all rights reserved

//! Each render limit at its boundary: exactly at the limit renders, one past
//! it is refused with its own kind. Counted, not timed: fuel is the number of
//! instructions executed, the same on every run.

use pulumi_rs_yaml_core::jinja::{
    JinjaContext, JinjaLimits, JinjaPreprocessor, RenderErrorKind, RenderStats, UndefinedMode,
};
use std::collections::HashMap;

fn render(source: &str, limits: JinjaLimits) -> Result<(String, RenderStats), RenderErrorKind> {
    let config = HashMap::new();
    let extra = HashMap::new();
    let ctx = JinjaContext {
        project_name: "app",
        stack_name: "dev",
        cwd: ".",
        organization: "org",
        root_directory: ".",
        config: &config,
        project_dir: ".",
        undefined: UndefinedMode::Strict,
        provider_templated_packages: &[],
        extra: &extra,
        limits,
    };
    JinjaPreprocessor::new(&ctx)
        .render_with_stats(source, "Pulumi.yaml")
        .map(|(out, stats)| (out.into_owned(), stats))
        .map_err(|d| d.kind)
}

fn with(f: impl FnOnce(&mut JinjaLimits)) -> JinjaLimits {
    let mut l = JinjaLimits::default();
    f(&mut l);
    l
}

const LOOP: &str = "{% for i in range(10) %}{{ i }}{% endfor %}";

#[test]
fn fuel_is_counted_and_deterministic() {
    let (_, a) = render(LOOP, JinjaLimits::default()).unwrap();
    let (_, b) = render(LOOP, JinjaLimits::default()).unwrap();
    assert!(
        a.fuel > 10,
        "a ten-step loop costs more than ten instructions"
    );
    assert_eq!(a, b, "the same render costs the same every time");
    // Twice the iterations, roughly twice the fuel: the count is work done.
    let (_, c) = render(
        "{% for i in range(20) %}{{ i }}{% endfor %}",
        JinjaLimits::default(),
    )
    .unwrap();
    assert!(c.fuel > a.fuel && c.fuel < a.fuel * 3, "{a:?} {c:?}");
}

#[test]
fn fuel_at_the_boundary() {
    let used = render(LOOP, JinjaLimits::default()).unwrap().1.fuel;
    // minijinja refuses the instruction that would bring the tank to zero.
    assert!(render(LOOP, with(|l| l.fuel = used + 1)).is_ok());
    assert_eq!(
        render(LOOP, with(|l| l.fuel = used)).unwrap_err(),
        RenderErrorKind::FuelExhausted
    );
}

#[test]
fn the_zero_copy_path_costs_no_fuel() {
    let (out, stats) = render("plain: yaml\n", with(|l| l.fuel = 1)).unwrap();
    assert_eq!(out, "plain: yaml\n");
    assert_eq!(
        stats,
        RenderStats {
            fuel: 0,
            output_bytes: 12
        }
    );
}

#[test]
fn output_at_the_boundary() {
    let src = "{{ 'x' * 100 }}";
    let (out, stats) = render(src, with(|l| l.output_bytes = 100)).unwrap();
    assert_eq!(out.len(), 100);
    assert_eq!(stats.output_bytes, 100);
    assert_eq!(
        render(src, with(|l| l.output_bytes = 99)).unwrap_err(),
        RenderErrorKind::OutputTooLarge
    );
}

#[test]
fn range_at_the_boundary() {
    assert!(render("{{ range(5)|length }}", with(|l| l.range = 5)).is_ok());
    assert_eq!(
        render("{{ range(6)|length }}", with(|l| l.range = 5)).unwrap_err(),
        RenderErrorKind::RangeTooLarge
    );
    // Length, not bounds, is what is limited.
    assert!(render("{{ range(1000, 1005)|list }}", with(|l| l.range = 5)).is_ok());
    assert!(render("{{ range(0, 50, 10)|list }}", with(|l| l.range = 5)).is_ok());
}

#[test]
fn recursion_at_the_boundary() {
    // Depth-limited recursion: it ends, so only the limit decides.
    let src = |n: u32| {
        format!("{{% macro r(n) %}}{{% if n > 0 %}}{{{{ r(n - 1) }}}}{{% endif %}}{{% endmacro %}}{{{{ r({n}) }}}}")
    };
    let deepest_ok = (1..200)
        .take_while(|n| render(&src(*n), with(|l| l.recursion = 100)).is_ok())
        .last()
        .unwrap();
    assert!(
        deepest_ok >= 10,
        "a shallow recursion must fit: {deepest_ok}"
    );
    assert_eq!(
        render(&src(deepest_ok + 1), with(|l| l.recursion = 100)).unwrap_err(),
        RenderErrorKind::RecursionLimit
    );
    // The default allows far deeper legitimate recursion.
    assert!(render(&src(deepest_ok + 1), JinjaLimits::default()).is_ok());
}

#[test]
fn defaults_are_what_the_documentation_says() {
    let d = JinjaLimits::default();
    assert_eq!(d.fuel, 100_000);
    assert_eq!(d.recursion, 500);
    assert_eq!(d.output_bytes, 16 * 1024 * 1024);
    assert_eq!(d.range, 100_000);
}

/// A resource loop of the shape real stacks take.
fn stack(n: usize) -> String {
    format!(
        "resources:\n{{% for i in range({n}) %}}\n  t{{{{ i }}}}:\n    type: gcp:bigquery:Table\n    properties:\n      tableId: {{{{ ('t_' ~ i) | upper | tojson }}}}\n{{% if i is even %}}      description: {{{{ ('n ' ~ i) | title | tojson }}}}\n{{% endif %}}{{% endfor %}}\n"
    )
}

#[test]
fn work_is_linear_in_resources_with_no_hidden_quadratic() {
    // Counted, not timed: the per-resource cost is a constant, so doubling
    // the stack adds exactly twice the previous increment.
    let fuel = |n| render(&stack(n), JinjaLimits::default()).unwrap().1.fuel;
    let (f50, f100, f200) = (fuel(50), fuel(100), fuel(200));
    assert_eq!(f200 - f100, 2 * (f100 - f50), "{f50} {f100} {f200}");
    // And output grows with it, byte for byte predictable.
    let out = |n| {
        render(&stack(n), JinjaLimits::default())
            .unwrap()
            .1
            .output_bytes
    };
    assert!(out(200) > 2 * out(50) * 3 / 2);
}

#[test]
fn the_default_budget_holds_thousands_of_resources() {
    let per = {
        let f = |n| render(&stack(n), JinjaLimits::default()).unwrap().1.fuel;
        (f(200) - f(100)) / 100
    };
    let capacity = JinjaLimits::DEFAULT_FUEL / per;
    assert!(
        capacity >= 2_000,
        "{per} fuel per resource allows only {capacity}"
    );
}

// ---------------------------------------------------------------------------
// Every limit names where it was crossed
// ---------------------------------------------------------------------------

/// The diagnostic for `source`, rendered under `limits` from `project_dir`.
fn diagnose_in(
    source: &str,
    limits: JinjaLimits,
    project_dir: &str,
) -> (RenderErrorKind, u32, u32, String) {
    let config = HashMap::new();
    let extra = HashMap::new();
    let ctx = JinjaContext {
        project_name: "app",
        stack_name: "dev",
        cwd: project_dir,
        organization: "org",
        root_directory: project_dir,
        config: &config,
        project_dir,
        undefined: UndefinedMode::Strict,
        provider_templated_packages: &[],
        extra: &extra,
        limits,
    };
    let d = JinjaPreprocessor::new(&ctx)
        .render(source, "Pulumi.yaml")
        .expect_err("the limit must refuse");
    let rich = d.format_rich("Pulumi.yaml");
    (d.kind, d.line, d.column, rich)
}

fn diagnose(source: &str, limits: JinjaLimits) -> (RenderErrorKind, u32, u32, String) {
    diagnose_in(source, limits, ".")
}

/// YAML above and below the line under test, which is always line 3.
fn at_line_3(line: &str) -> String {
    format!("name: app\nruntime: yaml\n{line}\nvariables:\n  v: 1\n")
}

#[track_caller]
fn assert_at(got: (RenderErrorKind, u32, u32, String), kind: RenderErrorKind, col: u32) {
    let (k, line, column, rich) = got;
    assert_eq!(k, kind, "{rich}");
    assert_eq!((line, column), (3, col), "{rich}");
    assert!(
        rich.starts_with(&format!("Pulumi.yaml:3:{col}: error: ")),
        "{rich}"
    );
}

#[test]
fn fuel_is_located_at_its_line() {
    let src = at_line_3("x: {% for i in range(100) %}{{ i }}{% endfor %}");
    assert_at(
        diagnose(&src, with(|l| l.fuel = 50)),
        RenderErrorKind::FuelExhausted,
        diagnose(&src, with(|l| l.fuel = 50)).2,
    );
}

#[test]
fn recursion_is_located_at_its_line() {
    let src = at_line_3("{% macro r(n) %}{{ r(n + 1) }}{% endmacro %}x: {{ r(0) }}");
    let got = diagnose(&src, with(|l| l.recursion = 40));
    let col = got.2;
    assert_at(got, RenderErrorKind::RecursionLimit, col);
}

#[test]
fn range_is_located_at_its_expression() {
    let src = at_line_3("x: {{ range(11) | length }}");
    assert_at(
        diagnose(&src, with(|l| l.range = 10)),
        RenderErrorKind::RangeTooLarge,
        7,
    );
}

#[test]
fn output_crossed_by_one_expression_is_located_at_it() {
    let src = at_line_3("x: {{ 'x' * 100 }}");
    assert_at(
        diagnose(&src, with(|l| l.output_bytes = 99)),
        RenderErrorKind::OutputTooLarge,
        7,
    );
}

#[test]
fn output_crossed_by_text_before_an_expression_is_located_at_the_expression() {
    // The expression alone fits; with the text written before it, it does
    // not. The default limit, as the file was found: a comment line that
    // writes just under 16 MiB beneath two lines of YAML.
    let src = "name: r\nruntime: yaml\n# {{ 'x' * 16777200 }}\n";
    let (kind, line, column, rich) = diagnose(src, JinjaLimits::default());
    assert_eq!(kind, RenderErrorKind::OutputTooLarge, "{rich}");
    assert_eq!((line, column), (3, 6), "{rich}");
    assert!(rich.starts_with("Pulumi.yaml:3:6: error: "), "{rich}");
    // The same, under a small limit.
    assert_at(
        diagnose(
            &at_line_3("x: {{ 'x' * 90 }}"),
            with(|l| l.output_bytes = 100),
        ),
        RenderErrorKind::OutputTooLarge,
        7,
    );
}

#[test]
fn output_crossed_by_literal_text_alone_is_located_where_it_crossed() {
    // No expression is ever written: a loop of plain text.
    let src = at_line_3("{% for i in range(50) %}0123456789{% endfor %}");
    let (kind, line, _, rich) = diagnose(&src, with(|l| l.output_bytes = 100));
    assert_eq!(kind, RenderErrorKind::OutputTooLarge, "{rich}");
    assert_eq!(line, 3, "{rich}");
    assert!(rich.starts_with("Pulumi.yaml:3:"), "{rich}");
    // Text after the last expression: the line it crossed on.
    let src = "a: {{ 'x' * 40 }}\nb: 1\nc: 0123456789012345678901234567890123456789\nd: 2\n";
    let (kind, line, column, rich) = diagnose(src, with(|l| l.output_bytes = 70));
    assert_eq!(kind, RenderErrorKind::OutputTooLarge, "{rich}");
    assert_eq!(line, 3, "{rich}");
    assert!(column > 0, "{rich}");
}

#[test]
fn output_crossed_by_a_read_file_is_located_at_the_call() {
    let dir = std::env::temp_dir().join(format!("rsyaml-limit-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("big.txt"), "y".repeat(80)).unwrap();
    let src = at_line_3("x: {{ 'a' * 30 }} {{ readFile('big.txt') }}");
    let got = diagnose_in(&src, with(|l| l.output_bytes = 100), dir.to_str().unwrap());
    let _ = std::fs::remove_dir_all(&dir);
    assert_at(got, RenderErrorKind::OutputTooLarge, 22);
}

#[test]
fn output_crossed_inside_an_include_or_a_parent_is_located_at_the_statement() {
    let dir = std::env::temp_dir().join(format!("rsyaml-limit-inc-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("part.j2"),
        "{% for i in range(50) %}0123456789{% endfor %}",
    )
    .unwrap();
    std::fs::write(
        dir.join("base.j2"),
        "{% for i in range(50) %}0123456789{% endfor %}{% block b %}{% endblock %}",
    )
    .unwrap();
    let d = dir.to_str().unwrap();
    let inc = diagnose_in(
        &at_line_3("{% include 'part.j2' %}"),
        with(|l| l.output_bytes = 100),
        d,
    );
    let ext = diagnose_in(
        "{% extends 'base.j2' %}\n\n{% block b %}x{% endblock %}\n",
        with(|l| l.output_bytes = 100),
        d,
    );
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(
        (inc.0, inc.1),
        (RenderErrorKind::OutputTooLarge, 3),
        "{}",
        inc.3
    );
    assert_eq!(
        (ext.0, ext.1),
        (RenderErrorKind::OutputTooLarge, 1),
        "{}",
        ext.3
    );
}

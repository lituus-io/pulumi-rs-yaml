// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

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

// Author: spicyzhug@gmail.com
// Copyright: lituus-io, all rights reserved

//! The undefined-name policy: every render is strict, passthrough only
//! escapes names this runtime does not define, and compiling for syntax
//! evaluates nothing. Each construct an undefined name can reach is checked
//! in both modes.

use pulumi_rs_yaml_core::jinja::{
    validate_jinja_syntax, JinjaContext, JinjaLimits, JinjaPreprocessor, RenderErrorKind,
    UndefinedMode, UNDEFINED_ENV,
};
use std::collections::HashMap;

fn render(source: &str, mode: UndefinedMode) -> Result<String, RenderErrorKind> {
    let config: HashMap<String, String> = [("proj:region".to_string(), "north".to_string())]
        .into_iter()
        .collect();
    let extra = HashMap::new();
    let ctx = JinjaContext {
        project_name: "app",
        stack_name: "dev",
        cwd: ".",
        organization: "org",
        root_directory: ".",
        config: &config,
        project_dir: ".",
        undefined: mode,
        provider_templated_packages: &[],
        extra: &extra,
        limits: JinjaLimits::default(),
    };
    JinjaPreprocessor::new(&ctx)
        .render(source, "Pulumi.yaml")
        .map(|c| c.into_owned())
        .map_err(|d| d.kind)
}

/// An undefined name in every position it can take.
const POSITIONS: &[(&str, &str)] = &[
    ("print", "v: {{ nope }}"),
    ("attribute", "v: {{ nope.attr }}"),
    ("missing key", "v: {{ config.nope }}"),
    ("filter input", "v: {{ nope | upper }}"),
    ("if", "{% if nope %}x{% endif %}"),
    ("for", "{% for i in nope %}{% endfor %}"),
    ("call", "v: {{ nope() }}"),
    ("concatenation", "v: {{ 'a' ~ nope }}"),
];

#[test]
fn strict_refuses_an_undefined_name_in_every_position() {
    for (what, src) in POSITIONS {
        assert_eq!(
            render(src, UndefinedMode::Strict),
            Err(RenderErrorKind::JinjaUndefinedVariable),
            "{what}: {src}"
        );
    }
}

#[test]
fn the_tests_and_default_that_exist_to_probe_still_work() {
    for (src, want) in [
        ("{{ nope is defined }}", "False"),
        ("{{ nope is undefined }}", "True"),
        ("{{ nope | default('d') }}", "d"),
        ("{{ config.nope | default('d') }}", "d"),
        ("{{ config.get('nope', 'd') }}", "d"),
    ] {
        assert_eq!(render(src, UndefinedMode::Strict).unwrap(), want, "{src}");
    }
}

#[test]
fn passthrough_keeps_foreign_expressions_verbatim_and_is_otherwise_strict() {
    // Names this runtime does not define come out as written ...
    assert_eq!(
        render(
            "sql: select * from {{ ref('m') }}",
            UndefinedMode::Passthrough
        )
        .unwrap(),
        "sql: select * from {{ ref('m') }}"
    );
    assert_eq!(
        render("{{ nope }}", UndefinedMode::Passthrough).unwrap(),
        "{{ nope }}"
    );
    // ... names it does define are still evaluated, typos included ...
    assert_eq!(
        render("{{ config.region }}", UndefinedMode::Passthrough).unwrap(),
        "north"
    );
    assert_eq!(
        render("{{ config.regoin }}", UndefinedMode::Passthrough),
        Err(RenderErrorKind::JinjaUndefinedVariable)
    );
    // ... and statements are never escaped, so they are strict too.
    for src in [
        "{% if nope %}x{% endif %}",
        "{% for i in nope %}{% endfor %}",
    ] {
        assert_eq!(
            render(src, UndefinedMode::Passthrough),
            Err(RenderErrorKind::JinjaUndefinedVariable),
            "{src}"
        );
    }
}

#[test]
fn syntax_validation_evaluates_nothing() {
    // Compiles: every position above is valid syntax ...
    for (what, src) in POSITIONS {
        assert!(validate_jinja_syntax(src, "Pulumi.yaml").is_ok(), "{what}");
    }
    // ... and a syntax error is still one.
    let err = validate_jinja_syntax("{% if %}", "Pulumi.yaml").unwrap_err();
    assert_eq!(err.kind, RenderErrorKind::JinjaSyntax);
}

#[test]
fn the_mode_variable_is_strict_unless_told_otherwise_and_refuses_typos() {
    assert_eq!(UndefinedMode::default(), UndefinedMode::Strict);
    assert_eq!(
        UndefinedMode::from_env_value(None),
        Ok(UndefinedMode::Strict)
    );
    assert_eq!(
        UndefinedMode::from_env_value(Some("")),
        Ok(UndefinedMode::Strict)
    );
    assert_eq!(
        UndefinedMode::from_env_value(Some("strict")),
        Ok(UndefinedMode::Strict)
    );
    assert_eq!(
        UndefinedMode::from_env_value(Some("passthrough")),
        Ok(UndefinedMode::Passthrough)
    );
    for bad in ["lenient", "Passthrough", "pass-through", "off", " strict"] {
        let err = UndefinedMode::from_env_value(Some(bad)).unwrap_err();
        assert!(err.starts_with(UNDEFINED_ENV) && err.contains(bad), "{err}");
    }
}

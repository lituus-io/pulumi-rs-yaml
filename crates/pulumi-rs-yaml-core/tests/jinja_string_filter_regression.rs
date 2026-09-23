// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

//! The field failure that asked for these filters, and the shapes around it.
//!
//! A stack computed a resource id as
//!
//! ```jinja
//! {{ (main.dataProductId ~ '-' ~ asset.dataAssetId | replace("_", "-")) | truncate(60, False, "", 0) }}
//! ```
//!
//! and every deploy of it stopped with `filter truncate is unknown`. The
//! engine's Jinja is minijinja, which carries most of Jinja2's filters and not
//! that one, so the render failed before Pulumi was ever invoked -- and because
//! a render fault is reported per file, the stack read as unrenderable rather
//! than as naming a filter the engine lacks. Same shape as the `tojson` gap
//! closed in 0.5.20.
//!
//! These render through the real preprocessor, so a filter registered only in a
//! test harness would not satisfy them.

use std::collections::HashMap;

use pulumi_rs_yaml_core::jinja::{
    JinjaContext, JinjaPreprocessor, TemplatePreprocessor, UndefinedMode,
};

fn render(body: &str) -> Result<String, String> {
    let config = HashMap::new();
    let extra = HashMap::new();
    let ctx = JinjaContext {
        project_name: "t",
        stack_name: "stg",
        cwd: "/tmp",
        organization: "org",
        root_directory: "/tmp",
        config: &config,
        project_dir: "/tmp",
        undefined: UndefinedMode::Strict,
        provider_templated_packages: &[],
        extra: &extra,
    };
    JinjaPreprocessor::new(&ctx)
        .preprocess(body, "Pulumi.yaml")
        .map(std::borrow::Cow::into_owned)
        .map_err(|e| e.to_string())
}

/// The expression from the failing stack, values and all.
const FIELD_TEMPLATE: &str = concat!(
    "{% set main = {'dataProductId': 'ran_agnostic'} %}",
    "{% set asset = {'dataAssetId': 'kpi_table'} %}",
    "<{{ (main.dataProductId ~ '-' ~ asset.dataAssetId | replace(\"_\", \"-\")) ",
    "| truncate(60, False, \"\", 0) }}>",
);

#[test]
fn the_field_expression_renders() {
    let out = render(FIELD_TEMPLATE).expect("the field expression should render");
    assert!(
        out.contains('<') && out.contains('>'),
        "unexpected shape: {out}"
    );
    // `|` binds tighter than `~` in Jinja, so `replace` applies to the LAST
    // term only -- the id keeps the underscore from `dataProductId`. That is
    // the template's own behaviour, not the filter's, and it is pinned here so
    // that adding `truncate` cannot be mistaken for having changed it.
    assert!(
        out.contains("ran_agnostic-kpi-table"),
        "precedence changed: {out}"
    );
}

#[test]
fn the_same_expression_with_the_paren_moved_replaces_throughout() {
    let template = FIELD_TEMPLATE.replace(
        "(main.dataProductId ~ '-' ~ asset.dataAssetId | replace(\"_\", \"-\"))",
        "(main.dataProductId ~ '-' ~ asset.dataAssetId) | replace(\"_\", \"-\")",
    );
    let out = render(&template).expect("should render");
    assert!(out.contains("ran-agnostic-kpi-table"), "got {out}");
}

#[test]
fn a_sixty_character_cap_is_what_that_call_means() {
    // truncate(60, False, "", 0): no end string and no leeway, so it is a hard
    // cap at 60 characters, cut at a word boundary when there is one.
    let long = "a".repeat(80);
    let out = render(&format!(
        "<{{{{ '{long}' | truncate(60, False, \"\", 0) }}}}>"
    ))
    .expect("should render");
    let inner = out.trim_start_matches('<').trim_end_matches('>').trim();
    assert_eq!(
        inner.chars().count(),
        60,
        "got {} chars",
        inner.chars().count()
    );
}

#[test]
fn a_value_inside_the_cap_is_returned_whole() {
    let out = render("<{{ 'short-id' | truncate(60, False, \"\", 0) }}>").expect("renders");
    assert!(out.contains("<short-id>"), "got {out}");
}

#[test]
fn the_other_two_filters_render_in_a_real_template() {
    let out =
        render("a: {{ 'x' | center(5) }}\nb: {{ 'aa bb cc' | wordwrap(5) }}\n").expect("renders");
    assert!(out.contains("a:   x  "), "center: {out:?}");
    assert!(out.contains("aa bb\ncc"), "wordwrap: {out:?}");
}

#[test]
fn a_filter_the_engine_still_lacks_is_named_as_unknown() {
    // The failure mode this release narrows must stay legible for whatever is
    // missing next: the message names the filter, not the file.
    let err = render("{{ 'x' | xmlattr }}").expect_err("xmlattr is not implemented");
    assert!(
        err.contains("xmlattr"),
        "an unknown filter must name itself: {err}"
    );
}

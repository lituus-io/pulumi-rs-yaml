// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

// The guarantee every existing stack relies on: listing no package changes
// nothing at all.
//
// This is the risk that matters most for a shared runtime. The pre-pass ships
// to every user of the YAML language host, most of whom have never heard of a
// provider-rendered template, and for them the output must be byte-identical to
// what it was before the feature existed.
//
// Checked three ways: an empty list must leave the source untouched without
// allocating; a list naming a package the file does not use must render
// identically to an empty list; and both must agree on failure as well as
// success, since a changed error is a changed behaviour too.
#![no_main]

use std::collections::HashMap;

use arbitrary::Unstructured;
use libfuzzer_sys::fuzz_target;
use pulumi_rs_yaml_core::jinja::{
    JinjaContext, JinjaPreprocessor, TemplatePreprocessor, UndefinedMode,
};
use pulumi_rs_yaml_core::provider_scope::{protect, Protected};

#[path = "scope_grammar.rs"]
mod grammar;

fn render(source: &str, packages: &[&str], mode: UndefinedMode) -> Result<String, String> {
    let mut config = HashMap::new();
    config.insert("gcpProject".to_owned(), "probe-project".to_owned());
    let extra = HashMap::new();
    let ctx = JinjaContext {
        project_name: "probe",
        stack_name: "dev",
        cwd: ".",
        organization: "org",
        root_directory: ".",
        config: &config,
        project_dir: ".",
        undefined: mode,
        provider_templated_packages: packages,
        extra: &extra,
    };
    JinjaPreprocessor::new(&ctx)
        .preprocess(source, "Pulumi.yaml")
        .map(|c| c.into_owned())
        .map_err(|e| e.to_string())
}

fuzz_target!(|data: &[u8]| {
    let mut u = Unstructured::new(data);
    let Ok(stack) = grammar::build(&mut u) else {
        return;
    };
    let source = stack.text.as_str();

    // An empty list must not even look at the file.
    assert!(
        matches!(
            protect(source, &[]).expect("empty list cannot refuse"),
            Protected::Unchanged
        ),
        "an empty package list altered the source"
    );

    for mode in [UndefinedMode::Strict, UndefinedMode::Passthrough] {
        let base = render(source, &[], mode);
        // A package the document never names must be indistinguishable from
        // listing nothing.
        let absent = render(source, &["definitelynotapackage"], mode);
        assert_eq!(
            base, absent,
            "naming an absent package changed the render in {mode:?} mode:\n{source}"
        );
    }
});

// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

// The property a deploy actually depends on: after the full pipeline —
// protect, render, restore — every protected scalar's bytes are identical to
// what the author wrote.
//
// The round-trip target proves protect and restore are inverses in isolation.
// This one puts minijinja between them, which is where the whole exercise
// started: `{{ ref('x') }}` and `{% if is_incremental() %}` must survive a
// renderer that knows neither.
//
// It also asserts the converse. When the generator plants no template syntax in
// an *unscoped* block scalar, rendering must succeed — proving the protection
// actually removed the dbt syntax from minijinja's view rather than the file
// happening to render for some other reason.
#![no_main]

use std::collections::HashMap;

use arbitrary::Unstructured;
use libfuzzer_sys::fuzz_target;
use pulumi_rs_yaml_core::jinja::{
    JinjaContext, JinjaPreprocessor, TemplatePreprocessor, UndefinedMode,
};

#[path = "scope_grammar.rs"]
mod grammar;

fuzz_target!(|data: &[u8]| {
    let mut u = Unstructured::new(data);
    let Ok(stack) = grammar::build(&mut u) else {
        return;
    };
    let source = stack.text.as_str();
    // The pass makes promises about documents that are actually YAML. On one
    // that is not, under-protecting is the safe answer, and the template error
    // that follows is the honest one.
    let parseable = serde_yaml::from_str::<serde_yaml::Value>(source).is_ok();

    let mut config = HashMap::new();
    config.insert("gcpProject".to_owned(), "probe-project".to_owned());
    let extra = HashMap::new();

    for mode in [UndefinedMode::Strict, UndefinedMode::Passthrough] {
        let ctx = JinjaContext {
            project_name: "probe",
            stack_name: "dev",
            cwd: ".",
            organization: "org",
            root_directory: ".",
            config: &config,
            project_dir: ".",
            undefined: mode,
            provider_templated_packages: grammar::SCOPED,
            extra: &extra,
        };
        let out = match JinjaPreprocessor::new(&ctx).preprocess(source, "Pulumi.yaml") {
            Ok(o) => o,
            Err(_) => {
                // Only text still addressed to this runtime may fail to render.
                assert!(
                    stack.unscoped_jinja || stack.bad_indicator || !parseable,
                    "a fully scoped, parseable stack failed to render in {mode:?} mode:\n{source}"
                );
                continue;
            }
        };

        // Every scoped block scalar must appear in the output exactly as
        // written. Comparing the region text rather than the whole file keeps
        // the assertion about protection and not about rendering.
        let protected = match pulumi_rs_yaml_core::provider_scope::protect(source, grammar::SCOPED)
        {
            Ok(p) => p,
            Err(_) => continue,
        };
        for (id, region) in protected.regions().iter().enumerate() {
            assert!(
                out.contains(region.text),
                "region {id} did not survive rendering in {mode:?} mode\n--- wanted ---\n{}\n--- got ---\n{out}",
                region.text
            );
        }
    }
});

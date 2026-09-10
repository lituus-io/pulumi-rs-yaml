// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

//! Fuzz target: Jinja preprocessing pipeline
//!
//! Tests the full Jinja pipeline that processes YAML before evaluation:
//! - strip_jinja_blocks: removes {% %} blocks, must produce valid YAML
//! - validate_jinja_syntax: must never panic
//! - has_jinja_block_syntax: must never panic
//! - classify_expression / extract_root_identifier: string parsing
//!
//! Security targets:
//! - Template injection via crafted {{ }} / {% %} sequences
//! - Stack overflow from nested Jinja blocks
//! - Infinite loops in block stripping

#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(input) = std::str::from_utf8(data) else {
        return;
    };

    if input.len() > 64 * 1024 {
        return;
    }

    // Classification functions must never panic
    let _ = pulumi_rs_yaml_core::jinja::has_jinja_block_syntax(input);
    let _ = pulumi_rs_yaml_core::jinja::classify_expression(input);
    let _ = pulumi_rs_yaml_core::jinja::extract_root_identifier(input);

    // Jinja block stripping must never panic
    if pulumi_rs_yaml_core::jinja::has_jinja_block_syntax(input) {
        let stripped = pulumi_rs_yaml_core::jinja::strip_jinja_blocks(input);

        // Stripped output must no longer be detected as containing
        // standalone block-level Jinja
        assert!(
            !pulumi_rs_yaml_core::jinja::has_jinja_block_syntax(&stripped),
            "strip_jinja_blocks must remove all standalone block-level syntax"
        );
    }

    // Jinja validation must never panic
    let _ = pulumi_rs_yaml_core::jinja::validate_jinja_syntax(input, "fuzz.yaml");

    // validate_rendered_yaml must never panic
    let _ = pulumi_rs_yaml_core::jinja::validate_rendered_yaml(input, input, "fuzz.yaml");

    // pre_escape_for_passthrough must never panic
    let _ = pulumi_rs_yaml_core::jinja::pre_escape_for_passthrough(input);

    // v0.4.0: has_any_jinja_block_syntax must never panic (inline detection)
    let _ = pulumi_rs_yaml_core::jinja::has_any_jinja_block_syntax(input);

    // Consistency: has_any is a superset of has_jinja_block_syntax
    if pulumi_rs_yaml_core::jinja::has_jinja_block_syntax(input) {
        assert!(
            pulumi_rs_yaml_core::jinja::has_any_jinja_block_syntax(input),
            "has_any must be superset of standalone block detection"
        );
    }

    // A full strict render must never panic, and when it fails the diagnostic
    // must be honest about where: the column is inside the line, the
    // expression is what sits at that column, and an include that was
    // refused says so in its message.
    {
        use pulumi_rs_yaml_core::jinja::{
            IncludeRefusal, JinjaContext, JinjaPreprocessor, RenderErrorKind,
            TemplatePreprocessor, UndefinedMode,
        };
        let config = std::collections::HashMap::new();
        let extra = std::collections::HashMap::new();
        let ctx = JinjaContext {
            project_name: "p",
            stack_name: "s",
            cwd: "/nonexistent-fuzz-root",
            organization: "",
            root_directory: "/nonexistent-fuzz-root",
            config: &config,
            project_dir: "/nonexistent-fuzz-root",
            undefined: UndefinedMode::Strict,
            provider_templated_packages: &[],
            extra: &extra,
        };
        // Any text read back as a refusal detail answers without panicking.
        let _ = IncludeRefusal::suggestion_for(input);
        if let Err(diag) = JinjaPreprocessor::new(&ctx).preprocess(input, "fuzz.yaml") {
            if diag.column > 0 {
                let col = (diag.column - 1) as usize;
                assert!(col <= diag.source_line.len(), "column past the line");
                assert!(diag.end_column >= diag.column, "end before start");
                assert!(!diag.expression.is_empty(), "a column with no expression");
                assert_eq!(
                    diag.source_line.get(col..col + diag.expression.len()),
                    Some(diag.expression),
                    "the expression is not at its column"
                );
            } else {
                assert_eq!(diag.end_column, 0);
                assert!(diag.expression.is_empty());
            }
            if diag.message.starts_with(IncludeRefusal::PREFIX) {
                assert_eq!(diag.kind, RenderErrorKind::JinjaTemplateNotFound);
                assert!(IncludeRefusal::suggestion_for(&diag.message).is_some());
                assert!(
                    [
                        IncludeRefusal::TAG_ABSOLUTE,
                        IncludeRefusal::TAG_ESCAPE,
                        IncludeRefusal::TAG_BINARY,
                        IncludeRefusal::TAG_TOO_LARGE,
                    ]
                    .iter()
                    .any(|tag| diag.message.contains(tag)),
                    "a refusal carries one of the four tags"
                );
            }
            let _ = diag.format_rich("fuzz.yaml");
        }
    }
});

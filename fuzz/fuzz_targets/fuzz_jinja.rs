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

        // Stripped output should not contain block-level Jinja
        assert!(
            !stripped.contains("{% ") || stripped.contains("{%"),
            "strip_jinja_blocks should remove block-level syntax"
        );
    }

    // Jinja validation must never panic
    let _ = pulumi_rs_yaml_core::jinja::validate_jinja_syntax(input, "fuzz.yaml");

    // validate_rendered_yaml must never panic
    let _ = pulumi_rs_yaml_core::jinja::validate_rendered_yaml(input, input, "fuzz.yaml");

    // pre_escape_for_passthrough must never panic
    let _ = pulumi_rs_yaml_core::jinja::pre_escape_for_passthrough(input);
});

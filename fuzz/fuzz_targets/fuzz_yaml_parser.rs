//! Fuzz target: YAML template parser
//!
//! Tests parse_template() with arbitrary YAML-like input to find:
//! - Panics on malformed input
//! - Stack overflows from deeply nested structures
//! - OOM from adversarial anchor/alias expansion
//! - Unexpected crashes in AST construction

#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(input) = std::str::from_utf8(data) else {
        return;
    };

    // Cap input size to prevent trivially large allocations
    if input.len() > 64 * 1024 {
        return;
    }

    // parse_template must never panic on any input
    let (template, diags) = pulumi_rs_yaml_core::ast::parse::parse_template(input, None);

    // If parsing succeeded, exercise the AST — trigger Display, Debug, Clone
    if !diags.has_errors() {
        let _ = format!("{:?}", template);
        let _clone = template.clone();
    }
});

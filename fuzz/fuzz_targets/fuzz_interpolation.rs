//! Fuzz target: Interpolation parser
//!
//! Tests parse_interpolation() and has_interpolations() with arbitrary strings.
//! Targets:
//! - Panics on malformed ${...} expressions
//! - Off-by-one in byte indexing (multi-byte UTF-8)
//! - Infinite loops on crafted input
//! - Property access parsing edge cases

#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(input) = std::str::from_utf8(data) else {
        return;
    };

    if input.len() > 4096 {
        return;
    }

    // has_interpolations must never panic
    let _ = pulumi_rs_yaml_core::ast::interpolation::has_interpolations(input);

    // parse_interpolation must never panic
    let mut diags = pulumi_rs_yaml_core::diag::Diagnostics::new();
    let parts = pulumi_rs_yaml_core::ast::interpolation::parse_interpolation(input, None, &mut diags);
    for part in &parts {
        let _ = format!("{:?}", part);
    }
});

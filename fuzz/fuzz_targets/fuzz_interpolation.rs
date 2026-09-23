// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

//! Fuzz target: Interpolation parser
//!
//! Tests parse_interpolation() and needs_interpolation_pass() with arbitrary
//! strings.
//! Targets:
//! - Panics on malformed ${...} expressions
//! - Off-by-one in byte indexing (multi-byte UTF-8)
//! - Infinite loops on crafted input
//! - Property access parsing edge cases
//! - A guard that disagrees with the parser about which strings it must see

#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(input) = std::str::from_utf8(data) else {
        return;
    };

    if input.len() > 4096 {
        return;
    }

    // needs_interpolation_pass must never panic
    let needed = pulumi_rs_yaml_core::ast::interpolation::needs_interpolation_pass(input);

    // parse_interpolation must never panic
    let mut diags = pulumi_rs_yaml_core::diag::Diagnostics::new();
    let parts = pulumi_rs_yaml_core::ast::interpolation::parse_interpolation(input, None, &mut diags);
    for part in &parts {
        let _ = format!("{:?}", part);
    }

    // The guard decides whether the parser runs at all, so a string it turns
    // away must be one the parser would have handed back unchanged. Anything
    // else is a value that reaches a provider in a spelling nobody wrote.
    if !needed && !diags.has_errors() {
        let text: String = parts.iter().map(|p| p.text.as_ref()).collect();
        assert!(
            parts.iter().all(|p| p.value.is_none()),
            "skipped a string the parser resolves: {input:?}"
        );
        assert_eq!(
            text, input,
            "skipped a string the parser rewrites: {input:?}"
        );
    }
});

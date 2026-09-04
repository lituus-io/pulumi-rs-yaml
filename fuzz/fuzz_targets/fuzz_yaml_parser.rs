// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

//! Fuzz target: YAML template parser
//!
//! Tests parse_template() with arbitrary YAML-like input to find:
//! - Panics on malformed input
//! - Stack overflows from deeply nested structures
//! - OOM from adversarial anchor/alias expansion
//! - Unexpected crashes in AST construction
//!
//! It also carries the differential property for the byte order mark: YAML 1.2
//! lets a stream begin with one, so the same bytes with and without a leading
//! mark are the same document and must parse to the same template and the same
//! diagnostics. Stated this way the property is two-sided — it fails if the
//! mark is not removed, and equally if removing it changes anything else about
//! the parse.

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

    // Inputs that already begin with a mark are excluded: prefixing a second
    // one is a different document, since exactly one mark at offset zero is a
    // stream marker and the rest is content.
    if input.starts_with(pulumi_rs_yaml_core::encoding::UTF8_BOM) {
        return;
    }
    let marked = format!("{}{input}", pulumi_rs_yaml_core::encoding::UTF8_BOM);
    let (marked_template, marked_diags) =
        pulumi_rs_yaml_core::ast::parse::parse_template(&marked, None);
    assert_eq!(
        diags.has_errors(),
        marked_diags.has_errors(),
        "a leading mark changed whether this parses"
    );
    assert_eq!(
        diags.to_string(),
        marked_diags.to_string(),
        "a leading mark changed the diagnostics"
    );
    assert_eq!(
        format!("{template:?}"),
        format!("{marked_template:?}"),
        "a leading mark changed the template"
    );
});

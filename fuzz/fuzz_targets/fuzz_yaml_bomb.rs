//! Fuzz target: YAML bomb / resource exhaustion
//!
//! Specifically targets denial-of-service vectors:
//! - Billion-laughs via YAML anchors/aliases
//! - Deep nesting causing stack overflow
//! - Huge key/value counts causing OOM
//! - Pathological interpolation strings
//!
//! Runs with strict memory/time limits to detect resource exhaustion.

#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(input) = std::str::from_utf8(data) else {
        return;
    };

    // Allow slightly larger inputs for bomb detection
    if input.len() > 128 * 1024 {
        return;
    }

    // Test 1: YAML parsing must not expand exponentially
    // serde_yaml processes anchors/aliases — watch for blowup
    {
        let result: Result<serde_yaml::Value, _> = serde_yaml::from_str(input);
        // We don't care if it errors; we care that it doesn't hang or OOM
        let _ = result;
    }

    // Test 2: Full template parsing pipeline
    {
        let (template, _diags) = pulumi_rs_yaml_core::ast::parse::parse_template(input, None);
        // Exercise the AST size — detect quadratic blowup
        let debug = format!("{:?}", template);
        // If debug output is >10MB from <128KB input, that's suspicious
        assert!(
            debug.len() < 10 * 1024 * 1024,
            "AST expansion ratio too high: {}B input → {}B AST debug",
            input.len(),
            debug.len()
        );
    }

    // Test 3: Jinja preprocessing pipeline
    {
        if pulumi_rs_yaml_core::jinja::has_jinja_block_syntax(input) {
            let stripped = pulumi_rs_yaml_core::jinja::strip_jinja_blocks(input);
            assert!(
                stripped.len() < 10 * 1024 * 1024,
                "Jinja strip expansion ratio too high: {}B input → {}B output",
                input.len(),
                stripped.len()
            );
        }
    }

    // Test 4: Interpolation parsing on every line
    // (tests for quadratic behavior on many ${} in one string)
    {
        for line in input.lines().take(1000) {
            if line.len() > 4096 {
                continue;
            }
            let mut diags = pulumi_rs_yaml_core::diag::Diagnostics::new();
            let _ = pulumi_rs_yaml_core::ast::interpolation::parse_interpolation(line, None, &mut diags);
        }
    }
});

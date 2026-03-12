//! Fuzz target: YAML-to-PCL converter
//!
//! Tests the full yaml_to_pcl pipeline with arbitrary YAML input.
//! This exercises: parsing → AST → PCL code generation.
//!
//! Security targets:
//! - Panics in the converter on malformed YAML
//! - OOM from adversarial YAML structures
//! - Stack overflow from deeply nested expressions
//! - Correctness: converter must not crash regardless of input

#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(input) = std::str::from_utf8(data) else {
        return;
    };

    // Cap at 64KB to avoid trivially large inputs
    if input.len() > 64 * 1024 {
        return;
    }

    // yaml_to_pcl must never panic on any input
    let result = pulumi_rs_yaml_converter::yaml_to_pcl(input);

    // Exercise the output — ensure Display/Debug don't panic
    let _ = format!("{}", result.diagnostics);
    let _ = result.pcl_text.len();
});

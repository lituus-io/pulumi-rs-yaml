#![no_main]
// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

use libfuzzer_sys::fuzz_target;
use std::borrow::Cow;
use std::collections::HashMap;

use pulumi_rs_yaml_core::ast::template::StarlarkFunctionDecl;
use pulumi_rs_yaml_core::diag::Diagnostics;
use pulumi_rs_yaml_core::eval::starlark_runtime::StarlarkRuntime;
use pulumi_rs_yaml_core::eval::value::Value;

fuzz_target!(|data: &[u8]| {
    // Convert bytes to a string for use as starlark source
    let source = match std::str::from_utf8(data) {
        Ok(s) => s,
        Err(_) => return,
    };

    // Skip empty inputs
    if source.is_empty() {
        return;
    }

    // Skip scripts that ask for an allocation BY DESIGN.
    //
    // `'   '*333333333` is a correct Starlark program that allocates about a
    // gigabyte, and this target found exactly that: an out-of-memory at
    // 2202 MB against libFuzzer's 2048 MB limit. It is not an engine defect.
    // starlark-rust offers no heap limit -- there is no `set_max_*` on the
    // evaluator and `Heap::alloc` is infallible -- and the allocation happens
    // inside ONE expression, so no statement hook and no wall-clock budget can
    // intervene before the memory is taken. The script also comes from the
    // `starlark:` block of the author's own program, so an author who writes
    // it has broken only their own build.
    //
    // What this target is for is PANICS and hangs on hostile text, so asking
    // it for deliberate exhaustion only produces findings a human has to
    // triage back to "working as intended" -- which is what happened once.
    //
    // The filter is a HARNESS heuristic and deliberately not an engine rule:
    // a skipped input is merely untested, so being approximate is free here
    // in a way it would not be in the engine. It is also knowingly
    // incomplete -- a script can reach a large number without a large literal
    // -- so `**` is skipped too, that being the other amplifier. A shape that
    // slips through still exhausts memory, and the answer to that is the
    // triage note above, not a cleverer regex.
    if asks_for_a_huge_allocation(source) {
        return;
    }

    // Try to compile the starlark source as a function definition
    let func = StarlarkFunctionDecl {
        name: Cow::Borrowed("fuzz_func"),
        script: Cow::Owned(source.to_string()),
    };

    let mut diags = Diagnostics::new();
    let runtime = StarlarkRuntime::compile(&[func], &mut diags);

    // If compilation succeeded, try calling the function with various inputs
    if !diags.has_errors() && runtime.has_function("fuzz_func") {
        let test_inputs = [
            Value::Null,
            Value::Bool(true),
            Value::Number(42.0),
            Value::String(Cow::Borrowed("test")),
            Value::List(vec![Value::Number(1.0), Value::Number(2.0)]),
            Value::Object(vec![(Cow::Borrowed("key"), Value::String(Cow::Borrowed("val")))]),
        ];

        for input in &test_inputs {
            let mut call_diags = Diagnostics::new();
            // This must not panic regardless of the starlark source
            let _ = runtime.call("fuzz_func", input, &mut call_diags);
        }
    }

    // Also fuzz the full template pipeline
    let yaml_source = format!(
        r#"name: fuzz
runtime: yaml
starlark:
  functions:
    fuzz_func:
      script: |
        {}
variables:
  result:
    fn::starlark:
      invoke: fuzz_func
      input: test
"#,
        source
            .lines()
            .map(|l| format!("        {}", l))
            .collect::<Vec<_>>()
            .join("\n")
    );

    let (template, _parse_diags) = pulumi_rs_yaml_core::ast::parse::parse_template(&yaml_source, None);
    let eval = pulumi_rs_yaml_core::eval::evaluator::Evaluator::new(
        "fuzz".to_string(),
        "dev".to_string(),
        "/tmp".to_string(),
        false,
    );
    let raw_config = HashMap::new();
    // Must not panic
    eval.evaluate_template(&template, &raw_config, &[]);
});

/// True when the script names a number big enough to make an intentional
/// allocation, or exponentiates to reach one.
///
/// Seven digits is the bound because that is where plausible constants stop:
/// a year, a port, a timeout in milliseconds and `1048576` all fit, while the
/// eight-digit-and-longer runs are the repeat counts that turn a
/// fifteen-character expression into a gigabyte. The check is a scan for a RUN
/// of digits rather than a parse, so it costs one pass over the bytes and
/// needs no Starlark grammar.
fn asks_for_a_huge_allocation(source: &str) -> bool {
    if source.contains("**") {
        return true;
    }
    let mut digits = 0usize;
    for b in source.bytes() {
        if b.is_ascii_digit() {
            digits += 1;
            if digits > 7 {
                return true;
            }
        } else {
            digits = 0;
        }
    }
    false
}

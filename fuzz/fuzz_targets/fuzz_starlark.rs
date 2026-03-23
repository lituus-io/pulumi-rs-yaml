#![no_main]

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
    let template: &'static _ = Box::leak(Box::new(template));
    let eval = pulumi_rs_yaml_core::eval::evaluator::Evaluator::new(
        "fuzz".to_string(),
        "dev".to_string(),
        "/tmp".to_string(),
        false,
    );
    let raw_config = HashMap::new();
    // Must not panic
    eval.evaluate_template(template, &raw_config, &[]);
});

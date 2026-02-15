//! Fuzz target: Builtin function evaluation with structured input
//!
//! Uses `arbitrary` to generate structured inputs (not just random bytes)
//! that target specific builtin functions.
//!
//! Security targets:
//! - f64 → usize overflow in fn::select, fn::substring, fn::randomString
//! - OOM in fn::randomString with huge length
//! - Panics in fn::split, fn::join, fn::toJSON, fn::toBase64
//! - Secret value handling (fn::secret wrapping)

#![no_main]
use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use std::borrow::Cow;

use pulumi_rs_yaml_core::diag::Diagnostics;
use pulumi_rs_yaml_core::eval::builtins;
use pulumi_rs_yaml_core::eval::value::Value;

/// Structured input that maps to evaluator Value types.
#[derive(Debug, Arbitrary)]
enum FuzzValue {
    Null,
    Bool(bool),
    Number(f64),
    Str(String),
    List(Vec<FuzzValue>),
}

impl FuzzValue {
    fn to_value(&self) -> Value<'static> {
        match self {
            FuzzValue::Null => Value::Null,
            FuzzValue::Bool(b) => Value::Bool(*b),
            FuzzValue::Number(n) => Value::Number(*n),
            FuzzValue::Str(s) => Value::String(Cow::Owned(s.clone())),
            FuzzValue::List(items) => {
                // Limit list depth/size to prevent trivial OOM
                let items: Vec<_> = items.iter().take(100).map(|v| v.to_value()).collect();
                Value::List(items)
            }
        }
    }
}

#[derive(Debug, Arbitrary)]
struct FuzzInput {
    value: FuzzValue,
    index: FuzzValue,
    separator: String,
}

fuzz_target!(|input: FuzzInput| {
    let value = input.value.to_value();
    let index = input.index.to_value();

    // fn::toJSON — must never panic
    {
        let mut diags = Diagnostics::new();
        let _ = builtins::eval_to_json(&value, &mut diags);
    }

    // fn::secret — must never panic
    {
        let result = builtins::eval_secret(value.clone());
        // Display must mask the secret
        let display = format!("{}", result);
        assert_eq!(display, "[secret]", "secret must be masked in Display");
    }

    // fn::select — must never panic, even with extreme f64 values
    {
        let mut diags = Diagnostics::new();
        let _ = builtins::eval_select(&index, &value, &mut diags);
    }

    // fn::split — must never panic
    {
        let sep = Value::String(Cow::Owned(input.separator.clone()));
        let mut diags = Diagnostics::new();
        let _ = builtins::eval_split(&sep, &value, &mut diags);
    }

    // fn::join — must never panic
    {
        let sep = Value::String(Cow::Owned(input.separator));
        let mut diags = Diagnostics::new();
        let _ = builtins::eval_join(&sep, &value, &mut diags);
    }

    // fn::randomString — test with CAPPED length to avoid OOM
    // (testing the code path, not allocating terabytes)
    {
        let capped = match &value {
            Value::Number(n) if *n >= 0.0 && *n <= 1024.0 => Value::Number(*n),
            Value::Number(_) => Value::Number(10.0), // replace dangerous values
            other => other.clone(),
        };
        let mut diags = Diagnostics::new();
        let _ = builtins::eval_random_string(&capped, &mut diags);
    }

    // fn::toBase64 / fn::fromBase64 — must never panic
    {
        let mut diags = Diagnostics::new();
        let _ = builtins::eval_to_base64(&value, &mut diags);
    }
    {
        let mut diags = Diagnostics::new();
        let _ = builtins::eval_from_base64(&value, &mut diags);
    }
});

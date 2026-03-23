//! Starlark function runtime for `fn::starlark` evaluation.
//!
//! Compiles user-defined Starlark functions from the `starlark:` top-level block
//! into frozen modules (Send + Sync), then executes them on demand during
//! expression evaluation.
//!
//! # Thread Safety
//!
//! `StarlarkRuntime` stores `FrozenModule` values which are `Send + Sync`.
//! Each `call()` invocation creates a transient `Module` + `Evaluator` on the
//! current thread, so parallel evaluation via rayon is safe.

use std::borrow::Cow;
use std::collections::HashMap;

use starlark::environment::{FrozenModule, Globals, Module};
use starlark::eval::Evaluator as StarlarkEvaluator;
use starlark::syntax::{AstModule, Dialect};
use starlark::values::dict::DictRef;
use starlark::values::list::ListRef;
use starlark::values::{Value as StarlarkValue, ValueLike};

use crate::ast::template::StarlarkFunctionDecl;
use crate::diag::Diagnostics;
use crate::eval::value::Value;

/// Compiled Starlark environment holding all user-defined functions.
///
/// Each function is compiled into its own `FrozenModule` for isolation.
/// `FrozenModule` is `Send + Sync`, so this struct can live inside
/// `EvalState` and be accessed from rayon worker threads.
pub struct StarlarkRuntime {
    /// Compiled frozen modules keyed by function name.
    modules: HashMap<String, FrozenModule>,
}

// Compile-time assertion that StarlarkRuntime is Send + Sync.
const _: () = {
    fn _assert_send_sync<T: Send + Sync>() {}
    fn _check() {
        _assert_send_sync::<StarlarkRuntime>();
    }
};

impl StarlarkRuntime {
    /// Compiles all Starlark function declarations into frozen modules.
    ///
    /// Each function's script is parsed, evaluated (to define the function),
    /// and frozen. Compilation errors are emitted as diagnostics.
    pub fn compile(functions: &[StarlarkFunctionDecl<'_>], diags: &mut Diagnostics) -> Self {
        let mut modules = HashMap::new();
        let globals = Globals::standard();

        for func in functions {
            let ast = match AstModule::parse(
                &format!("{}.star", func.name),
                func.script.to_string(),
                &Dialect::Standard,
            ) {
                Ok(ast) => ast,
                Err(e) => {
                    diags.error(
                        None,
                        format!("starlark parse error in '{}': {}", func.name, e),
                        "",
                    );
                    continue;
                }
            };

            let module = Module::new();
            {
                let mut eval = StarlarkEvaluator::new(&module);
                if let Err(e) = eval.eval_module(ast, &globals) {
                    diags.error(
                        None,
                        format!("starlark compile error in '{}': {}", func.name, e),
                        "",
                    );
                    continue;
                }
            }

            match module.freeze() {
                Ok(frozen) => {
                    modules.insert(func.name.to_string(), frozen);
                }
                Err(e) => {
                    diags.error(
                        None,
                        format!("starlark freeze error in '{}': {:?}", func.name, e),
                        "",
                    );
                }
            }
        }

        StarlarkRuntime { modules }
    }

    /// Returns true if a function with the given name exists.
    pub fn has_function(&self, name: &str) -> bool {
        self.modules.contains_key(name)
    }

    /// Calls a compiled Starlark function with the given input value.
    ///
    /// Creates a fresh `Module` per call (thread-local, dropped after call).
    /// The input `Value` is converted to a Starlark value, the function is
    /// invoked, and the result is converted back.
    pub fn call(
        &self,
        function_name: &str,
        input: &Value<'_>,
        diags: &mut Diagnostics,
    ) -> Option<Value<'static>> {
        let frozen = match self.modules.get(function_name) {
            Some(m) => m,
            None => {
                diags.error(
                    None,
                    format!("starlark function '{}' not found", function_name),
                    "",
                );
                return None;
            }
        };

        // Look up the function from the frozen module
        let owned_func = match frozen.get(function_name) {
            Ok(v) => v,
            Err(e) => {
                diags.error(
                    None,
                    format!(
                        "starlark function '{}' not defined in script: {}",
                        function_name, e
                    ),
                    "",
                );
                return None;
            }
        };

        let module = Module::new();
        let mut eval = StarlarkEvaluator::new(&module);

        // Convert pulumi Value to starlark Value
        let heap = module.heap();
        let starlark_input = value_to_starlark(input, heap);

        // Convert the frozen function value to a live value for the current module
        let func_value = owned_func.value().to_value();

        // Call the function with the input as a single positional argument
        match eval.eval_function(func_value, &[starlark_input], &[]) {
            Ok(result) => match starlark_to_value(result) {
                Ok(v) => Some(v),
                Err(e) => {
                    diags.error(
                        None,
                        format!(
                            "starlark function '{}' returned unconvertible type: {}",
                            function_name, e
                        ),
                        "",
                    );
                    None
                }
            },
            Err(e) => {
                diags.error(
                    None,
                    format!("starlark runtime error in '{}': {}", function_name, e),
                    "",
                );
                None
            }
        }
    }
}

/// Converts a pulumi `Value` to a Starlark value on the given heap.
fn value_to_starlark<'v>(val: &Value<'_>, heap: &'v starlark::values::Heap) -> StarlarkValue<'v> {
    match val {
        Value::Null => StarlarkValue::new_none(),
        Value::Bool(b) => StarlarkValue::new_bool(*b),
        Value::Number(n) => {
            // If the number is an exact integer, allocate as i64 for starlark's int type
            if n.fract() == 0.0 && n.is_finite() && *n >= i64::MIN as f64 && *n <= i64::MAX as f64 {
                heap.alloc(*n as i64)
            } else {
                heap.alloc(*n)
            }
        }
        Value::String(s) => heap.alloc_str(s.as_ref()).to_value(),
        Value::List(items) => {
            let converted: Vec<StarlarkValue<'v>> =
                items.iter().map(|v| value_to_starlark(v, heap)).collect();
            heap.alloc(converted)
        }
        Value::Object(entries) => {
            let mut dict =
                starlark::values::dict::Dict::new(starlark::collections::SmallMap::new());
            for (k, v) in entries {
                let key = heap.alloc_str(k.as_ref()).to_value();
                let val = value_to_starlark(v, heap);
                // String keys are always hashable in starlark
                if let Ok(hashed_key) = key.get_hashed() {
                    dict.insert_hashed(hashed_key, val);
                }
            }
            heap.alloc(dict)
        }
        // Secret: unwrap and convert inner (caller handles re-wrapping)
        Value::Secret(inner) => value_to_starlark(inner, heap),
        // Unknown should be short-circuited before reaching here
        Value::Unknown => StarlarkValue::new_none(),
        // Resource, Asset, Archive are not bridgeable — convert to None
        _ => StarlarkValue::new_none(),
    }
}

/// Converts a Starlark result value to a pulumi `Value`.
fn starlark_to_value(val: StarlarkValue<'_>) -> Result<Value<'static>, String> {
    if val.is_none() {
        return Ok(Value::Null);
    }
    if let Some(b) = val.unpack_bool() {
        return Ok(Value::Bool(b));
    }
    if let Some(i) = val.unpack_i32() {
        return Ok(Value::Number(i as f64));
    }
    if let Some(s) = val.unpack_str() {
        return Ok(Value::String(Cow::Owned(s.to_string())));
    }
    // Try list
    if let Some(list) = ListRef::from_value(val) {
        let items: Result<Vec<Value<'static>>, String> =
            list.iter().map(|v| starlark_to_value(v)).collect();
        return items.map(Value::List);
    }
    // Try dict
    if let Some(dict) = DictRef::from_value(val) {
        let entries: Result<Vec<(Cow<'static, str>, Value<'static>)>, String> = dict
            .iter()
            .map(|(k, v)| {
                let key = k
                    .unpack_str()
                    .ok_or_else(|| format!("dict key must be a string, got {}", k.get_type()))?;
                let value = starlark_to_value(v)?;
                Ok((Cow::Owned(key.to_string()), value))
            })
            .collect();
        return entries.map(Value::Object);
    }
    // Fallback: convert to string representation
    Ok(Value::String(Cow::Owned(val.to_string())))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_func(name: &str, script: &str) -> StarlarkFunctionDecl<'static> {
        StarlarkFunctionDecl {
            name: Cow::Owned(name.to_string()),
            script: Cow::Owned(script.to_string()),
        }
    }

    #[test]
    fn test_compile_valid_function() {
        let funcs = vec![make_func(
            "uppercase",
            "def uppercase(s):\n    return s.upper()\n",
        )];
        let mut diags = Diagnostics::new();
        let rt = StarlarkRuntime::compile(&funcs, &mut diags);
        assert!(!diags.has_errors(), "unexpected errors: {}", diags);
        assert!(rt.has_function("uppercase"));
    }

    #[test]
    fn test_compile_syntax_error() {
        let funcs = vec![make_func("bad", "def bad(:\n")];
        let mut diags = Diagnostics::new();
        let rt = StarlarkRuntime::compile(&funcs, &mut diags);
        assert!(diags.has_errors());
        assert!(!rt.has_function("bad"));
    }

    #[test]
    fn test_compile_multiple_functions() {
        let funcs = vec![
            make_func("upper", "def upper(s):\n    return s.upper()\n"),
            make_func("lower", "def lower(s):\n    return s.lower()\n"),
        ];
        let mut diags = Diagnostics::new();
        let rt = StarlarkRuntime::compile(&funcs, &mut diags);
        assert!(!diags.has_errors());
        assert!(rt.has_function("upper"));
        assert!(rt.has_function("lower"));
    }

    #[test]
    fn test_call_string_function() {
        let funcs = vec![make_func(
            "uppercase",
            "def uppercase(s):\n    return s.upper()\n",
        )];
        let mut diags = Diagnostics::new();
        let rt = StarlarkRuntime::compile(&funcs, &mut diags);
        assert!(!diags.has_errors());

        let input = Value::String(Cow::Borrowed("hello"));
        let result = rt.call("uppercase", &input, &mut diags);
        assert!(!diags.has_errors());
        assert_eq!(result, Some(Value::String(Cow::Owned("HELLO".to_string()))));
    }

    #[test]
    fn test_call_with_list_arg() {
        let funcs = vec![make_func(
            "double",
            "def double(items):\n    return [x * 2 for x in items]\n",
        )];
        let mut diags = Diagnostics::new();
        let rt = StarlarkRuntime::compile(&funcs, &mut diags);

        let input = Value::List(vec![
            Value::Number(1.0),
            Value::Number(2.0),
            Value::Number(3.0),
        ]);
        let result = rt.call("double", &input, &mut diags);
        assert!(!diags.has_errors());
        assert_eq!(
            result,
            Some(Value::List(vec![
                Value::Number(2.0),
                Value::Number(4.0),
                Value::Number(6.0),
            ]))
        );
    }

    #[test]
    fn test_call_with_dict_arg() {
        let funcs = vec![make_func(
            "get_name",
            "def get_name(d):\n    return d[\"name\"]\n",
        )];
        let mut diags = Diagnostics::new();
        let rt = StarlarkRuntime::compile(&funcs, &mut diags);

        let input = Value::Object(vec![(
            Cow::Borrowed("name"),
            Value::String(Cow::Borrowed("alice")),
        )]);
        let result = rt.call("get_name", &input, &mut diags);
        assert!(!diags.has_errors());
        assert_eq!(result, Some(Value::String(Cow::Owned("alice".to_string()))));
    }

    #[test]
    fn test_call_returns_dict() {
        let funcs = vec![make_func(
            "make_tags",
            "def make_tags(name):\n    return {\"Name\": name, \"Managed\": \"pulumi\"}\n",
        )];
        let mut diags = Diagnostics::new();
        let rt = StarlarkRuntime::compile(&funcs, &mut diags);

        let input = Value::String(Cow::Borrowed("my-resource"));
        let result = rt.call("make_tags", &input, &mut diags);
        assert!(!diags.has_errors());
        match result {
            Some(Value::Object(entries)) => {
                assert_eq!(entries.len(), 2);
                assert_eq!(entries[0].0.as_ref(), "Name");
                assert_eq!(
                    entries[0].1,
                    Value::String(Cow::Owned("my-resource".to_string()))
                );
                assert_eq!(entries[1].0.as_ref(), "Managed");
                assert_eq!(
                    entries[1].1,
                    Value::String(Cow::Owned("pulumi".to_string()))
                );
            }
            other => panic!("expected Object, got {:?}", other),
        }
    }

    #[test]
    fn test_call_function_not_found() {
        let funcs = vec![make_func("upper", "def upper(s):\n    return s.upper()\n")];
        let mut diags = Diagnostics::new();
        let rt = StarlarkRuntime::compile(&funcs, &mut diags);

        let input = Value::String(Cow::Borrowed("hello"));
        let result = rt.call("nonexistent", &input, &mut diags);
        assert!(diags.has_errors());
        assert!(result.is_none());
    }

    #[test]
    fn test_call_runtime_error() {
        let funcs = vec![make_func("divide", "def divide(n):\n    return n / 0\n")];
        let mut diags = Diagnostics::new();
        let rt = StarlarkRuntime::compile(&funcs, &mut diags);
        assert!(!diags.has_errors());

        let input = Value::Number(42.0);
        let result = rt.call("divide", &input, &mut diags);
        assert!(diags.has_errors());
        assert!(result.is_none());
    }

    #[test]
    fn test_value_roundtrip_null() {
        let funcs = vec![make_func("identity", "def identity(x):\n    return x\n")];
        let mut diags = Diagnostics::new();
        let rt = StarlarkRuntime::compile(&funcs, &mut diags);

        let result = rt.call("identity", &Value::Null, &mut diags);
        assert_eq!(result, Some(Value::Null));
    }

    #[test]
    fn test_value_roundtrip_bool() {
        let funcs = vec![make_func("identity", "def identity(x):\n    return x\n")];
        let mut diags = Diagnostics::new();
        let rt = StarlarkRuntime::compile(&funcs, &mut diags);

        let result = rt.call("identity", &Value::Bool(true), &mut diags);
        assert_eq!(result, Some(Value::Bool(true)));
    }

    #[test]
    fn test_value_roundtrip_number() {
        let funcs = vec![make_func("identity", "def identity(x):\n    return x\n")];
        let mut diags = Diagnostics::new();
        let rt = StarlarkRuntime::compile(&funcs, &mut diags);

        let result = rt.call("identity", &Value::Number(42.0), &mut diags);
        assert_eq!(result, Some(Value::Number(42.0)));
    }

    #[test]
    fn test_value_roundtrip_string() {
        let funcs = vec![make_func("identity", "def identity(x):\n    return x\n")];
        let mut diags = Diagnostics::new();
        let rt = StarlarkRuntime::compile(&funcs, &mut diags);

        let result = rt.call(
            "identity",
            &Value::String(Cow::Borrowed("hello")),
            &mut diags,
        );
        assert_eq!(result, Some(Value::String(Cow::Owned("hello".to_string()))));
    }

    #[test]
    fn test_value_roundtrip_nested() {
        let funcs = vec![make_func("identity", "def identity(x):\n    return x\n")];
        let mut diags = Diagnostics::new();
        let rt = StarlarkRuntime::compile(&funcs, &mut diags);

        let input = Value::List(vec![
            Value::Number(1.0),
            Value::String(Cow::Borrowed("two")),
            Value::Bool(true),
        ]);
        let result = rt.call("identity", &input, &mut diags);
        assert!(!diags.has_errors());
        match result {
            Some(Value::List(items)) => {
                assert_eq!(items.len(), 3);
                assert_eq!(items[0], Value::Number(1.0));
            }
            other => panic!("expected List, got {:?}", other),
        }
    }
}

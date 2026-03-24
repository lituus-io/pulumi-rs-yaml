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

use std::collections::HashMap;

use starlark::environment::{FrozenModule, Globals, Module};
use starlark::eval::Evaluator as StarlarkEvaluator;
use starlark::syntax::{AstModule, Dialect};
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
        let mut modules = HashMap::with_capacity(functions.len());
        let globals = Globals::standard();

        for func in functions {
            // Guard against duplicate function names (belt-and-suspenders;
            // parser also checks, but compile() may be called directly).
            if modules.contains_key(func.name.as_ref()) {
                diags.error(
                    None,
                    format!("duplicate starlark function '{}'", func.name),
                    format!(
                        "Function '{}' is already defined. Each starlark function \
                         name must be unique.",
                        func.name
                    ),
                );
                continue;
            }

            let ast = match AstModule::parse(
                &format!("{}.star", func.name),
                func.script.to_string(),
                &Dialect::Standard,
            ) {
                Ok(ast) => ast,
                Err(e) => {
                    diags.error(
                        None,
                        format!("starlark syntax error in function '{}'", func.name),
                        format!(
                            "{}\n\nCheck the 'script' field for syntax issues. \
                             Starlark uses Python-like syntax:\n  \
                             def {}(arg):\n      return arg.upper()",
                            e, func.name
                        ),
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
                        format!("starlark compilation failed for function '{}'", func.name),
                        format!(
                            "{}\n\nThe script must define a function named '{}'. \
                             Ensure all referenced variables and builtins exist.",
                            e, func.name
                        ),
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
                        format!(
                            "starlark internal error: failed to freeze module for '{}'",
                            func.name
                        ),
                        format!(
                            "{:?}\n\nThis is an internal error. \
                             Please report it with your Starlark script.",
                            e
                        ),
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
                let available: Vec<&str> = self.modules.keys().map(|s| s.as_str()).collect();
                let suggestion = suggest_function_name(function_name, &available);
                diags.error(
                    None,
                    format!("starlark function '{}' is not defined", function_name),
                    format!(
                        "No function named '{}' was found in the starlark: block. {}",
                        function_name, suggestion
                    ),
                );
                return None;
            }
        };

        // Look up the function from the frozen module
        let owned_func = match frozen.get(function_name) {
            Ok(v) => v,
            Err(_) => {
                diags.error(
                    None,
                    format!(
                        "starlark script for '{}' does not export a function named '{}'",
                        function_name, function_name
                    ),
                    format!(
                        "The script must define a function with a matching name:\n  \
                         script: |\n    def {}(input):\n        return ...",
                        function_name
                    ),
                );
                return None;
            }
        };

        // Warn about non-bridgeable types in the input
        warn_non_bridgeable(input, function_name, diags);

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
                            "starlark function '{}' returned a value that cannot be used in YAML",
                            function_name
                        ),
                        format!(
                            "{}\n\nSupported return types: string, int, float, bool, \
                             None, list, dict (with string keys)",
                            e
                        ),
                    );
                    None
                }
            },
            Err(e) => {
                diags.error(
                    None,
                    format!(
                        "starlark function '{}' failed during execution",
                        function_name
                    ),
                    e.to_string(),
                );
                None
            }
        }
    }
}

/// Computes the Levenshtein edit distance between two strings.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (m, n) = (a.len(), b.len());
    let mut dp = vec![vec![0usize; n + 1]; m + 1];
    for (i, row) in dp.iter_mut().enumerate().take(m + 1) {
        row[0] = i;
    }
    for j in 0..=n {
        dp[0][j] = j;
    }
    for i in 1..=m {
        for j in 1..=n {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            dp[i][j] = (dp[i - 1][j] + 1)
                .min(dp[i][j - 1] + 1)
                .min(dp[i - 1][j - 1] + cost);
        }
    }
    dp[m][n]
}

/// Suggests the closest matching function name from available functions.
fn suggest_function_name(name: &str, available: &[&str]) -> String {
    if available.is_empty() {
        return "No starlark functions are defined.".to_string();
    }
    let mut best_name = "";
    let mut best_dist = usize::MAX;
    for &candidate in available {
        let dist = edit_distance(name, candidate);
        if dist < best_dist {
            best_dist = dist;
            best_name = candidate;
        }
    }
    let available_list = available.join(", ");
    if best_dist <= 3 {
        format!(
            "Did you mean '{}'? Available functions: {}",
            best_name, available_list
        )
    } else {
        format!("Available functions: {}", available_list)
    }
}

/// Emits warnings for non-bridgeable value types passed to Starlark.
fn warn_non_bridgeable(val: &Value<'_>, function_name: &str, diags: &mut Diagnostics) {
    match val {
        Value::Resource(_) => {
            diags.warning(
                None,
                format!(
                    "fn::starlark '{}': input contains a Resource reference",
                    function_name
                ),
                "Resource references cannot be passed to Starlark functions and will be \
                 converted to None. Extract the specific property you need: ${resource.propertyName}",
            );
        }
        Value::Asset(_) => {
            diags.warning(
                None,
                format!(
                    "fn::starlark '{}': input contains an Asset",
                    function_name
                ),
                "Asset values cannot be passed to Starlark functions and will be converted to None.",
            );
        }
        Value::Archive(_) => {
            diags.warning(
                None,
                format!(
                    "fn::starlark '{}': input contains an Archive",
                    function_name
                ),
                "Archive values cannot be passed to Starlark functions and will be converted to None.",
            );
        }
        Value::List(items) => {
            for item in items {
                warn_non_bridgeable(item, function_name, diags);
            }
        }
        Value::Object(entries) => {
            for (_, v) in entries {
                warn_non_bridgeable(v, function_name, diags);
            }
        }
        Value::Secret(inner) => {
            warn_non_bridgeable(inner, function_name, diags);
        }
        _ => {}
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
///
/// Uses Starlark's built-in JSON serialization as the type bridge. This
/// correctly handles all Starlark types that map to JSON: null, bool,
/// int (all sizes including BigInt), float, string, list, dict, and
/// tuple (→ JSON array). Non-serializable types (set, struct, function,
/// range) produce a clear error instead of silent data loss.
fn starlark_to_value(val: StarlarkValue<'_>) -> Result<Value<'static>, String> {
    match val.to_json_value() {
        Ok(json) => Ok(Value::from_json(&json)),
        Err(e) => {
            let type_name = val.get_type();
            Err(format!(
                "cannot convert Starlark '{}' value to a Pulumi YAML value: {}\n\n\
                 Supported return types: null, bool, int, float, string, list, \
                 dict (with string keys).\n\
                 Unsupported types (such as '{}') must be converted to a supported \
                 type inside your Starlark function before returning.",
                type_name, e, type_name
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::borrow::Cow;

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
                // JSON bridge uses sorted keys (BTreeMap), check by key lookup
                let has_name = entries.iter().any(|(k, v)| {
                    k.as_ref() == "Name"
                        && *v == Value::String(Cow::Owned("my-resource".to_string()))
                });
                let has_managed = entries.iter().any(|(k, v)| {
                    k.as_ref() == "Managed" && *v == Value::String(Cow::Owned("pulumi".to_string()))
                });
                assert!(has_name, "missing Name entry in {:?}", entries);
                assert!(has_managed, "missing Managed entry in {:?}", entries);
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

#![no_main]
// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

//! Fuzz target: the `starlark:` block.
//!
//! # Why arbitrary bytes are not a script here
//!
//! `StarlarkRuntime::compile` does not only parse. It calls
//! `eval_module`, which EXECUTES the module's top-level statements -- that is
//! how the `def` is bound. So a top-level expression in an author's script
//! runs when the program is loaded, whether or not `fn::starlark` is ever
//! invoked, and a target that hands arbitrary bytes to `compile` is asking an
//! arbitrary program to run.
//!
//! An arbitrary program may ask for an arbitrary allocation, and nothing in
//! this process can refuse it. starlark-rust 0.13 exposes no heap limit --
//! there is no `set_max_*` on the evaluator and `Heap::alloc` is infallible --
//! the allocation happens inside ONE expression, so no statement hook and no
//! deadline can intervene, and an allocator returning null only moves the
//! abort into `handle_alloc_error`. `'   '*333333333` is 15 characters and
//! takes 2.2 GB; `'   '*3332323*3332323` is 21 and asks for 33 TB.
//!
//! Two lexical filters were tried against that and both were wrong, which is
//! why none is here now. A bound on the size of a numeric literal is defeated
//! by multiplying two literals under it. Widening the bound is defeated
//! without any large literal at all, by `s = s + s` in a loop of 45. Source
//! text cannot be screened for what it will allocate.
//!
//! So the input is split by what can hold it safely:
//!
//! - **arbitrary bytes** reach the YAML template parser, which parses and
//!   evaluates nothing, and is this crate's own code;
//! - **arbitrary bytes as DATA** reach fixed scripts, which is the realistic
//!   threat model: in a `Pulumi.yaml` the script is the author's own text,
//!   while the `input:` carries a config value or a resource output;
//! - **arbitrary bytes as TEXT INSIDE a string literal** reach the parser
//!   through a skeleton that cannot express an operator, so hostile text
//!   still drives `compile` without hostile arithmetic.
//!
//! What is deliberately not asserted is that executing an arbitrary program
//! leaves the process alive. That was never true, and asserting it produced
//! three findings in two runs, none of them a defect.
//!
//! Security targets:
//! - Panics or stack overflow in the template parser on hostile text
//! - Panics compiling a script whose literals carry hostile text
//! - Panics crossing the value bridge in either direction, on hostile data
//! - Panics in the diagnostic paths for a script that does not compile
//! - Non-determinism: one script and one input must answer the same twice

use libfuzzer_sys::fuzz_target;
use std::borrow::Cow;
use std::collections::HashMap;

use pulumi_rs_yaml_core::ast::template::StarlarkFunctionDecl;
use pulumi_rs_yaml_core::diag::Diagnostics;
use pulumi_rs_yaml_core::eval::starlark_runtime::StarlarkRuntime;
use pulumi_rs_yaml_core::eval::value::Value;

/// Scripts that are executed. Each is bounded by construction -- no
/// multiplication, no loop whose trip count comes from the input -- so the
/// only thing the fuzzer varies is the data flowing through them.
///
/// Chosen to cover what the bridge has to carry: every return type, a read of
/// each input shape, an index that may be out of range, a type error raised
/// from inside Starlark, and a `fail()`.
const SCRIPTS: &[&str] = &[
    "def fuzz_func(x):\n    return x\n",
    "def fuzz_func(x):\n    return str(x)\n",
    "def fuzz_func(x):\n    return len(str(x))\n",
    "def fuzz_func(x):\n    return [x, x]\n",
    "def fuzz_func(x):\n    return {'k': x}\n",
    "def fuzz_func(x):\n    return x == None\n",
    "def fuzz_func(x):\n    return str(x).upper().strip()\n",
    "def fuzz_func(x):\n    return str(x)[0]\n",
    "def fuzz_func(x):\n    return str(x).split(',')\n",
    "def fuzz_func(x):\n    return x + 1\n",
    "def fuzz_func(x):\n    return x['missing']\n",
    "def fuzz_func(x):\n    fail('refused: ' + str(x))\n",
    "def fuzz_func(x):\n    return [c for c in str(x)][:8]\n",
    "def fuzz_func(x):\n    return {'n': len(str(x)), 'v': str(x)[:4]}\n",
];

/// Scripts that must NOT compile, driving the diagnostic paths that are
/// otherwise unreached once arbitrary bytes stop being handed to the
/// compiler: a syntax error, an indentation error, a module that raises while
/// its top level executes, a body naming something undefined -- caught at
/// compile time, because Starlark resolves names when the module is evaluated
/// -- and an undefined name at the top level.
const NON_COMPILING: &[&str] = &[
    "def fuzz_func(\n",
    "def fuzz_func(x):\nreturn x\n",
    "fail('at module level')\n",
    "def fuzz_func(x):\n    return undefined_name(x)\n",
    "undefined_name()\n",
];

/// Scripts that compile cleanly and are still not callable, which is a
/// distinction worth pinning rather than assuming. `compile` keys the frozen
/// module on the DECLARED name, not on what the script defines, so a script
/// defining `other` -- or defining nothing at all -- leaves
/// `has_function("fuzz_func")` answering true. The mismatch surfaces at call
/// time instead, as a reported error and no value.
const COMPILES_BUT_NOT_CALLABLE: &[&str] = &["def other(x):\n    return x\n", ""];

/// Escapes hostile bytes so they can sit inside a Starlark `"` literal
/// without closing it. Backslash and quote are escaped; a byte that would
/// break the line, or that is not printable ASCII, is dropped rather than
/// escaped, because the point is to vary the text the parser sees, not to
/// round-trip it.
fn as_string_literal_body(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            c if c == ' ' || (c.is_ascii_graphic()) => out.push(c),
            _ => {}
        }
    }
    out
}

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }

    // The first byte selects the script; the rest is the hostile input. Taking
    // the selector from the input rather than from a loop keeps one crash
    // reproducer tied to one script.
    let selector = usize::from(data[0]);
    let rest = &data[1..];
    let text = String::from_utf8_lossy(rest);

    // --- The template parser, on arbitrary bytes -------------------------
    //
    // This is this crate's own parser, and it evaluates nothing, so the whole
    // byte range is safe to hand it.
    if let Ok(source) = std::str::from_utf8(rest) {
        let _ = pulumi_rs_yaml_core::ast::parse::parse_template(source, None);
    }

    // --- A script that does not compile ----------------------------------
    //
    // Fixed text, so nothing executes that was not written here. `compile`
    // must report rather than panic, and must not claim the function.
    {
        let script = NON_COMPILING[selector % NON_COMPILING.len()];
        let mut diags = Diagnostics::new();
        let runtime = StarlarkRuntime::compile(
            &[StarlarkFunctionDecl {
                name: Cow::Borrowed("fuzz_func"),
                script: Cow::Borrowed(script),
            }],
            &mut diags,
        );
        assert!(
            diags.has_errors(),
            "a script in NON_COMPILING must be reported: {script:?}"
        );
        assert!(
            !runtime.has_function("fuzz_func"),
            "a script in NON_COMPILING must not yield a callable function: {script:?}"
        );
    }

    // --- A script that compiles and still is not callable ----------------
    {
        let script = COMPILES_BUT_NOT_CALLABLE[selector % COMPILES_BUT_NOT_CALLABLE.len()];
        let mut diags = Diagnostics::new();
        let runtime = StarlarkRuntime::compile(
            &[StarlarkFunctionDecl {
                name: Cow::Borrowed("fuzz_func"),
                script: Cow::Borrowed(script),
            }],
            &mut diags,
        );
        assert!(
            !diags.has_errors(),
            "a script in COMPILES_BUT_NOT_CALLABLE must compile cleanly: {script:?}"
        );
        assert!(
            runtime.has_function("fuzz_func"),
            "`compile` keys the module on the DECLARED name, so this must be \
             claimed even though the script does not define it: {script:?}"
        );
        let mut call_diags = Diagnostics::new();
        let answered = runtime.call(
            "fuzz_func",
            &Value::String(Cow::Borrowed("x")),
            &mut call_diags,
        );
        assert!(
            answered.is_none() && call_diags.has_errors(),
            "the mismatch must surface at call time, reported: {script:?}"
        );
    }

    // --- Hostile text, inside a string literal ---------------------------
    //
    // The skeleton cannot express an operator, so the parser sees arbitrary
    // text without the module being able to compute a size.
    {
        let script = format!(
            "def fuzz_func(x):\n    return \"{}\" + str(x)\n",
            as_string_literal_body(text.as_ref())
        );
        let mut diags = Diagnostics::new();
        let runtime = StarlarkRuntime::compile(
            &[StarlarkFunctionDecl {
                name: Cow::Borrowed("fuzz_func"),
                script: Cow::Owned(script),
            }],
            &mut diags,
        );
        if runtime.has_function("fuzz_func") {
            let mut call_diags = Diagnostics::new();
            let _ = runtime.call(
                "fuzz_func",
                &Value::String(Cow::Borrowed("x")),
                &mut call_diags,
            );
        }
    }

    // --- Hostile data, through a fixed script ----------------------------
    let script = SCRIPTS[selector % SCRIPTS.len()];
    let mut diags = Diagnostics::new();
    let runtime = StarlarkRuntime::compile(
        &[StarlarkFunctionDecl {
            name: Cow::Borrowed("fuzz_func"),
            script: Cow::Borrowed(script),
        }],
        &mut diags,
    );
    assert!(
        !diags.has_errors() && runtime.has_function("fuzz_func"),
        "a script in SCRIPTS must always compile: {script:?}"
    );

    // Every input shape the bridge accepts, carrying the fuzzer's bytes
    // wherever a string can go -- including as an object KEY, which has its
    // own conversion path.
    let inputs = [
        Value::Null,
        Value::Bool(rest.len().is_multiple_of(2)),
        Value::Number(f64::from(u32::from_le_bytes([
            rest.first().copied().unwrap_or(0),
            rest.get(1).copied().unwrap_or(0),
            rest.get(2).copied().unwrap_or(0),
            rest.get(3).copied().unwrap_or(0),
        ]))),
        Value::String(Cow::Borrowed(text.as_ref())),
        Value::List(vec![
            Value::String(Cow::Borrowed(text.as_ref())),
            Value::Null,
        ]),
        Value::Object(vec![
            (
                Cow::Borrowed("k"),
                Value::String(Cow::Borrowed(text.as_ref())),
            ),
            (Cow::Borrowed(text.as_ref()), Value::Number(1.0)),
        ]),
        Value::Unknown,
    ];

    for input in &inputs {
        let mut d1 = Diagnostics::new();
        let first = runtime.call("fuzz_func", input, &mut d1);
        let mut d2 = Diagnostics::new();
        let second = runtime.call("fuzz_func", input, &mut d2);
        assert_eq!(
            first, second,
            "one script and one input must answer the same twice"
        );
        assert_eq!(
            d1.has_errors(),
            d2.has_errors(),
            "a repeated call must agree on whether it failed"
        );
    }

    // --- The template seam, with the data in the YAML --------------------
    //
    // The script is fixed and indented into the block; the fuzzer's bytes are
    // the `input:`, carried through serde_yaml so the document stays well
    // formed whatever they contain.
    let Ok(quoted) = serde_yaml::to_string(text.as_ref()) else {
        return;
    };
    let indented = script
        .lines()
        .map(|l| format!("        {l}"))
        .collect::<Vec<_>>()
        .join("\n");
    let yaml = format!(
        "name: fuzz\nruntime: yaml\nstarlark:\n  functions:\n    fuzz_func:\n      script: |\n{indented}\nvariables:\n  result:\n    fn::starlark:\n      invoke: fuzz_func\n      input: {}\n",
        quoted.trim_end()
    );

    let (template, _parse_diags) = pulumi_rs_yaml_core::ast::parse::parse_template(&yaml, None);
    let eval = pulumi_rs_yaml_core::eval::evaluator::Evaluator::new(
        "fuzz".to_string(),
        "dev".to_string(),
        "/tmp".to_string(),
        false,
    );
    let raw_config = HashMap::new();
    eval.evaluate_template(&template, &raw_config, &[]);
});

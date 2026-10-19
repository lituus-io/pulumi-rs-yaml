// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

//! Fuzz target: every Python method, with arbitrary receivers and arguments.
//!
//! The input picks a receiver (a string, a list, a dict, a number, a date,
//! or a value from the context), a method -- every name in the method
//! tables plus every list/dict mutator and date method -- and arguments of
//! any type, positional and keyword. The receiver is built in the template,
//! so containers are the mutable kind.
//!
//! Asserts: no panic; limits respected (output never past the cap, fuel
//! never past the budget); a deterministic second render; and an error is
//! always one of the typed runtime kinds, never a syntax error (the template
//! shape is valid by construction).

#![no_main]
use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use pulumi_rs_yaml_core::jinja::{
    method_names, JinjaContext, JinjaLimits, JinjaPreprocessor, RenderErrorKind, UndefinedMode,
};
use std::collections::HashMap;
use std::fmt::Write;

const EXTRA_METHODS: &[&str] = &[
    "append",
    "extend",
    "insert",
    "pop",
    "remove",
    "clear",
    "sort",
    "reverse",
    "update",
    "setdefault",
    "popitem",
    "strftime",
    "isoformat",
    "date",
    "weekday",
    "isoweekday",
    "toordinal",
    "replace",
    "total_seconds",
    "nosuchmethod",
];
const KWARGS: &[&str] = &[
    "sep", "maxsplit", "start", "end", "width", "fillchar", "count", "key", "reverse", "default",
    "days", "hour", "a", "keepends", "tabsize",
];

#[derive(Arbitrary, Debug)]
enum Arg {
    Int(i32),
    Big(i64),
    Float(f32),
    Str(String),
    Bool(bool),
    None,
    List(Vec<i8>),
    StrList(Vec<String>),
}

#[derive(Arbitrary, Debug)]
enum Receiver {
    Str(String),
    List(Vec<Arg>),
    Dict(Vec<(String, i16)>),
    Int(i64),
    Float(f64),
    Date(u16, u8, u8),
    Delta(i32),
    Context,
}

#[derive(Arbitrary, Debug)]
struct Input {
    receiver: Receiver,
    method: u16,
    args: Vec<Arg>,
    kwargs: Vec<(u8, Arg)>,
    twice: bool,
    context: String,
}

fn quote(s: &str, out: &mut String) {
    out.push('\'');
    for c in s.chars().take(64) {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\n' => out.push_str("\\n"),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out.push('\'');
}

fn arg(a: &Arg, out: &mut String) {
    match a {
        Arg::Int(n) => {
            let _ = write!(out, "{n}");
        }
        Arg::Big(n) => {
            let _ = write!(out, "{n}");
        }
        Arg::Float(f) if f.is_finite() => {
            let _ = write!(out, "{f:?}");
        }
        Arg::Float(_) => out.push_str("0.5"),
        Arg::Str(s) => quote(s, out),
        Arg::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Arg::None => out.push_str("none"),
        Arg::List(xs) => {
            out.push('[');
            for (i, x) in xs.iter().take(8).enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                let _ = write!(out, "{x}");
            }
            out.push(']');
        }
        Arg::StrList(xs) => {
            out.push('[');
            for (i, x) in xs.iter().take(8).enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                quote(x, out);
            }
            out.push(']');
        }
    }
}

const LIMITS: JinjaLimits = JinjaLimits {
    fuel: 20_000,
    recursion: 100,
    output_bytes: 64 * 1024,
    range: 1_000,
};

fuzz_target!(|input: Input| {
    let names: Vec<&str> = method_names()
        .map(|(_, n)| n)
        .chain(EXTRA_METHODS.iter().copied())
        .collect();
    let method = names[input.method as usize % names.len()];
    let mut src = String::from("{% set r = ");
    match &input.receiver {
        Receiver::Str(s) => quote(s, &mut src),
        Receiver::List(xs) => {
            src.push('[');
            for (i, x) in xs.iter().take(8).enumerate() {
                if i > 0 {
                    src.push_str(", ");
                }
                arg(x, &mut src);
            }
            src.push(']');
        }
        Receiver::Dict(kv) => {
            src.push('{');
            for (i, (k, v)) in kv.iter().take(8).enumerate() {
                if i > 0 {
                    src.push_str(", ");
                }
                quote(k, &mut src);
                let _ = write!(src, ": {v}");
            }
            src.push('}');
        }
        Receiver::Int(n) => {
            let _ = write!(src, "{n}");
        }
        Receiver::Float(f) if f.is_finite() => {
            let _ = write!(src, "{f:?}");
        }
        Receiver::Float(_) => src.push_str("1.5"),
        Receiver::Date(y, m, d) => {
            let _ = write!(src, "date({}, {}, {})", y % 10001, m % 14, d % 33);
        }
        Receiver::Delta(n) => {
            let _ = write!(src, "timedelta(seconds={n})");
        }
        Receiver::Context => src.push_str("ctx"),
    }
    src.push_str(" %}");
    // A mutator named anywhere makes the template's containers mutable.
    src.push_str("{% if false %}{% do [].append(0) %}{% endif %}");
    let mut call = format!("r.{method}(");
    let mut first = true;
    for a in input.args.iter().take(4) {
        if !first {
            call.push_str(", ");
        }
        first = false;
        arg(a, &mut call);
    }
    for (k, a) in input.kwargs.iter().take(3) {
        if !first {
            call.push_str(", ");
        }
        first = false;
        let _ = write!(call, "{}=", KWARGS[*k as usize % KWARGS.len()]);
        arg(a, &mut call);
    }
    call.push(')');
    let _ = write!(src, "{{{{ {call} }}}}");
    if input.twice {
        let _ = write!(src, "|{{{{ {call} }}}}|{{{{ r }}}}");
    }

    let extra: HashMap<String, String> = [("ctx".to_string(), input.context.clone())]
        .into_iter()
        .collect();
    let config = HashMap::new();
    let ctx = JinjaContext {
        project_name: "p",
        stack_name: "s",
        cwd: "/nonexistent-fuzz-root",
        organization: "",
        root_directory: "/nonexistent-fuzz-root",
        config: &config,
        project_dir: "/nonexistent-fuzz-root",
        undefined: UndefinedMode::Strict,
        provider_templated_packages: &[],
        extra: &extra,
        limits: LIMITS,
    };
    let pre = JinjaPreprocessor::new(&ctx);
    let a = pre.render_with_stats(&src, "m.j2");
    let b = pre.render_with_stats(&src, "m.j2");
    match (&a, &b) {
        (Ok((x, sx)), Ok((y, sy))) => {
            assert_eq!((x, sx), (y, sy), "non-deterministic: {src}");
            assert!(x.len() <= LIMITS.output_bytes && sx.fuel <= LIMITS.fuel);
        }
        (Err(x), Err(y)) => {
            assert_eq!((x.kind, &x.message), (y.kind, &y.message), "{src}");
            assert_ne!(x.kind, RenderErrorKind::JinjaSyntax, "{src}: {}", x.message);
        }
        _ => panic!("one render failed and the other did not: {src}"),
    }
});

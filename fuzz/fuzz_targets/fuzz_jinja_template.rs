// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

//! Fuzz target: structure-aware Jinja templates through the full renderer.
//!
//! The input is decoded into a template AST -- statements, expressions,
//! every registered filter, test, global and method -- and a context of
//! string variables, then printed as Jinja and rendered under tight limits.
//! Byte-level fuzzing (`fuzz_jinja`) rarely gets past the parser; this
//! reaches the evaluator.
//!
//! Asserts, for every input:
//! - no panic;
//! - the limits hold: output never exceeds the cap, fuel never exceeds the
//!   budget, and the run ends (libFuzzer's timeout is the hang detector);
//! - the render is deterministic: a second render gives the same text, or
//!   the same error at the same place;
//! - every error is located: a line in this file, an origin in another,
//!   or (for output that crossed the cap in literal text) the cap's own
//!   message.

#![no_main]
use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use pulumi_rs_yaml_core::jinja::{
    JinjaContext, JinjaLimits, JinjaPreprocessor, RenderErrorKind, UndefinedMode,
};
use std::collections::HashMap;
use std::fmt::Write;

const VARS: &[&str] = &["a", "b", "c", "d", "loop", "missing", "config", "ns"];
const FILTERS: &[&str] = &[
    "abs", "attr", "batch", "capitalize", "center", "count", "d", "default", "dictsort", "e",
    "escape", "filesizeformat", "first", "float", "format", "groupby", "indent", "int", "items",
    "join", "last", "length", "list", "lower", "map", "max", "min", "pprint", "reject",
    "rejectattr", "replace", "reverse", "round", "safe", "select", "selectattr", "slice", "sort",
    "string", "sum", "title", "tojson", "trim", "truncate", "unique", "upper", "urlencode",
    "wordcount", "wordwrap", "split", "lines", "chain", "zip", "bool", "to_json", "to_yaml",
    "base64_encode", "base64_decode",
];
const TESTS: &[&str] = &[
    "defined", "undefined", "none", "number", "string", "sequence", "mapping", "iterable", "odd",
    "even", "divisibleby", "eq", "lt", "in", "callable", "true", "false", "startingwith",
];
const METHODS: &[&str] = &[
    "upper", "lower", "strip", "split", "replace", "startswith", "join", "title", "format",
    "items", "keys", "values", "get", "count", "find", "zfill", "next", "reset",
];
const OPS: &[&str] = &[
    "+", "-", "*", "/", "//", "%", "**", "~", "==", "!=", "<", ">=", "and", "or", "in", "not in",
];

#[derive(Arbitrary, Debug)]
enum Lit {
    Int(i16),
    Float(i8),
    Str(String),
    Bool(bool),
    None,
}

#[derive(Arbitrary, Debug)]
enum Expr {
    Lit(Lit),
    Var(u8),
    Attr(Box<Expr>, u8),
    Index(Box<Expr>, i8),
    Slice(Box<Expr>, Option<i8>, Option<i8>, Option<i8>),
    Filter(Box<Expr>, u8, Vec<Lit>),
    Test(Box<Expr>, u8, bool),
    Method(Box<Expr>, u8, Vec<Lit>),
    Bin(Box<Expr>, u8, Box<Expr>),
    Not(Box<Expr>),
    Cond(Box<Expr>, Box<Expr>, Option<Box<Expr>>),
    List(Vec<Expr>),
    Dict(Vec<(String, Expr)>),
    Range(i32, Option<i32>, Option<i8>),
    Cycler(Vec<Lit>),
    Joiner,
    ReadFile(String),
}

#[derive(Arbitrary, Debug)]
enum Node {
    Text(String),
    Emit(Expr, bool, bool),
    If(Expr, Vec<Node>, Vec<Node>),
    For(u8, Expr, Option<Expr>, Vec<Node>, Vec<Node>),
    Set(u8, Expr),
    SetBlock(u8, Vec<Node>),
    NsSet(Expr),
    Macro(Vec<Node>, Expr),
    Call(Vec<Node>),
    FilterBlock(u8, Vec<Node>),
    With(u8, Expr, Vec<Node>),
    Raw(String),
    Comment(String),
    Break,
    Continue,
    Do(Expr),
}

#[derive(Arbitrary, Debug)]
struct Input {
    nodes: Vec<Node>,
    context: Vec<(u8, String)>,
}

const MAX_DEPTH: usize = 6;

fn pick<'a>(list: &[&'a str], i: u8) -> &'a str {
    list[i as usize % list.len()]
}

/// A Jinja string literal for arbitrary text.
fn quote(s: &str, out: &mut String) {
    out.push('\'');
    for c in s.chars().take(32) {
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

fn lit(l: &Lit, out: &mut String) {
    match l {
        Lit::Int(n) => {
            let _ = write!(out, "{n}");
        }
        Lit::Float(n) => {
            let _ = write!(out, "{}.5", n);
        }
        Lit::Str(s) => quote(s, out),
        Lit::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Lit::None => out.push_str("none"),
    }
}

fn args(list: &[Lit], out: &mut String) {
    out.push('(');
    for (i, l) in list.iter().take(3).enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        lit(l, out);
    }
    out.push(')');
}

fn expr(e: &Expr, depth: usize, out: &mut String) {
    if depth > MAX_DEPTH {
        out.push('1');
        return;
    }
    let d = depth + 1;
    match e {
        Expr::Lit(l) => lit(l, out),
        Expr::Var(v) => out.push_str(pick(VARS, *v)),
        Expr::Attr(e, k) => {
            out.push('(');
            expr(e, d, out);
            out.push_str(").");
            out.push_str(pick(VARS, *k));
        }
        Expr::Index(e, i) => {
            out.push('(');
            expr(e, d, out);
            let _ = write!(out, ")[{i}]");
        }
        Expr::Slice(e, a, b, c) => {
            out.push('(');
            expr(e, d, out);
            out.push_str(")[");
            for (i, x) in [a, b, c].iter().enumerate() {
                if i > 0 {
                    out.push(':');
                }
                if let Some(x) = x {
                    // A zero step is an error worth reaching, but rarely.
                    let _ = write!(out, "{x}");
                }
            }
            out.push(']');
        }
        Expr::Filter(e, f, a) => {
            out.push('(');
            expr(e, d, out);
            out.push_str(")|");
            out.push_str(pick(FILTERS, *f));
            if !a.is_empty() {
                args(a, out);
            }
        }
        Expr::Test(e, t, neg) => {
            out.push('(');
            expr(e, d, out);
            out.push_str(if *neg { ") is not " } else { ") is " });
            out.push_str(pick(TESTS, *t));
        }
        Expr::Method(e, m, a) => {
            out.push('(');
            expr(e, d, out);
            out.push_str(").");
            out.push_str(pick(METHODS, *m));
            args(a, out);
        }
        Expr::Bin(l, op, r) => {
            out.push('(');
            expr(l, d, out);
            out.push(' ');
            out.push_str(pick(OPS, *op));
            out.push(' ');
            expr(r, d, out);
            out.push(')');
        }
        Expr::Not(e) => {
            out.push_str("not (");
            expr(e, d, out);
            out.push(')');
        }
        Expr::Cond(a, b, c) => {
            out.push('(');
            expr(a, d, out);
            out.push_str(" if ");
            expr(b, d, out);
            if let Some(c) = c {
                out.push_str(" else ");
                expr(c, d, out);
            }
            out.push(')');
        }
        Expr::List(items) => {
            out.push('[');
            for (i, x) in items.iter().take(4).enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                expr(x, d, out);
            }
            out.push(']');
        }
        Expr::Dict(items) => {
            out.push('{');
            for (i, (k, v)) in items.iter().take(4).enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                quote(k, out);
                out.push_str(": ");
                expr(v, d, out);
            }
            out.push('}');
        }
        Expr::Range(a, b, c) => {
            let _ = write!(out, "range({a}");
            if let Some(b) = b {
                let _ = write!(out, ", {b}");
                if let Some(c) = c {
                    let _ = write!(out, ", {c}");
                }
            }
            out.push(')');
        }
        Expr::Cycler(items) => {
            out.push_str("cycler");
            args(items, out);
        }
        Expr::Joiner => out.push_str("joiner()"),
        Expr::ReadFile(name) => {
            out.push_str("readFile(");
            quote(name, out);
            out.push(')');
        }
    }
}

fn nodes(list: &[Node], depth: usize, out: &mut String) {
    for n in list.iter().take(8) {
        node(n, depth, out);
    }
}

fn node(n: &Node, depth: usize, out: &mut String) {
    if depth > MAX_DEPTH {
        return;
    }
    let d = depth + 1;
    match n {
        Node::Text(t) => out.extend(
            t.chars()
                .take(40)
                .filter(|c| !matches!(c, '{' | '}' | '%' | '#')),
        ),
        Node::Emit(e, l, r) => {
            out.push_str(if *l { "{{- " } else { "{{ " });
            expr(e, d, out);
            out.push_str(if *r { " -}}" } else { " }}" });
        }
        Node::If(c, a, b) => {
            out.push_str("{% if ");
            expr(c, d, out);
            out.push_str(" %}");
            nodes(a, d, out);
            if !b.is_empty() {
                out.push_str("{% else %}");
                nodes(b, d, out);
            }
            out.push_str("{% endif %}");
        }
        Node::For(v, it, filter, body, empty) => {
            let _ = write!(out, "{{% for {} in ", pick(&["a", "b", "x"], *v));
            expr(it, d, out);
            if let Some(f) = filter {
                out.push_str(" if ");
                expr(f, d, out);
            }
            out.push_str(" %}");
            nodes(body, d, out);
            if !empty.is_empty() {
                out.push_str("{% else %}");
                nodes(empty, d, out);
            }
            out.push_str("{% endfor %}");
        }
        Node::Set(v, e) => {
            let _ = write!(out, "{{% set {} = ", pick(VARS, *v));
            expr(e, d, out);
            out.push_str(" %}");
        }
        Node::SetBlock(v, body) => {
            let _ = write!(out, "{{% set {} %}}", pick(&["a", "b", "x"], *v));
            nodes(body, d, out);
            out.push_str("{% endset %}");
        }
        Node::NsSet(e) => {
            out.push_str("{% set ns = namespace(v=0) %}{% set ns.v = ");
            expr(e, d, out);
            out.push_str(" %}");
        }
        Node::Macro(body, arg) => {
            out.push_str("{% macro m(x, y=1) %}");
            nodes(body, d, out);
            out.push_str("{% endmacro %}{{ m(");
            expr(arg, d, out);
            out.push_str(") }}");
        }
        Node::Call(body) => {
            out.push_str("{% macro w() %}[{{ caller() }}]{% endmacro %}{% call w() %}");
            nodes(body, d, out);
            out.push_str("{% endcall %}");
        }
        Node::FilterBlock(f, body) => {
            let _ = write!(out, "{{% filter {} %}}", pick(&["upper", "trim", "lower", "title"], *f));
            nodes(body, d, out);
            out.push_str("{% endfilter %}");
        }
        Node::With(v, e, body) => {
            let _ = write!(out, "{{% with {} = ", pick(&["a", "b", "x"], *v));
            expr(e, d, out);
            out.push_str(" %}");
            nodes(body, d, out);
            out.push_str("{% endwith %}");
        }
        Node::Raw(t) => {
            out.push_str("{% raw %}");
            out.extend(t.chars().take(40).filter(|c| *c != '{'));
            out.push_str("{% endraw %}");
        }
        Node::Comment(t) => {
            out.push_str("{# ");
            out.extend(t.chars().take(20).filter(|c| *c != '#'));
            out.push_str(" #}");
        }
        Node::Break => out.push_str("{% break %}"),
        Node::Continue => out.push_str("{% continue %}"),
        Node::Do(e) => {
            out.push_str("{% do ");
            expr(e, d, out);
            out.push_str(" %}");
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
    let mut source = String::new();
    nodes(&input.nodes, 0, &mut source);
    if source.len() > 16 * 1024 {
        return;
    }
    let extra: HashMap<String, String> = input
        .context
        .iter()
        .take(4)
        .map(|(k, v)| (pick(&["a", "b", "c", "d"], *k).to_string(), v.clone()))
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
    let first = pre.render_with_stats(&source, "fuzz.yaml");
    let second = pre.render_with_stats(&source, "fuzz.yaml");
    match (&first, &second) {
        (Ok((a, sa)), Ok((b, sb))) => {
            assert_eq!(a, b, "non-deterministic render of {source:?}");
            assert_eq!(sa, sb, "non-deterministic cost of {source:?}");
            assert!(a.len() <= LIMITS.output_bytes, "output past the cap");
            assert!(sa.fuel <= LIMITS.fuel, "fuel past the budget");
        }
        (Err(a), Err(b)) => {
            assert_eq!(
                (a.kind, a.line, a.column, &a.message),
                (b.kind, b.line, b.column, &b.message),
                "non-deterministic error for {source:?}"
            );
            let located = a.line > 0
                || a.origin.is_some()
                || matches!(a.kind, RenderErrorKind::OutputTooLarge);
            assert!(located, "unlocated {:?} error for {source:?}: {}", a.kind, a.message);
            let _ = a.format_rich("fuzz.yaml");
        }
        _ => panic!("one render failed and the other did not: {source:?}"),
    }
});

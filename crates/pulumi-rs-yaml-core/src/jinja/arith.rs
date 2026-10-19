// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

//! Where minijinja's operators and statements differ from Python Jinja2,
//! the prepass routes them here (only in templates that use them):
//!
//! - `/`, `//` and `%` by zero raise, as Python's `ZeroDivisionError` does
//!   (minijinja rendered `1 / 0` as `inf`), and `%` takes the divisor's
//!   sign (`7 % -3` is `-2`, not `1`);
//! - `'%s-%05.2f' % (a, b)` is Python's printf formatting, through the core
//!   `str.format` uses;
//! - `for c in 'abc'` iterates a list of the characters, so `loop.length`
//!   and `loop.revindex` are defined (minijinja iterates a string lazily);
//! - `{% include x without context %}` renders `x` with the globals only;
//! - a macro whose body names `varargs` or `kwargs` takes extra positional
//!   and keyword arguments into them.

use std::sync::Arc;

use minijinja::value::{Kwargs, Object, ObjectRepr, Value, ValueKind};
use minijinja::{Error, ErrorKind, State};

use super::methods::{printf, value_error};

pub(super) const DIV_FN: &str = "__rsyaml_div";
pub(super) const FLOORDIV_FN: &str = "__rsyaml_floordiv";
pub(super) const MOD_FN: &str = "__rsyaml_mod";
pub(super) const ITER_FN: &str = "__rsyaml_iter";
pub(super) const INCLUDE_FN: &str = "__rsyaml_include";
pub(super) const VARIADIC_FN: &str = "__rsyaml_variadic";
/// The private context key an isolated include carries its depth in.
const INCLUDE_DEPTH: &str = "__rsyaml_include_depth";
/// How deep isolated includes may nest: an include costs 10 of the
/// recursion budget, so this matches what that budget allows.
const MAX_ISOLATED_DEPTH: i64 = 50;

/// The engine's own `a <op> b`, evaluated by it.
pub(super) fn engine_op(
    state: &State<'_, '_>,
    a: Value,
    b: Value,
    op: &str,
) -> Result<Value, Error> {
    let src = match op {
        "+" => "a + b",
        "-" => "a - b",
        "/" => "a / b",
        "//" => "a // b",
        _ => "a % b",
    };
    state
        .env()
        .compile_expression(src)?
        .eval(minijinja::context! { a => a, b => b })
}

/// A number as Python sees it: an exact integer or a float.
enum Num {
    Int(i128),
    Float(f64),
}

fn num(v: &Value) -> Option<Num> {
    match v.kind() {
        ValueKind::Bool => Some(Num::Int(i128::from(v.is_true()))),
        ValueKind::Number if minijinja::tests::is_integer(v) => {
            i128::try_from(v.clone()).ok().map(Num::Int)
        }
        ValueKind::Number => f64::try_from(v.clone()).ok().map(Num::Float),
        _ => None,
    }
}

fn as_f64(n: &Num) -> f64 {
    match n {
        Num::Int(i) => *i as f64,
        Num::Float(f) => *f,
    }
}

fn zero_division(what: &str) -> Error {
    Error::new(
        ErrorKind::InvalidOperation,
        format!("ZeroDivisionError: {what}"),
    )
}

fn is_zero(n: &Num) -> bool {
    match n {
        Num::Int(i) => *i == 0,
        Num::Float(f) => *f == 0.0,
    }
}

fn int_value(n: i128) -> Value {
    match i64::try_from(n) {
        Ok(i) => Value::from(i),
        Err(_) => Value::from(n),
    }
}

/// `__rsyaml_div(a, b)`: Python's true division.
pub(super) fn div(state: &State<'_, '_>, a: Value, b: Value) -> Result<Value, Error> {
    let (Some(x), Some(y)) = (num(&a), num(&b)) else {
        return engine_op(state, a, b, "/");
    };
    if is_zero(&y) {
        return Err(zero_division(match (&x, &y) {
            (Num::Int(_), Num::Int(_)) => "division by zero",
            _ => "float division by zero",
        }));
    }
    Ok(Value::from(as_f64(&x) / as_f64(&y)))
}

/// `__rsyaml_floordiv(a, b)`: Python's floor division.
pub(super) fn floordiv(state: &State<'_, '_>, a: Value, b: Value) -> Result<Value, Error> {
    let (Some(x), Some(y)) = (num(&a), num(&b)) else {
        return engine_op(state, a, b, "//");
    };
    if is_zero(&y) {
        return Err(zero_division(match (&x, &y) {
            (Num::Int(_), Num::Int(_)) => "integer division or modulo by zero",
            _ => "float floor division by zero",
        }));
    }
    Ok(match (x, y) {
        (Num::Int(p), Num::Int(q)) => {
            let d = p / q;
            int_value(if p % q != 0 && ((p < 0) != (q < 0)) {
                d - 1
            } else {
                d
            })
        }
        (x, y) => Value::from((as_f64(&x) / as_f64(&y)).floor()),
    })
}

/// `__rsyaml_mod(a, b)`: Python's modulo (the divisor's sign), or printf
/// formatting when `a` is a string.
pub(super) fn modulo(state: &State<'_, '_>, a: Value, b: Value) -> Result<Value, Error> {
    if let Some(fmt) = a.as_str() {
        let cap = super::methods::output_cap(state);
        return Ok(Value::from(printf(fmt, &b, cap)?));
    }
    let (Some(x), Some(y)) = (num(&a), num(&b)) else {
        return engine_op(state, a, b, "%");
    };
    if is_zero(&y) {
        return Err(zero_division(match (&x, &y) {
            (Num::Int(_), Num::Int(_)) => "integer modulo by zero",
            _ => "float modulo",
        }));
    }
    Ok(match (x, y) {
        (Num::Int(p), Num::Int(q)) => {
            let r = p % q;
            int_value(if r != 0 && ((r < 0) != (q < 0)) {
                r + q
            } else {
                r
            })
        }
        (x, y) => {
            let (p, q) = (as_f64(&x), as_f64(&y));
            let r = p % q;
            let r = if r != 0.0 && ((r < 0.0) != (q < 0.0)) {
                r + q
            } else if r == 0.0 {
                0.0f64.copysign(q)
            } else {
                r
            };
            Value::from(r)
        }
    })
}

/// `__rsyaml_iter(x)`: a string as the list of its characters, so a loop
/// over it knows its length; anything else unchanged.
pub(super) fn iter(v: Value) -> Value {
    match v.as_str() {
        Some(s) => Value::from(
            s.chars()
                .map(|c| Value::from(c.to_string()))
                .collect::<Vec<_>>(),
        ),
        None => v,
    }
}

/// `__rsyaml_include(name, ignore_missing)`: `{% include name without
/// context %}` -- the template rendered with the environment's globals and
/// nothing of the caller's context. `name` may be a list, as for include.
pub(super) fn include(
    state: &State<'_, '_>,
    name: Value,
    ignore_missing: bool,
) -> Result<Value, Error> {
    let depth = state
        .lookup(INCLUDE_DEPTH)
        .and_then(|v| i64::try_from(v).ok())
        .unwrap_or(0);
    if depth >= MAX_ISOLATED_DEPTH {
        return Err(Error::new(
            ErrorKind::InvalidOperation,
            "recursion limit exceeded: isolated includes nest too deep",
        ));
    }
    let names: Vec<Value> = match name.kind() {
        ValueKind::Seq | ValueKind::Iterable => name.try_iter()?.collect(),
        _ => vec![name],
    };
    for n in &names {
        let n = n
            .as_str()
            .ok_or_else(|| value_error("include expects a template name"))?;
        match state.env().get_template(n) {
            Ok(tmpl) => {
                let out =
                    tmpl.render(minijinja::context! { __rsyaml_include_depth => depth + 1 })?;
                let cap = super::methods::output_cap(state);
                if out.len() > cap {
                    return Err(Error::new(
                        ErrorKind::InvalidOperation,
                        format!("{} of {cap} bytes", super::OUTPUT_TAG),
                    ));
                }
                return Ok(Value::from_safe_string(out));
            }
            Err(e) if e.kind() == ErrorKind::TemplateNotFound => continue,
            Err(e) => return Err(e),
        }
    }
    if ignore_missing {
        return Ok(Value::from(""));
    }
    let tried: Vec<String> = names.iter().map(|n| format!("{n:?}")).collect();
    Err(Error::new(
        ErrorKind::TemplateNotFound,
        format!(
            "tried to include non-existing template {}",
            tried.join(", ")
        ),
    ))
}

/// A macro that takes extra arguments into `varargs` and `kwargs`.
#[derive(Debug)]
struct Variadic {
    inner: Value,
    params: Vec<String>,
    wants_varargs: bool,
    wants_kwargs: bool,
}

impl Object for Variadic {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Plain
    }

    fn call(self: &Arc<Self>, state: &State<'_, '_>, args: &[Value]) -> Result<Value, Error> {
        let (positional, kwargs) = match args.split_last() {
            Some((last, rest)) if last.is_kwargs() => {
                let (kw,): (Kwargs,) = minijinja::value::from_args(std::slice::from_ref(last))?;
                (rest, Some(kw))
            }
            _ => (args, None),
        };
        let n = self.params.len().min(positional.len());
        let extra = &positional[n..];
        if !extra.is_empty() && !self.wants_varargs {
            return Err(Error::new(
                ErrorKind::TooManyArguments,
                "too many arguments",
            ));
        }
        let mut named: Vec<(String, Value)> = Vec::new();
        let mut rest: Vec<(Value, Value)> = Vec::new();
        if let Some(kw) = &kwargs {
            for key in kw.args().map(str::to_string).collect::<Vec<_>>() {
                let v: Value = kw.get(&key)?;
                if self.params.contains(&key) || key == "caller" {
                    named.push((key, v));
                } else if self.wants_kwargs {
                    rest.push((Value::from(key), v));
                } else {
                    return Err(Error::new(
                        ErrorKind::TooManyArguments,
                        format!("unknown keyword argument `{key}`"),
                    ));
                }
            }
        }
        named.push(("varargs".into(), Value::from(extra.to_vec())));
        let kwmap: std::collections::BTreeMap<Value, Value> = rest.into_iter().collect();
        named.push(("kwargs".into(), Value::from_iter(kwmap)));
        let mut call_args: Vec<Value> = positional[..n].to_vec();
        call_args.push(Value::from(Kwargs::from_iter(named)));
        self.inner.call(state, &call_args)
    }
}

/// `__rsyaml_variadic(macro, 'param', ...)`: the macro, taking extra
/// arguments into `varargs`/`kwargs` as Jinja2 does.
pub(super) fn variadic(
    inner: Value,
    uses: String,
    params: minijinja::value::Rest<String>,
) -> Value {
    Value::from_object(Variadic {
        inner,
        params: params.0,
        wants_varargs: uses.contains('v'),
        wants_kwargs: uses.contains('k'),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval(src: &str) -> Result<String, Error> {
        let mut env = minijinja::Environment::new();
        env.add_function(DIV_FN, div);
        env.add_function(FLOORDIV_FN, floordiv);
        env.add_function(MOD_FN, modulo);
        env.render_str(src, ())
    }

    #[test]
    fn python_division_and_modulo() {
        assert_eq!(
            eval("{{ __rsyaml_mod(7, -3) }} {{ __rsyaml_mod(-7, 3) }} {{ __rsyaml_mod(-7.5, 2) }}")
                .unwrap(),
            "-2 2 0.5"
        );
        assert_eq!(eval("{{ __rsyaml_floordiv(-7, 2) }} {{ __rsyaml_floordiv(7, -2) }} {{ __rsyaml_floordiv(7.5, 2) }}").unwrap(), "-4 -4 3.0");
        assert_eq!(
            eval("{{ __rsyaml_div(7, 2) }} {{ __rsyaml_div(4, 2) }}").unwrap(),
            "3.5 2.0"
        );
        for src in [
            "{{ __rsyaml_div(1, 0) }}",
            "{{ __rsyaml_floordiv(1, 0) }}",
            "{{ __rsyaml_mod(1, 0) }}",
            "{{ __rsyaml_div(1.5, 0.0) }}",
        ] {
            let e = eval(src).unwrap_err();
            assert!(e.to_string().contains("ZeroDivisionError"), "{src}: {e}");
        }
    }
}

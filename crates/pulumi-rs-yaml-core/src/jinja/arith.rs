// Author: spicyzhug@gmail.com
// Copyright: lituus-io, all rights reserved

//! Where minijinja's operators and statements differ from Python Jinja2,
//! the prepass routes them here (only in templates that use them):
//!
//! - `/`, `//` and `%` by zero raise, as Python's `ZeroDivisionError` does
//!   (minijinja rendered `1 / 0` as `inf`), and `%` takes the divisor's
//!   sign (`7 % -3` is `-2`, not `1`);
//! - `*` repeats a string or a list as Python does (a count of zero or less
//!   is empty) within the output limit -- minijinja's repeat had no bound,
//!   and `[] * 9223372036854775807` spun for ever -- and `**` takes a
//!   negative exponent to a float, raising for a zero base;
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
pub(super) const MUL_FN: &str = "__rsyaml_mul";
pub(super) const POW_FN: &str = "__rsyaml_pow";
pub(super) const CYCLE_FN: &str = "__rsyaml_cycle";
pub(super) const ITER_FN: &str = "__rsyaml_iter";
pub(super) const INCLUDE_FN: &str = "__rsyaml_include";
pub(super) const VARIADIC_FN: &str = "__rsyaml_variadic";
/// The private context key an isolated include carries its depth in.
const INCLUDE_DEPTH: &str = "__rsyaml_include_depth";
/// How deep isolated includes may nest: an include costs 10 of the
/// recursion budget, so this matches what that budget allows.
const MAX_ISOLATED_DEPTH: i64 = 50;

/// `a <op> b` where Python's numbers do not apply: the engine's own rules
/// for its operators, with its messages, so a routed operator answers as
/// the unrouted one did. Numbers use checked arithmetic (an overflow is
/// "unable to calculate"); `+` joins two strings or two lists; anything
/// else is "tried to use <op> operator on unsupported types". A joined list
/// is a list, where the engine kept a lazy sequence, so it can be indexed
/// as in Python.
pub(super) fn engine_op(a: Value, b: Value, op: &str) -> Result<Value, Error> {
    let failed = || {
        Error::new(
            ErrorKind::InvalidOperation,
            format!("unable to calculate {a} {op} {b}"),
        )
    };
    let unsupported = || {
        Error::new(
            ErrorKind::InvalidOperation,
            format!(
                "tried to use {op} operator on unsupported types {} and {}",
                a.kind(),
                b.kind()
            ),
        )
    };
    let seq = |v: &Value| matches!(v.kind(), ValueKind::Seq | ValueKind::Iterable);
    match op {
        "+" | "-" => match (num(&a), num(&b)) {
            (Some(Num::Int(x)), Some(Num::Int(y))) => if op == "+" {
                x.checked_add(y)
            } else {
                x.checked_sub(y)
            }
            .map(int_value)
            .ok_or_else(failed),
            (Some(x), Some(y)) => {
                let (x, y) = (as_f64(&x), as_f64(&y));
                Ok(Value::from(if op == "+" { x + y } else { x - y }))
            }
            _ if op == "+" && seq(&a) && seq(&b) => {
                let mut out: Vec<Value> = a.try_iter()?.collect();
                out.extend(b.try_iter()?);
                Ok(Value::from(out))
            }
            _ => match (op, a.as_str(), b.as_str()) {
                ("+", Some(x), Some(y)) => Ok(Value::from([x, y].concat())),
                _ => Err(unsupported()),
            },
        },
        "*" if a.kind() == ValueKind::String || b.kind() == ValueKind::String => Err(Error::new(
            ErrorKind::InvalidOperation,
            "strings can only be multiplied with integers",
        )),
        "*" if seq(&a) || seq(&b) => Err(Error::new(
            ErrorKind::InvalidOperation,
            "sequences and iterables can only be multiplied with integers",
        )),
        _ => Err(unsupported()),
    }
}

/// An undefined operand is an undefined variable, as it is to the engine's
/// own operators: the prepass's call must not turn it into a type error.
fn defined(a: &Value, b: &Value) -> Result<(), Error> {
    if a.is_undefined() || b.is_undefined() {
        return Err(Error::from(ErrorKind::UndefinedError));
    }
    Ok(())
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

#[cold]
#[inline(never)]
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
pub(super) fn div(_state: &State<'_, '_>, a: Value, b: Value) -> Result<Value, Error> {
    defined(&a, &b)?;
    let (Some(x), Some(y)) = (num(&a), num(&b)) else {
        return engine_op(a, b, "/");
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
pub(super) fn floordiv(_state: &State<'_, '_>, a: Value, b: Value) -> Result<Value, Error> {
    defined(&a, &b)?;
    let (Some(x), Some(y)) = (num(&a), num(&b)) else {
        return engine_op(a, b, "//");
    };
    if is_zero(&y) {
        return Err(zero_division(match (&x, &y) {
            (Num::Int(_), Num::Int(_)) => "integer division or modulo by zero",
            _ => "float floor division by zero",
        }));
    }
    Ok(match (x, y) {
        (Num::Int(p), Num::Int(q)) => match p.checked_div(q) {
            Some(d) => int_value(if p % q != 0 && ((p < 0) != (q < 0)) {
                d - 1
            } else {
                d
            }),
            // i128::MIN // -1 is 2**127: past i128, inside u128, exact.
            None => Value::from(p.unsigned_abs()),
        },
        (x, y) => Value::from((as_f64(&x) / as_f64(&y)).floor()),
    })
}

/// `__rsyaml_mod(a, b)`: Python's modulo (the divisor's sign), or printf
/// formatting when `a` is a string.
pub(super) fn modulo(state: &State<'_, '_>, a: Value, b: Value) -> Result<Value, Error> {
    defined(&a, &b)?;
    if let Some(fmt) = a.as_str() {
        let cap = super::methods::output_cap(state);
        return Ok(Value::from(printf(fmt, &b, cap)?));
    }
    let (Some(x), Some(y)) = (num(&a), num(&b)) else {
        return engine_op(a, b, "%");
    };
    if is_zero(&y) {
        return Err(zero_division(match (&x, &y) {
            (Num::Int(_), Num::Int(_)) => "integer modulo by zero",
            _ => "float modulo",
        }));
    }
    Ok(match (x, y) {
        (Num::Int(p), Num::Int(q)) => {
            // i128::MIN % -1 overflows the machine remainder; it is 0.
            let r = p.checked_rem(q).unwrap_or(0);
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

/// A whole-number count: an integer or a bool, as Python's `*` takes.
fn count(v: &Value) -> Option<i128> {
    match num(v)? {
        Num::Int(n) => Some(n),
        Num::Float(_) => None,
    }
}

/// Python's integers are unbounded; this engine's stop at 128 bits.
#[cold]
#[inline(never)]
fn overflow(op: &str) -> Error {
    Error::new(
        ErrorKind::InvalidOperation,
        format!("OverflowError: the result of {op} is past the 128-bit integers this engine holds"),
    )
}

#[cold]
#[inline(never)]
fn too_large(what: &str, bytes: u128, cap: usize) -> Error {
    Error::new(
        ErrorKind::InvalidOperation,
        format!(
            "{}: {what} would be {bytes} bytes, past the {cap}-byte limit",
            super::OUTPUT_TAG
        ),
    )
}

/// `__rsyaml_mul(a, b)`: Python's `*`. A string or a list times a count
/// repeats it -- nothing for a count of zero or less; anything else is the
/// engine's.
///
/// A repeat is bounded for memory, not for output: a string may be built
/// up to four times the output limit (it may be measured or sliced rather
/// than written, and the formatter, which knows the line, refuses one that
/// is written), and a list up to the range limit, as `range()` is.
pub(super) fn mul(state: &State<'_, '_>, a: Value, b: Value) -> Result<Value, Error> {
    defined(&a, &b)?;
    let (seq, n) = match (count(&a), count(&b)) {
        (_, Some(n)) if matches!(a.kind(), ValueKind::String | ValueKind::Seq) => (a, n),
        (Some(n), _) if matches!(b.kind(), ValueKind::String | ValueKind::Seq) => (b, n),
        _ => {
            return match (num(&a), num(&b)) {
                (Some(Num::Int(x)), Some(Num::Int(y))) => {
                    x.checked_mul(y).map(int_value).ok_or_else(|| overflow("*"))
                }
                (Some(x), Some(y)) => Ok(Value::from(as_f64(&x) * as_f64(&y))),
                _ => engine_op(a, b, "*"),
            }
        }
    };
    let times = usize::try_from(n.max(0)).unwrap_or(usize::MAX);
    if let Some(s) = seq.as_str() {
        let cap = super::methods::output_cap(state).saturating_mul(4);
        let bytes = s.len() as u128 * times as u128;
        if bytes > cap as u128 {
            return Err(too_large("the repeated string", bytes, cap));
        }
        return Ok(Value::from(s.repeat(times)));
    }
    let items: Vec<Value> = seq.try_iter()?.collect();
    if items.is_empty() || times == 0 {
        // Nothing to repeat: answered without counting to `times`.
        return Ok(Value::from(Vec::<Value>::new()));
    }
    let len = items.len() as u128 * times as u128;
    let limit = super::limits::range_cap(state);
    if len > limit as u128 {
        return Err(Error::new(
            ErrorKind::InvalidOperation,
            format!(
                "{} the repeated list would hold {len} elements, more than {limit}",
                super::limits::LIST_TAG
            ),
        ));
    }
    let mut out = Vec::with_capacity(len as usize);
    for _ in 0..times {
        out.extend(items.iter().cloned());
    }
    Ok(Value::from(out))
}

/// `__rsyaml_pow(a, b)`: Python's `**`. An integer to a negative integer
/// power is a float, and zero to a negative power raises; the rest is the
/// engine's.
pub(super) fn pow(_state: &State<'_, '_>, a: Value, b: Value) -> Result<Value, Error> {
    defined(&a, &b)?;
    let (Some(x), Some(y)) = (num(&a), num(&b)) else {
        return engine_op(a, b, "**");
    };
    let negative = match y {
        Num::Int(e) => e < 0,
        Num::Float(e) => e < 0.0,
    };
    if !negative {
        return match (x, y) {
            (Num::Int(base), Num::Int(e)) => u32::try_from(e)
                .ok()
                .and_then(|e| base.checked_pow(e))
                .map(int_value)
                .ok_or_else(|| overflow("**")),
            (x, y) => Ok(Value::from(as_f64(&x).powf(as_f64(&y)))),
        };
    }
    if is_zero(&x) {
        return Err(zero_division("0.0 cannot be raised to a negative power"));
    }
    Ok(Value::from(as_f64(&x).powf(as_f64(&y))))
}

/// A required argument and an optional one, positionally or by name.
fn two(
    args: &[Value],
    kwargs: &Kwargs,
    first: &str,
    second: &str,
) -> Result<(Value, Option<Value>), Error> {
    if args.len() > 2 {
        return Err(Error::from(ErrorKind::TooManyArguments));
    }
    let a = match args.first() {
        Some(v) => v.clone(),
        None => kwargs.get::<Value>(first)?,
    };
    let b = match args.get(1) {
        Some(v) => Some(v.clone()),
        None => kwargs.get::<Option<Value>>(second)?,
    };
    kwargs.assert_all_used()?;
    Ok((a, b.filter(|v| !v.is_none())))
}

/// Jinja2's `slice(value, slices, fill_with=None)`: `slices` lists, the
/// first `len % slices` one longer, a zero count a ZeroDivisionError and a
/// negative one no lists. The count is bounded by the range limit -- each
/// slice is a list, however short -- where minijinja reserved it unchecked
/// and a huge count was an allocation panic.
pub(super) fn slice(
    state: &State<'_, '_>,
    value: Value,
    args: &[Value],
    kwargs: Kwargs,
) -> Result<Value, Error> {
    let (slices, fill_with) = two(args, &kwargs, "slices", "fill_with")?;
    let n = count(&slices).ok_or_else(|| value_error("slice: the count must be an integer"))?;
    if n == 0 {
        return Err(zero_division("integer division or modulo by zero"));
    }
    let slices = usize::try_from(n).unwrap_or(0);
    let limit = super::limits::range_cap(state);
    if slices > limit {
        return Err(Error::new(
            ErrorKind::InvalidOperation,
            format!(
                "{} slice({slices}) makes {slices} lists, more than {limit}",
                super::limits::LIST_TAG
            ),
        ));
    }
    let items: Vec<Value> = value.try_iter()?.collect();
    let (per, extra) = (items.len() / slices.max(1), items.len() % slices.max(1));
    let mut out = Vec::with_capacity(slices);
    let mut offset = 0;
    for i in 0..slices {
        let start = offset + i * per;
        if i < extra {
            offset += 1;
        }
        let end = offset + (i + 1) * per;
        let mut part = items[start..end].to_vec();
        if let (Some(fill), true) = (&fill_with, i >= extra) {
            part.push(fill.clone());
        }
        out.push(Value::from(part));
    }
    Ok(Value::from(out))
}

/// Jinja2's `batch(value, linecount, fill_with=None)`: lists of `linecount`
/// items, the last padded with `fill_with`. Python's loop is reproduced as
/// it is: a count of zero yields an empty list first and a negative count
/// one list of everything. The padding is bounded by the output limit.
pub(super) fn batch(
    state: &State<'_, '_>,
    value: Value,
    args: &[Value],
    kwargs: Kwargs,
) -> Result<Value, Error> {
    let (linecount, fill_with) = two(args, &kwargs, "linecount", "fill_with")?;
    let n = count(&linecount).ok_or_else(|| value_error("batch: the count must be an integer"))?;
    let mut out = Vec::new();
    let mut tmp: Vec<Value> = Vec::new();
    for item in value.try_iter()? {
        if tmp.len() as i128 == n {
            out.push(Value::from(std::mem::take(&mut tmp)));
        }
        tmp.push(item);
    }
    if !tmp.is_empty() {
        if let Some(fill) = fill_with {
            let missing = u128::try_from(n - tmp.len() as i128).unwrap_or(0);
            let cap = super::methods::output_cap(state);
            if missing > cap as u128 {
                return Err(too_large("the padded batch", missing, cap));
            }
            tmp.extend(std::iter::repeat_n(fill, missing as usize));
        }
        out.push(Value::from(tmp));
    }
    Ok(Value::from(out))
}

/// `x.cycle()` with nothing to cycle, routed by the prepass. On a loop it is
/// Jinja2's `TypeError` -- minijinja computed `index % 0` and panicked; on
/// anything else it is the call as written.
pub(super) fn cycle(state: &State<'_, '_>, obj: Value, _: Value) -> Result<Value, Error> {
    let is_loop = ["index0", "revindex0", "depth0"]
        .iter()
        .all(|a| obj.get_attr(a).is_ok_and(|v| !v.is_undefined()));
    if is_loop {
        return Err(super::methods::type_error("no items for cycling given"));
    }
    obj.call_method(state, "cycle", &[])
}

/// Jinja2's `divisibleby` test: `value % num == 0`, so a zero divisor
/// (`False` included) is a ZeroDivisionError there too. minijinja's own
/// computed the remainder unchecked and panicked on it.
pub(super) fn divisibleby(state: &State<'_, '_>, value: Value, num: Value) -> Result<bool, Error> {
    Ok(modulo(state, value, num)? == Value::from(0))
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
        // Inserted, not collected: a map's FromIterator sorts its input
        // with a sort instantiated per iterator type.
        let mut kwmap = std::collections::BTreeMap::<Value, Value>::new();
        for (k, v) in rest {
            kwmap.insert(k, v);
        }
        named.push(("kwargs".into(), Value::from_object(kwmap)));
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

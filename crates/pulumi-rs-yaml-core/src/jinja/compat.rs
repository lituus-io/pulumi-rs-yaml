// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

//! Jinja2 semantics where minijinja ships a name differently or not at all.
//!
//! Every function here is pinned by a conformance case rendered with
//! reference Jinja2 (`tests/jinja_conformance/`): the behaviour is Jinja2's,
//! taken from its output rather than from its documentation, because the two
//! disagree often enough (banker's rounding in `round`, `int` falling back
//! through `float`, `max` comparing case-insensitively) to matter.
//!
//! Registration replaces the minijinja builtin of the same name where one
//! exists. Each replacement accepts everything the builtin accepted, with the
//! builtin's result, so a template that rendered before renders the same.

use std::cmp::Ordering as CmpOrdering;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use minijinja::value::{Kwargs, Object, ObjectRepr, Value, ValueKind};
use minijinja::{Environment, Error, ErrorKind, State};

/// `value | name(args, kwargs)`.
type Filter = fn(Value, &[Value], Kwargs) -> Result<Value, Error>;
/// The same, for a filter that reads the render's limits.
type StateFilter = fn(&State<'_, '_>, Value, &[Value], Kwargs) -> Result<Value, Error>;
/// One value in, one out: a filter, or a function of one argument.
type Unary = fn(Value) -> Result<Value, Error>;
/// `a <op> b`, routed by the prepass.
type Binary = fn(&State<'_, '_>, Value, Value) -> Result<Value, Error>;

// One table per shape, registered through one closure per shape: a closure
// written once is one type whatever function it holds, so the engine's
// argument conversion is instantiated once per shape rather than once per
// filter -- the registrations were the largest generic code in the binary.
const FILTERS: &[(&str, Filter)] = &[
    ("int", int_compat),
    ("float", float_compat),
    ("round", round_compat),
    ("replace", replace_compat),
    ("join", join_compat),
    ("sum", sum_compat),
    ("max", max_compat),
    ("min", min_compat),
    ("dictsort", dictsort_compat),
    ("filesizeformat", filesizeformat),
    // Three Jinja2 string filters minijinja does not carry.
    ("truncate", super::truncate_compat),
    ("center", super::center_compat),
    ("wordwrap", super::wordwrap_compat),
];

const STATE_FILTERS: &[(&str, StateFilter)] = &[
    // Jinja2's `format` is `value % args`: the printf core `%` uses.
    // minijinja's own sliced its input by bytes and panicked on a
    // multi-byte character inside a `%(...)` key.
    ("format", format_filter),
    ("tojson", super::tojson_compat),
    ("indent", super::indent_compat),
    ("slice", super::arith::slice),
    ("batch", super::arith::batch),
];

const UNARY_FILTERS: &[(&str, Unary)] = &[
    ("title", title_compat),
    ("wordcount", wordcount),
    ("urlencode", urlencode),
    ("to_date", super::dates::to_date),
    ("to_datetime", super::dates::to_datetime),
];

const UNARY_FUNCTIONS: &[(&str, Unary)] = &[
    // The converters are callable as functions as well as filters.
    ("to_date", super::dates::to_date),
    ("to_datetime", super::dates::to_datetime),
    (super::containers::LIST_FN, super::containers::wrap_list),
    (super::containers::DICT_FN, super::containers::wrap_dict),
    (super::containers::OWN_FN, super::containers::own),
];

const OPERATORS: &[(&str, Binary)] = &[
    // Date `+`/`-`, and Python's `/`, `//`, `%`, `*` and `**`.
    (super::prepass::ADD_FN, super::dates::add),
    (super::prepass::SUB_FN, super::dates::sub),
    (super::arith::DIV_FN, super::arith::div),
    (super::arith::FLOORDIV_FN, super::arith::floordiv),
    (super::arith::MOD_FN, super::arith::modulo),
    (super::arith::MUL_FN, super::arith::mul),
    (super::arith::POW_FN, super::arith::pow),
];

/// The engine's own filters, tests and globals that Jinja2 has and this
/// module does not replace: minijinja's default set, less `debug` and less
/// every name `register` gives its own implementation. Registering the
/// replaced ones too would keep their code in the binary for nothing.
pub(super) fn builtins(env: &mut Environment<'_>) {
    use minijinja::{filters as f, functions as g, tests as t};
    env.add_filter("safe", f::safe);
    env.add_filter("escape", f::escape);
    env.add_filter("e", f::escape);
    env.add_filter("lower", f::lower);
    env.add_filter("upper", f::upper);
    env.add_filter("capitalize", f::capitalize);
    env.add_filter("length", f::length);
    env.add_filter("count", f::length);
    env.add_filter("items", f::items);
    env.add_filter("reverse", f::reverse);
    env.add_filter("trim", f::trim);
    env.add_filter("split", f::split);
    env.add_filter("lines", f::lines);
    env.add_filter("default", f::default);
    env.add_filter("d", f::default);
    env.add_filter("abs", f::abs);
    env.add_filter("attr", f::attr);
    env.add_filter("first", f::first);
    env.add_filter("last", f::last);
    env.add_filter("sort", f::sort);
    env.add_filter("list", f::list);
    env.add_filter("string", f::string);
    env.add_filter("bool", f::bool);
    env.add_filter("select", f::select);
    env.add_filter("reject", f::reject);
    env.add_filter("selectattr", f::selectattr);
    env.add_filter("rejectattr", f::rejectattr);
    env.add_filter("map", f::map);
    env.add_filter("groupby", f::groupby);
    env.add_filter("unique", f::unique);
    env.add_filter("chain", f::chain);
    env.add_filter("zip", f::zip);
    env.add_filter("pprint", f::pprint);

    env.add_test("undefined", t::is_undefined);
    env.add_test("defined", t::is_defined);
    env.add_test("none", t::is_none);
    env.add_test("safe", t::is_safe);
    env.add_test("escaped", t::is_safe);
    env.add_test("boolean", t::is_boolean);
    env.add_test("odd", t::is_odd);
    env.add_test("even", t::is_even);
    env.add_test("number", t::is_number);
    env.add_test("integer", t::is_integer);
    env.add_test("int", t::is_integer);
    env.add_test("float", t::is_float);
    env.add_test("string", t::is_string);
    env.add_test("iterable", t::is_iterable);
    env.add_test("mapping", t::is_mapping);
    env.add_test("startingwith", t::is_startingwith);
    env.add_test("endingwith", t::is_endingwith);
    env.add_test("lower", t::is_lower);
    env.add_test("upper", t::is_upper);
    env.add_test("sameas", t::is_sameas);
    for name in ["eq", "equalto", "=="] {
        env.add_test(name, t::is_eq);
    }
    for name in ["ne", "!="] {
        env.add_test(name, t::is_ne);
    }
    for name in ["lt", "lessthan", "<"] {
        env.add_test(name, t::is_lt);
    }
    for name in ["le", "<="] {
        env.add_test(name, t::is_le);
    }
    for name in ["gt", "greaterthan", ">"] {
        env.add_test(name, t::is_gt);
    }
    for name in ["ge", ">="] {
        env.add_test(name, t::is_ge);
    }
    env.add_test("in", t::is_in);
    env.add_test("true", t::is_true);
    env.add_test("false", t::is_false);
    env.add_test("filter", t::is_filter);
    env.add_test("test", t::is_test);

    env.add_function("dict", g::dict);
    env.add_function("namespace", g::namespace);
}

/// Registers every filter, test, global and method this module provides.
pub(super) fn register(env: &mut Environment<'_>) {
    for &(name, f) in FILTERS {
        env.add_filter(name, move |v: Value, a: &[Value], k: Kwargs| f(v, a, k));
    }
    for &(name, f) in STATE_FILTERS {
        env.add_filter(
            name,
            move |s: &State<'_, '_>, v: Value, a: &[Value], k: Kwargs| f(s, v, a, k),
        );
    }
    for &(name, f) in UNARY_FILTERS {
        env.add_filter(name, move |v: Value| f(v));
    }
    for &(name, f) in UNARY_FUNCTIONS {
        env.add_function(name, move |v: Value| f(v));
    }
    for &(name, f) in OPERATORS {
        env.add_function(name, move |s: &State<'_, '_>, a: Value, b: Value| {
            f(s, a, b)
        });
    }

    env.add_test("callable", is_callable);
    env.add_test("sequence", is_sequence);
    env.add_test("divisibleby", super::arith::divisibleby);

    env.add_function("cycler", cycler);
    env.add_function("joiner", joiner);
    // `debug()`, which dumps the whole render context -- config values and
    // every `JINJA_VAR_*` included -- into the output, is not registered:
    // Jinja2 has no such global without an extension, and a deploy has no
    // use for one.

    // Python's methods on values; template-built lists and dicts carry
    // their own (mutating) methods.
    env.set_unknown_method_callback(super::methods::dispatch);
    // Naive dates; `+`/`-` reach them through the prepass.
    env.add_function("date", super::dates::date);
    env.add_function("datetime", super::dates::datetime);
    env.add_function("timedelta", super::dates::timedelta);
    env.add_filter("strptime", super::dates::strptime);
    env.add_function("strptime", super::dates::strptime);
    // String loops, isolated includes and variadic macros, reached through
    // the prepass.
    env.add_function(super::arith::ITER_FN, super::arith::iter);
    env.add_function(super::arith::INCLUDE_FN, super::arith::include);
    env.add_function(super::arith::VARIADIC_FN, super::arith::variadic);
}

// ---------------------------------------------------------------------------
// Argument helpers
// ---------------------------------------------------------------------------

/// The `idx`-th argument, given positionally or by `name`.
fn arg(args: &[Value], idx: usize, kwargs: &Kwargs, name: &str) -> Result<Option<Value>, Error> {
    match args.get(idx) {
        Some(v) => Ok(Some(v.clone())),
        None => kwargs.get::<Option<Value>>(name),
    }
}

fn at_most(args: &[Value], n: usize, filter: &str) -> Result<(), Error> {
    if args.len() > n {
        Err(Error::new(
            ErrorKind::TooManyArguments,
            format!("{filter} takes at most {n} arguments"),
        ))
    } else {
        Ok(())
    }
}

fn invalid(msg: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidOperation, msg.into())
}

fn undefined() -> Error {
    Error::from(ErrorKind::UndefinedError)
}

fn as_bool(v: Option<Value>) -> bool {
    v.is_some_and(|v| v.is_true())
}

/// Jinja2's attribute getter: a dotted path, each segment an item lookup,
/// and an all-digit segment an index.
fn lookup(value: &Value, path: &str) -> Result<Value, Error> {
    let mut cur = value.clone();
    for part in path.split('.') {
        cur = match part.parse::<i64>() {
            Ok(i) if !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()) => {
                cur.get_item(&Value::from(i))?
            }
            _ => cur.get_attr(part)?,
        };
    }
    Ok(cur)
}

// ---------------------------------------------------------------------------
// Numbers
// ---------------------------------------------------------------------------

/// Python's `int(text, base)`: surrounding whitespace, a sign, the base
/// prefix when the base allows it, and single underscores between digits.
fn parse_py_int(text: &str, base: u32) -> Option<i128> {
    let t = text.trim();
    let (neg, t) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    let prefix = t.get(..2).map(str::to_ascii_lowercase);
    let (base, body, prefixed) = match (base, prefix.as_deref()) {
        (0 | 16, Some("0x")) => (16, &t[2..], true),
        (0 | 8, Some("0o")) => (8, &t[2..], true),
        (0 | 2, Some("0b")) => (2, &t[2..], true),
        (0, _) => (10, t, false),
        (b, _) => (b, t, false),
    };
    if !(2..=36).contains(&base) {
        return None;
    }
    let body = if prefixed {
        body.strip_prefix('_').unwrap_or(body)
    } else {
        body
    };
    let digits = strip_digit_underscores(body)?;
    if digits.is_empty() || digits.starts_with(['+', '-']) {
        return None;
    }
    let n = i128::from_str_radix(&digits, base).ok()?;
    Some(if neg { -n } else { n })
}

/// Removes single underscores that sit between two digits; `None` for any
/// other underscore, which Python rejects.
fn strip_digit_underscores(s: &str) -> Option<String> {
    if !s.contains('_') {
        return Some(s.to_string());
    }
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'_' {
            let ok = i > 0
                && i + 1 < bytes.len()
                && bytes[i - 1].is_ascii_alphanumeric()
                && bytes[i + 1].is_ascii_alphanumeric();
            if !ok {
                return None;
            }
        } else {
            out.push(b as char);
        }
    }
    Some(out)
}

/// Python's `float(text)`.
fn parse_py_float(text: &str) -> Option<f64> {
    let t = text.trim();
    let lower = t.to_ascii_lowercase();
    let unsigned = lower.trim_start_matches(['+', '-']);
    // Rust accepts exactly the special spellings Python does, but also
    // forms Python refuses ("infinity" is accepted by both; "+nan" too).
    if unsigned.starts_with("0x") {
        return None;
    }
    strip_digit_underscores(t)?.parse::<f64>().ok()
}

fn number_to_f64(v: &Value) -> Option<f64> {
    match v.kind() {
        ValueKind::Number => f64::try_from(v.clone()).ok(),
        ValueKind::Bool => Some(if v.is_true() { 1.0 } else { 0.0 }),
        ValueKind::String => parse_py_float(v.as_str().unwrap_or_default()),
        _ => None,
    }
}

fn is_integer(v: &Value) -> bool {
    minijinja::tests::is_integer(v)
}

/// Jinja2's `int(value, default=0, base=10)`: a string is parsed in `base`,
/// then as a float and truncated; anything that is neither yields `default`.
fn int_compat(value: Value, args: &[Value], kwargs: Kwargs) -> Result<Value, Error> {
    at_most(args, 2, "int")?;
    let default = arg(args, 0, &kwargs, "default")?.unwrap_or(Value::from(0));
    let base = match arg(args, 1, &kwargs, "base")? {
        Some(b) => u32::try_from(b).map_err(|_| invalid("int: base must be an integer"))?,
        None => 10,
    };
    kwargs.assert_all_used()?;
    let truncate = |f: f64| -> Result<Value, Error> {
        if f.is_nan() {
            Ok(default.clone())
        } else if f.is_infinite() {
            Err(invalid("cannot convert float infinity to integer"))
        } else if f.abs() < 2f64.powi(127) {
            Ok(Value::from(f.trunc() as i128))
        } else {
            // Python's int is unbounded; this engine's stops at 128 bits.
            // Saturating would answer a wrong number, so it refuses.
            Err(invalid(format!(
                "int: {f} is past the 128-bit integers this engine holds"
            )))
        }
    };
    match value.kind() {
        ValueKind::Undefined => Err(undefined()),
        ValueKind::Bool => Ok(Value::from(i64::from(value.is_true()))),
        ValueKind::Number if is_integer(&value) => Ok(value),
        ValueKind::Number => truncate(f64::try_from(value).unwrap_or(f64::NAN)),
        ValueKind::String => {
            let s = value.as_str().unwrap_or_default();
            match parse_py_int(s, base) {
                Some(n) => Ok(Value::from(n)),
                None => match parse_py_float(s) {
                    Some(f) => truncate(f),
                    None => Ok(default),
                },
            }
        }
        _ => Ok(default),
    }
}

/// Jinja2's `float(value, default=0.0)`.
fn float_compat(value: Value, args: &[Value], kwargs: Kwargs) -> Result<Value, Error> {
    at_most(args, 1, "float")?;
    let default = arg(args, 0, &kwargs, "default")?.unwrap_or(Value::from(0.0));
    kwargs.assert_all_used()?;
    if value.is_undefined() {
        return Err(undefined());
    }
    Ok(number_to_f64(&value).map_or(default, Value::from))
}

/// Jinja2's `round(value, precision=0, method='common')`. `common` is
/// Python's `round`: ties go to the even neighbour, an integer stays an
/// integer. `ceil` and `floor` always answer a float.
fn round_compat(value: Value, args: &[Value], kwargs: Kwargs) -> Result<Value, Error> {
    at_most(args, 2, "round")?;
    let precision = match arg(args, 0, &kwargs, "precision")? {
        Some(p) => i32::try_from(p).map_err(|_| invalid("round: precision must be an integer"))?,
        None => 0,
    };
    let method = arg(args, 1, &kwargs, "method")?;
    kwargs.assert_all_used()?;
    let method = method.as_ref().and_then(Value::as_str).unwrap_or("common");
    if value.is_undefined() {
        return Err(undefined());
    }
    let x = number_to_f64(&value)
        .filter(|_| value.kind() != ValueKind::String)
        .ok_or_else(|| invalid("round: value must be a number"))?;
    // Past these the arithmetic below has no answer (10**p overflows or
    // underflows a float); Python's does, so give it directly. Rounding to
    // more digits than any float carries changes nothing, and rounding to
    // fewer than -308 leaves zero; `ceil`/`floor` scale by 10**p first and
    // raise as Python does.
    if precision > 330 && method == "common" {
        return Ok(value);
    }
    if precision < -308 && method == "common" {
        return Ok(if is_integer(&value) {
            Value::from(0)
        } else {
            Value::from(0.0f64.copysign(x))
        });
    }
    if precision > 308 && method != "common" {
        return Err(invalid("OverflowError: int too large to convert to float"));
    }
    if precision < -323 && method != "common" {
        return Err(Error::new(
            ErrorKind::InvalidOperation,
            "ZeroDivisionError: float division by zero",
        ));
    }
    let scale = 10f64.powi(precision);
    match method {
        "common" if is_integer(&value) && precision >= 0 => Ok(value),
        "common" if is_integer(&value) => {
            let step = 10f64.powi(-precision);
            Ok(Value::from(((x / step).round_ties_even() * step) as i128))
        }
        "common" if precision >= 0 => {
            // Formatting is correctly rounded from the exact binary value,
            // which is what Python's round does; parsing back gives the
            // nearest float, as Python returns.
            let text = format!("{:.*}", precision as usize, x);
            Ok(Value::from(text.parse::<f64>().unwrap_or(x)))
        }
        "common" => {
            let step = 10f64.powi(-precision);
            Ok(Value::from((x / step).round_ties_even() * step))
        }
        "ceil" => Ok(Value::from((x * scale).ceil() / scale)),
        "floor" => Ok(Value::from((x * scale).floor() / scale)),
        _ => Err(invalid("round: method must be 'common', 'ceil' or 'floor'")),
    }
}

/// Jinja2's `sum(iterable, attribute=None, start=0)`.
fn sum_compat(value: Value, args: &[Value], kwargs: Kwargs) -> Result<Value, Error> {
    at_most(args, 2, "sum")?;
    let attribute = arg(args, 0, &kwargs, "attribute")?;
    let start = arg(args, 1, &kwargs, "start")?.unwrap_or(Value::from(0));
    kwargs.assert_all_used()?;
    if value.is_undefined() {
        return Err(undefined());
    }
    enum Acc {
        Int(i128),
        Float(f64),
    }
    let add = |acc: Acc, v: &Value| -> Result<Acc, Error> {
        if v.is_undefined() {
            return Err(undefined());
        }
        if is_integer(v) || v.kind() == ValueKind::Bool {
            let n = if v.kind() == ValueKind::Bool {
                i128::from(v.is_true())
            } else {
                i128::try_from(v.clone())?
            };
            return Ok(match acc {
                Acc::Int(a) => Acc::Int(
                    a.checked_add(n)
                        .ok_or_else(|| invalid("sum: integer overflow"))?,
                ),
                Acc::Float(a) => Acc::Float(a + n as f64),
            });
        }
        match (v.kind(), acc) {
            (ValueKind::Number, Acc::Int(a)) => {
                Ok(Acc::Float(a as f64 + f64::try_from(v.clone())?))
            }
            (ValueKind::Number, Acc::Float(a)) => Ok(Acc::Float(a + f64::try_from(v.clone())?)),
            _ => Err(invalid(format!(
                "sum: unsupported operand of type {}",
                v.kind()
            ))),
        }
    };
    let mut acc = add(Acc::Int(0), &start)?;
    for item in value.try_iter()? {
        let item = match &attribute {
            Some(a) => lookup(&item, a.as_str().unwrap_or_default())?,
            None => item,
        };
        acc = add(acc, &item)?;
    }
    Ok(match acc {
        Acc::Int(n) => Value::from(n),
        Acc::Float(f) => Value::from(f),
    })
}

/// Jinja2's `max`/`min(value, case_sensitive=False, attribute=None)`: the
/// first extreme item, strings compared case-insensitively by default.
fn extreme(
    value: Value,
    args: &[Value],
    kwargs: Kwargs,
    want: CmpOrdering,
) -> Result<Value, Error> {
    at_most(
        args,
        2,
        if want == CmpOrdering::Greater {
            "max"
        } else {
            "min"
        },
    )?;
    let case_sensitive = as_bool(arg(args, 0, &kwargs, "case_sensitive")?);
    let attribute = arg(args, 1, &kwargs, "attribute")?;
    kwargs.assert_all_used()?;
    if value.is_undefined() {
        return Err(undefined());
    }
    let key = |item: &Value| -> Result<Value, Error> {
        let k = match &attribute {
            Some(a) => lookup(item, a.as_str().unwrap_or_default())?,
            None => item.clone(),
        };
        Ok(match (case_sensitive, k.as_str()) {
            (false, Some(s)) => Value::from(s.to_lowercase()),
            _ => k,
        })
    };
    let mut best: Option<(Value, Value)> = None;
    for item in value.try_iter()? {
        let k = key(&item)?;
        let better = match &best {
            None => true,
            Some((bk, _)) => k.cmp(bk) == want,
        };
        if better {
            best = Some((k, item));
        }
    }
    Ok(best.map_or(Value::UNDEFINED, |(_, item)| item))
}

fn max_compat(value: Value, args: &[Value], kwargs: Kwargs) -> Result<Value, Error> {
    extreme(value, args, kwargs, CmpOrdering::Greater)
}

fn min_compat(value: Value, args: &[Value], kwargs: Kwargs) -> Result<Value, Error> {
    extreme(value, args, kwargs, CmpOrdering::Less)
}

/// Jinja2's `filesizeformat(value, binary=False)`.
fn filesizeformat(value: Value, args: &[Value], kwargs: Kwargs) -> Result<Value, Error> {
    at_most(args, 1, "filesizeformat")?;
    let binary = as_bool(arg(args, 0, &kwargs, "binary")?);
    kwargs.assert_all_used()?;
    if value.is_undefined() {
        return Err(undefined());
    }
    let bytes = number_to_f64(&value).ok_or_else(|| invalid("filesizeformat: not a number"))?;
    let (base, prefixes): (f64, [&str; 8]) = if binary {
        (
            1024.0,
            ["KiB", "MiB", "GiB", "TiB", "PiB", "EiB", "ZiB", "YiB"],
        )
    } else {
        (1000.0, ["kB", "MB", "GB", "TB", "PB", "EB", "ZB", "YB"])
    };
    if bytes == 1.0 {
        return Ok(Value::from("1 Byte"));
    }
    if bytes < base {
        return Ok(Value::from(format!("{} Bytes", bytes.trunc() as i128)));
    }
    let mut unit = base;
    let mut prefix = prefixes[0];
    for (i, p) in prefixes.iter().enumerate() {
        unit = base.powi(i as i32 + 2);
        prefix = p;
        if bytes < unit {
            break;
        }
    }
    Ok(Value::from(format!(
        "{:.1} {}",
        base * bytes / unit,
        prefix
    )))
}

// ---------------------------------------------------------------------------
// Strings
// ---------------------------------------------------------------------------

/// Text of a value as Jinja2's `str()` gives it; undefined is an error.
fn text(value: &Value) -> Result<String, Error> {
    if value.is_undefined() {
        return Err(undefined());
    }
    Ok(value.to_string())
}

/// Jinja2's `replace(s, old, new, count=None)`.
fn replace_compat(value: Value, args: &[Value], kwargs: Kwargs) -> Result<Value, Error> {
    at_most(args, 3, "replace")?;
    let old = arg(args, 0, &kwargs, "old")?.ok_or_else(|| {
        Error::new(
            ErrorKind::MissingArgument,
            "replace requires the text to replace",
        )
    })?;
    let new = arg(args, 1, &kwargs, "new")?
        .ok_or_else(|| Error::new(ErrorKind::MissingArgument, "replace requires a replacement"))?;
    let count = arg(args, 2, &kwargs, "count")?;
    kwargs.assert_all_used()?;
    let (s, old, new) = (text(&value)?, text(&old)?, text(&new)?);
    let count = match count.filter(|c| !c.is_none()) {
        Some(c) => i64::try_from(c).map_err(|_| invalid("replace: count must be an integer"))?,
        None => -1,
    };
    Ok(Value::from(if count < 0 {
        s.replace(&old, &new)
    } else {
        s.replacen(&old, &new, count as usize)
    }))
}

/// Jinja2's `join(value, d='', attribute=None)`.
fn join_compat(value: Value, args: &[Value], kwargs: Kwargs) -> Result<Value, Error> {
    at_most(args, 2, "join")?;
    let sep = match arg(args, 0, &kwargs, "d")? {
        Some(d) => text(&d)?,
        None => String::new(),
    };
    let attribute = arg(args, 1, &kwargs, "attribute")?;
    kwargs.assert_all_used()?;
    if value.is_undefined() {
        return Err(undefined());
    }
    let mut out = String::new();
    for (i, item) in value.try_iter()?.enumerate() {
        if i > 0 {
            out.push_str(&sep);
        }
        let item = match &attribute {
            Some(a) => lookup(&item, a.as_str().unwrap_or_default())?,
            None => item,
        };
        out.push_str(&text(&item)?);
    }
    Ok(Value::from(out))
}

/// Jinja2's `dictsort(value, case_sensitive=False, by='key', reverse=False)`,
/// positional or by name: the `(key, value)` pairs of a mapping, sorted by
/// key or value, strings compared case-insensitively unless asked, stable
/// (equal entries keep their order, reversed too, as Python's `sorted`).
///
/// Sorted as `[sort key, position]` values through the engine's own sort of
/// values, which the binary already carries.
fn dictsort_compat(value: Value, args: &[Value], kwargs: Kwargs) -> Result<Value, Error> {
    at_most(args, 3, "dictsort")?;
    let case_sensitive = as_bool(arg(args, 0, &kwargs, "case_sensitive")?);
    let by = arg(args, 1, &kwargs, "by")?;
    let reverse = as_bool(arg(args, 2, &kwargs, "reverse")?);
    kwargs.assert_all_used()?;
    let by_value = match by.as_ref().map(|b| b.as_str()) {
        None | Some(Some("key")) => false,
        Some(Some("value")) => true,
        _ => return Err(invalid("You can only sort by either \"key\" or \"value\"")),
    };
    if value.kind() != ValueKind::Map {
        return Err(invalid("cannot convert value into pair list"));
    }
    let mut pairs = Vec::new();
    for key in value.try_iter()? {
        let item = value.get_item(&key).unwrap_or(Value::UNDEFINED);
        pairs.push((key, item));
    }
    let mut order: Vec<Value> = Vec::with_capacity(pairs.len());
    for (i, (k, v)) in pairs.iter().enumerate() {
        let on = if by_value { v } else { k };
        let sort_key = match on.as_str() {
            Some(s) if !case_sensitive => Value::from(s.to_lowercase()),
            _ => on.clone(),
        };
        let at = if reverse { -(i as i64) } else { i as i64 };
        order.push(Value::from(vec![sort_key, Value::from(at)]));
    }
    order.sort();
    if reverse {
        order.reverse();
    }
    let mut out = Vec::with_capacity(order.len());
    for entry in &order {
        let at = entry
            .get_item_by_index(1)
            .ok()
            .and_then(|v| i64::try_from(v).ok())
            .unwrap_or(0)
            .unsigned_abs() as usize;
        let (k, v) = &pairs[at];
        out.push(Value::from(vec![k.clone(), v.clone()]));
    }
    Ok(Value::from(out))
}

/// The characters Jinja2's `title` treats as starting a word.
fn starts_word(c: char) -> bool {
    c == '-' || c.is_whitespace() || matches!(c, '(' | '{' | '[' | '<')
}

/// Jinja2's `format(value, *args, **kwargs)`: `value % kwargs` when keywords
/// are given, else `value % args`.
fn format_filter(
    state: &State<'_, '_>,
    value: Value,
    args: &[Value],
    kwargs: Kwargs,
) -> Result<Value, Error> {
    let fmt = text(&value)?;
    let names: Vec<String> = kwargs.args().map(str::to_string).collect();
    let rhs = if names.is_empty() {
        Value::from(args.to_vec())
    } else {
        let mut map = std::collections::BTreeMap::new();
        for n in names {
            let v: Value = kwargs.get(&n)?;
            map.insert(Value::from(n), v);
        }
        Value::from_object(map)
    };
    kwargs.assert_all_used()?;
    let cap = super::methods::output_cap(state);
    Ok(Value::from(super::methods::printf(&fmt, &rhs, cap)?))
}

/// Jinja2's `title`: a word starts after whitespace, `-` or an opening
/// bracket -- not after `_` or `'`, which minijinja's builtin also splits on.
fn title_compat(value: Value) -> Result<Value, Error> {
    let s = text(&value)?;
    let mut out = String::with_capacity(s.len());
    let mut at_start = true;
    for c in s.chars() {
        if starts_word(c) {
            out.push(c);
            at_start = true;
        } else if at_start {
            out.extend(c.to_uppercase());
            at_start = false;
        } else {
            out.extend(c.to_lowercase());
        }
    }
    Ok(Value::from(out))
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Jinja2's `wordcount`: the number of `\w+` runs.
fn wordcount(value: Value) -> Result<Value, Error> {
    let s = text(&value)?;
    let mut n = 0usize;
    let mut in_word = false;
    for c in s.chars() {
        let w = is_word_char(c);
        if w && !in_word {
            n += 1;
        }
        in_word = w;
    }
    Ok(Value::from(n))
}

/// Percent-encodes UTF-8, leaving RFC 3986's unreserved set and `safe`.
fn url_quote(s: &str, for_query: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        let keep = b.is_ascii_alphanumeric()
            || matches!(b, b'_' | b'.' | b'-' | b'~')
            || (!for_query && b == b'/');
        if keep {
            out.push(b as char);
        } else if for_query && b == b' ' {
            out.push('+');
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

/// Jinja2's `urlencode`: a string is quoted keeping `/`; a map, or a
/// sequence of pairs, becomes a query string.
fn urlencode(value: Value) -> Result<Value, Error> {
    match value.kind() {
        ValueKind::Undefined => Err(undefined()),
        ValueKind::Map => {
            let mut parts = Vec::new();
            for key in value.try_iter()? {
                let v = value.get_item(&key)?;
                parts.push(format!(
                    "{}={}",
                    url_quote(&text(&key)?, true),
                    url_quote(&text(&v)?, true)
                ));
            }
            Ok(Value::from(parts.join("&")))
        }
        ValueKind::Seq | ValueKind::Iterable => {
            let mut parts = Vec::new();
            for pair in value.try_iter()? {
                let (k, v) = (
                    pair.get_item(&Value::from(0))?,
                    pair.get_item(&Value::from(1))?,
                );
                parts.push(format!(
                    "{}={}",
                    url_quote(&text(&k)?, true),
                    url_quote(&text(&v)?, true)
                ));
            }
            Ok(Value::from(parts.join("&")))
        }
        _ => Ok(Value::from(url_quote(&text(&value)?, false))),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Jinja2's `callable`: functions, macros and joiners, but not data.
///
/// A function is a plain object. A macro is a map-shaped object (it exposes
/// `name` and `arguments`) and minijinja keeps its type private, so it is
/// recognised by the debug form it writes for itself, `<macro name>`.
fn is_callable(value: Value) -> bool {
    match value.kind() {
        ValueKind::Plain => value.downcast_object_ref::<Cycler>().is_none(),
        ValueKind::Map => {
            value.as_object().is_some() && format!("{value:?}").starts_with("<macro ")
        }
        _ => false,
    }
}

/// Jinja2's `sequence`: anything with a length and item access, which
/// includes strings and maps.
fn is_sequence(value: Value) -> bool {
    matches!(
        value.kind(),
        ValueKind::Seq | ValueKind::String | ValueKind::Map | ValueKind::Bytes
    )
}

// ---------------------------------------------------------------------------
// Globals: cycler, joiner
// ---------------------------------------------------------------------------

/// Jinja2's `cycler(*items)`: `next()`, `reset()` and `current`.
#[derive(Debug)]
struct Cycler {
    items: Vec<Value>,
    pos: AtomicUsize,
}

impl Object for Cycler {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Plain
    }

    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        match key.as_str()? {
            "current" => Some(self.items[self.pos.load(Ordering::Relaxed)].clone()),
            "items" => Some(Value::from(self.items.clone())),
            _ => None,
        }
    }

    fn call_method(
        self: &Arc<Self>,
        _state: &State<'_, '_>,
        method: &str,
        args: &[Value],
    ) -> Result<Value, Error> {
        if !args.is_empty() {
            return Err(Error::new(
                ErrorKind::TooManyArguments,
                format!("{method} takes no arguments"),
            ));
        }
        match method {
            "next" => {
                let len = self.items.len();
                let at = self
                    .pos
                    .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |p| {
                        Some((p + 1) % len)
                    })
                    .unwrap_or(0);
                Ok(self.items[at].clone())
            }
            "reset" => {
                self.pos.store(0, Ordering::Relaxed);
                Ok(Value::from(()))
            }
            _ => Err(Error::from(ErrorKind::UnknownMethod)),
        }
    }
}

fn cycler(args: minijinja::value::Rest<Value>) -> Result<Value, Error> {
    if args.is_empty() {
        return Err(Error::new(
            ErrorKind::MissingArgument,
            "at least one item has to be provided",
        ));
    }
    Ok(Value::from_object(Cycler {
        items: args.0,
        pos: AtomicUsize::new(0),
    }))
}

/// Jinja2's `joiner(sep=', ')`: empty on the first call, `sep` after.
#[derive(Debug)]
struct Joiner {
    sep: String,
    used: AtomicBool,
}

impl Object for Joiner {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Plain
    }

    fn call(self: &Arc<Self>, _state: &State<'_, '_>, args: &[Value]) -> Result<Value, Error> {
        if !args.is_empty() {
            return Err(Error::new(
                ErrorKind::TooManyArguments,
                "a joiner takes no arguments",
            ));
        }
        Ok(Value::from(if self.used.swap(true, Ordering::Relaxed) {
            self.sep.as_str()
        } else {
            ""
        }))
    }
}

fn joiner(sep: Option<String>) -> Value {
    Value::from_object(Joiner {
        sep: sep.unwrap_or_else(|| ", ".to_string()),
        used: AtomicBool::new(false),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_int_parsing() {
        assert_eq!(parse_py_int(" 42 ", 10), Some(42));
        assert_eq!(parse_py_int("-3", 10), Some(-3));
        assert_eq!(parse_py_int("1_000", 10), Some(1000));
        assert_eq!(parse_py_int("1__0", 10), None);
        assert_eq!(parse_py_int("_1", 10), None);
        assert_eq!(parse_py_int("0x1F", 16), Some(31));
        assert_eq!(parse_py_int("0x_1f", 0), Some(31));
        assert_eq!(parse_py_int("0b101", 0), Some(5));
        assert_eq!(parse_py_int("0x1F", 10), None);
        assert_eq!(parse_py_int("", 10), None);
        assert_eq!(parse_py_int("--1", 10), None);
        assert_eq!(parse_py_int("z", 36), Some(35));
        assert_eq!(parse_py_int("1", 37), None);
    }

    #[test]
    fn python_float_parsing() {
        assert_eq!(parse_py_float(" 1.5 "), Some(1.5));
        assert_eq!(parse_py_float("1e3"), Some(1000.0));
        assert_eq!(parse_py_float("1_0.5"), Some(10.5));
        assert_eq!(parse_py_float("0x10"), None);
        assert!(parse_py_float("nan").unwrap().is_nan());
        assert_eq!(parse_py_float("abc"), None);
    }

    #[test]
    fn url_quoting_matches_python() {
        assert_eq!(url_quote("a b&c/d", false), "a%20b%26c/d");
        assert_eq!(url_quote("v w/", true), "v+w%2F");
        assert_eq!(url_quote("é", false), "%C3%A9");
        assert_eq!(url_quote("_.-~", true), "_.-~");
    }
}

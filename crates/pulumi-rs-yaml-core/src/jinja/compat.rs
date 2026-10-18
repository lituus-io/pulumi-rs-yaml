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

use minijinja::value::{from_args, Kwargs, Object, ObjectRepr, Value, ValueKind};
use minijinja::{Environment, Error, ErrorKind, State};

/// Registers every filter, test, global and method this module provides.
pub(super) fn register(env: &mut Environment<'_>) {
    env.add_filter("int", int_compat);
    env.add_filter("float", float_compat);
    env.add_filter("round", round_compat);
    env.add_filter("replace", replace_compat);
    env.add_filter("join", join_compat);
    env.add_filter("sum", sum_compat);
    env.add_filter("max", max_compat);
    env.add_filter("min", min_compat);
    env.add_filter("dictsort", dictsort_compat);
    env.add_filter("title", title_compat);
    env.add_filter("wordcount", wordcount);
    env.add_filter("filesizeformat", filesizeformat);
    env.add_filter("urlencode", urlencode);

    env.add_test("callable", is_callable);
    env.add_test("sequence", is_sequence);

    env.add_function("cycler", cycler);
    env.add_function("joiner", joiner);
    // `debug()` dumps the whole render context -- config values and every
    // `JINJA_VAR_*` included -- into the output. Jinja2 has no such global
    // without an extension; a deploy has no use for one.
    env.remove_global("debug");

    env.set_unknown_method_callback(method_compat);
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
        } else {
            Ok(Value::from(f.trunc() as i128))
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

/// Jinja2's `dictsort(value, case_sensitive=False, by='key', reverse=False)`
/// with its positional spelling; the sort itself is minijinja's.
fn dictsort_compat(value: Value, args: &[Value], kwargs: Kwargs) -> Result<Value, Error> {
    at_most(args, 3, "dictsort")?;
    let mut pass: Vec<(String, Value)> = Vec::with_capacity(3);
    for (idx, name) in ["case_sensitive", "by", "reverse"].into_iter().enumerate() {
        if let Some(v) = arg(args, idx, &kwargs, name)? {
            pass.push((name.to_string(), v));
        }
    }
    kwargs.assert_all_used()?;
    minijinja::filters::dictsort(&value, Kwargs::from_iter(pass))
}

/// The characters Jinja2's `title` treats as starting a word.
fn starts_word(c: char) -> bool {
    c == '-' || c.is_whitespace() || matches!(c, '(' | '{' | '[' | '<')
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

// ---------------------------------------------------------------------------
// Methods: Python's str and dict methods, which minijinja exposes as filters
// ---------------------------------------------------------------------------

/// Splits a trailing keyword-argument map off a method call's arguments.
fn split_kwargs(args: &[Value]) -> Result<(&[Value], Option<Kwargs>), Error> {
    match args.split_last() {
        Some((last, rest)) if last.is_kwargs() => {
            let (kw,): (Kwargs,) = from_args(std::slice::from_ref(last))?;
            Ok((rest, Some(kw)))
        }
        _ => Ok((args, None)),
    }
}

fn no_args(name: &str, args: &[Value]) -> Result<(), Error> {
    if args.is_empty() {
        Ok(())
    } else {
        Err(Error::new(
            ErrorKind::TooManyArguments,
            format!("{name} takes no arguments"),
        ))
    }
}

fn str_arg(args: &[Value], idx: usize, name: &str) -> Result<String, Error> {
    let v = args.get(idx).ok_or_else(|| {
        Error::new(
            ErrorKind::MissingArgument,
            format!("{name} is missing an argument"),
        )
    })?;
    v.as_str()
        .map(str::to_string)
        .ok_or_else(|| invalid(format!("{name} expects a string argument")))
}

/// Python-style methods on the values Jinja2 templates call them on.
///
/// Borrows the receiver and arguments throughout. Unknown names fall
/// through to minijinja's own error, so a typo still reads as an unknown
/// method.
fn method_compat(
    _state: &State<'_, '_>,
    value: &Value,
    method: &str,
    args: &[Value],
) -> Result<Value, Error> {
    if let Some(s) = value.as_str() {
        return str_method(s, method, args);
    }
    match method {
        "items" => {
            no_args("items", args)?;
            minijinja::filters::items(value)
        }
        "keys" | "values" => {
            no_args(method, args)?;
            let idx = usize::from(method == "values");
            let pairs = minijinja::filters::items(value)?;
            let out = pairs
                .try_iter()?
                .filter_map(|pair| pair.get_item_by_index(idx).ok())
                .collect::<Vec<_>>();
            Ok(Value::from(out))
        }
        "get" => {
            let key = args
                .first()
                .ok_or_else(|| Error::new(ErrorKind::MissingArgument, "get requires a key"))?;
            let found = value.get_item(key).ok().filter(|v| !v.is_undefined());
            // Python's dict.get answers None when absent and no default given.
            Ok(found
                .or_else(|| args.get(1).cloned())
                .unwrap_or_else(|| Value::from(())))
        }
        _ => Err(Error::from(ErrorKind::UnknownMethod)),
    }
}

/// `str.split`/`rsplit` with Python's two modes: no separator splits on
/// runs of whitespace and drops empties; a separator splits exactly.
fn py_split(s: &str, sep: Option<&str>, maxsplit: i64) -> Result<Vec<Value>, Error> {
    let limit = (maxsplit >= 0).then_some(maxsplit as usize);
    let mut out = Vec::new();
    match sep {
        Some("") => return Err(invalid("empty separator")),
        Some(sep) => match limit {
            Some(n) => out.extend(s.splitn(n + 1, sep).map(Value::from)),
            None => out.extend(s.split(sep).map(Value::from)),
        },
        None => {
            let mut rest = s.trim_start();
            while !rest.is_empty() {
                if limit.is_some_and(|n| out.len() == n) {
                    out.push(Value::from(rest));
                    break;
                }
                let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
                out.push(Value::from(&rest[..end]));
                rest = rest[end..].trim_start();
            }
        }
    }
    Ok(out)
}

/// Python's `str.format` for automatic (`{}`), numbered (`{0}`) and named
/// (`{name}`) fields and `{{`/`}}` escapes. Format specs and conversions are
/// refused rather than ignored.
fn py_format(s: &str, positional: &[Value], kwargs: Option<&Kwargs>) -> Result<String, Error> {
    let mut out = String::with_capacity(s.len());
    let mut auto = 0usize;
    let mut chars = s.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        match c {
            '{' if matches!(chars.peek(), Some((_, '{'))) => {
                chars.next();
                out.push('{');
            }
            '}' if matches!(chars.peek(), Some((_, '}'))) => {
                chars.next();
                out.push('}');
            }
            '{' => {
                let end = s[i + 1..]
                    .find('}')
                    .ok_or_else(|| invalid("format: single '{' encountered"))?;
                let field = &s[i + 1..i + 1 + end];
                if field.contains([':', '!', '[', '.', '{']) {
                    return Err(invalid(
                        "format: only plain {}, {n} and {name} fields are supported",
                    ));
                }
                let value = if field.is_empty() {
                    auto += 1;
                    positional.get(auto - 1)
                } else if let Ok(n) = field.parse::<usize>() {
                    positional.get(n)
                } else {
                    None
                };
                let value = match value {
                    Some(v) => v.clone(),
                    None if !field.is_empty() && field.parse::<usize>().is_err() => kwargs
                        .map(|k| k.get::<Value>(field))
                        .transpose()?
                        .ok_or_else(|| invalid(format!("format: no argument named {field:?}")))?,
                    None => return Err(invalid("format: replacement index out of range")),
                };
                out.push_str(&text(&value)?);
                for _ in 0..field.chars().count() + 1 {
                    chars.next();
                }
            }
            '}' => return Err(invalid("format: single '}' encountered")),
            other => out.push(other),
        }
    }
    Ok(out)
}

fn str_method(s: &str, method: &str, args: &[Value]) -> Result<Value, Error> {
    let chars_arg = |args: &[Value]| -> Result<Option<String>, Error> {
        match args.first() {
            None => Ok(None),
            Some(v) if v.is_none() => Ok(None),
            Some(_) => str_arg(args, 0, method).map(Some),
        }
    };
    let strip = |chars: Option<String>, left: bool, right: bool| -> String {
        let pred = |c: char| match &chars {
            Some(set) => set.contains(c),
            None => c.is_whitespace(),
        };
        let mut t = s;
        if left {
            t = t.trim_start_matches(pred);
        }
        if right {
            t = t.trim_end_matches(pred);
        }
        t.to_string()
    };
    let (args, kwargs) = split_kwargs(args)?;
    if method != "format" {
        if let Some(kw) = &kwargs {
            if kw.args().next().is_some() && !matches!(method, "split" | "rsplit") {
                return Err(invalid(format!("{method} takes no keyword arguments")));
            }
        }
    }
    let v = match method {
        "upper" => {
            no_args(method, args)?;
            Value::from(s.to_uppercase())
        }
        "lower" => {
            no_args(method, args)?;
            Value::from(s.to_lowercase())
        }
        "strip" => Value::from(strip(chars_arg(args)?, true, true)),
        "lstrip" => Value::from(strip(chars_arg(args)?, true, false)),
        "rstrip" => Value::from(strip(chars_arg(args)?, false, true)),
        "split" => {
            let sep = match args.first() {
                Some(v) if !v.is_none() => Some(str_arg(args, 0, method)?),
                _ => kwargs
                    .as_ref()
                    .map(|k| k.get::<Option<String>>("sep"))
                    .transpose()?
                    .flatten(),
            };
            let maxsplit = match args.get(1) {
                Some(v) => i64::try_from(v.clone())?,
                None => kwargs
                    .as_ref()
                    .map(|k| k.get::<Option<i64>>("maxsplit"))
                    .transpose()?
                    .flatten()
                    .unwrap_or(-1),
            };
            if let Some(kw) = &kwargs {
                kw.assert_all_used()?;
            }
            Value::from(py_split(s, sep.as_deref(), maxsplit)?)
        }
        "splitlines" => {
            no_args(method, args)?;
            Value::from(s.lines().map(Value::from).collect::<Vec<_>>())
        }
        "replace" => {
            let (old, new) = (str_arg(args, 0, method)?, str_arg(args, 1, method)?);
            match args.get(2) {
                Some(c) => {
                    let c = i64::try_from(c.clone())?;
                    Value::from(if c < 0 {
                        s.replace(&old, &new)
                    } else {
                        s.replacen(&old, &new, c as usize)
                    })
                }
                None => Value::from(s.replace(&old, &new)),
            }
        }
        "startswith" | "endswith" => {
            let probe = args.first().ok_or_else(|| {
                Error::new(
                    ErrorKind::MissingArgument,
                    format!("{method} requires a prefix"),
                )
            })?;
            let test = |p: &str| {
                if method == "startswith" {
                    s.starts_with(p)
                } else {
                    s.ends_with(p)
                }
            };
            // Python accepts a tuple of candidates as well as one string.
            let hit = match probe.as_str() {
                Some(p) => test(p),
                None => probe.try_iter()?.any(|p| p.as_str().is_some_and(test)),
            };
            Value::from(hit)
        }
        "join" => {
            let items = args.first().ok_or_else(|| {
                Error::new(ErrorKind::MissingArgument, "join requires an iterable")
            })?;
            let mut out = String::new();
            for (i, item) in items.try_iter()?.enumerate() {
                if i > 0 {
                    out.push_str(s);
                }
                let piece = item
                    .as_str()
                    .ok_or_else(|| invalid("join: sequence item is not a string"))?;
                out.push_str(piece);
            }
            Value::from(out)
        }
        "title" => {
            // Python's str.title starts a word after ANY non-letter, unlike
            // the `title` filter.
            no_args(method, args)?;
            let mut out = String::with_capacity(s.len());
            let mut prev_cased = false;
            for c in s.chars() {
                if prev_cased {
                    out.extend(c.to_lowercase());
                } else {
                    out.extend(c.to_uppercase());
                }
                prev_cased = c.is_alphabetic();
            }
            Value::from(out)
        }
        "capitalize" => {
            no_args(method, args)?;
            let mut chars = s.chars();
            let mut out = String::with_capacity(s.len());
            if let Some(first) = chars.next() {
                out.extend(first.to_uppercase());
                out.extend(chars.flat_map(char::to_lowercase));
            }
            Value::from(out)
        }
        "count" => {
            let sub = str_arg(args, 0, method)?;
            Value::from(if sub.is_empty() {
                s.chars().count() + 1
            } else {
                s.matches(sub.as_str()).count()
            })
        }
        "find" => {
            let sub = str_arg(args, 0, method)?;
            Value::from(match s.find(sub.as_str()) {
                Some(byte) => s[..byte].chars().count() as i64,
                None => -1,
            })
        }
        "format" => Value::from(py_format(s, args, kwargs.as_ref())?),
        "isdigit" | "isalpha" | "isalnum" => {
            no_args(method, args)?;
            let pred: fn(char) -> bool = match method {
                "isdigit" => |c| c.is_numeric(),
                "isalpha" => char::is_alphabetic,
                _ => char::is_alphanumeric,
            };
            Value::from(!s.is_empty() && s.chars().all(pred))
        }
        "islower" | "isupper" => {
            no_args(method, args)?;
            let cased = s.chars().any(|c| c.is_lowercase() || c.is_uppercase());
            let ok = if method == "islower" {
                !s.chars().any(char::is_uppercase)
            } else {
                !s.chars().any(char::is_lowercase)
            };
            Value::from(cased && ok)
        }
        "zfill" => {
            let width = args
                .first()
                .map(|w| usize::try_from(w.clone()))
                .transpose()?
                .unwrap_or(0);
            let len = s.chars().count();
            if len >= width {
                Value::from(s)
            } else {
                let (sign, digits) = match s.as_bytes().first() {
                    Some(b'+' | b'-') => s.split_at(1),
                    _ => ("", s),
                };
                Value::from(format!("{sign}{}{digits}", "0".repeat(width - len)))
            }
        }
        _ => return Err(Error::from(ErrorKind::UnknownMethod)),
    };
    Ok(v)
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

    #[test]
    fn python_split_modes() {
        let s = |v: Vec<Value>| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(s(py_split("  a  b ", None, -1).unwrap()), ["a", "b"]);
        assert_eq!(s(py_split("a b c", None, 1).unwrap()), ["a", "b c"]);
        assert_eq!(s(py_split("a,,b", Some(","), -1).unwrap()), ["a", "", "b"]);
        assert_eq!(s(py_split("a,b,c", Some(","), 1).unwrap()), ["a", "b,c"]);
        assert!(py_split("a", Some(""), -1).is_err());
        assert!(py_split("", None, -1).unwrap().is_empty());
    }

    #[test]
    fn python_format_fields() {
        let pos = [Value::from("a"), Value::from(1)];
        assert_eq!(py_format("{}-{}", &pos, None).unwrap(), "a-1");
        assert_eq!(py_format("{1}{0}{1}", &pos, None).unwrap(), "1a1");
        assert_eq!(py_format("{{x}}", &pos, None).unwrap(), "{x}");
        assert!(py_format("{:>4}", &pos, None).is_err());
        assert!(py_format("{5}", &pos, None).is_err());
        assert!(py_format("{", &pos, None).is_err());
        assert!(py_format("}", &pos, None).is_err());
        assert!(py_format("{name}", &pos, None).is_err());
    }
}

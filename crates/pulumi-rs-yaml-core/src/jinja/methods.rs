// Author: spicyzhug@gmail.com
// Copyright: lituus-io, all rights reserved

//! Python methods on template values: `s.upper()`, `s.split(',')`,
//! `'{}-{}'.format(a, b)`, `d.items()`, `xs.index(x)`.
//!
//! One static, sorted table per receiver kind -- `str`, sequence, mapping,
//! number -- of plain `fn` pointers, found by binary search on the method
//! name: no allocation to dispatch, no trait objects, no captured state.
//! Receivers and arguments are borrowed; a result allocates only where
//! Python would create a new object. Semantics are Python's (the conformance
//! corpus holds them to reference Jinja2 and to the embedded Starlark),
//! including Unicode case mapping, `split()` on whitespace runs, negative
//! indices and `ValueError` for a failed `index`.
//!
//! A method that can grow its result past its inputs (`ljust`, `rjust`,
//! `center`, `zfill`, `expandtabs`, `replace`, `format`) checks the length
//! against the render's output limit before it allocates.

use std::borrow::Cow;

use minijinja::value::{from_args, Kwargs, Value, ValueKind};
use minijinja::{Error, ErrorKind, State};

/// The global a render publishes its output limit under, read by the
/// methods that can grow a value.
pub(super) const OUTPUT_LIMIT_GLOBAL: &str = "__rsyaml_output_limit";

/// The arguments of one method call: positional, and keyword when given.
pub(super) struct Call<'a, 's, 'e> {
    state: &'a State<'s, 'e>,
    args: &'a [Value],
    kwargs: Option<Kwargs>,
}

impl<'a, 's, 'e> Call<'a, 's, 'e> {
    pub(super) fn new(state: &'a State<'s, 'e>, args: &'a [Value]) -> Result<Self, Error> {
        let (args, kwargs) = match args.split_last() {
            Some((last, rest)) if last.is_kwargs() => {
                let (kw,): (Kwargs,) = from_args(std::slice::from_ref(last))?;
                (rest, Some(kw))
            }
            _ => (args, None),
        };
        Ok(Self {
            state,
            args,
            kwargs,
        })
    }

    pub(super) fn kwargs(&self) -> Option<&Kwargs> {
        self.kwargs.as_ref()
    }

    pub(super) fn args(&self) -> &'a [Value] {
        self.args
    }

    /// The `idx`-th argument, positional or by `name`.
    pub(super) fn get(&self, idx: usize, name: &str) -> Result<Option<Value>, Error> {
        match self.args.get(idx) {
            Some(v) => Ok(Some(v.clone())),
            None => match &self.kwargs {
                Some(k) => k.get::<Option<Value>>(name),
                None => Ok(None),
            },
        }
    }

    /// Refuses more than `n` positional arguments, and unused keywords.
    pub(super) fn done(&self, method: &str, n: usize) -> Result<(), Error> {
        if self.args.len() > n {
            return Err(Error::new(
                ErrorKind::TooManyArguments,
                if n == 0 {
                    format!("{method}() takes no arguments ({} given)", self.args.len())
                } else {
                    format!(
                        "{method}() takes at most {n} arguments ({} given)",
                        self.args.len()
                    )
                },
            ));
        }
        match &self.kwargs {
            Some(k) => k.assert_all_used(),
            None => Ok(()),
        }
    }

    fn required(&self, idx: usize, name: &str, method: &str) -> Result<Value, Error> {
        self.get(idx, name)?.ok_or_else(|| {
            Error::new(
                ErrorKind::MissingArgument,
                format!("{method}() missing required argument '{name}'"),
            )
        })
    }

    fn string(&self, idx: usize, name: &str, method: &str) -> Result<String, Error> {
        let v = self.required(idx, name, method)?;
        v.as_str().map(str::to_string).ok_or_else(|| {
            type_error(format!(
                "{method}() argument '{name}' must be str, not {}",
                v.kind()
            ))
        })
    }

    fn opt_int(&self, idx: usize, name: &str) -> Result<Option<i64>, Error> {
        match self.get(idx, name)? {
            Some(v) if v.is_none() => Ok(None),
            Some(v) => i64::try_from(v)
                .map(Some)
                .map_err(|_| type_error(format!("'{name}' must be an integer"))),
            None => Ok(None),
        }
    }

    /// The render's output limit, which a growing method may not pass.
    pub(super) fn cap(&self) -> usize {
        output_cap(self.state)
    }
}

/// The render's output limit, as published in its globals.
pub(super) fn output_cap(state: &State<'_, '_>) -> usize {
    state
        .lookup(OUTPUT_LIMIT_GLOBAL)
        .and_then(|v| usize::try_from(v).ok())
        .unwrap_or(super::limits::JinjaLimits::DEFAULT_OUTPUT_BYTES)
}

/// A render fault the template can name. Python's `TypeError` and
/// `ValueError` both surface as the engine's invalid operation, built in one
/// place, out of line: an error is the cold path at every call site.
#[cold]
#[inline(never)]
pub(super) fn fault(msg: String) -> Error {
    Error::new(ErrorKind::InvalidOperation, msg)
}

/// Python's `TypeError`.
pub(super) fn type_error(msg: impl Into<String>) -> Error {
    fault(msg.into())
}

/// Python's `ValueError`.
pub(super) fn value_error(msg: impl Into<String>) -> Error {
    fault(msg.into())
}

#[cold]
#[inline(never)]
fn too_large(method: &str, len: usize, cap: usize) -> Error {
    Error::new(
        ErrorKind::InvalidOperation,
        format!(
            "{} of {cap} bytes: {method}() would build a value of {len} bytes",
            super::OUTPUT_TAG
        ),
    )
}

pub(super) type StrMethod = fn(&str, &Call<'_, '_, '_>) -> Result<Value, Error>;
pub(super) type ValueMethod = fn(&Value, &Call<'_, '_, '_>) -> Result<Value, Error>;

/// Finds `name` in a table sorted by name.
fn find<T: Copy>(table: &[(&str, T)], name: &str) -> Option<T> {
    table
        .binary_search_by(|(n, _)| (*n).cmp(name))
        .ok()
        .map(|i| table[i].1)
}

/// Every method a `str` answers, sorted by name.
pub(super) const STR_METHODS: &[(&str, StrMethod)] = &[
    ("capitalize", s_capitalize),
    ("casefold", s_casefold),
    ("center", s_center),
    ("count", s_count),
    ("encode", s_encode),
    ("endswith", s_endswith),
    ("expandtabs", s_expandtabs),
    ("find", s_find),
    ("format", s_format),
    ("index", s_index),
    ("isalnum", s_isalnum),
    ("isalpha", s_isalpha),
    ("isdecimal", s_isdecimal),
    ("isdigit", s_isdigit),
    ("isidentifier", s_isidentifier),
    ("islower", s_islower),
    ("isnumeric", s_isnumeric),
    ("isspace", s_isspace),
    ("istitle", s_istitle),
    ("isupper", s_isupper),
    ("join", s_join),
    ("ljust", s_ljust),
    ("lower", s_lower),
    ("lstrip", s_lstrip),
    ("partition", s_partition),
    ("removeprefix", s_removeprefix),
    ("removesuffix", s_removesuffix),
    ("replace", s_replace),
    ("rfind", s_rfind),
    ("rindex", s_rindex),
    ("rjust", s_rjust),
    ("rpartition", s_rpartition),
    ("rsplit", s_rsplit),
    ("rstrip", s_rstrip),
    ("split", s_split),
    ("splitlines", s_splitlines),
    ("startswith", s_startswith),
    ("strip", s_strip),
    ("swapcase", s_swapcase),
    ("title", s_title),
    ("upper", s_upper),
    ("zfill", s_zfill),
];

/// Read-only methods of every sequence, sorted.
pub(super) const SEQ_METHODS: &[(&str, ValueMethod)] =
    &[("copy", q_copy), ("count", q_count), ("index", q_index)];

/// Read-only methods of every mapping, sorted.
pub(super) const MAP_METHODS: &[(&str, ValueMethod)] = &[
    ("copy", m_copy),
    ("get", m_get),
    ("items", m_items),
    ("keys", m_keys),
    ("values", m_values),
];

/// Methods of `int` and `float`, sorted.
pub(super) const NUM_METHODS: &[(&str, ValueMethod)] = &[
    ("as_integer_ratio", n_as_integer_ratio),
    ("bit_length", n_bit_length),
    ("conjugate", n_conjugate),
    ("is_integer", n_is_integer),
];

/// The unknown-method callback: a value's Python method, or minijinja's own
/// "unknown method" error so a typo still reads as one.
pub(super) fn dispatch(
    state: &State<'_, '_>,
    value: &Value,
    method: &str,
    args: &[Value],
) -> Result<Value, Error> {
    let call = Call::new(state, args)?;
    if let Some(s) = value.as_str() {
        if let Some(f) = find(STR_METHODS, method) {
            return f(s, &call);
        }
    } else {
        let table = match value.kind() {
            ValueKind::Seq | ValueKind::Iterable => SEQ_METHODS,
            ValueKind::Map => MAP_METHODS,
            ValueKind::Number => NUM_METHODS,
            _ => &[],
        };
        if let Some(f) = find(table, method) {
            return f(value, &call);
        }
        if super::containers::MUTATORS.contains(&method)
            && matches!(
                value.kind(),
                ValueKind::Seq | ValueKind::Map | ValueKind::Iterable
            )
        {
            return Err(type_error(format!(
                "{method}(): this {} is read-only -- it came from the program's inputs or a \
                 filter; copy it first (`x.copy()`, or `x | list`) and change the copy",
                value.kind()
            )));
        }
    }
    Err(Error::from(ErrorKind::UnknownMethod))
}

// ---------------------------------------------------------------------------
// Character positions (Python indexes str by character, not byte)
// ---------------------------------------------------------------------------

/// Python's slice bounds for `start`/`end` over `len` characters.
fn bounds(len: usize, start: Option<i64>, end: Option<i64>) -> (usize, usize) {
    let norm = |v: i64| -> usize {
        let v = if v < 0 { v + len as i64 } else { v };
        v.clamp(0, len as i64) as usize
    };
    (start.map_or(0, norm), end.map_or(len, norm))
}

/// The byte offset of the `n`-th character.
fn byte_at(s: &str, n: usize) -> usize {
    s.char_indices().nth(n).map_or(s.len(), |(i, _)| i)
}

/// `s[start:end]` by characters, and the character offset it starts at.
fn window<'s>(s: &'s str, call: &Call<'_, '_, '_>, from: usize) -> Result<(&'s str, usize), Error> {
    let len = s.chars().count();
    let (a, b) = bounds(
        len,
        call.opt_int(from, "start")?,
        call.opt_int(from + 1, "end")?,
    );
    if a > b {
        return Ok(("", a));
    }
    Ok((&s[byte_at(s, a)..byte_at(s, b)], a))
}

fn char_index(s: &str, byte: usize) -> usize {
    s[..byte].chars().count()
}

// ---------------------------------------------------------------------------
// str
// ---------------------------------------------------------------------------

fn s_upper(s: &str, c: &Call<'_, '_, '_>) -> Result<Value, Error> {
    c.done("upper", 0)?;
    Ok(Value::from(s.to_uppercase()))
}

fn s_lower(s: &str, c: &Call<'_, '_, '_>) -> Result<Value, Error> {
    c.done("lower", 0)?;
    Ok(Value::from(s.to_lowercase()))
}

fn s_casefold(s: &str, c: &Call<'_, '_, '_>) -> Result<Value, Error> {
    c.done("casefold", 0)?;
    // Full case folding differs from lowercasing for a handful of
    // characters; the German sharp s is the one templates meet.
    Ok(Value::from(s.to_lowercase().replace('ß', "ss")))
}

fn s_swapcase(s: &str, c: &Call<'_, '_, '_>) -> Result<Value, Error> {
    c.done("swapcase", 0)?;
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if ch.is_uppercase() {
            out.extend(ch.to_lowercase());
        } else if ch.is_lowercase() {
            out.extend(ch.to_uppercase());
        } else {
            out.push(ch);
        }
    }
    Ok(Value::from(out))
}

fn s_capitalize(s: &str, c: &Call<'_, '_, '_>) -> Result<Value, Error> {
    c.done("capitalize", 0)?;
    let mut chars = s.chars();
    let mut out = String::with_capacity(s.len());
    if let Some(first) = chars.next() {
        out.extend(first.to_uppercase());
        out.extend(chars.flat_map(char::to_lowercase));
    }
    Ok(Value::from(out))
}

/// Python's `str.title`: a word starts after any character that is not a
/// letter (unlike the `title` filter).
fn s_title(s: &str, c: &Call<'_, '_, '_>) -> Result<Value, Error> {
    c.done("title", 0)?;
    let mut out = String::with_capacity(s.len());
    let mut prev_cased = false;
    for ch in s.chars() {
        if prev_cased {
            out.extend(ch.to_lowercase());
        } else {
            out.extend(ch.to_uppercase());
        }
        prev_cased = ch.is_alphabetic();
    }
    Ok(Value::from(out))
}

fn strip_with(
    s: &str,
    c: &Call<'_, '_, '_>,
    method: &str,
    left: bool,
    right: bool,
) -> Result<Value, Error> {
    let chars = match c.get(0, "chars")? {
        Some(v) if v.is_none() => None,
        Some(v) => Some(
            v.as_str()
                .map(str::to_string)
                .ok_or_else(|| type_error(format!("{method} arg must be None or str")))?,
        ),
        None => None,
    };
    c.done(method, 1)?;
    let pred = |ch: char| match &chars {
        Some(set) => set.contains(ch),
        None => ch.is_whitespace(),
    };
    let mut t = s;
    if left {
        t = t.trim_start_matches(pred);
    }
    if right {
        t = t.trim_end_matches(pred);
    }
    Ok(Value::from(t))
}

fn s_strip(s: &str, c: &Call<'_, '_, '_>) -> Result<Value, Error> {
    strip_with(s, c, "strip", true, true)
}

fn s_lstrip(s: &str, c: &Call<'_, '_, '_>) -> Result<Value, Error> {
    strip_with(s, c, "lstrip", true, false)
}

fn s_rstrip(s: &str, c: &Call<'_, '_, '_>) -> Result<Value, Error> {
    strip_with(s, c, "rstrip", false, true)
}

fn s_replace(s: &str, c: &Call<'_, '_, '_>) -> Result<Value, Error> {
    let old = c.string(0, "old", "replace")?;
    let new = c.string(1, "new", "replace")?;
    let count = c.opt_int(2, "count")?.unwrap_or(-1);
    c.done("replace", 3)?;
    let hits = if old.is_empty() {
        s.chars().count() + 1
    } else {
        s.matches(old.as_str()).count()
    };
    let hits = if count < 0 {
        hits
    } else {
        hits.min(count as usize)
    };
    let len = s.len() + hits * new.len();
    if new.len() > old.len() && len > c.cap() {
        return Err(too_large("replace", len, c.cap()));
    }
    Ok(Value::from(if count < 0 {
        s.replace(&old, &new)
    } else {
        s.replacen(&old, &new, count as usize)
    }))
}

fn removefix(s: &str, c: &Call<'_, '_, '_>, method: &str, prefix: bool) -> Result<Value, Error> {
    let fix = c.string(0, if prefix { "prefix" } else { "suffix" }, method)?;
    c.done(method, 1)?;
    let out = if prefix {
        s.strip_prefix(fix.as_str())
    } else {
        s.strip_suffix(fix.as_str())
    };
    Ok(Value::from(out.unwrap_or(s)))
}

fn s_removeprefix(s: &str, c: &Call<'_, '_, '_>) -> Result<Value, Error> {
    removefix(s, c, "removeprefix", true)
}

fn s_removesuffix(s: &str, c: &Call<'_, '_, '_>) -> Result<Value, Error> {
    removefix(s, c, "removesuffix", false)
}

fn affix(s: &str, c: &Call<'_, '_, '_>, method: &str, start: bool) -> Result<Value, Error> {
    let probe = c.required(0, if start { "prefix" } else { "suffix" }, method)?;
    let (w, _) = window(s, c, 1)?;
    c.done(method, 3)?;
    let test = |p: &str| {
        if start {
            w.starts_with(p)
        } else {
            w.ends_with(p)
        }
    };
    // Python accepts a tuple of candidates as well as one string.
    let hit = match probe.as_str() {
        Some(p) => test(p),
        None => {
            let mut any = false;
            for p in probe.try_iter().map_err(|_| {
                type_error(format!("{method} first arg must be str or a tuple of str"))
            })? {
                let p = p.as_str().ok_or_else(|| {
                    type_error(format!("{method} first arg must be str or a tuple of str"))
                })?;
                any |= test(p);
            }
            any
        }
    };
    Ok(Value::from(hit))
}

fn s_startswith(s: &str, c: &Call<'_, '_, '_>) -> Result<Value, Error> {
    affix(s, c, "startswith", true)
}

fn s_endswith(s: &str, c: &Call<'_, '_, '_>) -> Result<Value, Error> {
    affix(s, c, "endswith", false)
}

/// `find`/`rfind`/`index`/`rindex`: the character index, or -1 / ValueError.
fn locate(
    s: &str,
    c: &Call<'_, '_, '_>,
    method: &str,
    reverse: bool,
    raise: bool,
) -> Result<Value, Error> {
    let sub = c.string(0, "sub", method)?;
    let (w, offset) = window(s, c, 1)?;
    c.done(method, 3)?;
    let at = if reverse {
        w.rfind(sub.as_str())
    } else {
        w.find(sub.as_str())
    };
    match at {
        Some(byte) => Ok(Value::from((offset + char_index(w, byte)) as i64)),
        None if raise => Err(value_error("substring not found")),
        None => Ok(Value::from(-1i64)),
    }
}

fn s_find(s: &str, c: &Call<'_, '_, '_>) -> Result<Value, Error> {
    locate(s, c, "find", false, false)
}

fn s_rfind(s: &str, c: &Call<'_, '_, '_>) -> Result<Value, Error> {
    locate(s, c, "rfind", true, false)
}

fn s_index(s: &str, c: &Call<'_, '_, '_>) -> Result<Value, Error> {
    locate(s, c, "index", false, true)
}

fn s_rindex(s: &str, c: &Call<'_, '_, '_>) -> Result<Value, Error> {
    locate(s, c, "rindex", true, true)
}

fn s_count(s: &str, c: &Call<'_, '_, '_>) -> Result<Value, Error> {
    let sub = c.string(0, "sub", "count")?;
    let (w, _) = window(s, c, 1)?;
    c.done("count", 3)?;
    Ok(Value::from(if sub.is_empty() {
        w.chars().count() + 1
    } else {
        w.matches(sub.as_str()).count()
    }))
}

/// `str.split`/`rsplit`: no separator splits on whitespace runs and drops
/// empties; a separator splits exactly.
fn split_with(s: &str, c: &Call<'_, '_, '_>, method: &str, reverse: bool) -> Result<Value, Error> {
    let sep = match c.get(0, "sep")? {
        Some(v) if v.is_none() => None,
        Some(v) => Some(
            v.as_str()
                .map(str::to_string)
                .ok_or_else(|| type_error("must be str or None"))?,
        ),
        None => None,
    };
    let maxsplit = c.opt_int(1, "maxsplit")?.unwrap_or(-1);
    c.done(method, 2)?;
    let limit = (maxsplit >= 0).then_some(maxsplit as usize);
    let mut out: Vec<Value> = Vec::new();
    match sep.as_deref() {
        Some("") => return Err(value_error("empty separator")),
        Some(sep) => match (limit, reverse) {
            (Some(n), false) => out.extend(s.splitn(n + 1, sep).map(Value::from)),
            (Some(n), true) => {
                let mut parts: Vec<Value> = s.rsplitn(n + 1, sep).map(Value::from).collect();
                parts.reverse();
                out = parts;
            }
            (None, _) => out.extend(s.split(sep).map(Value::from)),
        },
        None => {
            let words: Vec<&str> = s.split_whitespace().collect();
            match limit {
                Some(n) if n < words.len() => {
                    if reverse {
                        // The first len-n words stay joined, as written.
                        let mut rest = s.trim_end();
                        let mut tail: Vec<Value> = Vec::new();
                        for _ in 0..n {
                            let cut = rest.rfind(char::is_whitespace).map_or(0, |i| i);
                            tail.push(Value::from(rest[cut..].trim_start()));
                            rest = rest[..cut].trim_end();
                        }
                        if !rest.is_empty() {
                            out.push(Value::from(rest));
                        }
                        out.extend(tail.into_iter().rev());
                    } else {
                        let mut rest = s.trim_start();
                        for _ in 0..n {
                            let cut = rest.find(char::is_whitespace).unwrap_or(rest.len());
                            out.push(Value::from(&rest[..cut]));
                            rest = rest[cut..].trim_start();
                        }
                        if !rest.is_empty() {
                            out.push(Value::from(rest));
                        }
                    }
                }
                _ => out.extend(words.into_iter().map(Value::from)),
            }
        }
    }
    Ok(Value::from(out))
}

fn s_split(s: &str, c: &Call<'_, '_, '_>) -> Result<Value, Error> {
    split_with(s, c, "split", false)
}

fn s_rsplit(s: &str, c: &Call<'_, '_, '_>) -> Result<Value, Error> {
    split_with(s, c, "rsplit", true)
}

/// Python's line boundaries for `splitlines`.
fn is_line_break(ch: char) -> bool {
    matches!(
        ch,
        '\n' | '\r'
            | '\u{0b}'
            | '\u{0c}'
            | '\u{1c}'
            | '\u{1d}'
            | '\u{1e}'
            | '\u{85}'
            | '\u{2028}'
            | '\u{2029}'
    )
}

fn s_splitlines(s: &str, c: &Call<'_, '_, '_>) -> Result<Value, Error> {
    let keep = c.get(0, "keepends")?.is_some_and(|v| v.is_true());
    c.done("splitlines", 1)?;
    let mut out = Vec::new();
    let mut start = 0;
    let mut iter = s.char_indices().peekable();
    while let Some((i, ch)) = iter.next() {
        if is_line_break(ch) {
            let mut end = i + ch.len_utf8();
            if ch == '\r' {
                if let Some(&(j, '\n')) = iter.peek() {
                    iter.next();
                    end = j + 1;
                }
            }
            out.push(Value::from(if keep {
                &s[start..end]
            } else {
                &s[start..i]
            }));
            start = end;
        }
    }
    if start < s.len() {
        out.push(Value::from(&s[start..]));
    }
    Ok(Value::from(out))
}

fn partition_with(
    s: &str,
    c: &Call<'_, '_, '_>,
    method: &str,
    reverse: bool,
) -> Result<Value, Error> {
    let sep = c.string(0, "sep", method)?;
    c.done(method, 1)?;
    if sep.is_empty() {
        return Err(value_error("empty separator"));
    }
    let at = if reverse {
        s.rfind(sep.as_str())
    } else {
        s.find(sep.as_str())
    };
    let parts = match at {
        Some(i) => [&s[..i], &s[i..i + sep.len()], &s[i + sep.len()..]],
        None if reverse => ["", "", s],
        None => [s, "", ""],
    };
    Ok(Value::from(
        parts.iter().map(|p| Value::from(*p)).collect::<Vec<_>>(),
    ))
}

fn s_partition(s: &str, c: &Call<'_, '_, '_>) -> Result<Value, Error> {
    partition_with(s, c, "partition", false)
}

fn s_rpartition(s: &str, c: &Call<'_, '_, '_>) -> Result<Value, Error> {
    partition_with(s, c, "rpartition", true)
}

/// Where padding goes.
#[derive(Clone, Copy)]
pub(super) enum Align {
    Left,
    Right,
    Center,
}

/// `s` padded with `fill` to `width` characters; borrowed when already as
/// wide. The pad is checked against `cap` before anything is allocated.
pub(super) fn pad<'s>(
    s: &'s str,
    width: usize,
    fill: char,
    align: Align,
    cap: usize,
    method: &str,
) -> Result<Cow<'s, str>, Error> {
    let len = s.chars().take(width).count();
    if len >= width {
        return Ok(Cow::Borrowed(s));
    }
    let marg = width - len;
    let bytes = s.len() + marg * fill.len_utf8();
    if bytes > cap {
        return Err(too_large(method, bytes, cap));
    }
    let left = match align {
        Align::Left => 0,
        Align::Right => marg,
        // CPython's rule: the odd character goes left only when the width
        // is odd too.
        Align::Center => marg / 2 + (marg & width & 1),
    };
    let mut out = String::with_capacity(bytes);
    out.extend(std::iter::repeat_n(fill, left));
    out.push_str(s);
    out.extend(std::iter::repeat_n(fill, marg - left));
    Ok(Cow::Owned(out))
}

fn justify(s: &str, c: &Call<'_, '_, '_>, method: &str, align: Align) -> Result<Value, Error> {
    let width = c.opt_int(0, "width")?.ok_or_else(|| {
        Error::new(
            ErrorKind::MissingArgument,
            format!("{method}() missing required argument 'width'"),
        )
    })?;
    let fill = match c.get(1, "fillchar")? {
        Some(v) => {
            let f = v.as_str().unwrap_or_default();
            let mut it = f.chars();
            match (it.next(), it.next()) {
                (Some(ch), None) => ch,
                _ => {
                    return Err(type_error(
                        "The fill character must be exactly one character long",
                    ))
                }
            }
        }
        None => ' ',
    };
    c.done(method, 2)?;
    let width = width.max(0) as usize;
    Ok(Value::from(
        pad(s, width, fill, align, c.cap(), method)?.into_owned(),
    ))
}

fn s_ljust(s: &str, c: &Call<'_, '_, '_>) -> Result<Value, Error> {
    justify(s, c, "ljust", Align::Left)
}

fn s_rjust(s: &str, c: &Call<'_, '_, '_>) -> Result<Value, Error> {
    justify(s, c, "rjust", Align::Right)
}

fn s_center(s: &str, c: &Call<'_, '_, '_>) -> Result<Value, Error> {
    justify(s, c, "center", Align::Center)
}

fn s_zfill(s: &str, c: &Call<'_, '_, '_>) -> Result<Value, Error> {
    let width = c.opt_int(0, "width")?.unwrap_or(0).max(0) as usize;
    c.done("zfill", 1)?;
    let len = s.chars().count();
    if len >= width {
        return Ok(Value::from(s));
    }
    if width - len + s.len() > c.cap() {
        return Err(too_large("zfill", width - len + s.len(), c.cap()));
    }
    let (sign, digits) = match s.as_bytes().first() {
        Some(b'+' | b'-') => s.split_at(1),
        _ => ("", s),
    };
    Ok(Value::from(format!(
        "{sign}{}{digits}",
        "0".repeat(width - len)
    )))
}

fn s_expandtabs(s: &str, c: &Call<'_, '_, '_>) -> Result<Value, Error> {
    let size = c.opt_int(0, "tabsize")?.unwrap_or(8).max(0) as usize;
    c.done("expandtabs", 1)?;
    let tabs = s.matches('\t').count();
    let bound = s.len() + tabs * size;
    if bound > c.cap() {
        return Err(too_large("expandtabs", bound, c.cap()));
    }
    let mut out = String::with_capacity(bound);
    let mut col = 0usize;
    for ch in s.chars() {
        match ch {
            '\t' => {
                if size > 0 {
                    let n = size - col % size;
                    out.extend(std::iter::repeat_n(' ', n));
                    col += n;
                }
            }
            '\n' | '\r' => {
                out.push(ch);
                col = 0;
            }
            _ => {
                out.push(ch);
                col += 1;
            }
        }
    }
    Ok(Value::from(out))
}

fn predicate(
    s: &str,
    c: &Call<'_, '_, '_>,
    method: &str,
    f: fn(char) -> bool,
) -> Result<Value, Error> {
    c.done(method, 0)?;
    Ok(Value::from(!s.is_empty() && s.chars().all(f)))
}

fn s_isalnum(s: &str, c: &Call<'_, '_, '_>) -> Result<Value, Error> {
    predicate(s, c, "isalnum", char::is_alphanumeric)
}

fn s_isalpha(s: &str, c: &Call<'_, '_, '_>) -> Result<Value, Error> {
    predicate(s, c, "isalpha", char::is_alphabetic)
}

/// Decimal digits. Python's category Nd; Rust has no category table, so
/// the decimal digits are those with a base-10 value: exact for ASCII and
/// for the other scripts' digits `to_digit` does not cover, approximate.
fn is_decimal(ch: char) -> bool {
    ch.is_ascii_digit() || (ch.is_numeric() && !is_superscript_or_fraction(ch) && !ch.is_ascii())
}

fn is_superscript_or_fraction(ch: char) -> bool {
    matches!(ch, '²' | '³' | '¹' | '¼' | '½' | '¾' | '\u{2070}'..='\u{2089}' | '\u{2150}'..='\u{218b}' | '\u{2460}'..='\u{24ff}')
}

fn s_isdecimal(s: &str, c: &Call<'_, '_, '_>) -> Result<Value, Error> {
    predicate(s, c, "isdecimal", is_decimal)
}

fn s_isdigit(s: &str, c: &Call<'_, '_, '_>) -> Result<Value, Error> {
    predicate(s, c, "isdigit", |ch| {
        is_decimal(ch)
            || matches!(ch, '²' | '³' | '¹' | '\u{2070}'..='\u{2079}' | '\u{2080}'..='\u{2089}')
    })
}

fn s_isnumeric(s: &str, c: &Call<'_, '_, '_>) -> Result<Value, Error> {
    predicate(s, c, "isnumeric", char::is_numeric)
}

fn s_isspace(s: &str, c: &Call<'_, '_, '_>) -> Result<Value, Error> {
    predicate(s, c, "isspace", char::is_whitespace)
}

fn s_isidentifier(s: &str, c: &Call<'_, '_, '_>) -> Result<Value, Error> {
    c.done("isidentifier", 0)?;
    let mut it = s.chars();
    let ok = match it.next() {
        Some(f) => {
            (f.is_alphabetic() || f == '_') && it.all(|ch| ch.is_alphanumeric() || ch == '_')
        }
        None => false,
    };
    Ok(Value::from(ok))
}

fn s_islower(s: &str, c: &Call<'_, '_, '_>) -> Result<Value, Error> {
    c.done("islower", 0)?;
    let cased = s.chars().any(|ch| ch.is_lowercase() || ch.is_uppercase());
    Ok(Value::from(cased && !s.chars().any(char::is_uppercase)))
}

fn s_isupper(s: &str, c: &Call<'_, '_, '_>) -> Result<Value, Error> {
    c.done("isupper", 0)?;
    let cased = s.chars().any(|ch| ch.is_lowercase() || ch.is_uppercase());
    Ok(Value::from(cased && !s.chars().any(char::is_lowercase)))
}

/// Python's `istitle`: uppercase only after uncased characters, lowercase
/// only after cased ones, and at least one cased character.
fn s_istitle(s: &str, c: &Call<'_, '_, '_>) -> Result<Value, Error> {
    c.done("istitle", 0)?;
    let (mut prev_cased, mut any) = (false, false);
    for ch in s.chars() {
        if ch.is_uppercase() {
            if prev_cased {
                return Ok(Value::from(false));
            }
            prev_cased = true;
            any = true;
        } else if ch.is_lowercase() {
            if !prev_cased {
                return Ok(Value::from(false));
            }
            prev_cased = true;
            any = true;
        } else {
            prev_cased = false;
        }
    }
    Ok(Value::from(any))
}

fn s_join(s: &str, c: &Call<'_, '_, '_>) -> Result<Value, Error> {
    let items = c.required(0, "iterable", "join")?;
    c.done("join", 1)?;
    let mut out = String::new();
    for (i, item) in items
        .try_iter()
        .map_err(|_| type_error("can only join an iterable"))?
        .enumerate()
    {
        if i > 0 {
            out.push_str(s);
        }
        let piece = item.as_str().ok_or_else(|| {
            type_error(format!(
                "sequence item {i}: expected str instance, {} found",
                item.kind()
            ))
        })?;
        out.push_str(piece);
    }
    Ok(Value::from(out))
}

fn s_encode(s: &str, c: &Call<'_, '_, '_>) -> Result<Value, Error> {
    let enc = c.get(0, "encoding")?;
    c.done("encode", 2)?;
    match enc.as_ref().and_then(Value::as_str) {
        None | Some("utf-8" | "utf8" | "UTF-8") => Ok(Value::from_bytes(s.as_bytes().to_vec())),
        Some(other) => Err(value_error(format!(
            "unsupported encoding: {other} (only utf-8)"
        ))),
    }
}

// ---------------------------------------------------------------------------
// str.format, with the format-spec mini-language
// ---------------------------------------------------------------------------

/// A parsed `[[fill]align][sign][#][0][width][,][.precision][type]`.
#[derive(Default)]
pub(super) struct Spec {
    pub(super) fill: Option<char>,
    pub(super) align: Option<char>,
    pub(super) sign: Option<char>,
    pub(super) alt: bool,
    pub(super) zero: bool,
    pub(super) width: usize,
    pub(super) comma: bool,
    pub(super) precision: Option<usize>,
    pub(super) ty: Option<char>,
}

fn parse_spec(spec: &str) -> Result<Spec, Error> {
    let bad = || value_error(format!("Invalid format specifier '{spec}'"));
    let chars: Vec<char> = spec.chars().collect();
    let mut i = 0;
    let mut sp = Spec::default();
    let is_align = |c: char| matches!(c, '<' | '>' | '^' | '=');
    if chars.len() >= 2 && is_align(chars[1]) {
        sp.fill = Some(chars[0]);
        sp.align = Some(chars[1]);
        i = 2;
    } else if chars.first().copied().is_some_and(is_align) {
        sp.align = Some(chars[0]);
        i = 1;
    }
    if let Some(&c) = chars.get(i) {
        if matches!(c, '+' | '-' | ' ') {
            sp.sign = Some(c);
            i += 1;
        }
    }
    if chars.get(i) == Some(&'#') {
        sp.alt = true;
        i += 1;
    }
    if chars.get(i) == Some(&'0') {
        sp.zero = true;
        i += 1;
    }
    let start = i;
    while chars.get(i).is_some_and(char::is_ascii_digit) {
        i += 1;
    }
    if i > start {
        sp.width = chars[start..i]
            .iter()
            .collect::<String>()
            .parse()
            .map_err(|_| bad())?;
    }
    if chars.get(i) == Some(&',') {
        sp.comma = true;
        i += 1;
    }
    if chars.get(i) == Some(&'.') {
        i += 1;
        let start = i;
        while chars.get(i).is_some_and(char::is_ascii_digit) {
            i += 1;
        }
        if i == start {
            return Err(bad());
        }
        sp.precision = Some(
            chars[start..i]
                .iter()
                .collect::<String>()
                .parse()
                .map_err(|_| bad())?,
        );
    }
    if let Some(&c) = chars.get(i) {
        sp.ty = Some(c);
        i += 1;
    }
    if i != chars.len() {
        return Err(bad());
    }
    Ok(sp)
}

fn group_thousands(digits: &str) -> String {
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    let n = digits.len();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (n - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// One field formatted by its spec, as Python's `format(value, spec)`.
fn format_field(value: &Value, spec: &str, cap: usize) -> Result<String, Error> {
    if spec.is_empty() {
        return Ok(value.to_string());
    }
    render_spec(value, &parse_spec(spec)?, cap)
}

/// The one formatting core: a value rendered by a parsed spec. `str.format`
/// and `%` formatting both parse their own syntax into a [`Spec`] and come
/// here, so the two agree on every conversion they share.
pub(super) fn render_spec(value: &Value, sp: &Spec, cap: usize) -> Result<String, Error> {
    if sp.width > cap || sp.precision.is_some_and(|p| p > cap) {
        return Err(too_large("format", sp.width, cap));
    }
    let is_num = value.kind() == ValueKind::Number;
    let is_int = minijinja::tests::is_integer(value);
    let ty = sp.ty;
    let mismatch = || {
        value_error(format!(
            "Unknown format code '{}' for object of type '{}'",
            ty.unwrap_or('s'),
            if is_num {
                if is_int {
                    "int"
                } else {
                    "float"
                }
            } else {
                "str"
            }
        ))
    };
    let (mut sign, mut body): (&str, String) = match ty {
        None | Some('s') if !is_num => {
            if sp.sign.is_some() || sp.comma {
                return Err(value_error("Sign not allowed in string format specifier"));
            }
            let s = value.to_string();
            let s = match sp.precision {
                Some(p) => s.chars().take(p).collect(),
                None => s,
            };
            ("", s)
        }
        Some('s') => return Err(mismatch()),
        Some('d' | 'x' | 'X' | 'o' | 'b') | None if is_int => {
            let n = i128::try_from(value.clone()).map_err(|_| mismatch())?;
            let mag = n.unsigned_abs();
            let digits = match ty {
                Some('x') => format!("{mag:x}"),
                Some('X') => format!("{mag:X}"),
                Some('o') => format!("{mag:o}"),
                Some('b') => format!("{mag:b}"),
                _ => mag.to_string(),
            };
            let prefix = match (sp.alt, ty) {
                (true, Some('x')) => "0x",
                (true, Some('X')) => "0X",
                (true, Some('o')) => "0o",
                (true, Some('b')) => "0b",
                _ => "",
            };
            let digits = if sp.comma {
                group_thousands(&digits)
            } else {
                digits
            };
            (if n < 0 { "-" } else { "" }, format!("{prefix}{digits}"))
        }
        Some('e' | 'E') if is_num => {
            let x = f64::try_from(value.clone()).map_err(|_| mismatch())?;
            let p = sp.precision.unwrap_or(6);
            // Rust writes `1.2345e3`; Python `1.2345e+03`.
            let raw = format!("{:.*e}", p, x.abs());
            let (mant, exp) = raw.split_once('e').unwrap_or((&raw, "0"));
            let exp: i32 = exp.parse().unwrap_or(0);
            let e = if ty == Some('E') { 'E' } else { 'e' };
            let sign = if exp < 0 { '-' } else { '+' };
            (
                if x.is_sign_negative() && x != 0.0 {
                    "-"
                } else {
                    ""
                },
                format!("{mant}{e}{sign}{:02}", exp.abs()),
            )
        }
        Some('f' | 'F' | '%') | None if is_num => {
            let x = f64::try_from(value.clone()).map_err(|_| mismatch())?;
            let (x, pct) = if ty == Some('%') {
                (x * 100.0, "%")
            } else {
                (x, "")
            };
            let p = sp
                .precision
                .unwrap_or(if ty.is_none() { usize::MAX } else { 6 });
            let text = if p == usize::MAX {
                value.to_string()
            } else {
                format!("{:.*}", p, x.abs())
            };
            let text = text.trim_start_matches('-').to_string();
            let text = if sp.comma {
                match text.split_once('.') {
                    Some((i, f)) => format!("{}.{f}", group_thousands(i)),
                    None => group_thousands(&text),
                }
            } else {
                text
            };
            (
                if x.is_sign_negative() && x != 0.0 {
                    "-"
                } else {
                    ""
                },
                format!("{text}{pct}"),
            )
        }
        _ => return Err(mismatch()),
    };
    if sign.is_empty() && is_num {
        sign = match sp.sign {
            Some('+') => "+",
            Some(' ') => " ",
            _ => "",
        };
    }
    let len = sign.chars().count() + body.chars().count();
    if len < sp.width {
        let marg = sp.width - len;
        let (fill, align) = match (sp.fill, sp.align, sp.zero) {
            (f, Some(a), _) => (f.unwrap_or(' '), a),
            (None, None, true) if is_num => ('0', '='),
            (_, None, _) => (' ', if is_num { '>' } else { '<' }),
        };
        let pad = |n: usize| -> String { std::iter::repeat_n(fill, n).collect() };
        body = match align {
            '<' => format!("{sign}{body}{}", pad(marg)),
            '>' => format!("{}{sign}{body}", pad(marg)),
            '^' => format!("{}{sign}{body}{}", pad(marg / 2), pad(marg - marg / 2)),
            _ => format!("{sign}{}{body}", pad(marg)),
        };
        return Ok(body);
    }
    Ok(format!("{sign}{body}"))
}

/// Python's repr of a value, for `%r`: strings quoted as Python quotes
/// them, everything else as it renders.
fn py_repr(v: &Value) -> String {
    match v.as_str() {
        Some(s) => {
            let q = if s.contains('\'') && !s.contains('"') {
                '"'
            } else {
                '\''
            };
            let mut out = String::with_capacity(s.len() + 2);
            out.push(q);
            for c in s.chars() {
                match c {
                    '\\' => out.push_str("\\\\"),
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\t' => out.push_str("\\t"),
                    c if c == q => {
                        out.push('\\');
                        out.push(c);
                    }
                    c if (c as u32) < 0x20 || c == '\u{7f}' => {
                        let _ = std::fmt::Write::write_fmt(
                            &mut out,
                            format_args!("\\x{:02x}", c as u32),
                        );
                    }
                    c => out.push(c),
                }
            }
            out.push(q);
            out
        }
        None => v.to_string(),
    }
}

/// Python's `%` formatting -- `'%s-%05.2f' % (a, x)`, `'%(k)s' % d` -- for
/// the conversions Python offers on str, int and float (`s r d i u f F e E
/// x X o c %`), with flags, width and precision. Each field is rendered by
/// [`render_spec`], the core `str.format` uses; the result is bounded by
/// `cap`.
pub(super) fn printf(fmt: &str, args: &Value, cap: usize) -> Result<String, Error> {
    let type_err = |m: &str| Error::new(ErrorKind::InvalidOperation, m.to_string());
    let mapping = args.kind() == ValueKind::Map;
    let positional: Vec<Value> = match args.kind() {
        ValueKind::Seq | ValueKind::Iterable => args.try_iter()?.collect(),
        ValueKind::Map => Vec::new(),
        _ => vec![args.clone()],
    };
    let mut next = 0usize;
    let mut out = String::with_capacity(fmt.len() + 16);
    let mut chars = fmt.char_indices().peekable();
    while let Some((_, c)) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        let mut sp = Spec::default();
        // %(key)
        let mut keyed: Option<String> = None;
        if chars.peek().map(|&(_, c)| c) == Some('(') {
            chars.next();
            let mut key = String::new();
            loop {
                match chars.next() {
                    Some((_, ')')) => break,
                    Some((_, c)) => key.push(c),
                    None => return Err(value_error("incomplete format key")),
                }
            }
            keyed = Some(key);
        }
        let mut left = false;
        while let Some(&(_, f)) = chars.peek() {
            match f {
                '-' => left = true,
                '+' => sp.sign = Some('+'),
                ' ' => {
                    if sp.sign.is_none() {
                        sp.sign = Some(' ')
                    }
                }
                '#' => sp.alt = true,
                '0' => sp.zero = true,
                _ => break,
            }
            chars.next();
        }
        let num = |chars: &mut std::iter::Peekable<std::str::CharIndices<'_>>| -> Option<usize> {
            let mut n: Option<usize> = None;
            while let Some(&(_, d)) = chars.peek() {
                match d.to_digit(10) {
                    Some(v) => {
                        n = Some(n.unwrap_or(0).saturating_mul(10).saturating_add(v as usize));
                        chars.next();
                    }
                    None => break,
                }
            }
            n
        };
        if chars.peek().map(|&(_, c)| c) == Some('*') {
            return Err(value_error("'*' width or precision is not supported"));
        }
        sp.width = num(&mut chars).unwrap_or(0);
        if chars.peek().map(|&(_, c)| c) == Some('.') {
            chars.next();
            sp.precision = Some(num(&mut chars).unwrap_or(0));
        }
        let conv = chars
            .next()
            .map(|(_, c)| c)
            .ok_or_else(|| value_error("incomplete format"))?;
        if conv == '%' {
            out.push('%');
            continue;
        }
        let value = match &keyed {
            Some(k) => {
                if !mapping {
                    return Err(type_err("format requires a mapping"));
                }
                args.get_item(&Value::from(k.as_str()))
                    .ok()
                    .filter(|v| !v.is_undefined())
                    .ok_or_else(|| value_error(format!("KeyError: '{k}'")))?
            }
            None => {
                let v = positional
                    .get(next)
                    .cloned()
                    .ok_or_else(|| type_err("not enough arguments for format string"))?;
                next += 1;
                v
            }
        };
        if value.is_undefined() {
            return Err(Error::from(ErrorKind::UndefinedError));
        }
        if left {
            sp.align = Some('<');
            sp.zero = false;
        } else if !sp.zero {
            sp.align = Some('>');
        }
        let is_num = value.kind() == ValueKind::Number || value.kind() == ValueKind::Bool;
        let field = match conv {
            's' | 'r' | 'a' => {
                let text = if conv == 's' {
                    value.to_string()
                } else {
                    py_repr(&value)
                };
                sp.ty = Some('s');
                sp.sign = None;
                sp.zero = false;
                render_spec(&Value::from(text), &sp, cap)?
            }
            'd' | 'i' | 'u' => {
                if !is_num {
                    return Err(type_err(&format!(
                        "%{conv} format: a real number is required, not {}",
                        value.kind()
                    )));
                }
                let n = if minijinja::tests::is_integer(&value) {
                    i128::try_from(value.clone())?
                } else {
                    f64::try_from(value.clone())?.trunc() as i128
                };
                sp.ty = Some('d');
                let digits_min = sp.precision.take();
                if digits_min.is_some_and(|p| p > cap) {
                    return Err(too_large("%", digits_min.unwrap_or(0), cap));
                }
                let mut f = render_spec(&Value::from(n), &sp, cap)?;
                if let Some(p) = digits_min {
                    // `%.3d`: at least p digits.
                    let neg = n < 0;
                    let digits = n.unsigned_abs().to_string();
                    if digits.len() < p {
                        let body = format!(
                            "{}{}{digits}",
                            if neg { "-" } else { "" },
                            "0".repeat(p - digits.len())
                        );
                        f = render_spec(
                            &Value::from(body),
                            &Spec {
                                width: sp.width,
                                align: sp.align.or(Some('>')),
                                ty: Some('s'),
                                ..Spec::default()
                            },
                            cap,
                        )?;
                    }
                }
                f
            }
            'x' | 'X' | 'o' => {
                if !minijinja::tests::is_integer(&value) {
                    return Err(type_err(&format!(
                        "%{conv} format: an integer is required, not {}",
                        value.kind()
                    )));
                }
                sp.ty = Some(conv);
                render_spec(&value, &sp, cap)?
            }
            'f' | 'F' | 'e' | 'E' => {
                if !is_num {
                    return Err(type_err("must be real number, not str"));
                }
                sp.ty = Some(conv);
                sp.precision = Some(sp.precision.unwrap_or(6));
                let x = f64::try_from(value.clone())?;
                render_spec(&Value::from(x), &sp, cap)?
            }
            'c' => {
                let ch = match value.as_str() {
                    Some(s) if s.chars().count() == 1 => s.to_string(),
                    _ => {
                        let n = u32::try_from(value.clone())
                            .map_err(|_| type_err("%c requires int or char"))?;
                        char::from_u32(n)
                            .ok_or_else(|| value_error("%c arg not in range"))?
                            .to_string()
                    }
                };
                sp.ty = Some('s');
                render_spec(&Value::from(ch), &sp, cap)?
            }
            other => {
                return Err(value_error(format!(
                    "unsupported format character '{other}'"
                )))
            }
        };
        out.push_str(&field);
        if out.len() > cap {
            return Err(too_large("%", out.len(), cap));
        }
    }
    if !mapping && next < positional.len() {
        return Err(type_err(
            "not all arguments converted during string formatting",
        ));
    }
    Ok(out)
}

/// Python's `str.format`: `{}`, `{0}`, `{name}`, an optional `:spec`, and
/// `{{`/`}}`. Conversions (`!r`) and field indexing (`{0.x}`, `{0[k]}`) are
/// refused rather than ignored.
fn s_format(s: &str, c: &Call<'_, '_, '_>) -> Result<Value, Error> {
    let cap = c.cap();
    let mut out = String::with_capacity(s.len());
    let mut auto = 0usize;
    let mut rest = s;
    while let Some(i) = rest.find(['{', '}']) {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        if tail.starts_with("{{") || tail.starts_with("}}") {
            out.push(tail.as_bytes()[0] as char);
            rest = &tail[2..];
            continue;
        }
        if tail.starts_with('}') {
            return Err(value_error("Single '}' encountered in format string"));
        }
        let end = tail
            .find('}')
            .ok_or_else(|| value_error("Single '{' encountered in format string"))?;
        let field = &tail[1..end];
        let (name, spec) = field.split_once(':').unwrap_or((field, ""));
        if name.contains(['!', '[', '.']) {
            return Err(value_error(format!(
                "format field '{{{field}}}': conversions and indexing are not supported"
            )));
        }
        let value = if name.is_empty() {
            auto += 1;
            c.args().get(auto - 1).cloned()
        } else if let Ok(n) = name.parse::<usize>() {
            c.args().get(n).cloned()
        } else {
            match &c.kwargs {
                Some(k) => Some(k.get::<Value>(name).map_err(|_| {
                    Error::new(
                        ErrorKind::MissingArgument,
                        format!("format: no argument '{name}'"),
                    )
                })?),
                None => None,
            }
        }
        .ok_or_else(|| value_error("Replacement index out of range for positional args tuple"))?;
        if value.is_undefined() {
            return Err(Error::from(ErrorKind::UndefinedError));
        }
        out.push_str(&format_field(&value, spec, cap)?);
        if out.len() > cap {
            return Err(too_large("format", out.len(), cap));
        }
        rest = &tail[end + 1..];
    }
    out.push_str(rest);
    if let Some(k) = &c.kwargs {
        // Unused keywords are allowed by Python; mark them used.
        for key in k.args() {
            let _ = k.get::<Value>(key);
        }
    }
    Ok(Value::from(out))
}

// ---------------------------------------------------------------------------
// Sequences, mappings, numbers
// ---------------------------------------------------------------------------

fn q_count(v: &Value, c: &Call<'_, '_, '_>) -> Result<Value, Error> {
    let x = c.required(0, "value", "count")?;
    c.done("count", 1)?;
    Ok(Value::from(v.try_iter()?.filter(|i| *i == x).count()))
}

fn q_index(v: &Value, c: &Call<'_, '_, '_>) -> Result<Value, Error> {
    let x = c.required(0, "value", "index")?;
    let items: Vec<Value> = v.try_iter()?.collect();
    let (a, b) = bounds(items.len(), c.opt_int(1, "start")?, c.opt_int(2, "stop")?);
    c.done("index", 3)?;
    (a..b.max(a))
        .find(|&i| items[i] == x)
        .map(|i| Value::from(i as i64))
        .ok_or_else(|| value_error(format!("{x} is not in list")))
}

/// A copy of any sequence, as a list the template may change.
fn q_copy(v: &Value, c: &Call<'_, '_, '_>) -> Result<Value, Error> {
    c.done("copy", 0)?;
    Ok(super::containers::new_list(v.try_iter()?.collect()))
}

fn m_items(v: &Value, c: &Call<'_, '_, '_>) -> Result<Value, Error> {
    c.done("items", 0)?;
    minijinja::filters::items(v)
}

fn pairs_at(v: &Value, idx: usize) -> Result<Value, Error> {
    let pairs = minijinja::filters::items(v)?;
    Ok(Value::from(
        pairs
            .try_iter()?
            .filter_map(|pair| pair.get_item_by_index(idx).ok())
            .collect::<Vec<_>>(),
    ))
}

fn m_keys(v: &Value, c: &Call<'_, '_, '_>) -> Result<Value, Error> {
    c.done("keys", 0)?;
    pairs_at(v, 0)
}

fn m_values(v: &Value, c: &Call<'_, '_, '_>) -> Result<Value, Error> {
    c.done("values", 0)?;
    pairs_at(v, 1)
}

/// `dict.get(key, default=None)`.
fn m_get(v: &Value, c: &Call<'_, '_, '_>) -> Result<Value, Error> {
    let key = c.required(0, "key", "get")?;
    let default = c.get(1, "default")?;
    c.done("get", 2)?;
    let found = v.get_item(&key).ok().filter(|x| !x.is_undefined());
    Ok(found.or(default).unwrap_or_else(|| Value::from(())))
}

/// A copy of any mapping, as a dict the template may change.
fn m_copy(v: &Value, c: &Call<'_, '_, '_>) -> Result<Value, Error> {
    c.done("copy", 0)?;
    let mut pairs = Vec::new();
    for key in v.try_iter()? {
        let val = v.get_item(&key)?;
        pairs.push((key, val));
    }
    Ok(super::containers::new_dict(pairs))
}

fn n_is_integer(v: &Value, c: &Call<'_, '_, '_>) -> Result<Value, Error> {
    c.done("is_integer", 0)?;
    if minijinja::tests::is_integer(v) {
        return Ok(Value::from(true));
    }
    let x = f64::try_from(v.clone())?;
    Ok(Value::from(x.is_finite() && x.fract() == 0.0))
}

fn n_bit_length(v: &Value, c: &Call<'_, '_, '_>) -> Result<Value, Error> {
    c.done("bit_length", 0)?;
    if !minijinja::tests::is_integer(v) {
        return Err(Error::from(ErrorKind::UnknownMethod));
    }
    let n = i128::try_from(v.clone())?;
    Ok(Value::from(128 - n.unsigned_abs().leading_zeros()))
}

fn n_conjugate(v: &Value, c: &Call<'_, '_, '_>) -> Result<Value, Error> {
    c.done("conjugate", 0)?;
    Ok(v.clone())
}

/// `(numerator, denominator)` in lowest terms, as Python gives it.
fn n_as_integer_ratio(v: &Value, c: &Call<'_, '_, '_>) -> Result<Value, Error> {
    c.done("as_integer_ratio", 0)?;
    if minijinja::tests::is_integer(v) {
        return Ok(Value::from(vec![v.clone(), Value::from(1)]));
    }
    let x = f64::try_from(v.clone())?;
    if !x.is_finite() {
        return Err(value_error(
            "cannot convert Infinity or NaN to integer ratio",
        ));
    }
    // An f64 is m * 2^e exactly; reduce by the common power of two.
    let bits = x.to_bits();
    let exp = ((bits >> 52) & 0x7ff) as i32;
    let frac = bits & ((1u64 << 52) - 1);
    let (mut m, mut e) = if exp == 0 {
        (frac as i128, -1074)
    } else {
        ((frac | (1u64 << 52)) as i128, exp - 1075)
    };
    if m == 0 {
        return Ok(Value::from(vec![Value::from(0), Value::from(1)]));
    }
    while m & 1 == 0 && e < 0 {
        m >>= 1;
        e += 1;
    }
    if x < 0.0 {
        m = -m;
    }
    let (num, den) = if e >= 0 {
        let shifted = m.checked_shl(e as u32).filter(|n| (n >> e) == m);
        match shifted {
            Some(n) => (n, 1i128),
            None => return Err(value_error("ratio too large")),
        }
    } else if e >= -126 {
        (m, 1i128 << (-e))
    } else {
        return Err(value_error("ratio too large"));
    };
    Ok(Value::from(vec![Value::from(num), Value::from(den)]))
}

/// Every method name, by receiver kind, for the fuzz target and the matrix.
pub fn method_names() -> impl Iterator<Item = (&'static str, &'static str)> {
    STR_METHODS
        .iter()
        .map(|(n, _)| ("str", *n))
        .chain(SEQ_METHODS.iter().map(|(n, _)| ("seq", *n)))
        .chain(MAP_METHODS.iter().map(|(n, _)| ("map", *n)))
        .chain(NUM_METHODS.iter().map(|(n, _)| ("number", *n)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sorted<T>(t: &[(&str, T)]) -> bool {
        t.windows(2).all(|w| w[0].0 < w[1].0)
    }

    #[test]
    fn every_table_is_sorted_and_unique() {
        assert!(sorted(STR_METHODS));
        assert!(sorted(SEQ_METHODS));
        assert!(sorted(MAP_METHODS));
        assert!(sorted(NUM_METHODS));
    }

    #[test]
    fn python_bounds() {
        assert_eq!(bounds(5, None, None), (0, 5));
        assert_eq!(bounds(5, Some(-2), None), (3, 5));
        assert_eq!(bounds(5, Some(-10), Some(100)), (0, 5));
        assert_eq!(bounds(5, Some(3), Some(1)), (3, 1));
    }

    #[test]
    fn format_specs() {
        let f = |v: Value, s: &str| format_field(&v, s, 1 << 20).unwrap();
        assert_eq!(f(Value::from(42), ">5"), "   42");
        assert_eq!(f(Value::from(42), "05d"), "00042");
        assert_eq!(f(Value::from(-42), "05d"), "-0042");
        assert_eq!(f(Value::from(1.23456), ".2f"), "1.23");
        assert_eq!(f(Value::from(1234567), ","), "1,234,567");
        assert_eq!(f(Value::from(255), "#x"), "0xff");
        assert_eq!(f(Value::from("ab"), "*^6"), "**ab**");
        assert_eq!(f(Value::from(0.5), ".0%"), "50%");
        assert_eq!(f(Value::from(7), "+d"), "+7");
        assert!(format_field(&Value::from("x"), "d", 1 << 20).is_err());
        assert!(format_field(&Value::from(1), "s", 1 << 20).is_err());
        assert!(format_field(&Value::from(1), "10000000", 100).is_err());
    }

    #[test]
    fn printf_matches_python() {
        let p = |f: &str, v: Value| printf(f, &v, 1 << 20).unwrap();
        assert_eq!(
            p("%5d|%-5d|%05d", Value::from(vec![Value::from(42); 3])),
            "   42|42   |00042"
        );
        assert_eq!(p("%.3d", Value::from(7)), "007");
        assert_eq!(p("%5.2f", Value::from(1.23456)), " 1.23");
        assert_eq!(p("%e", Value::from(12345.678)), "1.234568e+04");
        assert_eq!(p("%r", Value::from("it's")), "\"it's\"");
        assert_eq!(p("%r", Value::from("a\nb")), "'a\\nb'");
        assert_eq!(p("%%%s", Value::from("x")), "%x");
        assert_eq!(
            p("%c%c", Value::from(vec![Value::from(65), Value::from("b")])),
            "Ab"
        );
        assert!(printf("%s %s", &Value::from(vec![Value::from(1)]), 100).is_err());
        assert!(printf(
            "%s",
            &Value::from(vec![Value::from(1), Value::from(2)]),
            100
        )
        .is_err());
        assert!(printf("%d", &Value::from("x"), 100).is_err());
        assert!(printf("%9999d", &Value::from(1), 100).is_err());
    }

    #[test]
    fn ratio_is_exact() {
        let mut env = minijinja::Environment::new();
        env.set_unknown_method_callback(dispatch);
        let r = env.render_str("{{ 0.75.as_integer_ratio() }}", ()).unwrap();
        assert_eq!(r, "[3, 4]");
    }
}

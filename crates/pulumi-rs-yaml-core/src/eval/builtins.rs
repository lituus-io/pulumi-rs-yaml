// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

use std::borrow::Cow;

use base64::Engine;

use crate::ast::property::PropertyAccessor;
use crate::diag::Diagnostics;
use crate::eval::value::Value;

/// Safely converts an `f64` to `usize`, emitting a diagnostic on failure.
///
/// Rejects NaN, infinity, negative values, non-integer values, and values
/// exceeding `usize::MAX`.
fn checked_f64_to_usize(f: f64, diags: &mut Diagnostics, context: &str) -> Option<usize> {
    if f.is_nan() || f.is_infinite() || f < 0.0 || f.fract() != 0.0 || f > usize::MAX as f64 {
        diags.error(
            None,
            format!("{context} must be a non-negative integer, got {f}"),
            "",
        );
        return None;
    }
    Some(f as usize)
}

/// Extracts a `&str` from a `Value::String`, or emits a diagnostic.
fn expect_string<'a>(value: &'a Value<'_>, ctx: &str, diags: &mut Diagnostics) -> Option<&'a str> {
    match value {
        Value::String(s) => Some(s.as_ref()),
        _ => {
            diags.error(
                None,
                format!(
                    "argument to {} must be a string, got {}",
                    ctx,
                    value.type_name()
                ),
                "",
            );
            None
        }
    }
}

/// Extracts an `f64` from a `Value::Number`, or emits a diagnostic.
fn expect_number(value: &Value<'_>, ctx: &str, diags: &mut Diagnostics) -> Option<f64> {
    match value {
        Value::Number(n) => Some(*n),
        _ => {
            diags.error(
                None,
                format!(
                    "argument to {} must be a number, got {}",
                    ctx,
                    value.type_name()
                ),
                "",
            );
            None
        }
    }
}

/// Extracts a `&[Value]` from a `Value::List`, or emits a diagnostic.
fn expect_list<'a, 'src>(
    value: &'a Value<'src>,
    ctx: &str,
    diags: &mut Diagnostics,
) -> Option<&'a [Value<'src>]> {
    match value {
        Value::List(items) => Some(items.as_slice()),
        _ => {
            diags.error(
                None,
                format!(
                    "argument to {} must be a list, got {}",
                    ctx,
                    value.type_name()
                ),
                "",
            );
            None
        }
    }
}

/// Returns true if a value contains any Unknown values (recursively).
/// Unknown is contagious — any operation on Unknown should propagate Unknown.
pub fn has_unknown(val: &Value<'_>) -> bool {
    match val {
        Value::Unknown => true,
        Value::Secret(inner) => has_unknown(inner),
        Value::List(items) => items.iter().any(has_unknown),
        Value::Object(entries) => entries.iter().any(|(_, v)| has_unknown(v)),
        _ => false,
    }
}

/// Evaluates `fn::join` - joins a list of strings with a delimiter.
///
/// Arguments: [delimiter, list_of_strings]
pub fn eval_join<'src>(
    delimiter: &Value<'src>,
    values: &Value<'src>,
    diags: &mut Diagnostics,
) -> Option<Value<'src>> {
    if has_unknown(delimiter) || has_unknown(values) {
        return Some(Value::Unknown);
    }
    let delim = match delimiter {
        Value::String(s) => s.as_ref(),
        Value::Null => "",
        _ => {
            diags.error(
                None,
                format!("delimiter must be a string, not {}", delimiter.type_name()),
                "",
            );
            return None;
        }
    };

    let items = match values {
        Value::List(items) => items,
        _ => {
            diags.error(
                None,
                format!(
                    "the second argument to fn::join must be a list, found {}",
                    values.type_name()
                ),
                "",
            );
            return None;
        }
    };

    let mut strs: Vec<&str> = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        match item {
            Value::String(s) => strs.push(s.as_ref()),
            _ => {
                diags.error(
                    None,
                    format!(
                        "the second argument to fn::join must be a list of strings, found {} at index {}",
                        item.type_name(),
                        i
                    ),
                    "",
                );
                return None;
            }
        }
    }

    Some(Value::String(Cow::Owned(strs.join(delim))))
}

/// Evaluates `fn::split` - splits a string by a delimiter.
///
/// Arguments: [delimiter, source]
pub fn eval_split<'src>(
    delimiter: &Value<'src>,
    source: &Value<'src>,
    diags: &mut Diagnostics,
) -> Option<Value<'src>> {
    if has_unknown(delimiter) || has_unknown(source) {
        return Some(Value::Unknown);
    }
    let delim = match delimiter {
        Value::String(s) => s.as_ref(),
        _ => {
            diags.error(
                None,
                format!("Must be a string, not {}", delimiter.type_name()),
                "",
            );
            return None;
        }
    };

    let src = match source {
        Value::String(s) => s.as_ref(),
        _ => {
            diags.error(
                None,
                format!("Must be a string, not {}", source.type_name()),
                "",
            );
            return None;
        }
    };

    let parts: Vec<Value<'src>> = src
        .split(delim)
        .map(|s| Value::String(Cow::Owned(s.to_string())))
        .collect();

    Some(Value::List(parts))
}

/// Evaluates `fn::select` - selects an element from a list by index.
///
/// Arguments: [index, list]
pub fn eval_select<'src>(
    index: &Value<'src>,
    values: &Value<'src>,
    diags: &mut Diagnostics,
) -> Option<Value<'src>> {
    if has_unknown(index) || has_unknown(values) {
        return Some(Value::Unknown);
    }
    let idx = match index {
        Value::Number(n) => checked_f64_to_usize(*n, diags, "fn::select index")?,
        _ => {
            diags.error(
                None,
                format!("index must be a number, not {}", index.type_name()),
                "",
            );
            return None;
        }
    };

    let items = match values {
        Value::List(items) => items,
        _ => {
            diags.error(
                None,
                format!(
                    "the second argument to fn::select must be a list, found {}",
                    values.type_name()
                ),
                "",
            );
            return None;
        }
    };

    if idx >= items.len() {
        diags.error(
            None,
            format!(
                "list index {} out-of-bounds for list of length {}",
                idx,
                items.len()
            ),
            "",
        );
        return None;
    }

    Some(items[idx].clone())
}

/// Evaluates `fn::toJSON` - converts a value to its JSON representation.
pub fn eval_to_json<'src>(value: &Value<'src>, diags: &mut Diagnostics) -> Option<Value<'src>> {
    if has_unknown(value) {
        return Some(Value::Unknown);
    }
    let json = value.to_json();
    match serde_json::to_string(&json) {
        Ok(s) => Some(Value::String(Cow::Owned(s))),
        Err(e) => {
            diags.error(None, format!("failed to encode JSON: {}", e), "");
            None
        }
    }
}

/// Evaluates `fn::toBase64` - encodes a string to base64.
pub fn eval_to_base64<'src>(value: &Value<'src>, diags: &mut Diagnostics) -> Option<Value<'src>> {
    if has_unknown(value) {
        return Some(Value::Unknown);
    }
    let s = expect_string(value, "fn::toBase64", diags)?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(s.as_bytes());
    Some(Value::String(Cow::Owned(encoded)))
}

/// Evaluates `fn::fromBase64` - decodes a base64 string.
pub fn eval_from_base64<'src>(value: &Value<'src>, diags: &mut Diagnostics) -> Option<Value<'src>> {
    if has_unknown(value) {
        return Some(Value::Unknown);
    }
    let s = expect_string(value, "fn::fromBase64", diags)?;
    match base64::engine::general_purpose::STANDARD.decode(s.as_bytes()) {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(decoded) => Some(Value::String(Cow::Owned(decoded))),
            Err(_) => {
                diags.error(
                    None,
                    "fn::fromBase64 output is not a valid UTF-8 string".to_string(),
                    "",
                );
                None
            }
        },
        Err(e) => {
            diags.error(
                None,
                format!("fn::fromBase64 unable to decode {}, error: {}", s, e),
                "",
            );
            None
        }
    }
}

/// Evaluates `fn::secret` - wraps a value as secret.
pub fn eval_secret(value: Value<'_>) -> Value<'_> {
    if value.is_unknown() {
        return Value::Secret(Box::new(Value::Unknown));
    }
    Value::Secret(Box::new(value))
}

/// Evaluates `fn::readFile` - reads the contents of a file.
pub fn eval_read_file<'src>(
    value: &Value<'src>,
    cwd: &str,
    diags: &mut Diagnostics,
) -> Option<Value<'src>> {
    if has_unknown(value) {
        return Some(Value::Unknown);
    }
    let s = expect_string(value, "fn::readFile", diags)?;
    let path = if std::path::Path::new(s).is_absolute() {
        s.to_string()
    } else {
        std::path::Path::new(cwd)
            .join(s)
            .to_string_lossy()
            .into_owned()
    };
    match std::fs::read_to_string(&path) {
        Ok(contents) => Some(Value::String(Cow::Owned(contents))),
        Err(e) => {
            diags.error(
                None,
                format!("Error reading file at path {}: {}", path, e),
                "",
            );
            None
        }
    }
}

// =============================================================================
// Math builtins
// =============================================================================

/// Evaluates `fn::abs` - absolute value of a number.
pub fn eval_abs<'src>(value: &Value<'src>, diags: &mut Diagnostics) -> Option<Value<'src>> {
    if has_unknown(value) {
        return Some(Value::Unknown);
    }
    Some(Value::Number(expect_number(value, "fn::abs", diags)?.abs()))
}

/// Evaluates `fn::floor` - floor of a number.
pub fn eval_floor<'src>(value: &Value<'src>, diags: &mut Diagnostics) -> Option<Value<'src>> {
    if has_unknown(value) {
        return Some(Value::Unknown);
    }
    Some(Value::Number(
        expect_number(value, "fn::floor", diags)?.floor(),
    ))
}

/// Evaluates `fn::ceil` - ceiling of a number.
pub fn eval_ceil<'src>(value: &Value<'src>, diags: &mut Diagnostics) -> Option<Value<'src>> {
    if has_unknown(value) {
        return Some(Value::Unknown);
    }
    Some(Value::Number(
        expect_number(value, "fn::ceil", diags)?.ceil(),
    ))
}

/// Evaluates `fn::max` - maximum value in a list of numbers.
pub fn eval_max<'src>(value: &Value<'src>, diags: &mut Diagnostics) -> Option<Value<'src>> {
    if has_unknown(value) {
        return Some(Value::Unknown);
    }
    let items = expect_list(value, "fn::max", diags)?;
    if items.is_empty() {
        diags.error(None, "fn::max requires a non-empty list", "");
        return None;
    }
    let mut max_val = f64::NEG_INFINITY;
    for (i, item) in items.iter().enumerate() {
        match item {
            Value::Number(n) => {
                if *n > max_val {
                    max_val = *n;
                }
            }
            _ => {
                diags.error(
                    None,
                    format!(
                        "fn::max list element at index {} must be a number, got {}",
                        i,
                        item.type_name()
                    ),
                    "",
                );
                return None;
            }
        }
    }
    Some(Value::Number(max_val))
}

/// Evaluates `fn::min` - minimum value in a list of numbers.
pub fn eval_min<'src>(value: &Value<'src>, diags: &mut Diagnostics) -> Option<Value<'src>> {
    if has_unknown(value) {
        return Some(Value::Unknown);
    }
    let items = expect_list(value, "fn::min", diags)?;
    if items.is_empty() {
        diags.error(None, "fn::min requires a non-empty list", "");
        return None;
    }
    let mut min_val = f64::INFINITY;
    for (i, item) in items.iter().enumerate() {
        match item {
            Value::Number(n) => {
                if *n < min_val {
                    min_val = *n;
                }
            }
            _ => {
                diags.error(
                    None,
                    format!(
                        "fn::min list element at index {} must be a number, got {}",
                        i,
                        item.type_name()
                    ),
                    "",
                );
                return None;
            }
        }
    }
    Some(Value::Number(min_val))
}

// =============================================================================
// String builtins
// =============================================================================

/// Evaluates `fn::stringLen` - Unicode character count of a string.
pub fn eval_string_len<'src>(value: &Value<'src>, diags: &mut Diagnostics) -> Option<Value<'src>> {
    if has_unknown(value) {
        return Some(Value::Unknown);
    }
    let s = expect_string(value, "fn::stringLen", diags)?;
    Some(Value::Number(s.chars().count() as f64))
}

/// Evaluates `fn::substring` - extracts a substring using char-based indices.
pub fn eval_substring<'src>(
    source: &Value<'src>,
    start: &Value<'src>,
    length: &Value<'src>,
    diags: &mut Diagnostics,
) -> Option<Value<'src>> {
    if has_unknown(source) || has_unknown(start) || has_unknown(length) {
        return Some(Value::Unknown);
    }
    let s = match source {
        Value::String(s) => s.as_ref(),
        _ => {
            diags.error(
                None,
                format!(
                    "first argument to fn::substring must be a string, got {}",
                    source.type_name()
                ),
                "",
            );
            return None;
        }
    };
    let start_idx = match start {
        Value::Number(n) => checked_f64_to_usize(*n, diags, "fn::substring start index")?,
        _ => {
            diags.error(
                None,
                format!(
                    "second argument to fn::substring must be a number, got {}",
                    start.type_name()
                ),
                "",
            );
            return None;
        }
    };
    let len = match length {
        Value::Number(n) => checked_f64_to_usize(*n, diags, "fn::substring length")?,
        _ => {
            diags.error(
                None,
                format!(
                    "third argument to fn::substring must be a number, got {}",
                    length.type_name()
                ),
                "",
            );
            return None;
        }
    };
    let result: String = s.chars().skip(start_idx).take(len).collect();
    Some(Value::String(Cow::Owned(result)))
}

// =============================================================================
// Time builtins
// =============================================================================

/// Converts a Unix timestamp to (year, month, day, hour, minute, second).
/// Uses the Howard Hinnant civil date algorithm. No chrono dependency.
fn unix_to_civil(secs: i64) -> (i32, u32, u32, u32, u32, u32) {
    let day_secs = secs.rem_euclid(86400);
    let mut days = (secs - day_secs) / 86400;
    let hour = (day_secs / 3600) as u32;
    let minute = ((day_secs % 3600) / 60) as u32;
    let second = (day_secs % 60) as u32;

    // Days since 1970-01-01
    days += 719468; // shift to 0000-03-01
    let era = if days >= 0 { days } else { days - 146096 } / 146097;
    let doe = (days - era * 146097) as u32; // day of era [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };

    (year as i32, m, d, hour, minute, second)
}

/// Evaluates `fn::timeUtc` - current UTC time as ISO 8601 string.
pub fn eval_time_utc<'src>(_value: &Value<'src>, _diags: &mut Diagnostics) -> Option<Value<'src>> {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let (y, m, d, h, min, s) = unix_to_civil(secs);
    let formatted = format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, m, d, h, min, s);
    Some(Value::String(Cow::Owned(formatted)))
}

/// Evaluates `fn::timeUnix` - current Unix timestamp as a number.
pub fn eval_time_unix<'src>(_value: &Value<'src>, _diags: &mut Diagnostics) -> Option<Value<'src>> {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    Some(Value::Number(secs as f64))
}

/// Evaluates `fn::dateFormat` - formats current date/time with a strftime-style format string.
///
/// Supported format specifiers: `%Y`, `%m`, `%d`, `%H`, `%M`, `%S`, `%%`.
pub fn eval_date_format<'src>(value: &Value<'src>, diags: &mut Diagnostics) -> Option<Value<'src>> {
    if has_unknown(value) {
        return Some(Value::Unknown);
    }
    let fmt = expect_string(value, "fn::dateFormat", diags)?;

    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let (y, m, d, h, min, s) = unix_to_civil(secs);

    let mut result = String::new();
    let mut chars = fmt.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' {
            match chars.next() {
                Some('Y') => result.push_str(&format!("{:04}", y)),
                Some('m') => result.push_str(&format!("{:02}", m)),
                Some('d') => result.push_str(&format!("{:02}", d)),
                Some('H') => result.push_str(&format!("{:02}", h)),
                Some('M') => result.push_str(&format!("{:02}", min)),
                Some('S') => result.push_str(&format!("{:02}", s)),
                Some('%') => result.push('%'),
                Some(other) => {
                    result.push('%');
                    result.push(other);
                }
                None => result.push('%'),
            }
        } else {
            result.push(c);
        }
    }

    Some(Value::String(Cow::Owned(result)))
}

// =============================================================================
// UUID/Random builtins
// =============================================================================

/// The default alphabet: lowercase alphanumeric, which is what the
/// `random:RandomString` calls this replaces ask for (`upper: false`,
/// `special: false`) and what GCP resource names accept. It can therefore begin
/// with a digit, which is fine for a suffix -- the only way it is used -- but a
/// value used as a whole name where the provider demands a leading letter needs
/// an explicit `alphabet` or a prefix.
pub(crate) const DERIVE_ALPHABET: &str = "0123456789abcdefghijklmnopqrstuvwxyz";

/// Default output length for the scalar shorthand.
pub(crate) const DERIVE_DEFAULT_LEN: usize = 8;

/// Longest derived value. Resource names are short; a cap keeps a program from
/// asking the allocator for a value nobody can use, and makes the extension
/// loop below provably finite.
const MAX_DERIVE_LEN: usize = 64;

/// Most characters an alphabet may offer. Rejection sampling draws one byte per
/// character, so an alphabet larger than a byte could not be addressed without
/// changing the draw -- and an alphabet that big is not a naming scheme.
const MAX_DERIVE_ALPHABET: usize = 256;

/// Hard ceiling on digest extensions, so the draw cannot spin.
///
/// Each extension yields 32 fresh bytes and the worst acceptance rate over all
/// admissible alphabet sizes is just above one half, so 64 characters need four
/// or five extensions in expectation. This bound is several hundred times that:
/// it exists to make termination a fact rather than a probability.
const MAX_DERIVE_EXTENSIONS: u32 = 1024;

/// Derives the value, or says why it cannot.
///
/// The one implementation of the algorithm. `eval_derive_string` reaches it
/// with values from the evaluator and [`crate::literal_resolve`] reaches it
/// with values read statically off the AST; both must produce the same string
/// for the same arguments, so neither may own a copy of this.
///
/// `Err` carries the message a diagnostic would print. A static caller that
/// cannot act on it discards it and resolves to nothing rather than guessing.
pub(crate) fn derive(from: &str, length: usize, alphabet: &str) -> Result<String, String> {
    if length == 0 {
        return Err("fn::deriveString 'length' must be at least 1".to_string());
    }
    if length > MAX_DERIVE_LEN {
        return Err(format!(
            "fn::deriveString 'length' {length} exceeds maximum {MAX_DERIVE_LEN}"
        ));
    }

    // `n` is a character count, not a byte count, so a multi-byte alphabet
    // selects whole characters and can never split one.
    let n = alphabet.chars().count();
    if n == 0 {
        return Err("fn::deriveString 'alphabet' must not be empty".to_string());
    }
    if n > MAX_DERIVE_ALPHABET {
        return Err(format!(
            "fn::deriveString 'alphabet' has {n} characters, over the \
             {MAX_DERIVE_ALPHABET} this draw can address"
        ));
    }

    // Rejection sampling, not `byte % n`. For the default alphabet
    // `256 % 36 == 4`, so a plain modulo would favour four of its thirty-six
    // characters; discarding the short final block removes the bias. `limit` is
    // the largest multiple of `n` that fits in a byte.
    let limit = 256 - (256 % n);

    let mut out = String::with_capacity(length);
    let mut digest = crate::sha256::digest(from.as_bytes());
    let mut cursor = 0usize;
    let mut extension = 0u32;
    let mut chars = 0usize;

    while chars < length {
        if cursor == digest.len() {
            // The digest is spent; extend deterministically with a counter so
            // the sequence stays a pure function of the seed.
            extension += 1;
            if extension > MAX_DERIVE_EXTENSIONS {
                return Err(
                    "fn::deriveString could not draw enough unbiased characters".to_string()
                );
            }
            let mut seed = Vec::with_capacity(from.len() + 4);
            seed.extend_from_slice(from.as_bytes());
            seed.extend_from_slice(&extension.to_le_bytes());
            digest = crate::sha256::digest(&seed);
            cursor = 0;
        }
        let byte = usize::from(digest[cursor]);
        cursor += 1;
        if byte >= limit {
            continue;
        }
        // `n` and `length` are both bounded small, so walking to the character
        // costs less than the allocation a lookup table would need.
        out.push(
            alphabet
                .chars()
                .nth(byte % n)
                .expect("index is below the character count"),
        );
        chars += 1;
    }

    Ok(out)
}

/// Reads `fn::deriveString`'s arguments off an already-evaluated value.
///
/// Shared with the static resolver through [`derive`]; this half is only the
/// argument shape, which the two callers read from different places.
pub(crate) fn derive_args<'a>(
    value: &'a Value<'_>,
    diags: &mut Diagnostics,
) -> Option<(&'a str, usize, &'a str)> {
    match value {
        // Shorthand: the seed alone.
        Value::String(s) => Some((s.as_ref(), DERIVE_DEFAULT_LEN, DERIVE_ALPHABET)),
        Value::Object(entries) => {
            let mut from: Option<&str> = None;
            let mut length = DERIVE_DEFAULT_LEN;
            let mut alphabet = DERIVE_ALPHABET;
            for (key, val) in entries {
                match key.as_ref() {
                    "from" => from = Some(expect_string(val, "fn::deriveString 'from'", diags)?),
                    "length" => {
                        let n = expect_number(val, "fn::deriveString 'length'", diags)?;
                        length = checked_f64_to_usize(n, diags, "fn::deriveString 'length'")?;
                    }
                    "alphabet" => {
                        alphabet = expect_string(val, "fn::deriveString 'alphabet'", diags)?;
                    }
                    // Named rather than ignored: a silently defaulted typo is
                    // how a stack gets a name nobody intended.
                    other => {
                        diags.error(
                            None,
                            format!("fn::deriveString has no argument named '{other}'"),
                            "Valid arguments are 'from', 'length' and 'alphabet'.",
                        );
                        return None;
                    }
                }
            }
            match from {
                Some(f) => Some((f, length, alphabet)),
                None => {
                    diags.error(
                        None,
                        "fn::deriveString requires a 'from' argument".to_string(),
                        "Give it the seed to derive from, e.g. `from: my-service`.",
                    );
                    None
                }
            }
        }
        _ => {
            diags.error(
                None,
                format!(
                    "argument to fn::deriveString must be a string or an object, got {}",
                    value.type_name()
                ),
                "",
            );
            None
        }
    }
}

/// Evaluates `fn::deriveString` - a stable value computed from a seed.
///
/// Unlike `fn::randomString` this is a pure function of its argument, so it is
/// known at preview, identical on every evaluation, and resolvable statically
/// (see [`crate::literal_resolve`]). That is the entire point: a name a policy
/// check, an import and a graph export can all read before anything is
/// deployed.
pub fn eval_derive_string<'src>(
    value: &Value<'src>,
    diags: &mut Diagnostics,
) -> Option<Value<'src>> {
    if has_unknown(value) {
        return Some(Value::Unknown);
    }
    let (from, length, alphabet) = derive_args(value, diags)?;
    match derive(from, length, alphabet) {
        Ok(s) => Some(Value::String(Cow::Owned(s))),
        Err(msg) => {
            diags.error(None, msg, "");
            None
        }
    }
}

/// Evaluates `fn::uuid` - generates a random UUID v4.
pub fn eval_uuid<'src>(_value: &Value<'src>, _diags: &mut Diagnostics) -> Option<Value<'src>> {
    let id = uuid::Uuid::new_v4().to_string();
    Some(Value::String(Cow::Owned(id)))
}

/// Evaluates `fn::randomString` - generates a random alphanumeric string.
pub fn eval_random_string<'src>(
    value: &Value<'src>,
    diags: &mut Diagnostics,
) -> Option<Value<'src>> {
    let length = match value {
        Value::Number(n) => checked_f64_to_usize(*n, diags, "fn::randomString length")?,
        _ => {
            diags.error(
                None,
                format!(
                    "argument to fn::randomString must be a number, got {}",
                    value.type_name()
                ),
                "",
            );
            return None;
        }
    };

    const MAX_RANDOM_STRING_LEN: usize = 1_048_576;
    if length > MAX_RANDOM_STRING_LEN {
        diags.error(
            None,
            format!(
                "fn::randomString length {} exceeds maximum {}",
                length, MAX_RANDOM_STRING_LEN
            ),
            "",
        );
        return None;
    }

    use rand::Rng;
    const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::thread_rng();
    let result: String = (0..length)
        .map(|_| {
            let idx = rng.gen_range(0..CHARSET.len());
            CHARSET[idx] as char
        })
        .collect();
    Some(Value::String(Cow::Owned(result)))
}

/// Evaluates property access on a value.
///
/// Given a value and a chain of property accessors (names and indices),
/// traverses the value by reference to resolve the access chain.
/// Only the final leaf value is cloned, eliminating intermediate allocations.
pub fn eval_property_access<'src>(
    value: &Value<'src>,
    accessors: &[PropertyAccessor<'_>],
    diags: &mut Diagnostics,
) -> Option<Value<'src>> {
    let mut current: &Value<'src> = value;

    for accessor in accessors {
        match accessor {
            PropertyAccessor::Name(name) | PropertyAccessor::StringSubscript(name) => match current
            {
                Value::Object(entries) => {
                    match entries.iter().find(|(k, _)| k.as_ref() == name.as_ref()) {
                        Some((_, v)) => current = v,
                        None => return Some(Value::Null),
                    }
                }
                Value::Secret(inner) => {
                    let result =
                        eval_property_access(inner, std::slice::from_ref(accessor), diags)?;
                    return Some(Value::Secret(Box::new(result)));
                }
                Value::Null | Value::Unknown => return Some(current.clone()),
                _ => {
                    diags.error(
                        None,
                        format!(
                            "cannot access .{} on {}",
                            name.as_ref(),
                            current.type_name()
                        ),
                        "",
                    );
                    return None;
                }
            },
            PropertyAccessor::IntSubscript(idx) => {
                let i = *idx;
                match current {
                    Value::List(items) => {
                        if i < 0 || (i as usize) >= items.len() {
                            diags.error(
                                None,
                                format!(
                                    "index {} out of bounds for list of length {}",
                                    i,
                                    items.len()
                                ),
                                "",
                            );
                            return None;
                        }
                        current = &items[i as usize];
                    }
                    Value::Secret(inner) => {
                        let result =
                            eval_property_access(inner, std::slice::from_ref(accessor), diags)?;
                        return Some(Value::Secret(Box::new(result)));
                    }
                    Value::Null | Value::Unknown => return Some(current.clone()),
                    _ => {
                        diags.error(
                            None,
                            format!("cannot index into {}", current.type_name()),
                            "",
                        );
                        return None;
                    }
                }
            }
        }
    }

    Some(current.clone()) // Only the leaf is cloned
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(val: &str) -> Value<'static> {
        Value::String(Cow::Owned(val.to_string()))
    }

    fn n(val: f64) -> Value<'static> {
        Value::Number(val)
    }

    #[test]
    fn test_join_basic() {
        let mut diags = Diagnostics::new();
        let delim = s(",");
        let items = Value::List(vec![s("a"), s("b"), s("c")]);
        let result = eval_join(&delim, &items, &mut diags).unwrap();
        assert_eq!(result.as_str(), Some("a,b,c"));
    }

    #[test]
    fn test_join_empty_delimiter() {
        let mut diags = Diagnostics::new();
        let delim = s("");
        let items = Value::List(vec![s("a"), s("b")]);
        let result = eval_join(&delim, &items, &mut diags).unwrap();
        assert_eq!(result.as_str(), Some("ab"));
    }

    #[test]
    fn test_join_null_delimiter() {
        let mut diags = Diagnostics::new();
        let delim = Value::Null;
        let items = Value::List(vec![s("a"), s("b")]);
        let result = eval_join(&delim, &items, &mut diags).unwrap();
        assert_eq!(result.as_str(), Some("ab"));
    }

    #[test]
    fn test_join_non_string_items() {
        let mut diags = Diagnostics::new();
        let delim = s(",");
        let items = Value::List(vec![s("a"), n(42.0)]);
        let result = eval_join(&delim, &items, &mut diags);
        assert!(diags.has_errors());
        assert!(result.is_none());
    }

    #[test]
    fn test_join_not_list() {
        let mut diags = Diagnostics::new();
        let delim = s(",");
        let items = s("not a list");
        let result = eval_join(&delim, &items, &mut diags);
        assert!(diags.has_errors());
        assert!(result.is_none());
    }

    #[test]
    fn test_split_basic() {
        let mut diags = Diagnostics::new();
        let delim = s(",");
        let source = s("a,b,c");
        let result = eval_split(&delim, &source, &mut diags).unwrap();
        match &result {
            Value::List(items) => {
                assert_eq!(items.len(), 3);
                assert_eq!(items[0].as_str(), Some("a"));
                assert_eq!(items[1].as_str(), Some("b"));
                assert_eq!(items[2].as_str(), Some("c"));
            }
            _ => panic!("expected list"),
        }
    }

    #[test]
    fn test_split_non_string() {
        let mut diags = Diagnostics::new();
        let result = eval_split(&s(","), &n(42.0), &mut diags);
        assert!(diags.has_errors());
        assert!(result.is_none());
    }

    #[test]
    fn test_select_basic() {
        let mut diags = Diagnostics::new();
        let idx = n(1.0);
        let items = Value::List(vec![s("a"), s("b"), s("c")]);
        let result = eval_select(&idx, &items, &mut diags).unwrap();
        assert_eq!(result.as_str(), Some("b"));
    }

    #[test]
    fn test_select_first() {
        let mut diags = Diagnostics::new();
        let idx = n(0.0);
        let items = Value::List(vec![s("only")]);
        let result = eval_select(&idx, &items, &mut diags).unwrap();
        assert_eq!(result.as_str(), Some("only"));
    }

    #[test]
    fn test_select_out_of_bounds() {
        let mut diags = Diagnostics::new();
        let idx = n(5.0);
        let items = Value::List(vec![s("a")]);
        let result = eval_select(&idx, &items, &mut diags);
        assert!(diags.has_errors());
        assert!(result.is_none());
    }

    #[test]
    fn test_select_negative() {
        let mut diags = Diagnostics::new();
        let idx = n(-1.0);
        let items = Value::List(vec![s("a")]);
        let result = eval_select(&idx, &items, &mut diags);
        assert!(diags.has_errors());
        assert!(result.is_none());
    }

    #[test]
    fn test_select_non_integer() {
        let mut diags = Diagnostics::new();
        let idx = n(1.5);
        let items = Value::List(vec![s("a")]);
        let result = eval_select(&idx, &items, &mut diags);
        assert!(diags.has_errors());
        assert!(result.is_none());
    }

    #[test]
    fn test_to_json_string() {
        let mut diags = Diagnostics::new();
        let val = s("hello");
        let result = eval_to_json(&val, &mut diags).unwrap();
        assert_eq!(result.as_str(), Some("\"hello\""));
    }

    #[test]
    fn test_to_json_object() {
        let mut diags = Diagnostics::new();
        let val = Value::Object(vec![(Cow::Owned("key".to_string()), s("value"))]);
        let result = eval_to_json(&val, &mut diags).unwrap();
        assert_eq!(result.as_str(), Some(r#"{"key":"value"}"#));
    }

    #[test]
    fn test_to_json_list() {
        let mut diags = Diagnostics::new();
        let val = Value::List(vec![n(1.0), n(2.0), n(3.0)]);
        let result = eval_to_json(&val, &mut diags).unwrap();
        assert_eq!(result.as_str(), Some("[1.0,2.0,3.0]"));
    }

    #[test]
    fn test_to_json_null() {
        let mut diags = Diagnostics::new();
        let result = eval_to_json(&Value::Null, &mut diags).unwrap();
        assert_eq!(result.as_str(), Some("null"));
    }

    #[test]
    fn test_to_base64() {
        let mut diags = Diagnostics::new();
        let result = eval_to_base64(&s("hello"), &mut diags).unwrap();
        assert_eq!(result.as_str(), Some("aGVsbG8="));
    }

    #[test]
    fn test_to_base64_non_string() {
        let mut diags = Diagnostics::new();
        let result = eval_to_base64(&n(42.0), &mut diags);
        assert!(diags.has_errors());
        assert!(result.is_none());
    }

    #[test]
    fn test_from_base64() {
        let mut diags = Diagnostics::new();
        let result = eval_from_base64(&s("aGVsbG8="), &mut diags).unwrap();
        assert_eq!(result.as_str(), Some("hello"));
    }

    #[test]
    fn test_from_base64_invalid() {
        let mut diags = Diagnostics::new();
        let result = eval_from_base64(&s("!!!invalid!!!"), &mut diags);
        assert!(diags.has_errors());
        assert!(result.is_none());
    }

    #[test]
    fn test_from_base64_non_string() {
        let mut diags = Diagnostics::new();
        let result = eval_from_base64(&n(42.0), &mut diags);
        assert!(diags.has_errors());
        assert!(result.is_none());
    }

    #[test]
    fn test_base64_round_trip() {
        let mut diags = Diagnostics::new();
        let original = s("Pulumi YAML rocks! 🎉");
        let encoded = eval_to_base64(&original, &mut diags).unwrap();
        let decoded = eval_from_base64(&encoded, &mut diags).unwrap();
        assert_eq!(decoded.as_str(), Some("Pulumi YAML rocks! 🎉"));
    }

    #[test]
    fn test_secret() {
        let val = s("password");
        let result = eval_secret(val);
        match &result {
            Value::Secret(inner) => assert_eq!(inner.as_str(), Some("password")),
            _ => panic!("expected secret"),
        }
    }

    #[test]
    fn test_property_access_name() {
        let mut diags = Diagnostics::new();
        let val = Value::Object(vec![
            (Cow::Owned("name".to_string()), s("test")),
            (Cow::Owned("count".to_string()), n(42.0)),
        ]);
        let result = eval_property_access(
            &val,
            &[PropertyAccessor::Name(Cow::Borrowed("name"))],
            &mut diags,
        )
        .unwrap();
        assert_eq!(result.as_str(), Some("test"));
    }

    #[test]
    fn test_property_access_index() {
        let mut diags = Diagnostics::new();
        let val = Value::List(vec![s("first"), s("second"), s("third")]);
        let result =
            eval_property_access(&val, &[PropertyAccessor::IntSubscript(1)], &mut diags).unwrap();
        assert_eq!(result.as_str(), Some("second"));
    }

    #[test]
    fn test_property_access_chain() {
        let mut diags = Diagnostics::new();
        let val = Value::Object(vec![(
            Cow::Owned("outer".to_string()),
            Value::Object(vec![(Cow::Owned("inner".to_string()), s("deep"))]),
        )]);
        let result = eval_property_access(
            &val,
            &[
                PropertyAccessor::Name(Cow::Borrowed("outer")),
                PropertyAccessor::Name(Cow::Borrowed("inner")),
            ],
            &mut diags,
        )
        .unwrap();
        assert_eq!(result.as_str(), Some("deep"));
    }

    #[test]
    fn test_property_access_missing() {
        let mut diags = Diagnostics::new();
        let val = Value::Object(vec![]);
        let result = eval_property_access(
            &val,
            &[PropertyAccessor::Name(Cow::Borrowed("missing"))],
            &mut diags,
        )
        .unwrap();
        assert!(result.is_null());
    }

    #[test]
    fn test_property_access_on_null() {
        let mut diags = Diagnostics::new();
        let result = eval_property_access(
            &Value::Null,
            &[PropertyAccessor::Name(Cow::Borrowed("x"))],
            &mut diags,
        )
        .unwrap();
        assert!(result.is_null());
    }

    #[test]
    fn test_property_access_through_secret() {
        let mut diags = Diagnostics::new();
        let val = Value::Secret(Box::new(Value::Object(vec![(
            Cow::Owned("key".to_string()),
            s("secret-val"),
        )])));
        let result = eval_property_access(
            &val,
            &[PropertyAccessor::Name(Cow::Borrowed("key"))],
            &mut diags,
        )
        .unwrap();
        match &result {
            Value::Secret(inner) => assert_eq!(inner.as_str(), Some("secret-val")),
            _ => panic!("expected secret wrapping, got {:?}", result),
        }
    }

    #[test]
    fn test_property_access_index_oob() {
        let mut diags = Diagnostics::new();
        let val = Value::List(vec![s("only")]);
        let result = eval_property_access(&val, &[PropertyAccessor::IntSubscript(5)], &mut diags);
        assert!(diags.has_errors());
        assert!(result.is_none());
    }

    #[test]
    fn test_value_to_json_secret() {
        // Secrets should be unwrapped for JSON
        let val = Value::Secret(Box::new(s("hidden")));
        let json = val.to_json();
        assert_eq!(json, serde_json::Value::String("hidden".to_string()));
    }

    // =========================================================================
    // Math builtin tests
    // =========================================================================

    #[test]
    fn test_abs_positive() {
        let mut diags = Diagnostics::new();
        let result = eval_abs(&n(42.0), &mut diags).unwrap();
        assert_eq!(result, Value::Number(42.0));
    }

    #[test]
    fn test_abs_negative() {
        let mut diags = Diagnostics::new();
        let result = eval_abs(&n(-42.0), &mut diags).unwrap();
        assert_eq!(result, Value::Number(42.0));
    }

    #[test]
    fn test_abs_zero() {
        let mut diags = Diagnostics::new();
        let result = eval_abs(&n(0.0), &mut diags).unwrap();
        assert_eq!(result, Value::Number(0.0));
    }

    #[test]
    fn test_abs_type_error() {
        let mut diags = Diagnostics::new();
        let result = eval_abs(&s("not a number"), &mut diags);
        assert!(result.is_none());
        assert!(diags.has_errors());
    }

    #[test]
    fn test_floor_basic() {
        let mut diags = Diagnostics::new();
        assert_eq!(eval_floor(&n(3.7), &mut diags).unwrap(), Value::Number(3.0));
    }

    #[test]
    fn test_floor_negative() {
        let mut diags = Diagnostics::new();
        assert_eq!(
            eval_floor(&n(-1.2), &mut diags).unwrap(),
            Value::Number(-2.0)
        );
    }

    #[test]
    fn test_floor_whole() {
        let mut diags = Diagnostics::new();
        assert_eq!(eval_floor(&n(5.0), &mut diags).unwrap(), Value::Number(5.0));
    }

    #[test]
    fn test_ceil_basic() {
        let mut diags = Diagnostics::new();
        assert_eq!(eval_ceil(&n(3.2), &mut diags).unwrap(), Value::Number(4.0));
    }

    #[test]
    fn test_ceil_negative() {
        let mut diags = Diagnostics::new();
        assert_eq!(
            eval_ceil(&n(-1.8), &mut diags).unwrap(),
            Value::Number(-1.0)
        );
    }

    #[test]
    fn test_ceil_whole() {
        let mut diags = Diagnostics::new();
        assert_eq!(eval_ceil(&n(5.0), &mut diags).unwrap(), Value::Number(5.0));
    }

    #[test]
    fn test_max_basic() {
        let mut diags = Diagnostics::new();
        let list = Value::List(vec![n(1.0), n(5.0), n(3.0)]);
        assert_eq!(eval_max(&list, &mut diags).unwrap(), Value::Number(5.0));
    }

    #[test]
    fn test_max_single() {
        let mut diags = Diagnostics::new();
        let list = Value::List(vec![n(42.0)]);
        assert_eq!(eval_max(&list, &mut diags).unwrap(), Value::Number(42.0));
    }

    #[test]
    fn test_max_empty() {
        let mut diags = Diagnostics::new();
        let list = Value::List(vec![]);
        assert!(eval_max(&list, &mut diags).is_none());
        assert!(diags.has_errors());
    }

    #[test]
    fn test_max_type_error() {
        let mut diags = Diagnostics::new();
        let list = Value::List(vec![n(1.0), s("not a number")]);
        assert!(eval_max(&list, &mut diags).is_none());
        assert!(diags.has_errors());
    }

    #[test]
    fn test_min_basic() {
        let mut diags = Diagnostics::new();
        let list = Value::List(vec![n(1.0), n(5.0), n(3.0)]);
        assert_eq!(eval_min(&list, &mut diags).unwrap(), Value::Number(1.0));
    }

    #[test]
    fn test_min_single() {
        let mut diags = Diagnostics::new();
        let list = Value::List(vec![n(42.0)]);
        assert_eq!(eval_min(&list, &mut diags).unwrap(), Value::Number(42.0));
    }

    #[test]
    fn test_min_empty() {
        let mut diags = Diagnostics::new();
        let list = Value::List(vec![]);
        assert!(eval_min(&list, &mut diags).is_none());
        assert!(diags.has_errors());
    }

    // =========================================================================
    // String builtin tests
    // =========================================================================

    #[test]
    fn test_string_len_ascii() {
        let mut diags = Diagnostics::new();
        assert_eq!(
            eval_string_len(&s("hello"), &mut diags).unwrap(),
            Value::Number(5.0)
        );
    }

    #[test]
    fn test_string_len_unicode() {
        let mut diags = Diagnostics::new();
        // Emoji counts as 1 char
        assert_eq!(
            eval_string_len(&s("hi🎉"), &mut diags).unwrap(),
            Value::Number(3.0)
        );
    }

    #[test]
    fn test_string_len_empty() {
        let mut diags = Diagnostics::new();
        assert_eq!(
            eval_string_len(&s(""), &mut diags).unwrap(),
            Value::Number(0.0)
        );
    }

    #[test]
    fn test_string_len_type_error() {
        let mut diags = Diagnostics::new();
        assert!(eval_string_len(&n(42.0), &mut diags).is_none());
        assert!(diags.has_errors());
    }

    #[test]
    fn test_substring_basic() {
        let mut diags = Diagnostics::new();
        let result = eval_substring(&s("hello world"), &n(0.0), &n(5.0), &mut diags).unwrap();
        assert_eq!(result.as_str(), Some("hello"));
    }

    #[test]
    fn test_substring_middle() {
        let mut diags = Diagnostics::new();
        let result = eval_substring(&s("hello world"), &n(6.0), &n(5.0), &mut diags).unwrap();
        assert_eq!(result.as_str(), Some("world"));
    }

    #[test]
    fn test_substring_beyond_length() {
        let mut diags = Diagnostics::new();
        let result = eval_substring(&s("hi"), &n(0.0), &n(100.0), &mut diags).unwrap();
        assert_eq!(result.as_str(), Some("hi"));
    }

    #[test]
    fn test_substring_zero_length() {
        let mut diags = Diagnostics::new();
        let result = eval_substring(&s("hello"), &n(2.0), &n(0.0), &mut diags).unwrap();
        assert_eq!(result.as_str(), Some(""));
    }

    // =========================================================================
    // Time builtin tests
    // =========================================================================

    #[test]
    fn test_time_utc_format() {
        let mut diags = Diagnostics::new();
        let result = eval_time_utc(&Value::Null, &mut diags).unwrap();
        let s = result.as_str().unwrap();
        // Should match ISO 8601 pattern: YYYY-MM-DDTHH:MM:SSZ
        assert!(s.len() == 20, "expected 20 chars, got {} ({})", s.len(), s);
        assert!(s.ends_with('Z'));
        assert_eq!(&s[4..5], "-");
        assert_eq!(&s[7..8], "-");
        assert_eq!(&s[10..11], "T");
    }

    #[test]
    fn test_time_unix_reasonable() {
        let mut diags = Diagnostics::new();
        let result = eval_time_unix(&Value::Null, &mut diags).unwrap();
        match result {
            Value::Number(n) => assert!(n > 1_700_000_000.0, "timestamp too small: {}", n),
            _ => panic!("expected number"),
        }
    }

    #[test]
    fn test_date_format_ymd() {
        let mut diags = Diagnostics::new();
        let result = eval_date_format(&s("%Y-%m-%d"), &mut diags).unwrap();
        let formatted = result.as_str().unwrap();
        // Should be YYYY-MM-DD
        assert_eq!(formatted.len(), 10);
        assert_eq!(&formatted[4..5], "-");
        assert_eq!(&formatted[7..8], "-");
    }

    #[test]
    fn test_date_format_hms() {
        let mut diags = Diagnostics::new();
        let result = eval_date_format(&s("%H:%M:%S"), &mut diags).unwrap();
        let formatted = result.as_str().unwrap();
        assert_eq!(formatted.len(), 8);
        assert_eq!(&formatted[2..3], ":");
    }

    #[test]
    fn test_date_format_type_error() {
        let mut diags = Diagnostics::new();
        assert!(eval_date_format(&n(42.0), &mut diags).is_none());
        assert!(diags.has_errors());
    }

    // =========================================================================
    // UUID/Random builtin tests
    // =========================================================================

    #[test]
    fn test_uuid_format() {
        let mut diags = Diagnostics::new();
        let result = eval_uuid(&Value::Null, &mut diags).unwrap();
        let id = result.as_str().unwrap();
        assert_eq!(id.split('-').count(), 5, "UUID should have 5 parts: {}", id);
        assert_eq!(id.len(), 36, "UUID should be 36 chars: {}", id);
    }

    #[test]
    fn test_uuid_unique() {
        let mut diags = Diagnostics::new();
        let a = eval_uuid(&Value::Null, &mut diags).unwrap();
        let b = eval_uuid(&Value::Null, &mut diags).unwrap();
        assert_ne!(a.as_str(), b.as_str());
    }

    #[test]
    fn test_random_string_length() {
        let mut diags = Diagnostics::new();
        let result = eval_random_string(&n(32.0), &mut diags).unwrap();
        assert_eq!(result.as_str().unwrap().len(), 32);
    }

    #[test]
    fn test_random_string_empty() {
        let mut diags = Diagnostics::new();
        let result = eval_random_string(&n(0.0), &mut diags).unwrap();
        assert_eq!(result.as_str().unwrap(), "");
    }

    #[test]
    fn test_random_string_alphanumeric() {
        let mut diags = Diagnostics::new();
        let result = eval_random_string(&n(100.0), &mut diags).unwrap();
        let chars = result.as_str().unwrap();
        assert!(chars.chars().all(|c| c.is_ascii_alphanumeric()));
    }

    // =========================================================================
    // unix_to_civil tests
    // =========================================================================

    #[test]
    fn test_unix_to_civil_epoch() {
        let (y, m, d, h, min, s) = unix_to_civil(0);
        assert_eq!((y, m, d, h, min, s), (1970, 1, 1, 0, 0, 0));
    }

    #[test]
    fn test_unix_to_civil_known_date() {
        // 2024-01-15T12:30:45Z = 1705321845
        let (y, m, d, h, min, s) = unix_to_civil(1705321845);
        assert_eq!((y, m, d, h, min, s), (2024, 1, 15, 12, 30, 45));
    }
}

#[cfg(test)]
mod derive_tests {
    use super::*;

    fn obj<'a>(pairs: &[(&'a str, Value<'a>)]) -> Value<'a> {
        Value::Object(
            pairs
                .iter()
                .map(|(k, v)| (Cow::Borrowed(*k), v.clone()))
                .collect(),
        )
    }

    /// An `Unknown` seed defers instead of erroring.
    ///
    /// `fn::randomString` and `fn::uuid` are the only two builtins that skip the
    /// `has_unknown` guard, and skipping it here would turn a seed built from a
    /// live output into a hard error at preview rather than a value the deploy
    /// resolves.
    #[test]
    fn an_unknown_seed_propagates_unknown() {
        let mut diags = Diagnostics::default();
        let out = eval_derive_string(&Value::Unknown, &mut diags);
        assert_eq!(out, Some(Value::Unknown));
        assert!(!diags.has_errors(), "deferring is not an error: {diags}");

        // Also when the unknown is nested in the argument object.
        let mut diags = Diagnostics::default();
        let arg = obj(&[("from", Value::Unknown), ("length", Value::Number(4.0))]);
        assert_eq!(
            eval_derive_string(&arg, &mut diags),
            Some(Value::Unknown),
            "an unknown inside the argument object defers too"
        );
        assert!(!diags.has_errors(), "{diags}");
    }

    /// A secret seed is refused rather than quietly unwrapped.
    ///
    /// A secret is not an unknown -- it has a value -- but unwrapping it here
    /// would put a secret-derived value into a plain resource name without
    /// saying so. Refusing is the conservative reading, and it is stated
    /// because the alternative is silent.
    #[test]
    fn a_secret_seed_is_refused_rather_than_unwrapped() {
        let mut diags = Diagnostics::default();
        let arg = Value::Secret(Box::new(Value::String(Cow::Borrowed("tap_collector"))));
        // A secret is not a string, so the argument shape is refused rather
        // than silently unwrapped -- unwrapping would put a secret-derived
        // value into a plain name without saying so.
        let out = eval_derive_string(&arg, &mut diags);
        assert!(out.is_none());
        assert!(diags.has_errors());
    }

    #[test]
    fn the_shorthand_defaults_to_eight_characters() {
        let mut diags = Diagnostics::default();
        let out = eval_derive_string(&Value::String(Cow::Borrowed("abc")), &mut diags);
        assert_eq!(out, Some(Value::String(Cow::Owned("6cmbz1ri".to_string()))));
        assert!(!diags.has_errors(), "{diags}");
    }

    #[test]
    fn a_missing_from_is_refused_with_advice() {
        let mut diags = Diagnostics::default();
        let arg = obj(&[("length", Value::Number(4.0))]);
        assert!(eval_derive_string(&arg, &mut diags).is_none());
        let text = format!("{diags}");
        assert!(text.contains("requires a 'from' argument"), "{text}");
    }

    /// A mistyped argument is named, not defaulted. A silently defaulted typo
    /// is how a stack gets a name nobody intended.
    #[test]
    fn an_unknown_argument_is_named_not_defaulted() {
        let mut diags = Diagnostics::default();
        let arg = obj(&[
            ("from", Value::String(Cow::Borrowed("abc"))),
            ("len", Value::Number(4.0)),
        ]);
        assert!(eval_derive_string(&arg, &mut diags).is_none());
        let text = format!("{diags}");
        assert!(text.contains("no argument named 'len'"), "{text}");
        assert!(text.contains("'from', 'length' and 'alphabet'"), "{text}");
    }

    #[test]
    fn a_non_string_non_object_argument_is_refused() {
        for bad in [
            Value::Number(4.0),
            Value::Bool(true),
            Value::Null,
            Value::List(vec![Value::String(Cow::Borrowed("abc"))]),
        ] {
            let mut diags = Diagnostics::default();
            assert!(
                eval_derive_string(&bad, &mut diags).is_none(),
                "{bad:?} was accepted"
            );
            assert!(diags.has_errors());
        }
    }

    /// The bounds, at and either side of each edge.
    #[test]
    fn the_length_bounds_are_exact() {
        assert!(derive("abc", 0, DERIVE_ALPHABET).is_err());
        assert_eq!(derive("abc", 1, DERIVE_ALPHABET).map(|s| s.len()), Ok(1));
        assert_eq!(derive("abc", 64, DERIVE_ALPHABET).map(|s| s.len()), Ok(64));
        assert!(derive("abc", 65, DERIVE_ALPHABET).is_err());
    }

    /// A one-character alphabet is legal and produces that character, which is
    /// the degenerate case rejection sampling must not divide by zero on.
    #[test]
    fn a_single_character_alphabet_repeats_it() {
        assert_eq!(derive("abc", 5, "q"), Ok("qqqqq".to_string()));
    }

    /// The draw is unbiased, measured rather than asserted.
    ///
    /// `256 % 36 == 4`, so a plain `byte % 36` would map bytes 252..=255 onto
    /// the alphabet's first four characters on top of their fair share, giving
    /// them 8/256 of the draw instead of 7/256. Rejection sampling discards
    /// that band instead.
    ///
    /// Coverage does not detect this -- a modulo draw still produces all
    /// thirty-six characters, which an earlier version of this test proved by
    /// passing against one -- so the test has to measure the share. Over 25,600
    /// draws the two algorithms separate cleanly and far apart: rejection lands
    /// at 0.1113 against an ideal 4/36 = 0.1111, while modulo lands at 0.1241.
    /// The bound below sits between them.
    #[test]
    fn the_draw_is_unbiased_not_merely_covering() {
        let first_four: Vec<char> = DERIVE_ALPHABET.chars().take(4).collect();
        let mut total = 0usize;
        let mut hits = 0usize;
        let mut seen = std::collections::HashSet::new();
        for i in 0..400 {
            let s = derive(&format!("seed-{i}"), 64, DERIVE_ALPHABET).expect("derives");
            total += s.chars().count();
            hits += s.chars().filter(|c| first_four.contains(c)).count();
            seen.extend(s.chars());
        }
        // Coverage, which is necessary but not sufficient.
        assert_eq!(
            seen.len(),
            36,
            "only {} of 36 characters were drawn",
            seen.len()
        );

        let share = hits as f64 / total as f64;
        let ideal = 4.0 / 36.0;
        assert!(
            share < 0.118,
            "the first four characters took {share:.5} of {total} draws against an \
             ideal {ideal:.5}; a plain modulo draw gives 0.1241, so this looks biased"
        );
        assert!(
            share > 0.104,
            "the first four characters took only {share:.5} of {total} draws against an \
             ideal {ideal:.5}, which is not a fair draw either"
        );
    }

    /// One implementation, two callers. `derive` is what both the evaluator and
    /// the static resolver reach, so this is the contract that keeps a resolved
    /// name and a deployed name identical.
    #[test]
    fn the_shared_implementation_is_a_pure_function() {
        let a = derive("voice-usage-egress", 4, DERIVE_ALPHABET);
        let b = derive("voice-usage-egress", 4, DERIVE_ALPHABET);
        assert_eq!(a, b);
        assert_eq!(a, Ok("b0aw".to_string()));
    }
}

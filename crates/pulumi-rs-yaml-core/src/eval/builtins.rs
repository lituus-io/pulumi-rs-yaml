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

    let mut strs = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        match item {
            Value::String(s) => strs.push(s.as_ref().to_string()),
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

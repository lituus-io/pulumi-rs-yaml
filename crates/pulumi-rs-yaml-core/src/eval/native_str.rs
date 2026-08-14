// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

//! In-process evaluation of the `str` package's string functions.
//!
//! `pulumi/pulumi-str` is a provider whose entire surface is pure string
//! manipulation — no state, no I/O, no cloud. Its last release is v1.0.0 from
//! October 2022, built against `pulumi-go-provider` v0.8.0, whose cancel
//! middleware races on shutdown: `drain()` checks an entry for nil but calls
//! its `evict` field without the same check, while a concurrent evict nils that
//! field under a deliberately mutex-free fast path. The result is a SIGSEGV in
//! the plugin during `Cancel`, after every resource has already been applied —
//! which the engine reports as a failed update on an otherwise perfect run.
//! Upstream replaced the whole structure in v1.5.0, but no `str` release
//! carries the fix and none is expected.
//!
//! Evaluating these functions here means the plugin is never asked for them, so
//! it is never loaded and cannot crash. It is also strictly faster: a plugin
//! process launch and a gRPC round-trip per invoke become a string operation.
//!
//! Semantics are Go's, verified case by case against `strings.ReplaceAll`,
//! `strings.TrimPrefix` and `strings.TrimSuffix` — including the empty-pattern
//! rule, where both languages match at every UTF-8 boundary and yield k+1
//! replacements for a k-rune string.
//!
//! Deliberately NOT handled: `str:regexp:*`. Go's RE2 and Rust's `regex` crate
//! are close but not identical, and a silent divergence inside a regular
//! expression is worse than a rare crash. Those tokens fall through to the
//! provider untouched.

use super::value::Value;
use std::collections::HashMap;

/// The single output property every `str` function returns.
const RESULT: &str = "result";

/// Read a string argument, accepting only a real string.
///
/// A non-string (an unresolved output, a number, a missing key) means this
/// invoke cannot be answered here, and it must go to the provider rather than
/// be guessed at.
fn arg<'a>(args: &'a HashMap<String, Value<'static>>, name: &str) -> Option<&'a str> {
    match args.get(name) {
        Some(Value::String(s)) => Some(s.as_ref()),
        _ => None,
    }
}

fn ok(value: String) -> Option<HashMap<String, Value<'static>>> {
    let mut out = HashMap::with_capacity(1);
    out.insert(RESULT.to_string(), Value::String(value.into()));
    Some(out)
}

/// Evaluate a `str` function in process, or return `None` to defer to the engine.
///
/// `None` is the "not ours" answer and is always safe: the caller falls back to
/// the normal invoke path, so an unknown token, an unresolved argument, or a
/// preview-time `Unknown` all behave exactly as before.
pub(crate) fn try_invoke(
    token: &str,
    args: &HashMap<String, Value<'static>>,
) -> Option<HashMap<String, Value<'static>>> {
    match token {
        // strings.ReplaceAll(s, old, new)
        "str:index:replace" => {
            let s = arg(args, "string")?;
            let old = arg(args, "old")?;
            let new = arg(args, "new")?;
            ok(s.replace(old, new))
        }
        // strings.TrimPrefix(s, prefix)
        "str:index:trimPrefix" => {
            let s = arg(args, "string")?;
            let prefix = arg(args, "prefix")?;
            ok(s.strip_prefix(prefix).unwrap_or(s).to_string())
        }
        // strings.TrimSuffix(s, suffix)
        "str:index:trimSuffix" => {
            let s = arg(args, "string")?;
            let suffix = arg(args, "suffix")?;
            ok(s.strip_suffix(suffix).unwrap_or(s).to_string())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(pairs: &[(&str, &str)]) -> HashMap<String, Value<'static>> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), Value::String((*v).to_string().into())))
            .collect()
    }

    fn result_of(token: &str, pairs: &[(&str, &str)]) -> Option<String> {
        let out = try_invoke(token, &args(pairs))?;
        match out.get(RESULT) {
            Some(Value::String(s)) => Some(s.to_string()),
            _ => None,
        }
    }

    /// Every row is Go's documented behaviour for the corresponding `strings` call.
    #[test]
    fn test_replace_matches_go_semantics() {
        let cases: &[(&str, &str, &str, &str)] = &[
            // (s, old, new, expected)
            ("a-b-c", "-", "_", "a_b_c"),
            ("aaa", "aa", "b", "ba"),    // non-overlapping, left to right
            ("abc", "z", "-", "abc"),    // no match
            ("abc", "", "-", "-a-b-c-"), // empty old: k+1 for k runes
            ("héllo", "", ".", ".h.é.l.l.o."), // ... at UTF-8 boundaries
            ("", "", "-", "-"),
            ("", "x", "-", ""),
            ("abc", "abc", "", ""),
            ("aXbXc", "X", "", "abc"),
        ];
        for (s, old, new, expected) in cases {
            assert_eq!(
                result_of(
                    "str:index:replace",
                    &[("string", s), ("old", old), ("new", new)]
                )
                .as_deref(),
                Some(*expected),
                "replace({:?}, {:?}, {:?})",
                s,
                old,
                new,
            );
        }
    }

    #[test]
    fn test_trim_prefix_matches_go_semantics() {
        let cases: &[(&str, &str, &str)] = &[
            ("abcabc", "abc", "abc"), // removes ONE occurrence
            ("abc", "z", "abc"),      // absent: unchanged
            ("abc", "", "abc"),       // empty prefix: unchanged
            ("abc", "abc", ""),
            ("abc", "abcd", "abc"), // longer than input
            ("", "x", ""),
        ];
        for (s, prefix, expected) in cases {
            assert_eq!(
                result_of("str:index:trimPrefix", &[("string", s), ("prefix", prefix)]).as_deref(),
                Some(*expected),
                "trimPrefix({:?}, {:?})",
                s,
                prefix,
            );
        }
    }

    #[test]
    fn test_trim_suffix_matches_go_semantics() {
        let cases: &[(&str, &str, &str)] = &[
            ("abcabc", "abc", "abc"),
            ("abc", "z", "abc"),
            ("abc", "", "abc"),
            ("abc", "abc", ""),
            ("abc", "zabc", "abc"),
            ("", "x", ""),
        ];
        for (s, suffix, expected) in cases {
            assert_eq!(
                result_of("str:index:trimSuffix", &[("string", s), ("suffix", suffix)]).as_deref(),
                Some(*expected),
                "trimSuffix({:?}, {:?})",
                s,
                suffix,
            );
        }
    }

    /// Regex functions are deliberately left to the provider.
    #[test]
    fn test_regexp_tokens_defer_to_the_provider() {
        for token in [
            "str:regexp:replace",
            "str:regexp:split",
            "str:index:split",
            "aws:index:getAmi",
            "str",
            "",
        ] {
            assert!(
                try_invoke(token, &args(&[("string", "a"), ("old", "a"), ("new", "b")])).is_none(),
                "{} must fall through to the engine",
                token,
            );
        }
    }

    /// A non-string argument is unanswerable here — defer rather than guess.
    #[test]
    fn test_non_string_arguments_defer_to_the_provider() {
        let mut a = args(&[("string", "abc"), ("old", "a")]);
        a.insert("new".to_string(), Value::Number(1.0));
        assert!(try_invoke("str:index:replace", &a).is_none());

        let mut unknown = args(&[("old", "a"), ("new", "b")]);
        unknown.insert("string".to_string(), Value::Unknown);
        assert!(
            try_invoke("str:index:replace", &unknown).is_none(),
            "an Unknown (preview) input must reach the normal path",
        );

        // Missing arguments entirely.
        assert!(try_invoke("str:index:replace", &args(&[("string", "abc")])).is_none());
        assert!(try_invoke("str:index:trimPrefix", &HashMap::new()).is_none());
    }

    /// Arbitrary input must never panic — these run inside the language host.
    #[test]
    fn test_no_panic_on_adversarial_input() {
        let long = "a".repeat(4096);
        let nasty = ["\0", "\u{feff}", "🙂🙂", long.as_str(), "\\", "%s"];
        for s in nasty {
            for old in nasty {
                let _ = result_of(
                    "str:index:replace",
                    &[("string", s), ("old", old), ("new", "x")],
                );
                let _ = result_of("str:index:trimPrefix", &[("string", s), ("prefix", old)]);
                let _ = result_of("str:index:trimSuffix", &[("string", s), ("suffix", old)]);
            }
        }
    }
}

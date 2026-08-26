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
//! `str:regexp:replace` is handled here too, and the caution that once kept it
//! out is answered rather than dropped. Both engines are RE2 lineage — linear
//! time, no backreferences, no lookaround — and the replacement template
//! syntax is the same in each: `$1`, `${name}`, and `$$` for a literal dollar.
//! Where they are NOT identical is pattern syntax at the edges, so a pattern
//! this crate cannot compile is never approximated: it returns `None` and
//! takes the provider path exactly as an unhandled token does. The remaining
//! failure mode is "declined to answer", not a quietly different string.
//!
//! Still NOT handled: `str:regexp:match` and `str:regexp:split`.

use super::value::Value;
use regex::Regex;
use std::borrow::Cow;
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

/// Collapse the bridged spelling back to the form `str` actually registers.
///
/// Callers hand this module an ALREADY-CANONICALIZED token, and function
/// canonicalization applies the bridged-provider rule to every three-part
/// token: `str:regexp:replace` becomes `str:regexp/replace:replace`. But `str`
/// is hand-written and registers `str:regexp:replace` verbatim, so the slashed
/// form matches nothing — not here, and not at the provider either, which is
/// exactly how a template authoring the three-part spelling fails with
/// "Invoke 'regexp/replace:replace' not found".
///
/// Recognising both spellings answers those invokes in process, so the
/// unresolvable token is never sent. Scoped to `str` and to the redundant
/// shape `pkg:mod/fn:fn` alone; every other token is returned untouched, so a
/// genuinely bridged token keeps its meaning.
fn provider_form(token: &str) -> Cow<'_, str> {
    let mut parts = token.split(':');
    let (Some("str"), Some(module), Some(func), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Cow::Borrowed(token);
    };
    match module.split_once('/') {
        // `str:regexp/replace:replace` -> `str:regexp:replace`
        Some((head, tail)) if tail == func => Cow::Owned(format!("str:{head}:{func}")),
        _ => Cow::Borrowed(token),
    }
}

/// Whether this token is answered here rather than by the provider.
///
/// Package discovery consults this so a natively-answered invoke does not pull
/// its provider into the referenced-package set. Intercepting the invoke alone
/// is not enough: a referenced package is registered with the engine and has
/// its schema fetched, both of which load the plugin binary — and the crash
/// this avoids happens in `Cancel`, on any provider the engine has loaded.
///
/// Keyed on the token alone, so it is a property of the template rather than of
/// any particular argument values. A template mixing handled and unhandled
/// tokens still pulls the package in, which is correct: it genuinely needs it.
pub(crate) fn handles(token: &str) -> bool {
    matches!(
        provider_form(token).as_ref(),
        "str:index:replace"
            | "str:index:trimPrefix"
            | "str:index:trimSuffix"
            | "str:regexp:replace"
    )
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
    match provider_form(token).as_ref() {
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
        // regexp.MustCompile(old).ReplaceAllString(s, new)
        //
        // `new` is a template, not a literal: Go expands `$1` / `${name}` in
        // it, and so does this crate, with the same `$$` escape for a literal
        // dollar. That is why the replacement is passed through rather than
        // escaped — escaping it would be a DIFFERENT function.
        //
        // An uncompilable pattern yields None, which sends the invoke to the
        // provider. Declining is the point: the pattern syntaxes agree almost
        // everywhere and diverge at the edges, and the edges are exactly where
        // guessing would produce a plausible wrong string instead of an error.
        "str:regexp:replace" => {
            let s = arg(args, "string")?;
            let old = arg(args, "old")?;
            let new = arg(args, "new")?;
            let re = Regex::new(old).ok()?;
            ok(re.replace_all(s, new).into_owned())
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
            // regexp:replace moved to the handled set; match and split did not.
            "str:regexp:match",
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

    // ---------------------------------------------------------------- //
    // str:regexp:replace                                                //
    // ---------------------------------------------------------------- //

    /// Every row is Go's `regexp.MustCompile(old).ReplaceAllString(s, new)`.
    #[test]
    fn test_regexp_replace_matches_go_semantics() {
        let cases: &[(&str, &str, &str, &str)] = &[
            // (s, old, new, expected)
            ("a1b22c", r"\d+", "#", "a#b#c"),
            ("hello", "l+", "L", "heLo"),
            ("abc", "x", "y", "abc"),              // no match -> unchanged
            ("aaa", "a", "b", "bbb"),              // ALL matches, not just the first
            ("a-b", "-", "", "ab"),                // empty replacement deletes
            ("2026-08-26", r"(\d+)-(\d+)-(\d+)", "$3/$2/$1", "26/08/2026"),
            ("john smith", r"(?P<f>\w+) (?P<l>\w+)", "${l}, ${f}", "smith, john"),
            ("price", "^", "$$", "$price"),        // $$ is a literal dollar
            ("ab", "", "-", "-a-b-"),              // empty pattern: every boundary
            ("héllo", "é", "e", "hello"),          // multi-byte safe
        ];
        for (s, old, new, expected) in cases {
            assert_eq!(
                result_of("str:regexp:replace",
                          &[("string", s), ("old", old), ("new", new)]).as_deref(),
                Some(*expected),
                "regexp replace({s:?}, {old:?}, {new:?})",
            );
        }
    }

    /// The case this was added for: stripping SQL comments from a query file.
    #[test]
    fn test_regexp_replace_strips_sql_comments() {
        let sql = "SELECT a, -- trailing\n/* block\n   spanning */ b FROM t";
        let out = result_of(
            "str:regexp:replace",
            &[
                ("string", sql),
                ("old", r"(--[^\n]*)|(/\*[\s\S]*?\*/)"),
                ("new", ""),
            ],
        )
        .expect("the lazy quantifier and alternation must be supported");
        assert!(!out.contains("trailing"), "line comment survived: {out:?}");
        assert!(!out.contains("spanning"), "block comment survived: {out:?}");
        assert!(out.contains("SELECT a,") && out.contains("b FROM t"));
    }

    /// An uncompilable pattern DECLINES rather than guessing.
    ///
    /// The whole safety argument for answering regexes in process: where the
    /// two syntaxes diverge, this must produce no answer at all, never a
    /// plausible wrong one.
    #[test]
    fn test_an_uncompilable_pattern_defers_to_the_provider() {
        for bad in [r"(unclosed", r"a{2,1}", r"[z-a]", r"(?P<>x)"] {
            assert!(
                try_invoke(
                    "str:regexp:replace",
                    &args(&[("string", "abc"), ("old", bad), ("new", "x")]),
                )
                .is_none(),
                "pattern {bad:?} must defer, not be approximated",
            );
        }
    }

    /// Backreferences and lookaround exist in neither engine, so a pattern
    /// using them must defer rather than appear to work.
    #[test]
    fn test_non_re2_constructs_defer() {
        for unsupported in [r"(a)\1", r"(?=foo)", r"(?<=foo)", r"(?!foo)"] {
            assert!(
                try_invoke(
                    "str:regexp:replace",
                    &args(&[("string", "foo"), ("old", unsupported), ("new", "x")]),
                )
                .is_none(),
                "{unsupported:?} is not RE2 and must defer",
            );
        }
    }

    #[test]
    fn test_regexp_replace_is_declared_handled() {
        // handles() gates package discovery: if this token is not listed, the
        // `str` plugin is still loaded and can still crash on Cancel, which is
        // the whole reason this module exists.
        assert!(handles("str:regexp:replace"));
        assert!(!handles("str:regexp:match"), "match is not implemented");
        assert!(!handles("str:regexp:split"), "split is not implemented");
    }

    #[test]
    fn test_regexp_replace_defers_on_unresolved_or_missing_args() {
        let mut unknown = args(&[("old", "a"), ("new", "b")]);
        unknown.insert("string".to_string(), Value::Unknown);
        assert!(try_invoke("str:regexp:replace", &unknown).is_none());
        assert!(try_invoke("str:regexp:replace", &args(&[("string", "abc")])).is_none());
    }

    /// Arbitrary pattern AND subject must never panic — this runs inside the
    /// language host, where a panic takes down the deploy.
    #[test]
    fn test_regexp_replace_never_panics() {
        // Pairs, not triples: cubing 17 inputs over a 4096-char subject cost
        // a minute and proved nothing the pairs do not. Each axis is still
        // swept against a fixed, adversarial partner.
        let long = "a".repeat(4096);
        let nasty = [
            "\0", "\u{feff}", "🙂🙂", long.as_str(), "\\", "%s", "$1", "$$",
            "(", "[", "*", "+", "?", "{", r"\", r"(?P<n>a)", ".*.*.*.*",
        ];
        for probe in nasty {
            // subject varies
            let _ = result_of(
                "str:regexp:replace",
                &[("string", probe), ("old", "a"), ("new", "x")],
            );
            // pattern varies (most are uncompilable and must simply decline)
            let _ = result_of(
                "str:regexp:replace",
                &[("string", "abc"), ("old", probe), ("new", "x")],
            );
            // replacement template varies — $ handling is the sharp edge here
            let _ = result_of(
                "str:regexp:replace",
                &[("string", "abc"), ("old", "(b)"), ("new", probe)],
            );
        }
    }

    // ---------------------------------------------------------------- //
    // The spelling that actually arrives                                //
    // ---------------------------------------------------------------- //

    /// The production path, and the reason the live failure happened.
    ///
    /// The evaluator canonicalizes before calling here, and function
    /// canonicalization slashes every three-part token. A template writing
    /// `fn::str:regexp:replace` therefore arrives as
    /// `str:regexp/replace:replace` — which `str` does not register, so
    /// deferring meant "Invoke 'regexp/replace:replace' not found". Answering
    /// both spellings is what keeps that token from ever being sent.
    #[test]
    fn test_the_canonicalized_spelling_is_answered() {
        assert_eq!(
            result_of(
                "str:regexp/replace:replace",
                &[("string", "a1b"), ("old", r"\d"), ("new", "#")],
            )
            .as_deref(),
            Some("a#b"),
            "the slashed spelling is what the evaluator actually passes",
        );
        // Same for the plain replace, whose three-part form was mangled too.
        assert_eq!(
            result_of(
                "str:index/replace:replace",
                &[("string", "a-b"), ("old", "-"), ("new", "_")],
            )
            .as_deref(),
            Some("a_b"),
        );
        assert!(handles("str:regexp/replace:replace"));
        assert!(handles("str:index/trimPrefix:trimPrefix"));
    }

    /// Both spellings must agree, or which one the caller happens to pass
    /// would change the answer.
    #[test]
    fn test_both_spellings_agree() {
        let cases: &[(&str, &str, &str, &str)] = &[
            ("str:regexp:replace", "str:regexp/replace:replace", r"\d+", "#"),
            ("str:index:replace", "str:index/replace:replace", "a", "z"),
        ];
        for (plain, slashed, old, new) in cases {
            let a = result_of(plain, &[("string", "a1b2"), ("old", old), ("new", new)]);
            let b = result_of(slashed, &[("string", "a1b2"), ("old", old), ("new", new)]);
            assert_eq!(a, b, "{plain} and {slashed} disagree");
            assert!(a.is_some());
        }
    }

    /// The collapse is narrow on purpose: only `str`, only `pkg:mod/fn:fn`.
    #[test]
    fn test_provider_form_does_not_rewrite_anything_else() {
        for untouched in [
            "gcp:compute/getNetwork:getNetwork", // a genuinely bridged token
            "str:regexp/replace:match",          // tail != func, not redundant
            "str:index:replace",                 // already the provider form
            "str:replace",
            "aws:index/getAmi:getAmi",
            "pulumi:pulumi:getResource",
            "str",
            "",
        ] {
            assert_eq!(
                provider_form(untouched).as_ref(),
                untouched,
                "provider_form rewrote {untouched:?}",
            );
        }
        // And a bridged token from another package is still not ours.
        assert!(!handles("gcp:compute/getNetwork:getNetwork"));
    }
}

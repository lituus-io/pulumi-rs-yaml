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
//! `match` and `split` are handled too. `split` is the stdlib algorithm ported
//! step for step rather than delegated: Rust's `Regex::split` is a DIFFERENT
//! function, and quietly so — on the empty pattern Go yields `["f","o","o"]`
//! where Rust yields `["", "f","o","o", ""]`. Go's own `splitTests` table is
//! reproduced verbatim in the tests below, which is what "1:1" is measured
//! against.

use super::value::Value;
use regex::Regex;
use std::borrow::Cow;
use std::collections::HashMap;

/// The output property `replace`, `trim*` and `split` return.
const RESULT: &str = "result";

/// `match` is the one function that names its output differently.
const MATCHES: &str = "matches";

/// Absent `count`, which `Split` takes as "every substring".
const SPLIT_ALL: i64 = -1;

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

fn ok_named(key: &str, value: Value<'static>) -> Option<HashMap<String, Value<'static>>> {
    let mut out = HashMap::with_capacity(1);
    out.insert(key.to_string(), value);
    Some(out)
}

/// Read `count`, distinguishing "absent" from "present but unusable".
///
/// `Ok(None)` means the caller omitted it and Go's default of every substring
/// applies. `Err(())` means it was supplied in a shape this cannot honour —
/// a non-number, a fractional number, or the `count <= 0` the provider raises
/// an error for. Every one of those defers, so the provider produces its own
/// behaviour (including its own error message) rather than this guessing at one.
#[allow(clippy::result_unit_err)]
fn split_count(args: &HashMap<String, Value<'static>>) -> Result<Option<i64>, ()> {
    match args.get("count") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => {
            let truncated = *n as i64;
            // Reject a fractional count rather than silently flooring it.
            if truncated as f64 != *n || truncated <= 0 {
                return Err(());
            }
            Ok(Some(truncated))
        }
        _ => Err(()),
    }
}

/// Go's `regexp.Regexp.Split`, ported rather than delegated.
///
/// Rust's `Regex::split` is NOT the same function, and quietly so. Splitting
/// `"foobar"` on the empty pattern gives Go `["f","o","o","b","a","r"]` and
/// Rust `["", "f","o","o","b","a","r", ""]`; Go also returns `[""]` for an
/// empty subject with a non-empty pattern, and nothing at all for `n == 0`.
/// Those are precisely the differences a template would never notice until the
/// resulting list changed length.
///
/// This is the stdlib algorithm step for step: the `n == 0` and empty-subject
/// early exits, the `n-1` cap that leaves the tail unsplit, the rule that a
/// match ENDING at offset 0 contributes no leading element, and the trailing
/// remainder emitted only when the last match did not reach the end.
///
/// Borrows throughout — the returned slices point into `s`, so the split
/// itself allocates only the vector.
fn go_split<'h>(re: &Regex, s: &'h str, n: i64) -> Vec<&'h str> {
    if n == 0 {
        return Vec::new();
    }
    if !re.as_str().is_empty() && s.is_empty() {
        return vec![""];
    }

    // `n` is attacker-reachable: it comes from `count` in a template, arrives
    // as an f64 and can be astronomically large. Reserving it directly would
    // ask the allocator for terabytes and abort the language host — one number
    // in a template taking a deploy down. A split of a string of length L
    // yields at most L+1 parts, so that is the real bound, and reserving past
    // it could never have helped.
    let cap = if n > 0 {
        (n as usize).min(s.len().saturating_add(1))
    } else {
        8
    };
    let mut out: Vec<&str> = Vec::with_capacity(cap);
    let mut beg = 0usize;
    let mut end = 0usize;

    for m in re.find_iter(s) {
        // `n > 0` keeps at most n substrings, the last being the remainder.
        if n > 0 && out.len() >= (n as usize) - 1 {
            break;
        }
        end = m.start();
        // A match ending at 0 is an empty match at the very start: Go emits no
        // leading "" for it, which is why an empty pattern does not produce
        // one here either.
        if m.end() != 0 {
            out.push(&s[beg..end]);
        }
        beg = m.end();
    }

    if end != s.len() {
        out.push(&s[beg..]);
    }
    out
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
pub fn handles(token: &str) -> bool {
    matches!(
        provider_form(token).as_ref(),
        "str:index:replace"
            | "str:index:trimPrefix"
            | "str:index:trimSuffix"
            | "str:regexp:replace"
            | "str:regexp:match"
            | "str:regexp:split"
    )
}

/// Evaluate a `str` function in process, or return `None` to defer to the engine.
///
/// `None` is the "not ours" answer and is always safe: the caller falls back to
/// the normal invoke path, so an unknown token, an unresolved argument, or a
/// preview-time `Unknown` all behave exactly as before.
pub fn try_invoke(
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
        // regexp.MatchString(pattern, s)
        //
        // The one function whose output is not `result`: the provider returns
        // `matches`, a boolean, and a template reading `${x.result}` here would
        // get null from the provider too.
        "str:regexp:match" => {
            let s = arg(args, "string")?;
            let pattern = arg(args, "pattern")?;
            let re = Regex::new(pattern).ok()?;
            ok_named(MATCHES, Value::Bool(re.is_match(s)))
        }
        // regexp.MustCompile(on).Split(s, count)
        //
        // `count` absent means every substring. The provider REJECTS
        // `count <= 0` with an error rather than passing it to Go, so a
        // non-positive or fractional count defers and lets that error happen
        // instead of inventing a list.
        "str:regexp:split" => {
            let s = arg(args, "string")?;
            let on = arg(args, "on")?;
            let n = split_count(args).ok()?.unwrap_or(SPLIT_ALL);
            let re = Regex::new(on).ok()?;
            let parts = go_split(&re, s, n);
            let mut list = Vec::with_capacity(parts.len());
            list.extend(
                parts
                    .into_iter()
                    .map(|p| Value::String(p.to_string().into())),
            );
            ok_named(RESULT, Value::List(list))
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
            // Every regexp function is handled now; these are the tokens that
            // genuinely are not ours.
            "str:index:split",
            "str:regexp:find",
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
            ("abc", "x", "y", "abc"), // no match -> unchanged
            ("aaa", "a", "b", "bbb"), // ALL matches, not just the first
            ("a-b", "-", "", "ab"),   // empty replacement deletes
            ("2026-08-26", r"(\d+)-(\d+)-(\d+)", "$3/$2/$1", "26/08/2026"),
            (
                "john smith",
                r"(?P<f>\w+) (?P<l>\w+)",
                "${l}, ${f}",
                "smith, john",
            ),
            ("price", "^", "$$", "$price"), // $$ is a literal dollar
            ("ab", "", "-", "-a-b-"),       // empty pattern: every boundary
            ("héllo", "é", "e", "hello"),   // multi-byte safe
        ];
        for (s, old, new, expected) in cases {
            assert_eq!(
                result_of(
                    "str:regexp:replace",
                    &[("string", s), ("old", old), ("new", new)]
                )
                .as_deref(),
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
        assert!(handles("str:regexp:match"));
        assert!(handles("str:regexp:split"));
        // The whole `str` surface is answered here now, so the plugin that
        // segfaults on Cancel is never loaded for any of it.
        assert!(!handles("str:regexp:find"), "not a str function");
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
            "\0",
            "\u{feff}",
            "🙂🙂",
            long.as_str(),
            "\\",
            "%s",
            "$1",
            "$$",
            "(",
            "[",
            "*",
            "+",
            "?",
            "{",
            r"\",
            r"(?P<n>a)",
            ".*.*.*.*",
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
            (
                "str:regexp:replace",
                "str:regexp/replace:replace",
                r"\d+",
                "#",
            ),
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

    // ================================================================== //
    // 1:1 with Go — vectors taken from the Go standard library itself    //
    // ================================================================== //
    //
    // These are not cases anyone invented for this port. `SPLIT_VECTORS` is
    // `splitTests` from Go's `src/regexp/all_test.go`, reproduced row for row,
    // and `MATCH_VECTORS` is drawn from `findTests` in `src/regexp/find_test.go`
    // (a nil match list there means MatchString is false). Testing against the
    // upstream table is the difference between "behaves how I expect" and
    // "behaves how Go does".

    fn list_of(token: &str, pairs: &[(&str, &str)]) -> Option<Vec<String>> {
        let out = try_invoke(token, &args(pairs))?;
        match out.get(RESULT) {
            Some(Value::List(items)) => Some(
                items
                    .iter()
                    .map(|v| match v {
                        Value::String(s) => s.to_string(),
                        other => panic!("split produced a non-string: {other:?}"),
                    })
                    .collect(),
            ),
            _ => None,
        }
    }

    fn matched(pattern: &str, subject: &str) -> Option<bool> {
        let out = try_invoke(
            "str:regexp:match",
            &args(&[("string", subject), ("pattern", pattern)]),
        )?;
        match out.get(MATCHES) {
            Some(Value::Bool(b)) => Some(*b),
            _ => None,
        }
    }

    /// `splitTests`, verbatim from Go's `src/regexp/all_test.go`.
    /// (s, pattern, n, expected)
    const SPLIT_VECTORS: &[(&str, &str, i64, &[&str])] = &[
        ("foo:and:bar", ":", -1, &["foo", "and", "bar"]),
        ("foo:and:bar", ":", 1, &["foo:and:bar"]),
        ("foo:and:bar", ":", 2, &["foo", "and:bar"]),
        ("foo:and:bar", "foo", -1, &["", ":and:bar"]),
        ("foo:and:bar", "bar", -1, &["foo:and:", ""]),
        ("foo:and:bar", "baz", -1, &["foo:and:bar"]),
        ("baabaab", "a", -1, &["b", "", "b", "", "b"]),
        ("baabaab", "a*", -1, &["b", "b", "b"]),
        ("baabaab", "ba*", -1, &["", "", "", ""]),
        ("foobar", "f*b*", -1, &["", "o", "o", "a", "r"]),
        ("foobar", "f+.*b+", -1, &["", "ar"]),
        ("foobooboar", "o{2}", -1, &["f", "b", "boar"]),
        ("a,b,c,d,e,f", ",", 3, &["a", "b", "c,d,e,f"]),
        ("a,b,c,d,e,f", ",", 0, &[]),
        (",", ",", -1, &["", ""]),
        (",,,", ",", -1, &["", "", "", ""]),
        ("", ",", -1, &[""]),
        ("", ".*", -1, &[""]),
        ("", ".+", -1, &[""]),
        ("", "", -1, &[]),
        ("foobar", "", -1, &["f", "o", "o", "b", "a", "r"]),
        ("abaabaccadaaae", "a*", 5, &["", "b", "b", "c", "cadaaae"]),
        (":x:y:z:", ":", -1, &["", "x", "y", "z", ""]),
    ];

    /// The algorithm itself, against Go's table. Covers `n` values the provider
    /// refuses (0 and -1 are not reachable through it), because the port must
    /// be faithful before the wrapper narrows it.
    #[test]
    fn test_go_split_reproduces_the_stdlib_table() {
        for (subject, pattern, n, expected) in SPLIT_VECTORS {
            let re = Regex::new(pattern)
                .unwrap_or_else(|e| panic!("Go compiles {pattern:?}, this does not: {e}"));
            let got = go_split(&re, subject, *n);
            assert_eq!(
                got, *expected,
                "Split({subject:?}, {pattern:?}, {n}) diverged from Go",
            );
        }
    }

    /// Rust's own `Regex::split` is NOT this function.
    ///
    /// Pinning the divergence keeps anyone from "simplifying" the port into a
    /// delegation later — the two disagree on exactly the inputs a template is
    /// least likely to have tried.
    #[test]
    fn test_rust_split_would_have_been_wrong() {
        let re = Regex::new("").unwrap();
        let rust: Vec<&str> = re.split("foobar").collect();
        let go = go_split(&re, "foobar", -1);
        assert_ne!(rust, go, "if these ever agree, re-check the port");
        assert_eq!(go, ["f", "o", "o", "b", "a", "r"]);

        // Non-empty pattern, empty subject: Go yields one empty string.
        let re = Regex::new(",").unwrap();
        assert_eq!(go_split(&re, "", -1), [""]);
    }

    /// The same table through the invoke surface, at the `count` values the
    /// provider actually permits: absent (all) and >= 1.
    #[test]
    fn test_split_invoke_matches_go_where_count_is_permitted() {
        for (subject, pattern, n, expected) in SPLIT_VECTORS {
            let got = if *n == SPLIT_ALL {
                list_of("str:regexp:split", &[("string", subject), ("on", pattern)])
            } else if *n >= 1 {
                let mut a = args(&[("string", subject), ("on", pattern)]);
                a.insert("count".to_string(), Value::Number(*n as f64));
                try_invoke("str:regexp:split", &a).map(|out| match out.get(RESULT) {
                    Some(Value::List(items)) => items
                        .iter()
                        .map(|v| match v {
                            Value::String(s) => s.to_string(),
                            other => panic!("non-string: {other:?}"),
                        })
                        .collect(),
                    _ => panic!("split returned no result list"),
                })
            } else {
                continue; // count <= 0 is an error at the provider, not a value
            };
            let got = got.expect("split must be answered here");
            assert_eq!(
                got.iter().map(String::as_str).collect::<Vec<_>>(),
                *expected,
                "split({subject:?}, {pattern:?}, count={n})",
            );
        }
    }

    /// `count <= 0` and a fractional count DEFER.
    ///
    /// The provider rejects them with "count <= 0 is not allowed" rather than
    /// calling Go, so answering here would replace an error with a list.
    #[test]
    fn test_split_defers_on_counts_the_provider_rejects() {
        for bad in [0.0, -1.0, -5.0, 1.5, f64::NAN, f64::INFINITY] {
            let mut a = args(&[("string", "a,b,c"), ("on", ",")]);
            a.insert("count".to_string(), Value::Number(bad));
            assert!(
                try_invoke("str:regexp:split", &a).is_none(),
                "count={bad} must defer so the provider raises its own error",
            );
        }
        // A count of the wrong TYPE defers too.
        let mut a = args(&[("string", "a,b,c"), ("on", ",")]);
        a.insert("count".to_string(), Value::String("2".into()));
        assert!(try_invoke("str:regexp:split", &a).is_none());
    }

    /// From `findTests` in Go's `src/regexp/find_test.go`: a nil match list
    /// there is exactly `MatchString == false`.
    const MATCH_VECTORS: &[(&str, &str, bool)] = &[
        ("", "", true),
        ("^abcdefg", "abcdefg", true),
        ("a+", "baaab", true),
        ("a", "bababaab", true),
        ("abcd..", "abcdef", true),
        ("x", "y", false),
        (".", "a", true),
        (".*", "abcdef", true),
        ("^", "abcde", true),
        ("$", "abcde", true),
        ("^abcd$", "abcd", true),
        ("^bcd'", "abcdef", false),
        ("^abcd$", "abcde", false),
        ("a*", "baaab", true),
        ("[a-z]+", "abcd", true),
        ("[^a-z]+", "ab1234cd", true),
        (r"[a\-\]z]+", "az]-bcz", true),
        (r"[^\n]+", "abcd\n", true),
        ("[日本語]+", "日本語日本語", true),
        ("日本語+", "日本語", true),
        ("()", "", true),
        ("(a)", "a", true),
        ("(.*)", "", true),
        ("((a|b|c)*(d))", "abcd", true),
        (r"\a\f\n\r\t\v", "\u{7}\u{c}\n\r\t\u{b}", true),
        ("[.]", ".", true),
        ("(.*).*", "ab", true),
    ];

    #[test]
    fn test_match_reproduces_go_findtests() {
        for (pattern, subject, expected) in MATCH_VECTORS {
            assert_eq!(
                matched(pattern, subject),
                Some(*expected),
                "MatchString({pattern:?}, {subject:?})",
            );
        }
    }

    /// `match` names its output `matches`, not `result` — the provider does,
    /// so a template written against the provider must keep working.
    #[test]
    fn test_match_output_is_named_matches() {
        let out = try_invoke(
            "str:regexp:match",
            &args(&[("string", "abc"), ("pattern", "b")]),
        )
        .expect("match must be answered here");
        assert_eq!(out.len(), 1);
        assert!(matches!(out.get(MATCHES), Some(Value::Bool(true))));
        assert!(!out.contains_key(RESULT), "match must not emit `result`");
    }

    /// Both regexp functions accept the canonicalized spelling too, since that
    /// is what the evaluator actually passes.
    #[test]
    fn test_match_and_split_accept_the_canonicalized_spelling() {
        assert_eq!(
            try_invoke(
                "str:regexp/match:match",
                &args(&[("string", "abc"), ("pattern", "b")]),
            )
            .and_then(|o| o.get(MATCHES).cloned()),
            Some(Value::Bool(true)),
        );
        assert_eq!(
            list_of("str:regexp/split:split", &[("string", "a,b"), ("on", ",")],).as_deref(),
            Some(&["a".to_string(), "b".to_string()][..]),
        );
        assert!(handles("str:regexp/match:match"));
        assert!(handles("str:regexp/split:split"));
    }

    /// Uncompilable and non-RE2 patterns defer for these two as well.
    #[test]
    fn test_match_and_split_defer_on_patterns_go_would_reject() {
        for bad in [r"(unclosed", r"(a)\1", r"(?=foo)", r"a{2,1}"] {
            assert!(
                try_invoke(
                    "str:regexp:match",
                    &args(&[("string", "abc"), ("pattern", bad)]),
                )
                .is_none(),
                "match must defer on {bad:?}",
            );
            assert!(
                try_invoke("str:regexp:split", &args(&[("string", "abc"), ("on", bad)]),).is_none(),
                "split must defer on {bad:?}",
            );
        }
    }

    /// Unresolved (preview) or missing arguments defer, like every other
    /// function here.
    #[test]
    fn test_match_and_split_defer_on_unresolved_args() {
        let mut unknown = args(&[("pattern", "a")]);
        unknown.insert("string".to_string(), Value::Unknown);
        assert!(try_invoke("str:regexp:match", &unknown).is_none());

        let mut unknown = args(&[("on", ",")]);
        unknown.insert("string".to_string(), Value::Unknown);
        assert!(try_invoke("str:regexp:split", &unknown).is_none());

        assert!(try_invoke("str:regexp:match", &args(&[("string", "a")])).is_none());
        assert!(try_invoke("str:regexp:split", &args(&[("string", "a")])).is_none());
    }

    /// Multi-byte subjects must split on character boundaries, never bytes —
    /// slicing mid-codepoint would panic inside the language host.
    #[test]
    fn test_split_is_utf8_safe() {
        assert_eq!(
            list_of("str:regexp:split", &[("string", "日,本,語"), ("on", ",")]).as_deref(),
            Some(&["日".to_string(), "本".to_string(), "語".to_string()][..]),
        );
        // Empty pattern over multi-byte input: one element per CHARACTER.
        let re = Regex::new("").unwrap();
        assert_eq!(go_split(&re, "日本語", -1), ["日", "本", "語"]);
    }

    /// Neither may panic on adversarial input — they run in the language host.
    #[test]
    fn test_match_and_split_never_panic() {
        let long = "a".repeat(4096);
        let nasty = [
            "\0",
            "\u{feff}",
            "🙂🙂",
            long.as_str(),
            "\\",
            "%s",
            "$1",
            "(",
            "[",
            "*",
            "+",
            "?",
            "{",
            r"\",
            r"(?P<n>a)",
            ".*.*.*.*",
            "",
        ];
        for probe in nasty {
            let _ = matched("a", probe);
            let _ = matched(probe, "abc");
            let _ = list_of("str:regexp:split", &[("string", probe), ("on", ",")]);
            let _ = list_of("str:regexp:split", &[("string", "a,b"), ("on", probe)]);
        }
    }

    // ================================================================== //
    // Cost                                                               //
    // ================================================================== //
    //
    // Measured on this machine: compiling a pattern costs 3.6-59us and the
    // operation itself 0.5-1.6us, so compilation dominates. It is still not
    // worth caching. The path this replaced was a plugin process launch plus a
    // gRPC round trip — tens of milliseconds — so even the worst pattern here
    // is ~1000x cheaper, and a cache would buy microseconds at the cost of
    // shared mutable state on a path the evaluator may run in parallel. The
    // trade is not close, and these tests pin the properties that make it hold
    // rather than the microseconds themselves, which are runner weather.

    /// Linear time is a GUARANTEE here, not an observation.
    ///
    /// Both engines are RE2 lineage, so a pattern that would pin a
    /// backtracking engine for exponential time completes promptly. This is
    /// what makes it safe to run untrusted template patterns inside the
    /// language host at all — the budget is deliberately loose because the
    /// failure it catches is exponential, not slow.
    #[test]
    fn test_pathological_patterns_stay_linear() {
        use std::time::Instant;

        let subject = "a".repeat(64);
        let cases = [r"(a+)+$", r"(a|a)*$", r"(a*)*b", r"(.*)*x"];
        for pattern in cases {
            let re =
                Regex::new(pattern).unwrap_or_else(|e| panic!("{pattern:?} must compile: {e}"));
            let started = Instant::now();
            let _ = re.is_match(&subject);
            let _ = go_split(&re, &subject, -1);
            let elapsed = started.elapsed();
            assert!(
                elapsed.as_millis() < 250,
                "{pattern:?} took {elapsed:?} — backtracking behaviour has appeared",
            );
        }
    }

    /// Splitting borrows from the subject; only the output vector allocates.
    ///
    /// Asserted structurally by pointer identity rather than by timing: every
    /// returned slice must point INTO the input, which is only true while the
    /// port stays allocation-free.
    #[test]
    fn test_split_borrows_from_the_subject() {
        let subject = "alpha,beta,gamma";
        let re = Regex::new(",").unwrap();
        let parts = go_split(&re, subject, -1);
        assert_eq!(parts, ["alpha", "beta", "gamma"]);

        let base = subject.as_ptr() as usize;
        let end = base + subject.len();
        for part in &parts {
            let at = part.as_ptr() as usize;
            assert!(
                at >= base && at <= end,
                "{part:?} was copied out of the subject instead of borrowed",
            );
        }
    }

    /// Cost tracks the input, not the call count.
    ///
    /// One large split must not cost dramatically less than many small ones
    /// beyond the fixed compile — the shape a hidden per-call setup would take.
    #[test]
    fn test_split_scales_with_input() {
        use std::time::Instant;

        let re = Regex::new(",").unwrap();
        let unit = "a,b,c,d,e,f,g,h";
        let big = unit.repeat(200);

        let started = Instant::now();
        for _ in 0..200 {
            let _ = go_split(&re, unit, -1);
        }
        let many = started.elapsed();

        let started = Instant::now();
        let _ = go_split(&re, &big, -1);
        let once = started.elapsed();

        assert!(
            many < once * 40 + std::time::Duration::from_millis(50),
            "200 small splits ({many:?}) dwarf one equivalent large split \
             ({once:?}) — per-call setup has appeared",
        );
    }
}

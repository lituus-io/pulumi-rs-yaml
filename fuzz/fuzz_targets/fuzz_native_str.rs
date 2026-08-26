// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

//! Fuzz target: the `str` package's functions evaluated in process.
//!
//! Every argument here comes from a template: the pattern, the subject, the
//! replacement template and the split count are all author-controlled, and the
//! code runs INSIDE the language host. A panic is not an error returned to the
//! user — it is a dead deploy — so "never unwinds" is the primary property.
//!
//! Security targets:
//! - Unbounded allocation through `count`, which reaches a capacity
//!   reservation and arrives as an f64 that can be astronomically large.
//! - Catastrophic backtracking. Both this crate and Go's regexp are RE2
//!   lineage, so it should be impossible — this is the standing proof.
//! - Slicing a multi-byte subject off a character boundary, which panics.
//! - `$`-expansion in the replacement reaching a group that does not exist.
//!
//! Invariants beyond not panicking:
//! - a split part is always a substring of the subject, never invented text;
//! - a split never returns more parts than a positive `count` asked for;
//! - `match` answers with `matches`, the others with `result` — a template
//!   reading the wrong key would silently see null.

#![no_main]
use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use std::collections::HashMap;

use pulumi_rs_yaml_core::eval::native_str::try_invoke;
use pulumi_rs_yaml_core::eval::value::Value;

#[derive(Debug, Arbitrary)]
enum Which {
    Match,
    Replace,
    Split,
    SplitWithCount(i32),
    /// A token that is not ours, to keep the decline path exercised.
    Unhandled,
}

#[derive(Debug, Arbitrary)]
struct Input {
    which: Which,
    pattern: String,
    subject: String,
    replacement: String,
    /// Exercises the canonicalized spelling the evaluator actually passes.
    slashed: bool,
}

fn args(pairs: &[(&str, &str)]) -> HashMap<String, Value<'static>> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), Value::String((*v).to_string().into())))
        .collect()
}

fuzz_target!(|input: Input| {
    // A pathological pattern length is the fuzzer's own denial of service, not
    // a finding: the compiler rejects oversized patterns anyway.
    if input.pattern.len() > 4096 || input.subject.len() > 65536 {
        return;
    }

    let (token, mut a) = match input.which {
        Which::Match => (
            if input.slashed { "str:regexp/match:match" } else { "str:regexp:match" },
            args(&[("string", &input.subject), ("pattern", &input.pattern)]),
        ),
        Which::Replace => (
            if input.slashed { "str:regexp/replace:replace" } else { "str:regexp:replace" },
            args(&[
                ("string", &input.subject),
                ("old", &input.pattern),
                ("new", &input.replacement),
            ]),
        ),
        Which::Split | Which::SplitWithCount(_) => (
            if input.slashed { "str:regexp/split:split" } else { "str:regexp:split" },
            args(&[("string", &input.subject), ("on", &input.pattern)]),
        ),
        Which::Unhandled => (
            "str:regexp:find",
            args(&[("string", &input.subject), ("pattern", &input.pattern)]),
        ),
    };

    let mut requested: Option<i64> = None;
    if let Which::SplitWithCount(n) = input.which {
        // i32 spans the rejected non-positive range AND values far past any
        // plausible number of parts, which is the allocation concern.
        a.insert("count".to_string(), Value::Number(n as f64));
        requested = Some(n as i64);
    }

    let Some(out) = try_invoke(token, &a) else {
        // Declining is always allowed: it is the "not ours / cannot honour
        // this" answer, and the provider path takes over.
        return;
    };

    assert_eq!(out.len(), 1, "{token} returned {} keys", out.len());

    match input.which {
        Which::Unhandled => panic!("an unhandled token must never be answered"),
        Which::Match => {
            assert!(
                matches!(out.get("matches"), Some(Value::Bool(_))),
                "match must answer with a boolean `matches`",
            );
        }
        Which::Replace => {
            assert!(
                matches!(out.get("result"), Some(Value::String(_))),
                "replace must answer with a string `result`",
            );
        }
        Which::Split | Which::SplitWithCount(_) => {
            let Some(Value::List(items)) = out.get("result") else {
                panic!("split must answer with a list `result`");
            };
            if let Some(n) = requested {
                // A non-positive count must have been declined above.
                assert!(n >= 1, "count={n} should not have been answered");
                assert!(
                    items.len() <= n as usize,
                    "split returned {} parts for count={n}",
                    items.len(),
                );
            }
            for item in items {
                let Value::String(part) = item else {
                    panic!("split produced a non-string part");
                };
                // Every part must come OUT of the subject. Invented text here
                // would mean the port had started synthesising content.
                assert!(
                    part.is_empty() || input.subject.contains(part.as_ref()),
                    "split produced {part:?}, which is not in the subject",
                );
            }
        }
    }
});

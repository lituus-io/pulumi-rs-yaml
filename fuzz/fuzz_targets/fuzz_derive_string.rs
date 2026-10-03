// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

//! Fuzz target: `fn::deriveString`.
//!
//! The value this produces becomes a resource name, and all three of its
//! arguments come from the program author. So the properties are total ones:
//! no combination may panic, spin, emit a character the alphabet did not
//! offer, produce the wrong number of characters, or answer differently on a
//! second call.
//!
//! Targets:
//! - termination of the rejection-sampling draw, whose discard band is the
//!   subtle part: an alphabet size whose `limit` left no room would spin
//! - character-boundary selection on a multi-byte alphabet, where indexing
//!   bytes instead of characters would produce something that is not a string
//! - the length cap, the one allocation an argument can drive
//! - purity, which is the whole reason this builtin exists over
//!   `fn::randomString`

#![no_main]
use libfuzzer_sys::fuzz_target;
use pulumi_rs_yaml_core::diag::Diagnostics;
use pulumi_rs_yaml_core::eval::builtins::eval_derive_string;
use pulumi_rs_yaml_core::eval::value::Value;
use std::borrow::Cow;

#[derive(arbitrary::Arbitrary, Debug)]
struct Input<'a> {
    from: &'a str,
    length: u16,
    alphabet: &'a str,
    /// Exercises the shorthand form as well as the object form.
    shorthand: bool,
}

fuzz_target!(|input: Input<'_>| {
    let arg = if input.shorthand {
        Value::String(Cow::Borrowed(input.from))
    } else {
        Value::Object(vec![
            (Cow::Borrowed("from"), Value::String(Cow::Borrowed(input.from))),
            (Cow::Borrowed("length"), Value::Number(f64::from(input.length))),
            (
                Cow::Borrowed("alphabet"),
                Value::String(Cow::Borrowed(input.alphabet)),
            ),
        ])
    };

    let mut diags = Diagnostics::default();
    let Some(out) = eval_derive_string(&arg, &mut diags) else {
        // Refusal is always allowed, but it must say why.
        assert!(diags.has_errors(), "refused without a diagnostic");
        return;
    };
    let Value::String(s) = out else {
        panic!("a derived value must be a string, got {out:?}");
    };

    let want_len = if input.shorthand {
        8
    } else {
        usize::from(input.length)
    };
    let alphabet = if input.shorthand {
        "0123456789abcdefghijklmnopqrstuvwxyz"
    } else {
        input.alphabet
    };

    // Exactly the requested number of CHARACTERS, not bytes.
    assert_eq!(
        s.chars().count(),
        want_len,
        "wrong length for from={:?} length={} alphabet={:?}",
        input.from,
        want_len,
        alphabet
    );

    // Nothing the author did not offer.
    for c in s.chars() {
        assert!(
            alphabet.contains(c),
            "{c:?} is not in the alphabet {alphabet:?}"
        );
    }

    // Purity: the same arguments, the same answer. This is the property
    // `fn::randomString` lacks and the reason this builtin exists.
    let mut again = Diagnostics::default();
    let second = eval_derive_string(&arg, &mut again).expect("derives twice");
    assert_eq!(Value::String(s), second, "the derivation is not pure");
});

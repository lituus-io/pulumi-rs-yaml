// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

// The load-bearing invariant: protect and restore are exact inverses.
//
// `restore(protect(s)) == s` for every input, with no rendering in between.
// Any extent error — off by a line, wrong indentation column, swallowing a
// sibling key, mis-measuring an indentation indicator — breaks byte equality
// here, and the failing case minimises to something readable.
//
// This runs on arbitrary text rather than generated stacks on purpose: the
// invariant has no preconditions, so anything that is valid UTF-8 is fair game.
#![no_main]

use libfuzzer_sys::fuzz_target;
use pulumi_rs_yaml_core::provider_scope::{protect, restore, Protected};

const LISTS: &[&[&str]] = &[
    &[],
    &["gcpx"],
    &["gcpx", "aws"],
    &["type"],
    &["sql"],
    &["a"],
];

fuzz_target!(|data: &[u8]| {
    let Ok(source) = std::str::from_utf8(data) else {
        return;
    };
    for packages in LISTS {
        let protected = match protect(source, packages) {
            Ok(p) => p,
            // A refusal is a legitimate outcome; it is the third outcome —
            // quietly altered text — that must not exist.
            Err(_) => continue,
        };
        let regions = protected.regions();
        if matches!(protected, Protected::Unchanged) {
            assert!(regions.is_empty(), "Unchanged must carry no regions");
            continue;
        }
        let marked = protected.source(source);
        assert!(
            !regions.is_empty(),
            "Marked must carry at least one region:\n{source:?}"
        );
        let back = restore(marked, regions).expect("restore must accept the markers protect wrote");
        assert_eq!(
            back.as_ref(),
            source,
            "round trip altered the source\npackages: {packages:?}\nmarked:\n{marked:?}"
        );
    }
});

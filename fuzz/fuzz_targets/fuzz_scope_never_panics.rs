// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

// A panic here takes down the language host in the middle of a deploy, with a
// half-applied stack and a backtrace instead of a diagnostic. Every entry point
// therefore has to answer, on any input, with a value or a refusal.
//
// Arbitrary bytes, including the ones that are not valid UTF-8 — the boundary
// has to reject those rather than slice through a multi-byte character — and
// restore is additionally handed markers that do not match its regions, which
// is what a mangled render would produce.
#![no_main]

use libfuzzer_sys::fuzz_target;
use pulumi_rs_yaml_core::provider_scope::{
    packages_from_source, protect, restore, Protected, Region, MARKER_PREFIX,
};

const LISTS: &[&[&str]] = &[&[], &["gcpx"], &["gcpx", "aws"], &[""], &[":"], &["|"]];

fuzz_target!(|data: &[u8]| {
    // Invalid UTF-8 must be turned away at the edge, not sliced.
    let lossy = String::from_utf8_lossy(data);
    let source = lossy.as_ref();

    let _ = packages_from_source(source);

    for packages in LISTS {
        let protected = match protect(source, packages) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let marked = protected.source(source).to_owned();
        let regions = protected.regions();
        let _ = restore(&marked, regions);

        // What a mangled render looks like: the right markers, the wrong
        // regions. Refusing is fine; panicking is not.
        let _ = restore(&marked, &[]);
        if let Some(first) = regions.first() {
            let one: [Region; 1] = [*first];
            let _ = restore(&marked, &one);
        }
        // And a marker the pass never wrote.
        let injected = format!("{marked}\n  {MARKER_PREFIX}999999\u{0}\n");
        let _ = restore(&injected, regions);
        // A marker with a non-numeric id, and one that is not alone.
        let bad = format!("{marked}\n  {MARKER_PREFIX}abc\u{0}\n  x {MARKER_PREFIX}0\u{0} y\n");
        let _ = restore(&bad, regions);

        if matches!(protected, Protected::Unchanged) {
            assert!(regions.is_empty());
        }
    }
});

// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

// The differential oracle: is the extent the one a real YAML parser sees?
//
// Round tripping proves protect and restore are inverses. It does not prove
// they took the right bytes — an extent can be wrong in both directions at once
// and still round trip. This target answers the other question by parsing the
// document with serde_yaml, which is the parser Pulumi's own validation uses,
// and checking three things:
//
//   1. The protected document still parses, with exactly the same structure.
//      Swallowing a sibling key removes it from the tree; truncating an extent
//      leaves stray lines behind. Both change the shape.
//   2. Exactly the protected scalars differ, and each differs by being a
//      marker — nothing else in the document moved.
//   3. Each protected region, re-parsed on its own with its header line,
//      yields the value the original document had at that position.
//
// And the completeness half: the generator knows how many block scalars it put
// inside scoped resources, so protecting fewer is a miss and protecting more
// means the scope leaked into a package that was not listed.
//
// serde_yaml cannot be used in the pass itself — the source may not be valid
// YAML until Jinja has run — but it is an excellent oracle for the subset that
// already is.
#![no_main]

use arbitrary::Unstructured;
use libfuzzer_sys::fuzz_target;
use pulumi_rs_yaml_core::provider_scope::{protect, Protected, Region, MARKER_PREFIX};
use serde_yaml::Value;

#[path = "scope_grammar.rs"]
mod grammar;

/// Strips up to `n` leading spaces from every line, so a nested block scalar
/// can be re-parsed at the document root with its indentation indicator still
/// meaning what it meant.
fn dedent(s: &str, n: usize) -> String {
    let mut out = String::with_capacity(s.len());
    for line in s.split_inclusive('\n') {
        let mut k = 0;
        while k < n && line.as_bytes().get(k) == Some(&b' ') {
            k += 1;
        }
        out.push_str(&line[k..]);
    }
    out
}

/// Walks two parsed documents in step. Returns false if the shapes differ;
/// otherwise records every position whose scalar changed.
fn walk(a: &Value, c: &Value, path: &str, diffs: &mut Vec<(String, Value, String)>) -> bool {
    match (a, c) {
        (Value::Mapping(ma), Value::Mapping(mc)) => {
            if ma.len() != mc.len() {
                return false;
            }
            for ((ka, va), (kc, vc)) in ma.iter().zip(mc.iter()) {
                if ka != kc {
                    return false;
                }
                let key = ka.as_str().unwrap_or("?");
                if !walk(va, vc, &format!("{path}.{key}"), diffs) {
                    return false;
                }
            }
            true
        }
        (Value::Sequence(sa), Value::Sequence(sc)) => {
            if sa.len() != sc.len() {
                return false;
            }
            for (i, (va, vc)) in sa.iter().zip(sc.iter()).enumerate() {
                if !walk(va, vc, &format!("{path}[{i}]"), diffs) {
                    return false;
                }
            }
            true
        }
        (Value::String(x), Value::String(y)) => {
            if x != y {
                diffs.push((path.to_owned(), a.clone(), y.clone()));
            }
            true
        }
        _ => a == c,
    }
}

/// A fuzz target that returns early on every input proves nothing. Set
/// `SCOPE_FUZZ_TRACE=1` to print how many inputs reached each stage, so the
/// coverage claim can be checked rather than assumed.
fn trace(stage: usize) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static COUNTS: [AtomicUsize; 4] = [
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
    ];
    if std::env::var_os("SCOPE_FUZZ_TRACE").is_none() {
        return;
    }
    let n = COUNTS[stage].fetch_add(1, Ordering::Relaxed) + 1;
    if stage == 0 && n % 500 == 0 {
        eprintln!(
            "TRACE generated={} parsed={} protected={} regions-checked={}",
            COUNTS[0].load(Ordering::Relaxed),
            COUNTS[1].load(Ordering::Relaxed),
            COUNTS[2].load(Ordering::Relaxed),
            COUNTS[3].load(Ordering::Relaxed),
        );
    }
}

fuzz_target!(|data: &[u8]| {
    let mut u = Unstructured::new(data);
    let Ok(stack) = grammar::build(&mut u) else {
        return;
    };
    let source = stack.text.as_str();
    trace(0);

    // The oracle only speaks for documents it can parse.
    let Ok(before) = serde_yaml::from_str::<Value>(source) else {
        return;
    };
    trace(1);

    let protected = match protect(source, grammar::SCOPED) {
        Ok(p) => p,
        // A refusal is correct where an extent cannot be determined; the
        // generator says when it planted such a case.
        Err(_) => {
            assert!(
                stack.bad_indicator,
                "refused a document with no deliberate defect:\n{source}"
            );
            return;
        }
    };
    let regions: &[Region] = protected.regions();

    assert_eq!(
        regions.len(),
        stack.planted,
        "protected {} block scalars, {} were planted in scoped resources:\n{source}",
        regions.len(),
        stack.planted
    );
    if matches!(protected, Protected::Unchanged) {
        return;
    }
    trace(2);

    // serde_yaml will not accept a NUL, so give the markers a printable form.
    // Same lines, same columns, same structure.
    let mut visible = protected.source(source).to_owned();
    for id in 0..regions.len() {
        visible = visible.replace(&format!("{MARKER_PREFIX}{id}\u{0}"), &format!("PTMARK{id}"));
    }
    let after = serde_yaml::from_str::<Value>(&visible).unwrap_or_else(|e| {
        panic!("protection broke a parseable document: {e}\n--- before ---\n{source}\n--- after ---\n{visible}")
    });

    let mut diffs = Vec::new();
    assert!(
        walk(&before, &after, "", &mut diffs),
        "protection changed the document's shape\n--- before ---\n{source}\n--- after ---\n{visible}"
    );
    assert_eq!(
        diffs.len(),
        regions.len(),
        "{} scalars changed but {} were protected\n{source}",
        diffs.len(),
        regions.len()
    );

    let lines: Vec<&str> = source.split_inclusive('\n').collect();
    let mut seen = vec![false; regions.len()];

    for (path, original, marked) in &diffs {
        let id: usize = marked
            .trim_end_matches('\n')
            .trim()
            .strip_prefix("PTMARK")
            .and_then(|n| n.parse().ok())
            .unwrap_or_else(|| {
                panic!("scalar at {path} changed without being protected: {marked:?}\n{source}")
            });
        assert!(id < regions.len(), "marker {id} names no region");
        assert!(!seen[id], "marker {id} appeared at two positions");
        seen[id] = true;
        trace(3);

        let r = &regions[id];
        // Re-parse the region on its own, with its own header line, at the
        // document root. If the extent is right this reproduces the value the
        // full document had.
        let header = dedent(lines[r.header_line], r.parent_indent);
        let body = dedent(r.text, r.parent_indent);
        let doc = format!("{header}{body}\n");
        let Ok(reparsed) = serde_yaml::from_str::<Value>(&doc) else {
            panic!("a protected region did not re-parse on its own:\n{doc:?}\n--- source ---\n{source}");
        };
        let value = reparsed
            .as_mapping()
            .and_then(|m| m.values().next())
            .and_then(Value::as_str)
            .unwrap_or_else(|| {
                panic!("region {id} re-parsed to something that is not a scalar:\n{doc:?}")
            });

        // Trailing blank lines sit outside the region by design — they carry no
        // template syntax, and `restore` puts the document back byte for byte
        // regardless, which the round-trip target proves.
        let expected = original.as_str().unwrap_or_default();
        assert_eq!(
            value.trim_end_matches('\n'),
            expected.trim_end_matches('\n'),
            "region {id} at {path} is not the scalar the parser sees\n--- region ---\n{doc:?}\n--- source ---\n{source}"
        );
    }
});

// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

//! Fuzz target: hostile bytes into the checkpoint index.
//!
//! The index this reads decides whether one stack may delete a resource
//! another stack manages. Its inputs are whole JSON blobs pulled out of a
//! shared state backend, so every byte is somebody else's, and the reader runs
//! before any gate has had a chance to refuse.
//!
//! Security targets:
//! - Panics or a stack overflow out of hostile JSON — nesting spirals, giant
//!   arrays, truncated documents, invalid UTF-8 inside a string.
//! - A fail-open `Ok` for something that is not a checkpoint. An empty index
//!   and an unread document are the same value to the caller, and the second
//!   one authorises a delete, so "read it wrong" and "read it not at all" have
//!   to be distinguishable.
//! - Non-determinism between the single and the batched read, which would make
//!   the gate's answer depend on how the caller happened to schedule it.
//!
//! Invariants beyond not panicking:
//! - the same bytes always produce the same answer;
//! - an `Ok` implies `serde_json` can also scan those bytes structurally — the
//!   reader never invents a document out of something that is not JSON. The
//!   oracle is `IgnoredAny`, not `Value`, and the difference is the point: the
//!   reader does not decode the fields it does not return, so a byte sequence
//!   that is not UTF-8 inside `inputs` or `outputs` is invisible to it, exactly
//!   as it is to serde_json's own structural scan. `Value` is stricter only
//!   because it materialises every string. The strings the reader does hand
//!   back are `str`, so they are valid by construction, and a bad byte in one
//!   of those is an error;
//! - filtering changes which entries survive and nothing else: the filtered
//!   entries are exactly the unfiltered ones that `IdFilter::matches` accepts,
//!   in the same order, with the same shape. A filter that silently dropped an
//!   entry would hide an owner;
//! - asking for an element changes what an entry carries and never which
//!   entries there are: the ids and urns are the same, in the same order, as a
//!   scan that asked for nothing. An element is present only for a resource
//!   whose type was named, its keys are a subset of the keys requested for
//!   that type, in the order the document wrote them, and each value is a
//!   slice of the document handed in rather than a copy of it. A scan that
//!   names no type at all is byte for byte the scan that asked for nothing —
//!   that is the property an older caller on a newer reader depends on;
//! - reading the same document four times in one batch, on four threads,
//!   agrees with reading it once.

#![no_main]
use libfuzzer_sys::fuzz_target;

use std::collections::HashSet;

use pulumi_rs_yaml_core::checkpoint::{
    index_checkpoint, index_checkpoint_with_elements, index_checkpoints, ElementSpec, IdFilter,
};

/// The types an element could be asked for. The fuzzer's documents carry
/// whatever types it invented, so the spec deliberately names a few that will
/// mostly miss: the interesting cases are the near-misses, and a hit only has
/// to happen sometimes for the projection properties to be exercised.
const ELEMENT_TYPES: [(&str, [&str; 4]); 2] = [
    (
        "gcp:bigquery/datasetAccess:DatasetAccess",
        ["role", "userByEmail", "view", "authorizedDataset"],
    ),
    ("gcp:t:T", ["a", "b", "role", "view"]),
];

fn leaf_of(id: &str) -> &str {
    match id.rsplit_once('/') {
        Some((_, tail)) => tail,
        None => id,
    }
}

fuzz_target!(|data: &[u8]| {
    // Past this size the fuzzer is measuring the allocator, not the reader;
    // the 64 MiB bracket bomb is pinned as a security test instead.
    if data.len() > 256 * 1024 {
        return;
    }

    let first = index_checkpoint(data, None).map_err(|e| e.to_string());
    let second = index_checkpoint(data, None).map_err(|e| e.to_string());
    assert_eq!(first, second, "the reader is not deterministic");

    let Ok(unfiltered) = first else {
        return;
    };

    assert!(
        serde_json::from_slice::<serde::de::IgnoredAny>(data).is_ok(),
        "read as a checkpoint, but not scannable as JSON",
    );

    // Every other id, by its leaf alone, so both halves of the matching rule
    // are exercised: the ones named and the ones deliberately left out.
    let targets: Vec<&str> = unfiltered
        .entries
        .iter()
        .step_by(2)
        .map(|entry| leaf_of(&entry.id))
        .collect();
    let filter = IdFilter::new(targets.iter().copied());

    let filtered = match index_checkpoint(data, Some(&filter)) {
        Ok(index) => index,
        Err(e) => unreachable!("a filter made a readable document unreadable: {e}"),
    };
    assert_eq!(
        filtered.shape, unfiltered.shape,
        "filtering changed the document's shape",
    );
    let expected: Vec<_> = unfiltered
        .entries
        .iter()
        .filter(|entry| filter.matches(&entry.id))
        .collect();
    assert_eq!(
        filtered.entries.iter().collect::<Vec<_>>(),
        expected,
        "filtering kept a different set than IdFilter::matches accepts",
    );

    // Asking for elements never changes which entries there are, and a scan
    // that names nothing is the scan above.
    let spec = ElementSpec::new(ELEMENT_TYPES.map(|(kind, keys)| (kind, keys)));
    if let Ok(projected) = index_checkpoint_with_elements(data, None, Some(&spec)) {
        assert_eq!(
            projected.shape, unfiltered.shape,
            "a projection changed the document's shape",
        );
        assert_eq!(
            projected.entries.len(),
            unfiltered.entries.len(),
            "a projection changed how many entries there are",
        );
        for (with, without) in projected.entries.iter().zip(unfiltered.entries.iter()) {
            assert_eq!(with.id, without.id, "a projection changed an id");
            assert_eq!(with.urn, without.urn, "a projection changed a urn");
            assert!(without.element.is_none(), "an element nobody asked for");
            let Some(element) = with.element.as_ref() else {
                continue;
            };
            let Some(wanted) = ELEMENT_TYPES
                .iter()
                .find(|(_, keys)| keys.iter().any(|k| element.iter().any(|(ek, _)| ek == k)))
                .map(|(_, keys)| keys.iter().copied().collect::<HashSet<&str>>())
            else {
                assert!(element.is_empty(), "keys came back for no known type");
                continue;
            };
            let mut seen = HashSet::new();
            for (key, value) in element {
                assert!(
                    wanted.contains(key.as_ref()),
                    "a key nobody asked for was projected: {key}",
                );
                assert!(seen.insert(key.as_ref()), "a key was projected twice");
                let at = value.get().as_ptr() as usize;
                let start = data.as_ptr() as usize;
                assert!(
                    (start..start + data.len()).contains(&at),
                    "a value was copied rather than borrowed",
                );
            }
        }
    }

    let none_named = ElementSpec::default();
    assert_eq!(
        index_checkpoint_with_elements(data, None, Some(&none_named))
            .map_err(|e| e.to_string()),
        Ok(unfiltered.clone()),
        "a spec that names no type is not the scan that asked for nothing",
    );

    let batch = index_checkpoints(&[data; 4], None, 4);
    assert_eq!(batch.len(), 4, "a batch dropped a slot");
    for result in batch {
        assert_eq!(
            result.map_err(|e| e.to_string()),
            Ok(unfiltered.clone()),
            "a batched read disagreed with the single read",
        );
    }
});

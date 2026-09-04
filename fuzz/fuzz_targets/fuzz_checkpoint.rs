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
//! - reading the same document four times in one batch, on four threads,
//!   agrees with reading it once.

#![no_main]
use libfuzzer_sys::fuzz_target;

use pulumi_rs_yaml_core::checkpoint::{index_checkpoint, index_checkpoints, IdFilter};

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
        .map(|(id, _)| leaf_of(id))
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
        .filter(|(id, _)| filter.matches(id))
        .collect();
    assert_eq!(
        filtered.entries.iter().collect::<Vec<_>>(),
        expected,
        "filtering kept a different set than IdFilter::matches accepts",
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

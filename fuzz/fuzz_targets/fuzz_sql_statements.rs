// Author: spicyzhug@gmail.com
// Copyright: lituus-io, all rights reserved

//! Fuzz target: SQL statement splitting on author text.
//!
//! Routine bodies, job queries and scheduled queries are split into
//! statements before anything parses them, and the splitter hands out byte
//! ranges into the author's text. Every range is sliced -- under
//! `panic = "abort"` a range that is out of bounds or inside a character
//! takes the host down -- so the properties are about the ranges themselves,
//! not about parsing.
//!
//! Properties:
//! - every range is in bounds, on `char` boundaries, non-empty, increasing
//!   and non-overlapping
//! - every range is trimmed: no statement starts or ends with whitespace
//! - `split_statements` is exactly the ranges, sliced
//! - the split is deterministic
//! - the statement-kind classifier never panics and is deterministic

#![no_main]
use libfuzzer_sys::fuzz_target;

use pulumi_rs_yaml_core::sql_lineage::{
    classify_statement_text, split_statement_spans, split_statements,
};

fuzz_target!(|data: &[u8]| {
    let Ok(script) = std::str::from_utf8(data) else {
        return;
    };
    // The lineage reader refuses SQL over 1 MiB; spend the budget on shapes.
    if script.len() > 64 * 1024 {
        return;
    }

    let spans = split_statement_spans(script);
    let mut prev_end = 0usize;
    for r in &spans {
        assert!(r.start < r.end, "empty or inverted range {r:?}");
        assert!(r.end <= script.len(), "range {r:?} past {}", script.len());
        assert!(r.start >= prev_end, "range {r:?} overlaps the previous");
        assert!(
            script.is_char_boundary(r.start) && script.is_char_boundary(r.end),
            "range {r:?} is inside a character"
        );
        let piece = &script[r.clone()];
        assert_eq!(piece, piece.trim(), "statement {r:?} is not trimmed");
        prev_end = r.end;
    }

    let sliced: Vec<&str> = spans.iter().map(|r| &script[r.clone()]).collect();
    assert_eq!(split_statements(script), sliced);
    assert_eq!(
        split_statement_spans(script),
        spans,
        "split must be deterministic"
    );

    // The keyword classifier reads author text too; it may answer anything,
    // but it must answer, and answer the same twice.
    for piece in sliced.iter().copied().chain(std::iter::once(script)) {
        assert_eq!(
            classify_statement_text(piece),
            classify_statement_text(piece)
        );
    }
});

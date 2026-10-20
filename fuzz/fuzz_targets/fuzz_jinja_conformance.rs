// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

//! Fuzz target: mutations of the conformance corpus.
//!
//! Seeded from every case template in `tests/jinja_conformance/cases/`
//! (copied into this target's corpus), so libFuzzer starts from programs
//! that exercise each statement, filter, test and global, and mutates them
//! into the neighbourhood real templates live in.
//!
//! Asserts: no panic; limits hold; a render is deterministic; and the
//! syntax gate agrees with the renderer -- a template the renderer compiles
//! is never refused by `validate_jinja_syntax`.
//!
//! A mode bit: an input whose first byte is `\x01` is rendered, without
//! that byte, behind a preamble that turns on mutable-container mode (a
//! template that calls a mutating method has its list and dict literals
//! built as containers it can change, through a different lowering). The
//! same invariants hold there, and where both modes render, they render the
//! same text: a construct that works in only one mode is a lowering bug.
//! Seeds with the bit set sit beside the plain ones (`*.mutable.txt`).
//!
//! The differential half (does the engine still agree with reference
//! Jinja2?) is the conformance CI job over the committed corpus; Python is
//! not run in-process.

#![no_main]
use libfuzzer_sys::fuzz_target;
use pulumi_rs_yaml_core::jinja::{
    validate_jinja_syntax, JinjaContext, JinjaLimits, JinjaPreprocessor, RenderErrorKind,
    UndefinedMode,
};
use std::collections::HashMap;

const LIMITS: JinjaLimits = JinjaLimits {
    fuel: 20_000,
    recursion: 100,
    output_bytes: 64 * 1024,
    range: 1_000,
};

/// Renders nothing; turns on mutable-container mode. One line, so no line
/// of the input moves.
const MUTABLE_PREAMBLE: &str = "{% set _rsyaml_mode = [] %}{% do _rsyaml_mode.append(0) %}";

fuzz_target!(|data: &[u8]| {
    let (mutable, data) = match data.split_first() {
        Some((1, rest)) => (true, rest),
        _ => (false, data),
    };
    let Ok(body) = std::str::from_utf8(data) else {
        return;
    };
    if body.len() > 16 * 1024 {
        return;
    }
    let doubled;
    let source = if mutable {
        doubled = format!("{MUTABLE_PREAMBLE}{body}");
        doubled.as_str()
    } else {
        body
    };
    let extra: HashMap<String, String> = [("name", "from-ctx"), ("v", "a: b\nc"), ("x", "1")]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let config = HashMap::new();
    let ctx = JinjaContext {
        project_name: "p",
        stack_name: "s",
        cwd: "/nonexistent-fuzz-root",
        organization: "",
        root_directory: "/nonexistent-fuzz-root",
        config: &config,
        project_dir: "/nonexistent-fuzz-root",
        undefined: UndefinedMode::Strict,
        provider_templated_packages: &[],
        extra: &extra,
        limits: LIMITS,
    };
    let pre = JinjaPreprocessor::new(&ctx);
    let first = pre.render_with_stats(source, "case.j2");
    let again = pre.render_with_stats(source, "case.j2");
    match (&first, &again) {
        (Ok((a, sa)), Ok((b, sb))) => {
            assert_eq!((a, sa), (b, sb), "non-deterministic: {source:?}");
            assert!(a.len() <= LIMITS.output_bytes);
            assert!(sa.fuel <= LIMITS.fuel);
        }
        (Err(a), Err(b)) => {
            assert_eq!((a.kind, a.line, &a.message), (b.kind, b.line, &b.message));
        }
        _ => panic!("one render failed and the other did not: {source:?}"),
    }
    // A template that rendered compiled; the syntax gate must agree.
    let compiled = match &first {
        Ok(_) => true,
        Err(d) => d.kind != RenderErrorKind::JinjaSyntax,
    };
    if compiled && first.is_ok() {
        assert!(
            validate_jinja_syntax(source, "case.j2").is_ok(),
            "rendered but refused by the syntax gate: {source:?}"
        );
    }
    // The two modes agree wherever both render. A template with no Jinja
    // of its own is returned as written, which the preamble changes.
    if mutable && pulumi_rs_yaml_core::jinja::has_jinja_syntax(body) {
        if let (Ok((a, _)), Ok((b, _))) = (&first, pre.render_with_stats(body, "case.j2")) {
            assert_eq!(a, &b, "the modes disagree: {body:?}");
        }
    }
});

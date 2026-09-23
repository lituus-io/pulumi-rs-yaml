// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

//! Fuzz target: the `truncate` / `center` / `wordwrap` string filters.
//!
//! These run over template text -- resource names, descriptions, anything a
//! config carries -- with a width or length the template author chose. So the
//! properties are total ones: no input and no argument may panic, hang, slice a
//! character in half, or make `wordwrap` invent text that was not there.
//!
//! Targets:
//! - character-boundary slicing on multi-byte input
//! - termination of the wrap fill loop, whose progress argument is subtle
//! - the bound on `center`'s padding, which is the one allocation an argument
//!   can drive
//! - text preservation: wrapping may only move characters between lines

#![no_main]
use libfuzzer_sys::fuzz_target;
use pulumi_rs_yaml_core::jinja::{
    JinjaContext, JinjaPreprocessor, TemplatePreprocessor, UndefinedMode,
};
use std::collections::HashMap;

#[derive(arbitrary::Arbitrary, Debug)]
struct Input<'a> {
    subject: &'a str,
    width: u16,
    killwords: bool,
    break_long_words: bool,
    break_on_hyphens: bool,
    leeway: u8,
    which: u8,
}

fn render(body: &str) -> Result<String, String> {
    let config = HashMap::new();
    let extra = HashMap::new();
    let ctx = JinjaContext {
        project_name: "f",
        stack_name: "dev",
        cwd: "/tmp",
        organization: "org",
        root_directory: "/tmp",
        config: &config,
        project_dir: "/tmp",
        undefined: UndefinedMode::Strict,
        provider_templated_packages: &[],
        extra: &extra,
    };
    JinjaPreprocessor::new(&ctx)
        .preprocess(body, "Pulumi.yaml")
        .map(std::borrow::Cow::into_owned)
        .map_err(|e| e.to_string())
}

/// The subject as a Jinja single-quoted literal.
fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        match ch {
            '\'' => out.push_str("\\'"),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            // A Jinja delimiter inside the literal would change the template
            // rather than the value, which is not what this target is about.
            '{' | '}' | '%' => out.push('_'),
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

fuzz_target!(|input: Input<'_>| {
    if input.subject.len() > 4096 {
        return;
    }
    let subject = quote(input.subject);
    let w = input.width;
    let expr = match input.which % 3 {
        0 => format!(
            "{subject} | truncate({w}, {}, '', {})",
            input.killwords, input.leeway
        ),
        1 => format!("{subject} | center({w})"),
        _ => format!(
            "{subject} | wordwrap({w}, {}, None, {})",
            input.break_long_words, input.break_on_hyphens
        ),
    };

    // Must return -- never panic, never hang.
    let Ok(out) = render(&format!("<{{{{ {expr} }}}}>")) else {
        return; // a refusal is a valid answer; an abort is not
    };
    // Valid UTF-8 by construction, so the only way to fail this is a slice
    // taken off a character boundary, which would have panicked already.
    assert!(std::str::from_utf8(out.as_bytes()).is_ok());

    // Strip exactly ONE marker at each end. `trim_start_matches` removes every
    // leading occurrence, so a subject that itself begins with the marker lost a
    // character and this target reported the filter for the harness's mistake --
    // which is how this line came to be written this way.
    let inner = out
        .strip_prefix('<')
        .and_then(|s| s.strip_suffix('>'))
        .unwrap_or(&out);

    if input.which % 3 == 2 && input.width > 0 {
        // Wrapping only moves characters between lines: it may drop the
        // whitespace it wrapped at, and insert separators, but it may not
        // introduce anything else. Compared over non-whitespace characters so
        // the separator bookkeeping does not have to be replicated here.
        let before: String = input.subject.chars().filter(|c| !c.is_whitespace()).collect();
        let after: String = inner.chars().filter(|c| !c.is_whitespace()).collect();
        // The quoting above substitutes Jinja delimiters, so compare likewise.
        let before: String = before
            .chars()
            .map(|c| if matches!(c, '{' | '}' | '%') { '_' } else { c })
            .collect();
        assert_eq!(
            before, after,
            "wordwrap changed the text: {:?} -> {:?}",
            input.subject, inner
        );
    }
});

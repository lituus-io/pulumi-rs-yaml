// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

// Structured input for the provider-scope fuzz targets.
//
// Random bytes will never produce a resource block, so the interesting targets
// need a generator that builds structurally valid but adversarial stacks. The
// point is not to be realistic — it is to put, at every position a line-based
// scanner makes a decision, something that looks like a different decision.
//
// Shared by several targets via `#[path]`, so it is a module rather than a bin.

#![allow(dead_code)]

use arbitrary::{Result, Unstructured};

/// The package the targets scope. Others must be left alone.
pub const SCOPED: &[&str] = &["gcpx"];

const PACKAGES: &[&str] = &["gcpx", "aws", "pulumi", "gcpxtra", "gcp"];

/// Content lines for block scalars.
///
/// Every entry mimics something the scanner looks for: a `type:` line, a key
/// with a block-scalar header, a document marker, a sequence entry, a comment.
/// None of it may end an extent early or start a phantom resource.
const CONTENT: &[&str] = &[
    "SELECT 1",
    "-- type: gcpx:dbt/model:Model",
    "SELECT '  sql: |' AS looks_like_a_key",
    "       'properties:' AS also_not_a_key,",
    "---",
    "...",
    "{{ ref('stg_outages') }}",
    "{% if is_incremental() %}",
    "WHERE updated_at > (SELECT MAX(updated_at) FROM {{ this }})",
    "{% endif %}",
    "{{ config(materialized='incremental', unique_key='outage_id') }}",
    "# this is SQL, not a comment",
    "- looks like a sequence entry",
    "  deeper: mapping",
    "\t-- a tab in content is legal, unlike a tab in indentation",
    "WHERE x > 1 AND y < 2",
    "{# a jinja comment #}",
    "{{ this }}",
    "type: gcpx:dbt/model:Model",
    "sql: |",
];

/// Lines that are blank or whitespace-only. Inside a block scalar these are
/// content; a scanner that treats them as separators truncates the extent.
const BLANKS: &[&str] = &["", "   ", "\t"];

/// Values for plain (non-block) properties. These must still be rendered.
const PLAIN: &[&str] = &[
    "probe",
    "\"{{ config.gcpProject }}\"",
    "'single quoted'",
    "us-central1",
    "123",
    "true",
];

pub struct Stack {
    pub text: String,
    /// Block scalars planted inside a resource whose package is scoped. This is
    /// the completeness half of the oracle: protecting fewer is a miss,
    /// protecting more means the scope leaked.
    pub planted: usize,
    /// True when the generator deliberately emitted an indentation indicator
    /// that disagrees with the content, so a refusal is the correct answer.
    pub bad_indicator: bool,
    /// True when template syntax landed in a block scalar of an *unscoped*
    /// resource. That text is still addressed to this runtime, so rendering is
    /// allowed to fail; without it, a successful render is required.
    pub unscoped_jinja: bool,
}

struct Builder {
    out: String,
    nl: &'static str,
    planted: usize,
    bad_indicator: bool,
    unscoped_jinja: bool,
}

impl Builder {
    fn line(&mut self, indent: usize, body: &str) {
        for _ in 0..indent {
            self.out.push(' ');
        }
        self.out.push_str(body);
        self.out.push_str(self.nl);
    }

    fn blank(&mut self, raw: &str) {
        self.out.push_str(raw);
        self.out.push_str(self.nl);
    }
}

/// Builds one stack file.
pub fn build(u: &mut Unstructured) -> Result<Stack> {
    let nl = if u.arbitrary::<bool>()? { "\n" } else { "\r\n" };
    let mut b = Builder {
        out: String::new(),
        nl,
        planted: 0,
        bad_indicator: false,
        unscoped_jinja: false,
    };

    if u.arbitrary::<bool>()? {
        b.out.push('\u{feff}');
    }

    b.line(0, "name: scope-probe");
    b.line(0, "runtime:");
    b.line(2, "name: yaml");
    if u.arbitrary::<bool>()? {
        b.line(2, "options:");
        b.line(4, "providerTemplatedPackages: [gcpx]");
    }
    b.line(0, "resources:");

    let count = u.int_in_range(1..=4usize)?;
    for n in 0..count {
        resource(u, &mut b, n)?;
        if u.arbitrary::<bool>()? {
            b.blank("");
        }
        if u.arbitrary::<bool>()? {
            b.line(u.int_in_range(0..=6usize)?, "# a comment between resources");
        }
    }

    Ok(Stack {
        text: b.out,
        planted: b.planted,
        bad_indicator: b.bad_indicator,
        unscoped_jinja: b.unscoped_jinja,
    })
}

fn resource(u: &mut Unstructured, b: &mut Builder, n: usize) -> Result<()> {
    let step = u.int_in_range(1..=8usize)?;
    let name_indent = 2usize;
    let key = name_indent + step;
    let pkg: &str = u.choose(PACKAGES)?;
    let scoped = SCOPED.contains(&pkg);

    b.line(name_indent, &format!("res{n}:"));

    // Key order is arbitrary in YAML, so `properties:` may precede `type:`.
    // A single forward pass would miss the scope entirely in that case.
    let type_first = u.arbitrary::<bool>()?;
    if type_first {
        b.line(key, &format!("type: {pkg}:dbt/model:Model"));
    }

    b.line(key, "properties:");
    let prop = key + step;

    let props = u.int_in_range(1..=3usize)?;
    for p in 0..props {
        match u.int_in_range(0..=3u8)? {
            0 => {
                let v: &str = u.choose(PLAIN)?;
                b.line(prop, &format!("plain{p}: {v}"));
            }
            1 => {
                block_scalar(u, b, prop, step, scoped, &format!("sql{p}"))?;
            }
            2 => {
                // One level deeper, to check the scope reaches nested maps.
                b.line(prop, &format!("nested{p}:"));
                block_scalar(u, b, prop + step, step, scoped, "body")?;
            }
            _ => {
                // A sequence whose entries carry block scalars.
                b.line(prop, &format!("seq{p}:"));
                b.line(prop + step, "- name: first");
                block_scalar(u, b, prop + step + 2, step, scoped, "script")?;
            }
        }
    }

    if !type_first {
        b.line(key, &format!("type: {pkg}:dbt/model:Model"));
    }
    Ok(())
}

fn block_scalar(
    u: &mut Unstructured,
    b: &mut Builder,
    key_indent: usize,
    step: usize,
    scoped: bool,
    name: &str,
) -> Result<()> {
    let style = if u.arbitrary::<bool>()? { '|' } else { '>' };
    let chomp: &str = u.choose(&["", "-", "+"])?;

    // The indentation indicator is measured from the header line's own
    // indentation, and is the case most likely to be mis-measured — especially
    // when the content's first line is itself indented further.
    let (indicator, content_indent, bad) = match u.int_in_range(0..=3u8)? {
        0 => (String::new(), key_indent + step, false),
        1 if step <= 9 => (step.to_string(), key_indent + step, false),
        2 if step <= 9 => {
            // Content deeper than the indicator says: legal YAML, the extra
            // spaces are content.
            (step.to_string(), key_indent + step + 2, false)
        }
        _ if step < 9 => {
            // Deliberately wrong: the indicator claims deeper than the content.
            // Refusing is the correct answer.
            ((step + 1).to_string(), key_indent + step, true)
        }
        _ => (String::new(), key_indent + step, false),
    };
    if bad {
        b.bad_indicator = true;
    }

    let comment = if u.arbitrary::<bool>()? {
        "  # header"
    } else {
        ""
    };
    b.line(
        key_indent,
        &format!("{name}: {style}{chomp}{indicator}{comment}"),
    );

    let lines = u.int_in_range(1..=6usize)?;
    for i in 0..lines {
        // The first content line must be non-blank: a leading blank line more
        // indented than the eventual content is a YAML error, and the point of
        // this generator is to produce documents a parser will accept.
        if i > 0 && u.int_in_range(0..=3u8)? == 0 {
            let raw: &str = u.choose(BLANKS)?;
            b.blank(raw);
        } else {
            let body: &str = u.choose(CONTENT)?;
            if !scoped && (body.contains("{{") || body.contains("{%") || body.contains("{#")) {
                b.unscoped_jinja = true;
            }
            // Without an explicit indicator YAML takes the scalar's indentation
            // from its first non-empty line, so a later line may be deeper but
            // never shallower. Emitting one anyway produces a document no
            // parser accepts, which tests nothing.
            let extra = if i > 0 && u.arbitrary::<bool>()? {
                2
            } else {
                0
            };
            b.line(content_indent + extra, body);
        }
    }

    if scoped {
        b.planted += 1;
    }
    Ok(())
}

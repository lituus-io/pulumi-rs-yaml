// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

//! Scoping provider-rendered templates out of the runtime's Jinja pass.
//!
//! Two renderers can see the same stack file. This runtime renders Jinja over
//! the whole file before anything is parsed; a provider may render its own
//! templates afterwards, per resource. Both use `{{ }}` and `{% %}`, so text
//! written for the second renderer is evaluated by the first — which knows
//! neither `ref('x')` nor `is_incremental()`.
//!
//! The fix is not to teach this runtime another vocabulary. It is to let the
//! runtime recognise which text is not addressed to it, by *where it sits*
//! rather than by what it says: a block scalar inside a resource whose package
//! is listed in `runtime.options.providerTemplatedPackages`.
//!
//! ```yaml
//! dailyRevenue:
//!   type: gcpx:dbt/model:Model
//!   properties:
//!     project: {{ config.gcpProject }}   # still rendered by the runtime
//!     sql: |
//!       {{ config(materialized='incremental') }}
//!       SELECT * FROM {{ ref('stg_outages') }}
//!       {% if is_incremental() %}
//!       WHERE updated_at > (SELECT MAX(updated_at) FROM {{ this }})
//!       {% endif %}
//! ```
//!
//! Rendering has to precede parsing — Jinja may generate YAML structure — so
//! this cannot consult a parsed document or a provider schema. It works on
//! text. That is viable because the two facts it needs are textual: a `type:`
//! line names the package, and a block scalar's extent is defined exactly by
//! indentation.
//!
//! The pass is split so the risky half stands alone:
//!
//! - [`protect`] finds the extents and swaps each for a NUL-delimited marker.
//!   Pure: no rendering, no context, no I/O. Every bug this design can have is
//!   a bug in this function, which is why it is fuzzed directly.
//! - [`restore`] puts the original bytes back, verbatim.
//!
//! `restore(protect(s)) == s` for every input, and that is the invariant the
//! fuzz targets assert. Where an extent cannot be determined with certainty,
//! the pass refuses rather than guesses: silently altered SQL is the failure
//! to avoid, not a rejected file.

use std::borrow::Cow;
use std::fmt;

/// Marker prefix. NUL cannot appear in a YAML document, which is what makes it
/// usable as a sentinel — the same reasoning behind the `readFile` markers.
pub const MARKER_PREFIX: &str = "\u{0}PT:";

/// Refusals. Each names the line so the message can point at the source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeError {
    /// A tab in the indentation of a structural line inside a scoped resource.
    /// Tabs are illegal as YAML indentation, and their width is not knowable,
    /// so no extent below them can be trusted.
    TabIndent { line: usize },
    /// A block scalar whose explicit indentation indicator does not match its
    /// content (`sql: |4` followed by two-space content), or an indicator of
    /// zero, which YAML forbids.
    BadIndentIndicator { line: usize },
    /// A marker survived rendering but is no longer alone on its line —
    /// whitespace control (`{%-`) pulled it onto another line.
    MarkerNotAlone { line: usize },
    /// A marker whose id names no protected region.
    UnknownMarker { line: usize },
}

impl fmt::Display for ScopeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TabIndent { line } => write!(
                f,
                "line {line}: tab in indentation inside a provider-templated resource; \
                 YAML forbids tabs as indentation, so the extent of the block scalar \
                 below it cannot be determined — use spaces"
            ),
            Self::BadIndentIndicator { line } => write!(
                f,
                "line {line}: the block scalar's indentation indicator does not match \
                 its content; remove the indicator and let YAML infer it"
            ),
            Self::MarkerNotAlone { line } => write!(
                f,
                "line {line}: a provider-templated block was moved by Jinja whitespace \
                 control; remove the `-` from the surrounding tag"
            ),
            Self::UnknownMarker { line } => write!(
                f,
                "line {line}: internal error — a provider-scope marker names no region"
            ),
        }
    }
}

impl std::error::Error for ScopeError {}

/// Result of [`protect`].
///
/// `Unchanged` is the common case and costs no allocation: it means the source
/// held nothing addressed to a provider, so rendering proceeds exactly as it
/// always has.
#[derive(Debug)]
pub enum Protected<'src> {
    Unchanged,
    Marked {
        source: String,
        /// Indexed by marker id.
        regions: Vec<Region<'src>>,
    },
}

/// One set-aside block scalar.
///
/// `text` is all that restoring needs. The rest describes where the extent was
/// found, which is what lets a test check the extent against a real YAML parser
/// rather than only checking that protect and restore are inverses — an
/// extent can be wrong in both directions at once and still round trip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region<'src> {
    /// Verbatim bytes of the scalar's content, without the final line
    /// terminator. Zero copy: a slice of the original source.
    pub text: &'src str,
    /// Zero-based index of the `key: |` line.
    pub header_line: usize,
    /// Indentation of the header line — what an explicit indicator counts from.
    pub parent_indent: usize,
    /// Column at which the content sits.
    pub content_indent: usize,
    /// Zero-based index of the first content line.
    pub first_line: usize,
    /// Zero-based index of the last content line, inclusive.
    pub last_line: usize,
}

impl<'src> Protected<'src> {
    /// The text to render.
    pub fn source(&self, original: &'src str) -> &str {
        match self {
            Self::Unchanged => original,
            Self::Marked { source, .. } => source,
        }
    }

    pub fn regions(&self) -> &[Region<'src>] {
        match self {
            Self::Unchanged => &[],
            Self::Marked { regions, .. } => regions,
        }
    }
}

// ---------------------------------------------------------------------------
// Line model
// ---------------------------------------------------------------------------

struct Line<'s> {
    /// Includes the terminator.
    raw: &'s str,
    /// Excludes the terminator.
    text: &'s str,
    start: usize,
    /// Count of leading spaces. Only spaces — a tab is not indentation.
    indent: usize,
    /// A tab sits in this line's indentation. A line that is *only* whitespace
    /// has no indentation to speak of — it is an empty line, tab or not — and
    /// refusing one would reject documents that deploy perfectly well today.
    tab_indent: bool,
    blank: bool,
    comment: bool,
}

impl<'s> Line<'s> {
    /// The terminator, `""` at end of input.
    fn term(&self) -> &'s str {
        &self.raw[self.text.len()..]
    }

    fn end_of_text(&self) -> usize {
        self.start + self.text.len()
    }
}

fn split_lines(source: &str) -> Vec<Line<'_>> {
    let mut lines = Vec::new();
    let mut start = 0usize;
    for raw in source.split_inclusive('\n') {
        let text = raw
            .strip_suffix("\r\n")
            .or_else(|| raw.strip_suffix('\n'))
            .unwrap_or(raw);
        let after_spaces = text.trim_start_matches(' ');
        let indent = text.len() - after_spaces.len();
        let blank = text.trim().is_empty();
        lines.push(Line {
            raw,
            text,
            start,
            indent,
            tab_indent: !blank && after_spaces.starts_with('\t'),
            blank,
            comment: text.trim_start().starts_with('#'),
        });
        start += raw.len();
    }
    lines
}

/// The column at which a node's own content begins, after any sequence-entry
/// dashes, and how many dashes were consumed.
///
/// `- - type: x` is two nested sequence entries whose mapping starts at
/// column 4; the boundary rules need both numbers, because a dash line ends
/// the previous entry even though its content sits deeper.
fn node_start(text: &str) -> (usize, usize) {
    let mut i = text.len() - text.trim_start_matches(' ').len();
    let mut dashes = 0usize;
    loop {
        let rest = &text[i..];
        if let Some(after) = rest.strip_prefix("- ") {
            i += 2;
            dashes += 1;
            i += after.len() - after.trim_start_matches(' ').len();
        } else if rest == "-" {
            i += 1;
            dashes += 1;
            break;
        } else {
            break;
        }
    }
    (i, dashes)
}

/// Splits `key: value` at the first `:` that acts as a separator, skipping
/// quoted regions and stopping at a comment. Returns the key and the byte
/// offset just past the `:`.
fn split_key(text: &str, from: usize) -> Option<(&str, usize)> {
    let b = text.as_bytes();
    let mut i = from;
    let mut quote: Option<u8> = None;
    while i < b.len() {
        match quote {
            Some(q) => {
                if b[i] == q {
                    if q == b'\'' && b.get(i + 1) == Some(&b'\'') {
                        i += 2;
                        continue;
                    }
                    quote = None;
                    i += 1;
                } else if q == b'"' && b[i] == b'\\' {
                    i += 2;
                } else {
                    i += 1;
                }
            }
            None => match b[i] {
                b'"' | b'\'' => {
                    quote = Some(b[i]);
                    i += 1;
                }
                b'#' if i == from || b[i - 1] == b' ' => return None,
                b':' if i + 1 == b.len() || b[i + 1] == b' ' || b[i + 1] == b'\t' => {
                    return Some((text[from..i].trim(), i + 1));
                }
                _ => i += 1,
            },
        }
    }
    None
}

fn unquote(s: &str) -> &str {
    let s = s.trim();
    let b = s.as_bytes();
    if s.len() >= 2
        && ((b[0] == b'"' && b[s.len() - 1] == b'"') || (b[0] == b'\'' && b[s.len() - 1] == b'\''))
    {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

/// Parsed block-scalar header: `|`, `>`, with optional chomping and an optional
/// indentation indicator.
enum Header {
    /// Indentation indicator, if given explicitly.
    Ok(Option<usize>),
    /// `|0` — YAML forbids a zero indicator.
    ZeroIndicator,
}

fn block_header(value: &str) -> Option<Header> {
    let v = value.trim_start_matches([' ', '\t']);
    let mut chars = v.chars();
    match chars.next()? {
        '|' | '>' => {}
        _ => return None,
    }
    let rest = chars.as_str();
    let rb = rest.as_bytes();
    let mut idx = 0usize;
    let mut explicit: Option<usize> = None;
    let mut chomp = false;
    while idx < rb.len() {
        match rb[idx] {
            b'+' | b'-' if !chomp => {
                chomp = true;
                idx += 1;
            }
            c @ b'0'..=b'9' if explicit.is_none() => {
                if c == b'0' {
                    return Some(Header::ZeroIndicator);
                }
                explicit = Some((c - b'0') as usize);
                idx += 1;
            }
            _ => break,
        }
    }
    let tail = &rest[idx..];
    if tail.is_empty() {
        return Some(Header::Ok(explicit));
    }
    // A comment must be preceded by whitespace.
    if !tail.starts_with([' ', '\t']) {
        return None;
    }
    let tail = tail.trim();
    if tail.is_empty() || tail.starts_with('#') {
        Some(Header::Ok(explicit))
    } else {
        None
    }
}

/// If the value opens a quoted scalar that does not close on this line,
/// returns the quote byte.
fn unterminated_quote(value: &str) -> Option<u8> {
    let b = value.as_bytes();
    let mut i = 0usize;
    while i < b.len() && (b[i] == b' ' || b[i] == b'\t') {
        i += 1;
    }
    let q = match b.get(i) {
        Some(&c) if c == b'"' || c == b'\'' => c,
        _ => return None,
    };
    i += 1;
    Some(q).filter(|_| !closes(&value[i..], q).0)
}

/// Whether a quoted scalar closes within `s`, and where.
fn closes(s: &str, q: u8) -> (bool, usize) {
    let b = s.as_bytes();
    let mut i = 0usize;
    while i < b.len() {
        if b[i] == q {
            if q == b'\'' && b.get(i + 1) == Some(&b'\'') {
                i += 2;
                continue;
            }
            return (true, i + 1);
        }
        if q == b'"' && b[i] == b'\\' {
            i += 2;
            continue;
        }
        i += 1;
    }
    (false, i)
}

// ---------------------------------------------------------------------------
// Pass A — where does literal content live?
// ---------------------------------------------------------------------------

struct Scalar {
    header: usize,
    content_indent: usize,
    /// First content line, inclusive.
    start: usize,
    /// One past the last content line.
    end: usize,
    /// The explicit indicator disagreed with the content. Refused only if this
    /// scalar turns out to sit inside a scoped resource.
    suspect: bool,
}

/// Classifies every line as structural or literal content, and records the
/// extent of every block scalar in the document.
///
/// This runs over the whole file, not only over scoped resources, because the
/// resource scan must not mistake SQL for structure. A model whose SQL contains
/// the text `type: gcpx:dbt/model:Model` must not start a phantom resource.
fn scan(lines: &[Line<'_>]) -> (Vec<Scalar>, Vec<bool>) {
    let mut scalars = Vec::new();
    let mut is_content = vec![false; lines.len()];
    let mut i = 0usize;

    while i < lines.len() {
        let line = &lines[i];
        if line.blank || line.comment {
            i += 1;
            continue;
        }
        let t = line.text.trim();
        if t == "---" || t == "..." {
            i += 1;
            continue;
        }

        let (col, _) = node_start(line.text);
        let (header, value_start) = match split_key(line.text, col) {
            Some((_, vstart)) => (block_header(&line.text[vstart..]), Some(vstart)),
            None => (block_header(&line.text[col..]), None),
        };

        let explicit = match header {
            Some(Header::Ok(e)) => e,
            Some(Header::ZeroIndicator) => {
                // Push a suspect scalar with no content so the refusal can be
                // raised if it turns out to be in scope.
                scalars.push(Scalar {
                    header: i,
                    content_indent: line.indent + 1,
                    start: i + 1,
                    end: i + 1,
                    suspect: true,
                });
                i += 1;
                continue;
            }
            None => {
                // A multi-line quoted scalar is literal too, and its content
                // could otherwise be read as structure.
                if let Some(vstart) = value_start {
                    if let Some(q) = unterminated_quote(&line.text[vstart..]) {
                        let mut j = i + 1;
                        while j < lines.len() {
                            is_content[j] = true;
                            let (done, _) = closes(lines[j].text, q);
                            j += 1;
                            if done {
                                break;
                            }
                        }
                        i = j;
                        continue;
                    }
                }
                i += 1;
                continue;
            }
        };

        // Extent by indentation. The parent node's indentation is the leading
        // whitespace of the header line, which is what an explicit indicator is
        // measured against — for `- |2` that is the sequence, not the dash's
        // content column.
        let parent_indent = line.indent;
        let mut content_indent = explicit.map(|d| parent_indent + d);
        let mut suspect = false;
        let mut last = None::<usize>;
        let mut j = i + 1;
        while j < lines.len() {
            let l = &lines[j];
            if l.blank {
                // Blank lines inside a block scalar are content, not a
                // separator. Whether a *trailing* run of them belongs to the
                // scalar does not matter here, so they are left outside and the
                // roundtrip stays exact.
                j += 1;
                continue;
            }
            match content_indent {
                None => {
                    if l.indent <= parent_indent {
                        break;
                    }
                    content_indent = Some(l.indent);
                }
                Some(ci) => {
                    if l.indent < ci {
                        if last.is_none() && l.indent > parent_indent {
                            // `sql: |4` with two-space content.
                            suspect = true;
                        }
                        break;
                    }
                }
            }
            last = Some(j);
            j += 1;
        }

        let end = last.map_or(i + 1, |l| l + 1);
        for slot in is_content.iter_mut().take(end).skip(i + 1) {
            *slot = true;
        }
        scalars.push(Scalar {
            header: i,
            content_indent: content_indent.unwrap_or(parent_indent + 1),
            start: i + 1,
            end,
            suspect,
        });
        i = end.max(i + 1);
    }

    (scalars, is_content)
}

// ---------------------------------------------------------------------------
// Pass B — which line ranges are scoped resources?
// ---------------------------------------------------------------------------

/// The package named by a `type:` line, if the line is one.
fn type_package(text: &str) -> Option<&str> {
    let (col, _) = node_start(text);
    let (key, vstart) = split_key(text, col)?;
    if unquote(key) != "type" {
        return None;
    }
    let raw = text[vstart..].trim();
    let value = if raw.starts_with('"') || raw.starts_with('\'') {
        unquote(raw)
    } else {
        // Strip a trailing comment from an unquoted value.
        match raw.find(" #") {
            Some(k) => raw[..k].trim(),
            None => raw,
        }
    };
    // A Pulumi type token is `pkg:module:Type`; anything without a colon is not
    // one, and treating it as a package would scope far too much.
    let (pkg, rest) = value.split_once(':')?;
    if pkg.is_empty() || rest.is_empty() {
        return None;
    }
    Some(pkg)
}

/// The line range of the mapping that owns the `type:` key on `idx`, and the
/// line at which extension stopped because its indentation could not be
/// measured.
///
/// A resource's keys all sit at the same column, so the block is the maximal
/// run of lines around `idx` that are no shallower than that column. Extending
/// in both directions is what makes key order irrelevant: `properties:` may
/// precede `type:`.
///
/// A tab-indented line is a boundary the pass cannot justify. It looks
/// shallower than everything, so the range simply stops there and the block
/// scalar below it is quietly left unprotected. Reporting it lets the caller
/// refuse with a message about the tab instead of letting a confusing template
/// error surface three layers down.
fn block_range(
    lines: &[Line<'_>],
    is_content: &[bool],
    idx: usize,
    key_col: usize,
) -> (usize, usize, Option<usize>) {
    let mut tab_boundary = None;
    let mut lo = idx;
    let mut j = idx;
    while j > 0 {
        j -= 1;
        let l = &lines[j];
        if l.blank || l.comment || is_content[j] {
            lo = j;
            continue;
        }
        let t = l.text.trim();
        if t == "---" || t == "..." {
            break;
        }
        if l.tab_indent {
            tab_boundary = Some(j);
            break;
        }
        let (c, dashes) = node_start(l.text);
        if dashes > 0 && l.indent < key_col {
            // The dash that opens this very sequence entry.
            if c == key_col {
                lo = j;
            }
            break;
        }
        if l.indent >= key_col {
            lo = j;
            continue;
        }
        break;
    }

    let mut hi = idx + 1;
    let mut j = idx + 1;
    while j < lines.len() {
        let l = &lines[j];
        if l.blank || l.comment || is_content[j] {
            hi = j + 1;
            j += 1;
            continue;
        }
        let t = l.text.trim();
        if t == "---" || t == "..." {
            break;
        }
        if l.tab_indent {
            tab_boundary = Some(j);
            break;
        }
        let (_, dashes) = node_start(l.text);
        if dashes > 0 && l.indent < key_col {
            break;
        }
        if l.indent >= key_col {
            hi = j + 1;
            j += 1;
            continue;
        }
        break;
    }

    (lo, hi, tab_boundary)
}

// ---------------------------------------------------------------------------
// protect / restore
// ---------------------------------------------------------------------------

/// Sets aside every block scalar that belongs to a resource whose package is
/// listed, replacing each with a marker.
///
/// Returns [`Protected::Unchanged`] — and allocates nothing — when the source
/// names none of `packages`, which is every stack that does not opt in.
pub fn protect<'src>(source: &'src str, packages: &[&str]) -> Result<Protected<'src>, ScopeError> {
    if packages.is_empty() || source.is_empty() {
        return Ok(Protected::Unchanged);
    }
    // A NUL in the source would make the markers ambiguous. YAML forbids one,
    // so this only happens on input that cannot deploy anyway; declining to
    // protect leaves behaviour exactly as it was.
    if source.contains('\u{0}') {
        return Ok(Protected::Unchanged);
    }
    // Cheap reject: no listed package is named anywhere.
    if !packages.iter().any(|p| source.contains(*p)) {
        return Ok(Protected::Unchanged);
    }

    let lines = split_lines(source);
    let (scalars, is_content) = scan(&lines);

    // Which line ranges are in scope.
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for (idx, line) in lines.iter().enumerate() {
        if is_content[idx] || line.blank || line.comment {
            continue;
        }
        let Some(pkg) = type_package(line.text) else {
            continue;
        };
        if !packages.contains(&pkg) {
            continue;
        }
        let (col, _) = node_start(line.text);
        let (lo, hi, tab) = block_range(&lines, &is_content, idx, col);
        if let Some(line) = tab {
            return Err(ScopeError::TabIndent { line: line + 1 });
        }
        ranges.push((lo, hi));
    }
    if ranges.is_empty() {
        return Ok(Protected::Unchanged);
    }

    // Flatten the ranges once. Testing each line against every range would be
    // quadratic in a stack with many resources, and this runs on every file of
    // every deploy.
    let mut in_scope = vec![false; lines.len()];
    for (lo, hi) in ranges {
        for slot in in_scope.iter_mut().take(hi).skip(lo) {
            *slot = true;
        }
    }

    // Tabs make indentation unmeasurable. Refuse before deciding any extent
    // inside a scoped resource rather than guessing at a width.
    for (idx, line) in lines.iter().enumerate() {
        if line.tab_indent && !is_content[idx] && in_scope[idx] {
            return Err(ScopeError::TabIndent { line: idx + 1 });
        }
    }

    let mut chosen: Vec<&Scalar> = Vec::new();
    for s in &scalars {
        if !in_scope[s.header] {
            continue;
        }
        if s.suspect {
            return Err(ScopeError::BadIndentIndicator { line: s.header + 1 });
        }
        if s.start < s.end {
            chosen.push(s);
        }
    }
    if chosen.is_empty() {
        return Ok(Protected::Unchanged);
    }

    let mut out = String::with_capacity(source.len());
    let mut regions: Vec<Region<'src>> = Vec::with_capacity(chosen.len());
    let mut next = 0usize;
    let mut i = 0usize;
    while i < lines.len() {
        if next < chosen.len() && chosen[next].start == i {
            let s = chosen[next];
            let last = &lines[s.end - 1];
            let id = regions.len();
            regions.push(Region {
                text: &source[lines[s.start].start..last.end_of_text()],
                header_line: s.header,
                parent_indent: lines[s.header].indent,
                content_indent: s.content_indent,
                first_line: s.start,
                last_line: s.end - 1,
            });
            for _ in 0..s.content_indent {
                out.push(' ');
            }
            out.push_str(MARKER_PREFIX);
            out.push_str(itoa(id).as_str());
            out.push('\u{0}');
            out.push_str(last.term());
            i = s.end;
            next += 1;
        } else {
            out.push_str(lines[i].raw);
            i += 1;
        }
    }

    Ok(Protected::Marked {
        source: out,
        regions,
    })
}

/// Small helper so the marker body does not go through `format!`.
fn itoa(mut n: usize) -> String {
    if n == 0 {
        return "0".to_owned();
    }
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    while n > 0 {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    String::from_utf8_lossy(&buf[i..]).into_owned()
}

/// Puts protected content back, byte for byte.
///
/// The marker's own indentation in the rendered text is ignored: the region
/// carries its original indentation, so restoring is a copy rather than a
/// re-layout. That is what makes `restore(protect(s)) == s` exact.
///
/// A marker may legitimately appear more than once — a `{% for %}` around the
/// resource duplicates it — and may legitimately vanish, if a `{% if %}`
/// removed the resource. What it may not do is arrive mangled, so anything
/// other than a marker alone on its line is refused.
pub fn restore<'r>(rendered: &'r str, regions: &[Region<'_>]) -> Result<Cow<'r, str>, ScopeError> {
    if !rendered.contains(MARKER_PREFIX) {
        return Ok(Cow::Borrowed(rendered));
    }
    let mut out = String::with_capacity(rendered.len());
    for (n, raw) in rendered.split_inclusive('\n').enumerate() {
        if !raw.contains(MARKER_PREFIX) {
            out.push_str(raw);
            continue;
        }
        let text = raw
            .strip_suffix("\r\n")
            .or_else(|| raw.strip_suffix('\n'))
            .unwrap_or(raw);
        let term = &raw[text.len()..];
        let trimmed = text.trim();
        let id = trimmed
            .strip_prefix(MARKER_PREFIX)
            .and_then(|s| s.strip_suffix('\u{0}'))
            .filter(|_| trimmed.matches('\u{0}').count() == 2)
            .and_then(|s| s.parse::<usize>().ok());
        match id {
            Some(id) if id < regions.len() => {
                out.push_str(regions[id].text);
                out.push_str(term);
            }
            Some(_) => return Err(ScopeError::UnknownMarker { line: n + 1 }),
            None => return Err(ScopeError::MarkerNotAlone { line: n + 1 }),
        }
    }
    Ok(Cow::Owned(out))
}

// ---------------------------------------------------------------------------
// Reading the setting
// ---------------------------------------------------------------------------

/// Packages listed in `runtime.options.providerTemplatedPackages`.
///
/// Read textually for the same reason the pass itself is textual: this is
/// needed *before* the file can be parsed. The scan is deliberately narrow —
/// a top-level `runtime:` mapping, its `options:`, and that one key — and
/// returns nothing when the shape is not exactly that. Reading nothing means
/// no protection, which is the behaviour every existing stack already has.
pub fn packages_from_source(source: &str) -> Vec<String> {
    let mut found = Vec::new();
    let lines = split_lines(source);
    let mut runtime_indent = None::<usize>;
    let mut options_indent = None::<usize>;

    let mut i = 0usize;
    while i < lines.len() {
        let line = &lines[i];
        if line.blank || line.comment {
            i += 1;
            continue;
        }
        let (col, _) = node_start(line.text);
        let Some((key, vstart)) = split_key(line.text, col) else {
            i += 1;
            continue;
        };
        let key = unquote(key);
        let value = line.text[vstart..].trim();

        match (runtime_indent, options_indent) {
            (None, _) => {
                if col == 0 && key == "runtime" && value.is_empty() {
                    runtime_indent = Some(col);
                }
            }
            (Some(ri), None) => {
                if col <= ri {
                    runtime_indent = None;
                    continue;
                }
                if key == "options" && value.is_empty() {
                    options_indent = Some(col);
                }
            }
            (Some(ri), Some(oi)) => {
                if col <= ri {
                    runtime_indent = None;
                    options_indent = None;
                    continue;
                }
                if col <= oi {
                    options_indent = None;
                    continue;
                }
                if key == "providerTemplatedPackages" {
                    if value.is_empty() {
                        // Block sequence on the following lines.
                        let mut j = i + 1;
                        while j < lines.len() {
                            let l = &lines[j];
                            if l.blank || l.comment {
                                j += 1;
                                continue;
                            }
                            // A block sequence's entries may sit at the key's own
                            // column or deeper; anything shallower, or anything
                            // that is not an entry at all, ends the list.
                            let (c, dashes) = node_start(l.text);
                            if dashes == 0 || l.indent < col {
                                break;
                            }
                            let item = unquote(l.text[c..].trim());
                            if !item.is_empty() {
                                found.push(item.to_owned());
                            }
                            j += 1;
                        }
                        i = j;
                        continue;
                    }
                    // Flow sequence, or a bare single value.
                    let inner = value
                        .strip_prefix('[')
                        .and_then(|v| v.strip_suffix(']'))
                        .unwrap_or(value);
                    for part in inner.split(',') {
                        let p = unquote(part);
                        if !p.is_empty() {
                            found.push(p.to_owned());
                        }
                    }
                }
            }
        }
        i += 1;
    }
    found
}

/// The effective list for a project.
///
/// `PULUMI_YAML_PROVIDER_TEMPLATED_PACKAGES` (comma separated) overrides the
/// project file, which gives an operator a way to turn the scope off — or on —
/// without editing a stack.
pub fn effective_packages(project_source: &str) -> Vec<String> {
    if let Ok(v) = std::env::var("PULUMI_YAML_PROVIDER_TEMPLATED_PACKAGES") {
        let trimmed = v.trim();
        if trimmed.is_empty() {
            return Vec::new();
        }
        return trimmed
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
    }
    packages_from_source(project_source)
}

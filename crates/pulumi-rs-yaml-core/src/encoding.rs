// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

//! Byte order marks, removed at the boundary where text becomes a value.
//!
//! YAML 1.2 permits a byte order mark at the start of a stream, so a file that
//! begins with one is a legal document and must parse. The backend here does
//! not agree. `serde_yaml` sits on libyaml, which *skips* a leading mark but
//! skips it as a character — the scanner's column advances to 1. The first key
//! therefore opens the root block mapping at indent 1; line 2's key at column 0
//! is shallower, so it unrolls that mapping and closes the document, and the
//! remaining lines start a second implicit one. `from_str` then fails with
//! `deserializing from YAML containing more than one document is not
//! supported` — a complaint about a defect the file does not have. The
//! signature is distinctive: a one-line file parses, two or more lines do not,
//! and the message names the wrong problem. Through the Jinja path the same
//! three bytes surface instead as `found character that cannot start any
//! token`.
//!
//! So the mark is removed here, once, at each boundary where a source string or
//! byte buffer is handed to a deserializer — not at each read. The reported
//! failure arrived as a `str` from a caller that never touched a file, and a
//! read-site strip would have missed it entirely.
//!
//! The contract, in both directions:
//!
//! - **Exactly one mark, at offset zero.** A mark anywhere else is content and
//!   is returned byte for byte, as is a second mark following the first. YAML
//!   allows a mark to begin a stream; it does not make `U+FEFF` disappear from
//!   the middle of a scalar.
//! - **Silent.** No diagnostic is raised. The input conforms to the spec, so
//!   there is nothing to warn about, and there is no honest span to attach to
//!   bytes the spec says are not part of the document. Removing the mark from
//!   the file on disk is a separate concern, and belongs to whichever tool owns
//!   the file.
//! - **Zero-copy.** Both functions return a subslice of their argument. No
//!   allocation, no copy, on any parse.
//! - **Nothing is masked.** A file that genuinely holds two documents still
//!   holds two after the mark is gone, and still fails with the same message.
//!   Only the phantom disappears.
//!
//! UTF-16 and UTF-32 marks are deliberately left alone. They are not a leading
//! `U+FEFF` in a UTF-8 stream, they are a different encoding, and silently
//! dropping the two bytes would leave a NUL-riddled buffer that fails later and
//! further from the cause.

#![forbid(unsafe_code)]

/// The byte order mark as a character: `U+FEFF`, zero width no-break space.
pub const UTF8_BOM: char = '\u{feff}';

/// The byte order mark as UTF-8 encodes it.
pub const UTF8_BOM_BYTES: [u8; 3] = [0xef, 0xbb, 0xbf];

/// Returns `source` without a leading byte order mark.
///
/// The result borrows `source`: on the unmarked path it *is* `source`, and on
/// the marked path it is the subslice starting three bytes in. See the module
/// documentation for why the strip happens here and what it deliberately does
/// not do.
#[must_use]
pub fn strip_bom(source: &str) -> &str {
    source.strip_prefix(UTF8_BOM).unwrap_or(source)
}

/// Returns `data` without a leading byte order mark, for buffers that are not
/// yet known to be UTF-8.
///
/// The byte form exists for readers that hand a deserializer raw bytes — a
/// 56 MB provider schema should not pay for a UTF-8 validation pass to have
/// three bytes removed.
#[must_use]
pub fn strip_bom_bytes(data: &[u8]) -> &[u8] {
    data.strip_prefix(&UTF8_BOM_BYTES).unwrap_or(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOM: &str = "\u{feff}";

    #[test]
    fn a_mark_at_offset_zero_is_the_only_one_removed() {
        let doubled = format!("{BOM}{BOM}name: app\n");
        assert_eq!(strip_bom(&doubled), format!("{BOM}name: app\n"));
        assert_eq!(strip_bom(&format!("{BOM}name: app\n")), "name: app\n");
        // The mark alone leaves an empty document, not an error and not a space.
        assert_eq!(strip_bom(BOM), "");
    }

    #[test]
    fn a_mark_anywhere_but_offset_zero_is_content() {
        let mid = format!("name: a{BOM}b\n");
        assert_eq!(strip_bom(&mid), mid);
        let trailing = format!("name: app\n{BOM}");
        assert_eq!(strip_bom(&trailing), trailing);
        // A newline first means the stream does not begin with the mark.
        let after_newline = format!("\n{BOM}name: app\n");
        assert_eq!(strip_bom(&after_newline), after_newline);
    }

    #[test]
    fn unmarked_text_is_returned_without_a_copy() {
        for source in ["", "name: app\n", "\u{fffe}not a mark", "\u{ef}\u{bb}"] {
            assert!(
                std::ptr::eq(strip_bom(source), source),
                "unmarked input must be handed straight back: {source:?}"
            );
        }
    }

    #[test]
    fn a_stripped_slice_borrows_three_bytes_into_the_input() {
        let marked = format!("{BOM}name: app\n");
        let out = strip_bom(&marked);
        assert_eq!(
            out.as_ptr(),
            marked.as_ptr().wrapping_add(UTF8_BOM_BYTES.len()),
            "the result must be a subslice, not an allocation"
        );
        assert_eq!(out.len(), marked.len() - UTF8_BOM_BYTES.len());
    }

    #[test]
    fn strip_bom_bytes_removes_the_encoded_mark_and_nothing_else() {
        let marked = b"\xef\xbb\xbf{}";
        let out = strip_bom_bytes(marked);
        assert_eq!(out, b"{}");
        assert_eq!(
            out.as_ptr(),
            marked.as_ptr().wrapping_add(UTF8_BOM_BYTES.len()),
            "the byte form is a subslice too"
        );
        // Shorter than a mark, a partial mark, and a mark that is content.
        for data in [&b""[..], &b"\xef\xbb"[..], &b"{}\xef\xbb\xbf"[..]] {
            assert!(std::ptr::eq(strip_bom_bytes(data), data));
        }
    }

    #[test]
    fn a_utf16_mark_is_neither_stripped_nor_decoded() {
        // UTF-16LE and UTF-16BE marks are a different encoding, not a leading
        // U+FEFF in a UTF-8 stream. Dropping two bytes would hand the parser a
        // NUL-riddled buffer that fails further from its cause.
        for data in [&b"\xff\xfen\x00a\x00"[..], &b"\xfe\xff\x00n\x00a"[..]] {
            assert!(std::ptr::eq(strip_bom_bytes(data), data));
            assert!(std::str::from_utf8(data).is_err());
        }
    }
}

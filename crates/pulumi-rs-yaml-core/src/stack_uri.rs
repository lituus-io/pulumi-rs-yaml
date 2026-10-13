// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

//! Where a stack's state lives, as a URI the graph export can carry.
//!
//! The exporter is a pure function of the program, so it cannot ask a backend
//! anything; it is TOLD the backend, and derives from it a URI for the stack
//! being exported, for every stack a `pulumi:pulumi:StackReference` names --
//! which resolves on the same backend, so its location is just as derivable --
//! and for each resource. A backend this module does not recognise yields no
//! URI at all, never a guess.
//!
//! # Formats
//!
//! | Backend | Stack | Resource |
//! |---|---|---|
//! | Pulumi Cloud (`https://api.pulumi.com`, `https://app.pulumi.com`) | `https://app.pulumi.com/{org}/{project}/{stack}` | stack URI `#` URN |
//! | Self-managed (`gs://`, `s3://`, `azblob://`, `file://`) | `{base}/.pulumi/stacks/{project}/{stack}.json` | stack URI `#` URN |
//!
//! The self-managed path is the project-scoped layout Pulumi uses for every
//! backend created by 3.61 or later; a backend still on the older global
//! layout (`.pulumi/stacks/{stack}.json`) needs `pulumi state upgrade` for the
//! path to point at its state. A resource URI is a LOCATOR -- the stack's
//! state plus the resource's URN, percent-encoded, as the fragment -- not a
//! console deep link: Pulumi documents no URL for a single resource.
//!
//! # What is refused
//!
//! A URL carrying credentials or anything a URI cannot hold is refused
//! outright: whitespace or control characters anywhere, or userinfo (`@`)
//! in the authority. The query and fragment of a self-managed URL are dropped
//! -- they carry settings such as an AWS profile or a region, which are not
//! part of where the state is and may not be meant for export. A Pulumi Cloud
//! URI is omitted when the organization is not known, because it would name
//! the wrong one.

#![forbid(unsafe_code)]

use std::fmt;

/// A backend this module can derive URIs for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend<'a> {
    /// The hosted Pulumi Cloud.
    Cloud,
    /// A self-managed backend; `base` is the URL with no query, fragment or
    /// trailing slash.
    SelfManaged { base: &'a str },
}

/// Recognises a backend URL. `None` for anything else: a self-hosted Pulumi
/// Cloud, an unknown scheme, or a URL that is refused (see the module docs).
pub fn parse_backend(url: &str) -> Option<Backend<'_>> {
    if url.is_empty() || url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return None;
    }
    let (scheme, rest) = url.split_once("://")?;
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    if rest[..authority_end].contains('@') {
        return None;
    }
    if scheme.eq_ignore_ascii_case("https") || scheme.eq_ignore_ascii_case("http") {
        let host = &rest[..authority_end];
        let after = &rest[authority_end..];
        let cloud_host = host.eq_ignore_ascii_case("api.pulumi.com")
            || host.eq_ignore_ascii_case("app.pulumi.com");
        return (cloud_host && (after.is_empty() || after == "/")).then_some(Backend::Cloud);
    }
    let self_managed = ["gs", "s3", "azblob", "file"]
        .iter()
        .any(|s| scheme.eq_ignore_ascii_case(s));
    if !self_managed {
        return None;
    }
    let end = url.find(['?', '#']).unwrap_or(url.len());
    let base = url[..end].trim_end_matches('/');
    // `gs://` alone names no bucket.
    if base.len() <= scheme.len() + 3 {
        return None;
    }
    Some(Backend::SelfManaged { base })
}

/// The backend URL to use: the caller's own, else the environment's, else the
/// project's `backend.url`, skipping any that is empty.
pub fn resolve_backend_url<'a>(
    explicit: Option<&'a str>,
    environment: Option<&'a str>,
    project: Option<&'a str>,
) -> Option<&'a str> {
    [explicit, environment, project]
        .into_iter()
        .flatten()
        .find(|u| !u.is_empty())
}

/// The `backend.url` a `Pulumi.yaml` declares, if any. Jinja block lines are
/// stripped first, as the exporters' other pre-pass reads do.
pub fn backend_url_from_project_source(source: &str) -> Option<String> {
    let stripped = crate::jinja::strip_jinja_blocks(source);
    let yaml: serde_yaml::Value =
        serde_yaml::from_str(crate::encoding::strip_bom(&stripped)).ok()?;
    let backend = yaml.as_mapping()?.iter().find_map(|(k, v)| {
        k.as_str()
            .is_some_and(|k| k.eq_ignore_ascii_case("backend"))
            .then_some(v)
    })?;
    backend.as_mapping()?.iter().find_map(|(k, v)| {
        (k.as_str()?.eq_ignore_ascii_case("url"))
            .then(|| v.as_str().map(str::to_owned))
            .flatten()
    })
}

/// A stack's location. Formats without building intermediate strings.
#[derive(Debug, Clone, Copy)]
pub struct StackUri<'a> {
    pub backend: Backend<'a>,
    pub organization: &'a str,
    pub project: &'a str,
    pub stack: &'a str,
}

impl<'a> StackUri<'a> {
    /// `None` for Pulumi Cloud when the organization is not known.
    pub fn new(
        backend: Backend<'a>,
        organization: &'a str,
        organization_known: bool,
        project: &'a str,
        stack: &'a str,
    ) -> Option<Self> {
        if matches!(backend, Backend::Cloud) && !organization_known {
            return None;
        }
        Some(Self {
            backend,
            organization,
            project,
            stack,
        })
    }

    /// The location of the resource `urn` in this stack.
    pub fn resource(self, urn: &'a str) -> ResourceUri<'a> {
        ResourceUri { stack: self, urn }
    }
}

impl fmt::Display for StackUri<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.backend {
            Backend::Cloud => {
                f.write_str("https://app.pulumi.com/")?;
                percent_encode(f, self.organization)?;
                f.write_str("/")?;
                percent_encode(f, self.project)?;
                f.write_str("/")?;
                percent_encode(f, self.stack)
            }
            Backend::SelfManaged { base } => {
                f.write_str(base)?;
                f.write_str("/.pulumi/stacks/")?;
                percent_encode(f, self.project)?;
                f.write_str("/")?;
                percent_encode(f, self.stack)?;
                f.write_str(".json")
            }
        }
    }
}

/// A resource's location: its stack's URI with the URN as the fragment.
#[derive(Debug, Clone, Copy)]
pub struct ResourceUri<'a> {
    pub stack: StackUri<'a>,
    pub urn: &'a str,
}

impl fmt::Display for ResourceUri<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}#", self.stack)?;
        percent_encode(f, self.urn)
    }
}

/// Writes `s` with every byte outside the RFC 3986 unreserved set, plus `:`
/// (which URNs are made of, and which needs no escaping in a path segment or
/// fragment), percent-encoded. Never writes whitespace, a control character,
/// `/`, `?` or `#` -- so no value can change the shape of the URI.
fn percent_encode(f: &mut fmt::Formatter<'_>, s: &str) -> fmt::Result {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    // A segment that IS `.` or `..` is a dot-segment, which a resolver
    // collapses -- a stack named `..` would step out of its directory. Its
    // dots are escaped; a dot anywhere else is an ordinary character.
    if s == "." || s == ".." {
        for _ in 0..s.len() {
            f.write_str("%2E")?;
        }
        return Ok(());
    }
    // `start..i` is a run of kept bytes, all ASCII, so both ends are
    // character boundaries -- but only when the run is non-empty: between two
    // bytes of one escaped character `start` sits INSIDE it, and even an
    // empty slice there panics. That is the slice this used to take.
    let mut start = 0;
    for (i, &b) in s.as_bytes().iter().enumerate() {
        let keep = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b':');
        if !keep {
            if start < i {
                f.write_str(&s[start..i])?;
            }
            let escaped = [b'%', HEX[usize::from(b >> 4)], HEX[usize::from(b & 0x0f)]];
            // Three ASCII bytes are always valid UTF-8.
            f.write_str(std::str::from_utf8(&escaped).map_err(|_| fmt::Error)?)?;
            start = i + 1;
        }
    }
    f.write_str(&s[start..])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stack(url: &str, org_known: bool) -> Option<String> {
        let backend = parse_backend(url)?;
        StackUri::new(backend, "acme", org_known, "data-platform", "prod").map(|u| u.to_string())
    }

    #[test]
    fn backends_and_their_stack_uris() {
        for (url, want) in [
            (
                "https://api.pulumi.com",
                Some("https://app.pulumi.com/acme/data-platform/prod"),
            ),
            (
                "https://app.pulumi.com/",
                Some("https://app.pulumi.com/acme/data-platform/prod"),
            ),
            (
                "HTTPS://API.PULUMI.COM",
                Some("https://app.pulumi.com/acme/data-platform/prod"),
            ),
            (
                "gs://state-bucket",
                Some("gs://state-bucket/.pulumi/stacks/data-platform/prod.json"),
            ),
            (
                "gs://state-bucket/",
                Some("gs://state-bucket/.pulumi/stacks/data-platform/prod.json"),
            ),
            (
                "gs://b/prefix/dir",
                Some("gs://b/prefix/dir/.pulumi/stacks/data-platform/prod.json"),
            ),
            (
                "s3://b?region=us-east-1&profile=ops",
                Some("s3://b/.pulumi/stacks/data-platform/prod.json"),
            ),
            (
                "azblob://container",
                Some("azblob://container/.pulumi/stacks/data-platform/prod.json"),
            ),
            (
                "file:///var/pulumi",
                Some("file:///var/pulumi/.pulumi/stacks/data-platform/prod.json"),
            ),
            ("https://pulumi.example.com", None),
            ("https://api.pulumi.com/some/path", None),
            ("ftp://x", None),
            ("gs://", None),
            ("", None),
            ("not a url", None),
            ("gs://b\n/evil", None),
            ("https://user:secret@api.pulumi.com", None),
            ("s3://key:secret@bucket", None),
        ] {
            assert_eq!(stack(url, true).as_deref(), want, "{url:?}");
        }
    }

    #[test]
    fn a_cloud_uri_needs_the_organization() {
        assert_eq!(stack("https://api.pulumi.com", false), None);
        // A self-managed path has no organization in it.
        assert!(stack("gs://b", false).is_some());
    }

    #[test]
    fn segments_and_urns_are_encoded() {
        let backend = parse_backend("gs://b").expect("backend");
        let s = StackUri::new(backend, "o", true, "my proj", "a/b?c#d").expect("uri");
        assert_eq!(
            s.to_string(),
            "gs://b/.pulumi/stacks/my%20proj/a%2Fb%3Fc%23d.json"
        );
        let urn = "urn:pulumi:prod::p::gcp:storage/bucket:Bucket::my bucket#1";
        assert_eq!(
            s.resource(urn).to_string(),
            "gs://b/.pulumi/stacks/my%20proj/a%2Fb%3Fc%23d.json#urn:pulumi:prod::p::gcp:storage%2Fbucket:Bucket::my%20bucket%231"
        );
    }

    #[test]
    fn multibyte_names_are_escaped_byte_by_byte() {
        // Found by fuzz_resource_graph: an em dash in a resource name.
        let backend = parse_backend("gs://b").expect("backend");
        let s = StackUri::new(backend, "o", true, "proj—x", "żółw").expect("uri");
        assert_eq!(
            s.to_string(),
            "gs://b/.pulumi/stacks/proj%E2%80%94x/%C5%BC%C3%B3%C5%82w.json"
        );
        let r = s.resource("urn:pulumi:prod::p::t:m:A—B::a—").to_string();
        assert!(
            r.ends_with("#urn:pulumi:prod::p::t:m:A%E2%80%94B::a%E2%80%94"),
            "{r}"
        );
    }

    #[test]
    fn a_dot_segment_is_escaped() {
        let backend = parse_backend("gs://b").expect("backend");
        let s = StackUri::new(backend, "o", true, "..", ".").expect("uri");
        assert_eq!(s.to_string(), "gs://b/.pulumi/stacks/%2E%2E/%2E.json");
        let s = StackUri::new(backend, "o", true, "a..b", "v1.2").expect("uri");
        assert_eq!(s.to_string(), "gs://b/.pulumi/stacks/a..b/v1.2.json");
    }

    #[test]
    fn precedence_skips_empty_values() {
        assert_eq!(
            resolve_backend_url(Some("gs://a"), Some("gs://b"), Some("gs://c")),
            Some("gs://a")
        );
        assert_eq!(
            resolve_backend_url(Some(""), Some("gs://b"), None),
            Some("gs://b")
        );
        assert_eq!(
            resolve_backend_url(None, None, Some("gs://c")),
            Some("gs://c")
        );
        assert_eq!(resolve_backend_url(None, Some(""), None), None);
    }

    #[test]
    fn backend_url_from_a_project_file() {
        let src = "name: p\nruntime: yaml\nbackend:\n  url: gs://state\n";
        assert_eq!(
            backend_url_from_project_source(src).as_deref(),
            Some("gs://state")
        );
        let src = "\u{feff}name: p\n{% if true %}\n{% endif %}\nBackend:\n  URL: s3://x\n";
        assert_eq!(
            backend_url_from_project_source(src).as_deref(),
            Some("s3://x")
        );
        assert_eq!(backend_url_from_project_source("name: p\n"), None);
        assert_eq!(
            backend_url_from_project_source("backend: gs://flat\n"),
            None
        );
        assert_eq!(backend_url_from_project_source(": not yaml : ["), None);
    }
}

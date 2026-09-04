// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

//! An `(id, urn)` index read out of a Pulumi checkpoint, without copying it.
//!
//! A stack's checkpoint is the one place that records which physical resources
//! a stack manages. A tool that has to answer "does another stack already own
//! this id?" reads sibling checkpoints out of the shared state backend, and the
//! answer decides whether a resource may be deleted. That makes this a very
//! small module with a very sharp contract.
//!
//! # Two encodings, one meaning
//!
//! The same deployment is written two ways, and both are in circulation:
//!
//! ```json
//! {"version": 3, "checkpoint": {"latest": {"resources": [ … ]}}}   // on disk
//! {"version": 3, "deployment": {"resources": [ … ]}}                // exported
//! ```
//!
//! The first is the blob a backend stores; the second is what an export
//! produces. Reading only one of them is not a partial answer — it is a
//! confident empty one, which is exactly the failure this module exists to
//! prevent. Both are accepted; a document carrying *both* keys is rejected as
//! [`CheckpointError::AmbiguousDeployment`] rather than silently preferring
//! one, because a document that means two things cannot be used to authorise a
//! delete.
//!
//! A `checkpoint` whose `latest` is absent or `null` is a stack that has never
//! been deployed. That is [`Shape::Empty`]: a real answer, "this stack manages
//! nothing". It is deliberately distinct from [`Shape::Resources`] with no
//! entries, which is a deployed stack whose resource list is empty — the
//! writer omits the key in that case.
//!
//! # Why an error, never a quiet `None`
//!
//! `evaluate_str_invoke` answers `None` for "not answered here", and that is
//! right for a resolver: an unanswered name simply stays dynamic. The contract
//! here is the opposite one. This index feeds an ownership gate, where
//! "unanswered" and "owns nothing" are the same value and the second one
//! authorises a deletion. So every failure to understand the bytes is an
//! `Err`, named, and the caller is expected to fail closed on it. There is no
//! input for which this returns an empty index it is not sure about.
//!
//! `version` is checked against [`SUPPORTED_VERSION`] for the same reason. A
//! future version that relocates `resources` would otherwise deserialise
//! cleanly into "zero resources" — a well-formed lie.
//!
//! # What is read
//!
//! A resource contributes an entry when it carries a string `id`; a resource
//! with no `id` (or `null`) has not been created yet and is skipped. `urn` is
//! required. Every other field — `type`, `custom`, `inputs`, `outputs`,
//! `dependencies` — is ignored without being materialised. Nesting depth is
//! bounded by `serde_json`'s recursion limit, so a hostile document is an
//! error rather than a stack overflow.
//!
//! # Zero-copy
//!
//! Bytes in ([`&[u8]`](slice)), [`Cow`] out. A JSON string that carries no
//! escape is borrowed straight out of the caller's buffer; only a string with
//! an escape (or a non-UTF-8 sequence, which is an error) allocates. A
//! 37 KiB checkpoint of a hundred resources therefore allocates one vector of
//! pointer pairs and nothing else.
//!
//! Never a guess: this reads the bytes it is handed and nothing else. It never
//! reads a file, the network, or a plugin.

#![forbid(unsafe_code)]

use std::borrow::Cow;
use std::collections::HashSet;
use std::fmt;
use std::marker::PhantomData;

use serde::de::value::MapAccessDeserializer;
use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer};

/// The only checkpoint version this reader understands.
pub const SUPPORTED_VERSION: u64 = 3;

/// Why a document could not be read as a checkpoint.
#[derive(Debug, thiserror::Error)]
pub enum CheckpointError {
    #[error("invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("no `version` field")]
    MissingVersion,
    #[error("unsupported checkpoint version {0}; expected {SUPPORTED_VERSION}")]
    UnsupportedVersion(u64),
    #[error("neither `checkpoint` nor `deployment` is present")]
    NoDeployment,
    #[error("both `checkpoint` and `deployment` are present")]
    AmbiguousDeployment,
}

/// Whether the document carried a deployment at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
    /// A deployment was present; `entries` is what it manages.
    Resources,
    /// `checkpoint.latest` was absent or `null`: the stack has never deployed.
    Empty,
}

impl Shape {
    pub fn as_str(&self) -> &'static str {
        match self {
            Shape::Resources => "resources",
            Shape::Empty => "empty",
        }
    }
}

/// The `(id, urn)` pairs a checkpoint records, in document order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Index<'a> {
    pub shape: Shape,
    pub entries: Vec<(Cow<'a, str>, Cow<'a, str>)>,
}

/// The ids a caller cares about.
///
/// A target may be written in full (`projects/p/locations/l/workflows/w`) or
/// as the leaf name alone (`w`), because a caller usually knows the name it
/// declared and not the provider-assembled id. Matching therefore accepts
/// either: the resource's whole id equals a target, or the resource's leaf —
/// everything after the last `/` — equals a target's leaf.
///
/// Matching on the leaf can admit an id a caller did not mean. For an
/// ownership gate that is the safe direction: a surplus candidate is examined,
/// a missing one is deleted. An empty filter matches nothing at all; "no
/// filter" is spelled `None`, not an empty set.
///
/// The targets are owned deliberately. A filter is built once and reused
/// across a whole batch, and [`IdFilter::matches`] is then a `&str` lookup
/// that allocates nothing.
#[derive(Debug, Clone, Default)]
pub struct IdFilter {
    full: HashSet<String>,
    leaf: HashSet<String>,
}

impl IdFilter {
    pub fn new<'i>(ids: impl IntoIterator<Item = &'i str>) -> Self {
        let mut full = HashSet::new();
        let mut leaf = HashSet::new();
        for id in ids {
            leaf.insert(leaf_of(id).to_string());
            full.insert(id.to_string());
        }
        Self { full, leaf }
    }

    pub fn matches(&self, id: &str) -> bool {
        self.full.contains(id) || self.leaf.contains(leaf_of(id))
    }
}

fn leaf_of(id: &str) -> &str {
    match id.rsplit_once('/') {
        Some((_, tail)) => tail,
        None => id,
    }
}

/// Read one checkpoint document.
///
/// The returned [`Index`] borrows from `bytes`. Every failure is named; there
/// is no input that yields a silently empty result.
pub fn index_checkpoint<'a>(
    bytes: &'a [u8],
    filter: Option<&IdFilter>,
) -> Result<Index<'a>, CheckpointError> {
    let Object(doc): Object<Document<'a>> = serde_json::from_slice(bytes)?;

    let Some(version) = doc.version else {
        return Err(CheckpointError::MissingVersion);
    };
    if version != SUPPORTED_VERSION {
        return Err(CheckpointError::UnsupportedVersion(version));
    }

    let latest = match (doc.checkpoint, doc.deployment) {
        (Some(_), Some(_)) => return Err(CheckpointError::AmbiguousDeployment),
        (None, None) => return Err(CheckpointError::NoDeployment),
        (Some(Object(body)), None) => body.latest,
        (None, Some(deployment)) => Some(deployment),
    };
    let Some(Object(latest)) = latest else {
        return Ok(Index {
            shape: Shape::Empty,
            entries: Vec::new(),
        });
    };

    let mut entries = Vec::new();
    if filter.is_none() {
        entries.reserve_exact(latest.resources.len());
    }
    for Object(resource) in latest.resources {
        let Some(id) = resource.id else { continue };
        if filter.is_none_or(|f| f.matches(&id)) {
            entries.push((id, resource.urn));
        }
    }
    Ok(Index {
        shape: Shape::Resources,
        entries,
    })
}

/// Read many documents, one result per input, in input order.
///
/// One unreadable document never hides the others: it occupies its own slot as
/// an `Err`. `parallel` caps the worker count; the pool is scoped to this call
/// and sized `min(parallel, docs.len())`, so a batch of three never starts
/// thirty-two threads. A pool that cannot be built is not fatal — the batch is
/// read sequentially instead.
pub fn index_checkpoints<'a>(
    docs: &[&'a [u8]],
    filter: Option<&IdFilter>,
    parallel: usize,
) -> Vec<Result<Index<'a>, CheckpointError>> {
    if parallel <= 1 || docs.len() <= 1 {
        return sequential(docs, filter);
    }
    let Ok(pool) = rayon::ThreadPoolBuilder::new()
        .num_threads(parallel.min(docs.len()))
        .build()
    else {
        return sequential(docs, filter);
    };
    pool.install(|| {
        use rayon::prelude::*;
        docs.par_iter()
            .map(|doc| index_checkpoint(doc, filter))
            .collect()
    })
}

fn sequential<'a>(
    docs: &[&'a [u8]],
    filter: Option<&IdFilter>,
) -> Vec<Result<Index<'a>, CheckpointError>> {
    docs.iter()
        .map(|doc| index_checkpoint(doc, filter))
        .collect()
}

// The wire shape. Unknown fields are ignored rather than rejected: a
// checkpoint carries far more than this reader needs, and none of it is
// deserialised.
//
// Every one of these is read through `Object`, and that is load-bearing.
// serde's derived struct visitor also accepts a JSON *array*, filling absent
// fields from their defaults — so `"latest": []` would deserialise cleanly
// into a deployment that manages nothing, and `[3, null, {}]` into a whole
// checkpoint. Those are the confident empty answers this module exists to
// refuse, so a shape that is an object in a real checkpoint is required to be
// one here.
#[derive(Deserialize)]
struct Document<'a> {
    version: Option<u64>,
    #[serde(borrow)]
    checkpoint: Option<Object<CheckpointBody<'a>>>,
    #[serde(borrow)]
    deployment: Option<Object<Deployment<'a>>>,
}

#[derive(Deserialize)]
struct CheckpointBody<'a> {
    #[serde(borrow)]
    latest: Option<Object<Deployment<'a>>>,
}

#[derive(Deserialize)]
struct Deployment<'a> {
    // Omitted by the writer for a deployed stack that manages nothing, which
    // is `Resources` with no entries — not `Empty`.
    #[serde(borrow, default)]
    resources: Vec<Object<Resource<'a>>>,
}

#[derive(Deserialize)]
struct Resource<'a> {
    #[serde(borrow)]
    urn: Cow<'a, str>,
    // Absent or null: not created yet, so it owns nothing and is skipped. A
    // non-string `id` is a malformed checkpoint, not a resource to skip, and
    // surfaces as a JSON error.
    #[serde(borrow, default, deserialize_with = "borrowed_option")]
    id: Option<Cow<'a, str>>,
}

/// A `T` that must have been written as a JSON object.
///
/// The inner value is still deserialised by the original deserializer, so
/// strings are borrowed exactly as they would be without the wrapper.
struct Object<T>(T);

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Object<T> {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        struct ObjectVisitor<T>(PhantomData<T>);

        impl<'de, T: Deserialize<'de>> Visitor<'de> for ObjectVisitor<T> {
            type Value = T;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a JSON object")
            }

            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<T, A::Error> {
                T::deserialize(MapAccessDeserializer::new(map))
            }
        }

        de.deserialize_map(ObjectVisitor(PhantomData)).map(Object)
    }
}

/// serde's `borrow` special-case covers `Cow<str>` but not `Option<Cow<str>>`,
/// which falls back to the owned impl and copies every id — the one field this
/// module exists to hand back. The optional case is therefore read through a
/// newtype that does get the special-case. `plain_strings_are_borrowed` is
/// what keeps this honest.
fn borrowed_option<'de: 'a, 'a, D>(de: D) -> Result<Option<Cow<'a, str>>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    struct Borrowed<'a>(#[serde(borrow)] Cow<'a, str>);

    Ok(Option::<Borrowed<'a>>::deserialize(de)?.map(|b| b.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    const URN: &str = "urn:pulumi:dev::app::gcp:workflows/workflow:Workflow::w";
    const ID: &str = "projects/p/locations/l/workflows/w";

    fn disk(resources: &str) -> String {
        format!(r#"{{"version":3,"checkpoint":{{"latest":{{"resources":[{resources}]}}}}}}"#)
    }

    fn one_resource() -> String {
        format!(r#"{{"urn":"{URN}","id":"{ID}","type":"gcp:workflows/workflow:Workflow"}}"#)
    }

    /// `CheckpointError` is not `PartialEq`, so results are compared through
    /// their rendered messages.
    fn comparable(
        results: Vec<Result<Index<'_>, CheckpointError>>,
    ) -> Vec<Result<Index<'_>, String>> {
        results
            .into_iter()
            .map(|r| r.map_err(|e| e.to_string()))
            .collect()
    }

    fn index(doc: &str) -> Index<'_> {
        let Ok(idx) = index_checkpoint(doc.as_bytes(), None) else {
            unreachable!("fixture must read")
        };
        idx
    }

    #[test]
    fn disk_shape_yields_id_urn_pairs() {
        let doc = disk(&one_resource());
        let idx = index(&doc);
        assert_eq!(idx.shape, Shape::Resources);
        assert_eq!(idx.entries, vec![(Cow::from(ID), Cow::from(URN))]);
    }

    #[test]
    fn export_shape_yields_id_urn_pairs() {
        let doc = format!(
            r#"{{"version":3,"deployment":{{"resources":[{}]}}}}"#,
            one_resource()
        );
        let idx = index(&doc);
        assert_eq!(idx.shape, Shape::Resources);
        assert_eq!(idx.entries, vec![(Cow::from(ID), Cow::from(URN))]);
    }

    #[test]
    fn null_latest_is_empty() {
        let idx = index(r#"{"version":3,"checkpoint":{"latest":null}}"#);
        assert_eq!(idx.shape, Shape::Empty);
        assert!(idx.entries.is_empty());
    }

    #[test]
    fn absent_latest_is_empty() {
        let idx = index(r#"{"version":3,"checkpoint":{}}"#);
        assert_eq!(idx.shape, Shape::Empty);
        assert!(idx.entries.is_empty());
    }

    #[test]
    fn absent_resources_is_resources_with_no_entries() {
        let idx = index(r#"{"version":3,"checkpoint":{"latest":{}}}"#);
        assert_eq!(
            idx.shape,
            Shape::Resources,
            "a deployed stack managing nothing is not an undeployed one"
        );
        assert!(idx.entries.is_empty());
    }

    #[test]
    fn resources_without_an_id_are_skipped() {
        let doc = disk(&format!(
            r#"{{"urn":"{URN}"}},{{"urn":"{URN}","id":null}},{}"#,
            one_resource()
        ));
        let idx = index(&doc);
        assert_eq!(idx.entries.len(), 1);
    }

    #[test]
    fn filter_keeps_full_id_matches() {
        let doc = disk(&one_resource());
        let filter = IdFilter::new([ID]);
        let Ok(idx) = index_checkpoint(doc.as_bytes(), Some(&filter)) else {
            unreachable!("fixture must read")
        };
        assert_eq!(idx.entries.len(), 1);
    }

    #[test]
    fn filter_keeps_leaf_matches() {
        let doc = disk(&one_resource());
        let filter = IdFilter::new(["w"]);
        let Ok(idx) = index_checkpoint(doc.as_bytes(), Some(&filter)) else {
            unreachable!("fixture must read")
        };
        assert_eq!(idx.entries.len(), 1);
    }

    #[test]
    fn filter_drops_non_matches() {
        let doc = disk(&one_resource());
        let filter = IdFilter::new(["projects/p/locations/l/workflows/other"]);
        let Ok(idx) = index_checkpoint(doc.as_bytes(), Some(&filter)) else {
            unreachable!("fixture must read")
        };
        assert!(idx.entries.is_empty());
        let empty = IdFilter::default();
        let Ok(idx) = index_checkpoint(doc.as_bytes(), Some(&empty)) else {
            unreachable!("fixture must read")
        };
        assert!(idx.entries.is_empty(), "an empty filter matches nothing");
    }

    #[test]
    fn plain_strings_are_borrowed() {
        let doc = disk(&one_resource());
        let idx = index(&doc);
        let [(id, urn)] = idx.entries.as_slice() else {
            unreachable!("one entry")
        };
        assert!(matches!(id, Cow::Borrowed(_)), "id was copied");
        assert!(matches!(urn, Cow::Borrowed(_)), "urn was copied");
    }

    #[test]
    fn escaped_strings_are_owned_and_unescaped() {
        let doc = r#"{"version":3,"checkpoint":{"latest":{"resources":[
            {"urn":"urn:pulumi:dev::app::gcp:workflows/workflow:Workflow::xé",
             "id":"a\/b"}]}}}"#;
        let idx = index(doc);
        let [(id, urn)] = idx.entries.as_slice() else {
            unreachable!("one entry")
        };
        assert_eq!(id.as_ref(), "a/b");
        assert!(matches!(id, Cow::Owned(_)), "an escape must be decoded");
        assert!(urn.as_ref().ends_with("xé"));
    }

    #[test]
    fn batch_equals_sequential() {
        let good = disk(&one_resource());
        let docs: Vec<&[u8]> = vec![good.as_bytes(), b"{", good.as_bytes(), b"{}"];
        let seq = index_checkpoints(&docs, None, 1);
        let par = index_checkpoints(&docs, None, 4);
        assert_eq!(comparable(seq), comparable(par));
    }

    #[test]
    fn batch_with_parallel_above_len_is_capped() {
        let good = disk(&one_resource());
        let docs: Vec<&[u8]> = vec![good.as_bytes(), good.as_bytes()];
        let out = index_checkpoints(&docs, None, 4096);
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(Result::is_ok));
    }

    #[test]
    fn batch_of_one_is_sequential() {
        let good = disk(&one_resource());
        let docs: Vec<&[u8]> = vec![good.as_bytes()];
        let out = index_checkpoints(&docs, None, 32);
        assert_eq!(out.len(), 1);
        assert!(index_checkpoints(&[], None, 32).is_empty());
    }

    #[test]
    fn error_display_strings() {
        let cases: [(&[u8], &str); 4] = [
            (b"{}", "no `version` field"),
            (
                br#"{"version":4,"deployment":{}}"#,
                "unsupported checkpoint version 4; expected 3",
            ),
            (
                br#"{"version":3}"#,
                "neither `checkpoint` nor `deployment` is present",
            ),
            (
                br#"{"version":3,"checkpoint":{},"deployment":{}}"#,
                "both `checkpoint` and `deployment` are present",
            ),
        ];
        for (bytes, expected) in cases {
            let Err(err) = index_checkpoint(bytes, None) else {
                unreachable!("{expected} must be an error")
            };
            assert_eq!(err.to_string(), expected);
        }
        let Err(err) = index_checkpoint(b"not json", None) else {
            unreachable!("must be an error")
        };
        assert!(err.to_string().starts_with("invalid JSON: "));
    }
}

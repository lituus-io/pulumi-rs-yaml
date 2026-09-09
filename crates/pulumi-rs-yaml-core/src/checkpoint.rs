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
//! required. `type` is read, and `inputs` is captured as a borrowed slice —
//! not parsed — so that an element can be projected out of it afterwards.
//! `custom`, `outputs`, `dependencies` and everything else are ignored without
//! being materialised. Nesting depth is bounded by `serde_json`'s recursion
//! limit, so a hostile document is an error rather than a stack overflow.
//!
//! Ignored means ignored, and that has one visible consequence: a byte
//! sequence that is not UTF-8 inside a field this never returns is not an
//! error, because nothing ever decodes it. The same is true of `serde_json`'s
//! own structural scan. It is safe because the strings that do come back are
//! [`str`], so they are valid by construction, and invalid UTF-8 in a `urn`,
//! an `id`, a `type` or anywhere inside `inputs` is an error like any other
//! malformed document. `inputs` is on that list because capturing a slice
//! validates it as UTF-8 even when nothing is projected out of it; a
//! checkpoint whose `inputs` are not UTF-8 is corrupt either way.
//!
//! # Why an element, and not just an id
//!
//! Some providers give every member of a parent's array the parent's own id.
//! Each `access[]` entry of a dataset, for instance, is a resource whose id is
//! the dataset's path — so an index keyed on ids alone reports that two stacks
//! manage "the same" resource when in truth they manage two different elements
//! of one array, and an ownership gate reading it refuses a removal it should
//! have allowed. The distinction is only in the resource's declared `inputs`.
//!
//! [`ElementSpec`] is how a caller asks for it: a set of input keys per
//! resource type. A resource whose `type` is named there has the listed
//! top-level keys of its `inputs` projected into [`Entry::element`], still
//! borrowed from the caller's buffer; every other resource, and every unnamed
//! key, is skipped exactly as in a scan that asked for nothing. Passing `None`
//! is that scan, and it behaves as it always has.
//!
//! # Zero-copy
//!
//! Bytes in ([`&[u8]`](slice)), [`Cow`] out. A JSON string that carries no
//! escape is borrowed straight out of the caller's buffer; only a string with
//! an escape allocates. A 37 KiB checkpoint of a hundred resources therefore
//! allocates one vector of pointer pairs and nothing else.
//!
//! Never a guess: this reads the bytes it is handed and nothing else. It never
//! reads a file, the network, or a plugin.

#![forbid(unsafe_code)]

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::marker::PhantomData;

use serde::de::value::MapAccessDeserializer;
use serde::de::{DeserializeSeed, IgnoredAny, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::value::RawValue;

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

/// The projection of one resource's `inputs`: the requested top-level keys,
/// in document order, each still borrowed from the caller's buffer.
///
/// The value is left as a [`RawValue`] — a slice of the original JSON — so a
/// scan that never looks at an element pays nothing to carry it.
pub type Element<'a> = Vec<(Cow<'a, str>, &'a RawValue)>;

/// One resource a checkpoint records.
///
/// `element` is `Some` only for a resource whose `type` was named in the
/// [`ElementSpec`] the scan was given, and only when it carries `inputs`.
#[derive(Debug, Clone)]
pub struct Entry<'a> {
    pub id: Cow<'a, str>,
    pub urn: Cow<'a, str>,
    pub element: Option<Element<'a>>,
}

// `RawValue` is a `str` newtype with no `PartialEq`, so equality is spelled
// out over the raw text. Two elements are equal when they project the same
// keys, in the same order, over the same bytes — which is what a caller
// comparing two checkpoints is asking.
impl PartialEq for Entry<'_> {
    fn eq(&self, other: &Self) -> bool {
        if self.id != other.id || self.urn != other.urn {
            return false;
        }
        match (&self.element, &other.element) {
            (None, None) => true,
            (Some(a), Some(b)) => {
                a.len() == b.len()
                    && a.iter()
                        .zip(b.iter())
                        .all(|((ka, va), (kb, vb))| ka == kb && va.get() == vb.get())
            }
            _ => false,
        }
    }
}

impl Eq for Entry<'_> {}

/// What a checkpoint records, in document order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Index<'a> {
    pub shape: Shape,
    pub entries: Vec<Entry<'a>>,
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

/// Which `inputs` keys identify an element, per resource type.
///
/// A caller that has to tell two members of one parent array apart names the
/// types it cares about and, for each, the top-level input keys that make up
/// the element. Nothing else is ever projected: an unnamed type keeps its
/// `inputs` unread, and an unnamed key inside a named type is skipped without
/// being captured. This module holds no table of its own — which keys mean
/// what belongs to the caller that knows the provider.
///
/// Like [`IdFilter`], the spec owns its strings deliberately: it is built once
/// per scan and read once per resource, and a lookup then allocates nothing.
#[derive(Debug, Clone, Default)]
pub struct ElementSpec {
    by_type: HashMap<String, HashSet<String>>,
}

impl ElementSpec {
    pub fn new<I, T, K>(types: I) -> Self
    where
        I: IntoIterator<Item = (T, K)>,
        T: Into<String>,
        K: IntoIterator,
        K::Item: Into<String>,
    {
        let by_type = types
            .into_iter()
            .map(|(kind, keys)| (kind.into(), keys.into_iter().map(Into::into).collect()))
            .collect();
        Self { by_type }
    }

    /// The keys to project for `kind`, or `None` if that type was not named.
    pub fn keys_for(&self, kind: &str) -> Option<&HashSet<String>> {
        self.by_type.get(kind)
    }
}

/// Read one checkpoint document.
///
/// The returned [`Index`] borrows from `bytes`. Every failure is named; there
/// is no input that yields a silently empty result. Every [`Entry::element`]
/// is `None`; a caller that needs elements asks for them by name through
/// [`index_checkpoint_with_elements`].
pub fn index_checkpoint<'a>(
    bytes: &'a [u8],
    filter: Option<&IdFilter>,
) -> Result<Index<'a>, CheckpointError> {
    index_checkpoint_with_elements(bytes, filter, None)
}

/// Read one checkpoint document, projecting an element out of the resources
/// whose type [`ElementSpec`] names.
///
/// `elements` of `None` is [`index_checkpoint`] exactly: no resource has its
/// `inputs` looked at, and every entry's element is `None`.
pub fn index_checkpoint_with_elements<'a>(
    bytes: &'a [u8],
    filter: Option<&IdFilter>,
    elements: Option<&ElementSpec>,
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
            // The projection runs after the filter, so a scan that keeps ten
            // of a thousand resources parses ten sets of inputs.
            let element = match (elements, resource.kind.as_deref(), resource.inputs) {
                (Some(spec), Some(kind), Some(inputs)) => match spec.keys_for(kind) {
                    Some(wanted) => Some(project(inputs, wanted)?),
                    None => None,
                },
                _ => None,
            };
            entries.push(Entry {
                id,
                urn: resource.urn,
                element,
            });
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
    index_checkpoints_with_elements(docs, filter, parallel, None)
}

/// Read many documents, projecting elements as [`index_checkpoint_with_elements`] does.
///
/// The spec is shared by reference across the pool and never written to, so
/// the batch keeps the shape it had: no per-document state, nothing to lock.
pub fn index_checkpoints_with_elements<'a>(
    docs: &[&'a [u8]],
    filter: Option<&IdFilter>,
    parallel: usize,
    elements: Option<&ElementSpec>,
) -> Vec<Result<Index<'a>, CheckpointError>> {
    if parallel <= 1 || docs.len() <= 1 {
        return sequential(docs, filter, elements);
    }
    let Ok(pool) = rayon::ThreadPoolBuilder::new()
        .num_threads(parallel.min(docs.len()))
        .build()
    else {
        return sequential(docs, filter, elements);
    };
    pool.install(|| {
        use rayon::prelude::*;
        docs.par_iter()
            .map(|doc| index_checkpoint_with_elements(doc, filter, elements))
            .collect()
    })
}

fn sequential<'a>(
    docs: &[&'a [u8]],
    filter: Option<&IdFilter>,
    elements: Option<&ElementSpec>,
) -> Vec<Result<Index<'a>, CheckpointError>> {
    docs.iter()
        .map(|doc| index_checkpoint_with_elements(doc, filter, elements))
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
    // The provider type token, which is what an `ElementSpec` is keyed on.
    #[serde(rename = "type", borrow, default, deserialize_with = "borrowed_option")]
    kind: Option<Cow<'a, str>>,
    // A slice of the caller's buffer, captured for every resource because
    // JSON puts no order on `type` and `inputs` and the decision to project
    // can only be taken once both are in hand. Capturing is not parsing: no
    // value inside is built, and nothing is allocated.
    #[serde(borrow, default)]
    inputs: Option<&'a RawValue>,
}

/// Project the requested top-level keys of one `inputs` object.
///
/// A `HashMap<Cow<str>, &RawValue>` would have been shorter and wrong: serde's
/// borrow special-case does not reach map keys, so every key of every
/// projected resource would be copied. The visitor below reads each key
/// through the same borrowed newtype the ids use, and takes a value only for a
/// key that was asked for — an unwanted key is stepped over, never captured.
fn project<'a>(
    inputs: &'a RawValue,
    wanted: &HashSet<String>,
) -> Result<Element<'a>, CheckpointError> {
    let mut de = serde_json::Deserializer::from_str(inputs.get());
    let element = Projection { wanted }.deserialize(&mut de)?;
    de.end()?;
    Ok(element)
}

struct Projection<'w> {
    wanted: &'w HashSet<String>,
}

impl<'de> DeserializeSeed<'de> for Projection<'_> {
    type Value = Element<'de>;

    fn deserialize<D: Deserializer<'de>>(self, de: D) -> Result<Self::Value, D::Error> {
        de.deserialize_map(self)
    }
}

impl<'de> Visitor<'de> for Projection<'_> {
    type Value = Element<'de>;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON object of resource inputs")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut element = Vec::new();
        while let Some(Borrowed(key)) = map.next_key::<Borrowed<'de>>()? {
            if self.wanted.contains(key.as_ref()) {
                element.push((key, map.next_value::<&'de RawValue>()?));
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(element)
    }
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
    Ok(Option::<Borrowed<'a>>::deserialize(de)?.map(|b| b.0))
}

/// The newtype that earns the borrow. Read `borrowed_option` for why it
/// exists; `Projection` reads its keys through the same one.
#[derive(Deserialize)]
struct Borrowed<'a>(#[serde(borrow)] Cow<'a, str>);

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

    /// An entry with no element, which is every entry of a scan that asked
    /// for none.
    fn plain<'a>(id: &'a str, urn: &'a str) -> Entry<'a> {
        Entry {
            id: Cow::from(id),
            urn: Cow::from(urn),
            element: None,
        }
    }

    /// The projected keys and their raw text, for comparison in a test.
    fn projected<'a>(entry: &Entry<'a>) -> Option<Vec<(&'a str, &'a str)>> {
        entry.element.as_ref().map(|pairs| {
            pairs
                .iter()
                .map(|(k, v)| match k {
                    Cow::Borrowed(k) => (*k, v.get()),
                    Cow::Owned(_) => unreachable!("an unescaped key must be borrowed"),
                })
                .collect()
        })
    }

    const ACCESS_TYPE: &str = "gcp:bigquery/datasetAccess:DatasetAccess";
    const ACCESS_URN: &str = "urn:pulumi:dev::app::gcp:bigquery/datasetAccess:DatasetAccess::a";
    const ACCESS_ID: &str = "projects/p/datasets/d";

    /// One `access[]` entry of a dataset: the id is the dataset's path, so
    /// only the inputs tell two of them apart.
    fn access_resource(name: &str, role: &str, member: &str) -> String {
        format!(
            concat!(
                r#"{{"urn":"{urn}{name}","id":"{id}","type":"{kind}","#,
                r#""inputs":{{"__defaults":[],"datasetId":"d","project":"p","#,
                r#""role":"{role}","userByEmail":"{member}"}},"#,
                r#""outputs":{{"role":"{role}"}}}}"#,
            ),
            urn = ACCESS_URN,
            name = name,
            id = ACCESS_ID,
            kind = ACCESS_TYPE,
            role = role,
            member = member,
        )
    }

    fn access_spec() -> ElementSpec {
        ElementSpec::new([(ACCESS_TYPE, ["role", "userByEmail", "view"])])
    }

    #[test]
    fn disk_shape_yields_id_urn_pairs() {
        let doc = disk(&one_resource());
        let idx = index(&doc);
        assert_eq!(idx.shape, Shape::Resources);
        assert_eq!(idx.entries, vec![plain(ID, URN)]);
    }

    #[test]
    fn export_shape_yields_id_urn_pairs() {
        let doc = format!(
            r#"{{"version":3,"deployment":{{"resources":[{}]}}}}"#,
            one_resource()
        );
        let idx = index(&doc);
        assert_eq!(idx.shape, Shape::Resources);
        assert_eq!(idx.entries, vec![plain(ID, URN)]);
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
        let [entry] = idx.entries.as_slice() else {
            unreachable!("one entry")
        };
        assert!(matches!(entry.id, Cow::Borrowed(_)), "id was copied");
        assert!(matches!(entry.urn, Cow::Borrowed(_)), "urn was copied");
    }

    #[test]
    fn escaped_strings_are_owned_and_unescaped() {
        let doc = r#"{"version":3,"checkpoint":{"latest":{"resources":[
            {"urn":"urn:pulumi:dev::app::gcp:workflows/workflow:Workflow::xé",
             "id":"a\/b"}]}}}"#;
        let idx = index(doc);
        let [entry] = idx.entries.as_slice() else {
            unreachable!("one entry")
        };
        assert_eq!(entry.id.as_ref(), "a/b");
        assert!(
            matches!(entry.id, Cow::Owned(_)),
            "an escape must be decoded"
        );
        assert!(entry.urn.as_ref().ends_with("xé"));
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

    // ---------------------------------------------------------------
    // elements — telling two members of one parent array apart
    // ---------------------------------------------------------------

    #[test]
    fn element_is_returned_for_a_requested_type() {
        let doc = disk(&access_resource("", "READER", "probe@example.com"));
        let spec = access_spec();
        let Ok(idx) = index_checkpoint_with_elements(doc.as_bytes(), None, Some(&spec)) else {
            unreachable!("fixture must read")
        };
        let [entry] = idx.entries.as_slice() else {
            unreachable!("one entry")
        };
        assert_eq!(entry.id.as_ref(), ACCESS_ID);
        assert_eq!(
            projected(entry),
            Some(vec![
                ("role", r#""READER""#),
                ("userByEmail", r#""probe@example.com""#),
            ]),
            "the element is the requested keys, in document order"
        );
    }

    #[test]
    fn two_entries_on_one_id_are_told_apart_by_their_elements() {
        let doc = disk(&format!(
            "{},{}",
            access_resource("a", "READER", "one@example.com"),
            access_resource("b", "WRITER", "two@example.com"),
        ));
        let spec = access_spec();
        let Ok(idx) = index_checkpoint_with_elements(doc.as_bytes(), None, Some(&spec)) else {
            unreachable!("fixture must read")
        };
        assert_eq!(idx.entries.len(), 2);
        assert_eq!(idx.entries[0].id, idx.entries[1].id, "one parent, two rows");
        assert_ne!(
            idx.entries[0], idx.entries[1],
            "two different elements must not compare equal"
        );
    }

    #[test]
    fn unrequested_types_keep_inputs_unread() {
        // The fixture's type is a Workflow; the spec names only the access
        // type, so its `inputs` are stepped over exactly as in 0.5.27.
        let doc = disk(&format!(
            concat!(
                r#"{{"urn":"{urn}","id":"{id}","type":"gcp:workflows/workflow:Workflow","#,
                r#""inputs":{{"role":"READER"}}}}"#,
            ),
            urn = URN,
            id = ID,
        ));
        let spec = access_spec();
        let Ok(idx) = index_checkpoint_with_elements(doc.as_bytes(), None, Some(&spec)) else {
            unreachable!("fixture must read")
        };
        let [entry] = idx.entries.as_slice() else {
            unreachable!("one entry")
        };
        assert_eq!(projected(entry), None);
    }

    #[test]
    fn a_scan_that_asks_for_nothing_projects_nothing() {
        let doc = disk(&access_resource("", "READER", "probe@example.com"));
        let asked = index(&doc);
        let [entry] = asked.entries.as_slice() else {
            unreachable!("one entry")
        };
        assert_eq!(projected(entry), None, "`None` is the 0.5.27 scan exactly");
        let Ok(explicit) = index_checkpoint_with_elements(doc.as_bytes(), None, None) else {
            unreachable!("fixture must read")
        };
        assert_eq!(explicit, asked);
    }

    #[test]
    fn only_projected_keys_come_back() {
        let doc = disk(&access_resource("", "READER", "probe@example.com"));
        let spec = ElementSpec::new([(ACCESS_TYPE, ["role"])]);
        let Ok(idx) = index_checkpoint_with_elements(doc.as_bytes(), None, Some(&spec)) else {
            unreachable!("fixture must read")
        };
        let [entry] = idx.entries.as_slice() else {
            unreachable!("one entry")
        };
        assert_eq!(
            projected(entry),
            Some(vec![("role", r#""READER""#)]),
            "`__defaults`, `datasetId`, `project` and `userByEmail` were not asked for"
        );
    }

    #[test]
    fn an_unknown_type_projects_nothing_at_all() {
        let doc = disk(&access_resource("", "READER", "probe@example.com"));
        let spec = ElementSpec::new([("gcp:storage/bucket:Bucket", ["name"])]);
        assert!(spec.keys_for(ACCESS_TYPE).is_none());
        let Ok(idx) = index_checkpoint_with_elements(doc.as_bytes(), None, Some(&spec)) else {
            unreachable!("fixture must read")
        };
        assert_eq!(idx.entries.len(), 1);
        assert_eq!(projected(&idx.entries[0]), None);
    }

    #[test]
    fn missing_inputs_is_none() {
        for resource in [
            format!(r#"{{"urn":"{ACCESS_URN}","id":"{ACCESS_ID}","type":"{ACCESS_TYPE}"}}"#),
            format!(
                r#"{{"urn":"{ACCESS_URN}","id":"{ACCESS_ID}","type":"{ACCESS_TYPE}","inputs":null}}"#
            ),
        ] {
            let doc = disk(&resource);
            let spec = access_spec();
            let Ok(idx) = index_checkpoint_with_elements(doc.as_bytes(), None, Some(&spec)) else {
                unreachable!("fixture must read")
            };
            assert_eq!(projected(&idx.entries[0]), None);
        }
    }

    #[test]
    fn an_empty_element_is_not_a_missing_one() {
        // The type was asked for and its inputs read; none of the requested
        // keys were there. That is an element with nothing in it, and it is a
        // different answer from "this resource was never looked at".
        let doc = disk(&format!(
            r#"{{"urn":"{ACCESS_URN}","id":"{ACCESS_ID}","type":"{ACCESS_TYPE}","inputs":{{"project":"p"}}}}"#
        ));
        let spec = access_spec();
        let Ok(idx) = index_checkpoint_with_elements(doc.as_bytes(), None, Some(&spec)) else {
            unreachable!("fixture must read")
        };
        assert_eq!(projected(&idx.entries[0]), Some(vec![]));
    }

    #[test]
    fn an_element_borrows_and_does_not_copy() {
        // The same pointer-range argument `plain_strings_are_borrowed` makes,
        // taken all the way through: the projected value is a slice of the
        // document that was handed in, not a copy of it.
        let doc = disk(&access_resource("", "READER", "probe@example.com"));
        let spec = access_spec();
        let Ok(idx) = index_checkpoint_with_elements(doc.as_bytes(), None, Some(&spec)) else {
            unreachable!("fixture must read")
        };
        let Some(element) = idx.entries[0].element.as_ref() else {
            unreachable!("an element was requested")
        };
        let start = doc.as_ptr() as usize;
        let end = start + doc.len();
        for (key, value) in element {
            assert!(matches!(key, Cow::Borrowed(_)), "a key was copied");
            let at = value.get().as_ptr() as usize;
            assert!(
                (start..end).contains(&at),
                "the value is not a slice of the document"
            );
        }
    }

    #[test]
    fn inputs_that_are_not_an_object_are_an_error() {
        // Only for a type that was asked for: an element that cannot be
        // projected is a refusal, not a silent `None`.
        let doc = disk(&format!(
            r#"{{"urn":"{ACCESS_URN}","id":"{ACCESS_ID}","type":"{ACCESS_TYPE}","inputs":["role"]}}"#
        ));
        let spec = access_spec();
        assert!(index_checkpoint_with_elements(doc.as_bytes(), None, Some(&spec)).is_err());
        let Ok(idx) = index_checkpoint(doc.as_bytes(), None) else {
            unreachable!("a scan that asks for no element never looks")
        };
        assert_eq!(idx.entries.len(), 1);
    }

    #[test]
    fn the_filter_runs_before_the_projection() {
        let doc = disk(&format!(
            "{},{}",
            access_resource("a", "READER", "one@example.com"),
            one_resource(),
        ));
        let filter = IdFilter::new([ID]);
        let spec = access_spec();
        let Ok(idx) = index_checkpoint_with_elements(doc.as_bytes(), Some(&filter), Some(&spec))
        else {
            unreachable!("fixture must read")
        };
        assert_eq!(idx.entries, vec![plain(ID, URN)]);
    }

    #[test]
    fn batch_equals_sequential_with_elements() {
        let doc = disk(&access_resource("", "READER", "probe@example.com"));
        let spec = access_spec();
        let docs: Vec<&[u8]> = vec![doc.as_bytes(), b"{", doc.as_bytes(), doc.as_bytes()];
        let seq = index_checkpoints_with_elements(&docs, None, 1, Some(&spec));
        let par = index_checkpoints_with_elements(&docs, None, 4, Some(&spec));
        assert_eq!(comparable(seq), comparable(par));
        let Ok(one) = index_checkpoint_with_elements(doc.as_bytes(), None, Some(&spec)) else {
            unreachable!("fixture must read")
        };
        let par = index_checkpoints_with_elements(&docs, None, 4, Some(&spec));
        assert_eq!(comparable(par)[0], Ok(one));
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

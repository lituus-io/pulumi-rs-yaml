// Author: spicyzhug@gmail.com
// Copyright: lituus-io, all rights reserved

//! Static IAM graph: who holds which role on what, from the program alone.
//!
//! [`export_iam_graph`] is a projection of the infrastructure graph's own
//! walk ([`crate::resource_graph`]): the same scopes, the same URNs, the same
//! literal resolution -- including host-supplied plaintext config -- so an
//! IAM edge joins the infra graph's nodes by construction.
//!
//! # What counts as a binding
//!
//! Any resource whose type's name ends `IAMMember`/`IamMember` (one `member`)
//! or `IAMBinding`/`IamBinding` (a `members` list), in any package and
//! module -- `gcp:projects:IAMMember`, `gcp:bigquery:DatasetIamMember`,
//! `gcp:storage:BucketIAMBinding`, ... -- plus `gcp:bigquery:DatasetAccess`,
//! whose member is spelled by `userByEmail`, `groupByEmail`, `domain`,
//! `iamMember` or `specialGroup`. `*IAMPolicy` carries a policy document,
//! not members, and is not read.
//!
//! # Rows
//!
//! - `principal` nodes, id `principal::{member}`: `user:`, `group:`,
//!   `serviceAccount:`, `domain:`, `principal:`/`principalSet:`, ... The
//!   email of a user, group or service account is lowercased (IAM compares
//!   them case-insensitively), so two stacks granting the same account mint
//!   one id.
//! - `role` nodes, id `role::{role}`.
//! - `has_role` edges from the principal to each resource the grant is on:
//!   the in-program resources the binding's other properties reference, or,
//!   when it references none (a grant on a literal project or dataset), the
//!   binding resource itself, whose `literal_properties` in the infra graph
//!   name the target.
//! - A member that is a resource OUTPUT (`serviceAccount:${sa.email}`,
//!   `${sa.member}`) becomes a `has_role` edge FROM that resource's URN.
//!
//! Every member is accounted for in `summary`: `member_resolved` (literal or
//! config), `member_from_resource`, `member_unresolved` (a warning names
//! each one). Nothing is guessed.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde::Serialize;

use crate::ast::expr::Expr;
use crate::ast::template::{PropertyEntry, TemplateDecl};
use crate::diag::Diagnostics;
use crate::literal_resolve::{resolve_literal, Vars};
use crate::resource_graph::{export_with_iam, GraphExportOptions};

/// IAM export schema version (independent of the infra graph's).
pub const SCHEMA_VERSION: u32 = 1;

/// How a binding resource names its members.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BindingShape {
    /// One `member`.
    Member,
    /// A `members` list.
    Members,
    /// `gcp:bigquery:DatasetAccess`: one of several typed member fields.
    DatasetAccess,
}

/// The binding shape of a canonical type token, if it is a binding at all.
pub(crate) fn binding_shape(token: &str) -> Option<BindingShape> {
    let name = token.rsplit(':').next()?;
    let lower_ends = |suffix: &str| {
        name.len() >= suffix.len()
            && name.is_char_boundary(name.len() - suffix.len())
            && name[name.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
    };
    if lower_ends("iammember") {
        Some(BindingShape::Member)
    } else if lower_ends("iambinding") {
        Some(BindingShape::Members)
    } else if name == "DatasetAccess" && token.starts_with("gcp:bigquery") {
        Some(BindingShape::DatasetAccess)
    } else {
        None
    }
}

/// `DatasetAccess` fields that each name one member, with the prefix that
/// turns the field's value into an IAM member string.
const DATASET_ACCESS_MEMBERS: &[(&str, &str)] = &[
    ("userByEmail", "user:"),
    ("groupByEmail", "group:"),
    ("domain", "domain:"),
    ("iamMember", ""),
    ("specialGroup", "specialGroup:"),
];

/// Properties that are never the grant's target.
fn is_member_or_role(shape: BindingShape, key: &str) -> bool {
    matches!(key, "role" | "member" | "members" | "condition")
        || (shape == BindingShape::DatasetAccess
            && DATASET_ACCESS_MEMBERS.iter().any(|(k, _)| *k == key))
}

/// One member, as the walk found it.
#[derive(Debug)]
enum MemberValue<'src> {
    Literal {
        member: Cow<'src, str>,
        from_config: bool,
    },
    Resource {
        ids: Vec<String>,
        principal_type: Option<&'static str>,
    },
    Unresolved,
}

#[derive(Debug)]
struct MemberRecord<'src> {
    path: Cow<'src, str>,
    value: MemberValue<'src>,
}

#[derive(Debug)]
enum RoleValue<'src> {
    Literal(Cow<'src, str>),
    Resource(String),
    Unresolved,
}

#[derive(Debug)]
struct BindingRecord<'src> {
    id: String,
    role: RoleValue<'src>,
    targets: Vec<String>,
    members: Vec<MemberRecord<'src>>,
}

/// What the infra walk hands the IAM projection.
#[derive(Debug, Default)]
pub(crate) struct IamCollector<'src> {
    bindings: Vec<BindingRecord<'src>>,
}

impl IamCollector<'_> {
    /// A binding whose properties are one expression: nothing in it can be
    /// read, so it counts as one unresolved member.
    pub(crate) fn unreadable(&mut self, id: &str) {
        self.bindings.push(BindingRecord {
            id: id.to_owned(),
            role: RoleValue::Unresolved,
            targets: Vec::new(),
            members: vec![MemberRecord {
                path: Cow::Borrowed("properties"),
                value: MemberValue::Unresolved,
            }],
        });
    }
}

/// Reads one binding resource. `resource_ids` appends the ids of the
/// in-scope resources an expression references.
pub(crate) fn record_binding<'src, F>(
    iam: &mut IamCollector<'src>,
    id: &str,
    shape: BindingShape,
    props: &'src [PropertyEntry<'src>],
    vars: &Vars<'src, '_>,
    resource_ids: &mut F,
) where
    F: FnMut(&'src Expr<'src>, &mut Vec<String>),
{
    let mut memo = HashMap::new();
    let mut visiting = HashSet::new();
    let mut literal = |expr: &'src Expr<'src>| -> Option<(Cow<'src, str>, bool)> {
        let before = vars.config_reads();
        let lit = resolve_literal(expr, vars, &mut memo, &mut visiting)?;
        Some((lit, vars.config_reads() != before))
    };

    let mut role = RoleValue::Unresolved;
    let mut targets = Vec::new();
    let mut members = Vec::new();
    for prop in props {
        let key = prop.key.as_ref();
        if key == "role" {
            role = match literal(&prop.value) {
                Some((r, _)) => RoleValue::Literal(r),
                None => {
                    let mut ids = Vec::new();
                    resource_ids(&prop.value, &mut ids);
                    ids.into_iter()
                        .next()
                        .map_or(RoleValue::Unresolved, RoleValue::Resource)
                }
            };
            continue;
        }
        if !is_member_or_role(shape, key) {
            resource_ids(&prop.value, &mut targets);
            continue;
        }
        let prefix = match shape {
            BindingShape::Member if key == "member" => "",
            BindingShape::Members if key == "members" => "",
            BindingShape::DatasetAccess => {
                match DATASET_ACCESS_MEMBERS.iter().find(|(k, _)| *k == key) {
                    Some((_, p)) => *p,
                    None => continue,
                }
            }
            _ => continue,
        };
        // A literal list is read element by element; anything else is one
        // member expression (a list behind a variable or a resource output
        // counts once, by what it references).
        let elements: Vec<(Cow<'src, str>, &'src Expr<'src>)> = match (&prop.value, shape) {
            (Expr::List(_, items), BindingShape::Members) => items
                .iter()
                .enumerate()
                .map(|(i, e)| (Cow::Owned(format!("{}.{}", key, i)), e))
                .collect(),
            (other, _) => vec![(Cow::Borrowed(key), other)],
        };
        for (path, expr) in elements {
            let value = match literal(expr) {
                Some((m, from_config)) => {
                    let member = if prefix.is_empty() {
                        m
                    } else {
                        Cow::Owned(format!("{}{}", prefix, m))
                    };
                    MemberValue::Literal {
                        member,
                        from_config,
                    }
                }
                None => {
                    let mut ids = Vec::new();
                    resource_ids(expr, &mut ids);
                    if ids.is_empty() {
                        MemberValue::Unresolved
                    } else {
                        let principal_type = if prefix.is_empty() {
                            leading_principal_type(expr)
                        } else {
                            principal_type(prefix)
                        };
                        MemberValue::Resource {
                            ids,
                            principal_type,
                        }
                    }
                }
            };
            members.push(MemberRecord { path, value });
        }
    }
    iam.bindings.push(BindingRecord {
        id: id.to_owned(),
        role,
        targets,
        members,
    });
}

/// The principal type an interpolation's leading literal text names --
/// `serviceAccount:` in `serviceAccount:${sa.email}`.
fn leading_principal_type(expr: &Expr<'_>) -> Option<&'static str> {
    match expr {
        Expr::Interpolate(_, parts) => parts.first().and_then(|p| principal_type(&p.text)),
        _ => None,
    }
}

/// The principal type of a member string (or of its prefix alone).
pub fn principal_type(member: &str) -> Option<&'static str> {
    const KINDS: &[&str] = &[
        "user",
        "group",
        "serviceAccount",
        "domain",
        "principalSet",
        "principal",
        "deleted",
        "projectOwner",
        "projectEditor",
        "projectViewer",
        "specialGroup",
    ];
    if member == "allUsers" || member == "allAuthenticatedUsers" {
        return Some("special");
    }
    let (kind, _) = member.split_once(':')?;
    KINDS.iter().copied().find(|k| *k == kind)
}

/// The canonical member string: an email-addressed member's address
/// lowercased, everything else as written.
fn canonical_member(member: &str) -> Cow<'_, str> {
    match member.split_once(':') {
        Some((kind @ ("user" | "group" | "serviceAccount" | "domain"), rest))
            if rest.bytes().any(|b| b.is_ascii_uppercase()) =>
        {
            Cow::Owned(format!("{}:{}", kind, rest.to_ascii_lowercase()))
        }
        _ => Cow::Borrowed(member),
    }
}

/// Node kinds of the IAM graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IamNodeKind {
    Principal,
    Role,
}

/// Where a member came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MemberSource {
    /// Written in the program (literals, variables, `str` invokes).
    Literal,
    /// Read from host-supplied plaintext config.
    Config,
    /// A resource output; the edge starts at that resource.
    Resource,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct IamNode<'src> {
    pub id: String,
    pub kind: IamNodeKind,
    /// The member or role string.
    pub name: Cow<'src, str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub principal_type: Option<&'static str>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct IamEdge<'src> {
    /// A `principal::` id, or the URN of the resource whose output the
    /// member is.
    pub source_id: Cow<'src, str>,
    /// The URN the role is held on.
    pub target_id: Cow<'src, str>,
    /// Always `has_role`.
    pub relationship: &'static str,
    /// `role::{role}`, the URN of a role resource, or absent when the role
    /// is not statically known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role_id: Option<Cow<'src, str>>,
    /// The URN of the binding resource that grants it.
    pub binding_id: Cow<'src, str>,
    /// The binding property the member was read from.
    pub property_path: Cow<'src, str>,
    pub member_source: MemberSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub principal_type: Option<&'static str>,
}

/// Every member accounted for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct IamSummary {
    pub bindings: usize,
    pub members: usize,
    /// Literal or config.
    pub member_resolved: usize,
    /// Of `member_resolved`, those read from config.
    pub member_from_config: usize,
    pub member_from_resource: usize,
    pub member_unresolved: usize,
}

/// The IAM graph of one stack.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct IamGraph<'src> {
    pub schema_version: u32,
    pub organization: Cow<'src, str>,
    pub project: Cow<'src, str>,
    pub stack: Cow<'src, str>,
    pub nodes: Vec<IamNode<'src>>,
    pub edges: Vec<IamEdge<'src>>,
    pub summary: IamSummary,
}

impl IamGraph<'_> {
    /// Serializes the whole graph as pretty JSON with a trailing newline.
    pub fn to_json(&self) -> Result<String, String> {
        serde_json::to_string_pretty(self)
            .map(|mut s| {
                s.push('\n');
                s
            })
            .map_err(|e| format!("failed to serialize IAM graph: {}", e))
    }
}

/// Exports the IAM graph of a template. Pure, deterministic and linear in
/// the program: the infra walk with one extra pass per binding resource.
/// Takes the same options as the infra export -- `config` above all, which
/// is what turns `${member}`-from-config bindings into principals.
pub fn export_iam_graph<'src>(
    template: &'src TemplateDecl<'src>,
    opts: &GraphExportOptions<'src>,
) -> (IamGraph<'src>, Diagnostics) {
    let (infra, mut diags, collector) = export_with_iam(template, opts, true);
    let mut graph = IamGraph {
        schema_version: SCHEMA_VERSION,
        organization: infra.organization,
        project: infra.project,
        stack: infra.stack,
        nodes: Vec::new(),
        edges: Vec::new(),
        summary: IamSummary::default(),
    };
    let Some(collector) = collector else {
        return (graph, diags);
    };

    let mut nodes: BTreeMap<String, IamNode<'src>> = BTreeMap::new();
    let mut edges: BTreeSet<EdgeKey<'src>> = BTreeSet::new();
    let summary = &mut graph.summary;
    for binding in collector.bindings {
        summary.bindings += 1;
        let role_id: Option<Cow<'src, str>> = match binding.role {
            RoleValue::Literal(role) => {
                let id = format!("role::{}", role);
                nodes.entry(id.clone()).or_insert_with(|| IamNode {
                    id: id.clone(),
                    kind: IamNodeKind::Role,
                    name: role,
                    principal_type: None,
                });
                Some(Cow::Owned(id))
            }
            RoleValue::Resource(urn) => Some(Cow::Owned(urn)),
            RoleValue::Unresolved => None,
        };
        let targets: Vec<String> = if binding.targets.is_empty() {
            vec![binding.id.clone()]
        } else {
            binding.targets
        };
        for member in binding.members {
            summary.members += 1;
            let (sources, source, principal_type): (Vec<String>, MemberSource, _) = match member
                .value
            {
                MemberValue::Literal {
                    member: m,
                    from_config,
                } => {
                    summary.member_resolved += 1;
                    if from_config {
                        summary.member_from_config += 1;
                    }
                    let canonical = canonical_member(&m).into_owned();
                    let kind = principal_type(&canonical);
                    let id = format!("principal::{}", canonical);
                    nodes.entry(id.clone()).or_insert_with(|| IamNode {
                        id: id.clone(),
                        kind: IamNodeKind::Principal,
                        name: Cow::Owned(canonical),
                        principal_type: kind,
                    });
                    let source = if from_config {
                        MemberSource::Config
                    } else {
                        MemberSource::Literal
                    };
                    (vec![id], source, kind)
                }
                MemberValue::Resource {
                    ids,
                    principal_type,
                } => {
                    summary.member_from_resource += 1;
                    (ids, MemberSource::Resource, principal_type)
                }
                MemberValue::Unresolved => {
                    summary.member_unresolved += 1;
                    diags.warning(
                            None,
                            format!(
                                "IAM member '{}' of '{}' is not statically resolvable",
                                member.path, binding.id
                            ),
                            "pass the stack's plaintext config, or reference the member's resource directly",
                        );
                    continue;
                }
            };
            for source_id in &sources {
                for target in &targets {
                    edges.insert(EdgeKey {
                        source_id: source_id.clone(),
                        target_id: target.clone(),
                        role_id: role_id.clone(),
                        binding_id: binding.id.clone(),
                        property_path: member.path.clone(),
                        member_source: source,
                        principal_type,
                    });
                }
            }
        }
    }
    graph.nodes = nodes.into_values().collect();
    graph.edges = edges
        .into_iter()
        .map(|k| IamEdge {
            source_id: Cow::Owned(k.source_id),
            target_id: Cow::Owned(k.target_id),
            relationship: "has_role",
            role_id: k.role_id,
            binding_id: Cow::Owned(k.binding_id),
            property_path: k.property_path,
            member_source: k.member_source,
            principal_type: k.principal_type,
        })
        .collect();
    (graph, diags)
}

/// An edge, ordered for deterministic output.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct EdgeKey<'src> {
    source_id: String,
    target_id: String,
    role_id: Option<Cow<'src, str>>,
    binding_id: String,
    property_path: Cow<'src, str>,
    member_source: MemberSource,
    principal_type: Option<&'static str>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::parse::parse_template;
    use crate::literal_resolve::HostConfig;

    fn export(yaml: &str, config: Option<&'static HostConfig>) -> (IamGraph<'static>, Diagnostics) {
        let (template, diags) = parse_template(yaml, None);
        assert!(!diags.has_errors(), "{diags}");
        let template: &'static TemplateDecl<'static> = Box::leak(Box::new(template));
        let opts = GraphExportOptions {
            organization: "org",
            project: "proj",
            stack: "dev",
            config,
            ..Default::default()
        };
        export_iam_graph(template, &opts)
    }

    fn host(pairs: &[(&str, &str)]) -> &'static HostConfig {
        Box::leak(Box::new(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        ))
    }

    #[test]
    fn shapes() {
        assert_eq!(
            binding_shape("gcp:projects/iAMMember:IAMMember"),
            Some(BindingShape::Member)
        );
        assert_eq!(
            binding_shape("gcp:bigquery/datasetIamMember:DatasetIamMember"),
            Some(BindingShape::Member)
        );
        assert_eq!(
            binding_shape("gcp:storage/bucketIAMBinding:BucketIAMBinding"),
            Some(BindingShape::Members)
        );
        assert_eq!(
            binding_shape("gcp:bigquery/datasetAccess:DatasetAccess"),
            Some(BindingShape::DatasetAccess)
        );
        assert_eq!(binding_shape("gcp:projects/iAMPolicy:IAMPolicy"), None);
        assert_eq!(binding_shape("gcp:storage/bucket:Bucket"), None);
        assert_eq!(binding_shape(""), None);
        // A multibyte name shorter than a suffix is not sliced mid-character.
        assert_eq!(binding_shape("x:y:żółwżółw"), None);
    }

    #[test]
    fn principal_kinds() {
        for (m, k) in [
            ("user:a@example.com", Some("user")),
            ("group:g@example.com", Some("group")),
            (
                "serviceAccount:s@p.iam.gserviceaccount.com",
                Some("serviceAccount"),
            ),
            ("domain:example.com", Some("domain")),
            ("principal://iam.googleapis.com/x", Some("principal")),
            ("principal:x", Some("principal")),
            ("principalSet:x", Some("principalSet")),
            ("allUsers", Some("special")),
            ("weird", None),
        ] {
            assert_eq!(principal_type(m), k, "{m}");
        }
    }

    const LITERAL: &str = "name: proj\nruntime: yaml\nresources:\n  ds:\n    type: gcp:bigquery:Dataset\n    properties:\n      datasetId: d\n  grant:\n    type: gcp:bigquery:DatasetIamMember\n    properties:\n      datasetId: ${ds.datasetId}\n      role: roles/bigquery.dataViewer\n      member: user:Alice@Example.com\n";

    #[test]
    fn a_literal_member_grants_on_the_referenced_resource() {
        let (g, diags) = export(LITERAL, None);
        assert!(!diags.has_errors());
        assert_eq!(g.summary.members, 1);
        assert_eq!(g.summary.member_resolved, 1);
        let e = &g.edges[0];
        assert_eq!(e.source_id, "principal::user:alice@example.com");
        assert_eq!(
            e.target_id,
            "urn:pulumi:dev::proj::gcp:bigquery/dataset:Dataset::ds"
        );
        assert_eq!(
            e.role_id.as_deref(),
            Some("role::roles/bigquery.dataViewer")
        );
        assert_eq!(e.member_source, MemberSource::Literal);
        assert!(g.nodes.iter().any(|n| n.kind == IamNodeKind::Role));
        assert!(g.nodes.iter().any(
            |n| n.id == "principal::user:alice@example.com" && n.principal_type == Some("user")
        ));
    }

    const FROM_CONFIG: &str = "name: proj\nruntime: yaml\nconfig:\n  reader:\n    type: string\n  writer:\n    type: string\n    secret: true\nresources:\n  a:\n    type: gcp:projects:IAMMember\n    properties:\n      project: p\n      role: roles/viewer\n      member: group:${reader}\n  b:\n    type: gcp:projects:IAMMember\n    properties:\n      project: p\n      role: roles/editor\n      member: ${writer}\n";

    #[test]
    fn config_resolves_only_with_host_config_and_never_a_secret() {
        let (without, _) = export(FROM_CONFIG, None);
        assert_eq!(without.summary.member_unresolved, 2);
        assert_eq!(without.summary.member_resolved, 0);

        let cfg = host(&[
            ("proj:reader", "readers@example.com"),
            ("proj:writer", "user:w@example.com"),
        ]);
        let (with, _) = export(FROM_CONFIG, Some(cfg));
        assert_eq!(with.summary.member_resolved, 1);
        assert_eq!(with.summary.member_from_config, 1);
        assert_eq!(with.summary.member_unresolved, 1, "a secret never resolves");
        let e = with
            .edges
            .iter()
            .find(|e| e.source_id == "principal::group:readers@example.com")
            .expect("config member");
        assert_eq!(e.member_source, MemberSource::Config);
        // A literal project is no resource here: the grant points at the
        // binding itself.
        assert_eq!(
            e.target_id,
            "urn:pulumi:dev::proj::gcp:projects/iAMMember:IAMMember::a"
        );
        let json = with.to_json().expect("json");
        assert!(!json.contains("w@example.com"), "{json}");
    }

    #[test]
    fn a_resource_output_member_is_an_edge_from_that_resource() {
        let yaml = "name: proj\nruntime: yaml\nresources:\n  sa:\n    type: gcp:serviceaccount:Account\n    properties:\n      accountId: runner\n  b:\n    type: gcp:storage:Bucket\n  g:\n    type: gcp:storage:BucketIAMBinding\n    properties:\n      bucket: ${b.name}\n      role: roles/storage.objectViewer\n      members:\n        - serviceAccount:${sa.email}\n        - user:x@example.com\n";
        let (g, _) = export(yaml, None);
        assert_eq!(g.summary.members, 2);
        assert_eq!(g.summary.member_from_resource, 1);
        assert_eq!(g.summary.member_resolved, 1);
        let e = g
            .edges
            .iter()
            .find(|e| e.member_source == MemberSource::Resource)
            .expect("resource edge");
        assert_eq!(
            e.source_id,
            "urn:pulumi:dev::proj::gcp:serviceaccount/account:Account::sa"
        );
        assert_eq!(
            e.target_id,
            "urn:pulumi:dev::proj::gcp:storage/bucket:Bucket::b"
        );
        assert_eq!(e.principal_type, Some("serviceAccount"));
        assert_eq!(e.property_path, "members.0");
    }

    #[test]
    fn dataset_access_fields_are_members() {
        let yaml = "name: proj\nruntime: yaml\nresources:\n  a:\n    type: gcp:bigquery:DatasetAccess\n    properties:\n      datasetId: d\n      role: READER\n      userByEmail: u@example.com\n";
        let (g, _) = export(yaml, None);
        assert_eq!(g.summary.member_resolved, 1);
        assert_eq!(g.edges[0].source_id, "principal::user:u@example.com");
    }

    #[test]
    fn deterministic() {
        let (a, _) = export(LITERAL, None);
        let (b, _) = export(LITERAL, None);
        assert_eq!(a.to_json().expect("a"), b.to_json().expect("b"));
    }
}

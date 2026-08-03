// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

//! Static resource dependency-graph export for external graph stores
//! (e.g. BigQuery Graph property graphs).
//!
//! [`export_resource_graph`] is a pure function of a parsed template plus
//! stack identity: it never touches state files, the network, or plugins.
//! Each stack exports independently; exports from many stacks union into
//! the same node/edge tables and join on the ID contract below.
//!
//! # ID contract (cross-stack join keys)
//!
//! - Resource-kind nodes use the engine URN format:
//!   `urn:pulumi:{stack}::{project}::{qualifiedType}::{name}` where
//!   `name` is the registration name (`name:` override or logical name)
//!   and `qualifiedType` nests the full parent-type chain with `$`
//!   (root stack type excluded). Component children nest under their
//!   instance's qualified type.
//! - The root stack node is
//!   `urn:pulumi:{stack}::{project}::pulumi:pulumi:Stack::{project}-{stack}`.
//! - Stack output nodes use
//!   `stackoutput::{organization}/{project}/{stack}::{output_key}`.
//! - `consumes_stack_output` edges target the same `stackoutput::` form,
//!   built from the consuming stack's `pulumi:pulumi:StackReference`
//!   declaration — identical by construction to the producer stack's own
//!   output-node id. No placeholder row is emitted for the external
//!   target; the producer's export owns that row.
//!
//! # Cross-stack identity linking
//!
//! Each resource-kind node carries `literal_properties`: the subset of
//! its properties statically resolvable to scalar literals (literals,
//! literal-only interpolations, and chains through variables that
//! collapse to literals). Dynamic values are omitted, never guessed.
//! Downstream stores derive identity edges (e.g. a BigQuery table in one
//! stack referencing a dataset declared in another by equal literal
//! `project` + `datasetId`) by joining these values across the unioned
//! node table; stringification is deterministic so equality joins are
//! exact.
//!
//! # Limitations (static analysis)
//!
//! - Values depending on config, resource outputs, invokes, or builtins
//!   are not literal and never appear in `literal_properties`.
//! - Component input binding is not projected through to children;
//!   outer→inner reachability is a two-hop traversal via the instance's
//!   `references` edges and its `contains` edges.
//! - A StackReference whose `name` is not a string literal produces only
//!   a local `references` edge (with a warning), no cross-stack edge.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde::Serialize;

use crate::ast::expr::Expr;
use crate::ast::property::{PropertyAccess, PropertyAccessor};
use crate::ast::template::{
    ComponentDecl, ResourceEntry, ResourceProperties, TemplateDecl, VariableEntry,
};
use crate::ast::visitor::{walk_expr, AccessCollector};
use crate::diag::Diagnostics;
use crate::eval::graph;
use crate::literal_resolve::{collect_literal_properties, resolve_literal};
use crate::packages;
use crate::schema::SchemaStore;

/// Current export schema version.
pub const SCHEMA_VERSION: u32 = 1;

/// Maximum nested-component expansion depth.
const MAX_COMPONENT_DEPTH: usize = 32;

/// Placeholder used when no organization is configured.
const ORG_PLACEHOLDER: &str = "organization";

/// Node kinds. Serialized snake_case; enum order is not part of the
/// contract (nodes sort by id).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    Stack,
    Resource,
    Component,
    ComponentChild,
    Provider,
    External,
    StackReference,
    Output,
}

/// Edge relationships. Serialized snake_case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    References,
    DependsOn,
    Parent,
    Provider,
    Contains,
    Exports,
    ConsumesStackOutput,
}

/// One graph node. `id` is the primary key across all stacks' exports.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GraphNode<'src> {
    pub id: Cow<'src, str>,
    pub kind: NodeKind,
    pub logical_name: Cow<'src, str>,
    pub name: Cow<'src, str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub type_token: Option<Cow<'src, str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub package: Option<Cow<'src, str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<Cow<'src, str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub component_id: Option<Cow<'src, str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_file: Option<&'src str>,
    /// Statically-resolved scalar literals, as sorted (dot-path, value)
    /// pairs. Serialized as an object.
    #[serde(serialize_with = "serialize_pairs_as_object")]
    pub literal_properties: Vec<(Cow<'src, str>, Cow<'src, str>)>,
    pub organization: Cow<'src, str>,
    pub project: Cow<'src, str>,
    pub stack: Cow<'src, str>,
}

/// One typed edge. Composite key: (source_id, target_id, relationship).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GraphEdge<'src> {
    pub source_id: Cow<'src, str>,
    pub target_id: Cow<'src, str>,
    pub relationship: EdgeKind,
    /// Property paths on the source that induced this edge; sorted and
    /// deduplicated. Empty for implicit edges (default provider,
    /// depends_on, parent, contains).
    pub property_paths: Vec<Cow<'src, str>>,
    pub organization: Cow<'src, str>,
    pub project: Cow<'src, str>,
    pub stack: Cow<'src, str>,
}

/// The exported graph for a single stack.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResourceGraph<'src> {
    pub schema_version: u32,
    pub organization: Cow<'src, str>,
    pub project: Cow<'src, str>,
    pub stack: Cow<'src, str>,
    pub nodes: Vec<GraphNode<'src>>,
    pub edges: Vec<GraphEdge<'src>>,
}

// Compile-time assertion: the export is shareable across threads.
const _: () = {
    fn _assert_send_sync<T: Send + Sync>() {}
    fn _check() {
        _assert_send_sync::<ResourceGraph<'static>>();
    }
};

impl ResourceGraph<'_> {
    /// Serializes the whole graph as pretty JSON with a trailing newline.
    pub fn to_json(&self) -> Result<String, String> {
        serde_json::to_string_pretty(self)
            .map(|mut s| {
                s.push('\n');
                s
            })
            .map_err(|e| format!("failed to serialize resource graph: {}", e))
    }
}

fn serialize_pairs_as_object<S: serde::Serializer>(
    pairs: &[(Cow<'_, str>, Cow<'_, str>)],
    ser: S,
) -> Result<S::Ok, S::Error> {
    use serde::ser::SerializeMap;
    let mut map = ser.serialize_map(Some(pairs.len()))?;
    for (k, v) in pairs {
        map.serialize_entry(k, v)?;
    }
    map.end()
}

/// Options for [`export_resource_graph`].
pub struct GraphExportOptions<'src> {
    /// Pulumi organization; empty uses a documented placeholder in
    /// cross-stack ids (with a warning).
    pub organization: &'src str,
    pub project: &'src str,
    pub stack: &'src str,
    /// Logical name → source filename (from multi-file merge).
    pub source_map: Option<&'src HashMap<String, String>>,
    /// Optional schema store for authoritative token resolution and
    /// remote-component classification.
    pub schema_store: Option<&'src SchemaStore>,
}

/// Exports the static dependency graph of a template.
///
/// Pure and deterministic: identical inputs produce byte-identical
/// output (nodes sorted by id; edges by source, target, relationship).
/// Returns an empty graph alongside the diagnostics when the template
/// fails DAG validation.
pub fn export_resource_graph<'src>(
    template: &'src TemplateDecl<'src>,
    opts: &GraphExportOptions<'src>,
) -> (ResourceGraph<'src>, Diagnostics) {
    let mut diags = Diagnostics::new();

    let (_, sort_diags) = graph::topological_sort_with_sources(template, opts.source_map);
    let failed = sort_diags.has_errors();
    diags.extend(sort_diags);
    if failed {
        return (empty_graph(opts), diags);
    }

    let mut ctx = Ctx::new(template, opts);
    if ctx.org_placeholder && (!template.outputs.is_empty() || template_has_stack_ref(template)) {
        diags.warning(
            None,
            "organization not set; cross-stack ids use placeholder 'organization'",
            "pass the Pulumi organization so stackoutput:: ids match across stacks",
        );
    }

    let mut nodes: Vec<GraphNode<'src>> = Vec::new();
    let mut edges: EdgeMap<'src> = BTreeMap::new();

    // Root stack node — the anchor for deployment/status enrichment.
    let stack_id = format!(
        "urn:pulumi:{}::{}::pulumi:pulumi:Stack::{}-{}",
        opts.stack, opts.project, opts.project, opts.stack
    );
    nodes.push(GraphNode {
        id: Cow::Owned(stack_id.clone()),
        kind: NodeKind::Stack,
        logical_name: Cow::Borrowed("stack"),
        name: Cow::Owned(format!("{}-{}", opts.project, opts.stack)),
        type_token: Some(Cow::Borrowed("pulumi:pulumi:Stack")),
        package: Some(Cow::Borrowed("pulumi")),
        parent_id: None,
        component_id: None,
        source_file: None,
        literal_properties: Vec::new(),
        organization: Cow::Borrowed(ctx.org),
        project: Cow::Borrowed(opts.project),
        stack: Cow::Borrowed(opts.stack),
    });

    // Top-level scope.
    let scope = Scope::build(
        &ctx,
        &template.resources,
        &template.variables,
        None,
        &mut diags,
    );

    // Stack contains its unparented top-level resources.
    for info in &scope.infos {
        if info.parent_logical.is_none() {
            add_edge(
                &mut edges,
                stack_id.clone(),
                info.id.clone(),
                EdgeKind::Contains,
                None,
            );
        }
    }

    let mut instantiation_stack: Vec<&'src str> = Vec::new();
    emit_scope(
        &mut ctx,
        &scope,
        &mut nodes,
        &mut edges,
        &mut instantiation_stack,
        &mut diags,
    );

    // Stack outputs.
    for output in &template.outputs {
        let out_id = format!(
            "stackoutput::{}/{}/{}::{}",
            ctx.org, opts.project, opts.stack, output.key
        );
        nodes.push(GraphNode {
            id: Cow::Owned(out_id.clone()),
            kind: NodeKind::Output,
            logical_name: Cow::Borrowed(output.key.as_ref()),
            name: Cow::Borrowed(output.key.as_ref()),
            type_token: None,
            package: None,
            parent_id: None,
            component_id: None,
            source_file: ctx.source_file(output.key.as_ref()),
            literal_properties: Vec::new(),
            organization: Cow::Borrowed(ctx.org),
            project: Cow::Borrowed(opts.project),
            stack: Cow::Borrowed(opts.stack),
        });
        let mut resolver = ScopeResolver::new(&scope);
        let targets = resolver.targets_of_expr(&output.value);
        for target in targets {
            match target {
                Target::Node(idx) => add_edge(
                    &mut edges,
                    out_id.clone(),
                    scope.infos[idx].id.clone(),
                    EdgeKind::Exports,
                    Some(Cow::Borrowed(output.key.as_ref())),
                ),
                Target::StackOutput { id } => add_edge(
                    &mut edges,
                    out_id.clone(),
                    id,
                    EdgeKind::ConsumesStackOutput,
                    Some(Cow::Borrowed(output.key.as_ref())),
                ),
            }
        }
    }

    let graph = finalize(opts, &ctx, nodes, edges, &mut diags);
    (graph, diags)
}

// ---------- internal machinery ----------

type EdgeMap<'src> = BTreeMap<(String, String, EdgeKind), BTreeSet<Cow<'src, str>>>;

fn add_edge<'src>(
    edges: &mut EdgeMap<'src>,
    source: String,
    target: String,
    relationship: EdgeKind,
    path: Option<Cow<'src, str>>,
) {
    let paths = edges.entry((source, target, relationship)).or_default();
    if let Some(p) = path {
        paths.insert(p);
    }
}

/// Template-wide context shared by every scope.
struct Ctx<'src, 'o> {
    opts: &'o GraphExportOptions<'src>,
    org: &'src str,
    org_placeholder: bool,
    /// Accepted instance tokens → component decl.
    component_tokens: HashMap<String, &'src ComponentDecl<'src>>,
    /// Component key → verbatim registered token `{pkg}:index:{key}`.
    component_canonical: HashMap<&'src str, String>,
}

impl<'src, 'o> Ctx<'src, 'o> {
    fn new(template: &'src TemplateDecl<'src>, opts: &'o GraphExportOptions<'src>) -> Self {
        let org_placeholder = opts.organization.is_empty();
        let org = if org_placeholder {
            ORG_PLACEHOLDER
        } else {
            opts.organization
        };
        let pkg = template.name.as_deref().unwrap_or("yaml-components");
        let mut component_tokens = HashMap::new();
        let mut component_canonical = HashMap::new();
        for comp in &template.components {
            let key = comp.key.as_ref();
            let canonical = format!("{}:index:{}", pkg, key);
            component_tokens.insert(canonical.clone(), comp);
            component_tokens.insert(format!("{}:{}", pkg, key), comp);
            component_tokens.insert(key.to_string(), comp);
            component_canonical.insert(key, canonical);
        }
        Self {
            opts,
            org,
            org_placeholder,
            component_tokens,
            component_canonical,
        }
    }

    fn source_file(&self, logical: &str) -> Option<&'src str> {
        self.opts
            .source_map
            .and_then(|m| m.get(logical))
            .map(String::as_str)
    }

    /// Canonical token with the evaluator's preference order.
    fn canonical_token(&self, raw: &'src str) -> Cow<'src, str> {
        if let Some(store) = self.opts.schema_store {
            if let Some(resolved) = store.resolve_resource_token(raw) {
                return Cow::Owned(resolved.into_owned());
            }
        }
        let canonical = packages::canonicalize_type_token(raw);
        if canonical == raw {
            Cow::Borrowed(raw)
        } else {
            Cow::Owned(canonical)
        }
    }
}

fn template_has_stack_ref(template: &TemplateDecl<'_>) -> bool {
    template
        .resources
        .iter()
        .any(|r| r.resource.type_.as_ref() == "pulumi:pulumi:StackReference")
}

/// Per-resource derived facts within one scope.
struct ResInfo<'src> {
    logical: &'src str,
    entry: &'src ResourceEntry<'src>,
    kind: NodeKind,
    token: Cow<'src, str>,
    reg_name: &'src str,
    display_logical: Cow<'src, str>,
    parent_logical: Option<&'src str>,
    /// Component decl when this is a local component instance.
    component_decl: Option<&'src ComponentDecl<'src>>,
    /// Normalized `org/project/stack` for StackReference nodes with a
    /// statically-known name.
    stack_fq: Option<String>,
    qualified_type: String,
    id: String,
    component_id: Option<String>,
    source_file: Option<&'src str>,
}

/// One namespace of resources/variables/config (the top level, or one
/// component instance's body).
struct Scope<'src> {
    infos: Vec<ResInfo<'src>>,
    by_name: HashMap<&'src str, usize>,
    variables: HashMap<&'src str, &'src Expr<'src>>,
    // Config/input roots need no set: they simply match neither
    // `by_name` nor `variables`, so no edge is produced for them.
}

/// Namespacing applied when a scope is a component instance body.
/// `'a` borrows from the instantiating scope's `ResInfo`; `'src` is the
/// template source lifetime (only `source_file` flows into child nodes).
struct InstancePrefix<'a, 'src> {
    qualified_type: &'a str,
    logical_prefix: &'a str,
    instance_id: &'a str,
    source_file: Option<&'src str>,
}

impl<'src> Scope<'src> {
    fn build(
        ctx: &Ctx<'src, '_>,
        resources: &'src [ResourceEntry<'src>],
        variables: &'src [VariableEntry<'src>],
        prefix: Option<&InstancePrefix<'_, 'src>>,
        diags: &mut Diagnostics,
    ) -> Self {
        let mut by_name = HashMap::new();
        let mut infos: Vec<ResInfo<'src>> = Vec::with_capacity(resources.len());

        // Pass 1: names, kinds, tokens.
        for entry in resources {
            let logical = entry.logical_name.as_ref();
            let raw = entry.resource.type_.as_ref();
            let (kind, token, component_decl) = classify(ctx, entry, raw);
            let reg_name = entry.resource.name.as_deref().unwrap_or(logical);
            let stack_fq = if kind == NodeKind::StackReference {
                stack_reference_fq(ctx, entry, variables, diags)
            } else {
                None
            };
            by_name.insert(logical, infos.len());
            infos.push(ResInfo {
                logical,
                entry,
                kind,
                token,
                reg_name,
                display_logical: Cow::Borrowed(logical),
                parent_logical: None,
                component_decl,
                stack_fq,
                qualified_type: String::new(),
                id: String::new(),
                component_id: None,
                source_file: None,
            });
        }

        // Pass 2: static parents (need the name table complete).
        for info in infos.iter_mut() {
            if let Some(expr) = info.entry.resource.options.parent.as_ref() {
                match single_name_root(expr).filter(|root| by_name.contains_key(root)) {
                    Some(root) => info.parent_logical = Some(root),
                    None => diags.warning(
                        None,
                        format!(
                            "resource '{}' has a dynamic parent; its exported URN may not match the engine",
                            info.logical
                        ),
                        "the qualified type omits the parent chain when the parent is not a static resource reference",
                    ),
                }
            }
        }

        // Pass 3: qualified types (memoized parent-chain walk).
        let mut qualified: Vec<Option<String>> = vec![None; infos.len()];
        for i in 0..infos.len() {
            resolve_qualified(&infos, &by_name, &mut qualified, i);
        }
        for (i, q) in qualified.into_iter().enumerate() {
            let own = q.unwrap_or_else(|| infos[i].token.to_string());
            infos[i].qualified_type = match prefix {
                Some(p) => format!("{}${}", p.qualified_type, own),
                None => own,
            };
        }

        // Pass 4: ids + namespacing.
        for info in &mut infos {
            info.id = format!(
                "urn:pulumi:{}::{}::{}::{}",
                ctx.opts.stack, ctx.opts.project, info.qualified_type, info.reg_name
            );
            match prefix {
                Some(p) => {
                    info.display_logical =
                        Cow::Owned(format!("{}.{}", p.logical_prefix, info.logical));
                    info.component_id = Some(p.instance_id.to_string());
                    info.source_file = p.source_file;
                    // Plain resources inside a component body are children;
                    // semantic kinds (component, stack_reference, provider,
                    // external) are preserved — `component_id` marks
                    // containment for those.
                    if info.kind == NodeKind::Resource {
                        info.kind = NodeKind::ComponentChild;
                    }
                }
                None => {
                    info.source_file = ctx.source_file(info.logical);
                }
            }
        }

        let variables = variables
            .iter()
            .map(|v| (v.key.as_ref(), &v.value))
            .collect();
        Self {
            infos,
            by_name,
            variables,
        }
    }
}

fn classify<'src>(
    ctx: &Ctx<'src, '_>,
    entry: &'src ResourceEntry<'src>,
    raw: &'src str,
) -> (NodeKind, Cow<'src, str>, Option<&'src ComponentDecl<'src>>) {
    if raw == "pulumi:pulumi:StackReference" {
        return (NodeKind::StackReference, Cow::Borrowed(raw), None);
    }
    if let Some(comp) = ctx.component_tokens.get(raw) {
        let canonical = ctx
            .component_canonical
            .get(comp.key.as_ref())
            .cloned()
            .unwrap_or_else(|| raw.to_string());
        return (NodeKind::Component, Cow::Owned(canonical), Some(comp));
    }
    let canonical = ctx.canonical_token(raw);
    if canonical.starts_with("pulumi:providers:") {
        return (NodeKind::Provider, canonical, None);
    }
    if entry.resource.get.is_some() {
        return (NodeKind::External, canonical, None);
    }
    if let Some(store) = ctx.opts.schema_store {
        if store.is_component(canonical.as_ref()) {
            return (NodeKind::Component, canonical, None);
        }
    }
    (NodeKind::Resource, canonical, None)
}

/// `${name}` with exactly one Name accessor → that name.
fn single_name_root<'src>(expr: &'src Expr<'src>) -> Option<&'src str> {
    match expr {
        Expr::Symbol(_, access) if access.accessors.len() == 1 => match access.accessors.first() {
            Some(PropertyAccessor::Name(n)) => Some(n.as_ref()),
            _ => None,
        },
        _ => None,
    }
}

fn resolve_qualified(
    infos: &[ResInfo<'_>],
    by_name: &HashMap<&str, usize>,
    memo: &mut Vec<Option<String>>,
    i: usize,
) -> String {
    if let Some(Some(q)) = memo.get(i) {
        return q.clone();
    }
    // Mark before recursing: parent cycles are impossible post-validation,
    // but a placeholder keeps this loop-safe regardless.
    if let Some(slot) = memo.get_mut(i) {
        *slot = Some(infos[i].token.to_string());
    }
    let q = match infos[i]
        .parent_logical
        .and_then(|p| by_name.get(p).copied())
    {
        Some(pi) => format!(
            "{}${}",
            resolve_qualified(infos, by_name, memo, pi),
            infos[i].token
        ),
        None => infos[i].token.to_string(),
    };
    if let Some(slot) = memo.get_mut(i) {
        *slot = Some(q.clone());
    }
    q
}

/// Extracts the normalized `org/project/stack` for a StackReference,
/// resolving the `name` property through literal analysis.
fn stack_reference_fq<'src>(
    ctx: &Ctx<'src, '_>,
    entry: &'src ResourceEntry<'src>,
    variables: &'src [VariableEntry<'src>],
    diags: &mut Diagnostics,
) -> Option<String> {
    let name_prop = match &entry.resource.properties {
        ResourceProperties::Map(props) => props.iter().find(|p| p.key.as_ref() == "name"),
        ResourceProperties::Expr(_) => None,
    };
    let var_map: HashMap<&str, &Expr<'_>> = variables
        .iter()
        .map(|v| (v.key.as_ref(), &v.value))
        .collect();
    let raw = match name_prop {
        Some(prop) => {
            let mut memo = HashMap::new();
            let mut visiting = HashSet::new();
            match resolve_literal(&prop.value, &var_map, &mut memo, &mut visiting) {
                Some(lit) => lit,
                None => {
                    diags.warning(
                        None,
                        format!(
                            "StackReference '{}' has a dynamic name; cross-stack edges omitted",
                            entry.logical_name
                        ),
                        "use a literal stack name to enable cross-stack graph links",
                    );
                    return None;
                }
            }
        }
        // Default mirrors the evaluator: the registration name.
        None => Cow::Borrowed(
            entry
                .resource
                .name
                .as_deref()
                .unwrap_or(entry.logical_name.as_ref()),
        ),
    };
    Some(normalize_stack_fq(ctx, raw.as_ref()))
}

fn normalize_stack_fq(ctx: &Ctx<'_, '_>, raw: &str) -> String {
    match raw.split('/').count() {
        3 => raw.to_string(),
        2 => format!("{}/{}", ctx.org, raw),
        _ => format!("{}/{}/{}", ctx.org, ctx.opts.project, raw),
    }
}

/// A resolved reference target within a scope.
enum Target {
    /// Index into the scope's `infos`.
    Node(usize),
    /// External stack output id (`stackoutput::…`).
    StackOutput { id: String },
}

/// Resolves property accesses to targets, collapsing variables/config.
struct ScopeResolver<'s, 'src> {
    scope: &'s Scope<'src>,
    var_memo: HashMap<&'src str, Vec<ResolvedTarget>>,
}

/// Owned variant of [`Target`] for memoization.
#[derive(Clone)]
enum ResolvedTarget {
    Node(usize),
    StackOutput { id: String },
}

impl<'s, 'src> ScopeResolver<'s, 'src> {
    fn new(scope: &'s Scope<'src>) -> Self {
        Self {
            scope,
            var_memo: HashMap::new(),
        }
    }

    fn targets_of_expr(&mut self, expr: &'src Expr<'src>) -> Vec<Target> {
        let mut accesses: Vec<&'src PropertyAccess<'src>> = Vec::new();
        walk_expr(expr, &AccessCollector, &mut accesses);
        let mut out = Vec::new();
        for access in accesses {
            self.targets_of_access(access, &mut out);
        }
        out
    }

    fn targets_of_access(&mut self, access: &'src PropertyAccess<'src>, out: &mut Vec<Target>) {
        let root = match access.root_name() {
            Ok(root) => root,
            Err(_) => return,
        };
        if let Some(&idx) = self.scope.by_name.get(root) {
            out.push(Target::Node(idx));
            let info = &self.scope.infos[idx];
            if info.kind == NodeKind::StackReference {
                if let (Some(fq), Some(key)) = (&info.stack_fq, output_key_of(access)) {
                    out.push(Target::StackOutput {
                        id: format!("stackoutput::{}::{}", fq, key),
                    });
                }
            }
            return;
        }
        if self.scope.variables.contains_key(root) {
            let mut visiting = HashSet::new();
            let resolved = self.resolve_variable(root, &mut visiting);
            for t in resolved {
                out.push(match t {
                    ResolvedTarget::Node(idx) => Target::Node(idx),
                    ResolvedTarget::StackOutput { id } => Target::StackOutput { id },
                });
            }
        }
        // config / pulumi / anything else: no edge.
    }

    fn resolve_variable(
        &mut self,
        name: &'src str,
        visiting: &mut HashSet<&'src str>,
    ) -> Vec<ResolvedTarget> {
        if let Some(cached) = self.var_memo.get(name) {
            return cached.clone();
        }
        if !visiting.insert(name) {
            return Vec::new();
        }
        let mut resolved: Vec<ResolvedTarget> = Vec::new();
        if let Some(expr) = self.scope.variables.get(name).copied() {
            let mut accesses: Vec<&'src PropertyAccess<'src>> = Vec::new();
            walk_expr(expr, &AccessCollector, &mut accesses);
            for access in accesses {
                let root = match access.root_name() {
                    Ok(root) => root,
                    Err(_) => continue,
                };
                if let Some(&idx) = self.scope.by_name.get(root) {
                    resolved.push(ResolvedTarget::Node(idx));
                    let info = &self.scope.infos[idx];
                    if info.kind == NodeKind::StackReference {
                        if let (Some(fq), Some(key)) = (&info.stack_fq, output_key_of(access)) {
                            resolved.push(ResolvedTarget::StackOutput {
                                id: format!("stackoutput::{}::{}", fq, key),
                            });
                        }
                    }
                } else if self.scope.variables.contains_key(root) && root != name {
                    resolved.extend(self.resolve_variable(root, visiting));
                }
            }
        }
        visiting.remove(name);
        self.var_memo.insert(name, resolved.clone());
        resolved
    }
}

/// `sref.outputs.key` / `sref.outputs["key"]` → `key`.
fn output_key_of<'src>(access: &'src PropertyAccess<'src>) -> Option<&'src str> {
    match (access.accessors.get(1), access.accessors.get(2)) {
        (Some(PropertyAccessor::Name(o)), Some(PropertyAccessor::Name(k))) if o == "outputs" => {
            Some(k.as_ref())
        }
        (Some(PropertyAccessor::Name(o)), Some(PropertyAccessor::StringSubscript(k)))
            if o == "outputs" =>
        {
            Some(k.as_ref())
        }
        _ => None,
    }
}

/// Emits nodes and typed edges for one scope, recursing into local
/// component instances.
fn emit_scope<'src>(
    ctx: &mut Ctx<'src, '_>,
    scope: &Scope<'src>,
    nodes: &mut Vec<GraphNode<'src>>,
    edges: &mut EdgeMap<'src>,
    instantiation_stack: &mut Vec<&'src str>,
    diags: &mut Diagnostics,
) {
    let var_map: HashMap<&'src str, &'src Expr<'src>> =
        scope.variables.iter().map(|(k, v)| (*k, *v)).collect();

    for info in &scope.infos {
        nodes.push(GraphNode {
            id: Cow::Owned(info.id.clone()),
            kind: info.kind,
            logical_name: info.display_logical.clone(),
            name: Cow::Borrowed(info.reg_name),
            type_token: Some(info.token.clone()),
            package: package_of(&info.token),
            parent_id: info
                .parent_logical
                .and_then(|p| scope.by_name.get(p).copied())
                .map(|pi| Cow::Owned(scope.infos[pi].id.clone()))
                .or_else(|| info.component_id.clone().map(Cow::Owned)),
            component_id: info.component_id.clone().map(Cow::Owned),
            source_file: info.source_file,
            literal_properties: collect_literal_properties(info.entry, &var_map),
            organization: Cow::Borrowed(ctx.org),
            project: Cow::Borrowed(ctx.opts.project),
            stack: Cow::Borrowed(ctx.opts.stack),
        });
    }

    emit_typed_edges(scope, edges);

    // Expand local component instances.
    for info in &scope.infos {
        let Some(comp) = info.component_decl else {
            continue;
        };
        let key = comp.key.as_ref();
        if instantiation_stack.contains(&key) {
            diags.warning(
                None,
                format!(
                    "recursive component instantiation of '{}'; expansion truncated",
                    key
                ),
                "component bodies that instantiate themselves are not expanded further",
            );
            continue;
        }
        if instantiation_stack.len() >= MAX_COMPONENT_DEPTH {
            diags.warning(
                None,
                "component nesting exceeds maximum expansion depth; expansion truncated",
                format!("maximum depth is {}", MAX_COMPONENT_DEPTH),
            );
            continue;
        }
        instantiation_stack.push(key);
        let prefix = InstancePrefix {
            qualified_type: &info.qualified_type,
            logical_prefix: info.display_logical.as_ref(),
            instance_id: &info.id,
            source_file: info.source_file,
        };
        let child_scope = Scope::build(
            ctx,
            &comp.component.resources,
            &comp.component.variables,
            Some(&prefix),
            diags,
        );
        for child in &child_scope.infos {
            add_edge(
                edges,
                info.id.clone(),
                child.id.clone(),
                EdgeKind::Contains,
                None,
            );
        }
        emit_scope(ctx, &child_scope, nodes, edges, instantiation_stack, diags);
        instantiation_stack.pop();
    }
}

fn package_of<'src>(token: &Cow<'src, str>) -> Option<Cow<'src, str>> {
    let pkg = token.split(':').next().filter(|p| !p.is_empty())?;
    Some(match token {
        Cow::Borrowed(t) => Cow::Borrowed(&t[..pkg.len()]),
        Cow::Owned(_) => Cow::Owned(pkg.to_string()),
    })
}

/// Typed edges for every resource in a scope (replaces the untyped
/// merged walk in `eval::graph`).
fn emit_typed_edges<'src>(scope: &Scope<'src>, edges: &mut EdgeMap<'src>) {
    let mut resolver = ScopeResolver::new(scope);

    // Default-provider donors (mirrors eval::graph's implicit edges).
    let default_providers: Vec<usize> = scope
        .infos
        .iter()
        .enumerate()
        .filter(|(_, i)| i.entry.resource.default_provider == Some(true))
        .map(|(i, _)| i)
        .collect();

    for (idx, info) in scope.infos.iter().enumerate() {
        let source_id = &info.id;
        let emit = |resolver: &mut ScopeResolver<'_, 'src>,
                    edges: &mut EdgeMap<'src>,
                    expr: &'src Expr<'src>,
                    relationship: EdgeKind,
                    path: Option<Cow<'src, str>>| {
            for target in resolver.targets_of_expr(expr) {
                match target {
                    Target::Node(t) if t != idx => add_edge(
                        edges,
                        source_id.clone(),
                        scope.infos[t].id.clone(),
                        relationship,
                        path.clone(),
                    ),
                    Target::Node(_) => {}
                    Target::StackOutput { id } => add_edge(
                        edges,
                        source_id.clone(),
                        id,
                        EdgeKind::ConsumesStackOutput,
                        path.clone(),
                    ),
                }
            }
        };

        match &info.entry.resource.properties {
            ResourceProperties::Map(props) => {
                for prop in props {
                    emit(
                        &mut resolver,
                        edges,
                        &prop.value,
                        EdgeKind::References,
                        Some(Cow::Borrowed(prop.key.as_ref())),
                    );
                }
            }
            ResourceProperties::Expr(expr) => emit(
                &mut resolver,
                edges,
                expr,
                EdgeKind::References,
                Some(Cow::Borrowed("properties")),
            ),
        }

        let opts = &info.entry.resource.options;
        if let Some(expr) = &opts.depends_on {
            emit(&mut resolver, edges, expr, EdgeKind::DependsOn, None);
        }
        if let Some(expr) = &opts.parent {
            emit(&mut resolver, edges, expr, EdgeKind::Parent, None);
        }
        if let Some(expr) = &opts.provider {
            emit(
                &mut resolver,
                edges,
                expr,
                EdgeKind::Provider,
                Some(Cow::Borrowed("options.provider")),
            );
        }
        if let Some(expr) = &opts.providers {
            emit(
                &mut resolver,
                edges,
                expr,
                EdgeKind::Provider,
                Some(Cow::Borrowed("options.providers")),
            );
        }
        if let Some(expr) = &opts.protect {
            emit(
                &mut resolver,
                edges,
                expr,
                EdgeKind::References,
                Some(Cow::Borrowed("options.protect")),
            );
        }
        if let Some(expr) = &opts.aliases {
            emit(
                &mut resolver,
                edges,
                expr,
                EdgeKind::References,
                Some(Cow::Borrowed("options.aliases")),
            );
        }
        if let Some(expr) = &opts.replace_with {
            emit(
                &mut resolver,
                edges,
                expr,
                EdgeKind::References,
                Some(Cow::Borrowed("options.replaceWith")),
            );
        }
        if let Some(expr) = &opts.deleted_with {
            emit(
                &mut resolver,
                edges,
                expr,
                EdgeKind::References,
                Some(Cow::Borrowed("options.deletedWith")),
            );
        }
        if let Some(get) = &info.entry.resource.get {
            emit(
                &mut resolver,
                edges,
                &get.id,
                EdgeKind::References,
                Some(Cow::Borrowed("get.id")),
            );
            for prop in &get.state {
                for target in resolver.targets_of_expr(&prop.value) {
                    match target {
                        Target::Node(t) if t != idx => add_edge(
                            edges,
                            source_id.clone(),
                            scope.infos[t].id.clone(),
                            EdgeKind::References,
                            Some(Cow::Owned(format!("get.state.{}", prop.key))),
                        ),
                        Target::Node(_) => {}
                        Target::StackOutput { id } => add_edge(
                            edges,
                            source_id.clone(),
                            id,
                            EdgeKind::ConsumesStackOutput,
                            Some(Cow::Owned(format!("get.state.{}", prop.key))),
                        ),
                    }
                }
            }
        }

        // Implicit default-provider edges (parity with eval::graph).
        if opts.provider.is_none() && info.entry.resource.default_provider != Some(true) {
            for &p in &default_providers {
                if p != idx {
                    add_edge(
                        edges,
                        source_id.clone(),
                        scope.infos[p].id.clone(),
                        EdgeKind::Provider,
                        None,
                    );
                }
            }
        }
    }
}

fn empty_graph<'src>(opts: &GraphExportOptions<'src>) -> ResourceGraph<'src> {
    let org = if opts.organization.is_empty() {
        ORG_PLACEHOLDER
    } else {
        opts.organization
    };
    ResourceGraph {
        schema_version: SCHEMA_VERSION,
        organization: Cow::Borrowed(org),
        project: Cow::Borrowed(opts.project),
        stack: Cow::Borrowed(opts.stack),
        nodes: Vec::new(),
        edges: Vec::new(),
    }
}

fn finalize<'src>(
    opts: &GraphExportOptions<'src>,
    ctx: &Ctx<'src, '_>,
    mut nodes: Vec<GraphNode<'src>>,
    edges: EdgeMap<'src>,
    diags: &mut Diagnostics,
) -> ResourceGraph<'src> {
    nodes.sort_by(|a, b| a.id.cmp(&b.id));
    let mut seen: HashSet<&str> = HashSet::new();
    for node in &nodes {
        if !seen.insert(node.id.as_ref()) {
            diags.warning(
                None,
                format!("duplicate node id '{}'", node.id),
                "two declarations produce the same URN (same qualified type and name); \
                 graph rows are kept but the id is ambiguous",
            );
        }
    }
    let edges = edges
        .into_iter()
        .map(|((source_id, target_id, relationship), paths)| GraphEdge {
            source_id: Cow::Owned(source_id),
            target_id: Cow::Owned(target_id),
            relationship,
            property_paths: paths.into_iter().collect(),
            organization: Cow::Borrowed(ctx.org),
            project: Cow::Borrowed(opts.project),
            stack: Cow::Borrowed(opts.stack),
        })
        .collect();
    ResourceGraph {
        schema_version: SCHEMA_VERSION,
        organization: Cow::Borrowed(ctx.org),
        project: Cow::Borrowed(opts.project),
        stack: Cow::Borrowed(opts.stack),
        nodes,
        edges,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::parse::parse_template;

    fn export(yaml: &str) -> (ResourceGraph<'static>, Diagnostics) {
        export_with(yaml, "org", "proj", "dev")
    }

    fn export_with(
        yaml: &str,
        organization: &'static str,
        project: &'static str,
        stack: &'static str,
    ) -> (ResourceGraph<'static>, Diagnostics) {
        let (template, parse_diags) = parse_template(yaml, None);
        assert!(!parse_diags.has_errors(), "parse failed: {}", parse_diags);
        let template: &'static TemplateDecl<'static> = Box::leak(Box::new(template));
        let opts = GraphExportOptions {
            organization,
            project,
            stack,
            source_map: None,
            schema_store: None,
        };
        export_resource_graph(template, &opts)
    }

    fn node<'g>(graph: &'g ResourceGraph<'static>, logical: &str) -> &'g GraphNode<'static> {
        graph
            .nodes
            .iter()
            .find(|n| n.logical_name == logical)
            .unwrap_or_else(move || panic!("no node with logical name '{}'", logical))
    }

    fn edges_between<'g>(
        graph: &'g ResourceGraph<'static>,
        source_logical: &str,
        target_logical: &str,
    ) -> Vec<&'g GraphEdge<'static>> {
        let s = &node(graph, source_logical).id;
        let t = &node(graph, target_logical).id;
        graph
            .edges
            .iter()
            .filter(|e| &e.source_id == s && &e.target_id == t)
            .collect()
    }

    const BASIC: &str = "name: proj\nruntime: yaml\nresources:\n  bucket:\n    type: gcp:storage:Bucket\n    properties:\n      location: US\n";

    #[test]
    fn urn_format_basic() {
        let (graph, _) = export(BASIC);
        let n = node(&graph, "bucket");
        assert_eq!(
            n.id,
            "urn:pulumi:dev::proj::gcp:storage/bucket:Bucket::bucket"
        );
        assert_eq!(n.kind, NodeKind::Resource);
        assert_eq!(n.type_token.as_deref(), Some("gcp:storage/bucket:Bucket"));
        assert_eq!(n.package.as_deref(), Some("gcp"));
    }

    #[test]
    fn urn_uses_name_override() {
        let yaml = "name: proj\nruntime: yaml\nresources:\n  bucket:\n    type: gcp:storage:Bucket\n    name: physical-bucket\n";
        let (graph, _) = export(yaml);
        let n = node(&graph, "bucket");
        assert_eq!(
            n.id,
            "urn:pulumi:dev::proj::gcp:storage/bucket:Bucket::physical-bucket"
        );
        assert_eq!(n.name, "physical-bucket");
        assert_eq!(n.logical_name, "bucket");
    }

    #[test]
    fn stack_node_and_contains() {
        let (graph, _) = export(BASIC);
        let stack = graph
            .nodes
            .iter()
            .find(|n| n.kind == NodeKind::Stack)
            .expect("stack node");
        assert_eq!(
            stack.id,
            "urn:pulumi:dev::proj::pulumi:pulumi:Stack::proj-dev"
        );
        let contains: Vec<_> = graph
            .edges
            .iter()
            .filter(|e| e.relationship == EdgeKind::Contains && e.source_id == stack.id)
            .collect();
        assert_eq!(contains.len(), 1);
        assert_eq!(contains[0].target_id, node(&graph, "bucket").id);
    }

    #[test]
    fn qualified_type_nests_parents() {
        let yaml = "name: proj\nruntime: yaml\nresources:\n  parent:\n    type: test:mod:Parent\n  child:\n    type: test:mod:Child\n    options:\n      parent: ${parent}\n  grandchild:\n    type: test:mod:Grand\n    options:\n      parent: ${child}\n";
        let (graph, diags) = export(yaml);
        assert!(!diags.has_warnings(), "unexpected warnings: {}", diags);
        assert_eq!(
            node(&graph, "grandchild").id,
            "urn:pulumi:dev::proj::test:mod/parent:Parent$test:mod/child:Child$test:mod/grand:Grand::grandchild"
        );
        let parent_edges = edges_between(&graph, "child", "parent");
        assert!(parent_edges
            .iter()
            .any(|e| e.relationship == EdgeKind::Parent));
    }

    #[test]
    fn dynamic_parent_warns() {
        let yaml = "name: proj\nruntime: yaml\nresources:\n  a:\n    type: test:mod:A\n  b:\n    type: test:mod:B\n    options:\n      parent: ${a.someOutput}\n";
        let (graph, diags) = export(yaml);
        assert!(diags.iter().any(|d| d.summary.contains("dynamic parent")));
        // Falls back to un-nested type.
        assert_eq!(
            node(&graph, "b").id,
            "urn:pulumi:dev::proj::test:mod/b:B::b"
        );
    }

    #[test]
    fn typed_edge_kinds() {
        let yaml = concat!(
            "name: proj\nruntime: yaml\nresources:\n",
            "  prov:\n    type: pulumi:providers:gcp\n",
            "  a:\n    type: test:mod:A\n",
            "  b:\n    type: test:mod:B\n",
            "    properties:\n      ref: ${a.id}\n",
            "    options:\n      dependsOn:\n        - ${a}\n      provider: ${prov}\n",
        );
        let (graph, _) = export(yaml);
        let kinds: Vec<EdgeKind> = edges_between(&graph, "b", "a")
            .iter()
            .map(|e| e.relationship)
            .collect();
        assert!(kinds.contains(&EdgeKind::References));
        assert!(kinds.contains(&EdgeKind::DependsOn));
        let prov_edges = edges_between(&graph, "b", "prov");
        assert!(prov_edges
            .iter()
            .any(|e| e.relationship == EdgeKind::Provider
                && e.property_paths.iter().any(|p| p == "options.provider")));
        assert_eq!(node(&graph, "prov").kind, NodeKind::Provider);
    }

    #[test]
    fn implicit_default_provider_edge_has_empty_path() {
        let yaml = concat!(
            "name: proj\nruntime: yaml\nresources:\n",
            "  prov:\n    type: pulumi:providers:gcp\n    defaultProvider: true\n",
            "  a:\n    type: test:mod:A\n",
            "  b:\n    type: test:mod:B\n    options:\n      provider: ${prov}\n",
        );
        let (graph, _) = export(yaml);
        let implicit = edges_between(&graph, "a", "prov");
        assert_eq!(implicit.len(), 1);
        assert_eq!(implicit[0].relationship, EdgeKind::Provider);
        assert!(
            implicit[0].property_paths.is_empty(),
            "implicit edge has no path"
        );
        // Explicit provider present: only the explicit (pathed) edge.
        let explicit = edges_between(&graph, "b", "prov");
        assert_eq!(explicit.len(), 1);
        assert!(!explicit[0].property_paths.is_empty());
    }

    #[test]
    fn edge_merge_unions_property_paths() {
        let yaml = "name: proj\nruntime: yaml\nresources:\n  a:\n    type: test:mod:A\n  b:\n    type: test:mod:B\n    properties:\n      one: ${a.id}\n      two: ${a.name}\n";
        let (graph, _) = export(yaml);
        let refs = edges_between(&graph, "b", "a");
        assert_eq!(
            refs.len(),
            1,
            "edges merged by (source, target, relationship)"
        );
        assert_eq!(refs[0].property_paths, vec!["one", "two"]);
    }

    #[test]
    fn variable_collapse_chain() {
        let yaml = concat!(
            "name: proj\nruntime: yaml\n",
            "config:\n  region:\n    type: string\n",
            "variables:\n",
            "  v1: ${a.id}\n",
            "  v2: ${v1}\n",
            "  vconfig: ${region}\n",
            "resources:\n",
            "  a:\n    type: test:mod:A\n",
            "  b:\n    type: test:mod:B\n    properties:\n      ref: ${v2}\n      reg: ${vconfig}\n",
        );
        let (graph, _) = export(yaml);
        let refs = edges_between(&graph, "b", "a");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].relationship, EdgeKind::References);
        assert_eq!(refs[0].property_paths, vec!["ref"]);
        // Config-only chains produce no edges; b has edges only to a.
        let b_id = &node(&graph, "b").id;
        let b_out: Vec<_> = graph
            .edges
            .iter()
            .filter(|e| &e.source_id == b_id)
            .collect();
        assert_eq!(b_out.len(), 1);
    }

    #[test]
    fn stackref_literal_consumes_edge() {
        let yaml = concat!(
            "name: proj\nruntime: yaml\nresources:\n",
            "  upstream:\n    type: pulumi:pulumi:StackReference\n",
            "    properties:\n      name: org/producer/prod\n",
            "  b:\n    type: test:mod:B\n    properties:\n      ds: ${upstream.outputs[\"datasetId\"]}\n",
        );
        let (graph, diags) = export(yaml);
        assert!(!diags.has_warnings(), "unexpected warnings: {}", diags);
        assert_eq!(node(&graph, "upstream").kind, NodeKind::StackReference);
        // Local edge keeps the intra-stack graph connected.
        assert!(edges_between(&graph, "b", "upstream")
            .iter()
            .any(|e| e.relationship == EdgeKind::References));
        // Cross-stack edge targets the producer's output-node id.
        let b_id = &node(&graph, "b").id;
        let consumes: Vec<_> = graph
            .edges
            .iter()
            .filter(|e| &e.source_id == b_id && e.relationship == EdgeKind::ConsumesStackOutput)
            .collect();
        assert_eq!(consumes.len(), 1);
        assert_eq!(
            consumes[0].target_id,
            "stackoutput::org/producer/prod::datasetId"
        );
        assert_eq!(consumes[0].property_paths, vec!["ds"]);
    }

    #[test]
    fn stackref_fq_normalization() {
        for (name_line, expected) in [
            ("      name: a/b/c\n", "stackoutput::a/b/c::k"),
            ("      name: b/c\n", "stackoutput::org/b/c::k"),
            ("      name: c\n", "stackoutput::org/proj/c::k"),
        ] {
            let yaml = format!(
                "name: proj\nruntime: yaml\nresources:\n  sref:\n    type: pulumi:pulumi:StackReference\n    properties:\n{}  b:\n    type: test:mod:B\n    properties:\n      x: ${{sref.outputs.k}}\n",
                name_line
            );
            let (graph, _) = export(&yaml);
            assert!(
                graph.edges.iter().any(|e| e.target_id == expected),
                "expected target {} in {:?}",
                expected,
                graph
                    .edges
                    .iter()
                    .map(|e| e.target_id.as_ref())
                    .collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn stackref_default_name_is_registration_name() {
        let yaml = "name: proj\nruntime: yaml\nresources:\n  upstream:\n    type: pulumi:pulumi:StackReference\n  b:\n    type: test:mod:B\n    properties:\n      x: ${upstream.outputs.k}\n";
        let (graph, _) = export(yaml);
        assert!(graph
            .edges
            .iter()
            .any(|e| e.target_id == "stackoutput::org/proj/upstream::k"));
    }

    #[test]
    fn stackref_dynamic_name_warns_and_omits_cross_edge() {
        let yaml = concat!(
            "name: proj\nruntime: yaml\n",
            "config:\n  env:\n    type: string\n",
            "resources:\n",
            "  sref:\n    type: pulumi:pulumi:StackReference\n    properties:\n      name: ${env}\n",
            "  b:\n    type: test:mod:B\n    properties:\n      x: ${sref.outputs.k}\n",
        );
        let (graph, diags) = export(yaml);
        assert!(diags.iter().any(|d| d.summary.contains("dynamic name")));
        assert!(!graph
            .edges
            .iter()
            .any(|e| e.relationship == EdgeKind::ConsumesStackOutput));
        assert!(edges_between(&graph, "b", "sref")
            .iter()
            .any(|e| e.relationship == EdgeKind::References));
    }

    #[test]
    fn output_nodes_and_exports_edges() {
        let yaml = "name: proj\nruntime: yaml\nresources:\n  bucket:\n    type: gcp:storage:Bucket\noutputs:\n  bucketId: ${bucket.id}\n";
        let (graph, _) = export(yaml);
        let out = node(&graph, "bucketId");
        assert_eq!(out.kind, NodeKind::Output);
        assert_eq!(out.id, "stackoutput::org/proj/dev::bucketId");
        let exports: Vec<_> = graph
            .edges
            .iter()
            .filter(|e| e.source_id == out.id && e.relationship == EdgeKind::Exports)
            .collect();
        assert_eq!(exports.len(), 1);
        assert_eq!(exports[0].target_id, node(&graph, "bucket").id);
    }

    #[test]
    fn cross_stack_producer_consumer_id_equality() {
        // Producer stack exports an output...
        let (producer, _) = export_with(
            "name: producer\nruntime: yaml\nresources:\n  ds:\n    type: gcp:bigquery:Dataset\noutputs:\n  datasetId: ${ds.datasetId}\n",
            "org",
            "producer",
            "prod",
        );
        let out_id = node(&producer, "datasetId").id.clone();
        // ...and the consumer's cross-stack edge targets exactly that id.
        let (consumer, _) = export_with(
            "name: consumer\nruntime: yaml\nresources:\n  up:\n    type: pulumi:pulumi:StackReference\n    properties:\n      name: org/producer/prod\n  t:\n    type: gcp:bigquery:Table\n    properties:\n      ds: ${up.outputs.datasetId}\n",
            "org",
            "consumer",
            "dev",
        );
        assert!(
            consumer.edges.iter().any(|e| e.target_id == out_id),
            "consumer edge must target the producer's output node id"
        );
    }

    #[test]
    fn get_resource_is_external() {
        let yaml = "name: proj\nruntime: yaml\nresources:\n  existing:\n    type: gcp:storage:Bucket\n    get:\n      id: my-bucket\n";
        let (graph, _) = export(yaml);
        assert_eq!(node(&graph, "existing").kind, NodeKind::External);
    }

    #[test]
    fn component_children_and_edges() {
        let yaml = concat!(
            "name: proj\nruntime: yaml\n",
            "components:\n",
            "  Widget:\n",
            "    inputs:\n      size:\n        type: string\n",
            "    resources:\n",
            "      inner:\n        type: test:mod:Inner\n",
            "      outer:\n        type: test:mod:Outer\n        properties:\n          ref: ${inner.id}\n",
            "resources:\n",
            "  w:\n    type: proj:index:Widget\n    properties:\n      size: large\n",
        );
        let (graph, diags) = export(yaml);
        assert!(!diags.has_errors(), "unexpected errors: {}", diags);
        let instance = node(&graph, "w");
        assert_eq!(instance.kind, NodeKind::Component);
        assert_eq!(instance.type_token.as_deref(), Some("proj:index:Widget"));
        let inner = node(&graph, "w.inner");
        assert_eq!(inner.kind, NodeKind::ComponentChild);
        assert_eq!(
            inner.id,
            "urn:pulumi:dev::proj::proj:index:Widget$test:mod/inner:Inner::inner"
        );
        assert_eq!(inner.component_id.as_deref(), Some(instance.id.as_ref()));
        // contains: instance -> each child.
        let contains: Vec<_> = graph
            .edges
            .iter()
            .filter(|e| e.source_id == instance.id && e.relationship == EdgeKind::Contains)
            .collect();
        assert_eq!(contains.len(), 2);
        // intra-component reference edge.
        assert!(edges_between(&graph, "w.outer", "w.inner")
            .iter()
            .any(|e| e.relationship == EdgeKind::References));
    }

    #[test]
    fn duplicate_urns_warn() {
        let yaml = "name: proj\nruntime: yaml\nresources:\n  a:\n    type: test:mod:Thing\n    name: shared\n  b:\n    type: test:mod:Thing\n    name: shared\n";
        let (_, diags) = export(yaml);
        assert!(diags
            .iter()
            .any(|d| d.summary.contains("duplicate node id")));
    }

    #[test]
    fn org_placeholder_warns_when_cross_stack_ids_emitted() {
        let (graph, diags) = export_with(
            "name: proj\nruntime: yaml\nresources:\n  a:\n    type: test:mod:A\noutputs:\n  x: ${a.id}\n",
            "",
            "proj",
            "dev",
        );
        assert!(diags
            .iter()
            .any(|d| d.summary.contains("organization not set")));
        assert_eq!(
            node(&graph, "x").id,
            "stackoutput::organization/proj/dev::x"
        );
        // No warning when nothing cross-stack is emitted.
        let (_, quiet) = export_with(BASIC, "", "proj", "dev");
        assert!(!quiet
            .iter()
            .any(|d| d.summary.contains("organization not set")));
    }

    #[test]
    fn deterministic_and_sorted() {
        let yaml = concat!(
            "name: proj\nruntime: yaml\nresources:\n",
            "  z:\n    type: test:mod:Z\n",
            "  a:\n    type: test:mod:A\n    properties:\n      ref: ${z.id}\n",
            "outputs:\n  o: ${a.id}\n",
        );
        let (g1, _) = export(yaml);
        let (g2, _) = export(yaml);
        assert_eq!(g1, g2);
        let ids: Vec<_> = g1.nodes.iter().map(|n| n.id.as_ref()).collect();
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(ids, sorted);
        let json1 = g1.to_json().expect("serializes");
        let json2 = g2.to_json().expect("serializes");
        assert_eq!(json1, json2);
        assert!(json1.ends_with('\n'));
    }

    #[test]
    fn dag_error_returns_empty_graph() {
        let yaml = "name: proj\nruntime: yaml\nresources:\n  a:\n    type: test:mod:A\n    properties:\n      ref: ${missing}\n";
        let (graph, diags) = export(yaml);
        assert!(diags.has_errors());
        assert!(graph.nodes.is_empty());
        assert!(graph.edges.is_empty());
        assert_eq!(graph.project, "proj");
    }

    // ---- literal properties ----

    #[test]
    fn literal_properties_paths_and_values() {
        let yaml = concat!(
            "name: proj\nruntime: yaml\n",
            "config:\n  env:\n    type: string\n",
            "variables:\n  region: us-central1\n",
            "resources:\n",
            "  ds:\n    type: gcp:bigquery:Dataset\n",
            "    properties:\n",
            "      datasetId: analytics\n",
            "      project: data-proj\n",
            "      deleteContents: true\n",
            "      maxAge: 30\n",
            "      location: ${region}\n",
            "      envName: ${env}\n",
            "      labels:\n        team: data\n",
            "      replicas:\n        - r1\n        - r2\n",
        );
        let (graph, _) = export(yaml);
        let props: HashMap<&str, &str> = node(&graph, "ds")
            .literal_properties
            .iter()
            .map(|(k, v)| (k.as_ref(), v.as_ref()))
            .collect();
        assert_eq!(props.get("datasetId"), Some(&"analytics"));
        assert_eq!(props.get("project"), Some(&"data-proj"));
        assert_eq!(props.get("deleteContents"), Some(&"true"));
        assert_eq!(
            props.get("maxAge"),
            Some(&"30"),
            "integral number, no fraction"
        );
        assert_eq!(props.get("location"), Some(&"us-central1"), "via variable");
        assert_eq!(props.get("labels.team"), Some(&"data"));
        assert_eq!(props.get("replicas.0"), Some(&"r1"));
        assert_eq!(props.get("replicas.1"), Some(&"r2"));
        assert!(
            !props.contains_key("envName"),
            "config values are not literals"
        );
    }

    #[test]
    fn literal_interpolation_all_literal_parts() {
        let yaml = concat!(
            "name: proj\nruntime: yaml\n",
            "variables:\n  env: prod\n",
            "resources:\n",
            "  a:\n    type: test:mod:A\n",
            "  ds:\n    type: gcp:bigquery:Dataset\n",
            "    properties:\n",
            "      datasetId: analytics_${env}\n",
            "      mixed: prefix-${a.id}\n",
        );
        let (graph, _) = export(yaml);
        let props: HashMap<&str, &str> = node(&graph, "ds")
            .literal_properties
            .iter()
            .map(|(k, v)| (k.as_ref(), v.as_ref()))
            .collect();
        assert_eq!(props.get("datasetId"), Some(&"analytics_prod"));
        assert!(
            !props.contains_key("mixed"),
            "resource refs are not literals"
        );
    }

    #[test]
    fn identity_self_link_contract() {
        // Stack A declares the dataset; stacks B and C declare tables using
        // it by literal (project, datasetId). All three must stringify the
        // identity byte-identically so the BQ derived-edge join matches.
        let (a, _) = export_with(
            "name: stack-a\nruntime: yaml\nresources:\n  ds:\n    type: gcp:bigquery:Dataset\n    properties:\n      project: data-proj\n      datasetId: analytics\n",
            "org", "stack-a", "prod",
        );
        let (b, _) = export_with(
            "name: stack-b\nruntime: yaml\nresources:\n  t:\n    type: gcp:bigquery:Table\n    properties:\n      project: data-proj\n      datasetId: analytics\n      tableId: events\n",
            "org", "stack-b", "prod",
        );
        let (c, _) = export_with(
            "name: stack-c\nruntime: yaml\nvariables:\n  dsName: analytics\nresources:\n  t:\n    type: gcp:bigquery:Table\n    properties:\n      project: data-proj\n      datasetId: ${dsName}\n      tableId: clicks\n",
            "org", "stack-c", "prod",
        );
        let get = |g: &ResourceGraph<'static>, l: &str, k: &str| -> String {
            node(g, l)
                .literal_properties
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.to_string())
                .unwrap_or_else(|| panic!("missing literal {}", k))
        };
        assert_eq!(get(&a, "ds", "project"), get(&b, "t", "project"));
        assert_eq!(get(&a, "ds", "datasetId"), get(&b, "t", "datasetId"));
        assert_eq!(get(&a, "ds", "datasetId"), get(&c, "t", "datasetId"));
    }
}

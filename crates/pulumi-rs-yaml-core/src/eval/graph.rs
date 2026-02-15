use crate::ast::expr::Expr;
use crate::ast::template::*;
use crate::diag::{self, Diagnostics};
use std::collections::{HashMap, HashSet};

/// A node in the dependency graph. Replaces Go's `graphNode` interface.
#[derive(Debug, Clone)]
pub enum GraphNode<'a> {
    Pulumi(&'a PulumiDecl<'a>),
    ConfigEntry { key: &'a str },
    Variable { key: &'a str },
    Resource { logical_name: &'a str },
    Missing { name: String },
}

impl GraphNode<'_> {
    /// Returns the kind of this node for error messages.
    pub fn kind(&self) -> &'static str {
        match self {
            GraphNode::Pulumi(_) => "pulumi",
            GraphNode::ConfigEntry { .. } => "config",
            GraphNode::Variable { .. } => "variable",
            GraphNode::Resource { .. } => "resource",
            GraphNode::Missing { .. } => "missing",
        }
    }

    /// Returns the key/name for this node.
    pub fn key(&self) -> &str {
        match self {
            GraphNode::Pulumi(_) => "pulumi",
            GraphNode::ConfigEntry { key } => key,
            GraphNode::Variable { key } => key,
            GraphNode::Resource { logical_name } => logical_name,
            GraphNode::Missing { name } => name.as_str(),
        }
    }
}

/// Result of topological sort: ordered list of node keys.
pub struct SortResult {
    pub order: Vec<String>,
}

/// Result of topological sort with dependency graph exposed.
pub struct SortResultWithDeps {
    pub order: Vec<String>,
    pub deps: HashMap<String, HashSet<String>>,
}

/// Performs a topological sort of all nodes in a template.
///
/// Returns the nodes in dependency order (dependencies come first).
/// Reports errors for cycles and duplicate node names.
pub fn topological_sort<'a>(template: &'a TemplateDecl<'a>) -> (Vec<String>, Diagnostics) {
    topological_sort_with_sources(template, None)
}

/// Performs a topological sort with optional source file map for rich error messages.
///
/// When `source_map` is provided (mapping logical name → filename), error messages
/// include the source file where each node is defined. This is used for multi-file
/// projects where resources/variables span multiple `Pulumi.*.yaml` files.
pub fn topological_sort_with_sources<'a>(
    template: &'a TemplateDecl<'a>,
    source_map: Option<&HashMap<String, String>>,
) -> (Vec<String>, Diagnostics) {
    let mut diags = Diagnostics::new();
    let mut names: HashMap<&str, &str> = HashMap::new(); // name -> kind

    // Always insert "pulumi" as a node — Go always does this regardless of settings
    names.insert("pulumi", "pulumi");

    for entry in &template.config {
        let key = entry.key.as_ref();
        if key == "pulumi" {
            diags.error(None, "\"pulumi\" is a reserved name", "");
            continue;
        }
        if let Some(existing_kind) = names.insert(key, "config") {
            diags.error(
                None,
                format!(
                    "duplicate node name \"{}\": already defined as {}",
                    key, existing_kind
                ),
                "",
            );
        }
    }

    for entry in &template.variables {
        let key = entry.key.as_ref();
        if key == "pulumi" {
            diags.error(None, "\"pulumi\" is a reserved name", "");
            continue;
        }
        if let Some(existing_kind) = names.insert(key, "variable") {
            diags.error(
                None,
                format!(
                    "duplicate node name \"{}\": already defined as {}",
                    key, existing_kind
                ),
                "",
            );
        }
    }

    for entry in &template.resources {
        let key = entry.logical_name.as_ref();
        if key == "pulumi" {
            diags.error(None, "\"pulumi\" is a reserved name", "");
            continue;
        }
        if let Some(existing_kind) = names.insert(key, "resource") {
            diags.error(
                None,
                format!(
                    "duplicate node name \"{}\": already defined as {}",
                    key, existing_kind
                ),
                "",
            );
        }
    }

    if diags.has_errors() {
        return (Vec::new(), diags);
    }

    // Validate all references exist before building the dependency graph
    validate_references(template, &names, source_map, &mut diags);
    if diags.has_errors() {
        return (Vec::new(), diags);
    }

    // Build adjacency: for each node, collect the set of nodes it depends on
    let mut deps: HashMap<&str, HashSet<&str>> = HashMap::new();

    // Config entries have no dependencies (they come from external config)
    for entry in &template.config {
        deps.entry(entry.key.as_ref()).or_default();
    }

    // Variables depend on whatever their expression references
    for entry in &template.variables {
        let mut node_deps = HashSet::new();
        collect_expr_deps(&entry.value, &names, &mut node_deps);
        deps.insert(entry.key.as_ref(), node_deps);
    }

    // Resources depend on whatever their properties, options, etc. reference
    for entry in &template.resources {
        let mut node_deps = HashSet::new();
        collect_resource_deps(&entry.resource, &names, &mut node_deps);

        // Default provider dependencies: resources without an explicit provider
        // depend on any resource marked as defaultProvider
        if entry.resource.options.provider.is_none() {
            for other_entry in &template.resources {
                if other_entry.resource.default_provider == Some(true)
                    && other_entry.logical_name != entry.logical_name
                {
                    node_deps.insert(other_entry.logical_name.as_ref());
                }
            }
        }

        deps.insert(entry.logical_name.as_ref(), node_deps);
    }

    // "pulumi" node has no dependencies — always present
    deps.entry("pulumi").or_default();

    // Topological sort using DFS with cycle detection and path reconstruction
    let mut visited: HashSet<&str> = HashSet::new();
    let mut order: Vec<String> = Vec::new();
    let mut path: Vec<&str> = Vec::new();
    let mut path_set: HashSet<&str> = HashSet::new();

    // Sort in a deterministic order
    let mut all_nodes: Vec<&str> = deps.keys().copied().collect();
    all_nodes.sort();

    for node in &all_nodes {
        if !visited.contains(node) {
            dfs_with_path(
                node,
                &deps,
                &mut visited,
                &mut path,
                &mut path_set,
                &mut order,
                source_map,
                &mut diags,
            );
        }
    }

    (order, diags)
}

/// Performs a topological sort and returns the dependency graph alongside the ordering.
///
/// This variant is used by the parallel evaluator to compute topological levels.
pub fn topological_sort_with_deps<'a>(
    template: &'a TemplateDecl<'a>,
    source_map: Option<&HashMap<String, String>>,
) -> (SortResultWithDeps, Diagnostics) {
    let mut diags = Diagnostics::new();
    let mut names: HashMap<&str, &str> = HashMap::new();

    // Always insert "pulumi" as a node — Go always does this regardless of settings
    names.insert("pulumi", "pulumi");
    for entry in &template.config {
        let key = entry.key.as_ref();
        if key == "pulumi" {
            diags.error(None, "\"pulumi\" is a reserved name", "");
            continue;
        }
        if let Some(existing_kind) = names.insert(key, "config") {
            diags.error(
                None,
                format!(
                    "duplicate node name \"{}\": already defined as {}",
                    key, existing_kind
                ),
                "",
            );
        }
    }
    for entry in &template.variables {
        let key = entry.key.as_ref();
        if key == "pulumi" {
            diags.error(None, "\"pulumi\" is a reserved name", "");
            continue;
        }
        if let Some(existing_kind) = names.insert(key, "variable") {
            diags.error(
                None,
                format!(
                    "duplicate node name \"{}\": already defined as {}",
                    key, existing_kind
                ),
                "",
            );
        }
    }
    for entry in &template.resources {
        let key = entry.logical_name.as_ref();
        if key == "pulumi" {
            diags.error(None, "\"pulumi\" is a reserved name", "");
            continue;
        }
        if let Some(existing_kind) = names.insert(key, "resource") {
            diags.error(
                None,
                format!(
                    "duplicate node name \"{}\": already defined as {}",
                    key, existing_kind
                ),
                "",
            );
        }
    }

    if diags.has_errors() {
        return (
            SortResultWithDeps {
                order: Vec::new(),
                deps: HashMap::new(),
            },
            diags,
        );
    }

    validate_references(template, &names, source_map, &mut diags);
    if diags.has_errors() {
        return (
            SortResultWithDeps {
                order: Vec::new(),
                deps: HashMap::new(),
            },
            diags,
        );
    }

    // Build adjacency
    let mut deps: HashMap<&str, HashSet<&str>> = HashMap::new();
    for entry in &template.config {
        deps.entry(entry.key.as_ref()).or_default();
    }
    for entry in &template.variables {
        let mut node_deps = HashSet::new();
        collect_expr_deps(&entry.value, &names, &mut node_deps);
        deps.insert(entry.key.as_ref(), node_deps);
    }
    for entry in &template.resources {
        let mut node_deps = HashSet::new();
        collect_resource_deps(&entry.resource, &names, &mut node_deps);
        if entry.resource.options.provider.is_none() {
            for other_entry in &template.resources {
                if other_entry.resource.default_provider == Some(true)
                    && other_entry.logical_name != entry.logical_name
                {
                    node_deps.insert(other_entry.logical_name.as_ref());
                }
            }
        }
        deps.insert(entry.logical_name.as_ref(), node_deps);
    }
    // "pulumi" node has no dependencies — always present
    deps.entry("pulumi").or_default();

    // DFS topological sort
    let mut visited: HashSet<&str> = HashSet::new();
    let mut order: Vec<String> = Vec::new();
    let mut path: Vec<&str> = Vec::new();
    let mut path_set: HashSet<&str> = HashSet::new();
    let mut all_nodes: Vec<&str> = deps.keys().copied().collect();
    all_nodes.sort();
    for node in &all_nodes {
        if !visited.contains(node) {
            dfs_with_path(
                node,
                &deps,
                &mut visited,
                &mut path,
                &mut path_set,
                &mut order,
                source_map,
                &mut diags,
            );
        }
    }

    // Convert deps to owned strings
    let owned_deps: HashMap<String, HashSet<String>> = deps
        .iter()
        .map(|(k, v)| (k.to_string(), v.iter().map(|s| s.to_string()).collect()))
        .collect();

    (
        SortResultWithDeps {
            order,
            deps: owned_deps,
        },
        diags,
    )
}

/// Groups topologically sorted nodes into levels by dependency depth.
///
/// Level 0 contains nodes with no dependencies (or only external deps).
/// Level N contains nodes whose dependencies are all in levels < N.
/// Within each level, nodes are sorted alphabetically for determinism.
///
/// This enables parallel evaluation: all nodes at the same level can be
/// evaluated concurrently since they have no inter-dependencies.
pub fn topological_levels(
    sorted: &[String],
    deps: &HashMap<String, HashSet<String>>,
) -> Vec<Vec<String>> {
    // Compute the level of each node
    let mut levels: HashMap<&str, usize> = HashMap::new();

    for node in sorted {
        let node_deps = deps.get(node.as_str());
        let max_dep_level = node_deps
            .map(|d| {
                d.iter()
                    .filter_map(|dep| levels.get(dep.as_str()))
                    .max()
                    .copied()
                    .map(|l| l + 1)
                    .unwrap_or(0)
            })
            .unwrap_or(0);
        levels.insert(node.as_str(), max_dep_level);
    }

    // Group nodes by level
    let max_level = levels.values().max().copied().unwrap_or(0);
    let mut result: Vec<Vec<String>> = vec![Vec::new(); max_level + 1];
    for node in sorted {
        let level = levels.get(node.as_str()).copied().unwrap_or(0);
        result[level].push(node.clone());
    }

    // Sort within each level for determinism
    for level in &mut result {
        level.sort();
    }

    result
}

/// Validates that all `${ref}` references in the template refer to defined names.
///
/// Scans variables, resources, and outputs for references. Any reference whose
/// root name is not in `names` (and is not "pulumi") produces an error with
/// a "did you mean?" suggestion if a close match exists.
fn validate_references(
    template: &TemplateDecl<'_>,
    names: &HashMap<&str, &str>,
    source_map: Option<&HashMap<String, String>>,
    diags: &mut Diagnostics,
) {
    let known_names: Vec<String> = names.keys().map(|k| k.to_string()).collect();

    // Check variables
    for entry in &template.variables {
        let mut refs = HashSet::new();
        collect_all_expr_refs(&entry.value, &mut refs);
        for ref_name in refs {
            check_ref(
                ref_name,
                entry.key.as_ref(),
                "variable",
                names,
                &known_names,
                source_map,
                diags,
            );
        }
    }

    // Check resources
    for entry in &template.resources {
        let mut refs = HashSet::new();
        collect_all_resource_refs(&entry.resource, &mut refs);
        for ref_name in refs {
            check_ref(
                ref_name,
                entry.logical_name.as_ref(),
                "resource",
                names,
                &known_names,
                source_map,
                diags,
            );
        }
    }

    // Check outputs
    for output in &template.outputs {
        let mut refs = HashSet::new();
        collect_all_expr_refs(&output.value, &mut refs);
        for ref_name in refs {
            check_ref(
                ref_name,
                output.key.as_ref(),
                "output",
                names,
                &known_names,
                source_map,
                diags,
            );
        }
    }
}

/// Checks whether a single reference name is valid, emitting an error if not.
fn check_ref(
    ref_name: &str,
    node_name: &str,
    node_kind: &str,
    names: &HashMap<&str, &str>,
    known_names: &[String],
    source_map: Option<&HashMap<String, String>>,
    diags: &mut Diagnostics,
) {
    if ref_name == "pulumi" || names.contains_key(ref_name) {
        return;
    }

    // Build error message with suggestion
    let sorted = diag::sort_by_edit_distance(known_names, ref_name);
    let suggestion = if let Some(best) = sorted.first() {
        let source_info = source_map
            .and_then(|m| m.get(best.as_str()))
            .map(|f| format!(" (defined in {})", f))
            .unwrap_or_default();
        format!("; did you mean '{}'?{}", best, source_info)
    } else {
        String::new()
    };

    let in_source = source_map
        .and_then(|m| m.get(node_name))
        .map(|f| format!(" in {}", f))
        .unwrap_or_default();

    diags.error(
        None,
        format!(
            "resource or variable '{}' referenced by {} '{}'{} is not defined{}",
            ref_name, node_kind, node_name, in_source, suggestion,
        ),
        "",
    );
}

#[allow(clippy::too_many_arguments)]
fn dfs_with_path<'a>(
    node: &'a str,
    deps: &HashMap<&'a str, HashSet<&'a str>>,
    visited: &mut HashSet<&'a str>,
    path: &mut Vec<&'a str>,
    path_set: &mut HashSet<&'a str>,
    order: &mut Vec<String>,
    source_map: Option<&HashMap<String, String>>,
    diags: &mut Diagnostics,
) {
    if visited.contains(node) {
        return;
    }
    if path_set.contains(node) {
        // Found a cycle — reconstruct the cycle path
        let cycle_start = path.iter().position(|&n| n == node).unwrap_or(0);
        let cycle_nodes = &path[cycle_start..];

        let cycle_str = if let Some(sm) = source_map {
            let parts: Vec<String> = cycle_nodes
                .iter()
                .map(|&n| {
                    if let Some(file) = sm.get(n) {
                        format!("{} ({})", n, file)
                    } else {
                        n.to_string()
                    }
                })
                .collect();
            let last = if let Some(file) = sm.get(node) {
                format!("{} ({})", node, file)
            } else {
                node.to_string()
            };
            format!("{} -> {}", parts.join(" -> "), last)
        } else {
            let parts: Vec<&str> = cycle_nodes.to_vec();
            format!("{} -> {}", parts.join(" -> "), node)
        };

        diags.error(None, format!("circular dependency: {}", cycle_str), "");
        return;
    }

    path.push(node);
    path_set.insert(node);

    if let Some(node_deps) = deps.get(node) {
        let mut sorted_deps: Vec<&str> = node_deps.iter().copied().collect();
        sorted_deps.sort();
        for dep in sorted_deps {
            if deps.contains_key(dep) {
                dfs_with_path(dep, deps, visited, path, path_set, order, source_map, diags);
            }
        }
    }

    path.pop();
    path_set.remove(node);
    visited.insert(node);
    order.push(node.to_string());
}

/// Collects ALL `${ref}` root names from an expression, without filtering by known names.
fn collect_all_expr_refs<'a>(expr: &'a Expr<'a>, refs: &mut HashSet<&'a str>) {
    match expr {
        Expr::Symbol(_, access) => {
            let root = access.root_name();
            refs.insert(root);
        }
        Expr::Interpolate(_, parts) => {
            for part in parts {
                if let Some(ref access) = part.value {
                    let root = access.root_name();
                    refs.insert(root);
                }
            }
        }
        Expr::List(_, elements) => {
            for elem in elements {
                collect_all_expr_refs(elem, refs);
            }
        }
        Expr::Object(_, entries) => {
            for entry in entries {
                collect_all_expr_refs(&entry.key, refs);
                collect_all_expr_refs(&entry.value, refs);
            }
        }
        Expr::Invoke(_, invoke) => {
            if let Some(ref args) = invoke.call_args {
                collect_all_expr_refs(args, refs);
            }
            if let Some(ref parent) = invoke.call_opts.parent {
                collect_all_expr_refs(parent, refs);
            }
            if let Some(ref provider) = invoke.call_opts.provider {
                collect_all_expr_refs(provider, refs);
            }
            if let Some(ref depends_on) = invoke.call_opts.depends_on {
                collect_all_expr_refs(depends_on, refs);
            }
        }
        Expr::Join(_, a, b) | Expr::Select(_, a, b) | Expr::Split(_, a, b) => {
            collect_all_expr_refs(a, refs);
            collect_all_expr_refs(b, refs);
        }
        Expr::ToJson(_, inner)
        | Expr::ToBase64(_, inner)
        | Expr::FromBase64(_, inner)
        | Expr::Secret(_, inner)
        | Expr::ReadFile(_, inner)
        | Expr::Abs(_, inner)
        | Expr::Floor(_, inner)
        | Expr::Ceil(_, inner)
        | Expr::Max(_, inner)
        | Expr::Min(_, inner)
        | Expr::StringLen(_, inner)
        | Expr::TimeUtc(_, inner)
        | Expr::TimeUnix(_, inner)
        | Expr::Uuid(_, inner)
        | Expr::RandomString(_, inner)
        | Expr::DateFormat(_, inner)
        | Expr::StringAsset(_, inner)
        | Expr::FileAsset(_, inner)
        | Expr::RemoteAsset(_, inner)
        | Expr::FileArchive(_, inner)
        | Expr::RemoteArchive(_, inner) => {
            collect_all_expr_refs(inner, refs);
        }
        Expr::Substring(_, a, b, c) => {
            collect_all_expr_refs(a, refs);
            collect_all_expr_refs(b, refs);
            collect_all_expr_refs(c, refs);
        }
        Expr::AssetArchive(_, entries) => {
            for (_, v) in entries {
                collect_all_expr_refs(v, refs);
            }
        }
        Expr::Null(_) | Expr::Bool(_, _) | Expr::Number(_, _) | Expr::String(_, _) => {}
    }
}

/// Collects ALL `${ref}` root names from a resource declaration, without filtering.
fn collect_all_resource_refs<'a>(resource: &'a ResourceDecl<'a>, refs: &mut HashSet<&'a str>) {
    match &resource.properties {
        ResourceProperties::Map(props) => {
            for prop in props {
                collect_all_expr_refs(&prop.value, refs);
            }
        }
        ResourceProperties::Expr(expr) => {
            collect_all_expr_refs(expr, refs);
        }
    }

    let opts = &resource.options;
    if let Some(ref expr) = opts.depends_on {
        collect_all_expr_refs(expr, refs);
    }
    if let Some(ref expr) = opts.parent {
        collect_all_expr_refs(expr, refs);
    }
    if let Some(ref expr) = opts.provider {
        collect_all_expr_refs(expr, refs);
    }
    if let Some(ref expr) = opts.providers {
        collect_all_expr_refs(expr, refs);
    }
    if let Some(ref expr) = opts.protect {
        collect_all_expr_refs(expr, refs);
    }
    if let Some(ref expr) = opts.aliases {
        collect_all_expr_refs(expr, refs);
    }
    if let Some(ref expr) = opts.replace_with {
        collect_all_expr_refs(expr, refs);
    }
    if let Some(ref expr) = opts.deleted_with {
        collect_all_expr_refs(expr, refs);
    }
    if let Some(ref get) = resource.get {
        collect_all_expr_refs(&get.id, refs);
        for prop in &get.state {
            collect_all_expr_refs(&prop.value, refs);
        }
    }
}

/// Extracts dependency names from an expression.
pub fn collect_expr_deps<'a>(
    expr: &'a Expr<'a>,
    known_names: &HashMap<&str, &str>,
    deps: &mut HashSet<&'a str>,
) {
    match expr {
        Expr::Symbol(_, access) => {
            let root = access.root_name();
            if known_names.contains_key(root) {
                deps.insert(root);
            }
        }
        Expr::Interpolate(_, parts) => {
            for part in parts {
                if let Some(ref access) = part.value {
                    let root = access.root_name();
                    if known_names.contains_key(root) {
                        deps.insert(root);
                    }
                }
            }
        }
        Expr::List(_, elements) => {
            for elem in elements {
                collect_expr_deps(elem, known_names, deps);
            }
        }
        Expr::Object(_, entries) => {
            for entry in entries {
                collect_expr_deps(&entry.key, known_names, deps);
                collect_expr_deps(&entry.value, known_names, deps);
            }
        }
        Expr::Invoke(_, invoke) => {
            if let Some(ref args) = invoke.call_args {
                collect_expr_deps(args, known_names, deps);
            }
            if let Some(ref parent) = invoke.call_opts.parent {
                collect_expr_deps(parent, known_names, deps);
            }
            if let Some(ref provider) = invoke.call_opts.provider {
                collect_expr_deps(provider, known_names, deps);
            }
            if let Some(ref depends_on) = invoke.call_opts.depends_on {
                collect_expr_deps(depends_on, known_names, deps);
            }
        }
        Expr::Join(_, a, b) | Expr::Select(_, a, b) | Expr::Split(_, a, b) => {
            collect_expr_deps(a, known_names, deps);
            collect_expr_deps(b, known_names, deps);
        }
        Expr::ToJson(_, inner)
        | Expr::ToBase64(_, inner)
        | Expr::FromBase64(_, inner)
        | Expr::Secret(_, inner)
        | Expr::ReadFile(_, inner)
        | Expr::Abs(_, inner)
        | Expr::Floor(_, inner)
        | Expr::Ceil(_, inner)
        | Expr::Max(_, inner)
        | Expr::Min(_, inner)
        | Expr::StringLen(_, inner)
        | Expr::TimeUtc(_, inner)
        | Expr::TimeUnix(_, inner)
        | Expr::Uuid(_, inner)
        | Expr::RandomString(_, inner)
        | Expr::DateFormat(_, inner)
        | Expr::StringAsset(_, inner)
        | Expr::FileAsset(_, inner)
        | Expr::RemoteAsset(_, inner)
        | Expr::FileArchive(_, inner)
        | Expr::RemoteArchive(_, inner) => {
            collect_expr_deps(inner, known_names, deps);
        }
        Expr::Substring(_, a, b, c) => {
            collect_expr_deps(a, known_names, deps);
            collect_expr_deps(b, known_names, deps);
            collect_expr_deps(c, known_names, deps);
        }
        Expr::AssetArchive(_, entries) => {
            for (_, v) in entries {
                collect_expr_deps(v, known_names, deps);
            }
        }
        Expr::Null(_) | Expr::Bool(_, _) | Expr::Number(_, _) | Expr::String(_, _) => {}
    }
}

/// Extracts dependency names from a resource declaration.
fn collect_resource_deps<'a>(
    resource: &'a ResourceDecl<'a>,
    known_names: &HashMap<&str, &str>,
    deps: &mut HashSet<&'a str>,
) {
    // Properties
    match &resource.properties {
        ResourceProperties::Map(props) => {
            for prop in props {
                collect_expr_deps(&prop.value, known_names, deps);
            }
        }
        ResourceProperties::Expr(expr) => {
            collect_expr_deps(expr, known_names, deps);
        }
    }

    // Options
    let opts = &resource.options;
    if let Some(ref expr) = opts.depends_on {
        collect_expr_deps(expr, known_names, deps);
    }
    if let Some(ref expr) = opts.parent {
        collect_expr_deps(expr, known_names, deps);
    }
    if let Some(ref expr) = opts.provider {
        collect_expr_deps(expr, known_names, deps);
    }
    if let Some(ref expr) = opts.providers {
        collect_expr_deps(expr, known_names, deps);
    }
    if let Some(ref expr) = opts.protect {
        collect_expr_deps(expr, known_names, deps);
    }
    if let Some(ref expr) = opts.aliases {
        collect_expr_deps(expr, known_names, deps);
    }
    if let Some(ref expr) = opts.replace_with {
        collect_expr_deps(expr, known_names, deps);
    }
    if let Some(ref expr) = opts.deleted_with {
        collect_expr_deps(expr, known_names, deps);
    }

    // Get resource
    if let Some(ref get) = resource.get {
        collect_expr_deps(&get.id, known_names, deps);
        for prop in &get.state {
            collect_expr_deps(&prop.value, known_names, deps);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::parse::parse_template;

    #[test]
    fn test_simple_ordering() {
        let source = r#"
name: test
runtime: yaml
resources:
  a:
    type: test:Resource
  b:
    type: test:Resource
    properties:
      dep: ${a.id}
"#;
        let (template, parse_diags) = parse_template(source, None);
        assert!(!parse_diags.has_errors());

        let (order, diags) = topological_sort(&template);
        assert!(!diags.has_errors(), "errors: {}", diags);

        let a_pos = order.iter().position(|x| x == "a").unwrap();
        let b_pos = order.iter().position(|x| x == "b").unwrap();
        assert!(a_pos < b_pos, "a should come before b");
    }

    #[test]
    fn test_no_deps() {
        let source = r#"
name: test
runtime: yaml
resources:
  a:
    type: test:Resource
  b:
    type: test:Resource
"#;
        let (template, _) = parse_template(source, None);
        let (order, diags) = topological_sort(&template);
        assert!(!diags.has_errors());
        // 2 resources + "pulumi" node (always present)
        assert_eq!(order.len(), 3);
    }

    #[test]
    fn test_cycle_detection() {
        let source = r#"
name: test
runtime: yaml
resources:
  a:
    type: test:Resource
    properties:
      dep: ${b.id}
  b:
    type: test:Resource
    properties:
      dep: ${a.id}
"#;
        let (template, _) = parse_template(source, None);
        let (_, diags) = topological_sort(&template);
        assert!(diags.has_errors());
    }

    #[test]
    fn test_variable_deps() {
        let source = r#"
name: test
runtime: yaml
variables:
  x: hello
  y: ${x}
"#;
        let (template, _) = parse_template(source, None);
        let (order, diags) = topological_sort(&template);
        assert!(!diags.has_errors(), "errors: {}", diags);

        let x_pos = order.iter().position(|x| x == "x").unwrap();
        let y_pos = order.iter().position(|x| x == "y").unwrap();
        assert!(x_pos < y_pos);
    }

    #[test]
    fn test_duplicate_name() {
        let source = r#"
name: test
runtime: yaml
variables:
  dup: hello
resources:
  dup:
    type: test:Resource
"#;
        let (template, _) = parse_template(source, None);
        let (_, diags) = topological_sort(&template);
        assert!(diags.has_errors());
    }

    #[test]
    fn test_reserved_pulumi_name() {
        let source = r#"
name: test
runtime: yaml
resources:
  pulumi:
    type: test:Resource
"#;
        let (template, _) = parse_template(source, None);
        let (_, diags) = topological_sort(&template);
        assert!(diags.has_errors());
    }

    #[test]
    fn test_config_first() {
        let source = r#"
name: test
runtime: yaml
config:
  myConfig:
    type: string
resources:
  a:
    type: test:Resource
    properties:
      name: ${myConfig}
"#;
        let (template, _) = parse_template(source, None);
        let (order, diags) = topological_sort(&template);
        assert!(!diags.has_errors(), "errors: {}", diags);

        let config_pos = order.iter().position(|x| x == "myConfig").unwrap();
        let a_pos = order.iter().position(|x| x == "a").unwrap();
        assert!(config_pos < a_pos);
    }

    #[test]
    fn test_default_provider_ordering() {
        let source = r#"
name: test
runtime: yaml
resources:
  myProvider:
    type: pulumi:providers:aws
    defaultProvider: true
  myBucket:
    type: aws:s3:Bucket
"#;
        let (template, _) = parse_template(source, None);
        let (order, diags) = topological_sort(&template);
        assert!(!diags.has_errors(), "errors: {}", diags);

        let provider_pos = order.iter().position(|x| x == "myProvider").unwrap();
        let bucket_pos = order.iter().position(|x| x == "myBucket").unwrap();
        assert!(
            provider_pos < bucket_pos,
            "default provider should come before resources"
        );
    }

    // --- New tests for enhanced DAG validation ---

    #[test]
    fn test_missing_reference_detected() {
        let source = r#"
name: test
runtime: yaml
resources:
  bucket:
    type: test:Resource
    properties:
      dep: ${nonexistent.id}
"#;
        let (template, _) = parse_template(source, None);
        let (_, diags) = topological_sort(&template);
        assert!(diags.has_errors());
        let errors: Vec<String> = diags
            .iter()
            .filter(|d| d.is_error())
            .map(|d| d.summary.clone())
            .collect();
        assert!(
            errors
                .iter()
                .any(|e| e.contains("nonexistent") && e.contains("not defined")),
            "expected 'not defined' error for nonexistent, got: {:?}",
            errors
        );
    }

    #[test]
    fn test_missing_reference_with_suggestion() {
        let source = r#"
name: test
runtime: yaml
resources:
  storageBucket:
    type: test:Resource
  tableBucket:
    type: test:Resource
    properties:
      dep: ${strageBucket.id}
"#;
        let (template, _) = parse_template(source, None);
        let (_, diags) = topological_sort(&template);
        assert!(diags.has_errors());
        let errors: Vec<String> = diags
            .iter()
            .filter(|d| d.is_error())
            .map(|d| d.summary.clone())
            .collect();
        assert!(
            errors
                .iter()
                .any(|e| e.contains("did you mean 'storageBucket'?")),
            "expected suggestion for storageBucket, got: {:?}",
            errors
        );
    }

    #[test]
    fn test_cycle_shows_full_path() {
        let source = r#"
name: test
runtime: yaml
resources:
  a:
    type: test:Resource
    properties:
      dep: ${b.id}
  b:
    type: test:Resource
    properties:
      dep: ${a.id}
"#;
        let (template, _) = parse_template(source, None);
        let (_, diags) = topological_sort(&template);
        assert!(diags.has_errors());
        let errors: Vec<String> = diags
            .iter()
            .filter(|d| d.is_error())
            .map(|d| d.summary.clone())
            .collect();
        assert!(
            errors.iter().any(|e| e.contains("circular dependency:")
                && e.contains("->")
                && e.contains("a")
                && e.contains("b")),
            "expected full cycle path in error, got: {:?}",
            errors
        );
    }

    #[test]
    fn test_cycle_with_source_files() {
        let source = r#"
name: test
runtime: yaml
resources:
  a:
    type: test:Resource
    properties:
      dep: ${b.id}
  b:
    type: test:Resource
    properties:
      dep: ${a.id}
"#;
        let (template, _) = parse_template(source, None);
        let mut source_map = HashMap::new();
        source_map.insert("a".to_string(), "Pulumi.a.yaml".to_string());
        source_map.insert("b".to_string(), "Pulumi.b.yaml".to_string());
        let (_, diags) = topological_sort_with_sources(&template, Some(&source_map));
        assert!(diags.has_errors());
        let errors: Vec<String> = diags
            .iter()
            .filter(|d| d.is_error())
            .map(|d| d.summary.clone())
            .collect();
        assert!(
            errors
                .iter()
                .any(|e| e.contains("Pulumi.a.yaml") && e.contains("Pulumi.b.yaml")),
            "expected filenames in cycle error, got: {:?}",
            errors
        );
    }

    #[test]
    fn test_output_missing_reference() {
        let source = r#"
name: test
runtime: yaml
resources:
  bucket:
    type: test:Resource
outputs:
  name: ${unknownBucket.name}
"#;
        let (template, _) = parse_template(source, None);
        let (_, diags) = topological_sort(&template);
        assert!(diags.has_errors());
        let errors: Vec<String> = diags
            .iter()
            .filter(|d| d.is_error())
            .map(|d| d.summary.clone())
            .collect();
        assert!(
            errors
                .iter()
                .any(|e| e.contains("unknownBucket") && e.contains("not defined")),
            "expected missing ref error for output, got: {:?}",
            errors
        );
    }

    #[test]
    fn test_missing_ref_in_depends_on() {
        let source = r#"
name: test
runtime: yaml
resources:
  bucket:
    type: test:Resource
    options:
      dependsOn:
        - ${unknown}
"#;
        let (template, _) = parse_template(source, None);
        let (_, diags) = topological_sort(&template);
        assert!(diags.has_errors());
        let errors: Vec<String> = diags
            .iter()
            .filter(|d| d.is_error())
            .map(|d| d.summary.clone())
            .collect();
        assert!(
            errors
                .iter()
                .any(|e| e.contains("unknown") && e.contains("not defined")),
            "expected missing ref error for dependsOn, got: {:?}",
            errors
        );
    }

    // --- Topological levels tests ---

    #[test]
    fn test_topological_levels_independent() {
        let source = r#"
name: test
runtime: yaml
resources:
  a:
    type: test:Resource
  b:
    type: test:Resource
  c:
    type: test:Resource
"#;
        let (template, _) = parse_template(source, None);
        let (result, diags) = topological_sort_with_deps(&template, None);
        assert!(!diags.has_errors());

        let levels = topological_levels(&result.order, &result.deps);
        // All independent resources + "pulumi" should be at the same level
        assert_eq!(levels.len(), 1);
        assert_eq!(levels[0].len(), 4); // a, b, c + pulumi
    }

    #[test]
    fn test_topological_levels_chain() {
        let source = r#"
name: test
runtime: yaml
resources:
  a:
    type: test:Resource
  b:
    type: test:Resource
    properties:
      dep: ${a.id}
  c:
    type: test:Resource
    properties:
      dep: ${b.id}
"#;
        let (template, _) = parse_template(source, None);
        let (result, diags) = topological_sort_with_deps(&template, None);
        assert!(!diags.has_errors());

        let levels = topological_levels(&result.order, &result.deps);
        assert_eq!(levels.len(), 3);
        assert_eq!(levels[0], vec!["a", "pulumi"]);
        assert_eq!(levels[1], vec!["b"]);
        assert_eq!(levels[2], vec!["c"]);
    }

    #[test]
    fn test_topological_levels_diamond() {
        let source = r#"
name: test
runtime: yaml
resources:
  a:
    type: test:Resource
  b:
    type: test:Resource
    properties:
      dep: ${a.id}
  c:
    type: test:Resource
    properties:
      dep: ${a.id}
  d:
    type: test:Resource
    properties:
      depB: ${b.id}
      depC: ${c.id}
"#;
        let (template, _) = parse_template(source, None);
        let (result, diags) = topological_sort_with_deps(&template, None);
        assert!(!diags.has_errors());

        let levels = topological_levels(&result.order, &result.deps);
        assert_eq!(levels.len(), 3);
        assert_eq!(levels[0], vec!["a", "pulumi"]);
        assert_eq!(levels[1], vec!["b", "c"]); // b and c at same level
        assert_eq!(levels[2], vec!["d"]);
    }

    #[test]
    fn test_topological_levels_config_and_variables() {
        let source = r#"
name: test
runtime: yaml
config:
  region:
    type: string
variables:
  prefix: hello
resources:
  bucket:
    type: test:Resource
    properties:
      name: ${prefix}
      region: ${region}
"#;
        let (template, _) = parse_template(source, None);
        let (result, diags) = topological_sort_with_deps(&template, None);
        assert!(!diags.has_errors());

        let levels = topological_levels(&result.order, &result.deps);
        // Level 0: config (region) + variable (prefix) — no deps
        // Level 1: bucket — depends on both
        assert!(levels.len() >= 2);
        assert!(levels[0].contains(&"region".to_string()));
        assert!(levels[0].contains(&"prefix".to_string()));
        assert!(levels.last().unwrap().contains(&"bucket".to_string()));
    }
}

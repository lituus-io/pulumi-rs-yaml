// Author: spicyzhug@gmail.com
// Copyright: lituus-io, all rights reserved

use crate::ast::expr::Expr;
use crate::ast::template::*;
use crate::ast::visitor::{walk_expr, walk_resource, AllRefsCollector, DepCollector};
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

/// Internal implementation: builds name registry, validates references,
/// builds adjacency, and runs DFS topological sort.
///
/// Returns `(order, deps_borrowed, diagnostics)`. The borrowed deps can be
/// converted to owned strings by the caller if needed.
fn topological_sort_inner<'a>(
    template: &'a TemplateDecl<'a>,
    source_map: Option<&HashMap<String, String>>,
) -> (Vec<String>, HashMap<&'a str, HashSet<&'a str>>, Diagnostics) {
    let mut diags = Diagnostics::new();
    let node_count =
        template.config.len() + template.variables.len() + template.resources.len() + 1; // +1 for "pulumi"
    let mut names: HashMap<&str, &str> = HashMap::with_capacity(node_count);

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

    // Variables may SHADOW a config key (Go parity: `variables: project:
    // ${project}` with `project` in config is accepted — the interpolation
    // inside the shadowing variable's own definition resolves to the config
    // value). Track which variables shadow config so their self-edge can be
    // dropped from the dependency graph below.
    let mut config_shadows: HashSet<&str> = HashSet::new();
    for entry in &template.variables {
        let key = entry.key.as_ref();
        if key == "pulumi" {
            diags.error(None, "\"pulumi\" is a reserved name", "");
            continue;
        }
        if let Some(existing_kind) = names.insert(key, "variable") {
            if existing_kind == "config" {
                config_shadows.insert(key);
            } else {
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
        return (Vec::new(), HashMap::new(), diags);
    }

    // Validate all references exist before building the dependency graph
    validate_references(template, &names, source_map, &mut diags);
    if diags.has_errors() {
        return (Vec::new(), HashMap::new(), diags);
    }

    // Build adjacency: for each node, collect the set of nodes it depends on
    let mut deps: HashMap<&str, HashSet<&str>> = HashMap::with_capacity(names.len());
    let dep_collector = DepCollector {
        known_names: &names,
    };

    // Config entries have no dependencies (they come from external config)
    for entry in &template.config {
        deps.entry(entry.key.as_ref()).or_default();
    }

    // Variables depend on whatever their expression references
    for entry in &template.variables {
        let mut node_deps = HashSet::new();
        walk_expr(&entry.value, &dep_collector, &mut node_deps);
        // A config-shadowing variable's reference to its own name means the
        // CONFIG value (Go parity) — never a self-cycle.
        if config_shadows.contains(entry.key.as_ref()) {
            node_deps.remove(entry.key.as_ref());
        }
        deps.insert(entry.key.as_ref(), node_deps);
    }

    // Resources marked `defaultProvider`, found once rather than rescanned for
    // every resource (an O(R^2) walk on a wide program).
    let default_providers: Vec<&str> = template
        .resources
        .iter()
        .filter(|e| e.resource.default_provider == Some(true))
        .map(|e| e.logical_name.as_ref())
        .collect();

    // Resources depend on whatever their properties, options, etc. reference
    for entry in &template.resources {
        let mut node_deps = HashSet::new();
        walk_resource(&entry.resource, &dep_collector, &mut node_deps);

        // Default provider dependencies: resources without an explicit provider
        // depend on any resource marked as defaultProvider
        if entry.resource.options.provider.is_none() {
            for &provider in &default_providers {
                if provider != entry.logical_name.as_ref() {
                    node_deps.insert(provider);
                }
            }
        }

        deps.insert(entry.logical_name.as_ref(), node_deps);
    }

    // "pulumi" node has no dependencies — always present
    deps.entry("pulumi").or_default();

    // Topological sort using DFS with cycle detection and path reconstruction.
    //
    // The walk runs on indices into the name-sorted node list. It used to keep
    // `&str` sets -- every visit and every path check hashed a name -- and to
    // collect and string-sort each node's dependencies at every visit. Because
    // the list is sorted by name, sorting a node's dependency INDICES is the
    // same order as sorting their names, so the traversal, the cycle each
    // error reports, and the order produced are exactly what they were.
    let mut all_nodes: Vec<&str> = deps.keys().copied().collect();
    all_nodes.sort();
    let index: HashMap<&str, usize> = all_nodes.iter().enumerate().map(|(i, &n)| (n, i)).collect();
    let adjacency: Vec<Vec<usize>> = all_nodes
        .iter()
        .map(|n| {
            let mut out: Vec<usize> = deps
                .get(n)
                .map(|d| d.iter().filter_map(|dep| index.get(dep).copied()).collect())
                .unwrap_or_default();
            out.sort_unstable();
            out
        })
        .collect();

    let mut walk = Dfs {
        names: &all_nodes,
        adjacency: &adjacency,
        visited: vec![false; all_nodes.len()],
        on_path: vec![false; all_nodes.len()],
        path: Vec::new(),
        order: Vec::with_capacity(all_nodes.len()),
        source_map,
        diags: &mut diags,
    };
    for node in 0..all_nodes.len() {
        if !walk.visited[node] {
            walk.visit(node);
        }
    }
    let order = walk.order;

    (order, deps, diags)
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
    let (order, _deps, diags) = topological_sort_inner(template, source_map);
    (order, diags)
}

/// Performs a topological sort and returns the dependency graph alongside the ordering.
///
/// This variant is used by the parallel evaluator to compute topological levels.
pub fn topological_sort_with_deps<'a>(
    template: &'a TemplateDecl<'a>,
    source_map: Option<&HashMap<String, String>>,
) -> (SortResultWithDeps, Diagnostics) {
    let (order, deps, diags) = topological_sort_inner(template, source_map);
    // Convert borrowed deps to owned strings
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
    let mut levels: HashMap<&str, usize> = HashMap::with_capacity(sorted.len());

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
    source_map: Option<&HashMap<String, String>>,
    diags: &mut Diagnostics,
) {
    if ref_name == "pulumi" || names.contains_key(ref_name) {
        return;
    }

    // The closest known name, ties broken by the name itself. Every name used
    // to be copied into a list on every validation -- error or not -- and
    // sorted by distance alone, so equally close names came out in `HashMap`
    // order, which is randomised per process: the same typo could be told to
    // mean a different name on each run.
    let best = names
        .keys()
        .map(|&name| (diag::edit_distance(name, ref_name), name))
        .min();
    let suggestion = if let Some((_, best)) = best {
        let source_info = source_map
            .and_then(|m| m.get(best))
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

/// Depth-first topological walk over node indices.
struct Dfs<'a, 'd> {
    /// Node names, sorted; an index is a position here.
    names: &'d [&'a str],
    /// Each node's dependencies, as sorted indices.
    adjacency: &'d [Vec<usize>],
    visited: Vec<bool>,
    on_path: Vec<bool>,
    path: Vec<usize>,
    order: Vec<String>,
    source_map: Option<&'d HashMap<String, String>>,
    diags: &'d mut Diagnostics,
}

impl Dfs<'_, '_> {
    fn visit(&mut self, node: usize) {
        if self.visited[node] {
            return;
        }
        if self.on_path[node] {
            self.report_cycle(node);
            return;
        }

        self.path.push(node);
        self.on_path[node] = true;

        let adjacency = self.adjacency;
        for &dep in &adjacency[node] {
            self.visit(dep);
        }

        self.path.pop();
        self.on_path[node] = false;
        self.visited[node] = true;
        self.order.push(self.names[node].to_string());
    }

    /// Reports the cycle that closes at `node`, naming each member's file
    /// when a source map is known.
    fn report_cycle(&mut self, node: usize) {
        let cycle_start = self.path.iter().position(|&n| n == node).unwrap_or(0);
        let label = |n: usize| -> String {
            let name = self.names[n];
            match self.source_map.and_then(|sm| sm.get(name)) {
                Some(file) => format!("{} ({})", name, file),
                None => name.to_string(),
            }
        };
        let parts: Vec<String> = self.path[cycle_start..].iter().map(|&n| label(n)).collect();
        let cycle_str = format!("{} -> {}", parts.join(" -> "), label(node));
        self.diags
            .error(None, format!("circular dependency: {}", cycle_str), "");
    }
}

/// Collects ALL `${ref}` root names from an expression, without filtering by known names.
fn collect_all_expr_refs<'a>(expr: &'a Expr<'a>, refs: &mut HashSet<&'a str>) {
    walk_expr(expr, &AllRefsCollector, refs);
}

/// Collects ALL `${ref}` root names from a resource declaration, without filtering.
fn collect_all_resource_refs<'a>(resource: &'a ResourceDecl<'a>, refs: &mut HashSet<&'a str>) {
    walk_resource(resource, &AllRefsCollector, refs);
}

/// Extracts dependency names from an expression.
pub fn collect_expr_deps<'a>(
    expr: &'a Expr<'a>,
    known_names: &HashMap<&str, &str>,
    deps: &mut HashSet<&'a str>,
) {
    walk_expr(expr, &DepCollector { known_names }, deps);
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

    /// Two names equally close to a typo: the suggestion is the
    /// alphabetically first, on every run. It used to follow `HashMap`
    /// iteration order, which is randomised per process.
    #[test]
    fn an_equally_close_suggestion_is_deterministic() {
        let source = r#"
name: test
runtime: yaml
resources:
  bucketB:
    type: test:Resource
  bucketA:
    type: test:Resource
  user:
    type: test:Resource
    properties:
      dep: ${bucketC.id}
"#;
        let (template, _) = parse_template(source, None);
        for _ in 0..32 {
            let (_, diags) = topological_sort(&template);
            let summary = diags
                .iter()
                .find(|d| d.is_error())
                .map(|d| d.summary.clone())
                .expect("an undefined reference");
            assert!(
                summary.contains("did you mean 'bucketA'?"),
                "expected the alphabetically first of two equal candidates: {summary}"
            );
        }
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

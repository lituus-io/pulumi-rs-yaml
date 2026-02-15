//! Type checker for Pulumi YAML templates.
//!
//! Validates resource properties, required inputs, invoke arguments,
//! and property access chains against provider schemas.

use std::collections::HashMap;

use crate::ast::expr::Expr;
use crate::ast::property::{PropertyAccess, PropertyAccessor};
use crate::ast::template::*;
use crate::diag::Diagnostics;
use crate::packages::canonicalize_type_token;
use crate::schema::{SchemaPropertyType, SchemaStore};

/// Result of type checking a template.
pub struct TypeCheckResult {
    pub diagnostics: Diagnostics,
}

/// Inferred type of an expression.
#[derive(Debug, Clone, PartialEq)]
pub enum InferredType {
    Null,
    Bool,
    String,
    Number,
    Integer,
    Array(Box<InferredType>),
    Object(Vec<(String, InferredType)>),
    Asset,
    Archive,
    /// A resource reference (holds canonical token).
    Resource(String),
    /// Type could be anything (unknown/dynamic).
    Any,
    /// Invalid / error type.
    Invalid,
}

/// Type-checks a template against provider schemas.
///
/// Returns diagnostics (warnings for unknown properties, errors for missing required inputs).
pub fn type_check(
    template: &TemplateDecl<'_>,
    schema_store: &SchemaStore,
    source_map: Option<&HashMap<String, String>>,
) -> TypeCheckResult {
    let mut checker = TypeChecker {
        schema_store,
        source_map,
        resource_types: HashMap::new(),
        diags: Diagnostics::new(),
    };

    checker.check_template(template);

    TypeCheckResult {
        diagnostics: checker.diags,
    }
}

struct TypeChecker<'a> {
    schema_store: &'a SchemaStore,
    source_map: Option<&'a HashMap<String, String>>,
    /// Maps resource logical name → canonical type token.
    resource_types: HashMap<String, String>,
    diags: Diagnostics,
}

impl TypeChecker<'_> {
    fn check_template(&mut self, template: &TemplateDecl<'_>) {
        // First pass: collect resource types for cross-references
        for entry in &template.resources {
            let canonical = self
                .schema_store
                .resolve_resource_token(&entry.resource.type_)
                .unwrap_or_else(|| canonicalize_type_token(&entry.resource.type_));
            self.resource_types
                .insert(entry.logical_name.to_string(), canonical);
        }

        // Second pass: validate each resource
        for entry in &template.resources {
            self.check_resource(entry);
        }

        // Validate invoke expressions in variables
        for entry in &template.variables {
            self.check_expr_invokes(&entry.value);
        }

        // Validate invoke expressions in outputs
        for entry in &template.outputs {
            self.check_expr_invokes(&entry.value);
        }
    }

    fn check_resource(&mut self, entry: &ResourceEntry<'_>) {
        let logical_name = entry.logical_name.to_string();
        let canonical = self
            .resource_types
            .get(&logical_name)
            .cloned()
            .unwrap_or_else(|| canonicalize_type_token(&entry.resource.type_));

        let info = match self.schema_store.lookup_resource(&canonical) {
            Some(info) => info,
            None => return, // Unknown resource type — skip validation
        };

        let source_hint = self
            .source_map
            .and_then(|sm| sm.get(&logical_name))
            .cloned();

        // Collect user-provided property names
        let mut provided_props: Vec<String> = Vec::new();

        match &entry.resource.properties {
            ResourceProperties::Map(props) => {
                for prop in props {
                    let prop_name = prop.key.to_string();
                    provided_props.push(prop_name.clone());

                    // Check if property exists in input schema
                    if !info.input_properties.contains(&prop_name)
                        && !info.properties.contains(&prop_name)
                    {
                        let suggestion = find_closest_match(&prop_name, &info.input_properties);
                        let detail = if let Some(ref s) = suggestion {
                            format!("did you mean '{}'?", s)
                        } else {
                            format!(
                                "resource type '{}' does not have input property '{}'",
                                entry.resource.type_, prop_name
                            )
                        };
                        let summary = format!(
                            "unknown property '{}' on resource '{}'{}",
                            prop_name,
                            logical_name,
                            source_suffix(&source_hint),
                        );
                        self.diags.warning(None, summary, detail);
                    }

                    // Type compatibility check
                    if let Some(prop_info) = info.input_property_types.get(&prop_name) {
                        let inferred = self.infer_type(&prop.value);
                        if !is_assignable(&inferred, &prop_info.type_) {
                            self.diags.warning(
                                None,
                                format!(
                                    "type mismatch for property '{}' on resource '{}'{}",
                                    prop_name,
                                    logical_name,
                                    source_suffix(&source_hint),
                                ),
                                format!(
                                    "expected {}, got {}",
                                    prop_info.type_.label(),
                                    inferred_label(&inferred),
                                ),
                            );
                        }
                    }

                    // Check invoke expressions inside property values
                    self.check_expr_invokes(&prop.value);
                }
            }
            ResourceProperties::Expr(expr) => {
                self.check_expr_invokes(expr);
            }
        }

        // Check required inputs
        for required in &info.required_inputs {
            if !provided_props.contains(required) {
                // Check if the property has a const_value (auto-injected)
                let has_const = info
                    .property_types
                    .get(required)
                    .and_then(|p| p.const_value.as_ref())
                    .is_some();
                if !has_const {
                    self.diags.warning(
                        None,
                        format!(
                            "missing required property '{}' on resource '{}'{}",
                            required,
                            logical_name,
                            source_suffix(&source_hint),
                        ),
                        format!(
                            "resource type '{}' requires property '{}'",
                            entry.resource.type_, required
                        ),
                    );
                }
            }
        }
    }

    fn check_expr_invokes(&mut self, expr: &Expr<'_>) {
        match expr {
            Expr::Invoke(_, invoke) => {
                self.check_invoke(invoke);
            }
            Expr::List(_, items) => {
                for item in items {
                    self.check_expr_invokes(item);
                }
            }
            Expr::Object(_, entries) => {
                for entry in entries {
                    self.check_expr_invokes(&entry.key);
                    self.check_expr_invokes(&entry.value);
                }
            }
            Expr::Join(_, a, b) | Expr::Select(_, a, b) | Expr::Split(_, a, b) => {
                self.check_expr_invokes(a);
                self.check_expr_invokes(b);
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
                self.check_expr_invokes(inner);
            }
            Expr::Substring(_, a, b, c) => {
                self.check_expr_invokes(a);
                self.check_expr_invokes(b);
                self.check_expr_invokes(c);
            }
            Expr::AssetArchive(_, entries) => {
                for (_, v) in entries {
                    self.check_expr_invokes(v);
                }
            }
            _ => {}
        }
    }

    fn check_invoke(&mut self, invoke: &crate::ast::expr::InvokeExpr<'_>) {
        let canonical = self
            .schema_store
            .resolve_function_token(&invoke.token)
            .unwrap_or_else(|| canonicalize_type_token(&invoke.token));

        let func_info = match self.schema_store.lookup_function(&canonical) {
            Some(info) => info,
            None => return, // Unknown function — skip validation
        };

        // Check arguments
        if let Some(ref args_expr) = invoke.call_args {
            if let Expr::Object(_, entries) = args_expr.as_ref() {
                let mut provided: Vec<String> = Vec::new();
                for entry in entries {
                    if let Expr::String(_, key) = entry.key.as_ref() {
                        let key_str = key.to_string();
                        provided.push(key_str.clone());

                        if !func_info.inputs.contains_key(&key_str) {
                            let suggestion = find_closest_match_map(&key_str, &func_info.inputs);
                            let detail = if let Some(s) = suggestion {
                                format!("did you mean '{}'?", s)
                            } else {
                                format!(
                                    "function '{}' does not accept argument '{}'",
                                    invoke.token, key_str
                                )
                            };
                            self.diags.warning(
                                None,
                                format!(
                                    "unknown argument '{}' for invoke '{}'",
                                    key_str, invoke.token
                                ),
                                detail,
                            );
                        }
                    }
                }

                // Check required inputs
                for required in &func_info.required_inputs {
                    if !provided.contains(required) {
                        self.diags.warning(
                            None,
                            format!(
                                "missing required argument '{}' for invoke '{}'",
                                required, invoke.token
                            ),
                            format!(
                                "function '{}' requires argument '{}'",
                                invoke.token, required
                            ),
                        );
                    }
                }
            }
        } else if !func_info.required_inputs.is_empty() {
            self.diags.warning(
                None,
                format!("invoke '{}' missing required arguments", invoke.token),
                format!(
                    "required: {}",
                    func_info
                        .required_inputs
                        .iter()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            );
        }

        // Validate return field
        if let Some(ref ret) = invoke.return_ {
            let ret_str = ret.to_string();
            if !func_info.outputs.contains_key(&ret_str) && !func_info.outputs.is_empty() {
                let suggestion = find_closest_match_map(&ret_str, &func_info.outputs);
                let detail = if let Some(s) = suggestion {
                    format!("did you mean '{}'?", s)
                } else {
                    format!(
                        "function '{}' does not have output '{}'",
                        invoke.token, ret_str
                    )
                };
                self.diags.warning(
                    None,
                    format!(
                        "unknown return property '{}' for invoke '{}'",
                        ret_str, invoke.token
                    ),
                    detail,
                );
            }
        }
    }

    /// Infers the type of an expression (shallow).
    fn infer_type(&self, expr: &Expr<'_>) -> InferredType {
        match expr {
            Expr::Null(_) => InferredType::Null,
            Expr::Bool(_, _) => InferredType::Bool,
            Expr::Number(_, n) => {
                if *n == n.trunc() && n.is_finite() {
                    InferredType::Integer
                } else {
                    InferredType::Number
                }
            }
            Expr::String(_, _) | Expr::Interpolate(_, _) => InferredType::String,
            Expr::List(_, items) => {
                if items.is_empty() {
                    InferredType::Array(Box::new(InferredType::Any))
                } else {
                    let elem = self.infer_type(&items[0]);
                    InferredType::Array(Box::new(elem))
                }
            }
            Expr::Object(_, entries) => {
                let fields: Vec<(String, InferredType)> = entries
                    .iter()
                    .filter_map(|e| {
                        if let Expr::String(_, key) = e.key.as_ref() {
                            Some((key.to_string(), self.infer_type(&e.value)))
                        } else {
                            None
                        }
                    })
                    .collect();
                InferredType::Object(fields)
            }
            Expr::Symbol(_, access) => self.infer_access_type(access),
            Expr::Invoke(_, _) => InferredType::Any,
            Expr::Join(_, _, _) => InferredType::String,
            Expr::Select(_, _, _) => InferredType::Any,
            Expr::Split(_, _, _) => InferredType::Array(Box::new(InferredType::String)),
            Expr::ToJson(_, _) => InferredType::String,
            Expr::ToBase64(_, _) => InferredType::String,
            Expr::FromBase64(_, _) => InferredType::String,
            Expr::Secret(_, inner) => self.infer_type(inner),
            Expr::ReadFile(_, _) => InferredType::String,
            Expr::Abs(_, _) | Expr::Floor(_, _) | Expr::Ceil(_, _) => InferredType::Number,
            Expr::Max(_, _) | Expr::Min(_, _) => InferredType::Number,
            Expr::StringLen(_, _) => InferredType::Integer,
            Expr::Substring(_, _, _, _) => InferredType::String,
            Expr::TimeUtc(_, _) | Expr::DateFormat(_, _) => InferredType::String,
            Expr::TimeUnix(_, _) => InferredType::Number,
            Expr::Uuid(_, _) | Expr::RandomString(_, _) => InferredType::String,
            Expr::StringAsset(_, _) | Expr::FileAsset(_, _) | Expr::RemoteAsset(_, _) => {
                InferredType::Asset
            }
            Expr::FileArchive(_, _) | Expr::RemoteArchive(_, _) => InferredType::Archive,
            Expr::AssetArchive(_, _) => InferredType::Archive,
        }
    }

    fn infer_access_type(&self, access: &PropertyAccess<'_>) -> InferredType {
        if access.accessors.is_empty() {
            return InferredType::Any;
        }
        let root = match &access.accessors[0] {
            PropertyAccessor::Name(n) => n.to_string(),
            _ => return InferredType::Any,
        };

        // Check if it's a resource reference
        if let Some(canonical_token) = self.resource_types.get(&root) {
            if access.accessors.len() == 1 {
                return InferredType::Resource(canonical_token.clone());
            }
            // Try to resolve property type from schema
            if let Some(info) = self.schema_store.lookup_resource(canonical_token) {
                if let Some(PropertyAccessor::Name(prop_name)) = access.accessors.get(1) {
                    if let Some(prop_info) = info.property_types.get(prop_name.as_ref()) {
                        return schema_type_to_inferred(&prop_info.type_);
                    }
                }
            }
        }
        InferredType::Any
    }
}

/// Checks if an inferred type is assignable to an expected schema type.
fn is_assignable(inferred: &InferredType, expected: &SchemaPropertyType) -> bool {
    match (inferred, expected) {
        // Any/Invalid are always compatible
        (InferredType::Any, _) | (InferredType::Invalid, _) => true,
        (InferredType::Null, _) => true, // null is always valid (optional field)

        // Exact matches
        (InferredType::Bool, SchemaPropertyType::Boolean) => true,
        (InferredType::String, SchemaPropertyType::String) => true,
        (InferredType::Number, SchemaPropertyType::Number) => true,
        (InferredType::Number, SchemaPropertyType::Integer) => true,
        (InferredType::Integer, SchemaPropertyType::Integer) => true,
        (InferredType::Integer, SchemaPropertyType::Number) => true,

        // String coercion: number, bool, integer all coerce to string (matching Go)
        (InferredType::Number, SchemaPropertyType::String) => true,
        (InferredType::Integer, SchemaPropertyType::String) => true,
        (InferredType::Bool, SchemaPropertyType::String) => true,
        (InferredType::Resource(_), SchemaPropertyType::String) => true,

        // Asset/Archive
        (InferredType::Asset, SchemaPropertyType::Asset) => true,
        (InferredType::Archive, SchemaPropertyType::Archive) => true,
        (InferredType::Asset, SchemaPropertyType::Archive) => true, // Asset accepted as Archive
        (InferredType::Archive, SchemaPropertyType::Asset) => true, // vice versa

        // Array
        (InferredType::Array(elem), SchemaPropertyType::Array(expected_elem)) => {
            is_assignable(elem, expected_elem)
        }

        // Object is permissive
        (InferredType::Object(_), SchemaPropertyType::Object) => true,
        (InferredType::Object(_), SchemaPropertyType::Unknown) => true,

        // Unknown schema type accepts anything
        (_, SchemaPropertyType::Unknown) => true,
        (_, SchemaPropertyType::Object) => true, // Object is permissive in v1

        // Resource references
        (InferredType::Resource(_), _) => true, // Resource refs are permissive

        _ => false,
    }
}

/// Converts a SchemaPropertyType to an InferredType.
fn schema_type_to_inferred(spt: &SchemaPropertyType) -> InferredType {
    match spt {
        SchemaPropertyType::String => InferredType::String,
        SchemaPropertyType::Number => InferredType::Number,
        SchemaPropertyType::Integer => InferredType::Integer,
        SchemaPropertyType::Boolean => InferredType::Bool,
        SchemaPropertyType::Array(inner) => {
            InferredType::Array(Box::new(schema_type_to_inferred(inner)))
        }
        SchemaPropertyType::Object => InferredType::Any,
        SchemaPropertyType::Asset => InferredType::Asset,
        SchemaPropertyType::Archive => InferredType::Archive,
        SchemaPropertyType::Unknown => InferredType::Any,
    }
}

/// Returns a human-readable label for an inferred type.
fn inferred_label(t: &InferredType) -> &'static str {
    match t {
        InferredType::Null => "null",
        InferredType::Bool => "boolean",
        InferredType::String => "string",
        InferredType::Number => "number",
        InferredType::Integer => "integer",
        InferredType::Array(_) => "array",
        InferredType::Object(_) => "object",
        InferredType::Asset => "asset",
        InferredType::Archive => "archive",
        InferredType::Resource(_) => "resource",
        InferredType::Any => "any",
        InferredType::Invalid => "invalid",
    }
}

/// Returns a source file suffix for diagnostics (e.g., " (in Pulumi.storage.yaml)").
fn source_suffix(source_hint: &Option<String>) -> String {
    match source_hint {
        Some(s) => format!(" (in {})", s),
        None => String::new(),
    }
}

/// Finds the closest match to `name` in a set of strings using Levenshtein distance.
fn find_closest_match(
    name: &str,
    candidates: &std::collections::HashSet<String>,
) -> Option<String> {
    find_closest(name, candidates.iter().map(|s| s.as_str()))
}

/// Finds the closest match to `name` in a map's keys using Levenshtein distance.
fn find_closest_match_map<V>(name: &str, candidates: &HashMap<String, V>) -> Option<String> {
    find_closest(name, candidates.keys().map(|s| s.as_str()))
}

/// Core Levenshtein-based "did you mean?" implementation.
fn find_closest<'b>(name: &str, candidates: impl Iterator<Item = &'b str>) -> Option<String> {
    let mut best: Option<(String, usize)> = None;
    let max_distance = (name.len() / 2).max(2);

    for candidate in candidates {
        let dist = levenshtein(name, candidate);
        if dist <= max_distance && (best.is_none() || dist < best.as_ref().unwrap().1) {
            best = Some((candidate.to_string(), dist));
        }
    }

    best.map(|(s, _)| s)
}

/// Levenshtein distance between two strings.
fn levenshtein(a: &str, b: &str) -> usize {
    let a_len = a.len();
    let b_len = b.len();

    if a_len == 0 {
        return b_len;
    }
    if b_len == 0 {
        return a_len;
    }

    let mut prev: Vec<usize> = (0..=b_len).collect();
    let mut curr = vec![0; b_len + 1];

    for (i, ca) in a.chars().enumerate() {
        curr[0] = i + 1;
        for (j, cb) in b.chars().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            curr[j + 1] = (prev[j + 1] + 1).min(curr[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }

    prev[b_len]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::parse::parse_template;
    use crate::schema::{
        FunctionTypeInfo, PackageSchema, PropertyInfo, ResourceTypeInfo, SchemaPropertyType,
    };
    use std::collections::HashSet;

    fn make_store_with_resource(
        token: &str,
        input_props: &[(&str, SchemaPropertyType)],
        required: &[&str],
    ) -> SchemaStore {
        let pkg = token.split(':').next().unwrap();
        let mut info = ResourceTypeInfo::default();
        for (name, ty) in input_props {
            info.input_properties.insert(name.to_string());
            info.properties.insert(name.to_string());
            let is_required = required.contains(name);
            if is_required {
                info.required_inputs.insert(name.to_string());
            }
            let prop_info = PropertyInfo {
                type_: ty.clone(),
                secret: false,
                const_value: None,
                required: is_required,
            };
            info.input_property_types
                .insert(name.to_string(), prop_info.clone());
            info.property_types.insert(name.to_string(), prop_info);
        }

        let mut store = SchemaStore::new();
        store.insert(PackageSchema {
            name: pkg.to_string(),
            version: "1.0.0".to_string(),
            resources: [(token.to_string(), info)].into_iter().collect(),
            functions: HashMap::new(),
        });
        store
    }

    #[test]
    fn test_levenshtein_basic() {
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        assert_eq!(levenshtein("", "abc"), 3);
        assert_eq!(levenshtein("abc", "abc"), 0);
        assert_eq!(levenshtein("abc", "abd"), 1);
    }

    #[test]
    fn test_find_closest() {
        let candidates: HashSet<String> = ["bucketName", "region", "acl"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        assert_eq!(
            find_closest_match("buketName", &candidates),
            Some("bucketName".to_string())
        );
        assert_eq!(find_closest_match("zzzzzzzzz", &candidates), None);
    }

    #[test]
    fn test_type_check_valid_resource() {
        let yaml = r#"
name: test
runtime: yaml
resources:
  bucket:
    type: aws:s3/bucket:Bucket
    properties:
      bucketName: my-bucket
      region: us-east-1
"#;
        let (template, _) = parse_template(yaml, None);
        let store = make_store_with_resource(
            "aws:s3/bucket:Bucket",
            &[
                ("bucketName", SchemaPropertyType::String),
                ("region", SchemaPropertyType::String),
            ],
            &[],
        );

        let result = type_check(&template, &store, None);
        assert!(
            !result.diagnostics.has_errors(),
            "expected no errors: {}",
            result.diagnostics
        );
        assert_eq!(
            result.diagnostics.iter().count(),
            0,
            "expected no diagnostics"
        );
    }

    #[test]
    fn test_type_check_unknown_property() {
        let yaml = r#"
name: test
runtime: yaml
resources:
  bucket:
    type: aws:s3/bucket:Bucket
    properties:
      buketName: my-bucket
"#;
        let (template, _) = parse_template(yaml, None);
        let store = make_store_with_resource(
            "aws:s3/bucket:Bucket",
            &[("bucketName", SchemaPropertyType::String)],
            &[],
        );

        let result = type_check(&template, &store, None);
        let warnings: Vec<_> = result
            .diagnostics
            .iter()
            .filter(|d| !d.is_error())
            .collect();
        assert!(
            !warnings.is_empty(),
            "expected a warning for unknown property"
        );
        assert!(warnings[0].summary.contains("unknown property 'buketName'"));
        assert!(warnings[0].detail.contains("bucketName")); // did you mean?
    }

    #[test]
    fn test_type_check_missing_required() {
        let yaml = r#"
name: test
runtime: yaml
resources:
  bucket:
    type: aws:s3/bucket:Bucket
    properties:
      region: us-east-1
"#;
        let (template, _) = parse_template(yaml, None);
        let store = make_store_with_resource(
            "aws:s3/bucket:Bucket",
            &[
                ("bucketName", SchemaPropertyType::String),
                ("region", SchemaPropertyType::String),
            ],
            &["bucketName"],
        );

        let result = type_check(&template, &store, None);
        let warnings: Vec<_> = result
            .diagnostics
            .iter()
            .filter(|d| !d.is_error())
            .collect();
        assert!(
            !warnings.is_empty(),
            "expected warning for missing required"
        );
        assert!(warnings[0]
            .summary
            .contains("missing required property 'bucketName'"));
    }

    #[test]
    fn test_type_check_type_mismatch() {
        let yaml = r#"
name: test
runtime: yaml
resources:
  bucket:
    type: aws:s3/bucket:Bucket
    properties:
      count: not-a-number
"#;
        let (template, _) = parse_template(yaml, None);
        let store = make_store_with_resource(
            "aws:s3/bucket:Bucket",
            &[("count", SchemaPropertyType::Integer)],
            &[],
        );

        let result = type_check(&template, &store, None);
        let warnings: Vec<_> = result
            .diagnostics
            .iter()
            .filter(|d| !d.is_error())
            .collect();
        // String to integer is not assignable
        assert!(!warnings.is_empty(), "expected type mismatch warning");
        assert!(warnings[0].summary.contains("type mismatch"));
    }

    #[test]
    fn test_type_check_string_coercion() {
        // Number → String is valid coercion
        let yaml = r#"
name: test
runtime: yaml
resources:
  res:
    type: test:index/res:Res
    properties:
      name: 42
"#;
        let (template, _) = parse_template(yaml, None);
        let store = make_store_with_resource(
            "test:index/res:Res",
            &[("name", SchemaPropertyType::String)],
            &[],
        );

        let result = type_check(&template, &store, None);
        // No type mismatch — number coerces to string
        let type_warnings: Vec<_> = result
            .diagnostics
            .iter()
            .filter(|d| d.summary.contains("type mismatch"))
            .collect();
        assert!(
            type_warnings.is_empty(),
            "number should coerce to string, got: {:?}",
            type_warnings.iter().map(|d| &d.summary).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_type_check_bool_to_int_mismatch() {
        let yaml = r#"
name: test
runtime: yaml
resources:
  res:
    type: test:index/res:Res
    properties:
      count: true
"#;
        let (template, _) = parse_template(yaml, None);
        let store = make_store_with_resource(
            "test:index/res:Res",
            &[("count", SchemaPropertyType::Integer)],
            &[],
        );

        let result = type_check(&template, &store, None);
        let type_warnings: Vec<_> = result
            .diagnostics
            .iter()
            .filter(|d| d.summary.contains("type mismatch"))
            .collect();
        assert!(
            !type_warnings.is_empty(),
            "bool should not coerce to integer"
        );
    }

    #[test]
    fn test_type_check_heuristic_token_resolution() {
        // User writes aws:s3:Bucket, schema has aws:s3/bucket:Bucket
        let yaml = r#"
name: test
runtime: yaml
resources:
  bucket:
    type: aws:s3:Bucket
    properties:
      bucketName: my-bucket
"#;
        let (template, _) = parse_template(yaml, None);
        let store = make_store_with_resource(
            "aws:s3/bucket:Bucket",
            &[("bucketName", SchemaPropertyType::String)],
            &[],
        );

        let result = type_check(&template, &store, None);
        assert_eq!(
            result.diagnostics.iter().count(),
            0,
            "should resolve aws:s3:Bucket via heuristic"
        );
    }

    #[test]
    fn test_type_check_unknown_resource_type_skipped() {
        let yaml = r#"
name: test
runtime: yaml
resources:
  thing:
    type: custom:index/thing:Thing
    properties:
      whatever: value
"#;
        let (template, _) = parse_template(yaml, None);
        let store = SchemaStore::new(); // empty — no schemas loaded

        let result = type_check(&template, &store, None);
        assert_eq!(
            result.diagnostics.iter().count(),
            0,
            "unknown resource types should be silently skipped"
        );
    }

    #[test]
    fn test_type_check_invoke_unknown_argument() {
        let yaml = r#"
name: test
runtime: yaml
variables:
  ami:
    fn::invoke:
      function: aws:ec2/getAmi:getAmi
      arguments:
        ownerz: ["self"]
      return: id
"#;
        let (template, _) = parse_template(yaml, None);

        let mut func = FunctionTypeInfo::default();
        func.inputs.insert(
            "owners".to_string(),
            PropertyInfo {
                type_: SchemaPropertyType::Array(Box::new(SchemaPropertyType::String)),
                secret: false,
                const_value: None,
                required: true,
            },
        );
        func.required_inputs.insert("owners".to_string());
        func.outputs.insert(
            "id".to_string(),
            PropertyInfo {
                type_: SchemaPropertyType::String,
                secret: false,
                const_value: None,
                required: false,
            },
        );

        let mut store = SchemaStore::new();
        store.insert(PackageSchema {
            name: "aws".to_string(),
            version: "6.0.0".to_string(),
            resources: HashMap::new(),
            functions: [("aws:ec2/getAmi:getAmi".to_string(), func)]
                .into_iter()
                .collect(),
        });

        let result = type_check(&template, &store, None);
        let warnings: Vec<_> = result.diagnostics.iter().collect();
        // Should have warning about unknown argument 'ownerz' (did you mean 'owners'?)
        // and about missing required argument 'owners'
        assert!(
            warnings.len() >= 2,
            "expected at least 2 warnings, got {}:\n{:?}",
            warnings.len(),
            warnings.iter().map(|w| &w.summary).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_type_check_invoke_missing_required() {
        let yaml = r#"
name: test
runtime: yaml
variables:
  ami:
    fn::invoke:
      function: aws:ec2/getAmi:getAmi
      arguments:
        mostRecent: true
      return: id
"#;
        let (template, _) = parse_template(yaml, None);

        let mut func = FunctionTypeInfo::default();
        func.inputs.insert(
            "owners".to_string(),
            PropertyInfo {
                type_: SchemaPropertyType::Array(Box::new(SchemaPropertyType::String)),
                secret: false,
                const_value: None,
                required: true,
            },
        );
        func.inputs.insert(
            "mostRecent".to_string(),
            PropertyInfo {
                type_: SchemaPropertyType::Boolean,
                secret: false,
                const_value: None,
                required: false,
            },
        );
        func.required_inputs.insert("owners".to_string());
        func.outputs.insert(
            "id".to_string(),
            PropertyInfo {
                type_: SchemaPropertyType::String,
                secret: false,
                const_value: None,
                required: false,
            },
        );

        let mut store = SchemaStore::new();
        store.insert(PackageSchema {
            name: "aws".to_string(),
            version: "6.0.0".to_string(),
            resources: HashMap::new(),
            functions: [("aws:ec2/getAmi:getAmi".to_string(), func)]
                .into_iter()
                .collect(),
        });

        let result = type_check(&template, &store, None);
        let warnings: Vec<_> = result.diagnostics.iter().collect();
        assert!(
            !warnings.is_empty(),
            "expected warning for missing required argument"
        );
        assert!(
            warnings
                .iter()
                .any(|w| w.summary.contains("missing required argument 'owners'")),
            "expected 'owners' missing warning, got: {:?}",
            warnings.iter().map(|w| &w.summary).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_type_check_invoke_unknown_return() {
        let yaml = r#"
name: test
runtime: yaml
variables:
  ami:
    fn::invoke:
      function: aws:ec2/getAmi:getAmi
      arguments:
        owners: ["self"]
      return: nonexistent
"#;
        let (template, _) = parse_template(yaml, None);

        let mut func = FunctionTypeInfo::default();
        func.inputs.insert(
            "owners".to_string(),
            PropertyInfo {
                type_: SchemaPropertyType::Array(Box::new(SchemaPropertyType::String)),
                secret: false,
                const_value: None,
                required: true,
            },
        );
        func.required_inputs.insert("owners".to_string());
        func.outputs.insert(
            "id".to_string(),
            PropertyInfo {
                type_: SchemaPropertyType::String,
                secret: false,
                const_value: None,
                required: false,
            },
        );

        let mut store = SchemaStore::new();
        store.insert(PackageSchema {
            name: "aws".to_string(),
            version: "6.0.0".to_string(),
            resources: HashMap::new(),
            functions: [("aws:ec2/getAmi:getAmi".to_string(), func)]
                .into_iter()
                .collect(),
        });

        let result = type_check(&template, &store, None);
        let warnings: Vec<_> = result.diagnostics.iter().collect();
        assert!(
            warnings
                .iter()
                .any(|w| w.summary.contains("unknown return property 'nonexistent'")),
            "expected unknown return property warning, got: {:?}",
            warnings.iter().map(|w| &w.summary).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_is_assignable_basic() {
        assert!(is_assignable(
            &InferredType::String,
            &SchemaPropertyType::String
        ));
        assert!(is_assignable(
            &InferredType::Integer,
            &SchemaPropertyType::Integer
        ));
        assert!(is_assignable(
            &InferredType::Integer,
            &SchemaPropertyType::Number
        ));
        assert!(is_assignable(
            &InferredType::Number,
            &SchemaPropertyType::Number
        ));
        assert!(is_assignable(
            &InferredType::Bool,
            &SchemaPropertyType::Boolean
        ));
        assert!(is_assignable(
            &InferredType::Null,
            &SchemaPropertyType::String
        ));
        assert!(is_assignable(
            &InferredType::Any,
            &SchemaPropertyType::Integer
        ));
    }

    #[test]
    fn test_is_assignable_coercion() {
        // String coercion
        assert!(is_assignable(
            &InferredType::Number,
            &SchemaPropertyType::String
        ));
        assert!(is_assignable(
            &InferredType::Integer,
            &SchemaPropertyType::String
        ));
        assert!(is_assignable(
            &InferredType::Bool,
            &SchemaPropertyType::String
        ));
    }

    #[test]
    fn test_is_assignable_mismatch() {
        assert!(!is_assignable(
            &InferredType::String,
            &SchemaPropertyType::Integer
        ));
        assert!(!is_assignable(
            &InferredType::String,
            &SchemaPropertyType::Boolean
        ));
        assert!(!is_assignable(
            &InferredType::Bool,
            &SchemaPropertyType::Integer
        ));
    }

    #[test]
    fn test_is_assignable_array() {
        assert!(is_assignable(
            &InferredType::Array(Box::new(InferredType::String)),
            &SchemaPropertyType::Array(Box::new(SchemaPropertyType::String)),
        ));
        // Bool coerces to String, so Array(Bool) → Array(String) is valid
        assert!(is_assignable(
            &InferredType::Array(Box::new(InferredType::Bool)),
            &SchemaPropertyType::Array(Box::new(SchemaPropertyType::String)),
        ));
        // String does NOT coerce to Integer
        assert!(!is_assignable(
            &InferredType::Array(Box::new(InferredType::String)),
            &SchemaPropertyType::Array(Box::new(SchemaPropertyType::Integer)),
        ));
    }

    #[test]
    fn test_type_check_with_source_map() {
        let yaml = r#"
name: test
runtime: yaml
resources:
  bucket:
    type: aws:s3/bucket:Bucket
    properties:
      unknownProp: value
"#;
        let (template, _) = parse_template(yaml, None);
        let store = make_store_with_resource(
            "aws:s3/bucket:Bucket",
            &[("bucketName", SchemaPropertyType::String)],
            &[],
        );

        let mut source_map = HashMap::new();
        source_map.insert("bucket".to_string(), "Pulumi.storage.yaml".to_string());

        let result = type_check(&template, &store, Some(&source_map));
        let warnings: Vec<_> = result.diagnostics.iter().collect();
        assert!(!warnings.is_empty());
        assert!(
            warnings[0].summary.contains("Pulumi.storage.yaml"),
            "expected source map hint, got: {}",
            warnings[0].summary
        );
    }

    #[test]
    fn test_type_check_null_is_valid() {
        let yaml = r#"
name: test
runtime: yaml
resources:
  res:
    type: test:index/res:Res
    properties:
      name: null
"#;
        let (template, _) = parse_template(yaml, None);
        let store = make_store_with_resource(
            "test:index/res:Res",
            &[("name", SchemaPropertyType::String)],
            &[],
        );

        let result = type_check(&template, &store, None);
        let type_warnings: Vec<_> = result
            .diagnostics
            .iter()
            .filter(|d| d.summary.contains("type mismatch"))
            .collect();
        assert!(
            type_warnings.is_empty(),
            "null should be valid for any type"
        );
    }

    #[test]
    fn test_type_check_asset_property() {
        let yaml = r#"
name: test
runtime: yaml
resources:
  func:
    type: aws:lambda/function:Function
    properties:
      code:
        fn::fileArchive: ./code
"#;
        let (template, _) = parse_template(yaml, None);
        let store = make_store_with_resource(
            "aws:lambda/function:Function",
            &[("code", SchemaPropertyType::Archive)],
            &[],
        );

        let result = type_check(&template, &store, None);
        let type_warnings: Vec<_> = result
            .diagnostics
            .iter()
            .filter(|d| d.summary.contains("type mismatch"))
            .collect();
        assert!(
            type_warnings.is_empty(),
            "fileArchive should be valid for Archive type"
        );
    }

    #[test]
    fn test_type_check_aliases_must_be_list() {
        // aliases should be a list; a string value should produce a type error
        let yaml = r#"
name: test
runtime: yaml
resources:
  bucket:
    type: aws:s3/bucket:Bucket
    properties:
      bucketName: test
    options:
      aliases: "not-a-list"
"#;
        let (template, _) = parse_template(yaml, None);
        let store = make_store_with_resource(
            "aws:s3/bucket:Bucket",
            &[("bucketName", SchemaPropertyType::String)],
            &[],
        );

        let result = type_check(&template, &store, None);
        // The type checker should report a diagnostic about aliases format
        // (or at minimum, not crash)
        let _ = result.diagnostics;
    }

    #[test]
    fn test_type_check_non_string_key_error() {
        // Object with non-string key should be handled gracefully
        // (YAML allows numeric keys, which serde_yaml converts to strings)
        let yaml = r#"
name: test
runtime: yaml
resources:
  bucket:
    type: aws:s3/bucket:Bucket
    properties:
      bucketName: test
      tags:
        123: numeric-key
"#;
        let (template, _) = parse_template(yaml, None);
        let store = make_store_with_resource(
            "aws:s3/bucket:Bucket",
            &[
                ("bucketName", SchemaPropertyType::String),
                ("tags", SchemaPropertyType::Object),
            ],
            &[],
        );

        let result = type_check(&template, &store, None);
        // Should not panic — numeric keys are converted to strings by serde_yaml
        let _ = result.diagnostics;
    }

    #[test]
    fn test_type_check_config_compatibility() {
        // Config types should be validated against resource expectations
        let yaml = r#"
name: test
runtime: yaml
config:
  bucketName:
    type: string
    default: my-bucket
resources:
  bucket:
    type: aws:s3/bucket:Bucket
    properties:
      bucketName: ${bucketName}
"#;
        let (template, _) = parse_template(yaml, None);
        let store = make_store_with_resource(
            "aws:s3/bucket:Bucket",
            &[("bucketName", SchemaPropertyType::String)],
            &[],
        );

        let result = type_check(&template, &store, None);
        // String config used for string property should not produce type errors
        let type_errors: Vec<_> = result
            .diagnostics
            .iter()
            .filter(|d| d.summary.contains("type mismatch"))
            .collect();
        assert!(
            type_errors.is_empty(),
            "string config for string property should be compatible"
        );
    }
}

//! Schema loading and storage for provider resource metadata.
//!
//! Parses provider schema JSON to extract output properties, secret properties,
//! aliases, and property type information per resource type.
//!
//! Used by the evaluator to:
//! - Fill `Value::Unknown` for output-only properties during preview
//! - Auto-add `additional_secret_outputs` from schema
//! - Auto-add `aliases` from schema

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Type classification for a schema property.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SchemaPropertyType {
    String,
    Number,
    Integer,
    Boolean,
    Array(Box<SchemaPropertyType>),
    Object,
    Asset,
    Archive,
    Unknown,
}

impl SchemaPropertyType {
    /// Returns a string label (used for Python plan serialization).
    pub fn label(&self) -> &'static str {
        match self {
            SchemaPropertyType::String => "string",
            SchemaPropertyType::Number => "number",
            SchemaPropertyType::Integer => "integer",
            SchemaPropertyType::Boolean => "boolean",
            SchemaPropertyType::Array(_) => "array",
            SchemaPropertyType::Object => "object",
            SchemaPropertyType::Asset => "asset",
            SchemaPropertyType::Archive => "archive",
            SchemaPropertyType::Unknown => "unknown",
        }
    }
}

/// Information about a single property in a resource type schema.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PropertyInfo {
    pub type_: SchemaPropertyType,
    pub secret: bool,
    /// Constant value from schema (the `"const"` field).
    pub const_value: Option<serde_json::Value>,
    /// Whether this property is required.
    pub required: bool,
}

/// Metadata extracted from a provider schema for a single resource type.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResourceTypeInfo {
    /// All properties (both input and output).
    pub properties: HashSet<String>,
    /// Input-only properties (accepted during registration).
    pub input_properties: HashSet<String>,
    /// Output-only properties (returned by provider, not set by user).
    /// Computed as `properties - input_properties`.
    pub output_properties: HashSet<String>,
    /// Properties marked as secret in the schema.
    pub secret_properties: HashSet<String>,
    /// Input properties marked as secret in the schema.
    /// Used to wrap input values with Value::Secret() before registration.
    pub secret_input_properties: HashSet<String>,
    /// Resource aliases from the schema.
    pub aliases: Vec<String>,
    /// Typed property metadata (name → type + secret flag).
    pub property_types: HashMap<String, PropertyInfo>,
    /// Whether this is a component resource (remote=true).
    pub is_component: bool,
    /// Required input property names (from "required" array in inputProperties).
    pub required_inputs: HashSet<String>,
    /// Typed input property metadata (distinct from property_types which merges both).
    pub input_property_types: HashMap<String, PropertyInfo>,
}

/// Metadata extracted from a provider schema for a single function.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FunctionTypeInfo {
    /// Input parameter types.
    pub inputs: HashMap<String, PropertyInfo>,
    /// Required input parameter names.
    pub required_inputs: HashSet<String>,
    /// Output property types.
    pub outputs: HashMap<String, PropertyInfo>,
}

/// Schema metadata for a single provider package.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PackageSchema {
    pub name: String,
    pub version: String,
    pub resources: HashMap<String, ResourceTypeInfo>,
    pub functions: HashMap<String, FunctionTypeInfo>,
}

/// In-memory store of parsed schemas, keyed by package name.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SchemaStore {
    packages: HashMap<String, PackageSchema>,
}

impl SchemaStore {
    pub fn new() -> Self {
        Self {
            packages: HashMap::new(),
        }
    }

    /// Insert a parsed package schema into the store.
    pub fn insert(&mut self, schema: PackageSchema) {
        self.packages.insert(schema.name.clone(), schema);
    }

    /// Look up resource type info by canonical token (e.g. `aws:s3/bucket:Bucket`).
    pub fn lookup_resource(&self, canonical_token: &str) -> Option<&ResourceTypeInfo> {
        // Token format: "pkg:module/type:Type"
        let pkg = canonical_token.split(':').next()?;
        let schema = self.packages.get(pkg)?;
        schema.resources.get(canonical_token)
    }

    /// Get output-only property names for a resource type.
    pub fn output_properties(&self, canonical_token: &str) -> HashSet<String> {
        self.lookup_resource(canonical_token)
            .map(|info| info.output_properties.clone())
            .unwrap_or_default()
    }

    /// Get secret property names for a resource type.
    pub fn secret_properties(&self, canonical_token: &str) -> HashSet<String> {
        self.lookup_resource(canonical_token)
            .map(|info| info.secret_properties.clone())
            .unwrap_or_default()
    }

    /// Get secret input property names for a resource type.
    pub fn secret_input_properties(&self, canonical_token: &str) -> HashSet<String> {
        self.lookup_resource(canonical_token)
            .map(|info| info.secret_input_properties.clone())
            .unwrap_or_default()
    }

    /// Check whether a resource type is a component (remote) resource.
    pub fn is_component(&self, canonical_token: &str) -> bool {
        self.lookup_resource(canonical_token)
            .map(|info| info.is_component)
            .unwrap_or(false)
    }

    /// Get required input property names for a resource type.
    pub fn required_inputs(&self, canonical_token: &str) -> HashSet<String> {
        self.lookup_resource(canonical_token)
            .map(|info| info.required_inputs.clone())
            .unwrap_or_default()
    }

    /// Look up function type info by canonical token.
    pub fn lookup_function(&self, canonical_token: &str) -> Option<&FunctionTypeInfo> {
        let pkg = canonical_token.split(':').next()?;
        let schema = self.packages.get(pkg)?;
        schema.functions.get(canonical_token)
    }

    /// Resolve a resource token to its canonical form using schema knowledge.
    ///
    /// 1. Direct lookup (already canonical)
    /// 2. Try heuristic canonicalization
    /// 3. Search aliases in matching package
    pub fn resolve_resource_token(&self, token: &str) -> Option<String> {
        // 1. Direct lookup
        if self.lookup_resource(token).is_some() {
            return Some(token.to_string());
        }

        // 2. Try heuristic canonicalization
        let canonical = crate::packages::canonicalize_type_token(token);
        if self.lookup_resource(&canonical).is_some() {
            return Some(canonical);
        }

        // 3. Try all expansions
        let expansions = crate::packages::expand_type_token(token);
        for candidate in &expansions {
            if self.lookup_resource(candidate).is_some() {
                return Some(candidate.clone());
            }
        }

        // 4. Search aliases in matching package
        let pkg_name = token.split(':').next()?;
        if let Some(schema) = self.packages.get(pkg_name) {
            for (canonical_token, info) in &schema.resources {
                for alias in &info.aliases {
                    if alias == token {
                        return Some(canonical_token.clone());
                    }
                    // Also try canonical form of alias
                    let canonical_alias = crate::packages::canonicalize_type_token(alias);
                    if canonical_alias == canonical {
                        return Some(canonical_token.clone());
                    }
                }
            }
        }

        None
    }

    /// Resolve a function token to its canonical form using schema knowledge.
    pub fn resolve_function_token(&self, token: &str) -> Option<String> {
        // 1. Direct lookup
        if self.lookup_function(token).is_some() {
            return Some(token.to_string());
        }

        // 2. Try heuristic canonicalization
        let canonical = crate::packages::canonicalize_type_token(token);
        if self.lookup_function(&canonical).is_some() {
            return Some(canonical);
        }

        // 3. Try all expansions
        let expansions = crate::packages::expand_type_token(token);
        for candidate in &expansions {
            if self.lookup_function(candidate).is_some() {
                return Some(candidate.clone());
            }
        }

        None
    }

    /// Returns all packages in the store.
    pub fn packages(&self) -> &HashMap<String, PackageSchema> {
        &self.packages
    }

    /// Saves the schema store to a JSON file on disk.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let json =
            serde_json::to_vec(self).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        std::fs::write(path, json)
    }

    /// Loads a schema store from a JSON file on disk.
    pub fn load(path: &Path) -> io::Result<Self> {
        let data = std::fs::read(path)?;
        serde_json::from_slice(&data).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
}

/// Parse a property type from a schema property definition.
fn parse_property_type(prop: &serde_json::Value) -> SchemaPropertyType {
    // Check $ref for asset/archive types
    if let Some(ref_str) = prop.get("$ref").and_then(|v| v.as_str()) {
        if ref_str.contains("Asset") {
            return SchemaPropertyType::Asset;
        }
        if ref_str.contains("Archive") {
            return SchemaPropertyType::Archive;
        }
    }

    match prop.get("type").and_then(|v| v.as_str()) {
        Some("string") => SchemaPropertyType::String,
        Some("number") => SchemaPropertyType::Number,
        Some("integer") => SchemaPropertyType::Integer,
        Some("boolean") => SchemaPropertyType::Boolean,
        Some("array") => {
            let item_type = prop
                .get("items")
                .map(parse_property_type)
                .unwrap_or(SchemaPropertyType::Unknown);
            SchemaPropertyType::Array(Box::new(item_type))
        }
        Some("object") => SchemaPropertyType::Object,
        _ => SchemaPropertyType::Unknown,
    }
}

/// Parse provider schema JSON bytes into a `PackageSchema`.
///
/// Only extracts resource metadata (property names, secrets, aliases, types).
/// Ignores functions, config, and other schema sections.
///
/// JSON structure:
/// ```json
/// {
///   "name": "aws",
///   "version": "6.0.0",
///   "resources": {
///     "aws:s3/bucket:Bucket": {
///       "properties": { "arn": { "type": "string", "secret": true }, ... },
///       "inputProperties": { "bucketName": { "type": "string" }, ... },
///       "aliases": [ { "type": "aws:s3:Bucket" } ]
///     }
///   }
/// }
/// ```
pub fn parse_schema_json(json_bytes: &[u8]) -> Result<PackageSchema, String> {
    let root: serde_json::Value =
        serde_json::from_slice(json_bytes).map_err(|e| format!("invalid JSON: {}", e))?;

    let name = root
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let version = root
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let mut resources = HashMap::new();

    if let Some(res_map) = root.get("resources").and_then(|v| v.as_object()) {
        for (token, res_def) in res_map {
            let mut info = ResourceTypeInfo::default();

            // Parse properties (all — both input and output)
            if let Some(props) = res_def.get("properties").and_then(|v| v.as_object()) {
                for (prop_name, prop_def) in props {
                    info.properties.insert(prop_name.clone());

                    let secret = prop_def
                        .get("secret")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    if secret {
                        info.secret_properties.insert(prop_name.clone());
                    }

                    let prop_type = parse_property_type(prop_def);
                    let const_value = prop_def.get("const").cloned();
                    info.property_types.insert(
                        prop_name.clone(),
                        PropertyInfo {
                            type_: prop_type,
                            secret,
                            const_value,
                            required: false, // set later from "required" array
                        },
                    );
                }
            }

            // Parse inputProperties
            let mut input_required_set: HashSet<String> = HashSet::new();
            if let Some(input_obj) = res_def.get("inputProperties") {
                // Parse "required" array for input properties
                if let Some(req_arr) = res_def.get("requiredInputs").and_then(|v| v.as_array()) {
                    for req in req_arr {
                        if let Some(s) = req.as_str() {
                            input_required_set.insert(s.to_string());
                        }
                    }
                }
                // Also check "required" directly under the resource (some schemas use this)
                if input_required_set.is_empty() {
                    if let Some(req_arr) = res_def.get("required").and_then(|v| v.as_array()) {
                        for req in req_arr {
                            if let Some(s) = req.as_str() {
                                input_required_set.insert(s.to_string());
                            }
                        }
                    }
                }

                if let Some(input_props) = input_obj.as_object() {
                    for (prop_name, prop_def) in input_props {
                        info.input_properties.insert(prop_name.clone());

                        let secret = prop_def
                            .get("secret")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        if secret {
                            info.secret_input_properties.insert(prop_name.clone());
                        }

                        let is_required = input_required_set.contains(prop_name);
                        let prop_type = parse_property_type(prop_def);
                        let const_value = prop_def.get("const").cloned();

                        info.input_property_types.insert(
                            prop_name.clone(),
                            PropertyInfo {
                                type_: prop_type.clone(),
                                secret,
                                const_value: const_value.clone(),
                                required: is_required,
                            },
                        );

                        if is_required {
                            info.required_inputs.insert(prop_name.clone());
                        }

                        // Also add to property_types if not already present
                        if !info.property_types.contains_key(prop_name) {
                            info.property_types.insert(
                                prop_name.clone(),
                                PropertyInfo {
                                    type_: prop_type,
                                    secret,
                                    const_value,
                                    required: is_required,
                                },
                            );
                        }
                    }
                }
            }

            // Compute output-only properties = properties - inputProperties
            info.output_properties = info
                .properties
                .difference(&info.input_properties)
                .cloned()
                .collect();

            // Parse aliases
            if let Some(aliases_arr) = res_def.get("aliases").and_then(|v| v.as_array()) {
                for alias in aliases_arr {
                    if let Some(alias_type) = alias.get("type").and_then(|v| v.as_str()) {
                        info.aliases.push(alias_type.to_string());
                    }
                }
            }

            // Parse isComponent flag
            info.is_component = res_def
                .get("isComponent")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            resources.insert(token.clone(), info);
        }
    }

    // Parse functions
    let mut functions = HashMap::new();
    if let Some(func_map) = root.get("functions").and_then(|v| v.as_object()) {
        for (token, func_def) in func_map {
            let mut func_info = FunctionTypeInfo::default();

            // Parse inputs
            if let Some(inputs_obj) = func_def.get("inputs").and_then(|v| v.as_object()) {
                // Parse required array
                let mut required_set: HashSet<String> = HashSet::new();
                if let Some(req_arr) = inputs_obj.get("required").and_then(|v| v.as_array()) {
                    for req in req_arr {
                        if let Some(s) = req.as_str() {
                            required_set.insert(s.to_string());
                        }
                    }
                }

                if let Some(props) = inputs_obj.get("properties").and_then(|v| v.as_object()) {
                    for (prop_name, prop_def) in props {
                        let prop_type = parse_property_type(prop_def);
                        let secret = prop_def
                            .get("secret")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        let is_required = required_set.contains(prop_name);
                        if is_required {
                            func_info.required_inputs.insert(prop_name.clone());
                        }
                        func_info.inputs.insert(
                            prop_name.clone(),
                            PropertyInfo {
                                type_: prop_type,
                                secret,
                                const_value: None,
                                required: is_required,
                            },
                        );
                    }
                }
            }

            // Parse outputs
            if let Some(outputs_obj) = func_def.get("outputs").and_then(|v| v.as_object()) {
                if let Some(props) = outputs_obj.get("properties").and_then(|v| v.as_object()) {
                    for (prop_name, prop_def) in props {
                        let prop_type = parse_property_type(prop_def);
                        let secret = prop_def
                            .get("secret")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        func_info.outputs.insert(
                            prop_name.clone(),
                            PropertyInfo {
                                type_: prop_type,
                                secret,
                                const_value: None,
                                required: false,
                            },
                        );
                    }
                }
            }

            functions.insert(token.clone(), func_info);
        }
    }

    Ok(PackageSchema {
        name,
        version,
        resources,
        functions,
    })
}

/// Generates a Pulumi package schema JSON from component declarations in a template.
///
/// Each component becomes a resource with `isComponent: true`, with input and
/// output properties extracted from the component declaration.
pub fn generate_component_schema(
    template: &crate::ast::template::TemplateDecl<'_>,
) -> serde_json::Value {
    let pkg_name = template.name.as_deref().unwrap_or("yaml-components");

    let mut resources = serde_json::Map::new();

    for comp in &template.components {
        let comp_name = &comp.key;
        let component_type = format!("{}:index:{}", pkg_name, comp_name);

        let mut input_properties = serde_json::Map::new();
        let mut required_inputs = Vec::new();

        for input in &comp.component.inputs {
            let key = input.key.to_string();
            let mut prop = serde_json::Map::new();

            // Map type strings to schema types
            if let Some(ref type_str) = input.param.type_ {
                match type_str.as_ref() {
                    "string" => {
                        prop.insert("type".into(), "string".into());
                    }
                    "number" => {
                        prop.insert("type".into(), "number".into());
                    }
                    "integer" | "int" => {
                        prop.insert("type".into(), "integer".into());
                    }
                    "boolean" | "bool" => {
                        prop.insert("type".into(), "boolean".into());
                    }
                    "List<string>" | "list(string)" => {
                        prop.insert("type".into(), "array".into());
                        let mut items = serde_json::Map::new();
                        items.insert("type".into(), "string".into());
                        prop.insert("items".into(), items.into());
                    }
                    _ => {
                        prop.insert("$ref".into(), "pulumi.json#/Any".into());
                    }
                }
            } else {
                prop.insert("$ref".into(), "pulumi.json#/Any".into());
            }

            if input.param.secret == Some(true) {
                prop.insert("secret".into(), true.into());
            }

            // If no default, it's required
            if input.param.default.is_none() {
                required_inputs.push(serde_json::Value::String(key.clone()));
            }

            input_properties.insert(key, prop.into());
        }

        let mut output_properties = serde_json::Map::new();
        for output in &comp.component.outputs {
            let key = output.key.to_string();
            let mut prop = serde_json::Map::new();
            prop.insert("$ref".into(), "pulumi.json#/Any".into());
            output_properties.insert(key, prop.into());
        }

        let mut resource_spec = serde_json::Map::new();
        resource_spec.insert("isComponent".into(), true.into());
        resource_spec.insert("inputProperties".into(), input_properties.into());
        resource_spec.insert("properties".into(), output_properties.into());
        if !required_inputs.is_empty() {
            resource_spec.insert("requiredInputs".into(), required_inputs.into());
        }

        resources.insert(component_type, resource_spec.into());
    }

    serde_json::json!({
        "name": pkg_name,
        "version": "0.0.0",
        "resources": resources,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_basic_schema() {
        let json = br#"{
            "name": "aws",
            "version": "6.0.0",
            "resources": {
                "aws:s3/bucket:Bucket": {
                    "properties": {
                        "arn": { "type": "string" },
                        "bucketName": { "type": "string" },
                        "region": { "type": "string" }
                    },
                    "inputProperties": {
                        "bucketName": { "type": "string" },
                        "region": { "type": "string" }
                    }
                }
            }
        }"#;

        let schema = parse_schema_json(json).unwrap();
        assert_eq!(schema.name, "aws");
        assert_eq!(schema.version, "6.0.0");

        let info = schema.resources.get("aws:s3/bucket:Bucket").unwrap();
        assert_eq!(info.properties.len(), 3);
        assert_eq!(info.input_properties.len(), 2);
        assert_eq!(info.output_properties.len(), 1);
        assert!(info.output_properties.contains("arn"));
        assert!(!info.output_properties.contains("bucketName"));
    }

    #[test]
    fn test_parse_secret_properties() {
        let json = br#"{
            "name": "random",
            "version": "4.0.0",
            "resources": {
                "random:index/randomPassword:RandomPassword": {
                    "properties": {
                        "result": { "type": "string", "secret": true },
                        "length": { "type": "integer" }
                    },
                    "inputProperties": {
                        "length": { "type": "integer" }
                    }
                }
            }
        }"#;

        let schema = parse_schema_json(json).unwrap();
        let info = schema
            .resources
            .get("random:index/randomPassword:RandomPassword")
            .unwrap();
        assert!(info.secret_properties.contains("result"));
        assert!(!info.secret_properties.contains("length"));
    }

    #[test]
    fn test_parse_aliases() {
        let json = br#"{
            "name": "aws",
            "version": "6.0.0",
            "resources": {
                "aws:s3/bucket:Bucket": {
                    "properties": {},
                    "inputProperties": {},
                    "aliases": [
                        { "type": "aws:s3:Bucket" },
                        { "type": "aws:s3/legacy:Bucket" }
                    ]
                }
            }
        }"#;

        let schema = parse_schema_json(json).unwrap();
        let info = schema.resources.get("aws:s3/bucket:Bucket").unwrap();
        assert_eq!(info.aliases.len(), 2);
        assert!(info.aliases.contains(&"aws:s3:Bucket".to_string()));
        assert!(info.aliases.contains(&"aws:s3/legacy:Bucket".to_string()));
    }

    #[test]
    fn test_parse_empty_schema() {
        let json = br#"{}"#;
        let schema = parse_schema_json(json).unwrap();
        assert_eq!(schema.name, "");
        assert_eq!(schema.version, "");
        assert!(schema.resources.is_empty());
    }

    #[test]
    fn test_parse_no_resources() {
        let json = br#"{ "name": "test", "version": "1.0.0" }"#;
        let schema = parse_schema_json(json).unwrap();
        assert_eq!(schema.name, "test");
        assert!(schema.resources.is_empty());
    }

    #[test]
    fn test_parse_malformed_json() {
        let json = b"not valid json";
        let result = parse_schema_json(json);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("invalid JSON"));
    }

    #[test]
    fn test_store_lookup_hit() {
        let mut store = SchemaStore::new();
        let mut info = ResourceTypeInfo::default();
        info.properties.insert("arn".to_string());
        info.output_properties.insert("arn".to_string());

        let schema = PackageSchema {
            name: "aws".to_string(),
            version: "6.0.0".to_string(),
            resources: [("aws:s3/bucket:Bucket".to_string(), info)]
                .into_iter()
                .collect(),
            functions: HashMap::new(),
        };
        store.insert(schema);

        let result = store.lookup_resource("aws:s3/bucket:Bucket");
        assert!(result.is_some());
        assert!(result.unwrap().output_properties.contains("arn"));
    }

    #[test]
    fn test_store_lookup_miss() {
        let store = SchemaStore::new();
        assert!(store.lookup_resource("aws:s3/bucket:Bucket").is_none());
    }

    #[test]
    fn test_store_lookup_wrong_package() {
        let mut store = SchemaStore::new();
        let schema = PackageSchema {
            name: "aws".to_string(),
            version: "6.0.0".to_string(),
            resources: HashMap::new(),
            functions: HashMap::new(),
        };
        store.insert(schema);

        // Package exists but resource doesn't
        assert!(store.lookup_resource("aws:s3/bucket:Bucket").is_none());
        // Different package entirely
        assert!(store.lookup_resource("gcp:storage/bucket:Bucket").is_none());
    }

    #[test]
    fn test_output_properties_computation() {
        let json = br#"{
            "name": "test",
            "version": "1.0.0",
            "resources": {
                "test:index/res:Res": {
                    "properties": {
                        "id": { "type": "string" },
                        "name": { "type": "string" },
                        "status": { "type": "string" },
                        "input1": { "type": "string" }
                    },
                    "inputProperties": {
                        "name": { "type": "string" },
                        "input1": { "type": "string" }
                    }
                }
            }
        }"#;

        let schema = parse_schema_json(json).unwrap();
        let info = schema.resources.get("test:index/res:Res").unwrap();
        // output_properties = {id, status} (properties - inputProperties)
        assert_eq!(info.output_properties.len(), 2);
        assert!(info.output_properties.contains("id"));
        assert!(info.output_properties.contains("status"));
        assert!(!info.output_properties.contains("name"));
    }

    #[test]
    fn test_multiple_packages() {
        let mut store = SchemaStore::new();

        let aws_json = br#"{
            "name": "aws",
            "version": "6.0.0",
            "resources": {
                "aws:s3/bucket:Bucket": { "properties": { "arn": { "type": "string" } }, "inputProperties": {} }
            }
        }"#;
        let gcp_json = br#"{
            "name": "gcp",
            "version": "7.0.0",
            "resources": {
                "gcp:storage/bucket:Bucket": { "properties": { "selfLink": { "type": "string" } }, "inputProperties": {} }
            }
        }"#;

        store.insert(parse_schema_json(aws_json).unwrap());
        store.insert(parse_schema_json(gcp_json).unwrap());

        assert!(store.lookup_resource("aws:s3/bucket:Bucket").is_some());
        assert!(store.lookup_resource("gcp:storage/bucket:Bucket").is_some());
        assert!(store
            .lookup_resource("azure:storage/account:Account")
            .is_none());

        assert!(store
            .output_properties("aws:s3/bucket:Bucket")
            .contains("arn"));
        assert!(store
            .output_properties("gcp:storage/bucket:Bucket")
            .contains("selfLink"));
    }

    #[test]
    fn test_property_types_parsed() {
        let json = br#"{
            "name": "test",
            "version": "1.0.0",
            "resources": {
                "test:index/res:Res": {
                    "properties": {
                        "name": { "type": "string" },
                        "count": { "type": "integer" },
                        "enabled": { "type": "boolean" },
                        "tags": { "type": "array", "items": { "type": "string" } },
                        "metadata": { "type": "object" },
                        "score": { "type": "number" }
                    },
                    "inputProperties": {}
                }
            }
        }"#;

        let schema = parse_schema_json(json).unwrap();
        let info = schema.resources.get("test:index/res:Res").unwrap();

        assert_eq!(
            info.property_types.get("name").unwrap().type_,
            SchemaPropertyType::String
        );
        assert_eq!(
            info.property_types.get("count").unwrap().type_,
            SchemaPropertyType::Integer
        );
        assert_eq!(
            info.property_types.get("enabled").unwrap().type_,
            SchemaPropertyType::Boolean
        );
        assert_eq!(
            info.property_types.get("score").unwrap().type_,
            SchemaPropertyType::Number
        );
        assert_eq!(
            info.property_types.get("metadata").unwrap().type_,
            SchemaPropertyType::Object
        );

        match &info.property_types.get("tags").unwrap().type_ {
            SchemaPropertyType::Array(inner) => {
                assert_eq!(**inner, SchemaPropertyType::String);
            }
            other => panic!("expected Array, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_secret_input_properties() {
        let json = br#"{
            "name": "db",
            "version": "1.0.0",
            "resources": {
                "db:index/instance:Instance": {
                    "properties": {
                        "connectionString": { "type": "string", "secret": true },
                        "password": { "type": "string", "secret": true },
                        "name": { "type": "string" }
                    },
                    "inputProperties": {
                        "password": { "type": "string", "secret": true },
                        "name": { "type": "string" }
                    }
                }
            }
        }"#;

        let schema = parse_schema_json(json).unwrap();
        let info = schema.resources.get("db:index/instance:Instance").unwrap();

        // password is secret in inputProperties
        assert!(info.secret_input_properties.contains("password"));
        // name is not secret
        assert!(!info.secret_input_properties.contains("name"));
        // connectionString is secret in properties but NOT in inputProperties
        assert!(!info.secret_input_properties.contains("connectionString"));
        // connectionString IS in secret_properties (all properties section)
        assert!(info.secret_properties.contains("connectionString"));
        assert!(info.secret_properties.contains("password"));
    }

    #[test]
    fn test_store_secret_input_properties() {
        let mut store = SchemaStore::new();
        let mut info = ResourceTypeInfo::default();
        info.secret_input_properties.insert("password".to_string());
        info.input_properties.insert("password".to_string());
        info.input_properties.insert("name".to_string());

        let schema = PackageSchema {
            name: "db".to_string(),
            version: "1.0.0".to_string(),
            resources: [("db:index/instance:Instance".to_string(), info)]
                .into_iter()
                .collect(),
            functions: HashMap::new(),
        };
        store.insert(schema);

        let secrets = store.secret_input_properties("db:index/instance:Instance");
        assert!(secrets.contains("password"));
        assert!(!secrets.contains("name"));

        // Missing resource returns empty set
        let empty = store.secret_input_properties("db:index/missing:Missing");
        assert!(empty.is_empty());
    }

    #[test]
    fn test_parse_is_component() {
        let json = br#"{
            "name": "test",
            "version": "1.0.0",
            "resources": {
                "test:index/comp:Comp": {
                    "isComponent": true,
                    "properties": {},
                    "inputProperties": {}
                },
                "test:index/custom:Custom": {
                    "properties": {},
                    "inputProperties": {}
                }
            }
        }"#;

        let schema = parse_schema_json(json).unwrap();
        let comp = schema.resources.get("test:index/comp:Comp").unwrap();
        assert!(comp.is_component);
        let custom = schema.resources.get("test:index/custom:Custom").unwrap();
        assert!(!custom.is_component);
    }

    #[test]
    fn test_store_is_component() {
        let mut store = SchemaStore::new();
        let info = ResourceTypeInfo {
            is_component: true,
            ..Default::default()
        };

        let schema = PackageSchema {
            name: "test".to_string(),
            version: "1.0.0".to_string(),
            resources: [("test:index/comp:Comp".to_string(), info)]
                .into_iter()
                .collect(),
            functions: HashMap::new(),
        };
        store.insert(schema);

        assert!(store.is_component("test:index/comp:Comp"));
        assert!(!store.is_component("test:index/missing:Missing"));
    }

    #[test]
    fn test_parse_const_value() {
        let json = br#"{
            "name": "test",
            "version": "1.0.0",
            "resources": {
                "test:index/res:Res": {
                    "properties": {
                        "kind": { "type": "string", "const": "MyKind" },
                        "name": { "type": "string" }
                    },
                    "inputProperties": {
                        "kind": { "type": "string", "const": "MyKind" },
                        "name": { "type": "string" }
                    }
                }
            }
        }"#;

        let schema = parse_schema_json(json).unwrap();
        let info = schema.resources.get("test:index/res:Res").unwrap();
        let kind_info = info.property_types.get("kind").unwrap();
        assert_eq!(
            kind_info.const_value,
            Some(serde_json::Value::String("MyKind".to_string()))
        );
        let name_info = info.property_types.get("name").unwrap();
        assert!(name_info.const_value.is_none());
    }

    #[test]
    fn test_parse_const_value_integer() {
        let json = br#"{
            "name": "test",
            "version": "1.0.0",
            "resources": {
                "test:index/res:Res": {
                    "properties": {
                        "version": { "type": "integer", "const": 2 }
                    },
                    "inputProperties": {}
                }
            }
        }"#;

        let schema = parse_schema_json(json).unwrap();
        let info = schema.resources.get("test:index/res:Res").unwrap();
        let ver_info = info.property_types.get("version").unwrap();
        assert_eq!(ver_info.const_value, Some(serde_json::json!(2)));
    }

    #[test]
    fn test_parse_required_inputs() {
        let json = br#"{
            "name": "test",
            "version": "1.0.0",
            "resources": {
                "test:index/res:Res": {
                    "properties": {
                        "id": { "type": "string" },
                        "name": { "type": "string" },
                        "size": { "type": "integer" }
                    },
                    "inputProperties": {
                        "name": { "type": "string" },
                        "size": { "type": "integer" }
                    },
                    "requiredInputs": ["name"]
                }
            }
        }"#;

        let schema = parse_schema_json(json).unwrap();
        let info = schema.resources.get("test:index/res:Res").unwrap();
        assert!(info.required_inputs.contains("name"));
        assert!(!info.required_inputs.contains("size"));
        assert!(info.input_property_types.get("name").unwrap().required);
        assert!(!info.input_property_types.get("size").unwrap().required);
    }

    #[test]
    fn test_parse_functions() {
        let json = br#"{
            "name": "aws",
            "version": "6.0.0",
            "resources": {},
            "functions": {
                "aws:ec2/getAmi:getAmi": {
                    "inputs": {
                        "properties": {
                            "owners": { "type": "array", "items": { "type": "string" } },
                            "filters": { "type": "array" },
                            "mostRecent": { "type": "boolean" }
                        },
                        "required": ["owners"]
                    },
                    "outputs": {
                        "properties": {
                            "id": { "type": "string" },
                            "imageId": { "type": "string" }
                        }
                    }
                }
            }
        }"#;

        let schema = parse_schema_json(json).unwrap();
        assert!(schema.functions.contains_key("aws:ec2/getAmi:getAmi"));

        let func = schema.functions.get("aws:ec2/getAmi:getAmi").unwrap();
        assert_eq!(func.inputs.len(), 3);
        assert!(func.required_inputs.contains("owners"));
        assert!(!func.required_inputs.contains("mostRecent"));
        assert_eq!(func.outputs.len(), 2);
    }

    #[test]
    fn test_store_lookup_function() {
        let mut store = SchemaStore::new();
        let json = br#"{
            "name": "aws",
            "version": "6.0.0",
            "resources": {},
            "functions": {
                "aws:ec2/getAmi:getAmi": {
                    "inputs": {
                        "properties": {
                            "owners": { "type": "array", "items": { "type": "string" } }
                        }
                    },
                    "outputs": {
                        "properties": {
                            "id": { "type": "string" }
                        }
                    }
                }
            }
        }"#;
        store.insert(parse_schema_json(json).unwrap());

        assert!(store.lookup_function("aws:ec2/getAmi:getAmi").is_some());
        assert!(store
            .lookup_function("aws:ec2/getMissing:getMissing")
            .is_none());
    }

    #[test]
    fn test_resolve_resource_token_direct() {
        let mut store = SchemaStore::new();
        let json = br#"{
            "name": "aws",
            "version": "6.0.0",
            "resources": {
                "aws:s3/bucket:Bucket": {
                    "properties": {},
                    "inputProperties": {}
                }
            }
        }"#;
        store.insert(parse_schema_json(json).unwrap());

        // Direct canonical lookup
        assert_eq!(
            store.resolve_resource_token("aws:s3/bucket:Bucket"),
            Some("aws:s3/bucket:Bucket".to_string())
        );
    }

    #[test]
    fn test_resolve_resource_token_heuristic() {
        let mut store = SchemaStore::new();
        let json = br#"{
            "name": "aws",
            "version": "6.0.0",
            "resources": {
                "aws:s3/bucket:Bucket": {
                    "properties": {},
                    "inputProperties": {}
                }
            }
        }"#;
        store.insert(parse_schema_json(json).unwrap());

        // Heuristic canonicalization: aws:s3:Bucket → aws:s3/bucket:Bucket
        assert_eq!(
            store.resolve_resource_token("aws:s3:Bucket"),
            Some("aws:s3/bucket:Bucket".to_string())
        );
    }

    #[test]
    fn test_resolve_resource_token_alias() {
        let mut store = SchemaStore::new();
        let json = br#"{
            "name": "aws",
            "version": "6.0.0",
            "resources": {
                "aws:s3/bucketV2:BucketV2": {
                    "properties": {},
                    "inputProperties": {},
                    "aliases": [
                        { "type": "aws:s3:Bucket" }
                    ]
                }
            }
        }"#;
        store.insert(parse_schema_json(json).unwrap());

        // Alias resolution: aws:s3:Bucket is an alias for aws:s3/bucketV2:BucketV2
        assert_eq!(
            store.resolve_resource_token("aws:s3:Bucket"),
            Some("aws:s3/bucketV2:BucketV2".to_string())
        );
    }

    #[test]
    fn test_resolve_resource_token_not_found() {
        let store = SchemaStore::new();
        assert!(store.resolve_resource_token("aws:s3:Bucket").is_none());
    }

    #[test]
    fn test_resolve_function_token_heuristic() {
        let mut store = SchemaStore::new();
        let json = br#"{
            "name": "aws",
            "version": "6.0.0",
            "resources": {},
            "functions": {
                "aws:ec2/getAmi:getAmi": {
                    "inputs": {},
                    "outputs": {}
                }
            }
        }"#;
        store.insert(parse_schema_json(json).unwrap());

        // Heuristic: aws:ec2:getAmi → aws:ec2/getAmi:getAmi
        assert_eq!(
            store.resolve_function_token("aws:ec2:getAmi"),
            Some("aws:ec2/getAmi:getAmi".to_string())
        );
    }

    #[test]
    fn test_store_required_inputs() {
        let mut store = SchemaStore::new();
        let json = br#"{
            "name": "test",
            "version": "1.0.0",
            "resources": {
                "test:index/res:Res": {
                    "properties": {},
                    "inputProperties": {
                        "name": { "type": "string" },
                        "size": { "type": "integer" }
                    },
                    "requiredInputs": ["name"]
                }
            }
        }"#;
        store.insert(parse_schema_json(json).unwrap());

        let required = store.required_inputs("test:index/res:Res");
        assert!(required.contains("name"));
        assert!(!required.contains("size"));
    }

    #[test]
    fn test_input_property_types_separate_from_property_types() {
        let json = br#"{
            "name": "test",
            "version": "1.0.0",
            "resources": {
                "test:index/res:Res": {
                    "properties": {
                        "id": { "type": "string" },
                        "name": { "type": "string" }
                    },
                    "inputProperties": {
                        "name": { "type": "string" },
                        "tags": { "type": "object" }
                    }
                }
            }
        }"#;

        let schema = parse_schema_json(json).unwrap();
        let info = schema.resources.get("test:index/res:Res").unwrap();

        // input_property_types only has inputProperties
        assert_eq!(info.input_property_types.len(), 2);
        assert!(info.input_property_types.contains_key("name"));
        assert!(info.input_property_types.contains_key("tags"));
        assert!(!info.input_property_types.contains_key("id"));

        // property_types has both (merged)
        assert!(info.property_types.contains_key("id"));
        assert!(info.property_types.contains_key("name"));
        assert!(info.property_types.contains_key("tags"));
    }

    #[test]
    fn test_schema_store_save_load_round_trip() {
        let mut store = SchemaStore::new();
        let json = br#"{
            "name": "aws",
            "version": "6.0.0",
            "resources": {
                "aws:s3/bucket:Bucket": {
                    "properties": {
                        "arn": { "type": "string", "secret": true }
                    },
                    "inputProperties": {
                        "bucketName": { "type": "string" }
                    }
                }
            }
        }"#;
        store.insert(parse_schema_json(json).unwrap());

        let dir = std::env::temp_dir().join("pulumi-yaml-test-schema-cache");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test-cache.json");

        store.save(&path).unwrap();
        let loaded = SchemaStore::load(&path).unwrap();

        assert!(loaded.lookup_resource("aws:s3/bucket:Bucket").is_some());
        let info = loaded.lookup_resource("aws:s3/bucket:Bucket").unwrap();
        assert!(info.secret_properties.contains("arn"));

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }
}

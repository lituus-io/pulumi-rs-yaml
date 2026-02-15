use std::borrow::Cow;
use std::collections::HashMap;

use crate::eval::value::Value;

/// State of a registered resource.
///
/// Stores the URN, ID, and outputs returned by the engine after registration.
/// Replaces Go's `lateboundResource` interface.
#[derive(Debug, Clone)]
pub struct ResourceState {
    /// The resource's URN (set after registration).
    pub urn: String,
    /// The resource's ID (set after registration, empty for component resources).
    pub id: String,
    /// Whether this is a provider resource (e.g. `pulumi:providers:aws`).
    pub is_provider: bool,
    /// Whether this is a component resource (remote=true).
    pub is_component: bool,
    /// The resolved output properties from the engine.
    pub outputs: HashMap<String, Value<'static>>,
    /// Which output properties are known to be stable.
    pub stables: Vec<String>,
}

impl ResourceState {
    /// Creates a new empty resource state.
    pub fn new() -> Self {
        Self {
            urn: String::new(),
            id: String::new(),
            is_provider: false,
            is_component: false,
            outputs: HashMap::new(),
            stables: Vec::new(),
        }
    }

    /// Gets a named output from this resource.
    pub fn get_output(&self, key: &str) -> Option<&Value<'static>> {
        self.outputs.get(key)
    }

    /// Gets the resource's URN as a Value.
    pub fn urn_value(&self) -> Value<'static> {
        Value::String(Cow::Owned(self.urn.clone()))
    }

    /// Gets the resource's ID as a Value.
    pub fn id_value(&self) -> Value<'static> {
        Value::String(Cow::Owned(self.id.clone()))
    }
}

impl Default for ResourceState {
    fn default() -> Self {
        Self::new()
    }
}

/// A resolved alias for a resource — either a URN string or a structured spec.
#[derive(Debug, Clone, PartialEq)]
pub enum ResolvedAlias {
    /// A simple URN alias.
    Urn(String),
    /// A structured alias spec with optional fields.
    Spec {
        name: String,
        r#type: String,
        stack: String,
        project: String,
        parent_urn: String,
        no_parent: bool,
    },
}

/// Options gathered from a resource declaration for registration.
#[derive(Debug, Clone, Default)]
pub struct ResolvedResourceOptions {
    pub parent_urn: Option<String>,
    pub provider_ref: Option<String>,
    pub depends_on: Vec<String>,
    pub property_dependencies: HashMap<String, Vec<String>>,
    pub delete_before_replace: bool,
    pub ignore_changes: Vec<String>,
    pub protect: bool,
    pub additional_secret_outputs: Vec<String>,
    pub replace_on_changes: Vec<String>,
    pub retain_on_delete: bool,
    pub aliases: Vec<ResolvedAlias>,
    pub import_id: String,
    pub custom_timeouts: Option<(String, String, String)>,
    pub version: String,
    pub plugin_download_url: String,
    /// Provider map: package name → provider reference (urn::id).
    pub providers: HashMap<String, String>,
    /// Resource URNs to replace with.
    pub replace_with: Vec<String>,
    /// Resource URN to delete with.
    pub deleted_with: String,
    /// Package reference for the resource type.
    pub package_ref: String,
    /// Properties to hide diffs for during updates.
    pub hide_diffs: Vec<String>,
}

/// Request to register a resource with the engine.
#[derive(Debug)]
pub struct ResourceRegistration<'a> {
    pub type_token: &'a str,
    pub logical_name: &'a str,
    pub custom: bool,
    pub remote: bool,
    pub inputs: HashMap<String, Value<'static>>,
    pub options: ResolvedResourceOptions,
}

/// Request to read a resource from the engine.
#[derive(Debug)]
pub struct ResourceRead<'a> {
    pub type_token: &'a str,
    pub logical_name: &'a str,
    pub id: String,
    pub parent_urn: String,
    pub inputs: HashMap<String, Value<'static>>,
    pub provider_ref: String,
    pub version: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resource_state_new() {
        let state = ResourceState::new();
        assert!(state.urn.is_empty());
        assert!(state.id.is_empty());
        assert!(!state.is_provider);
        assert!(!state.is_component);
        assert!(state.outputs.is_empty());
    }

    #[test]
    fn test_resource_state_get_output() {
        let mut state = ResourceState::new();
        state.outputs.insert(
            "bucket".to_string(),
            Value::String(Cow::Owned("my-bucket".to_string())),
        );
        assert_eq!(
            state.get_output("bucket").and_then(|v| v.as_str()),
            Some("my-bucket")
        );
        assert!(state.get_output("missing").is_none());
    }

    #[test]
    fn test_resource_state_urn_id_values() {
        let mut state = ResourceState::new();
        state.urn = "urn:pulumi:test::proj::type::name".to_string();
        state.id = "abc-123".to_string();
        assert_eq!(
            state.urn_value().as_str(),
            Some("urn:pulumi:test::proj::type::name")
        );
        assert_eq!(state.id_value().as_str(), Some("abc-123"));
    }

    #[test]
    fn test_resource_state_is_provider() {
        let mut state = ResourceState::new();
        state.is_provider = true;
        assert!(state.is_provider);
    }

    #[test]
    fn test_resolved_resource_options_default() {
        let opts = ResolvedResourceOptions::default();
        assert!(opts.parent_urn.is_none());
        assert!(opts.provider_ref.is_none());
        assert!(opts.depends_on.is_empty());
        assert!(!opts.delete_before_replace);
        assert!(!opts.protect);
        assert!(opts.import_id.is_empty());
    }

    #[test]
    fn test_resource_registration() {
        let reg = ResourceRegistration {
            type_token: "aws:s3:Bucket",
            logical_name: "myBucket",
            custom: true,
            remote: false,
            inputs: HashMap::new(),
            options: ResolvedResourceOptions::default(),
        };
        assert_eq!(reg.type_token, "aws:s3:Bucket");
        assert_eq!(reg.logical_name, "myBucket");
        assert!(reg.custom);
        assert!(!reg.remote);
    }
}

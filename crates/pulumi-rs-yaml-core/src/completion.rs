//! Schema-driven completion API for IDE support.
//!
//! Provides completion items for resource properties based on provider schemas.

use crate::schema::SchemaStore;

/// A single completion item for a resource property.
pub struct CompletionItem<'a> {
    /// Property name.
    pub name: &'a str,
    /// Type label (e.g. "string", "integer", "array").
    pub type_label: &'a str,
    /// Whether this property is required.
    pub required: bool,
    /// Whether this property is secret.
    pub secret: bool,
}

/// Returns completion items for a resource type's input properties.
///
/// Used by IDE integrations (e.g. Python bindings) to provide autocomplete
/// for resource property names.
pub fn complete_resource_properties<'a>(
    store: &'a SchemaStore,
    resource_type: &str,
) -> Vec<CompletionItem<'a>> {
    let Some(info) = store.lookup_resource(resource_type) else {
        return Vec::new();
    };

    let mut items: Vec<CompletionItem<'a>> = info
        .input_property_types
        .iter()
        .map(|(name, prop)| CompletionItem {
            name: name.as_str(),
            type_label: prop.type_.label(),
            required: prop.required,
            secret: prop.secret,
        })
        .collect();

    // Sort: required first, then alphabetical
    items.sort_by(|a, b| b.required.cmp(&a.required).then(a.name.cmp(b.name)));
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{PackageSchema, PropertyInfo, ResourceTypeInfo, SchemaPropertyType};
    use std::collections::HashMap;

    #[test]
    fn test_complete_resource_properties_basic() {
        let mut store = SchemaStore::new();
        let mut info = ResourceTypeInfo::default();
        info.input_property_types.insert(
            "name".to_string(),
            PropertyInfo {
                type_: SchemaPropertyType::String,
                secret: false,
                const_value: None,
                required: true,
            },
        );
        info.input_property_types.insert(
            "tags".to_string(),
            PropertyInfo {
                type_: SchemaPropertyType::Object,
                secret: false,
                const_value: None,
                required: false,
            },
        );
        info.input_property_types.insert(
            "password".to_string(),
            PropertyInfo {
                type_: SchemaPropertyType::String,
                secret: true,
                const_value: None,
                required: true,
            },
        );

        let schema = PackageSchema {
            name: "test".to_string(),
            version: "1.0.0".to_string(),
            resources: [("test:index/res:Res".to_string(), info)]
                .into_iter()
                .collect(),
            functions: HashMap::new(),
        };
        store.insert(schema);

        let items = complete_resource_properties(&store, "test:index/res:Res");
        assert_eq!(items.len(), 3);
        // Required items should come first
        assert!(items[0].required);
        assert!(items[1].required);
        assert!(!items[2].required);
    }

    #[test]
    fn test_complete_resource_properties_missing() {
        let store = SchemaStore::new();
        let items = complete_resource_properties(&store, "missing:index/res:Res");
        assert!(items.is_empty());
    }
}

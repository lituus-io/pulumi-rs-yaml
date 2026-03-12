use crate::ast::expr::Expr;
use crate::syntax::ExprMeta;
use std::borrow::Cow;

/// A Pulumi YAML template declaration - the top-level structure of a Pulumi.yaml program.
#[derive(Debug, Clone, PartialEq)]
pub struct TemplateDecl<'src> {
    pub meta: ExprMeta,
    pub name: Option<Cow<'src, str>>,
    pub namespace: Option<Cow<'src, str>>,
    pub description: Option<Cow<'src, str>>,
    pub pulumi: PulumiDecl<'src>,
    pub config: Vec<ConfigEntry<'src>>,
    pub variables: Vec<VariableEntry<'src>>,
    pub resources: Vec<ResourceEntry<'src>>,
    pub outputs: Vec<OutputEntry<'src>>,
    pub components: Vec<ComponentDecl<'src>>,
}

/// Pulumi settings (e.g. `pulumi: requiredVersion: ">=3.0.0"`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PulumiDecl<'src> {
    pub meta: ExprMeta,
    pub required_version: Option<Expr<'src>>,
}

impl PulumiDecl<'_> {
    pub fn has_settings(&self) -> bool {
        self.required_version.is_some()
    }
}

/// A configuration parameter entry.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigEntry<'src> {
    pub meta: ExprMeta,
    pub key: Cow<'src, str>,
    pub param: ConfigParamDecl<'src>,
}

/// A configuration parameter declaration.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConfigParamDecl<'src> {
    pub type_: Option<Cow<'src, str>>,
    pub name: Option<Cow<'src, str>>,
    pub secret: Option<bool>,
    pub default: Option<Expr<'src>>,
    pub value: Option<Expr<'src>>,
    pub items: Option<Box<ConfigParamDecl<'src>>>,
}

/// A variables map entry.
#[derive(Debug, Clone, PartialEq)]
pub struct VariableEntry<'src> {
    pub meta: ExprMeta,
    pub key: Cow<'src, str>,
    pub value: Expr<'src>,
}

/// A resource map entry.
#[derive(Debug, Clone, PartialEq)]
pub struct ResourceEntry<'src> {
    pub meta: ExprMeta,
    pub logical_name: Cow<'src, str>,
    pub resource: ResourceDecl<'src>,
}

/// A resource declaration.
#[derive(Debug, Clone, PartialEq)]
pub struct ResourceDecl<'src> {
    pub type_: Cow<'src, str>,
    pub name: Option<Cow<'src, str>>,
    pub default_provider: Option<bool>,
    pub properties: ResourceProperties<'src>,
    pub options: ResourceOptionsDecl<'src>,
    pub get: Option<GetResourceDecl<'src>>,
}

/// Resource properties: either an object map or a single expression.
#[derive(Debug, Clone, PartialEq)]
pub enum ResourceProperties<'src> {
    /// Standard properties as key-value pairs.
    Map(Vec<PropertyEntry<'src>>),
    /// A single expression for the properties (e.g., a variable reference).
    Expr(Box<Expr<'src>>),
}

impl Default for ResourceProperties<'_> {
    fn default() -> Self {
        ResourceProperties::Map(Vec::new())
    }
}

/// A property key-value pair within a resource's properties or outputs.
#[derive(Debug, Clone, PartialEq)]
pub struct PropertyEntry<'src> {
    pub key: Cow<'src, str>,
    pub value: Expr<'src>,
}

/// Resource options (dependsOn, protect, provider, etc.).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResourceOptionsDecl<'src> {
    pub additional_secret_outputs: Option<Vec<Cow<'src, str>>>,
    pub aliases: Option<Expr<'src>>,
    pub custom_timeouts: Option<CustomTimeoutsDecl<'src>>,
    pub delete_before_replace: Option<bool>,
    pub depends_on: Option<Expr<'src>>,
    pub ignore_changes: Option<Vec<Cow<'src, str>>>,
    pub import: Option<Cow<'src, str>>,
    pub parent: Option<Expr<'src>>,
    pub protect: Option<Expr<'src>>,
    pub provider: Option<Expr<'src>>,
    pub providers: Option<Expr<'src>>,
    pub version: Option<Cow<'src, str>>,
    pub plugin_download_url: Option<Cow<'src, str>>,
    pub replace_on_changes: Option<Vec<Cow<'src, str>>>,
    pub retain_on_delete: Option<bool>,
    pub replace_with: Option<Expr<'src>>,
    pub deleted_with: Option<Expr<'src>>,
    pub hide_diffs: Option<Vec<Cow<'src, str>>>,
}

/// Custom timeouts for resource operations.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CustomTimeoutsDecl<'src> {
    pub create: Option<Cow<'src, str>>,
    pub update: Option<Cow<'src, str>>,
    pub delete: Option<Cow<'src, str>>,
}

/// Get-resource declaration (for importing existing resources).
#[derive(Debug, Clone, PartialEq)]
pub struct GetResourceDecl<'src> {
    pub id: Expr<'src>,
    pub state: Vec<PropertyEntry<'src>>,
}

/// An output entry.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputEntry<'src> {
    pub key: Cow<'src, str>,
    pub value: Expr<'src>,
}

/// A component declaration.
#[derive(Debug, Clone, PartialEq)]
pub struct ComponentDecl<'src> {
    pub key: Cow<'src, str>,
    pub component: ComponentParamDecl<'src>,
}

/// A component parameter declaration.
#[derive(Debug, Clone, PartialEq)]
pub struct ComponentParamDecl<'src> {
    pub name: Option<Cow<'src, str>>,
    pub description: Option<Cow<'src, str>>,
    pub pulumi: PulumiDecl<'src>,
    pub inputs: Vec<ConfigEntry<'src>>,
    pub variables: Vec<VariableEntry<'src>>,
    pub resources: Vec<ResourceEntry<'src>>,
    pub outputs: Vec<OutputEntry<'src>>,
}

impl TemplateDecl<'_> {
    /// Creates a new empty template.
    pub fn new() -> Self {
        Self {
            meta: ExprMeta::no_span(),
            name: None,
            namespace: None,
            description: None,
            pulumi: PulumiDecl::default(),
            config: Vec::new(),
            variables: Vec::new(),
            resources: Vec::new(),
            outputs: Vec::new(),
            components: Vec::new(),
        }
    }
}

impl Default for TemplateDecl<'_> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_template_default() {
        let t = TemplateDecl::new();
        assert!(t.name.is_none());
        assert!(t.resources.is_empty());
        assert!(t.config.is_empty());
        assert!(t.variables.is_empty());
        assert!(t.outputs.is_empty());
    }

    #[test]
    fn test_pulumi_decl_has_settings() {
        let pd = PulumiDecl::default();
        assert!(!pd.has_settings());

        let pd = PulumiDecl {
            meta: ExprMeta::no_span(),
            required_version: Some(Expr::String(ExprMeta::no_span(), Cow::Borrowed(">=3.0.0"))),
        };
        assert!(pd.has_settings());
    }

    #[test]
    fn test_resource_properties_default() {
        let props = ResourceProperties::default();
        match props {
            ResourceProperties::Map(v) => assert!(v.is_empty()),
            _ => panic!("expected Map"),
        }
    }

    #[test]
    fn test_resource_options_default() {
        let opts = ResourceOptionsDecl::default();
        assert!(opts.depends_on.is_none());
        assert!(opts.protect.is_none());
        assert!(opts.provider.is_none());
    }
}

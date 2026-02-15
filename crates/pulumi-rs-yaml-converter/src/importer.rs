use std::collections::HashMap;
use std::fmt::Write as FmtWrite;

use pulumi_rs_yaml_core::ast::expr::{Expr, InvokeExpr, ObjectProperty};
use pulumi_rs_yaml_core::ast::interpolation::InterpolationPart;
use pulumi_rs_yaml_core::ast::property::{PropertyAccess, PropertyAccessor};
use pulumi_rs_yaml_core::ast::template::*;
use pulumi_rs_yaml_core::diag::Diagnostics;
use pulumi_rs_yaml_core::packages::{canonicalize_type_token, collapse_type_token};
use pulumi_rs_yaml_core::schema::SchemaStore;

use crate::names::{assign_names, AssignedNames};

/// The main YAML→PCL importer.
pub struct Importer {
    /// YAML name → PCL name for each category
    configuration: HashMap<String, String>,
    variables: HashMap<String, String>,
    resources: HashMap<String, String>,
    outputs: HashMap<String, String>,
    components: HashMap<String, String>,
    diags: Diagnostics,
    /// Optional schema store for schema-based token resolution.
    schema_store: Option<SchemaStore>,
}

impl Default for Importer {
    fn default() -> Self {
        Self {
            configuration: HashMap::new(),
            variables: HashMap::new(),
            resources: HashMap::new(),
            outputs: HashMap::new(),
            components: HashMap::new(),
            diags: Diagnostics::new(),
            schema_store: None,
        }
    }
}

impl Importer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates an importer with a schema store for schema-based token resolution.
    pub fn with_schema(schema_store: SchemaStore) -> Self {
        Self {
            schema_store: Some(schema_store),
            ..Self::default()
        }
    }

    /// Returns diagnostics collected during import.
    pub fn diagnostics(self) -> Diagnostics {
        self.diags
    }

    /// Main entry: walks a TemplateDecl and produces PCL text.
    pub fn import_template(&mut self, template: &TemplateDecl<'_>) -> String {
        // Assign names
        let names = assign_names(template);
        self.populate_name_maps(&names);

        let mut w = String::new();
        let mut first = true;

        // Config — sorted alphabetically by key (matching Go behavior)
        let mut config_sorted: Vec<&ConfigEntry<'_>> = template.config.iter().collect();
        config_sorted.sort_by(|a, b| a.key.cmp(&b.key));
        for entry in config_sorted {
            if !first {
                w.push('\n');
            }
            self.import_config(entry, &mut w);
            first = false;
        }

        // Variables — sorted alphabetically by key
        let mut vars_sorted: Vec<&VariableEntry<'_>> = template.variables.iter().collect();
        vars_sorted.sort_by(|a, b| a.key.cmp(&b.key));
        for entry in vars_sorted {
            if !first {
                w.push('\n');
            }
            self.import_variable(entry, &mut w);
            first = false;
        }

        // Resources — sorted alphabetically by logical name
        let mut res_sorted: Vec<&ResourceEntry<'_>> = template.resources.iter().collect();
        res_sorted.sort_by(|a, b| a.logical_name.cmp(&b.logical_name));
        for entry in res_sorted {
            if !first {
                w.push('\n');
            }
            self.import_resource(entry, &mut w);
            first = false;
        }

        // Outputs — sorted alphabetically by key
        let mut out_sorted: Vec<&OutputEntry<'_>> = template.outputs.iter().collect();
        out_sorted.sort_by(|a, b| a.key.cmp(&b.key));
        for entry in out_sorted {
            if !first {
                w.push('\n');
            }
            self.import_output(entry, &mut w);
            first = false;
        }

        // Components — sorted alphabetically by key
        let mut comp_sorted: Vec<&ComponentDecl<'_>> = template.components.iter().collect();
        comp_sorted.sort_by(|a, b| a.key.cmp(&b.key));
        for entry in comp_sorted {
            if !first {
                w.push('\n');
            }
            self.import_component(entry, &mut w);
            first = false;
        }

        w
    }

    fn populate_name_maps(&mut self, names: &AssignedNames) {
        for (yaml, pcl) in &names.configuration {
            self.configuration.insert(yaml.clone(), pcl.clone());
        }
        for (yaml, pcl) in &names.outputs {
            self.outputs.insert(yaml.clone(), pcl.clone());
        }
        for (yaml, pcl) in &names.variables {
            self.variables.insert(yaml.clone(), pcl.clone());
        }
        for (yaml, pcl) in &names.resources {
            self.resources.insert(yaml.clone(), pcl.clone());
        }
        for (yaml, pcl) in &names.components {
            self.components.insert(yaml.clone(), pcl.clone());
        }
    }

    /// Resolves a YAML name to its PCL name, checking all categories.
    fn resolve_name<'a>(&'a self, yaml_name: &'a str) -> &'a str {
        if let Some(n) = self.configuration.get(yaml_name) {
            return n;
        }
        if let Some(n) = self.variables.get(yaml_name) {
            return n;
        }
        if let Some(n) = self.resources.get(yaml_name) {
            return n;
        }
        if let Some(n) = self.outputs.get(yaml_name) {
            return n;
        }
        if let Some(n) = self.components.get(yaml_name) {
            return n;
        }
        yaml_name
    }

    /// Resolves a type token to its canonical form, using schema if available.
    fn resolve_type_token(&self, token: &str) -> String {
        if let Some(ref store) = self.schema_store {
            if let Some(resolved) = store.resolve_resource_token(token) {
                return resolved;
            }
        }
        canonicalize_type_token(token)
    }

    /// Resolves a function token to its canonical form, using schema if available.
    fn resolve_function_token(&self, token: &str) -> String {
        if let Some(ref store) = self.schema_store {
            if let Some(resolved) = store.resolve_function_token(token) {
                return resolved;
            }
        }
        canonicalize_type_token(token)
    }

    // ─── Config ───────────────────────────────────────────────

    fn import_config(&mut self, entry: &ConfigEntry<'_>, w: &mut String) {
        let pcl_name = self
            .configuration
            .get(entry.key.as_ref())
            .cloned()
            .unwrap_or_else(|| entry.key.to_string());

        let pcl_type = entry
            .param
            .type_
            .as_ref()
            .and_then(|t| config_type_to_pcl(t));

        let _ = write!(w, "config {} ", pcl_name);
        if let Some(ref t) = pcl_type {
            let _ = write!(w, "{} ", t);
        } else {
            w.push_str("string ");
        }

        w.push_str("{\n");
        let _ = writeln!(w, "\t__logicalName = \"{}\"", escape_string(&entry.key));

        if let Some(ref default_val) = entry.param.default {
            let pcl = self.expr_to_pcl(default_val, 1);
            let _ = writeln!(w, "\tdefault = {}", pcl);
        }

        if entry.param.secret == Some(true) {
            let _ = writeln!(w, "\tsecret = true");
        }

        w.push_str("}\n");
    }

    // ─── Variables ────────────────────────────────────────────

    fn import_variable(&mut self, entry: &VariableEntry<'_>, w: &mut String) {
        let pcl_name = self
            .variables
            .get(entry.key.as_ref())
            .cloned()
            .unwrap_or_else(|| entry.key.to_string());

        let pcl = self.expr_to_pcl(&entry.value, 0);
        let _ = writeln!(w, "{} = {}", pcl_name, pcl);
    }

    // ─── Resources ────────────────────────────────────────────

    fn import_resource(&mut self, entry: &ResourceEntry<'_>, w: &mut String) {
        let pcl_name = self
            .resources
            .get(entry.logical_name.as_ref())
            .cloned()
            .unwrap_or_else(|| entry.logical_name.to_string());

        let canonical_token = self.resolve_type_token(&entry.resource.type_);
        let display_token = collapse_type_token(&canonical_token);

        let _ = writeln!(w, "resource {} \"{}\" {{", pcl_name, display_token);

        // __logicalName
        let _ = writeln!(
            w,
            "\t__logicalName = \"{}\"",
            escape_string(&entry.logical_name)
        );

        // Properties
        match &entry.resource.properties {
            ResourceProperties::Map(props) => {
                for prop in props {
                    let pcl = self.expr_to_pcl(&prop.value, 1);
                    let _ = writeln!(w, "\t{} = {}", prop.key, pcl);
                }
            }
            ResourceProperties::Expr(expr) => {
                // Expression-style properties — unusual but supported
                let pcl = self.expr_to_pcl(expr, 1);
                let _ = writeln!(w, "\tproperties = {}", pcl);
            }
        }

        // Options block
        self.import_resource_options(&entry.resource.options, w);

        w.push_str("}\n");
    }

    fn import_resource_options(&mut self, opts: &ResourceOptionsDecl<'_>, w: &mut String) {
        let mut options_buf = String::new();

        // dependsOn
        if let Some(ref deps) = opts.depends_on {
            match deps {
                Expr::List(_, items) => {
                    if items.len() == 1 {
                        let dep = self.expr_to_bare_traversal(&items[0]);
                        let _ = writeln!(options_buf, "\t\tdependsOn = [{}]", dep);
                    } else if !items.is_empty() {
                        options_buf.push_str("\t\tdependsOn = [\n");
                        for item in items {
                            let dep = self.expr_to_bare_traversal(item);
                            let _ = writeln!(options_buf, "\t\t\t{},", dep);
                        }
                        options_buf.push_str("\t\t]\n");
                    }
                }
                _ => {
                    let dep = self.expr_to_pcl(deps, 2);
                    let _ = writeln!(options_buf, "\t\tdependsOn = {}", dep);
                }
            }
        }

        // protect
        if let Some(ref protect) = opts.protect {
            let pcl = self.expr_to_pcl(protect, 2);
            let _ = writeln!(options_buf, "\t\tprotect = {}", pcl);
        }

        // provider
        if let Some(ref provider) = opts.provider {
            let pcl = self.expr_to_bare_traversal(provider);
            let _ = writeln!(options_buf, "\t\tprovider = {}", pcl);
        }

        // ignoreChanges
        if let Some(ref changes) = opts.ignore_changes {
            if changes.len() == 1 {
                let _ = writeln!(options_buf, "\t\tignoreChanges = [{}]", changes[0]);
            } else if !changes.is_empty() {
                options_buf.push_str("\t\tignoreChanges = [\n");
                for change in changes {
                    let _ = writeln!(options_buf, "\t\t\t{},", change);
                }
                options_buf.push_str("\t\t]\n");
            }
        }

        // version
        if let Some(ref version) = opts.version {
            let _ = writeln!(options_buf, "\t\tversion = \"{}\"", escape_string(version));
        }

        // pluginDownloadURL
        if let Some(ref url) = opts.plugin_download_url {
            let _ = writeln!(
                options_buf,
                "\t\tpluginDownloadUrl = \"{}\"",
                escape_string(url)
            );
        }

        // parent
        if let Some(ref parent) = opts.parent {
            let pcl = self.expr_to_bare_traversal(parent);
            let _ = writeln!(options_buf, "\t\tparent = {}", pcl);
        }

        // import
        if let Some(ref import_id) = opts.import {
            let _ = writeln!(options_buf, "\t\timport = \"{}\"", escape_string(import_id));
        }

        // retainOnDelete
        if opts.retain_on_delete == Some(true) {
            let _ = writeln!(options_buf, "\t\tretainOnDelete = true");
        }

        // deletedWith
        if let Some(ref deleted_with) = opts.deleted_with {
            let pcl = self.expr_to_bare_traversal(deleted_with);
            let _ = writeln!(options_buf, "\t\tdeletedWith = {}", pcl);
        }

        // replaceOnChanges
        if let Some(ref changes) = opts.replace_on_changes {
            if changes.len() == 1 {
                let _ = writeln!(options_buf, "\t\treplaceOnChanges = [{}]", changes[0]);
            } else if !changes.is_empty() {
                options_buf.push_str("\t\treplaceOnChanges = [\n");
                for change in changes {
                    let _ = writeln!(options_buf, "\t\t\t{},", change);
                }
                options_buf.push_str("\t\t]\n");
            }
        }

        // hideDiffs
        if let Some(ref diffs) = opts.hide_diffs {
            if diffs.len() == 1 {
                let _ = writeln!(options_buf, "\t\thideDiffs = [{}]", diffs[0]);
            } else if !diffs.is_empty() {
                options_buf.push_str("\t\thideDiffs = [\n");
                for diff in diffs {
                    let _ = writeln!(options_buf, "\t\t\t{},", diff);
                }
                options_buf.push_str("\t\t]\n");
            }
        }

        if !options_buf.is_empty() {
            w.push('\n');
            w.push_str("\toptions {\n");
            w.push_str(&options_buf);
            w.push_str("\t}\n");
        }
    }

    // ─── Outputs ──────────────────────────────────────────────

    fn import_output(&mut self, entry: &OutputEntry<'_>, w: &mut String) {
        let pcl_name = self
            .outputs
            .get(entry.key.as_ref())
            .cloned()
            .unwrap_or_else(|| entry.key.to_string());

        let _ = writeln!(w, "output {} {{", pcl_name);
        let _ = writeln!(w, "\t__logicalName = \"{}\"", escape_string(&entry.key));
        let pcl = self.expr_to_pcl(&entry.value, 1);
        let _ = writeln!(w, "\tvalue = {}", pcl);
        w.push_str("}\n");
    }

    // ─── Components ──────────────────────────────────────────

    fn import_component(&mut self, decl: &ComponentDecl<'_>, w: &mut String) {
        let pcl_name = self
            .components
            .get(decl.key.as_ref())
            .cloned()
            .unwrap_or_else(|| decl.key.to_string());

        // Component block: component <name> "<path>" { ... }
        // The path comes from the component's resources/variables/etc.
        // In YAML, components are inline definitions with nested resources.
        let _ = writeln!(w, "component {} \"./{}\" {{", pcl_name, decl.key);

        // __logicalName
        let _ = writeln!(w, "\t__logicalName = \"{}\"", escape_string(&decl.key));

        // Inputs (config entries)
        for input in &decl.component.inputs {
            let pcl_type = input
                .param
                .type_
                .as_ref()
                .and_then(|t| config_type_to_pcl(t))
                .unwrap_or_else(|| "string".to_string());
            if let Some(ref default_val) = input.param.default {
                let pcl = self.expr_to_pcl(default_val, 1);
                let _ = writeln!(w, "\t{} = {} // type: {}", input.key, pcl, pcl_type);
            } else if let Some(ref val) = input.param.value {
                let pcl = self.expr_to_pcl(val, 1);
                let _ = writeln!(w, "\t{} = {}", input.key, pcl);
            }
        }

        w.push_str("}\n");
    }

    // ─── Expressions ──────────────────────────────────────────

    fn expr_to_pcl(&mut self, expr: &Expr<'_>, indent: usize) -> String {
        match expr {
            Expr::Null(_) => "null".to_string(),
            Expr::Bool(_, b) => b.to_string(),
            Expr::Number(_, n) => format_number(*n),
            Expr::String(_, s) => format!("\"{}\"", escape_string(s)),

            Expr::Symbol(_, access) => self.property_access_to_pcl(access),

            Expr::Interpolate(_, parts) => self.interpolation_to_pcl(parts),

            Expr::List(_, items) => self.list_to_pcl(items, indent),
            Expr::Object(_, entries) => self.object_to_pcl(entries, indent),

            Expr::Invoke(_, invoke) => self.invoke_to_pcl(invoke),

            Expr::Join(_, delim, values) => {
                let d = self.expr_to_pcl(delim, indent);
                let v = self.expr_to_pcl(values, indent);
                format!("join({}, {})", d, v)
            }
            Expr::Select(_, idx, values) => {
                // fn::select → IndexExpression: values[idx]
                let v = self.expr_to_pcl(values, indent);
                let i = self.expr_to_pcl(idx, indent);
                format!("{}[{}]", v, i)
            }
            Expr::Split(_, delim, source) => {
                let d = self.expr_to_pcl(delim, indent);
                let s = self.expr_to_pcl(source, indent);
                format!("split({}, {})", d, s)
            }
            Expr::ToJson(_, inner) => {
                let v = self.expr_to_pcl(inner, indent);
                format!("toJSON({})", v)
            }
            Expr::ToBase64(_, inner) => {
                let v = self.expr_to_pcl(inner, indent);
                format!("toBase64({})", v)
            }
            Expr::FromBase64(_, inner) => {
                let v = self.expr_to_pcl(inner, indent);
                format!("fromBase64({})", v)
            }
            Expr::Secret(_, inner) => {
                let v = self.expr_to_pcl(inner, indent);
                format!("secret({})", v)
            }
            Expr::ReadFile(_, inner) => {
                let v = self.expr_to_pcl(inner, indent);
                format!("readFile({})", v)
            }

            // Assets and archives
            Expr::StringAsset(_, inner) => {
                let v = self.expr_to_pcl(inner, indent);
                format!("stringAsset({})", v)
            }
            Expr::FileAsset(_, inner) => {
                let v = self.expr_to_pcl(inner, indent);
                format!("fileAsset({})", v)
            }
            Expr::RemoteAsset(_, inner) => {
                let v = self.expr_to_pcl(inner, indent);
                format!("remoteAsset({})", v)
            }
            Expr::FileArchive(_, inner) => {
                let v = self.expr_to_pcl(inner, indent);
                format!("fileArchive({})", v)
            }
            Expr::RemoteArchive(_, inner) => {
                let v = self.expr_to_pcl(inner, indent);
                format!("remoteArchive({})", v)
            }
            Expr::AssetArchive(_, entries) => self.asset_archive_to_pcl(entries, indent),

            // Rust-only builtins — emit warning + null
            Expr::Abs(_, _)
            | Expr::Floor(_, _)
            | Expr::Ceil(_, _)
            | Expr::Max(_, _)
            | Expr::Min(_, _)
            | Expr::StringLen(_, _)
            | Expr::Substring(_, _, _, _)
            | Expr::TimeUtc(_, _)
            | Expr::TimeUnix(_, _)
            | Expr::Uuid(_, _)
            | Expr::RandomString(_, _)
            | Expr::DateFormat(_, _) => {
                let name = rust_only_builtin_name(expr);
                self.diags.warning(
                    None,
                    format!("unsupported builtin 'fn::{}' in PCL conversion", name),
                    "this builtin is not available in standard PCL and will be emitted as null",
                );
                "null /* unsupported builtin */".to_string()
            }
        }
    }

    fn property_access_to_pcl(&self, access: &PropertyAccess<'_>) -> String {
        let mut result = String::new();
        for (i, accessor) in access.accessors.iter().enumerate() {
            match accessor {
                PropertyAccessor::Name(name) => {
                    if i == 0 {
                        let root = name.as_ref();
                        // Check for pulumi.* special variables
                        if root == "pulumi" {
                            return self.pulumi_access_to_pcl(access);
                        }
                        result.push_str(self.resolve_name(root));
                    } else {
                        result.push('.');
                        result.push_str(name);
                    }
                }
                PropertyAccessor::StringSubscript(key) => {
                    let _ = write!(result, "[\"{}\"]", escape_string(key));
                }
                PropertyAccessor::IntSubscript(idx) => {
                    let _ = write!(result, "[{}]", idx);
                }
            }
        }
        result
    }

    fn pulumi_access_to_pcl(&self, access: &PropertyAccess<'_>) -> String {
        if access.accessors.len() < 2 {
            self.diags.iter().count(); // just to avoid unused warning
            return "null".to_string();
        }

        let prop_name = match &access.accessors[1] {
            PropertyAccessor::Name(n) => n.as_ref(),
            _ => return "null".to_string(),
        };

        match prop_name {
            "cwd" => "cwd()".to_string(),
            "project" => "project()".to_string(),
            "stack" => "stack()".to_string(),
            "organization" => "organization()".to_string(),
            "rootDirectory" => "rootDirectory()".to_string(),
            other => {
                // Unknown pulumi property — we should emit a diagnostic
                // but since self is not &mut here in this call chain,
                // we'll return a placeholder
                format!("null /* unknown pulumi.{} */", other)
            }
        }
    }

    fn interpolation_to_pcl(&mut self, parts: &[InterpolationPart<'_>]) -> String {
        // Special case: single interpolation with no text → bare traversal
        if parts.len() == 1 && parts[0].text.is_empty() {
            if let Some(ref access) = parts[0].value {
                return self.property_access_to_pcl(access);
            }
        }

        // Build interpolated string
        let mut result = String::from("\"");
        for part in parts {
            result.push_str(&escape_string(&part.text));
            if let Some(ref access) = part.value {
                result.push_str("${");
                result.push_str(&self.property_access_to_pcl(access));
                result.push('}');
            }
        }
        result.push('"');
        result
    }

    fn list_to_pcl(&mut self, items: &[Expr<'_>], indent: usize) -> String {
        if items.is_empty() {
            return "[]".to_string();
        }
        if items.len() == 1 {
            let v = self.expr_to_pcl(&items[0], indent + 1);
            return format!("[{}]", v);
        }

        let tabs = "\t".repeat(indent);
        let inner_tabs = "\t".repeat(indent + 1);
        let mut result = String::from("[\n");
        for item in items {
            let v = self.expr_to_pcl(item, indent + 1);
            let _ = writeln!(result, "{}{},", inner_tabs, v);
        }
        let _ = write!(result, "{}]", tabs);
        result
    }

    fn object_to_pcl(&mut self, entries: &[ObjectProperty<'_>], indent: usize) -> String {
        if entries.is_empty() {
            return "{}".to_string();
        }

        let tabs = "\t".repeat(indent);
        let inner_tabs = "\t".repeat(indent + 1);
        let mut result = String::from("{\n");
        for entry in entries {
            let key = self.object_key_to_pcl(&entry.key);
            let val = self.expr_to_pcl(&entry.value, indent + 1);
            let _ = writeln!(result, "{}{} = {}", inner_tabs, key, val);
        }
        let _ = write!(result, "{}}}", tabs);
        result
    }

    fn object_key_to_pcl(&mut self, key: &Expr<'_>) -> String {
        match key {
            Expr::String(_, s) => {
                // If it's a valid identifier, use it bare; otherwise quote
                if is_valid_pcl_attr(s) {
                    s.to_string()
                } else {
                    format!("\"{}\"", escape_string(s))
                }
            }
            _ => self.expr_to_pcl(key, 0),
        }
    }

    fn invoke_to_pcl(&mut self, invoke: &InvokeExpr<'_>) -> String {
        let canonical = self.resolve_function_token(&invoke.token);
        let display = collapse_type_token(&canonical);

        let args = if let Some(ref call_args) = invoke.call_args {
            self.expr_to_pcl(call_args, 0)
        } else {
            "{}".to_string()
        };

        // Build options
        let opts = self.invoke_options_to_pcl(&invoke.call_opts);

        let mut result = format!("invoke(\"{}\", {}{})", display, args, opts);

        // Return directive
        if let Some(ref ret) = invoke.return_ {
            let _ = write!(result, ".{}", ret);
        }

        result
    }

    fn invoke_options_to_pcl(
        &mut self,
        opts: &pulumi_rs_yaml_core::ast::expr::InvokeOptions<'_>,
    ) -> String {
        let mut entries = Vec::new();

        if let Some(ref parent) = opts.parent {
            let pcl = self.expr_to_bare_traversal(parent);
            entries.push(format!("\tparent = {}", pcl));
        }
        if let Some(ref provider) = opts.provider {
            let pcl = self.expr_to_bare_traversal(provider);
            entries.push(format!("\tprovider = {}", pcl));
        }
        if let Some(ref version) = opts.version {
            entries.push(format!("\tversion = \"{}\"", escape_string(version)));
        }
        if let Some(ref url) = opts.plugin_download_url {
            entries.push(format!("\tpluginDownloadUrl = \"{}\"", escape_string(url)));
        }

        if entries.is_empty() {
            return String::new();
        }

        let mut result = String::from(", {\n");
        for (i, entry) in entries.iter().enumerate() {
            result.push_str(entry);
            if i < entries.len() - 1 {
                result.push(',');
            }
            result.push('\n');
        }
        result.push('}');
        result
    }

    fn asset_archive_to_pcl(
        &mut self,
        entries: &[(std::borrow::Cow<'_, str>, Expr<'_>)],
        indent: usize,
    ) -> String {
        if entries.is_empty() {
            return "assetArchive({})".to_string();
        }

        let inner_tabs = "\t".repeat(indent + 1);
        let tabs = "\t".repeat(indent);
        let mut result = String::from("assetArchive({\n");
        for (key, value) in entries {
            let v = self.expr_to_pcl(value, indent + 1);
            let _ = writeln!(result, "{}\"{}\" = {}", inner_tabs, escape_string(key), v);
        }
        let _ = write!(result, "{}}})", tabs);
        result
    }

    /// Converts an expression to a bare traversal (no quotes around symbols).
    /// Used for resource references in options like `provider = myProvider`.
    fn expr_to_bare_traversal(&mut self, expr: &Expr<'_>) -> String {
        match expr {
            Expr::Symbol(_, access) => self.property_access_to_pcl(access),
            Expr::Interpolate(_, parts) => {
                // If single interpolation → bare traversal
                if parts.len() == 1 && parts[0].text.is_empty() {
                    if let Some(ref access) = parts[0].value {
                        return self.property_access_to_pcl(access);
                    }
                }
                self.expr_to_pcl(expr, 0)
            }
            _ => self.expr_to_pcl(expr, 0),
        }
    }
}

// ─── Helpers ──────────────────────────────────────────────────

/// Returns true if `s` is a valid PCL attribute name (doesn't need quoting).
fn is_valid_pcl_attr(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Formats a number for PCL output (integers without decimals).
fn format_number(n: f64) -> String {
    if n == n.trunc() && n.is_finite() {
        format!("{}", n as i64)
    } else {
        format!("{}", n)
    }
}

/// Escapes a string for PCL output.
fn escape_string(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => result.push_str("\\\\"),
            '"' => result.push_str("\\\""),
            '\n' => result.push_str("\\n"),
            '\t' => result.push_str("\\t"),
            '\r' => result.push_str("\\r"),
            _ => result.push(c),
        }
    }
    result
}

/// Maps a YAML config type to its PCL type string.
fn config_type_to_pcl(yaml_type: &str) -> Option<String> {
    let lower = yaml_type.to_lowercase();

    // Check for list<...> syntax
    if lower.starts_with("list<") && lower.ends_with('>') {
        let inner = &yaml_type[5..yaml_type.len() - 1];
        let inner_pcl = config_type_to_pcl(inner)?;
        return Some(format!("list({})", inner_pcl));
    }

    match lower.as_str() {
        "string" => Some("string".to_string()),
        "int" | "integer" => Some("int".to_string()),
        "number" => Some("number".to_string()),
        "bool" | "boolean" => Some("bool".to_string()),
        "dynamic" | "any" => Some("any".to_string()),
        "object" => Some("map(any)".to_string()),
        _ => None,
    }
}

/// Returns the name of a Rust-only builtin for diagnostics.
fn rust_only_builtin_name(expr: &Expr<'_>) -> &'static str {
    match expr {
        Expr::Abs(_, _) => "abs",
        Expr::Floor(_, _) => "floor",
        Expr::Ceil(_, _) => "ceil",
        Expr::Max(_, _) => "max",
        Expr::Min(_, _) => "min",
        Expr::StringLen(_, _) => "stringLen",
        Expr::Substring(_, _, _, _) => "substring",
        Expr::TimeUtc(_, _) => "timeUtc",
        Expr::TimeUnix(_, _) => "timeUnix",
        Expr::Uuid(_, _) => "uuid",
        Expr::RandomString(_, _) => "randomString",
        Expr::DateFormat(_, _) => "dateFormat",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_number_integer() {
        assert_eq!(format_number(42.0), "42");
        assert_eq!(format_number(0.0), "0");
        assert_eq!(format_number(-1.0), "-1");
    }

    #[test]
    fn test_format_number_float() {
        assert_eq!(format_number(2.75), "2.75");
    }

    #[test]
    fn test_escape_string() {
        assert_eq!(escape_string("hello"), "hello");
        assert_eq!(escape_string("a\"b"), "a\\\"b");
        assert_eq!(escape_string("a\nb"), "a\\nb");
        assert_eq!(escape_string("a\\b"), "a\\\\b");
    }

    #[test]
    fn test_config_type_to_pcl() {
        assert_eq!(config_type_to_pcl("string"), Some("string".to_string()));
        assert_eq!(config_type_to_pcl("int"), Some("int".to_string()));
        assert_eq!(config_type_to_pcl("integer"), Some("int".to_string()));
        assert_eq!(config_type_to_pcl("number"), Some("number".to_string()));
        assert_eq!(config_type_to_pcl("bool"), Some("bool".to_string()));
        assert_eq!(config_type_to_pcl("boolean"), Some("bool".to_string()));
        assert_eq!(config_type_to_pcl("object"), Some("map(any)".to_string()));
        assert_eq!(
            config_type_to_pcl("List<string>"),
            Some("list(string)".to_string())
        );
        assert_eq!(
            config_type_to_pcl("List<int>"),
            Some("list(int)".to_string())
        );
        assert_eq!(config_type_to_pcl("unknown"), None);
    }

    #[test]
    fn test_is_valid_pcl_attr() {
        assert!(is_valid_pcl_attr("foo"));
        assert!(is_valid_pcl_attr("_bar"));
        assert!(is_valid_pcl_attr("foo123"));
        assert!(!is_valid_pcl_attr(""));
        assert!(!is_valid_pcl_attr("123foo"));
        assert!(!is_valid_pcl_attr("foo-bar"));
    }

    #[test]
    fn test_basic_import() {
        use pulumi_rs_yaml_core::ast::parse::parse_template;

        let yaml = r#"
name: test
runtime: yaml
resources:
  bar:
    type: test:mod:Typ
    properties:
      foo: hello
"#;
        let (template, _) = parse_template(yaml, None);
        let mut importer = Importer::new();
        let pcl = importer.import_template(&template);

        // test:mod:Typ → canonicalize → test:mod/typ:Typ → collapse → test:mod:Typ
        assert!(
            pcl.contains("resource bar \"test:mod:Typ\""),
            "got:\n{}",
            pcl
        );
        assert!(pcl.contains("__logicalName = \"bar\""), "got:\n{}", pcl);
        assert!(pcl.contains("foo = \"hello\""), "got:\n{}", pcl);
    }

    #[test]
    fn test_config_import() {
        use pulumi_rs_yaml_core::ast::parse::parse_template;

        let yaml = r#"
name: test
runtime: yaml
config:
  name:
    type: string
    default: world
"#;
        let (template, _) = parse_template(yaml, None);
        let mut importer = Importer::new();
        let pcl = importer.import_template(&template);

        assert!(pcl.contains("config name string"));
        assert!(pcl.contains("__logicalName = \"name\""));
        assert!(pcl.contains("default = \"world\""));
    }

    #[test]
    fn test_output_import() {
        use pulumi_rs_yaml_core::ast::parse::parse_template;

        let yaml = r#"
name: test
runtime: yaml
resources:
  bucket:
    type: aws:s3:Bucket
outputs:
  bucketName: ${bucket.id}
"#;
        let (template, _) = parse_template(yaml, None);
        let mut importer = Importer::new();
        let pcl = importer.import_template(&template);

        assert!(pcl.contains("output bucketName {"));
        assert!(pcl.contains("__logicalName = \"bucketName\""));
        assert!(pcl.contains("value = bucket.id"));
    }

    #[test]
    fn test_variable_import() {
        use pulumi_rs_yaml_core::ast::parse::parse_template;

        let yaml = r#"
name: test
runtime: yaml
variables:
  encoded:
    fn::toBase64: hello
"#;
        let (template, _) = parse_template(yaml, None);
        let mut importer = Importer::new();
        let pcl = importer.import_template(&template);

        assert!(pcl.contains("encoded = toBase64(\"hello\")"));
    }

    #[test]
    fn test_pulumi_variables() {
        use pulumi_rs_yaml_core::ast::parse::parse_template;

        let yaml = r#"
name: test
runtime: yaml
resources:
  bar:
    type: test:mod:typ
    properties:
      foo: ${pulumi.cwd}
"#;
        let (template, _) = parse_template(yaml, None);
        let mut importer = Importer::new();
        let pcl = importer.import_template(&template);

        assert!(pcl.contains("foo = cwd()"));
    }
}

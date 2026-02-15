//! PCL→YAML generator.
//!
//! Converts PCL (HCL2) source to Pulumi YAML format.
//! Used by the `GenerateProgram` RPC for `pulumi convert --to yaml`.

use std::collections::HashMap;

use crate::diag::Diagnostics;

/// Result of generating a YAML program from PCL sources.
pub struct GenerateResult {
    /// Generated files (filename → content bytes).
    pub files: HashMap<String, Vec<u8>>,
    /// Diagnostics from the generation process.
    pub diagnostics: Diagnostics,
}

/// Generates a Pulumi YAML program from PCL (HCL2) sources.
///
/// The `sources` map is keyed by filename with PCL text as values.
/// Typically there's a single entry `("main.pp", pcl_text)`.
pub fn generate_program(sources: &HashMap<String, String>) -> GenerateResult {
    let mut gen = PclToYamlGenerator::new();

    for (filename, source) in sources {
        gen.process_source(filename, source);
    }

    gen.finish()
}

struct PclToYamlGenerator {
    config: Vec<serde_yaml::Value>,
    resources: Vec<(String, serde_yaml::Value)>,
    variables: Vec<(String, serde_yaml::Value)>,
    outputs: Vec<(String, serde_yaml::Value)>,
    diags: Diagnostics,
}

impl PclToYamlGenerator {
    fn new() -> Self {
        Self {
            config: Vec::new(),
            resources: Vec::new(),
            variables: Vec::new(),
            outputs: Vec::new(),
            diags: Diagnostics::new(),
        }
    }

    fn process_source(&mut self, filename: &str, source: &str) {
        let body = match hcl::parse(source) {
            Ok(body) => body,
            Err(e) => {
                self.diags
                    .error(None, format!("failed to parse {}: {}", filename, e), "");
                return;
            }
        };

        self.process_body(&body);
    }

    fn process_body(&mut self, body: &hcl::Body) {
        for structure in body.iter() {
            match structure {
                hcl::Structure::Attribute(attr) => {
                    // Top-level attributes are variables
                    let key = attr.key.to_string();
                    let value = self.expr_to_yaml(&attr.expr);
                    self.variables.push((key, value));
                }
                hcl::Structure::Block(block) => {
                    let ident = block.identifier.to_string();
                    match ident.as_str() {
                        "config" => self.gen_config(block),
                        "resource" => self.gen_resource(block),
                        "output" => self.gen_output(block),
                        "component" => self.gen_component(block),
                        _ => {
                            self.diags.warning(
                                None,
                                format!("unknown block type '{}'", ident),
                                "block will be skipped",
                            );
                        }
                    }
                }
            }
        }
    }

    fn gen_config(&mut self, block: &hcl::Block) {
        // config <name> <type> { default = <value>; secret = true }
        let pcl_name = block
            .labels
            .first()
            .map(label_to_string)
            .unwrap_or_default();
        let type_str = block
            .labels
            .get(1)
            .map(label_to_string)
            .unwrap_or_else(|| "string".to_string());

        let mut entry = serde_yaml::Mapping::new();

        // Map PCL types to YAML types
        let yaml_type = pcl_type_to_yaml(&type_str);
        entry.insert(
            serde_yaml::Value::String("type".to_string()),
            serde_yaml::Value::String(yaml_type),
        );

        // Check for __logicalName — if present, emit name: field and use pcl_name as key
        let logical_name = find_logical_name(&block.body);
        if let Some(ref ln) = logical_name {
            entry.insert(
                serde_yaml::Value::String("name".to_string()),
                serde_yaml::Value::String(ln.clone()),
            );
        }

        // Check for default value
        if let Some(default_attr) = block.body.iter().find_map(|s| {
            if let hcl::Structure::Attribute(a) = s {
                if a.key.to_string() == "default" {
                    return Some(a);
                }
            }
            None
        }) {
            let value = self.expr_to_yaml(&default_attr.expr);
            entry.insert(serde_yaml::Value::String("default".to_string()), value);
        }

        // Check for secret
        if let Some(secret_attr) = block.body.iter().find_map(|s| {
            if let hcl::Structure::Attribute(a) = s {
                if a.key.to_string() == "secret" {
                    return Some(a);
                }
            }
            None
        }) {
            if let hcl::Expression::Bool(true) = &secret_attr.expr {
                entry.insert(
                    serde_yaml::Value::String("secret".to_string()),
                    serde_yaml::Value::Bool(true),
                );
            }
        }

        // Use pcl_name as the key (matching Go behavior)
        let mut config_map = serde_yaml::Mapping::new();
        config_map.insert(
            serde_yaml::Value::String(pcl_name),
            serde_yaml::Value::Mapping(entry),
        );
        self.config.push(serde_yaml::Value::Mapping(config_map));
    }

    fn gen_resource(&mut self, block: &hcl::Block) {
        // resource <name> "<type>" { properties... options { ... } }
        let pcl_name = block
            .labels
            .first()
            .map(label_to_string)
            .unwrap_or_default();
        let type_token = block.labels.get(1).map(label_to_string).unwrap_or_default();

        let mut resource = serde_yaml::Mapping::new();
        resource.insert(
            serde_yaml::Value::String("type".to_string()),
            serde_yaml::Value::String(collapse_token(&type_token)),
        );

        let logical_name = find_logical_name(&block.body);

        // If __logicalName differs from pcl_name, emit name: field (matching Go behavior)
        if let Some(ref ln) = logical_name {
            resource.insert(
                serde_yaml::Value::String("name".to_string()),
                serde_yaml::Value::String(ln.clone()),
            );
        }

        // Collect properties and options
        let mut properties = serde_yaml::Mapping::new();
        let mut options = serde_yaml::Mapping::new();

        for structure in block.body.iter() {
            match structure {
                hcl::Structure::Attribute(attr) => {
                    let key = attr.key.to_string();
                    if key == "__logicalName" {
                        continue; // Already handled
                    }
                    // Strip secret() wrapper (Go strips it — secrets come from schema at runtime)
                    let value = if is_secret_call(&attr.expr) {
                        self.expr_to_yaml(unwrap_secret_call(&attr.expr))
                    } else {
                        self.expr_to_yaml(&attr.expr)
                    };
                    properties.insert(serde_yaml::Value::String(key), value);
                }
                hcl::Structure::Block(inner_block) => {
                    if inner_block.identifier.to_string() == "options" {
                        self.gen_resource_options(&inner_block.body, &mut options);
                    }
                }
            }
        }

        if !properties.is_empty() {
            resource.insert(
                serde_yaml::Value::String("properties".to_string()),
                serde_yaml::Value::Mapping(properties),
            );
        }

        if !options.is_empty() {
            resource.insert(
                serde_yaml::Value::String("options".to_string()),
                serde_yaml::Value::Mapping(options),
            );
        }

        // Use pcl_name as the key (matching Go: n.Name() is the PCL identifier)
        self.resources
            .push((pcl_name, serde_yaml::Value::Mapping(resource)));
    }

    fn gen_resource_options(&mut self, body: &hcl::Body, options: &mut serde_yaml::Mapping) {
        for structure in body.iter() {
            if let hcl::Structure::Attribute(attr) = structure {
                let key = attr.key.to_string();

                let value = match key.as_str() {
                    "dependsOn" => self.expr_to_yaml_refs(&attr.expr),
                    "provider" | "parent" | "deletedWith" => self.expr_to_yaml_ref(&attr.expr),
                    _ => self.expr_to_yaml(&attr.expr),
                };

                options.insert(serde_yaml::Value::String(key), value);
            }
        }
    }

    fn gen_output(&mut self, block: &hcl::Block) {
        // output <name> { value = <expr> }
        let pcl_name = block
            .labels
            .first()
            .map(label_to_string)
            .unwrap_or_default();

        let logical_name = find_logical_name(&block.body);

        let value = block.body.iter().find_map(|s| {
            if let hcl::Structure::Attribute(a) = s {
                if a.key.to_string() == "value" {
                    return Some(self.expr_to_yaml(&a.expr));
                }
            }
            None
        });

        let output_key = logical_name.unwrap_or(pcl_name);
        if let Some(v) = value {
            self.outputs.push((output_key, v));
        }
    }

    fn gen_component(&mut self, block: &hcl::Block) {
        // component <name> "<path>" { properties... }
        // For now, emit as a warning — components in PCL→YAML are complex
        let pcl_name = block
            .labels
            .first()
            .map(label_to_string)
            .unwrap_or_default();

        self.diags.warning(
            None,
            format!(
                "component '{}' in PCL cannot be fully converted to YAML",
                pcl_name
            ),
            "components require manual conversion",
        );
    }

    // ─── Expression conversion ──────────────────────────────

    fn expr_to_yaml(&mut self, expr: &hcl::Expression) -> serde_yaml::Value {
        match expr {
            hcl::Expression::Null => serde_yaml::Value::Null,
            hcl::Expression::Bool(b) => serde_yaml::Value::Bool(*b),
            hcl::Expression::Number(n) => {
                if let Some(i) = n.as_i64() {
                    serde_yaml::Value::Number(serde_yaml::Number::from(i))
                } else if let Some(f) = n.as_f64() {
                    serde_yaml::Value::Number(serde_yaml::Number::from(f))
                } else {
                    serde_yaml::Value::String(n.to_string())
                }
            }
            hcl::Expression::String(s) => serde_yaml::Value::String(s.clone()),
            hcl::Expression::Array(items) => {
                let yaml_items: Vec<serde_yaml::Value> =
                    items.iter().map(|e| self.expr_to_yaml(e)).collect();
                serde_yaml::Value::Sequence(yaml_items)
            }
            hcl::Expression::Object(obj) => {
                let mut map = serde_yaml::Mapping::new();
                for (k, v) in obj.iter() {
                    let key = self.object_key_to_yaml(k);
                    let val = self.expr_to_yaml(v);
                    map.insert(key, val);
                }
                serde_yaml::Value::Mapping(map)
            }
            hcl::Expression::Variable(var) => {
                // Variable references → ${varName}
                let name = var.as_str();
                serde_yaml::Value::String(format!("${{{}}}", name))
            }
            hcl::Expression::Traversal(traversal) => self.traversal_to_yaml(traversal),
            hcl::Expression::FuncCall(func_call) => self.func_call_to_yaml(func_call),
            hcl::Expression::TemplateExpr(template_expr) => {
                self.template_expr_to_yaml(template_expr)
            }
            hcl::Expression::Parenthesis(inner) => self.expr_to_yaml(inner),
            hcl::Expression::Conditional(cond) => {
                // Conditionals don't have a direct YAML equivalent
                self.diags.warning(
                    None,
                    "conditional expression cannot be directly represented in YAML".to_string(),
                    "manual conversion needed",
                );
                // Best effort: emit as the true branch
                self.expr_to_yaml(&cond.true_expr)
            }
            _ => {
                self.diags.warning(
                    None,
                    "unsupported PCL expression type".to_string(),
                    "will be emitted as null",
                );
                serde_yaml::Value::Null
            }
        }
    }

    fn traversal_to_yaml(&mut self, traversal: &hcl::expr::Traversal) -> serde_yaml::Value {
        let mut path = String::new();

        // Root expression
        match &traversal.expr {
            hcl::Expression::Variable(var) => {
                let name = var.as_str().to_string();
                // Check for pulumi built-in variables
                match name.as_str() {
                    "cwd" | "project" | "stack" | "organization" | "rootDirectory" => {
                        if traversal.operators.is_empty() {
                            // Bare function call → ${pulumi.<name>}
                            return serde_yaml::Value::String(format!("${{pulumi.{}}}", name));
                        }
                    }
                    _ => {}
                }
                path.push_str(&name);
            }
            _ => {
                // Complex root — fallback
                let root = self.expr_to_yaml(&traversal.expr);
                return root;
            }
        }

        // Operators
        for op in &traversal.operators {
            match op {
                hcl::expr::TraversalOperator::GetAttr(ident) => {
                    path.push('.');
                    path.push_str(ident.as_ref());
                }
                hcl::expr::TraversalOperator::Index(idx_expr) => match idx_expr {
                    hcl::Expression::Number(n) => {
                        path.push_str(&format!("[{}]", n));
                    }
                    hcl::Expression::String(s) => {
                        path.push_str(&format!("[\"{}\"]", s));
                    }
                    _ => {
                        let idx_yaml = self.expr_to_yaml(idx_expr);
                        path.push_str(&format!("[{}]", yaml_to_inline_string(&idx_yaml)));
                    }
                },
                hcl::expr::TraversalOperator::LegacyIndex(idx) => {
                    path.push_str(&format!("[{}]", idx));
                }
                _ => {} // Splats not supported in YAML
            }
        }

        serde_yaml::Value::String(format!("${{{}}}", path))
    }

    fn func_call_to_yaml(&mut self, func_call: &hcl::expr::FuncCall) -> serde_yaml::Value {
        let name = func_call.name.to_string();
        let args: Vec<serde_yaml::Value> = func_call
            .args
            .iter()
            .map(|a| self.expr_to_yaml(a))
            .collect();

        match name.as_str() {
            "invoke" => self.invoke_to_yaml(&args),
            "secret" => {
                if let Some(inner) = args.into_iter().next() {
                    let mut map = serde_yaml::Mapping::new();
                    map.insert(serde_yaml::Value::String("fn::secret".to_string()), inner);
                    serde_yaml::Value::Mapping(map)
                } else {
                    serde_yaml::Value::Null
                }
            }
            "join" => {
                if args.len() == 2 {
                    let mut map = serde_yaml::Mapping::new();
                    let join_args = serde_yaml::Value::Sequence(args);
                    map.insert(serde_yaml::Value::String("fn::join".to_string()), join_args);
                    serde_yaml::Value::Mapping(map)
                } else {
                    serde_yaml::Value::Null
                }
            }
            "split" => {
                if args.len() == 2 {
                    let mut map = serde_yaml::Mapping::new();
                    let split_args = serde_yaml::Value::Sequence(args);
                    map.insert(
                        serde_yaml::Value::String("fn::split".to_string()),
                        split_args,
                    );
                    serde_yaml::Value::Mapping(map)
                } else {
                    serde_yaml::Value::Null
                }
            }
            "toJSON" => {
                if let Some(inner) = args.into_iter().next() {
                    let mut map = serde_yaml::Mapping::new();
                    map.insert(serde_yaml::Value::String("fn::toJSON".to_string()), inner);
                    serde_yaml::Value::Mapping(map)
                } else {
                    serde_yaml::Value::Null
                }
            }
            "toBase64" => single_fn_mapping("fn::toBase64", args),
            "fromBase64" => single_fn_mapping("fn::fromBase64", args),
            "readFile" => single_fn_mapping("fn::readFile", args),
            "fileAsset" => single_fn_mapping("fn::fileAsset", args),
            "stringAsset" => single_fn_mapping("fn::stringAsset", args),
            "remoteAsset" => single_fn_mapping("fn::remoteAsset", args),
            "fileArchive" => single_fn_mapping("fn::fileArchive", args),
            "remoteArchive" => single_fn_mapping("fn::remoteArchive", args),
            "assetArchive" => single_fn_mapping("fn::assetArchive", args),
            "element" => {
                // element(list, idx) → fn::select: [idx, list]
                if args.len() == 2 {
                    let reordered = vec![args[1].clone(), args[0].clone()];
                    let mut map = serde_yaml::Mapping::new();
                    map.insert(
                        serde_yaml::Value::String("fn::select".to_string()),
                        serde_yaml::Value::Sequence(reordered),
                    );
                    serde_yaml::Value::Mapping(map)
                } else {
                    serde_yaml::Value::Null
                }
            }
            // Pulumi built-in functions
            "cwd" => serde_yaml::Value::String("${pulumi.cwd}".to_string()),
            "project" => serde_yaml::Value::String("${pulumi.project}".to_string()),
            "stack" => serde_yaml::Value::String("${pulumi.stack}".to_string()),
            "organization" => serde_yaml::Value::String("${pulumi.organization}".to_string()),
            "rootDirectory" => serde_yaml::Value::String("${pulumi.rootDirectory}".to_string()),
            _ => {
                self.diags.warning(
                    None,
                    format!("unsupported PCL function '{}' in YAML generation", name),
                    "will be emitted as null",
                );
                serde_yaml::Value::Null
            }
        }
    }

    fn invoke_to_yaml(&mut self, args: &[serde_yaml::Value]) -> serde_yaml::Value {
        self.invoke_to_yaml_with_return(args, None)
    }

    fn invoke_to_yaml_with_return(
        &mut self,
        args: &[serde_yaml::Value],
        return_field: Option<&str>,
    ) -> serde_yaml::Value {
        // invoke("token", {args}, {options}?)
        let token = match args.first() {
            Some(serde_yaml::Value::String(s)) => collapse_token(s),
            _ => return serde_yaml::Value::Null,
        };
        let arguments = args.get(1).cloned();

        let mut invoke_map = serde_yaml::Mapping::new();
        invoke_map.insert(
            serde_yaml::Value::String("function".to_string()),
            serde_yaml::Value::String(token),
        );

        if let Some(ref args_val) = arguments {
            if args_val != &serde_yaml::Value::Null
                && args_val != &serde_yaml::Value::Mapping(serde_yaml::Mapping::new())
            {
                invoke_map.insert(
                    serde_yaml::Value::String("arguments".to_string()),
                    args_val.clone(),
                );
            }
        }

        if let Some(ret) = return_field {
            invoke_map.insert(
                serde_yaml::Value::String("return".to_string()),
                serde_yaml::Value::String(ret.to_string()),
            );
        }

        let mut map = serde_yaml::Mapping::new();
        map.insert(
            serde_yaml::Value::String("fn::invoke".to_string()),
            serde_yaml::Value::Mapping(invoke_map),
        );
        serde_yaml::Value::Mapping(map)
    }

    fn template_expr_to_yaml(
        &mut self,
        template_expr: &hcl::expr::TemplateExpr,
    ) -> serde_yaml::Value {
        match template_expr {
            hcl::expr::TemplateExpr::QuotedString(s) => serde_yaml::Value::String(s.clone()),
            hcl::expr::TemplateExpr::Heredoc(heredoc) => {
                serde_yaml::Value::String(heredoc.template.to_string())
            }
        }
    }

    fn object_key_to_yaml(&mut self, key: &hcl::expr::ObjectKey) -> serde_yaml::Value {
        match key {
            hcl::expr::ObjectKey::Identifier(ident) => serde_yaml::Value::String(ident.to_string()),
            hcl::expr::ObjectKey::Expression(expr) => self.expr_to_yaml(expr),
            _ => serde_yaml::Value::String("unknown".to_string()),
        }
    }

    /// Convert an expression to a YAML reference (for dependsOn, etc.)
    fn expr_to_yaml_refs(&mut self, expr: &hcl::Expression) -> serde_yaml::Value {
        match expr {
            hcl::Expression::Array(items) => {
                let refs: Vec<serde_yaml::Value> =
                    items.iter().map(|e| self.expr_to_yaml_ref(e)).collect();
                serde_yaml::Value::Sequence(refs)
            }
            _ => self.expr_to_yaml(expr),
        }
    }

    /// Convert a single expression to a ${ref} style reference.
    fn expr_to_yaml_ref(&mut self, expr: &hcl::Expression) -> serde_yaml::Value {
        match expr {
            hcl::Expression::Variable(var) => {
                serde_yaml::Value::String(format!("${{{}}}", var.as_str()))
            }
            hcl::Expression::Traversal(traversal) => self.traversal_to_yaml(traversal),
            _ => self.expr_to_yaml(expr),
        }
    }

    fn finish(self) -> GenerateResult {
        let mut root = serde_yaml::Mapping::new();

        // Config section
        if !self.config.is_empty() {
            let mut config_map = serde_yaml::Mapping::new();
            for entry in &self.config {
                if let serde_yaml::Value::Mapping(m) = entry {
                    for (k, v) in m {
                        config_map.insert(k.clone(), v.clone());
                    }
                }
            }
            root.insert(
                serde_yaml::Value::String("configuration".to_string()),
                serde_yaml::Value::Mapping(config_map),
            );
        }

        // Variables section
        if !self.variables.is_empty() {
            let mut vars_map = serde_yaml::Mapping::new();
            for (key, value) in &self.variables {
                vars_map.insert(serde_yaml::Value::String(key.clone()), value.clone());
            }
            root.insert(
                serde_yaml::Value::String("variables".to_string()),
                serde_yaml::Value::Mapping(vars_map),
            );
        }

        // Resources section
        if !self.resources.is_empty() {
            let mut res_map = serde_yaml::Mapping::new();
            for (key, value) in &self.resources {
                res_map.insert(serde_yaml::Value::String(key.clone()), value.clone());
            }
            root.insert(
                serde_yaml::Value::String("resources".to_string()),
                serde_yaml::Value::Mapping(res_map),
            );
        }

        // Outputs section
        if !self.outputs.is_empty() {
            let mut out_map = serde_yaml::Mapping::new();
            for (key, value) in &self.outputs {
                out_map.insert(serde_yaml::Value::String(key.clone()), value.clone());
            }
            root.insert(
                serde_yaml::Value::String("outputs".to_string()),
                serde_yaml::Value::Mapping(out_map),
            );
        }

        let yaml_text = if root.is_empty() {
            String::new()
        } else {
            serde_yaml::to_string(&root).unwrap_or_default()
        };

        let mut files = HashMap::new();
        if !yaml_text.is_empty() {
            files.insert("Pulumi.yaml".to_string(), yaml_text.into_bytes());
        }

        GenerateResult {
            files,
            diagnostics: self.diags,
        }
    }
}

/// Creates a single-argument fn:: mapping.
fn single_fn_mapping(fn_name: &str, args: Vec<serde_yaml::Value>) -> serde_yaml::Value {
    if let Some(inner) = args.into_iter().next() {
        let mut map = serde_yaml::Mapping::new();
        map.insert(serde_yaml::Value::String(fn_name.to_string()), inner);
        serde_yaml::Value::Mapping(map)
    } else {
        serde_yaml::Value::Null
    }
}

/// Extracts the __logicalName from a block body.
fn find_logical_name(body: &hcl::Body) -> Option<String> {
    for structure in body.iter() {
        if let hcl::Structure::Attribute(attr) = structure {
            if attr.key.to_string() == "__logicalName" {
                if let hcl::Expression::String(s) = &attr.expr {
                    return Some(s.clone());
                }
            }
        }
    }
    None
}

/// Converts a PCL type string to a YAML config type.
fn pcl_type_to_yaml(pcl_type: &str) -> String {
    match pcl_type {
        "string" => "string".to_string(),
        "int" => "int".to_string(),
        "number" => "number".to_string(),
        "bool" => "bool".to_string(),
        "any" => "dynamic".to_string(),
        _ if pcl_type.starts_with("list(") => {
            let inner = &pcl_type[5..pcl_type.len() - 1];
            format!("List<{}>", pcl_type_to_yaml(inner))
        }
        _ if pcl_type.starts_with("map(") => "Object".to_string(),
        _ => "string".to_string(),
    }
}

/// Converts a block label to a string.
fn label_to_string(label: &hcl::BlockLabel) -> String {
    match label {
        hcl::BlockLabel::String(s) => s.clone(),
        hcl::BlockLabel::Identifier(id) => id.to_string(),
    }
}

/// Collapses a canonical type token into a shorter display form.
/// Partial inverse of `canonicalize_type_token`.
///
/// Examples:
///   aws:s3/bucket:Bucket   → aws:s3:Bucket
///   foo:index/bar:Bar      → foo:Bar       (index collapses)
///   foo:index:Bar          → foo:Bar       (index collapses)
///   foo::Bar               → foo:Bar       (empty middle collapses)
///   fizz:mod:buzz          → fizz:mod:buzz (no change)
fn collapse_token(token: &str) -> String {
    let parts: Vec<&str> = token.split(':').collect();
    if parts.len() == 3 {
        let pkg = parts[0];
        let module = parts[1];
        let type_name = parts[2];

        // Check for mod/lowerType pattern: aws:s3/bucket:Bucket
        if let Some(slash_idx) = module.find('/') {
            let mod_part = &module[..slash_idx];
            let lower_part = &module[slash_idx + 1..];
            // Collapse if the part after / title-cased == type_name
            let title_lower = {
                let mut chars = lower_part.chars();
                match chars.next() {
                    Some(c) => {
                        let upper: String = c.to_uppercase().collect();
                        format!("{}{}", upper, chars.as_str())
                    }
                    None => String::new(),
                }
            };
            if title_lower == type_name {
                // aws:s3/bucket:Bucket → aws:s3:Bucket
                // Then check if mod_part is "index"
                let collapsed_module = mod_part;
                if collapsed_module == "index" || collapsed_module.is_empty() {
                    return format!("{}:{}", pkg, type_name);
                }
                return format!("{}:{}:{}", pkg, collapsed_module, type_name);
            }
        }

        // Check for index or empty middle segment
        if module == "index" || module.is_empty() {
            return format!("{}:{}", pkg, type_name);
        }
    }
    token.to_string()
}

/// Checks if an expression is a secret() function call.
fn is_secret_call(expr: &hcl::Expression) -> bool {
    if let hcl::Expression::FuncCall(func_call) = expr {
        func_call.name.to_string() == "secret" && func_call.args.len() == 1
    } else {
        false
    }
}

/// Unwraps a secret() call, returning the inner expression.
fn unwrap_secret_call(expr: &hcl::Expression) -> &hcl::Expression {
    if let hcl::Expression::FuncCall(func_call) = expr {
        if func_call.name.to_string() == "secret" && !func_call.args.is_empty() {
            return &func_call.args[0];
        }
    }
    expr
}

/// Converts a YAML value to an inline string representation.
fn yaml_to_inline_string(value: &serde_yaml::Value) -> String {
    match value {
        serde_yaml::Value::String(s) => s.clone(),
        serde_yaml::Value::Number(n) => n.to_string(),
        serde_yaml::Value::Bool(b) => b.to_string(),
        serde_yaml::Value::Null => "null".to_string(),
        _ => serde_yaml::to_string(value).unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gen(pcl: &str) -> (String, Diagnostics) {
        let mut sources = HashMap::new();
        sources.insert("main.pp".to_string(), pcl.to_string());
        let result = generate_program(&sources);
        let text = result
            .files
            .get("Pulumi.yaml")
            .map(|b| String::from_utf8_lossy(b).to_string())
            .unwrap_or_default();
        (text, result.diagnostics)
    }

    #[test]
    fn test_basic_resource() {
        let (yaml, diags) = gen(r#"
resource myBucket "aws:s3:Bucket" {
    __logicalName = "my-bucket"
    bucketName = "test-bucket"
}
"#);
        assert!(!diags.has_errors(), "errors: {}", diags);
        // Key is pcl_name; name: field emitted for __logicalName
        assert!(yaml.contains("myBucket:"), "got:\n{}", yaml);
        assert!(yaml.contains("name: my-bucket"), "got:\n{}", yaml);
        assert!(yaml.contains("type: aws:s3:Bucket"), "got:\n{}", yaml);
        assert!(yaml.contains("bucketName: test-bucket"), "got:\n{}", yaml);
    }

    #[test]
    fn test_config() {
        let (yaml, diags) = gen(r#"
config name string {
    __logicalName = "appName"
    default = "myapp"
}
"#);
        assert!(!diags.has_errors(), "errors: {}", diags);
        // Key is pcl_name; name: field emitted for __logicalName
        assert!(yaml.contains("name:"), "got:\n{}", yaml);
        assert!(yaml.contains("name: appName"), "got:\n{}", yaml);
        assert!(yaml.contains("type: string"), "got:\n{}", yaml);
        assert!(yaml.contains("default: myapp"), "got:\n{}", yaml);
    }

    #[test]
    fn test_output() {
        let (yaml, diags) = gen(r#"
resource myBucket "aws:s3:Bucket" {
    bucketName = "test"
}
output bucketId {
    __logicalName = "bucket-id"
    value = myBucket.id
}
"#);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert!(yaml.contains("bucket-id:"), "got:\n{}", yaml);
        assert!(yaml.contains("${myBucket.id}"), "got:\n{}", yaml);
    }

    #[test]
    fn test_variable() {
        let (yaml, diags) = gen(r#"
encoded = toBase64("hello")
"#);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert!(yaml.contains("variables:"), "got:\n{}", yaml);
        assert!(yaml.contains("encoded:"), "got:\n{}", yaml);
        assert!(yaml.contains("fn::toBase64"), "got:\n{}", yaml);
    }

    #[test]
    fn test_invoke() {
        let (yaml, diags) = gen(r#"
ami = invoke("aws:ec2/getAmi:getAmi", {
    owners = ["self"]
    mostRecent = true
})
"#);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert!(yaml.contains("fn::invoke"), "got:\n{}", yaml);
        // Token is not collapsed since title("getAmi") = "GetAmi" != "getAmi"
        assert!(
            yaml.contains("function: aws:ec2/getAmi:getAmi"),
            "got:\n{}",
            yaml
        );
    }

    #[test]
    fn test_secret() {
        let (yaml, diags) = gen(r#"
secretVal = secret("my-password")
"#);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert!(yaml.contains("fn::secret"), "got:\n{}", yaml);
    }

    #[test]
    fn test_join() {
        let (yaml, diags) = gen(r#"
joined = join(",", ["a", "b", "c"])
"#);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert!(yaml.contains("fn::join"), "got:\n{}", yaml);
    }

    #[test]
    fn test_split() {
        let (yaml, diags) = gen(r#"
parts = split(",", "a,b,c")
"#);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert!(yaml.contains("fn::split"), "got:\n{}", yaml);
    }

    #[test]
    fn test_traversal_reference() {
        let (yaml, diags) = gen(r#"
resource myBucket "aws:s3:Bucket" {
    bucketName = "test"
}
resource myObject "aws:s3:BucketObject" {
    bucket = myBucket.bucketName
    content = "hello"
}
"#);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert!(yaml.contains("${myBucket.bucketName}"), "got:\n{}", yaml);
    }

    #[test]
    fn test_resource_options() {
        let (yaml, diags) = gen(r#"
resource prov "test:mod:Prov" {}
resource myRes "test:mod:Res" {
    name = "test"

    options {
        protect = true
        provider = prov
        dependsOn = [prov]
        ignoreChanges = ["name"]
    }
}
"#);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert!(yaml.contains("protect: true"), "got:\n{}", yaml);
        assert!(yaml.contains("provider:"), "got:\n{}", yaml);
        assert!(yaml.contains("dependsOn:"), "got:\n{}", yaml);
        assert!(yaml.contains("ignoreChanges:"), "got:\n{}", yaml);
    }

    #[test]
    fn test_pulumi_builtins() {
        let (yaml, diags) = gen(r#"
resource myRes "test:mod:Res" {
    dir = cwd()
    proj = project()
    stk = stack()
}
"#);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert!(yaml.contains("${pulumi.cwd}"), "got:\n{}", yaml);
        assert!(yaml.contains("${pulumi.project}"), "got:\n{}", yaml);
        assert!(yaml.contains("${pulumi.stack}"), "got:\n{}", yaml);
    }

    #[test]
    fn test_empty_program() {
        let (yaml, diags) = gen("");
        assert!(!diags.has_errors());
        assert_eq!(yaml, "");
    }

    #[test]
    fn test_config_secret() {
        let (yaml, diags) = gen(r#"
config password string {
    secret = true
}
"#);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert!(yaml.contains("secret: true"), "got:\n{}", yaml);
    }

    #[test]
    fn test_file_asset() {
        let (yaml, diags) = gen(r#"
resource myRes "test:mod:Res" {
    code = fileArchive("./code")
}
"#);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert!(yaml.contains("fn::fileArchive"), "got:\n{}", yaml);
    }

    #[test]
    fn test_select_element() {
        let (yaml, diags) = gen(r#"
picked = element(["a", "b", "c"], 1)
"#);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert!(yaml.contains("fn::select"), "got:\n{}", yaml);
    }

    #[test]
    fn test_read_file() {
        let (yaml, diags) = gen(r#"
content = readFile("./data.txt")
"#);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert!(yaml.contains("fn::readFile"), "got:\n{}", yaml);
    }

    #[test]
    fn test_pcl_type_to_yaml() {
        assert_eq!(pcl_type_to_yaml("string"), "string");
        assert_eq!(pcl_type_to_yaml("int"), "int");
        assert_eq!(pcl_type_to_yaml("number"), "number");
        assert_eq!(pcl_type_to_yaml("bool"), "bool");
        assert_eq!(pcl_type_to_yaml("any"), "dynamic");
        assert_eq!(pcl_type_to_yaml("list(string)"), "List<string>");
        assert_eq!(pcl_type_to_yaml("map(any)"), "Object");
    }

    #[test]
    fn test_config_int_type() {
        let (yaml, diags) = gen(r#"
config count int {
    default = 3
}
"#);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert!(yaml.contains("type: int"), "got:\n{}", yaml);
        assert!(yaml.contains("default: 3"), "got:\n{}", yaml);
    }

    #[test]
    fn test_index_expression() {
        let (yaml, diags) = gen(r#"
resource myRes "test:mod:Res" {
    item = myList[0]
}
"#);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert!(yaml.contains("${myList[0]}"), "got:\n{}", yaml);
    }

    #[test]
    fn test_null_value() {
        let (yaml, diags) = gen(r#"
resource myRes "test:mod:Res" {
    value = null
}
"#);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert!(yaml.contains("value: null"), "got:\n{}", yaml);
    }

    #[test]
    fn test_bool_value() {
        let (yaml, diags) = gen(r#"
resource myRes "test:mod:Res" {
    enabled = true
    disabled = false
}
"#);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert!(yaml.contains("enabled: true"), "got:\n{}", yaml);
        assert!(yaml.contains("disabled: false"), "got:\n{}", yaml);
    }

    #[test]
    fn test_array_value() {
        let (yaml, diags) = gen(r#"
resource myRes "test:mod:Res" {
    tags = ["a", "b", "c"]
}
"#);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert!(yaml.contains("tags:"), "got:\n{}", yaml);
        assert!(yaml.contains("- a"), "got:\n{}", yaml);
        assert!(yaml.contains("- b"), "got:\n{}", yaml);
    }

    #[test]
    fn test_to_json() {
        let (yaml, diags) = gen(r#"
jsonified = toJSON({
    key = "value"
})
"#);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert!(yaml.contains("fn::toJSON"), "got:\n{}", yaml);
    }

    #[test]
    fn test_from_base64() {
        let (yaml, diags) = gen(r#"
decoded = fromBase64("aGVsbG8=")
"#);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert!(yaml.contains("fn::fromBase64"), "got:\n{}", yaml);
    }

    #[test]
    fn test_multiple_resources() {
        let (yaml, diags) = gen(r#"
resource bucket "aws:s3:Bucket" {
    bucketName = "test"
}
resource object "aws:s3:BucketObject" {
    bucket = bucket.id
    content = "hello"
}
"#);
        assert!(!diags.has_errors(), "errors: {}", diags);
        // Both resources should be present
        assert!(yaml.contains("type: aws:s3:Bucket"), "got:\n{}", yaml);
        assert!(yaml.contains("type: aws:s3:BucketObject"), "got:\n{}", yaml);
        assert!(yaml.contains("${bucket.id}"), "got:\n{}", yaml);
    }

    // ─── Token collapse tests (Go parity: collapseToken) ────

    #[test]
    fn test_collapse_token_module_path() {
        assert_eq!(collapse_token("aws:s3/bucket:Bucket"), "aws:s3:Bucket");
    }

    #[test]
    fn test_collapse_token_index_module() {
        assert_eq!(collapse_token("foo:index/bar:Bar"), "foo:Bar");
    }

    #[test]
    fn test_collapse_token_index_plain() {
        assert_eq!(collapse_token("foo:index:Bar"), "foo:Bar");
    }

    #[test]
    fn test_collapse_token_empty_middle() {
        assert_eq!(collapse_token("foo::Bar"), "foo:Bar");
    }

    #[test]
    fn test_collapse_token_no_change() {
        assert_eq!(collapse_token("fizz:mod:buzz"), "fizz:mod:buzz");
    }

    #[test]
    fn test_collapse_token_random_pet() {
        assert_eq!(
            collapse_token("random:index/randomPet:RandomPet"),
            "random:RandomPet"
        );
    }

    #[test]
    fn test_collapse_token_function_no_match() {
        // title("getAmi") = "GetAmi" != "getAmi" → no collapse
        assert_eq!(
            collapse_token("aws:ec2/getAmi:getAmi"),
            "aws:ec2/getAmi:getAmi"
        );
    }

    #[test]
    fn test_collapse_token_two_part() {
        assert_eq!(collapse_token("aws:Bucket"), "aws:Bucket");
    }

    #[test]
    fn test_collapse_token_dashes() {
        assert_eq!(
            collapse_token("using-dashes:index/dash:Dash"),
            "using-dashes:Dash"
        );
    }

    // ─── Go parity: config-variables fixture ────────────────

    #[test]
    fn test_config_all_types() {
        let (yaml, diags) = gen(r#"
config requiredString string {}
config requiredInt int {}
config requiredFloat number {}
config requiredBool bool {}
config requiredAny any {}
config optionalString string { default = "defaultStringValue" }
config optionalInt int { default = 42 }
config optionalFloat number { default = 3.14 }
config optionalBool bool { default = true }
config optionalAny any { default = {"key" = "value"} }
"#);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert!(yaml.contains("type: string"), "got:\n{}", yaml);
        assert!(yaml.contains("type: int"), "got:\n{}", yaml);
        assert!(yaml.contains("type: number"), "got:\n{}", yaml);
        assert!(yaml.contains("type: bool"), "got:\n{}", yaml);
        assert!(yaml.contains("type: dynamic"), "got:\n{}", yaml);
        assert!(
            yaml.contains("default: defaultStringValue"),
            "got:\n{}",
            yaml
        );
        assert!(yaml.contains("default: 42"), "got:\n{}", yaml);
        assert!(yaml.contains("default: 3.14"), "got:\n{}", yaml);
        assert!(yaml.contains("default: true"), "got:\n{}", yaml);
    }

    // ─── Go parity: output-literals fixture ─────────────────

    #[test]
    fn test_output_literals() {
        let (yaml, diags) = gen(r#"
output output_true { value = true }
output output_false { value = false }
output output_number { value = 4 }
output output_string { value = "hello" }
"#);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert!(yaml.contains("output_true: true"), "got:\n{}", yaml);
        assert!(yaml.contains("output_false: false"), "got:\n{}", yaml);
        assert!(yaml.contains("output_number: 4"), "got:\n{}", yaml);
        assert!(yaml.contains("output_string: hello"), "got:\n{}", yaml);
    }

    // ─── Go parity: assets-archives fixture ─────────────────

    #[test]
    fn test_assets_archives_all() {
        let (yaml, diags) = gen(r#"
resource siteBucket "aws:s3:Bucket" {}
resource testFileAsset "aws:s3:BucketObject" {
    bucket = siteBucket.id
    source = fileAsset("file.txt")
}
resource testStringAsset "aws:s3:BucketObject" {
    bucket = siteBucket.id
    source = stringAsset("<h1>File contents</h1>")
}
resource testRemoteAsset "aws:s3:BucketObject" {
    bucket = siteBucket.id
    source = remoteAsset("https://pulumi.test")
}
resource testFileArchive "aws:lambda:Function" {
    role = siteBucket.arn
    code = fileArchive("file.tar.gz")
}
resource testRemoteArchive "aws:lambda:Function" {
    role = siteBucket.arn
    code = remoteArchive("https://pulumi.test/foo.tar.gz")
}
resource testAssetArchive "aws:lambda:Function" {
    role = siteBucket.arn
    code = assetArchive({
        "file.txt" = fileAsset("file.txt")
        "string.txt" = stringAsset("<h1>File contents</h1>")
    })
}
"#);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert!(yaml.contains("fn::fileAsset: file.txt"), "got:\n{}", yaml);
        assert!(yaml.contains("fn::stringAsset:"), "got:\n{}", yaml);
        assert!(
            yaml.contains("fn::remoteAsset: https://pulumi.test"),
            "got:\n{}",
            yaml
        );
        assert!(
            yaml.contains("fn::fileArchive: file.tar.gz"),
            "got:\n{}",
            yaml
        );
        assert!(
            yaml.contains("fn::remoteArchive: https://pulumi.test/foo.tar.gz"),
            "got:\n{}",
            yaml
        );
        assert!(yaml.contains("fn::assetArchive:"), "got:\n{}", yaml);
    }

    // ─── Go parity: aws-resource-options fixture ────────────

    #[test]
    fn test_resource_options_full() {
        let (yaml, diags) = gen(r#"
resource provider "pulumi:providers:aws" {
    region = "us-west-2"
}
resource bucket1 "aws:s3:Bucket" {
    options {
        provider = provider
        dependsOn = [provider]
        protect = true
        ignoreChanges = ["bucket"]
    }
}
"#);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert!(yaml.contains("provider: ${provider}"), "got:\n{}", yaml);
        assert!(yaml.contains("- ${provider}"), "got:\n{}", yaml);
        assert!(yaml.contains("protect: true"), "got:\n{}", yaml);
        assert!(yaml.contains("- bucket"), "got:\n{}", yaml);
    }

    // ─── Go parity: depends-on-array (logical names) ────────

    #[test]
    fn test_logical_name_resource() {
        let (yaml, diags) = gen(r#"
resource indexHtml "aws:s3/bucketObject:BucketObject" {
    __logicalName = "index.html"
    bucket = myBucket.id
    source = fileAsset("./index.html")
    contentType = "text/html"
}
"#);
        assert!(!diags.has_errors(), "errors: {}", diags);
        // Key is pcl_name
        assert!(yaml.contains("indexHtml:"), "got:\n{}", yaml);
        // name: field has logical name
        assert!(yaml.contains("name: index.html"), "got:\n{}", yaml);
        // Type collapsed: aws:s3/bucketObject:BucketObject → aws:s3:BucketObject
        assert!(yaml.contains("type: aws:s3:BucketObject"), "got:\n{}", yaml);
    }

    // ─── Go parity: aws-secret (secret() stripped) ──────────

    #[test]
    fn test_secret_stripped_from_resource() {
        let (yaml, diags) = gen(r#"
resource dbCluster "aws:rds:Cluster" {
    masterPassword = secret("foobar")
}
"#);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert!(yaml.contains("masterPassword: foobar"), "got:\n{}", yaml);
        assert!(
            !yaml.contains("fn::secret"),
            "secret() should be stripped, got:\n{}",
            yaml
        );
    }

    // ─── Go parity: negative-literals fixture ───────────────

    #[test]
    fn test_negative_literals() {
        let (yaml, diags) = gen(r#"
resource ecsPolicy "aws:appautoscaling:Policy" {
    name = "scale-down"
    scalingAdjustment = -1
}
"#);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert!(yaml.contains("scalingAdjustment: -1"), "got:\n{}", yaml);
    }

    // ─── Go parity: using-dashes fixture ────────────────────

    #[test]
    fn test_dashes_in_token() {
        let (yaml, diags) = gen(r#"
resource main "using-dashes:index/dash:Dash" {
    stack = "dev"
}
"#);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert!(yaml.contains("type: using-dashes:Dash"), "got:\n{}", yaml);
        assert!(yaml.contains("stack: dev"), "got:\n{}", yaml);
    }

    // ─── Go parity: simplified-invokes fixture ──────────────

    #[test]
    fn test_simplified_invokes() {
        let (yaml, diags) = gen(r#"
everyArg = invoke("std:index:AbsMultiArgs", {
    a = 10
    b = 20
    c = 30
})
onlyRequired = invoke("std:index:AbsMultiArgs", { a = 10 })
output result { value = everyArg }
"#);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert!(
            yaml.contains("function: std:AbsMultiArgs"),
            "got:\n{}",
            yaml
        );
        assert!(yaml.contains("a: 10"), "got:\n{}", yaml);
        assert!(yaml.contains("${everyArg}"), "got:\n{}", yaml);
    }

    // ─── Go parity: aws-webserver fixture ───────────────────

    #[test]
    fn test_webserver_complex() {
        let (yaml, diags) = gen(r#"
resource securityGroup "aws:ec2:SecurityGroup" {
    ingress = [{
        protocol = "tcp"
        fromPort = 0
        toPort = 0
        cidrBlocks = ["0.0.0.0/0"]
    }]
}
ami = invoke("aws:index:getAmi", {
    filters = [{
        name = "name"
        values = ["amzn-ami-hvm-*-x86_64-ebs"]
    }]
    owners = ["137112412989"]
    mostRecent = true
})
resource server "aws:ec2:Instance" {
    tags = { Name = "web-server-www" }
    instanceType = "t2.micro"
    securityGroups = [securityGroup.name]
    ami = ami.id
}
output publicIp { value = server.publicIp }
"#);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert!(
            yaml.contains("type: aws:ec2:SecurityGroup"),
            "got:\n{}",
            yaml
        );
        assert!(yaml.contains("type: aws:ec2:Instance"), "got:\n{}", yaml);
        assert!(yaml.contains("function: aws:getAmi"), "got:\n{}", yaml);
        assert!(yaml.contains("${securityGroup.name}"), "got:\n{}", yaml);
        assert!(yaml.contains("${ami.id}"), "got:\n{}", yaml);
        assert!(yaml.contains("${server.publicIp}"), "got:\n{}", yaml);
        assert!(yaml.contains("protocol: tcp"), "got:\n{}", yaml);
        assert!(yaml.contains("- 0.0.0.0/0"), "got:\n{}", yaml);
    }

    // ─── Template expression with interpolation ─────────────

    #[test]
    fn test_template_expression_simple() {
        let (yaml, diags) = gen(r#"
resource myBucket "aws:s3:Bucket" {}
output endpoint { value = "http://${myBucket.websiteEndpoint}" }
"#);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert!(
            yaml.contains("http://${myBucket.websiteEndpoint}"),
            "got:\n{}",
            yaml
        );
    }

    // ─── Resource options: parent, deletedWith, retainOnDelete

    #[test]
    fn test_resource_options_parent_deleted_with() {
        let (yaml, diags) = gen(r#"
resource parentRes "test:mod:Parent" {}
resource childRes "test:mod:Child" {
    name = "child"
    options {
        parent = parentRes
        deletedWith = parentRes
        retainOnDelete = true
    }
}
"#);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert!(yaml.contains("parent: ${parentRes}"), "got:\n{}", yaml);
        assert!(yaml.contains("deletedWith: ${parentRes}"), "got:\n{}", yaml);
        assert!(yaml.contains("retainOnDelete: true"), "got:\n{}", yaml);
    }

    // ─── Resource options: import ───────────────────────────

    #[test]
    fn test_resource_options_import() {
        let (yaml, diags) = gen(r#"
resource imported "aws:s3:Bucket" {
    options { import = "my-existing-bucket-id" }
}
"#);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert!(
            yaml.contains("import: my-existing-bucket-id"),
            "got:\n{}",
            yaml
        );
    }

    // ─── Section ordering ───────────────────────────────────

    #[test]
    fn test_section_ordering() {
        let (yaml, diags) = gen(r#"
config name string {}
myVar = "hello"
resource myRes "test:mod:Res" { v = myVar }
output result { value = myRes.id }
"#);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert!(yaml.contains("configuration:"), "got:\n{}", yaml);
        assert!(yaml.contains("variables:"), "got:\n{}", yaml);
        assert!(yaml.contains("resources:"), "got:\n{}", yaml);
        assert!(yaml.contains("outputs:"), "got:\n{}", yaml);
    }

    // ─── Nested invokes ─────────────────────────────────────

    #[test]
    fn test_nested_invokes() {
        let (yaml, diags) = gen(r#"
nested = invoke("std:index:Abs", {
    a = invoke("std:index:Abs", { a = 42 })
})
"#);
        assert!(!diags.has_errors(), "errors: {}", diags);
        let count = yaml.matches("fn::invoke").count();
        assert_eq!(count, 2, "expected 2 invokes, got:\n{}", yaml);
    }

    // ─── Resource with no properties ────────────────────────

    #[test]
    fn test_resource_no_properties() {
        let (yaml, diags) = gen(r#"resource bucket "aws:s3:Bucket" {}"#);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert!(yaml.contains("type: aws:s3:Bucket"), "got:\n{}", yaml);
        assert!(!yaml.contains("properties:"), "got:\n{}", yaml);
    }

    // ─── Config without default ─────────────────────────────

    #[test]
    fn test_config_without_default() {
        let (yaml, diags) = gen(r#"config requiredString string {}"#);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert!(yaml.contains("type: string"), "got:\n{}", yaml);
        assert!(!yaml.contains("default:"), "got:\n{}", yaml);
    }

    // ─── Secret in variable NOT stripped ─────────────────────

    #[test]
    fn test_secret_in_variable_preserved() {
        let (yaml, diags) = gen(r#"secretVal = secret("my-password")"#);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert!(yaml.contains("fn::secret"), "got:\n{}", yaml);
    }

    // ─── invoke_to_yaml_with_return ─────────────────────────

    #[test]
    fn test_invoke_with_return_field() {
        let mut gen = PclToYamlGenerator::new();
        let args = vec![serde_yaml::Value::String("aws:ec2:getVpc".to_string()), {
            let mut m = serde_yaml::Mapping::new();
            m.insert(
                serde_yaml::Value::String("default".to_string()),
                serde_yaml::Value::Bool(true),
            );
            serde_yaml::Value::Mapping(m)
        }];
        let result = gen.invoke_to_yaml_with_return(&args, Some("id"));
        let yaml_text = serde_yaml::to_string(&result).unwrap();
        assert!(
            yaml_text.contains("function: aws:ec2:getVpc"),
            "got:\n{}",
            yaml_text
        );
        assert!(yaml_text.contains("return: id"), "got:\n{}", yaml_text);
    }

    // ─── Multiple outputs with refs ─────────────────────────

    #[test]
    fn test_multiple_outputs_with_refs() {
        let (yaml, diags) = gen(r#"
resource myBucket "aws:s3:Bucket" { bucketName = "test" }
output bucketId { value = myBucket.id }
output bucketArn { value = myBucket.arn }
"#);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert!(yaml.contains("bucketId: ${myBucket.id}"), "got:\n{}", yaml);
        assert!(
            yaml.contains("bucketArn: ${myBucket.arn}"),
            "got:\n{}",
            yaml
        );
    }

    // ─── Array index in traversal ───────────────────────────

    #[test]
    fn test_traversal_array_index() {
        let (yaml, diags) = gen(r#"
resource myRes "test:mod:Res" {
    cidrBlock = prefixList.cidrBlocks[0]
}
"#);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert!(
            yaml.contains("${prefixList.cidrBlocks[0]}"),
            "got:\n{}",
            yaml
        );
    }

    // ─── Invoke arguments with refs ─────────────────────────

    #[test]
    fn test_invoke_arguments_with_refs() {
        let (yaml, diags) = gen(r#"
resource vpc "aws:ec2:VpcEndpoint" { vpcId = v.id }
prefixList = invoke("aws:ec2:getPrefixList", {
    prefixListId = vpc.prefixListId
})
"#);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert!(
            yaml.contains("function: aws:ec2:getPrefixList"),
            "got:\n{}",
            yaml
        );
        assert!(yaml.contains("${vpc.prefixListId}"), "got:\n{}", yaml);
    }

    // ─── Component warning ──────────────────────────────────

    #[test]
    fn test_component_emits_warning() {
        let (_, diags) = gen(r#"component myComp "./path" { input1 = "value" }"#);
        let has = diags.iter().any(|d| d.summary.contains("component"));
        assert!(has, "expected component warning, diags: {}", diags);
        assert!(!diags.has_errors());
    }

    // ─── Unknown block type warning ─────────────────────────

    #[test]
    fn test_unknown_block_type_warning() {
        let (yaml, diags) = gen(r#"
unknown_block foo {}
resource myRes "test:mod:Res" { name = "test" }
"#);
        let has = diags
            .iter()
            .any(|d| d.summary.contains("unknown block type"));
        assert!(has, "expected unknown block warning");
        assert!(yaml.contains("type: test:mod:Res"), "got:\n{}", yaml);
    }

    // ─── Malformed PCL ──────────────────────────────────────

    #[test]
    fn test_malformed_pcl_produces_error() {
        let (yaml, diags) = gen("this is not valid HCL {{{{");
        assert!(diags.has_errors(), "expected parse error");
        assert_eq!(yaml, "");
    }

    // ─── Conditional expression warning ─────────────────────

    #[test]
    fn test_conditional_expression_warning() {
        let (yaml, diags) = gen(r#"
resource myRes "test:mod:Res" {
    value = true ? "yes" : "no"
}
"#);
        let has = diags.iter().any(|d| d.summary.contains("conditional"));
        assert!(has, "expected conditional warning");
        assert!(yaml.contains("value: yes"), "got:\n{}", yaml);
    }

    // ─── Parenthesized expression ───────────────────────────

    #[test]
    fn test_parenthesized_expression() {
        let (yaml, diags) = gen(r#"
resource myRes "test:mod:Res" { name = ("hello") }
"#);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert!(yaml.contains("name: hello"), "got:\n{}", yaml);
    }

    // ─── Object with string keys ────────────────────────────

    #[test]
    fn test_string_key_in_object() {
        let (yaml, diags) = gen(r#"
resource myRes "test:mod:Res" {
    tags = { "Name" = "my-resource", "env" = "prod" }
}
"#);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert!(yaml.contains("Name: my-resource"), "got:\n{}", yaml);
        assert!(yaml.contains("env: prod"), "got:\n{}", yaml);
    }

    // ─── Invoke with empty arguments ────────────────────────

    #[test]
    fn test_invoke_empty_arguments_omitted() {
        let (yaml, diags) = gen(r#"
result = invoke("test:index:getData", {})
"#);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert!(yaml.contains("function: test:getData"), "got:\n{}", yaml);
        // Empty arguments should be omitted
        assert!(!yaml.contains("arguments:"), "got:\n{}", yaml);
    }

    // ─── Resource with all asset/archive types ──────────────

    #[test]
    fn test_string_asset() {
        let (yaml, diags) = gen(r#"
resource obj "aws:s3:BucketObject" {
    source = stringAsset("<h1>Hello</h1>")
}
"#);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert!(yaml.contains("fn::stringAsset:"), "got:\n{}", yaml);
    }

    #[test]
    fn test_remote_asset() {
        let (yaml, diags) = gen(r#"
resource obj "aws:s3:BucketObject" {
    source = remoteAsset("https://example.com/file.txt")
}
"#);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert!(
            yaml.contains("fn::remoteAsset: https://example.com/file.txt"),
            "got:\n{}",
            yaml
        );
    }

    #[test]
    fn test_file_asset_in_resource() {
        let (yaml, diags) = gen(r#"
resource obj "aws:s3:BucketObject" {
    source = fileAsset("./index.html")
}
"#);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert!(
            yaml.contains("fn::fileAsset: ./index.html"),
            "got:\n{}",
            yaml
        );
    }

    // ─── is_secret_call / unwrap_secret_call helpers ────────

    #[test]
    fn test_is_secret_call_true() {
        let body = hcl::parse(r#"x = secret("val")"#).unwrap();
        let structures: Vec<_> = body.into_iter().collect();
        if let hcl::Structure::Attribute(attr) = &structures[0] {
            assert!(is_secret_call(&attr.expr));
        } else {
            panic!("expected attribute");
        }
    }

    #[test]
    fn test_is_secret_call_false() {
        let body = hcl::parse(r#"x = "val""#).unwrap();
        let structures: Vec<_> = body.into_iter().collect();
        if let hcl::Structure::Attribute(attr) = &structures[0] {
            assert!(!is_secret_call(&attr.expr));
        } else {
            panic!("expected attribute");
        }
    }
}

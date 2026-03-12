use crate::ast::expr::{Expr, InvokeExpr, InvokeOptions, ObjectProperty};
use crate::ast::interpolation::{has_interpolations, parse_interpolation};
use crate::ast::template::*;
use crate::diag::{unexpected_casing, Diagnostics};
use crate::syntax::{ExprMeta, Span};
use std::borrow::Cow;

/// Parses a YAML/JSON source string into a `TemplateDecl`.
///
/// Since `serde_yaml` doesn't support zero-copy deserialization, all strings
/// produced by parsing are `Cow::Owned`. The `'static` lifetime reflects this.
/// When the source text is available (e.g., for interpolation parsing), we use
/// owned copies of the relevant substrings.
pub fn parse_template(source: &str, span: Option<Span>) -> (TemplateDecl<'static>, Diagnostics) {
    let mut diags = Diagnostics::new();

    let yaml: serde_yaml::Value = match serde_yaml::from_str(source) {
        Ok(v) => v,
        Err(e) => {
            diags.error(span, format!("failed to parse YAML: {}", e), "");
            return (TemplateDecl::new(), diags);
        }
    };

    let mapping = match yaml.as_mapping() {
        Some(m) => m,
        None => {
            diags.error(span, "expected a YAML mapping at the top level", "");
            return (TemplateDecl::new(), diags);
        }
    };

    let meta = ExprMeta { span };
    let mut template = TemplateDecl::new();
    template.meta = meta;

    for (key, value) in mapping {
        let key_str = match key.as_str() {
            Some(s) => s,
            None => continue,
        };

        match key_str.to_lowercase().as_str() {
            "name" => {
                if let Some(s) = value.as_str() {
                    template.name = Some(Cow::Owned(s.to_string()));
                }
            }
            "namespace" => {
                if let Some(s) = value.as_str() {
                    template.namespace = Some(Cow::Owned(s.to_string()));
                }
            }
            "description" => {
                if let Some(s) = value.as_str() {
                    template.description = Some(Cow::Owned(s.to_string()));
                }
            }
            "runtime" => {
                // Runtime is metadata for the engine, not parsed into AST
            }
            "pulumi" => {
                template.pulumi = parse_pulumi_decl(value, &mut diags);
            }
            "config" | "configuration" => {
                template.config = parse_config_map(value, &mut diags);
            }
            "variables" => {
                template.variables = parse_variables_map(value, &mut diags);
            }
            "resources" => {
                template.resources = parse_resources_map(value, &mut diags);
            }
            "outputs" => {
                template.outputs = parse_outputs_map(value, &mut diags);
            }
            "components" => {
                template.components = parse_components(value, &mut diags);
            }
            _ => {
                // Unknown top-level keys are ignored
            }
        }
    }

    (template, diags)
}

/// Parses a `serde_yaml::Value` into an `Expr<'static>`.
pub fn parse_expr(value: &serde_yaml::Value, diags: &mut Diagnostics) -> Expr<'static> {
    let meta = ExprMeta::no_span();
    match value {
        serde_yaml::Value::Null => Expr::Null(meta),
        serde_yaml::Value::Bool(b) => Expr::Bool(meta, *b),
        serde_yaml::Value::Number(n) => Expr::Number(meta, n.as_f64().unwrap_or(0.0)),
        serde_yaml::Value::String(s) => parse_string_expr_owned(s, meta, diags),
        serde_yaml::Value::Sequence(seq) => {
            let elements: Vec<Expr<'static>> = seq.iter().map(|v| parse_expr(v, diags)).collect();
            Expr::List(meta, elements)
        }
        serde_yaml::Value::Mapping(map) => parse_object_or_builtin(map, meta, diags),
        serde_yaml::Value::Tagged(tagged) => parse_expr(&tagged.value, diags),
    }
}

/// Parses an owned string that may contain interpolations.
fn parse_string_expr_owned(s: &str, meta: ExprMeta, diags: &mut Diagnostics) -> Expr<'static> {
    if !has_interpolations(s) {
        return Expr::String(meta, Cow::Owned(s.to_string()));
    }

    let parts = parse_interpolation(s, meta.span, diags);

    if parts.is_empty() {
        return Expr::String(meta, Cow::Owned(s.to_string()));
    }

    // Convert all parts to 'static by ensuring owned data
    let owned_parts: Vec<_> = parts
        .into_iter()
        .map(|p| crate::ast::interpolation::InterpolationPart {
            text: Cow::Owned(p.text.into_owned()),
            value: p.value.map(|a| crate::ast::property::PropertyAccess {
                accessors: a
                    .accessors
                    .into_iter()
                    .map(|acc| match acc {
                        crate::ast::property::PropertyAccessor::Name(n) => {
                            crate::ast::property::PropertyAccessor::Name(Cow::Owned(n.into_owned()))
                        }
                        crate::ast::property::PropertyAccessor::StringSubscript(s) => {
                            crate::ast::property::PropertyAccessor::StringSubscript(Cow::Owned(
                                s.into_owned(),
                            ))
                        }
                        crate::ast::property::PropertyAccessor::IntSubscript(i) => {
                            crate::ast::property::PropertyAccessor::IntSubscript(i)
                        }
                    })
                    .collect(),
            }),
        })
        .collect();

    // Single part with no text prefix -> symbol reference
    if owned_parts.len() == 1 {
        if owned_parts[0].value.is_none() {
            // Pure text (all interpolations were escaped)
            let text = owned_parts.into_iter().next().unwrap().text;
            return Expr::String(meta, text);
        }
        if owned_parts[0].text.is_empty() {
            // Pure symbol: ${resource.prop}
            let part = owned_parts.into_iter().next().unwrap();
            return Expr::Symbol(meta, part.value.unwrap());
        }
    }

    Expr::Interpolate(meta, owned_parts)
}

/// Parses a YAML mapping as either a builtin function call or a plain object.
fn parse_object_or_builtin(
    map: &serde_yaml::Mapping,
    meta: ExprMeta,
    diags: &mut Diagnostics,
) -> Expr<'static> {
    // Try to parse as a builtin function (single-key objects starting with "fn::")
    if map.len() == 1 {
        let (key, value) = map.iter().next().unwrap();
        if let Some(key_str) = key.as_str() {
            if let Some(expr) = try_parse_builtin(key_str, value, meta, diags) {
                return expr;
            }
        }
    }

    // Parse as a plain object
    let entries: Vec<ObjectProperty<'static>> = map
        .iter()
        .map(|(k, v)| {
            let key_expr = parse_expr(k, diags);
            let value_expr = parse_expr(v, diags);
            ObjectProperty {
                key: Box::new(key_expr),
                value: Box::new(value_expr),
            }
        })
        .collect();

    Expr::Object(meta, entries)
}

/// Tries to parse a single-key object as a builtin function call.
fn try_parse_builtin(
    key: &str,
    value: &serde_yaml::Value,
    meta: ExprMeta,
    diags: &mut Diagnostics,
) -> Option<Expr<'static>> {
    let lower = key.to_lowercase();

    // Check asset/archive types first
    match lower.as_str() {
        "fn::stringasset" => {
            check_casing(key, "fn::stringAsset", diags);
            let source = parse_expr(value, diags);
            return Some(Expr::StringAsset(meta, Box::new(source)));
        }
        "fn::fileasset" => {
            check_casing(key, "fn::fileAsset", diags);
            let source = parse_expr(value, diags);
            return Some(Expr::FileAsset(meta, Box::new(source)));
        }
        "fn::remoteasset" => {
            check_casing(key, "fn::remoteAsset", diags);
            let source = parse_expr(value, diags);
            return Some(Expr::RemoteAsset(meta, Box::new(source)));
        }
        "fn::filearchive" => {
            check_casing(key, "fn::fileArchive", diags);
            let source = parse_expr(value, diags);
            return Some(Expr::FileArchive(meta, Box::new(source)));
        }
        "fn::remotearchive" => {
            check_casing(key, "fn::remoteArchive", diags);
            let source = parse_expr(value, diags);
            return Some(Expr::RemoteArchive(meta, Box::new(source)));
        }
        _ => {}
    }

    // Check function builtins
    match lower.as_str() {
        "fn::invoke" => {
            check_casing(key, "fn::invoke", diags);
            let args = parse_expr(value, diags);
            return Some(parse_invoke(args, meta, diags));
        }
        "fn::join" => {
            check_casing(key, "fn::join", diags);
            let args = parse_expr(value, diags);
            return Some(parse_join(args, meta, diags));
        }
        "fn::tojson" => {
            check_casing(key, "fn::toJSON", diags);
            let args = parse_expr(value, diags);
            return Some(Expr::ToJson(meta, Box::new(args)));
        }
        "fn::tobase64" => {
            check_casing(key, "fn::toBase64", diags);
            let args = parse_expr(value, diags);
            return Some(Expr::ToBase64(meta, Box::new(args)));
        }
        "fn::frombase64" => {
            check_casing(key, "fn::fromBase64", diags);
            let args = parse_expr(value, diags);
            return Some(Expr::FromBase64(meta, Box::new(args)));
        }
        "fn::select" => {
            check_casing(key, "fn::select", diags);
            let args = parse_expr(value, diags);
            return Some(parse_select(args, meta, diags));
        }
        "fn::split" => {
            check_casing(key, "fn::split", diags);
            let args = parse_expr(value, diags);
            return Some(parse_split(args, meta, diags));
        }
        "fn::stackreference" => {
            diags.error(
                None,
                "fn::stackReference is not supported; use a 'pulumi:pulumi:StackReference' resource type instead",
                "",
            );
            return Some(Expr::Null(meta));
        }
        "fn::assetarchive" => {
            check_casing(key, "fn::assetArchive", diags);
            let args = parse_expr(value, diags);
            return Some(parse_asset_archive(args, meta, diags));
        }
        "fn::secret" => {
            check_casing(key, "fn::secret", diags);
            let args = parse_expr(value, diags);
            return Some(Expr::Secret(meta, Box::new(args)));
        }
        "fn::readfile" => {
            check_casing(key, "fn::readFile", diags);
            let args = parse_expr(value, diags);
            return Some(Expr::ReadFile(meta, Box::new(args)));
        }
        // Math builtins
        "fn::abs" => {
            check_casing(key, "fn::abs", diags);
            let args = parse_expr(value, diags);
            return Some(Expr::Abs(meta, Box::new(args)));
        }
        "fn::floor" => {
            check_casing(key, "fn::floor", diags);
            let args = parse_expr(value, diags);
            return Some(Expr::Floor(meta, Box::new(args)));
        }
        "fn::ceil" => {
            check_casing(key, "fn::ceil", diags);
            let args = parse_expr(value, diags);
            return Some(Expr::Ceil(meta, Box::new(args)));
        }
        "fn::max" => {
            check_casing(key, "fn::max", diags);
            let args = parse_expr(value, diags);
            return Some(Expr::Max(meta, Box::new(args)));
        }
        "fn::min" => {
            check_casing(key, "fn::min", diags);
            let args = parse_expr(value, diags);
            return Some(Expr::Min(meta, Box::new(args)));
        }
        // String builtins
        "fn::stringlen" => {
            check_casing(key, "fn::stringLen", diags);
            let args = parse_expr(value, diags);
            return Some(Expr::StringLen(meta, Box::new(args)));
        }
        "fn::substring" => {
            check_casing(key, "fn::substring", diags);
            let args = parse_expr(value, diags);
            return Some(parse_substring(args, meta, diags));
        }
        // Time builtins
        "fn::timeutc" => {
            check_casing(key, "fn::timeUtc", diags);
            let args = parse_expr(value, diags);
            return Some(Expr::TimeUtc(meta, Box::new(args)));
        }
        "fn::timeunix" => {
            check_casing(key, "fn::timeUnix", diags);
            let args = parse_expr(value, diags);
            return Some(Expr::TimeUnix(meta, Box::new(args)));
        }
        // UUID/Random builtins
        "fn::uuid" => {
            check_casing(key, "fn::uuid", diags);
            let args = parse_expr(value, diags);
            return Some(Expr::Uuid(meta, Box::new(args)));
        }
        "fn::randomstring" => {
            check_casing(key, "fn::randomString", diags);
            let args = parse_expr(value, diags);
            return Some(Expr::RandomString(meta, Box::new(args)));
        }
        // Date builtins
        "fn::dateformat" => {
            check_casing(key, "fn::dateFormat", diags);
            let args = parse_expr(value, diags);
            return Some(Expr::DateFormat(meta, Box::new(args)));
        }
        _ => {}
    }

    // Check for fn::pkg:module(:name)? invoke shorthand
    if is_invoke_shorthand(key) {
        let fn_token = &key[4..]; // strip "fn::"
        return Some(parse_invoke_shorthand(fn_token, value, meta, diags));
    }

    // Warn about reserved fn:: prefix
    if lower.starts_with("fn::") {
        diags.warning(
            None,
            "'fn::' is a reserved prefix",
            format!(
                "If you need to use the raw key '{}', please open an issue at https://github.com/pulumi/pulumi-yaml/issues",
                key
            ),
        );
    }

    None
}

/// Checks if a key matches the fn::pkg:module(:name)? invoke shorthand pattern.
fn is_invoke_shorthand(key: &str) -> bool {
    let lower = key.to_lowercase();
    if !lower.starts_with("fn::") {
        return false;
    }
    let rest = &lower[4..];
    let colon_count = rest.chars().filter(|&c| c == ':').count();
    if !(1..=2).contains(&colon_count) {
        return false;
    }
    rest.split(':').all(|s| !s.is_empty())
}

fn check_casing(found: &str, expected: &str, diags: &mut Diagnostics) {
    if let Some(diag) = unexpected_casing(None, expected, found) {
        diags.add(diag);
    }
}

fn parse_invoke(args: Expr<'static>, meta: ExprMeta, diags: &mut Diagnostics) -> Expr<'static> {
    // We need to destructure args to extract the object entries
    let entries = match args {
        Expr::Object(_, entries) => entries,
        _ => {
            diags.error(
                None,
                "the argument to fn::invoke must be an object containing 'function', 'arguments', 'options', and 'return'",
                "",
            );
            return args;
        }
    };

    let mut token: Option<Cow<'static, str>> = None;
    let mut call_args: Option<Expr<'static>> = None;
    let mut return_: Option<Cow<'static, str>> = None;
    let mut opts = InvokeOptions::default();

    for entry in &entries {
        if let Some(key_str) = entry.key.as_str() {
            match key_str.to_lowercase().as_str() {
                "function" => {
                    token = entry.value.as_str().map(|s| Cow::Owned(s.to_string()));
                }
                "arguments" => {
                    call_args = Some((*entry.value).clone());
                }
                "return" => {
                    return_ = entry.value.as_str().map(|s| Cow::Owned(s.to_string()));
                }
                "options" => {
                    if let Expr::Object(_, ref opt_entries) = *entry.value {
                        for opt_entry in opt_entries {
                            if let Some(opt_key) = opt_entry.key.as_str() {
                                match opt_key.to_lowercase().as_str() {
                                    "parent" => {
                                        opts.parent = Some(Box::new((*opt_entry.value).clone()))
                                    }
                                    "provider" => {
                                        opts.provider = Some(Box::new((*opt_entry.value).clone()))
                                    }
                                    "dependson" => {
                                        opts.depends_on = Some(Box::new((*opt_entry.value).clone()))
                                    }
                                    "version" => {
                                        opts.version = opt_entry
                                            .value
                                            .as_str()
                                            .map(|s| Cow::Owned(s.to_string()))
                                    }
                                    "plugindownloadurl" => {
                                        opts.plugin_download_url = opt_entry
                                            .value
                                            .as_str()
                                            .map(|s| Cow::Owned(s.to_string()))
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    let token = match token {
        Some(t) => t,
        None => {
            diags.error(None, "missing function name ('function')", "");
            return Expr::Object(meta, entries);
        }
    };

    Expr::Invoke(
        meta,
        InvokeExpr {
            token,
            call_args: call_args.map(Box::new),
            call_opts: opts,
            return_,
        },
    )
}

fn parse_invoke_shorthand(
    fn_token: &str,
    value: &serde_yaml::Value,
    meta: ExprMeta,
    diags: &mut Diagnostics,
) -> Expr<'static> {
    let call_args = if value.is_mapping() {
        Some(Box::new(parse_expr(value, diags)))
    } else {
        None
    };

    Expr::Invoke(
        meta,
        InvokeExpr {
            token: Cow::Owned(fn_token.to_string()),
            call_args,
            call_opts: InvokeOptions::default(),
            return_: None,
        },
    )
}

fn parse_join(args: Expr<'static>, meta: ExprMeta, diags: &mut Diagnostics) -> Expr<'static> {
    match args {
        Expr::List(_, elements) if elements.len() == 2 => {
            let mut iter = elements.into_iter();
            let delimiter = iter.next().unwrap();
            let values = iter.next().unwrap();
            Expr::Join(meta, Box::new(delimiter), Box::new(values))
        }
        _ => {
            diags.error(
                None,
                "the argument to fn::join must be a two-valued list",
                "",
            );
            args
        }
    }
}

fn parse_select(args: Expr<'static>, meta: ExprMeta, diags: &mut Diagnostics) -> Expr<'static> {
    match args {
        Expr::List(_, elements) if elements.len() == 2 => {
            let mut iter = elements.into_iter();
            let index = iter.next().unwrap();
            let values = iter.next().unwrap();
            Expr::Select(meta, Box::new(index), Box::new(values))
        }
        _ => {
            diags.error(
                None,
                "the argument to fn::select must be a two-valued list",
                "",
            );
            args
        }
    }
}

fn parse_split(args: Expr<'static>, meta: ExprMeta, diags: &mut Diagnostics) -> Expr<'static> {
    match args {
        Expr::List(_, elements) if elements.len() == 2 => {
            let mut iter = elements.into_iter();
            let delimiter = iter.next().unwrap();
            let source = iter.next().unwrap();
            Expr::Split(meta, Box::new(delimiter), Box::new(source))
        }
        _ => {
            diags.error(
                None,
                "The argument to fn::split must be a two-values list",
                "",
            );
            args
        }
    }
}

fn parse_asset_archive(
    args: Expr<'static>,
    meta: ExprMeta,
    diags: &mut Diagnostics,
) -> Expr<'static> {
    match args {
        Expr::Object(_, entries) => {
            let mut assets: Vec<(Cow<'static, str>, Expr<'static>)> = Vec::new();
            for entry in entries {
                let key = match entry.key.as_str() {
                    Some(s) => Cow::Owned(s.to_string()),
                    None => {
                        diags.error(
                            None,
                            "keys in fn::assetArchive arguments must be string literals",
                            "",
                        );
                        continue;
                    }
                };
                if !entry.value.is_asset_or_archive() {
                    diags.error(None, "value must be an asset or an archive", "");
                }
                assets.push((key, *entry.value));
            }
            Expr::AssetArchive(meta, assets)
        }
        _ => {
            diags.error(
                None,
                "the argument to fn::assetArchive must be an object",
                "",
            );
            args
        }
    }
}

fn parse_substring(args: Expr<'static>, meta: ExprMeta, diags: &mut Diagnostics) -> Expr<'static> {
    match args {
        Expr::List(_, elements) if elements.len() == 3 => {
            let mut iter = elements.into_iter();
            let source = iter.next().unwrap();
            let start = iter.next().unwrap();
            let length = iter.next().unwrap();
            Expr::Substring(meta, Box::new(source), Box::new(start), Box::new(length))
        }
        _ => {
            diags.error(
                None,
                "the argument to fn::substring must be a three-valued list [string, start, length]",
                "",
            );
            args
        }
    }
}

// --- Template-level parsing helpers ---

fn parse_pulumi_decl(value: &serde_yaml::Value, diags: &mut Diagnostics) -> PulumiDecl<'static> {
    let mut decl = PulumiDecl::default();
    if let Some(map) = value.as_mapping() {
        for (k, v) in map {
            if let Some(key) = k.as_str() {
                if key.to_lowercase() == "requiredversion" {
                    decl.required_version = Some(parse_expr(v, diags));
                }
            }
        }
    }
    decl
}

fn parse_config_map(
    value: &serde_yaml::Value,
    diags: &mut Diagnostics,
) -> Vec<ConfigEntry<'static>> {
    let map = match value.as_mapping() {
        Some(m) => m,
        None => {
            diags.error(None, "config must be an object", "");
            return Vec::new();
        }
    };

    let mut entries = Vec::with_capacity(map.len());
    for (k, v) in map {
        let key = match k.as_str() {
            Some(s) => s,
            None => continue,
        };
        let param = if v.is_mapping() {
            parse_config_param(v, diags)
        } else {
            ConfigParamDecl {
                value: Some(parse_expr(v, diags)),
                ..Default::default()
            }
        };
        entries.push(ConfigEntry {
            meta: ExprMeta::no_span(),
            key: Cow::Owned(key.to_string()),
            param,
        });
    }
    entries
}

fn parse_config_param(
    value: &serde_yaml::Value,
    diags: &mut Diagnostics,
) -> ConfigParamDecl<'static> {
    let mut param = ConfigParamDecl::default();
    if let Some(map) = value.as_mapping() {
        for (k, v) in map {
            if let Some(key) = k.as_str() {
                match key.to_lowercase().as_str() {
                    "type" => param.type_ = v.as_str().map(|s| Cow::Owned(s.to_string())),
                    "name" => param.name = v.as_str().map(|s| Cow::Owned(s.to_string())),
                    "secret" => param.secret = v.as_bool(),
                    "default" => param.default = Some(parse_expr(v, diags)),
                    "value" => param.value = Some(parse_expr(v, diags)),
                    "items" => {
                        param.items = Some(Box::new(parse_config_param(v, diags)));
                    }
                    _ => {}
                }
            }
        }
    }
    param
}

fn parse_variables_map(
    value: &serde_yaml::Value,
    diags: &mut Diagnostics,
) -> Vec<VariableEntry<'static>> {
    let map = match value.as_mapping() {
        Some(m) => m,
        None => {
            diags.error(None, "variables must be an object", "");
            return Vec::new();
        }
    };

    let mut entries = Vec::with_capacity(map.len());
    for (k, v) in map {
        let key = match k.as_str() {
            Some(s) => s,
            None => continue,
        };
        entries.push(VariableEntry {
            meta: ExprMeta::no_span(),
            key: Cow::Owned(key.to_string()),
            value: parse_expr(v, diags),
        });
    }
    entries
}

fn parse_resources_map(
    value: &serde_yaml::Value,
    diags: &mut Diagnostics,
) -> Vec<ResourceEntry<'static>> {
    let map = match value.as_mapping() {
        Some(m) => m,
        None => {
            diags.error(None, "resources must be an object", "");
            return Vec::new();
        }
    };

    let mut entries = Vec::with_capacity(map.len());
    for (k, v) in map {
        let key = match k.as_str() {
            Some(s) => s,
            None => continue,
        };
        let resource = parse_resource_decl(v, diags);
        entries.push(ResourceEntry {
            meta: ExprMeta::no_span(),
            logical_name: Cow::Owned(key.to_string()),
            resource,
        });
    }
    entries
}

fn parse_resource_decl(
    value: &serde_yaml::Value,
    diags: &mut Diagnostics,
) -> ResourceDecl<'static> {
    let map = match value.as_mapping() {
        Some(m) => m,
        None => {
            diags.error(None, "resource must be an object", "");
            return ResourceDecl {
                type_: Cow::Owned(String::new()),
                name: None,
                default_provider: None,
                properties: ResourceProperties::default(),
                options: ResourceOptionsDecl::default(),
                get: None,
            };
        }
    };

    let mut type_: Cow<'static, str> = Cow::Owned(String::new());
    let mut name = None;
    let mut default_provider = None;
    let mut properties = ResourceProperties::default();
    let mut options = ResourceOptionsDecl::default();
    let mut get = None;

    for (k, v) in map {
        let key = match k.as_str() {
            Some(s) => s,
            None => continue,
        };
        match key.to_lowercase().as_str() {
            "type" => {
                if let Some(s) = v.as_str() {
                    type_ = Cow::Owned(s.to_string());
                }
            }
            "name" => name = v.as_str().map(|s| Cow::Owned(s.to_string())),
            "defaultprovider" => default_provider = v.as_bool(),
            "properties" => {
                if let Some(m) = v.as_mapping() {
                    let props: Vec<PropertyEntry<'static>> = m
                        .iter()
                        .filter_map(|(pk, pv)| {
                            let pk_str = pk.as_str()?;
                            Some(PropertyEntry {
                                key: Cow::Owned(pk_str.to_string()),
                                value: parse_expr(pv, diags),
                            })
                        })
                        .collect();
                    properties = ResourceProperties::Map(props);
                } else {
                    properties = ResourceProperties::Expr(Box::new(parse_expr(v, diags)));
                }
            }
            "options" => {
                options = parse_resource_options(v, diags);
            }
            "get" => {
                get = Some(parse_get_resource(v, diags));
            }
            _ => {}
        }
    }

    ResourceDecl {
        type_,
        name,
        default_provider,
        properties,
        options,
        get,
    }
}

fn parse_resource_options(
    value: &serde_yaml::Value,
    diags: &mut Diagnostics,
) -> ResourceOptionsDecl<'static> {
    let mut opts = ResourceOptionsDecl::default();
    let map = match value.as_mapping() {
        Some(m) => m,
        None => return opts,
    };

    for (k, v) in map {
        let key = match k.as_str() {
            Some(s) => s,
            None => continue,
        };
        match key.to_lowercase().as_str() {
            "additionalsecretoutputs" => {
                opts.additional_secret_outputs = parse_string_list_owned(v);
            }
            "aliases" => opts.aliases = Some(parse_expr(v, diags)),
            "customtimeouts" => {
                opts.custom_timeouts = Some(parse_custom_timeouts(v));
            }
            "deletebeforereplace" => opts.delete_before_replace = v.as_bool(),
            "dependson" => opts.depends_on = Some(parse_expr(v, diags)),
            "ignorechanges" => {
                opts.ignore_changes = parse_string_list_owned(v);
            }
            "import" => opts.import = v.as_str().map(|s| Cow::Owned(s.to_string())),
            "parent" => opts.parent = Some(parse_expr(v, diags)),
            "protect" => opts.protect = Some(parse_expr(v, diags)),
            "provider" => opts.provider = Some(parse_expr(v, diags)),
            "providers" => opts.providers = Some(parse_expr(v, diags)),
            "version" => opts.version = v.as_str().map(|s| Cow::Owned(s.to_string())),
            "plugindownloadurl" => {
                opts.plugin_download_url = v.as_str().map(|s| Cow::Owned(s.to_string()));
            }
            "replaceonchanges" => {
                opts.replace_on_changes = parse_string_list_owned(v);
            }
            "retainondelete" => opts.retain_on_delete = v.as_bool(),
            "replacewith" => opts.replace_with = Some(parse_expr(v, diags)),
            "deletedwith" => opts.deleted_with = Some(parse_expr(v, diags)),
            "hidediffs" => {
                opts.hide_diffs = parse_string_list_owned(v);
            }
            _ => {}
        }
    }

    opts
}

fn parse_custom_timeouts(value: &serde_yaml::Value) -> CustomTimeoutsDecl<'static> {
    let mut ct = CustomTimeoutsDecl::default();
    if let Some(map) = value.as_mapping() {
        for (k, v) in map {
            if let Some(key) = k.as_str() {
                match key.to_lowercase().as_str() {
                    "create" => ct.create = v.as_str().map(|s| Cow::Owned(s.to_string())),
                    "update" => ct.update = v.as_str().map(|s| Cow::Owned(s.to_string())),
                    "delete" => ct.delete = v.as_str().map(|s| Cow::Owned(s.to_string())),
                    _ => {}
                }
            }
        }
    }
    ct
}

fn parse_get_resource(
    value: &serde_yaml::Value,
    diags: &mut Diagnostics,
) -> GetResourceDecl<'static> {
    let map = match value.as_mapping() {
        Some(m) => m,
        None => {
            diags.error(None, "get must be an object", "");
            return GetResourceDecl {
                id: Expr::Null(ExprMeta::no_span()),
                state: Vec::new(),
            };
        }
    };

    let mut id = Expr::Null(ExprMeta::no_span());
    let mut state = Vec::new();

    for (k, v) in map {
        let key = match k.as_str() {
            Some(s) => s,
            None => continue,
        };
        match key.to_lowercase().as_str() {
            "id" => id = parse_expr(v, diags),
            "state" => {
                if let Some(m) = v.as_mapping() {
                    state = m
                        .iter()
                        .filter_map(|(sk, sv)| {
                            let sk_str = sk.as_str()?;
                            Some(PropertyEntry {
                                key: Cow::Owned(sk_str.to_string()),
                                value: parse_expr(sv, diags),
                            })
                        })
                        .collect();
                }
            }
            _ => {}
        }
    }

    GetResourceDecl { id, state }
}

fn parse_outputs_map(
    value: &serde_yaml::Value,
    diags: &mut Diagnostics,
) -> Vec<OutputEntry<'static>> {
    let map = match value.as_mapping() {
        Some(m) => m,
        None => {
            diags.error(None, "outputs must be an object", "");
            return Vec::new();
        }
    };

    let mut entries = Vec::with_capacity(map.len());
    for (k, v) in map {
        let key = match k.as_str() {
            Some(s) => s,
            None => continue,
        };
        entries.push(OutputEntry {
            key: Cow::Owned(key.to_string()),
            value: parse_expr(v, diags),
        });
    }
    entries
}

fn parse_components(
    value: &serde_yaml::Value,
    diags: &mut Diagnostics,
) -> Vec<ComponentDecl<'static>> {
    let map = match value.as_mapping() {
        Some(m) => m,
        None => {
            diags.error(None, "components must be an object", "");
            return Vec::new();
        }
    };

    let mut components = Vec::with_capacity(map.len());
    for (k, v) in map {
        let key = match k.as_str() {
            Some(s) => s,
            None => continue,
        };
        let comp = parse_component_param(v, diags);
        components.push(ComponentDecl {
            key: Cow::Owned(key.to_string()),
            component: comp,
        });
    }
    components
}

fn parse_component_param(
    value: &serde_yaml::Value,
    diags: &mut Diagnostics,
) -> ComponentParamDecl<'static> {
    let mut comp = ComponentParamDecl {
        name: None,
        description: None,
        pulumi: PulumiDecl::default(),
        inputs: Vec::new(),
        variables: Vec::new(),
        resources: Vec::new(),
        outputs: Vec::new(),
    };

    if let Some(map) = value.as_mapping() {
        for (k, v) in map {
            if let Some(key) = k.as_str() {
                match key.to_lowercase().as_str() {
                    "name" => comp.name = v.as_str().map(|s| Cow::Owned(s.to_string())),
                    "description" => {
                        comp.description = v.as_str().map(|s| Cow::Owned(s.to_string()))
                    }
                    "pulumi" => comp.pulumi = parse_pulumi_decl(v, diags),
                    "inputs" => comp.inputs = parse_config_map(v, diags),
                    "variables" => comp.variables = parse_variables_map(v, diags),
                    "resources" => comp.resources = parse_resources_map(v, diags),
                    "outputs" => comp.outputs = parse_outputs_map(v, diags),
                    _ => {}
                }
            }
        }
    }

    comp
}

fn parse_string_list_owned(value: &serde_yaml::Value) -> Option<Vec<Cow<'static, str>>> {
    let seq = value.as_sequence()?;
    let list: Vec<Cow<'static, str>> = seq
        .iter()
        .filter_map(|v| v.as_str().map(|s| Cow::Owned(s.to_string())))
        .collect();
    Some(list)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_minimal_template() {
        let source = r#"
name: test
runtime: yaml
"#;
        let (template, diags) = parse_template(source, None);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert_eq!(template.name.as_deref(), Some("test"));
    }

    #[test]
    fn test_parse_template_with_resources() {
        let source = r#"
name: test
runtime: yaml
resources:
  bucket:
    type: aws:s3:Bucket
    properties:
      bucketName: my-bucket
"#;
        let (template, diags) = parse_template(source, None);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert_eq!(template.resources.len(), 1);
        assert_eq!(template.resources[0].logical_name.as_ref(), "bucket");
        assert_eq!(
            template.resources[0].resource.type_.as_ref(),
            "aws:s3:Bucket"
        );
    }

    #[test]
    fn test_parse_template_with_config() {
        let source = r#"
name: test
runtime: yaml
config:
  myParam:
    type: string
    default: hello
"#;
        let (template, diags) = parse_template(source, None);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert_eq!(template.config.len(), 1);
        assert_eq!(template.config[0].key.as_ref(), "myParam");
        assert_eq!(template.config[0].param.type_.as_deref(), Some("string"));
    }

    #[test]
    fn test_parse_template_with_variables() {
        let source = r#"
name: test
runtime: yaml
variables:
  suffix:
    fn::invoke:
      function: random:index:RandomString
      arguments:
        length: 8
      return: result
"#;
        let (template, diags) = parse_template(source, None);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert_eq!(template.variables.len(), 1);
        assert_eq!(template.variables[0].key.as_ref(), "suffix");
        match &template.variables[0].value {
            Expr::Invoke(_, invoke) => {
                assert_eq!(invoke.token.as_ref(), "random:index:RandomString");
                assert_eq!(invoke.return_.as_deref(), Some("result"));
            }
            other => panic!("expected invoke, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_template_with_outputs() {
        let source = r#"
name: test
runtime: yaml
resources:
  bucket:
    type: aws:s3:Bucket
outputs:
  bucketName: ${bucket.id}
"#;
        let (template, diags) = parse_template(source, None);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert_eq!(template.outputs.len(), 1);
        assert_eq!(template.outputs[0].key.as_ref(), "bucketName");
        assert!(template.outputs[0].value.is_symbol());
    }

    #[test]
    fn test_parse_string_expr_plain() {
        let mut diags = Diagnostics::new();
        let expr = parse_string_expr_owned("hello", ExprMeta::no_span(), &mut diags);
        assert!(!diags.has_errors());
        assert_eq!(expr.as_str(), Some("hello"));
    }

    #[test]
    fn test_parse_string_expr_symbol() {
        let mut diags = Diagnostics::new();
        let expr = parse_string_expr_owned("${resource.prop}", ExprMeta::no_span(), &mut diags);
        assert!(!diags.has_errors());
        assert!(expr.is_symbol());
    }

    #[test]
    fn test_parse_string_expr_interpolation() {
        let mut diags = Diagnostics::new();
        let expr = parse_string_expr_owned(
            "prefix-${resource.prop}-suffix",
            ExprMeta::no_span(),
            &mut diags,
        );
        assert!(!diags.has_errors());
        match expr {
            Expr::Interpolate(_, parts) => {
                assert_eq!(parts.len(), 2);
            }
            other => panic!("expected interpolate, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_join() {
        let source = r#"
name: test
runtime: yaml
variables:
  joined:
    fn::join:
      - ","
      - ["a", "b", "c"]
"#;
        let (template, diags) = parse_template(source, None);
        assert!(!diags.has_errors(), "errors: {}", diags);
        match &template.variables[0].value {
            Expr::Join(_, delimiter, values) => {
                assert_eq!(delimiter.as_str(), Some(","));
                match values.as_ref() {
                    Expr::List(_, elements) => assert_eq!(elements.len(), 3),
                    other => panic!("expected list, got {:?}", other),
                }
            }
            other => panic!("expected join, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_secret() {
        let source = r#"
name: test
runtime: yaml
variables:
  secretVal:
    fn::secret: my-secret-value
"#;
        let (template, diags) = parse_template(source, None);
        assert!(!diags.has_errors(), "errors: {}", diags);
        match &template.variables[0].value {
            Expr::Secret(_, inner) => {
                assert_eq!(inner.as_str(), Some("my-secret-value"));
            }
            other => panic!("expected secret, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_resource_options() {
        let source = r#"
name: test
runtime: yaml
resources:
  bucket:
    type: aws:s3:Bucket
    options:
      protect: true
      dependsOn:
        - ${other}
      ignoreChanges:
        - tags
"#;
        let (template, diags) = parse_template(source, None);
        assert!(!diags.has_errors(), "errors: {}", diags);
        let opts = &template.resources[0].resource.options;
        assert!(opts.protect.is_some());
        assert!(opts.depends_on.is_some());
        assert_eq!(opts.ignore_changes.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn test_parse_invalid_yaml() {
        let source = "{{invalid yaml";
        let (_, diags) = parse_template(source, None);
        assert!(diags.has_errors());
    }

    #[test]
    fn test_parse_non_mapping_toplevel() {
        let source = "- list\n- item\n";
        let (_, diags) = parse_template(source, None);
        assert!(diags.has_errors());
    }

    #[test]
    fn test_is_invoke_shorthand() {
        assert!(is_invoke_shorthand("fn::aws:s3:getBucket"));
        assert!(is_invoke_shorthand("fn::random:index:RandomString"));
        assert!(is_invoke_shorthand("fn::pkg:mod"));
        assert!(!is_invoke_shorthand("fn::invoke"));
        assert!(!is_invoke_shorthand("fn::join"));
        assert!(!is_invoke_shorthand("not-fn"));
        assert!(!is_invoke_shorthand("fn::"));
    }

    #[test]
    fn test_parse_to_json() {
        let source = r#"
name: test
runtime: yaml
variables:
  json:
    fn::toJSON:
      key: value
"#;
        let (template, diags) = parse_template(source, None);
        assert!(!diags.has_errors(), "errors: {}", diags);
        match &template.variables[0].value {
            Expr::ToJson(_, _) => {}
            other => panic!("expected toJSON, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_base64() {
        let source = r#"
name: test
runtime: yaml
variables:
  encoded:
    fn::toBase64: hello
  decoded:
    fn::fromBase64: aGVsbG8=
"#;
        let (template, diags) = parse_template(source, None);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert!(matches!(&template.variables[0].value, Expr::ToBase64(_, _)));
        assert!(matches!(
            &template.variables[1].value,
            Expr::FromBase64(_, _)
        ));
    }

    #[test]
    fn test_parse_select() {
        let source = r#"
name: test
runtime: yaml
variables:
  selected:
    fn::select:
      - 1
      - ["a", "b", "c"]
"#;
        let (template, diags) = parse_template(source, None);
        assert!(!diags.has_errors(), "errors: {}", diags);
        match &template.variables[0].value {
            Expr::Select(_, idx, vals) => {
                match idx.as_ref() {
                    Expr::Number(_, n) => assert_eq!(*n, 1.0),
                    other => panic!("expected number, got {:?}", other),
                }
                match vals.as_ref() {
                    Expr::List(_, elements) => assert_eq!(elements.len(), 3),
                    other => panic!("expected list, got {:?}", other),
                }
            }
            other => panic!("expected select, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_split() {
        let source = r#"
name: test
runtime: yaml
variables:
  parts:
    fn::split:
      - ","
      - "a,b,c"
"#;
        let (template, diags) = parse_template(source, None);
        assert!(!diags.has_errors(), "errors: {}", diags);
        match &template.variables[0].value {
            Expr::Split(_, delim, source_expr) => {
                assert_eq!(delim.as_str(), Some(","));
                assert_eq!(source_expr.as_str(), Some("a,b,c"));
            }
            other => panic!("expected split, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_file_asset() {
        let source = r#"
name: test
runtime: yaml
resources:
  obj:
    type: aws:s3:BucketObject
    properties:
      source:
        fn::fileAsset: ./index.html
"#;
        let (template, diags) = parse_template(source, None);
        assert!(!diags.has_errors(), "errors: {}", diags);
        let props = match &template.resources[0].resource.properties {
            ResourceProperties::Map(props) => props,
            _ => panic!("expected map"),
        };
        match &props[0].value {
            Expr::FileAsset(_, source) => {
                assert_eq!(source.as_str(), Some("./index.html"));
            }
            other => panic!("expected fileAsset, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_pulumi_required_version() {
        let source = r#"
name: test
runtime: yaml
pulumi:
  requiredVersion: ">=3.0.0"
"#;
        let (template, diags) = parse_template(source, None);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert!(template.pulumi.has_settings());
        match &template.pulumi.required_version {
            Some(Expr::String(_, s)) => assert_eq!(s.as_ref(), ">=3.0.0"),
            other => panic!("expected string, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_abs() {
        let source = "name: test\nruntime: yaml\nvariables:\n  v:\n    fn::abs: -42\n";
        let (template, diags) = parse_template(source, None);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert!(matches!(&template.variables[0].value, Expr::Abs(_, _)));
    }

    #[test]
    fn test_parse_floor() {
        let source = "name: test\nruntime: yaml\nvariables:\n  v:\n    fn::floor: 3.7\n";
        let (template, diags) = parse_template(source, None);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert!(matches!(&template.variables[0].value, Expr::Floor(_, _)));
    }

    #[test]
    fn test_parse_ceil() {
        let source = "name: test\nruntime: yaml\nvariables:\n  v:\n    fn::ceil: 3.2\n";
        let (template, diags) = parse_template(source, None);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert!(matches!(&template.variables[0].value, Expr::Ceil(_, _)));
    }

    #[test]
    fn test_parse_max() {
        let source = "name: test\nruntime: yaml\nvariables:\n  v:\n    fn::max: [1, 5, 3]\n";
        let (template, diags) = parse_template(source, None);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert!(matches!(&template.variables[0].value, Expr::Max(_, _)));
    }

    #[test]
    fn test_parse_min() {
        let source = "name: test\nruntime: yaml\nvariables:\n  v:\n    fn::min: [1, 5, 3]\n";
        let (template, diags) = parse_template(source, None);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert!(matches!(&template.variables[0].value, Expr::Min(_, _)));
    }

    #[test]
    fn test_parse_string_len() {
        let source = "name: test\nruntime: yaml\nvariables:\n  v:\n    fn::stringLen: hello\n";
        let (template, diags) = parse_template(source, None);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert!(matches!(
            &template.variables[0].value,
            Expr::StringLen(_, _)
        ));
    }

    #[test]
    fn test_parse_substring() {
        let source = "name: test\nruntime: yaml\nvariables:\n  v:\n    fn::substring:\n      - hello\n      - 0\n      - 3\n";
        let (template, diags) = parse_template(source, None);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert!(matches!(
            &template.variables[0].value,
            Expr::Substring(_, _, _, _)
        ));
    }

    #[test]
    fn test_parse_time_utc() {
        let source = "name: test\nruntime: yaml\nvariables:\n  v:\n    fn::timeUtc: {}\n";
        let (template, diags) = parse_template(source, None);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert!(matches!(&template.variables[0].value, Expr::TimeUtc(_, _)));
    }

    #[test]
    fn test_parse_time_unix() {
        let source = "name: test\nruntime: yaml\nvariables:\n  v:\n    fn::timeUnix: {}\n";
        let (template, diags) = parse_template(source, None);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert!(matches!(&template.variables[0].value, Expr::TimeUnix(_, _)));
    }

    #[test]
    fn test_parse_uuid() {
        let source = "name: test\nruntime: yaml\nvariables:\n  v:\n    fn::uuid: {}\n";
        let (template, diags) = parse_template(source, None);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert!(matches!(&template.variables[0].value, Expr::Uuid(_, _)));
    }

    #[test]
    fn test_parse_random_string() {
        let source = "name: test\nruntime: yaml\nvariables:\n  v:\n    fn::randomString: 32\n";
        let (template, diags) = parse_template(source, None);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert!(matches!(
            &template.variables[0].value,
            Expr::RandomString(_, _)
        ));
    }

    #[test]
    fn test_parse_date_format() {
        let source =
            "name: test\nruntime: yaml\nvariables:\n  v:\n    \"fn::dateFormat\": \"%Y-%m-%d\"\n";
        let (template, diags) = parse_template(source, None);
        assert!(!diags.has_errors(), "errors: {}", diags);
        assert!(matches!(
            &template.variables[0].value,
            Expr::DateFormat(_, _)
        ));
    }
}

//! dbt environment resolution and `{{ }}` substitution, turning
//! Jinja-laced model SQL into parseable BigQuery SQL while preserving
//! lineage: `ref()`/`source()`/`this` become backticked fully-qualified
//! names; declared macros are textually expanded one level so column
//! references inside them survive; anything unknown degrades to `NULL`
//! (scalar position) or an `__unresolved__` marker table (relation
//! position) with a warning — never a parse-breaking hole.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};

use crate::ast::expr::Expr;
use crate::ast::template::ResourceEntry;
use crate::diag::Diagnostics;
use crate::jinja::{extract_root_identifier, find_expression_end, strip_jinja_blocks};
use crate::literal_resolve::resolve_literal;

use super::ids::{table_name, TableName, UNRESOLVED};
use super::resolve::{get_property_by_path, literal_prop};

const MAX_EXPANSION_DEPTH: usize = 8;

/// A resolved dbt project declaration.
pub(crate) struct DbtProject {
    pub gcp_project: Option<String>,
    pub dataset: Option<String>,
    /// source name → (dataset, declared tables)
    pub sources: BTreeMap<String, (String, Vec<String>)>,
}

/// A resolved dbt macro: declared arg names + SQL fragment.
pub(crate) struct DbtMacro {
    pub args: Vec<String>,
    pub sql: String,
}

/// A resolved dbt model.
pub(crate) struct DbtModel {
    pub fq: Option<TableName>,
    /// dbt ref name → target model logical name.
    pub model_refs: BTreeMap<String, String>,
    /// call-site macro name → macro resource logical name.
    pub macros: BTreeMap<String, String>,
    /// Logical name of the model's dbt project resource, if resolvable.
    pub project_logical: Option<String>,
}

/// Full dbt environment for one template scope.
pub(crate) struct DbtEnv {
    pub projects: BTreeMap<String, DbtProject>,
    pub models: BTreeMap<String, DbtModel>,
    pub macros: BTreeMap<String, DbtMacro>,
}

fn is_dbt_type(token: &str, name: &str) -> bool {
    super::registry::matches_token(&format!("gcpx:dbt:{}", name), token)
}

/// Extracts `${res}`- or `${res.attr}`-style root from an expression.
fn symbol_root<'src>(expr: &'src Expr<'src>) -> Option<&'src str> {
    match expr {
        Expr::Symbol(_, access) => access.root_name().ok(),
        _ => None,
    }
}

impl DbtEnv {
    pub(crate) fn build_scope<'src>(
        resources: &'src [ResourceEntry<'src>],
        variables: &HashMap<&'src str, &'src Expr<'src>>,
        diags: &mut Diagnostics,
    ) -> Self {
        let mut projects = BTreeMap::new();
        let mut macros = BTreeMap::new();
        let mut models = BTreeMap::new();

        for entry in resources {
            let token = entry.resource.type_.as_ref();
            let logical = entry.logical_name.to_string();
            if is_dbt_type(token, "Project") {
                projects.insert(logical, build_project(entry, variables));
            } else if is_dbt_type(token, "Macro") {
                if let Some(m) = build_macro(entry, variables) {
                    macros.insert(logical, m);
                }
            }
        }

        for entry in resources {
            if !is_dbt_type(entry.resource.type_.as_ref(), "Model") {
                continue;
            }
            let model = build_model(entry, variables, &projects, diags);
            models.insert(entry.logical_name.to_string(), model);
        }

        Self {
            projects,
            models,
            macros,
        }
    }

    /// Looks up a ref name for `model`: its own `modelRefs` first, then
    /// any sibling model whose table name matches within the same
    /// project.
    fn resolve_ref(&self, model: &DbtModel, ref_name: &str) -> Option<TableName> {
        if let Some(target_logical) = model.model_refs.get(ref_name) {
            if let Some(target) = self.models.get(target_logical) {
                return target.fq.clone();
            }
        }
        self.models
            .values()
            .find(|m| {
                m.project_logical == model.project_logical
                    && m.fq.as_ref().is_some_and(|fq| fq.table == ref_name)
            })
            .and_then(|m| m.fq.clone())
    }

    fn resolve_source(&self, model: &DbtModel, source: &str, table: &str) -> Option<TableName> {
        let project = match &model.project_logical {
            Some(logical) => self.projects.get(logical),
            None if self.projects.len() == 1 => self.projects.values().next(),
            None => None,
        }?;
        let (dataset, tables) = project.sources.get(source)?;
        let gcp_project = project.gcp_project.as_deref()?;
        let known = tables.iter().any(|t| t == table);
        let name = table_name(gcp_project, dataset, table)?;
        Some(if known {
            name
        } else {
            // Still substitute — the source exists, the table just is
            // not declared; caller warns.
            name
        })
    }
}

fn build_project<'src>(
    entry: &'src ResourceEntry<'src>,
    variables: &HashMap<&'src str, &'src Expr<'src>>,
) -> DbtProject {
    let gcp_project =
        literal_prop(entry, variables, &["gcpProject", "project"]).map(Cow::into_owned);
    let dataset = literal_prop(entry, variables, &["dataset"]).map(Cow::into_owned);
    let mut sources = BTreeMap::new();
    if let Some(Expr::Object(_, source_entries)) =
        get_property_by_path(&entry.resource.properties, "sources")
    {
        for source in source_entries {
            let Expr::String(_, source_name) = source.key.as_ref() else {
                continue;
            };
            let Expr::Object(_, fields) = source.value.as_ref() else {
                continue;
            };
            let mut memo = HashMap::new();
            let mut visiting = HashSet::new();
            let dataset = fields
                .iter()
                .find(|f| matches!(f.key.as_ref(), Expr::String(_, k) if k == "dataset"))
                .and_then(|f| resolve_literal(&f.value, variables, &mut memo, &mut visiting));
            let mut tables = Vec::new();
            if let Some(Expr::List(_, items)) = fields
                .iter()
                .find(|f| matches!(f.key.as_ref(), Expr::String(_, k) if k == "tables"))
                .map(|f| f.value.as_ref())
            {
                for item in items {
                    if let Some(t) = resolve_literal(item, variables, &mut memo, &mut visiting) {
                        tables.push(t.into_owned());
                    }
                }
            }
            if let Some(ds) = dataset {
                sources.insert(source_name.to_string(), (ds.into_owned(), tables));
            }
        }
    }
    DbtProject {
        gcp_project,
        dataset,
        sources,
    }
}

fn build_macro<'src>(
    entry: &'src ResourceEntry<'src>,
    variables: &HashMap<&'src str, &'src Expr<'src>>,
) -> Option<DbtMacro> {
    let sql = literal_prop(entry, variables, &["sql"])?.into_owned();
    let mut args = Vec::new();
    if let Some(Expr::List(_, items)) = get_property_by_path(&entry.resource.properties, "args") {
        let mut memo = HashMap::new();
        let mut visiting = HashSet::new();
        for item in items {
            if let Some(a) = resolve_literal(item, variables, &mut memo, &mut visiting) {
                args.push(a.into_owned());
            }
        }
    }
    Some(DbtMacro { args, sql })
}

fn build_model<'src>(
    entry: &'src ResourceEntry<'src>,
    variables: &HashMap<&'src str, &'src Expr<'src>>,
    projects: &BTreeMap<String, DbtProject>,
    diags: &mut Diagnostics,
) -> DbtModel {
    // Context: either a symbol into a gcpx:dbt:Project resource, or an
    // inline object with gcpProject/project + dataset.
    let mut project_logical = None;
    let mut gcp_project = None;
    let mut dataset = None;
    match get_property_by_path(&entry.resource.properties, "context") {
        Some(expr) => {
            if let Some(root) = symbol_root(expr) {
                if let Some(project) = projects.get(root) {
                    project_logical = Some(root.to_string());
                    gcp_project = project.gcp_project.clone();
                    dataset = project.dataset.clone();
                }
            } else if let Expr::Object(_, fields) = expr {
                let mut memo: HashMap<&str, Option<Cow<'_, str>>> = HashMap::new();
                let mut visiting: HashSet<&str> = HashSet::new();
                let get = |key: &str| {
                    fields
                        .iter()
                        .find(|f| matches!(f.key.as_ref(), Expr::String(_, k) if k == key))
                        .and_then(|f| {
                            resolve_literal(
                                &f.value,
                                variables,
                                &mut HashMap::new(),
                                &mut HashSet::new(),
                            )
                        })
                        .map(Cow::into_owned)
                };
                gcp_project = get("gcpProject").or_else(|| get("project"));
                dataset = get("dataset");
                let _ = (&mut memo, &mut visiting);
            }
        }
        None => {
            if projects.len() == 1 {
                if let Some((logical, project)) = projects.iter().next() {
                    project_logical = Some(logical.clone());
                    gcp_project = project.gcp_project.clone();
                    dataset = project.dataset.clone();
                }
            }
        }
    }

    let table = literal_prop(entry, variables, &["name"])
        .map(Cow::into_owned)
        .unwrap_or_else(|| entry.logical_name.to_string());

    let fq = match (&gcp_project, &dataset) {
        (Some(p), Some(d)) => table_name(p, d, &table),
        _ => {
            diags.warning(
                None,
                format!(
                    "dbt model '{}' has no resolvable project/dataset context",
                    entry.logical_name
                ),
                "its lineage is limited to structural modelRefs edges",
            );
            None
        }
    };

    let collect_map = |path: &str| -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        if let Some(Expr::Object(_, fields)) =
            get_property_by_path(&entry.resource.properties, path)
        {
            for field in fields {
                if let Expr::String(_, key) = field.key.as_ref() {
                    if let Expr::Symbol(_, access) = field.value.as_ref() {
                        if let Ok(root) = access.root_name() {
                            out.insert(key.to_string(), root.to_string());
                        }
                    }
                }
            }
        }
        out
    };

    DbtModel {
        fq,
        model_refs: collect_map("modelRefs"),
        macros: collect_map("macros"),
        project_logical,
    }
}

/// Splits a call-expression body like `ref('x')` into (fn, args).
/// Args are split on top-level commas respecting quotes; each arg keeps
/// its raw text. Returns `None` when the body is not a simple call.
fn parse_call(body: &str) -> Option<(&str, Vec<&str>)> {
    let open = body.find('(')?;
    let name = body[..open].trim();
    if name.is_empty() || !body.trim_end().ends_with(')') {
        return None;
    }
    let inner = &body[open + 1..body.trim_end().len() - 1];
    let mut args = Vec::new();
    let mut depth = 0u32;
    let mut quote: Option<char> = None;
    let mut start = 0usize;
    for (i, ch) in inner.char_indices() {
        match quote {
            Some(q) => {
                if ch == q {
                    quote = None;
                }
            }
            None => match ch {
                '\'' | '"' => quote = Some(ch),
                '(' => depth += 1,
                ')' => depth = depth.saturating_sub(1),
                ',' if depth == 0 => {
                    args.push(inner[start..i].trim());
                    start = i + 1;
                }
                _ => {}
            },
        }
    }
    let last = inner[start..].trim();
    if !last.is_empty() {
        args.push(last);
    }
    Some((name, args))
}

/// Strips one layer of surrounding quotes from a raw arg.
fn unquote(raw: &str) -> Option<&str> {
    let trimmed = raw.trim();
    if trimmed.len() >= 2 {
        let bytes = trimmed.as_bytes();
        if (bytes[0] == b'\'' && bytes[trimmed.len() - 1] == b'\'')
            || (bytes[0] == b'"' && bytes[trimmed.len() - 1] == b'"')
        {
            return Some(&trimmed[1..trimmed.len() - 1]);
        }
    }
    None
}

/// Word-boundary textual replacement used for macro arg binding,
/// skipping matches inside quoted strings.
fn replace_word(fragment: &str, word: &str, replacement: &str) -> String {
    let mut out = String::with_capacity(fragment.len());
    let bytes = fragment.as_bytes();
    let mut i = 0;
    let mut quote: Option<u8> = None;
    let is_word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    while i < bytes.len() {
        if let Some(q) = quote {
            out.push(bytes[i] as char);
            if bytes[i] == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        match bytes[i] {
            b'\'' | b'"' => {
                quote = Some(bytes[i]);
                out.push(bytes[i] as char);
                i += 1;
            }
            _ if fragment[i..].starts_with(word)
                && (i == 0 || !is_word(bytes[i - 1]))
                && (i + word.len() >= bytes.len() || !is_word(bytes[i + word.len()])) =>
            {
                out.push_str(replacement);
                i += word.len();
            }
            b => {
                out.push(b as char);
                i += 1;
            }
        }
    }
    out
}

/// Substitutes all `{{ }}` spans in `sql` for the given model. Returns
/// the parseable SQL. Warnings are appended per unresolved construct.
pub(crate) fn substitute(
    env: &DbtEnv,
    model: &DbtModel,
    sql: &str,
    context: &str,
    diags: &mut Diagnostics,
) -> String {
    let mut current = strip_jinja_blocks(sql);
    for _depth in 0..MAX_EXPANSION_DEPTH {
        if !current.contains("{{") {
            return current;
        }
        let mut out = String::with_capacity(current.len());
        let mut cursor = 0usize;
        while let Some(rel) = current[cursor..].find("{{") {
            let start = cursor + rel;
            out.push_str(&current[cursor..start]);
            let Some(end) = find_expression_end(&current, start) else {
                // Unterminated expression: emit the rest verbatim.
                out.push_str(&current[start..]);
                cursor = current.len();
                break;
            };
            let body = current[start + 2..end - 2].trim();
            out.push_str(&substitute_one(env, model, body, context, diags));
            cursor = end;
        }
        out.push_str(&current[cursor..]);
        current = out;
    }
    if current.contains("{{") {
        diags.warning(
            None,
            format!(
                "{}: jinja expansion exceeded depth {}",
                context, MAX_EXPANSION_DEPTH
            ),
            "remaining expressions replaced with NULL",
        );
        current = current.replace("{{", "NULL /*").replace("}}", "*/");
    }
    current
}

fn substitute_one(
    env: &DbtEnv,
    model: &DbtModel,
    body: &str,
    context: &str,
    diags: &mut Diagnostics,
) -> String {
    if body == "this" {
        if let Some(fq) = &model.fq {
            return fq.sql_name();
        }
        return format!("`{}.{}.this`", UNRESOLVED, UNRESOLVED);
    }
    let Some((root, is_call)) = extract_root_identifier(body) else {
        diags.warning(
            None,
            format!(
                "{}: unrecognized jinja expression replaced with NULL",
                context
            ),
            body.to_string(),
        );
        return "NULL".to_string();
    };
    if !is_call {
        diags.warning(
            None,
            format!(
                "{}: non-call jinja expression '{}' replaced with NULL",
                context, root
            ),
            "static lineage cannot resolve runtime variables",
        );
        return "NULL".to_string();
    }
    let Some((name, args)) = parse_call(body) else {
        diags.warning(
            None,
            format!("{}: unparseable jinja call replaced with NULL", context),
            body.to_string(),
        );
        return "NULL".to_string();
    };
    match name {
        "config" => String::new(),
        "ref" => {
            let target = args.first().and_then(|a| unquote(a));
            match target.and_then(|t| env.resolve_ref(model, t)) {
                Some(fq) => fq.sql_name(),
                None => {
                    let raw = target.unwrap_or("unknown");
                    diags.warning(
                        None,
                        format!("{}: unresolved dbt ref('{}')", context, raw),
                        "no matching modelRefs entry or sibling model; edge omitted",
                    );
                    format!("`{}.{}.{}`", UNRESOLVED, UNRESOLVED, sanitize(raw))
                }
            }
        }
        "source" => {
            let source = args.first().and_then(|a| unquote(a));
            let table = args.get(1).and_then(|a| unquote(a));
            match (source, table) {
                (Some(s), Some(t)) => match env.resolve_source(model, s, t) {
                    Some(fq) => {
                        if let Some(project) = model
                            .project_logical
                            .as_ref()
                            .and_then(|l| env.projects.get(l))
                        {
                            if let Some((_, tables)) = project.sources.get(s) {
                                if !tables.iter().any(|x| x == t) {
                                    diags.warning(
                                        None,
                                        format!(
                                            "{}: source('{}','{}') table not declared",
                                            context, s, t
                                        ),
                                        "substituted anyway; check the dbt project sources",
                                    );
                                }
                            }
                        }
                        fq.sql_name()
                    }
                    None => {
                        diags.warning(
                            None,
                            format!("{}: unresolved dbt source('{}','{}')", context, s, t),
                            "no matching sources declaration; edge omitted",
                        );
                        format!("`{}.{}.{}`", UNRESOLVED, UNRESOLVED, sanitize(t))
                    }
                },
                _ => {
                    diags.warning(
                        None,
                        format!("{}: source() needs two string args", context),
                        body.to_string(),
                    );
                    "NULL".to_string()
                }
            }
        }
        other => {
            // Declared macro? Expand one level with arg binding.
            if let Some(macro_logical) = model.macros.get(other) {
                if let Some(mac) = env.macros.get(macro_logical) {
                    return expand_macro(mac, &args, context, diags);
                }
            }
            diags.warning(
                None,
                format!(
                    "{}: unknown jinja call '{}' replaced with NULL",
                    context, other
                ),
                "declare it under the model's `macros:` map to expand it",
            );
            "NULL".to_string()
        }
    }
}

fn expand_macro(mac: &DbtMacro, args: &[&str], context: &str, diags: &mut Diagnostics) -> String {
    let mut fragment = mac.sql.clone();
    let mut bound: Vec<(String, String)> = Vec::new();
    let mut positional = 0usize;
    for raw in args {
        if let Some(eq) = raw
            .find('=')
            .filter(|_| !raw.trim_start().starts_with(['\'', '"']))
        {
            let key = raw[..eq].trim().to_string();
            let value = raw[eq + 1..].trim();
            bound.push((key, unquote(value).unwrap_or(value).to_string()));
        } else {
            if let Some(param) = mac.args.get(positional) {
                bound.push((param.clone(), unquote(raw).unwrap_or(raw).to_string()));
            }
            positional += 1;
        }
    }
    if positional > mac.args.len() {
        diags.warning(
            None,
            format!("{}: macro arity mismatch; replaced with NULL", context),
            format!("expected {} args, got {}", mac.args.len(), args.len()),
        );
        return "NULL".to_string();
    }
    for (param, value) in &bound {
        fragment = replace_word(&fragment, param, value);
    }
    format!("({})", fragment.trim())
}

fn sanitize(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::parse::parse_template;
    use crate::ast::template::TemplateDecl;

    const DBT_YAML: &str = concat!(
        "name: p\nruntime: yaml\nresources:\n",
        "  proj:\n    type: gcpx:dbt:Project\n    properties:\n",
        "      gcpProject: data-proj\n      dataset: analytics\n",
        "      sources:\n        raw_src:\n          dataset: raw\n          tables:\n            - orders\n            - users\n",
        "  toDollars:\n    type: gcpx:dbt:Macro\n    properties:\n",
        "      sql: CAST(amount AS FLOAT64) / 100.0\n",
        "      args:\n        - amount\n",
        "  stgOrders:\n    type: gcpx:dbt:Model\n    properties:\n",
        "      name: stg_orders\n      context: ${proj.context}\n      sql: \"SELECT * FROM {{ source('raw_src', 'orders') }}\"\n",
        "  mart:\n    type: gcpx:dbt:Model\n    properties:\n",
        "      name: mart_revenue\n      context: ${proj.context}\n",
        "      modelRefs:\n        stg_orders: ${stgOrders.modelOutput}\n",
        "      macros:\n        cents_to_dollars: ${toDollars.macroOutput}\n",
        "      sql: \"SELECT user_id, {{ cents_to_dollars('amount_cents') }} AS revenue FROM {{ ref('stg_orders') }}\"\n",
    );

    fn build_env(template: &'static TemplateDecl<'static>) -> (DbtEnv, Diagnostics) {
        let vars = HashMap::new();
        let mut diags = Diagnostics::new();
        let env = DbtEnv::build_scope(&template.resources, &vars, &mut diags);
        (env, diags)
    }

    fn parsed() -> &'static TemplateDecl<'static> {
        let (t, d) = parse_template(DBT_YAML, None);
        assert!(!d.has_errors(), "{}", d);
        Box::leak(Box::new(t))
    }

    #[test]
    fn env_resolves_projects_models_macros() {
        let (env, diags) = build_env(parsed());
        assert!(!diags.has_warnings(), "{}", diags);
        let project = env.projects.get("proj").expect("project");
        assert_eq!(project.gcp_project.as_deref(), Some("data-proj"));
        assert_eq!(project.sources["raw_src"].0, "raw");
        let mart = env.models.get("mart").expect("mart");
        assert_eq!(
            mart.fq.as_ref().expect("fq").id(),
            "bq://data-proj/analytics/mart_revenue"
        );
        assert_eq!(mart.model_refs["stg_orders"], "stgOrders");
        assert!(env.macros.contains_key("toDollars"));
    }

    #[test]
    fn substitute_ref_source_macro() {
        let (env, _) = build_env(parsed());
        let mut diags = Diagnostics::new();
        let mart = env.models.get("mart").expect("mart");
        let out = substitute(
            &env,
            mart,
            "SELECT user_id, {{ cents_to_dollars('amount_cents') }} AS revenue FROM {{ ref('stg_orders') }}",
            "mart.sql",
            &mut diags,
        );
        assert_eq!(
            out,
            "SELECT user_id, (CAST(amount_cents AS FLOAT64) / 100.0) AS revenue FROM `data-proj.analytics.stg_orders`"
        );
        assert!(!diags.has_warnings(), "{}", diags);

        let stg = env.models.get("stgOrders").expect("stg");
        let out = substitute(
            &env,
            stg,
            "SELECT * FROM {{ source('raw_src', 'orders') }}",
            "stg.sql",
            &mut diags,
        );
        assert_eq!(out, "SELECT * FROM `data-proj.raw.orders`");
    }

    #[test]
    fn substitute_unresolved_and_unknown() {
        let (env, _) = build_env(parsed());
        let mut diags = Diagnostics::new();
        let mart = env.models.get("mart").expect("mart");
        let out = substitute(
            &env,
            mart,
            "SELECT * FROM {{ ref('nope') }}",
            "m",
            &mut diags,
        );
        assert_eq!(out, "SELECT * FROM `__unresolved__.__unresolved__.nope`");
        let out = substitute(&env, mart, "SELECT {{ mystery() }} FROM x", "m", &mut diags);
        assert_eq!(out, "SELECT NULL FROM x");
        let out = substitute(
            &env,
            mart,
            "{{ config(materialized='view') }}SELECT 1",
            "m",
            &mut diags,
        );
        assert_eq!(out, "SELECT 1");
        assert!(diags.has_warnings());
    }

    #[test]
    fn substitute_this_and_blocks() {
        let (env, _) = build_env(parsed());
        let mut diags = Diagnostics::new();
        let mart = env.models.get("mart").expect("mart");
        let out = substitute(
            &env,
            mart,
            "{% if x %}\nSELECT * FROM {{ this }}\n{% endif %}\n",
            "m",
            &mut diags,
        );
        assert_eq!(
            out.trim(),
            "SELECT * FROM `data-proj.analytics.mart_revenue`"
        );
    }

    #[test]
    fn macro_keyword_args_and_word_boundary() {
        let mac = DbtMacro {
            args: vec!["col".to_string()],
            sql: "SUM(col) / col_total".to_string(),
        };
        let mut diags = Diagnostics::new();
        let out = expand_macro(&mac, &["col='x'"], "m", &mut diags);
        // `col` replaced, `col_total` untouched (word boundary).
        assert_eq!(out, "(SUM(x) / col_total)");
    }
}

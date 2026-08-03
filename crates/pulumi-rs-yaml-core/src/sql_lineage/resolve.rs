// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

//! Static resolution: property lookup by dot-path, SQL text
//! materialization (inline literals and contained `fn::readFile`),
//! BigQuery entity identity, and table-schema normalization.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde::Serialize;

use crate::ast::expr::Expr;
use crate::ast::template::{ResourceEntry, ResourceProperties};
use crate::diag::Diagnostics;
use crate::jinja::resolve_contained_path;
use crate::literal_resolve::resolve_literal;

/// Where a SQL string came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SqlProvenance {
    Inline,
    File,
}

/// Fetches a property expression by dot-path (`view.query`), mirroring
/// the `literal_properties` path convention: object keys must be string
/// literals; list segments are numeric indices. Whole-`Expr` property
/// blocks resolve to `None`.
pub(crate) fn get_property_by_path<'src>(
    props: &'src ResourceProperties<'src>,
    path: &str,
) -> Option<&'src Expr<'src>> {
    let mut segments = path.split('.');
    let first = segments.next()?;
    let ResourceProperties::Map(entries) = props else {
        return None;
    };
    let mut current = &entries.iter().find(|p| p.key == first)?.value;
    for segment in segments {
        current = match current {
            Expr::Object(_, obj) => obj
                .iter()
                .find(|e| matches!(e.key.as_ref(), Expr::String(_, k) if k == segment))
                .map(|e| e.value.as_ref())?,
            Expr::List(_, items) => {
                let idx: usize = segment.parse().ok()?;
                items.get(idx)?
            }
            _ => return None,
        };
    }
    Some(current)
}

/// Materializes a SQL string: literal resolution first, then
/// `fn::readFile` with a **contained** read (the Jinja rule — reject
/// absolute paths, canonicalize both sides, `starts_with` check).
/// Jinja-form `{{ readFile(...) }}` content is already inlined by
/// render time and arrives here as a plain literal.
pub(crate) fn resolve_sql_text<'src>(
    expr: &'src Expr<'src>,
    variables: &HashMap<&'src str, &'src Expr<'src>>,
    project_dir: Option<&Path>,
    context: &str,
    diags: &mut Diagnostics,
) -> Option<(Cow<'src, str>, SqlProvenance)> {
    let mut memo = HashMap::new();
    let mut visiting = HashSet::new();
    if let Some(lit) = resolve_literal(expr, variables, &mut memo, &mut visiting) {
        return Some((lit, SqlProvenance::Inline));
    }
    if let Expr::ReadFile(_, inner) = expr {
        let path_lit = resolve_literal(inner, variables, &mut memo, &mut visiting)?;
        let Some(dir) = project_dir else {
            diags.warning(
                None,
                format!("{}: fn::readFile skipped (no project directory)", context),
                "pass a project directory to resolve referenced SQL files",
            );
            return None;
        };
        let dir_str = dir.to_string_lossy();
        match resolve_contained_path(dir_str.as_ref(), path_lit.as_ref()) {
            Ok(resolved) => match std::fs::read_to_string(&resolved) {
                Ok(content) => return Some((Cow::Owned(content), SqlProvenance::File)),
                Err(e) => diags.warning(
                    None,
                    format!("{}: failed to read '{}'", context, path_lit),
                    e.to_string(),
                ),
            },
            Err(msg) => diags.warning(None, format!("{}: fn::readFile rejected", context), msg),
        }
        return None;
    }
    diags.warning(
        None,
        format!("{}: SQL is not statically resolvable", context),
        "dynamic expressions (config, resource outputs, invokes) are skipped by static lineage",
    );
    None
}

/// Resolves a literal string property trying several key spellings.
pub(crate) fn literal_prop<'src>(
    entry: &'src ResourceEntry<'src>,
    variables: &HashMap<&'src str, &'src Expr<'src>>,
    keys: &[&str],
) -> Option<Cow<'src, str>> {
    let mut memo = HashMap::new();
    let mut visiting = HashSet::new();
    for key in keys {
        if let Some(expr) = get_property_by_path(&entry.resource.properties, key) {
            if let Some(lit) = resolve_literal(expr, variables, &mut memo, &mut visiting) {
                return Some(lit);
            }
        }
    }
    None
}

/// A normalized BigQuery column definition.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ColumnDef {
    pub name: String,
    pub data_type: Option<String>,
    pub mode: Option<String>,
    pub description: Option<String>,
    /// Previous name declared via `alter: rename` + `alterFrom`.
    pub renamed_from: Option<String>,
}

/// Parses the JSON-string form of a table schema:
/// `[{"name","type","mode"?,"description"?,"fields"?}]`; nested RECORD
/// fields become dotted names.
pub(crate) fn parse_schema_json(json: &str) -> Option<Vec<ColumnDef>> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let arr = value.as_array()?;
    let mut out = Vec::new();
    for field in arr {
        collect_json_field(field, None, &mut out);
    }
    Some(out)
}

fn collect_json_field(field: &serde_json::Value, prefix: Option<&str>, out: &mut Vec<ColumnDef>) {
    let Some(name) = field.get("name").and_then(|v| v.as_str()) else {
        return;
    };
    let full = match prefix {
        Some(p) => format!("{}.{}", p, name),
        None => name.to_string(),
    };
    out.push(ColumnDef {
        name: full.clone(),
        data_type: field.get("type").and_then(|v| v.as_str()).map(String::from),
        mode: field.get("mode").and_then(|v| v.as_str()).map(String::from),
        description: field
            .get("description")
            .and_then(|v| v.as_str())
            .map(String::from),
        renamed_from: None,
    });
    if let Some(fields) = field.get("fields").and_then(|v| v.as_array()) {
        for child in fields {
            collect_json_field(child, Some(&full), out);
        }
    }
}

/// Parses the native YAML-list schema form (`gcpx:bigquery:TableSchema`
/// `schema:` / `columns:` entries), including `alter: rename` +
/// `alterFrom` pairs.
pub(crate) fn parse_schema_yaml<'src>(
    expr: &'src Expr<'src>,
    variables: &HashMap<&'src str, &'src Expr<'src>>,
) -> Option<Vec<ColumnDef>> {
    let Expr::List(_, items) = expr else {
        return None;
    };
    let mut memo: HashMap<&str, Option<Cow<'_, str>>> = HashMap::new();
    let mut visiting: HashSet<&str> = HashSet::new();
    let mut out = Vec::new();
    for item in items {
        let Expr::Object(_, entries) = item else {
            continue;
        };
        let get = |key: &str| -> Option<Cow<'src, str>> {
            entries
                .iter()
                .find(|e| matches!(e.key.as_ref(), Expr::String(_, k) if k == key))
                .and_then(|e| {
                    resolve_literal(
                        &e.value,
                        variables,
                        &mut HashMap::new(),
                        &mut HashSet::new(),
                    )
                })
        };
        let Some(name) = get("name") else { continue };
        let alter = get("alter");
        let alter_from = get("alterFrom");
        let renamed_from = match (alter.as_deref(), alter_from) {
            (Some("rename"), Some(from)) => Some(from.into_owned()),
            _ => None,
        };
        out.push(ColumnDef {
            name: name.into_owned(),
            data_type: get("type").map(Cow::into_owned),
            mode: get("mode").map(Cow::into_owned),
            description: get("description").map(Cow::into_owned),
            renamed_from,
        });
    }
    let _ = (&mut memo, &mut visiting);
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::parse::parse_template;
    use crate::ast::template::TemplateDecl;

    fn parsed(yaml: &str) -> TemplateDecl<'static> {
        let (t, d) = parse_template(yaml, None);
        assert!(!d.has_errors(), "{}", d);
        t
    }

    #[test]
    fn path_getter_nested_and_lists() {
        let t = parsed(
            "name: p\nruntime: yaml\nresources:\n  r:\n    type: t:m:X\n    properties:\n      view:\n        query: SELECT 1\n      items:\n        - a\n        - b\n",
        );
        let props = &t.resources[0].resource.properties;
        assert!(matches!(
            get_property_by_path(props, "view.query"),
            Some(Expr::String(_, s)) if s == "SELECT 1"
        ));
        assert!(matches!(
            get_property_by_path(props, "items.1"),
            Some(Expr::String(_, s)) if s == "b"
        ));
        assert!(get_property_by_path(props, "view.missing").is_none());
        assert!(get_property_by_path(props, "nope").is_none());
    }

    #[test]
    fn sql_text_inline_literal() {
        let t = parsed(
            "name: p\nruntime: yaml\nresources:\n  r:\n    type: t:m:X\n    properties:\n      sql: SELECT 1\n",
        );
        let vars = HashMap::new();
        let mut diags = Diagnostics::new();
        let expr = get_property_by_path(&t.resources[0].resource.properties, "sql").expect("sql");
        let (text, prov) =
            resolve_sql_text(expr, &vars, None, "r.sql", &mut diags).expect("resolved");
        assert_eq!(text, "SELECT 1");
        assert_eq!(prov, SqlProvenance::Inline);
    }

    #[test]
    fn sql_text_readfile_contained() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("q.sql"), "SELECT 2").expect("write");
        let t = parsed(
            "name: p\nruntime: yaml\nresources:\n  r:\n    type: t:m:X\n    properties:\n      sql:\n        fn::readFile: q.sql\n",
        );
        let vars = HashMap::new();
        let mut diags = Diagnostics::new();
        let expr = get_property_by_path(&t.resources[0].resource.properties, "sql").expect("sql");
        let (text, prov) =
            resolve_sql_text(expr, &vars, Some(dir.path()), "r.sql", &mut diags).expect("resolved");
        assert_eq!(text, "SELECT 2");
        assert_eq!(prov, SqlProvenance::File);
    }

    #[test]
    fn sql_text_readfile_escape_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let t = parsed(
            "name: p\nruntime: yaml\nresources:\n  r:\n    type: t:m:X\n    properties:\n      sql:\n        fn::readFile: ../../etc/passwd\n",
        );
        let vars = HashMap::new();
        let mut diags = Diagnostics::new();
        let expr = get_property_by_path(&t.resources[0].resource.properties, "sql").expect("sql");
        assert!(resolve_sql_text(expr, &vars, Some(dir.path()), "r.sql", &mut diags).is_none());
        assert!(diags.has_warnings());
    }

    #[test]
    fn schema_json_nested_records() {
        let cols = parse_schema_json(
            r#"[{"name":"id","type":"STRING","description":"pk"},
                {"name":"meta","type":"RECORD","fields":[{"name":"src","type":"STRING"}]}]"#,
        )
        .expect("parsed");
        assert_eq!(cols.len(), 3);
        assert_eq!(cols[0].name, "id");
        assert_eq!(cols[0].description.as_deref(), Some("pk"));
        assert_eq!(cols[2].name, "meta.src");
    }

    #[test]
    fn schema_yaml_with_rename() {
        let t = parsed(
            "name: p\nruntime: yaml\nresources:\n  r:\n    type: t:m:X\n    properties:\n      columns:\n        - name: user_id\n          type: STRING\n        - name: event_kind\n          type: STRING\n          alter: rename\n          alterFrom: event_type\n",
        );
        let vars = HashMap::new();
        let expr =
            get_property_by_path(&t.resources[0].resource.properties, "columns").expect("cols");
        let cols = parse_schema_yaml(expr, &vars).expect("parsed");
        assert_eq!(cols.len(), 2);
        assert_eq!(cols[1].renamed_from.as_deref(), Some("event_type"));
    }
}

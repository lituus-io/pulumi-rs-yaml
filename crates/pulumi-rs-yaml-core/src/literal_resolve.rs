// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

//! Static literal resolution shared by graph exporters.
//!
//! Resolves expressions to scalar literal strings without evaluation:
//! plain literals, literal-only interpolations, and chains through
//! variables that collapse to literals. Anything dynamic (config,
//! resource outputs, builtins, and every invoke that is not a pure `str`
//! function — including `fn::readFile`, which callers must handle
//! themselves under their own containment rules) resolves to `None`,
//! never a guess.
//!
//! The one invoke this module answers is the `str` package, and only
//! because answering it is not evaluation: those functions are pure
//! string manipulation with no state, no I/O and no provider, they are
//! already evaluated in process by `eval::native_str` on the deploy path,
//! and every argument must itself resolve to a literal here first. So the
//! answer is the string the deploy would produce, produced by the same
//! code — a bucket named from `str:replace` gets its real name in the
//! exported graph instead of no name at all. A `str` invoke whose
//! argument is dynamic, whose result is a list (`split`), or which
//! selects no output stays `None` like any other invoke, and the "never a
//! guess" contract is unchanged: nothing here reads a file, calls a
//! provider, or invents a value.
//!
//! Moved verbatim from `resource_graph.rs` so sibling exporters (e.g.
//! SQL lineage) share one deterministic stringification.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use crate::ast::expr::{Expr, InvokeExpr};
use crate::ast::property::{PropertyAccess, PropertyAccessor};
use crate::ast::template::{ResourceEntry, ResourceProperties};
use crate::eval::native_str;
use crate::eval::value::Value;
use crate::packages::canonicalize_function_token;

/// Statically resolves an expression to a scalar literal string.
pub(crate) fn resolve_literal<'src>(
    expr: &'src Expr<'src>,
    variables: &HashMap<&'src str, &'src Expr<'src>>,
    memo: &mut HashMap<&'src str, Option<Cow<'src, str>>>,
    visiting: &mut HashSet<&'src str>,
) -> Option<Cow<'src, str>> {
    match expr {
        Expr::String(_, s) => Some(s.clone()),
        Expr::Bool(_, b) => Some(Cow::Borrowed(if *b { "true" } else { "false" })),
        Expr::Number(_, n) => format_literal_number(*n).map(Cow::Owned),
        Expr::Symbol(_, access) => resolve_access_literal(access, variables, memo, visiting),
        Expr::Interpolate(_, parts) => {
            let mut out = String::new();
            for part in parts {
                out.push_str(part.text.as_ref());
                if let Some(ref access) = part.value {
                    let lit = resolve_access_literal(access, variables, memo, visiting)?;
                    out.push_str(lit.as_ref());
                }
            }
            Some(Cow::Owned(out))
        }
        Expr::Invoke(_, invoke) => {
            // A bare `str` invoke evaluates to an OBJECT (`{result: ...}`),
            // which is not a scalar literal; only `return:` names a scalar.
            let field = invoke.return_.as_ref()?;
            let outputs = resolve_str_invoke(invoke, variables, memo, visiting)?;
            literal_from_value(outputs.get(field.as_ref())?)
        }
        _ => None,
    }
}

/// Resolves one `${...}` access: a bare variable, or `${var.output}` for a
/// variable holding a `str` invoke.
///
/// Exactly two accessors are accepted for the invoke form — `[var, output]`,
/// each a name or a string subscript. Anything longer reaches into a
/// structure this module does not model, and an integer subscript indexes a
/// list, which is never a scalar literal here; both stay `None`, as does
/// every access whose root is not a declared variable (a resource output, for
/// one, which is dynamic by definition).
fn resolve_access_literal<'src>(
    access: &'src PropertyAccess<'src>,
    variables: &HashMap<&'src str, &'src Expr<'src>>,
    memo: &mut HashMap<&'src str, Option<Cow<'src, str>>>,
    visiting: &mut HashSet<&'src str>,
) -> Option<Cow<'src, str>> {
    if let Some(root) = single_name_access(access) {
        return resolve_variable_literal(root, variables, memo, visiting);
    }
    if access.accessors.len() != 2 {
        return None;
    }
    let root = accessor_name(access.accessors.first()?)?;
    let field = accessor_name(access.accessors.get(1)?)?;
    resolve_invoke_variable_output(root, field, variables, memo, visiting)
}

/// The text of a name or string-subscript accessor; an integer index has none.
fn accessor_name<'src>(accessor: &'src PropertyAccessor<'src>) -> Option<&'src str> {
    match accessor {
        PropertyAccessor::Name(n) | PropertyAccessor::StringSubscript(n) => Some(n.as_ref()),
        PropertyAccessor::IntSubscript(_) => None,
    }
}

/// Resolves `${name.field}` where `name` is a variable holding a `str` invoke.
///
/// The invoke output is deliberately NOT memoised: the memo is keyed by
/// variable name and holds one scalar per name, while an invoke has a whole
/// output object and may be read under several fields. Recomputing it is a
/// pure string operation over already-memoised arguments — linear in the
/// argument length, with no I/O to repeat.
fn resolve_invoke_variable_output<'src>(
    name: &'src str,
    field: &str,
    variables: &HashMap<&'src str, &'src Expr<'src>>,
    memo: &mut HashMap<&'src str, Option<Cow<'src, str>>>,
    visiting: &mut HashSet<&'src str>,
) -> Option<Cow<'src, str>> {
    let Some(Expr::Invoke(_, invoke)) = variables.get(name).copied() else {
        return None;
    };
    // Guard the cycle a.result -> b.result -> a.result, which the per-variable
    // guard in `resolve_variable_literal` never sees: this path does not go
    // through it.
    if !visiting.insert(name) {
        return None;
    }
    let outputs = resolve_str_invoke(invoke, variables, memo, visiting);
    visiting.remove(name);
    literal_from_value(outputs?.get(field)?)
}

/// Evaluates a pure `str` invoke whose every argument is already a literal.
///
/// Returns `None` — deferring exactly as the rest of this module does — for a
/// token `native_str` does not handle, arguments that are not a literal-keyed
/// object, or any argument that does not itself resolve. The token is
/// canonicalized the way the evaluator canonicalizes it, so the shorthand
/// (`str:replace`), the registered form (`str:index:replace`) and the slashed
/// schema form all reach the same function.
fn resolve_str_invoke<'src>(
    invoke: &'src InvokeExpr<'src>,
    variables: &HashMap<&'src str, &'src Expr<'src>>,
    memo: &mut HashMap<&'src str, Option<Cow<'src, str>>>,
    visiting: &mut HashSet<&'src str>,
) -> Option<HashMap<String, Value<'static>>> {
    let token = canonicalize_function_token(invoke.token.as_ref());
    if !native_str::handles(&token) {
        return None;
    }
    let Expr::Object(_, entries) = invoke.call_args.as_deref()? else {
        return None;
    };
    let mut args: HashMap<String, Value<'static>> = HashMap::with_capacity(entries.len());
    for entry in entries {
        let Expr::String(_, key) = entry.key.as_ref() else {
            return None;
        };
        let lit = resolve_literal(&entry.value, variables, memo, visiting)?;
        args.insert(
            key.as_ref().to_string(),
            Value::String(Cow::Owned(lit.into_owned())),
        );
    }
    native_str::try_invoke(&token, &args)
}

/// Renders one invoke output as a literal.
///
/// Strings pass through; a bool renders `true`/`false`, matching the
/// `Expr::Bool` arm, so `str:regexp:match`'s `matches` is a literal. A list
/// (`str:regexp:split`) or a null is not a scalar and stays `None`.
fn literal_from_value<'src>(value: &Value<'_>) -> Option<Cow<'src, str>> {
    match value {
        Value::String(s) => Some(Cow::Owned(s.as_ref().to_string())),
        Value::Bool(b) => Some(Cow::Borrowed(if *b { "true" } else { "false" })),
        _ => None,
    }
}

pub(crate) fn single_name_access<'src>(access: &'src PropertyAccess<'src>) -> Option<&'src str> {
    if access.accessors.len() != 1 {
        return None;
    }
    match access.accessors.first() {
        Some(PropertyAccessor::Name(n)) => Some(n.as_ref()),
        _ => None,
    }
}

pub(crate) fn resolve_variable_literal<'src>(
    name: &'src str,
    variables: &HashMap<&'src str, &'src Expr<'src>>,
    memo: &mut HashMap<&'src str, Option<Cow<'src, str>>>,
    visiting: &mut HashSet<&'src str>,
) -> Option<Cow<'src, str>> {
    if let Some(cached) = memo.get(name) {
        return cached.clone();
    }
    if !visiting.insert(name) {
        return None;
    }
    let result = variables
        .get(name)
        .copied()
        .and_then(|expr| resolve_literal(expr, variables, memo, visiting));
    visiting.remove(name);
    memo.insert(name, result.clone());
    result
}

/// Deterministic number rendering shared by every export site: integral
/// finite values print without a fraction; non-finite values are not
/// literals.
pub(crate) fn format_literal_number(n: f64) -> Option<String> {
    if !n.is_finite() {
        return None;
    }
    if n.fract() == 0.0 && n.abs() < 9_007_199_254_740_992.0 {
        Some(format!("{}", n as i64))
    } else {
        Some(format!("{}", n))
    }
}

/// Collects `literal_properties` for one resource.
pub(crate) fn collect_literal_properties<'src>(
    entry: &'src ResourceEntry<'src>,
    variables: &HashMap<&'src str, &'src Expr<'src>>,
) -> Vec<(Cow<'src, str>, Cow<'src, str>)> {
    let mut out = Vec::new();
    let mut memo = HashMap::new();
    let mut visiting = HashSet::new();
    if let ResourceProperties::Map(props) = &entry.resource.properties {
        for prop in props {
            collect_literals_rec(
                Cow::Borrowed(prop.key.as_ref()),
                &prop.value,
                variables,
                &mut memo,
                &mut visiting,
                &mut out,
            );
        }
    }
    out.sort();
    out.dedup_by(|a, b| a.0 == b.0);
    out
}

pub(crate) fn collect_literals_rec<'src>(
    path: Cow<'src, str>,
    expr: &'src Expr<'src>,
    variables: &HashMap<&'src str, &'src Expr<'src>>,
    memo: &mut HashMap<&'src str, Option<Cow<'src, str>>>,
    visiting: &mut HashSet<&'src str>,
    out: &mut Vec<(Cow<'src, str>, Cow<'src, str>)>,
) {
    match expr {
        Expr::Object(_, entries) => {
            for entry in entries {
                if let Expr::String(_, key) = entry.key.as_ref() {
                    collect_literals_rec(
                        Cow::Owned(format!("{}.{}", path, key)),
                        &entry.value,
                        variables,
                        memo,
                        visiting,
                        out,
                    );
                }
            }
        }
        Expr::List(_, elements) => {
            for (i, elem) in elements.iter().enumerate() {
                collect_literals_rec(
                    Cow::Owned(format!("{}.{}", path, i)),
                    elem,
                    variables,
                    memo,
                    visiting,
                    out,
                );
            }
        }
        Expr::Null(_) => {}
        _ => {
            if let Some(lit) = resolve_literal(expr, variables, memo, visiting) {
                out.push((path, lit));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::parse::parse_template;
    use crate::ast::template::TemplateDecl;

    /// Parses a template and returns the literal properties of its single
    /// resource, so every case below is written as the YAML an author writes.
    fn literals(yaml: &str) -> HashMap<String, String> {
        let (template, diags) = parse_template(yaml, None);
        assert!(!diags.has_errors(), "parse failed: {}", diags);
        let template: &'static TemplateDecl<'static> = Box::leak(Box::new(template));
        let variables: HashMap<&str, &Expr<'_>> = template
            .variables
            .iter()
            .map(|v| (v.key.as_ref(), &v.value))
            .collect();
        let entry = template
            .resources
            .first()
            .expect("template declares a resource");
        collect_literal_properties(entry, &variables)
            .into_iter()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect()
    }

    /// The `name` property of the single resource, or `None` when it did not
    /// resolve to a literal.
    fn name_of(yaml: &str) -> Option<String> {
        literals(yaml).remove("name")
    }

    /// One resource whose `name` is `expr`, on top of `variables`.
    fn template(variables: &str, expr: &str) -> String {
        format!(
            "name: proj\nruntime: yaml\nvariables:\n{}resources:\n  r:\n    type: gcp:storage:Bucket\n    properties:\n      name: {}\n",
            variables, expr
        )
    }

    // ---------------------------------------------------------------
    // The `str` invoke arms
    // ---------------------------------------------------------------

    #[test]
    fn replace_through_a_variable() {
        let yaml = template(
            "  process: geo_fence\n  sanitized:\n    fn::str:replace:\n      string: ${process}\n      old: '_'\n      new: '-'\n",
            "${sanitized.result}",
        );
        assert_eq!(name_of(&yaml).as_deref(), Some("geo-fence"));
    }

    #[test]
    fn replace_inside_an_interpolation() {
        let yaml = template(
            "  process: geo_fence\n  sanitized:\n    fn::str:replace:\n      string: ${process}\n      old: '_'\n      new: '-'\n",
            "acme-bkt-${sanitized.result}-eu",
        );
        assert_eq!(name_of(&yaml).as_deref(), Some("acme-bkt-geo-fence-eu"));
    }

    /// Both spellings the parser lower-cases to the same expression, driven
    /// end to end so the equivalence is proven rather than assumed.
    #[test]
    fn both_fn_prefix_spellings_resolve_identically() {
        for prefix in ["Fn", "fn"] {
            let yaml = template(
                &format!(
                    "  process: geo_fence\n  sanitized:\n    {}::str:replace:\n      string: ${{process}}\n      old: '_'\n      new: '-'\n",
                    prefix
                ),
                "${sanitized.result}",
            );
            assert_eq!(
                name_of(&yaml).as_deref(),
                Some("geo-fence"),
                "spelling {}::",
                prefix
            );
        }
    }

    /// A string-subscript access is the same access with different syntax.
    #[test]
    fn string_subscript_selects_the_same_output() {
        let yaml = template(
            "  sanitized:\n    fn::str:replace:\n      string: geo_fence\n      old: '_'\n      new: '-'\n",
            "${sanitized[\"result\"]}",
        );
        assert_eq!(name_of(&yaml).as_deref(), Some("geo-fence"));
    }

    #[test]
    fn chained_invokes_resolve() {
        let yaml = template(
            "  process: geo_fence\n  sanitized:\n    fn::str:replace:\n      string: ${process}\n      old: '_'\n      new: '-'\n  trimmed:\n    fn::str:trimSuffix:\n      string: ${sanitized.result}\n      suffix: '-fence'\n",
            "${trimmed.result}",
        );
        assert_eq!(name_of(&yaml).as_deref(), Some("geo"));
    }

    #[test]
    fn trim_prefix_resolves() {
        let yaml = template(
            "  trimmed:\n    fn::str:trimPrefix:\n      string: raw-orders\n      prefix: 'raw-'\n",
            "${trimmed.result}",
        );
        assert_eq!(name_of(&yaml).as_deref(), Some("orders"));
    }

    /// The long form is the only spelling that carries `return:`, which is
    /// what turns the invoke's output object into a scalar.
    #[test]
    fn long_form_with_return_resolves() {
        let yaml = template(
            "  sanitized:\n    fn::invoke:\n      function: str:replace\n      arguments:\n        string: geo_fence\n        old: '_'\n        new: '-'\n      return: result\n",
            "${sanitized}",
        );
        assert_eq!(name_of(&yaml).as_deref(), Some("geo-fence"));
    }

    /// Go's replacement template expands `$1`, and so does this path.
    #[test]
    fn regexp_replace_expands_a_capture_group() {
        let yaml = template(
            "  renamed:\n    fn::str:regexp:replace:\n      string: geo_fence\n      old: '^(geo)_(.*)$'\n      new: 'x$1-$2'\n",
            "${renamed.result}",
        );
        assert_eq!(name_of(&yaml).as_deref(), Some("xgeo-fence"));
    }

    /// `str:regexp:match` answers `matches`, a bool, rendered like any other
    /// literal bool in the graph.
    #[test]
    fn regexp_match_renders_its_bool() {
        let yaml = template(
            "  hit:\n    fn::invoke:\n      function: str:regexp:match\n      arguments:\n        string: geo_fence\n        pattern: '^geo'\n      return: matches\n",
            "${hit}",
        );
        assert_eq!(name_of(&yaml).as_deref(), Some("true"));

        let miss = template(
            "  hit:\n    fn::invoke:\n      function: str:regexp:match\n      arguments:\n        string: geo_fence\n        pattern: '^zzz'\n      return: matches\n",
            "${hit}",
        );
        assert_eq!(name_of(&miss).as_deref(), Some("false"));
    }

    /// `split` answers a list. A list is not a scalar literal, with or
    /// without `return:`.
    #[test]
    fn split_is_not_a_literal() {
        let yaml = template(
            "  parts:\n    fn::invoke:\n      function: str:regexp:split\n      arguments:\n        string: a,b,c\n        on: ','\n      return: result\n",
            "${parts}",
        );
        assert_eq!(name_of(&yaml), None);
    }

    /// A bare invoke evaluates to an object; only `return:` names a scalar.
    #[test]
    fn bare_invoke_without_return_is_not_a_literal() {
        let yaml = template(
            "  sanitized:\n    fn::str:replace:\n      string: geo_fence\n      old: '_'\n      new: '-'\n",
            "${sanitized}",
        );
        assert_eq!(name_of(&yaml), None);
    }

    #[test]
    fn an_output_the_function_does_not_produce_is_not_a_literal() {
        let yaml = template(
            "  sanitized:\n    fn::str:replace:\n      string: geo_fence\n      old: '_'\n      new: '-'\n",
            "${sanitized.matches}",
        );
        assert_eq!(name_of(&yaml), None);
    }

    // ---------------------------------------------------------------
    // What stays unresolved
    // ---------------------------------------------------------------

    /// Two invokes each taking the other's output: the visiting set stops it
    /// and the resolution simply fails.
    #[test]
    fn a_cycle_through_an_argument_terminates_unresolved() {
        let yaml = template(
            "  a:\n    fn::str:replace:\n      string: ${b.result}\n      old: x\n      new: y\n  b:\n    fn::str:replace:\n      string: ${a.result}\n      old: x\n      new: y\n",
            "${a.result}",
        );
        assert_eq!(name_of(&yaml), None);
    }

    /// A 200-long cycle closes on itself the same way a two-long one does,
    /// and in the same bounded number of steps. The graph exporter rejects a
    /// cyclic template before it gets here, so this is the resolver's own
    /// guarantee rather than the DAG check's.
    #[test]
    fn a_long_cycle_terminates_unresolved() {
        let mut variables = String::new();
        for i in 0..200 {
            variables.push_str(&format!(
                "  v{}:\n    fn::str:replace:\n      string: ${{{{v{}.result}}}}\n      old: x\n      new: y\n",
                i,
                (i + 1) % 200
            ));
        }
        let yaml = template(&variables, "${v0.result}");
        assert_eq!(name_of(&yaml), None);
    }

    /// A config value is dynamic, so an invoke reading one is unanswerable.
    #[test]
    fn an_unresolved_argument_leaves_the_invoke_unresolved() {
        let yaml = "name: proj\nruntime: yaml\nconfig:\n  region:\n    type: string\nvariables:\n  sanitized:\n    fn::str:replace:\n      string: ${region}\n      old: '_'\n      new: '-'\nresources:\n  r:\n    type: gcp:storage:Bucket\n    properties:\n      name: ${sanitized.result}\n";
        assert_eq!(name_of(yaml), None);
    }

    /// Every other provider's invoke is a call this module will not make.
    #[test]
    fn a_non_str_token_is_never_evaluated() {
        let yaml = template(
            "  net:\n    fn::invoke:\n      function: gcp:compute:getNetwork\n      arguments:\n        name: default\n      return: name\n",
            "${net}",
        );
        assert_eq!(name_of(&yaml), None);
    }

    /// `fn::readFile` is the caller's business under its own containment
    /// rules; nothing here opens a file.
    #[test]
    fn read_file_is_never_evaluated() {
        let yaml = template("  secret:\n    fn::readFile: /etc/passwd\n", "${secret}");
        assert_eq!(name_of(&yaml), None);
    }

    /// An argument that is itself an object is not a literal, so the invoke
    /// is not attempted with a stringified stand-in.
    #[test]
    fn a_nested_object_argument_is_not_stringified() {
        let yaml = template(
            "  sanitized:\n    fn::str:replace:\n      string:\n        nested: value\n      old: '_'\n      new: '-'\n",
            "${sanitized.result}",
        );
        assert_eq!(name_of(&yaml), None);
    }

    /// Three accessors reach into a structure this module does not model.
    #[test]
    fn a_deeper_access_stays_unresolved() {
        let yaml = template(
            "  sanitized:\n    fn::str:replace:\n      string: geo_fence\n      old: '_'\n      new: '-'\n",
            "${sanitized.result.inner}",
        );
        assert_eq!(name_of(&yaml), None);
    }

    /// A resource output shares the `${a.b}` shape with an invoke read and
    /// must not be confused with one.
    #[test]
    fn a_resource_output_stays_unresolved() {
        let yaml = "name: proj\nruntime: yaml\nresources:\n  src:\n    type: gcp:storage:Bucket\n    properties:\n      name: src-bucket\n  r:\n    type: gcp:storage:Bucket\n    properties:\n      name: ${src.url}\n";
        let (template, diags) = parse_template(yaml, None);
        assert!(!diags.has_errors(), "parse failed: {}", diags);
        let template: &'static TemplateDecl<'static> = Box::leak(Box::new(template));
        let variables: HashMap<&str, &Expr<'_>> = HashMap::new();
        let entry = &template.resources[1];
        let props: HashMap<String, String> = collect_literal_properties(entry, &variables)
            .into_iter()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        assert_eq!(props.get("name"), None);
    }

    // ---------------------------------------------------------------
    // Preservation: the pre-existing surface answers exactly as before
    // ---------------------------------------------------------------

    /// Every literal shape the resolver answered before the `str` arms
    /// existed, pinned to a hard-coded map so a future change to the invoke
    /// path cannot quietly move one of them.
    #[test]
    fn plain_literals_resolve_exactly_as_before() {
        let yaml = "name: proj\nruntime: yaml\nvariables:\n  env: prod\n  region: ${env}-eu\nresources:\n  r:\n    type: gcp:storage:Bucket\n    properties:\n      name: acme-${region}\n      location: US\n      versioning: true\n      retention: 30\n      labels:\n        team: data\n      cors:\n        - origin: https://example.test\n      dynamic: ${undeclared}\n";
        let expected: HashMap<String, String> = [
            ("name", "acme-prod-eu"),
            ("location", "US"),
            ("versioning", "true"),
            ("retention", "30"),
            ("labels.team", "data"),
            ("cors.0.origin", "https://example.test"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        assert_eq!(literals(yaml), expected);
    }
}

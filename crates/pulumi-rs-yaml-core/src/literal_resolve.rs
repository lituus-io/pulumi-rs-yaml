// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

//! Static literal resolution shared by graph exporters.
//!
//! Resolves expressions to scalar literal strings without evaluation:
//! plain literals, literal-only interpolations, and chains through
//! variables that collapse to literals. Anything dynamic (config,
//! resource outputs, invokes, builtins — including `fn::readFile`,
//! which callers must handle themselves under their own containment
//! rules) resolves to `None`, never a guess.
//!
//! Moved verbatim from `resource_graph.rs` so sibling exporters (e.g.
//! SQL lineage) share one deterministic stringification.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use crate::ast::expr::Expr;
use crate::ast::property::{PropertyAccess, PropertyAccessor};
use crate::ast::template::{ResourceEntry, ResourceProperties};

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
        Expr::Symbol(_, access) => {
            let root = single_name_access(access)?;
            resolve_variable_literal(root, variables, memo, visiting)
        }
        Expr::Interpolate(_, parts) => {
            let mut out = String::new();
            for part in parts {
                out.push_str(part.text.as_ref());
                if let Some(ref access) = part.value {
                    let root = single_name_access(access)?;
                    let lit = resolve_variable_literal(root, variables, memo, visiting)?;
                    out.push_str(lit.as_ref());
                }
            }
            Some(Cow::Owned(out))
        }
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

//! Zero-cost expression visitor. GAT controls accumulator type.
//!
//! Three visitors unify ~300 lines of duplicated recursive walkers
//! into a single `walk_expr` function + zero-sized-type visitors.

use crate::ast::expr::{Expr, InvokeExpr};
use crate::ast::template::{ResourceDecl, ResourceProperties};

/// Expression visitor trait. Each impl is a zero-sized type that
/// monomorphizes to the same assembly as a hand-written match.
pub trait ExprVisitor {
    /// Accumulator type — GAT so each visitor controls its own.
    type Acc<'a>;

    /// Called for each `${symbol}` reference.
    fn visit_symbol<'a>(&self, root: &'a str, acc: &mut Self::Acc<'a>);

    /// Called for each `${ref}` inside an interpolated string.
    fn visit_interpolation_ref<'a>(&self, root: &'a str, acc: &mut Self::Acc<'a>);

    /// Called for each `fn::invoke` expression. Default: no-op.
    fn visit_invoke<'a>(&self, _invoke: &'a InvokeExpr<'a>, _acc: &mut Self::Acc<'a>) {}
}

/// Walk an expression tree, calling visitor methods at each leaf node.
pub fn walk_expr<'a, V: ExprVisitor>(expr: &'a Expr<'a>, visitor: &V, acc: &mut V::Acc<'a>) {
    match expr {
        Expr::Symbol(_, access) => {
            if let Ok(root) = access.root_name() {
                visitor.visit_symbol(root, acc);
            }
        }
        Expr::Interpolate(_, parts) => {
            for part in parts {
                if let Some(ref access) = part.value {
                    if let Ok(root) = access.root_name() {
                        visitor.visit_interpolation_ref(root, acc);
                    }
                }
            }
        }
        Expr::Invoke(_, invoke) => {
            visitor.visit_invoke(invoke, acc);
            if let Some(ref args) = invoke.call_args {
                walk_expr(args, visitor, acc);
            }
            if let Some(ref parent) = invoke.call_opts.parent {
                walk_expr(parent, visitor, acc);
            }
            if let Some(ref provider) = invoke.call_opts.provider {
                walk_expr(provider, visitor, acc);
            }
            if let Some(ref depends_on) = invoke.call_opts.depends_on {
                walk_expr(depends_on, visitor, acc);
            }
        }
        Expr::List(_, elements) => {
            for elem in elements {
                walk_expr(elem, visitor, acc);
            }
        }
        Expr::Object(_, entries) => {
            for entry in entries {
                walk_expr(&entry.key, visitor, acc);
                walk_expr(&entry.value, visitor, acc);
            }
        }
        Expr::Join(_, a, b) | Expr::Select(_, a, b) | Expr::Split(_, a, b) => {
            walk_expr(a, visitor, acc);
            walk_expr(b, visitor, acc);
        }
        Expr::ToJson(_, inner)
        | Expr::ToBase64(_, inner)
        | Expr::FromBase64(_, inner)
        | Expr::Secret(_, inner)
        | Expr::ReadFile(_, inner)
        | Expr::Abs(_, inner)
        | Expr::Floor(_, inner)
        | Expr::Ceil(_, inner)
        | Expr::Max(_, inner)
        | Expr::Min(_, inner)
        | Expr::StringLen(_, inner)
        | Expr::TimeUtc(_, inner)
        | Expr::TimeUnix(_, inner)
        | Expr::Uuid(_, inner)
        | Expr::RandomString(_, inner)
        | Expr::DateFormat(_, inner)
        | Expr::StringAsset(_, inner)
        | Expr::FileAsset(_, inner)
        | Expr::RemoteAsset(_, inner)
        | Expr::FileArchive(_, inner)
        | Expr::RemoteArchive(_, inner) => {
            walk_expr(inner, visitor, acc);
        }
        Expr::Substring(_, a, b, c) => {
            walk_expr(a, visitor, acc);
            walk_expr(b, visitor, acc);
            walk_expr(c, visitor, acc);
        }
        Expr::AssetArchive(_, entries) => {
            for (_, v) in entries {
                walk_expr(v, visitor, acc);
            }
        }
        // Terminals
        Expr::Null(_) | Expr::Bool(_, _) | Expr::Number(_, _) | Expr::String(_, _) => {}
    }
}

/// Walk all expressions in a resource declaration.
pub fn walk_resource<'a, V: ExprVisitor>(
    resource: &'a ResourceDecl<'a>,
    visitor: &V,
    acc: &mut V::Acc<'a>,
) {
    match &resource.properties {
        ResourceProperties::Map(props) => {
            for prop in props {
                walk_expr(&prop.value, visitor, acc);
            }
        }
        ResourceProperties::Expr(expr) => {
            walk_expr(expr, visitor, acc);
        }
    }

    let opts = &resource.options;
    if let Some(ref expr) = opts.depends_on {
        walk_expr(expr, visitor, acc);
    }
    if let Some(ref expr) = opts.parent {
        walk_expr(expr, visitor, acc);
    }
    if let Some(ref expr) = opts.provider {
        walk_expr(expr, visitor, acc);
    }
    if let Some(ref expr) = opts.providers {
        walk_expr(expr, visitor, acc);
    }
    if let Some(ref expr) = opts.protect {
        walk_expr(expr, visitor, acc);
    }
    if let Some(ref expr) = opts.aliases {
        walk_expr(expr, visitor, acc);
    }
    if let Some(ref expr) = opts.replace_with {
        walk_expr(expr, visitor, acc);
    }
    if let Some(ref expr) = opts.deleted_with {
        walk_expr(expr, visitor, acc);
    }
    if let Some(ref get) = resource.get {
        walk_expr(&get.id, visitor, acc);
        for prop in &get.state {
            walk_expr(&prop.value, visitor, acc);
        }
    }
}

// ---------- Concrete visitors ----------

use std::collections::{HashMap, HashSet};

/// Collects ALL `${ref}` root names (for validation — no filtering).
pub struct AllRefsCollector;

impl ExprVisitor for AllRefsCollector {
    type Acc<'a> = HashSet<&'a str>;

    fn visit_symbol<'a>(&self, root: &'a str, acc: &mut Self::Acc<'a>) {
        acc.insert(root);
    }

    fn visit_interpolation_ref<'a>(&self, root: &'a str, acc: &mut Self::Acc<'a>) {
        acc.insert(root);
    }
}

/// Collects dependency names filtered by known names (for topological sort).
pub struct DepCollector<'n> {
    pub known_names: &'n HashMap<&'n str, &'n str>,
}

impl ExprVisitor for DepCollector<'_> {
    type Acc<'a> = HashSet<&'a str>;

    fn visit_symbol<'a>(&self, root: &'a str, acc: &mut Self::Acc<'a>) {
        if self.known_names.contains_key(root) {
            acc.insert(root);
        }
    }

    fn visit_interpolation_ref<'a>(&self, root: &'a str, acc: &mut Self::Acc<'a>) {
        if self.known_names.contains_key(root) {
            acc.insert(root);
        }
    }
}

/// Collects invoke type tokens for package dependency scanning.
pub struct InvokePackageCollector;

impl ExprVisitor for InvokePackageCollector {
    type Acc<'a> = Vec<InvokeInfo<'a>>;

    fn visit_symbol<'a>(&self, _root: &'a str, _acc: &mut Self::Acc<'a>) {}
    fn visit_interpolation_ref<'a>(&self, _root: &'a str, _acc: &mut Self::Acc<'a>) {}

    fn visit_invoke<'a>(&self, invoke: &'a InvokeExpr<'a>, acc: &mut Self::Acc<'a>) {
        acc.push(InvokeInfo {
            token: invoke.token.as_ref(),
            version: invoke.call_opts.version.as_deref(),
            plugin_download_url: invoke.call_opts.plugin_download_url.as_deref(),
        });
    }
}

/// Info about an invoke expression collected by InvokePackageCollector.
pub struct InvokeInfo<'a> {
    pub token: &'a str,
    pub version: Option<&'a str>,
    pub plugin_download_url: Option<&'a str>,
}

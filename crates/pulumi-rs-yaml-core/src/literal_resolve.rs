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
//!
//! # Host-supplied config
//!
//! A config value is dynamic to a program read on its own, so by default it
//! never resolves. A host that knows the stack's PLAINTEXT config -- the
//! values a `Pulumi.<stack>.yaml` holds, with every `secure:` value already
//! dropped -- may pass them as a [`ConfigLookup`]. A name that is not a
//! variable then falls back to that config exactly the way the deploy looks
//! it up: the declared key, by its own name or with the project namespace
//! stripped (`${k}`, `${proj:k}`, `${gcp:k}`); its raw value under
//! `{project}:{key}`, then `{key}`; then the declaration's `value:`, then its
//! `default:`; parsed by the declared type with the deploy's own parser. A
//! key declared `secret: true` never resolves, and neither does a non-scalar
//! type. Without a lookup nothing here reads config and the output is what it
//! always was.

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::ops::Deref;

use crate::ast::expr::{Expr, InvokeExpr};
use crate::ast::property::{PropertyAccess, PropertyAccessor};
use crate::ast::template::{ConfigEntry, ResourceEntry, ResourceProperties, VariableEntry};
use crate::config_types::ConfigType;
use crate::eval::native_str;
use crate::eval::value::Value;
use crate::packages::canonicalize_function_token;

/// Host-supplied plaintext stack config, keyed the way `Pulumi.<stack>.yaml`
/// keys it (`project:key`, `gcp:project`, or a bare `key`). Never a secure
/// value: a host drops those before the call.
pub type HostConfig = HashMap<String, String>;

/// The config one program can resolve statically: its `config:`
/// declarations plus the host's plaintext values. Built once per export.
pub struct ConfigLookup<'src> {
    /// The Pulumi project name -- the namespace config keys are written in.
    namespace: &'src str,
    host: &'src HostConfig,
    declared: HashMap<&'src str, &'src ConfigEntry<'src>>,
}

impl<'src> ConfigLookup<'src> {
    /// `namespace` is the program's Pulumi project name (the `name:` of
    /// `Pulumi.yaml`), which is what config keys are namespaced by even when
    /// a host exports the stack under another identity.
    pub fn new(
        namespace: &'src str,
        declared: &'src [ConfigEntry<'src>],
        host: &'src HostConfig,
    ) -> Self {
        Self {
            namespace,
            host,
            declared: declared.iter().map(|c| (c.key.as_ref(), c)).collect(),
        }
    }

    /// The literal the deploy would bind to the config name `root`, or `None`
    /// for an undeclared, secret, non-scalar, unparsable or absent value.
    pub fn get(&self, root: &str) -> Option<Cow<'src, str>> {
        let stripped = crate::eval::config::strip_config_namespace(self.namespace, root);
        let entry = self
            .declared
            .get(root)
            .or_else(|| self.declared.get(stripped))
            .copied()?;
        if entry.param.secret == Some(true) {
            return None;
        }
        let key = entry.key.as_ref();
        let declared_type = match entry.param.type_.as_deref() {
            Some(t) => ConfigType::parse(t)?,
            None => ConfigType::String,
        };
        if !matches!(
            declared_type,
            ConfigType::String | ConfigType::Int | ConfigType::Number | ConfigType::Boolean
        ) {
            return None;
        }
        let raw = self
            .host
            .get(&format!("{}:{}", self.namespace, key))
            .or_else(|| self.host.get(key));
        let Some(raw) = raw else {
            // The project file's own `value:`, then the declared default.
            let expr = entry
                .param
                .value
                .as_ref()
                .or(entry.param.default.as_ref())?;
            return match expr {
                Expr::String(_, s) => Some(Cow::Owned(s.to_string())),
                Expr::Bool(_, b) => Some(Cow::Borrowed(if *b { "true" } else { "false" })),
                Expr::Number(_, n) => format_literal_number(*n).map(Cow::Owned),
                _ => None,
            };
        };
        let mut scratch = crate::diag::Diagnostics::new();
        match crate::eval::config::parse_config_value(raw, declared_type, &mut scratch)? {
            Value::Number(n) => format_literal_number(n).map(Cow::Owned),
            other => literal_from_value(&other),
        }
    }
}

/// The variables one scope resolves through, and the config a name that is
/// not a variable falls back to (top-level scopes only: inside a component
/// body such a name is a component input, never stack config).
///
/// Dereferences to the variable map, so a caller that walks variables for
/// edges reads it as before.
pub(crate) struct Vars<'src, 'c> {
    map: HashMap<&'src str, &'src Expr<'src>>,
    config: Option<&'c ConfigLookup<'src>>,
    /// Bumped on every resolution that read config, so a caller can tell a
    /// config-sourced literal from a plain one.
    config_reads: Cell<usize>,
    /// Variables whose value was built from config: a memo hit on one of
    /// them reads config too. Touched only when a lookup is present.
    from_config: RefCell<HashSet<&'src str>>,
}

impl<'src, 'c> Vars<'src, 'c> {
    pub(crate) fn new(map: HashMap<&'src str, &'src Expr<'src>>) -> Self {
        Self {
            map,
            config: None,
            config_reads: Cell::new(0),
            from_config: RefCell::new(HashSet::new()),
        }
    }

    pub(crate) fn from_entries(entries: &'src [VariableEntry<'src>]) -> Self {
        Self::new(entries.iter().map(|v| (v.key.as_ref(), &v.value)).collect())
    }

    /// No variables and no config.
    #[cfg(any(test, feature = "sql-lineage"))]
    pub(crate) fn empty() -> Self {
        Self::new(HashMap::new())
    }

    pub(crate) fn with_config(mut self, config: Option<&'c ConfigLookup<'src>>) -> Self {
        self.config = config;
        self
    }

    /// How many resolutions have read config so far.
    pub(crate) fn config_reads(&self) -> usize {
        self.config_reads.get()
    }

    pub(crate) fn has_config(&self) -> bool {
        self.config.is_some()
    }

    fn bump(&self) {
        self.config_reads.set(self.config_reads.get() + 1);
    }

    /// The config value for a name no variable answers.
    fn config_value(&self, name: &str) -> Option<Cow<'src, str>> {
        let value = self.config?.get(name)?;
        self.bump();
        Some(value)
    }
}

impl<'src> Deref for Vars<'src, '_> {
    type Target = HashMap<&'src str, &'src Expr<'src>>;
    fn deref(&self) -> &Self::Target {
        &self.map
    }
}

/// Statically resolves an expression to a scalar literal string.
/// Resolves a `fn::deriveString` whose arguments are written inline.
///
/// The one door onto this module for callers outside the crate, and
/// deliberately a narrow one. The general [`resolve_literal`] reaches
/// `resolve_str_invoke`, which reaches `native_str`, which links `regex` --
/// about a megabyte. The converter is the only outside caller and only ever
/// needs this one builtin with literal arguments, so exposing the general
/// resolver cost it **+1,075,648 bytes, a 62% increase** on a 1.7 MB binary
/// for a convenience. Measured, then narrowed to this.
///
/// Nothing is lost by the narrowing: the general entry it replaces passed an
/// empty variable map, so a reference never resolved through it either.
/// Anything other than inline literals returns `None`, and the caller emits
/// what it would have emitted before.
pub fn derive_string_literal(expr: &Expr<'_>) -> Option<String> {
    let Expr::DeriveString(_, inner) = expr else {
        return None;
    };
    let mut from: Option<&str> = None;
    let mut length = crate::eval::builtins::DERIVE_DEFAULT_LEN;
    let mut alphabet: Option<&str> = None;
    match inner.as_ref() {
        // The shorthand: the seed alone.
        Expr::String(_, seed) => from = Some(seed.as_ref()),
        Expr::Object(_, entries) => {
            for entry in entries {
                let Expr::String(_, key) = entry.key.as_ref() else {
                    return None;
                };
                match (key.as_ref(), entry.value.as_ref()) {
                    ("from", Expr::String(_, v)) => from = Some(v.as_ref()),
                    ("alphabet", Expr::String(_, v)) => alphabet = Some(v.as_ref()),
                    ("length", Expr::Number(_, n)) => {
                        if !n.is_finite() || *n < 0.0 || n.fract() != 0.0 {
                            return None;
                        }
                        length = *n as usize;
                    }
                    // A non-literal argument, or one the evaluator refuses.
                    _ => return None,
                }
            }
        }
        _ => return None,
    }
    crate::eval::builtins::derive(
        from?,
        length,
        alphabet.unwrap_or(crate::eval::builtins::DERIVE_ALPHABET),
    )
    .ok()
}

pub(crate) fn resolve_literal<'src>(
    expr: &'src Expr<'src>,
    variables: &Vars<'src, '_>,
    memo: &mut HashMap<&'src str, Option<Cow<'src, str>>>,
    visiting: &mut HashSet<&'src str>,
) -> Option<Cow<'src, str>> {
    match expr {
        // Borrowed, never cloned: the parser stores every literal as an owned
        // `Cow`, so `s.clone()` copied the whole string -- a view's SQL, a
        // schema -- once per resolution and again per memo hit. `expr` lives
        // for `'src`, so a borrow of its text does too.
        Expr::String(_, s) => Some(Cow::Borrowed(s.as_ref())),
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
        // The one builtin this resolver answers, because it is the one whose
        // value is a pure function of its argument. That is the whole reason
        // `fn::deriveString` exists: an import, a graph export and a
        // shift-left policy check all need the name before anything is
        // deployed. The match below ends in `_ => None`, so losing this arm
        // would be silent -- a test pins it for exactly that reason.
        Expr::DeriveString(_, inner) => resolve_derive_literal(inner, variables, memo, visiting),
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

/// Resolves `fn::deriveString` without evaluating the program.
///
/// Reads the same three arguments the evaluator reads and hands them to the one
/// shared implementation, so a statically resolved name and a deployed name
/// cannot differ. Any argument that is not itself a literal -- a seed built
/// from a resource output, a length behind a config value -- resolves to
/// `None`, never a guess, per this module's contract.
fn resolve_derive_literal<'src>(
    inner: &'src Expr<'src>,
    variables: &Vars<'src, '_>,
    memo: &mut HashMap<&'src str, Option<Cow<'src, str>>>,
    visiting: &mut HashSet<&'src str>,
) -> Option<Cow<'src, str>> {
    let (from, length, alphabet) = match inner {
        Expr::Object(_, entries) => {
            let mut from: Option<Cow<'src, str>> = None;
            let mut length = crate::eval::builtins::DERIVE_DEFAULT_LEN;
            let mut alphabet: Option<Cow<'src, str>> = None;
            for entry in entries {
                let Expr::String(_, key) = entry.key.as_ref() else {
                    return None;
                };
                match key.as_ref() {
                    "from" => {
                        from = Some(resolve_literal(&entry.value, variables, memo, visiting)?)
                    }
                    "length" => {
                        let Expr::Number(_, n) = entry.value.as_ref() else {
                            return None;
                        };
                        if !n.is_finite() || *n < 0.0 || n.fract() != 0.0 {
                            return None;
                        }
                        length = *n as usize;
                    }
                    "alphabet" => {
                        alphabet = Some(resolve_literal(&entry.value, variables, memo, visiting)?)
                    }
                    // Unknown keys are refused by the evaluator, so resolving
                    // one here would answer where the deploy will not.
                    _ => return None,
                }
            }
            (from?, length, alphabet)
        }
        // Shorthand: the seed alone.
        other => (
            resolve_literal(other, variables, memo, visiting)?,
            crate::eval::builtins::DERIVE_DEFAULT_LEN,
            None,
        ),
    };
    let alphabet = alphabet
        .as_deref()
        .unwrap_or(crate::eval::builtins::DERIVE_ALPHABET);
    crate::eval::builtins::derive(from.as_ref(), length, alphabet)
        .ok()
        .map(Cow::Owned)
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
    variables: &Vars<'src, '_>,
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
    variables: &Vars<'src, '_>,
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
    variables: &Vars<'src, '_>,
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
    variables: &Vars<'src, '_>,
    memo: &mut HashMap<&'src str, Option<Cow<'src, str>>>,
    visiting: &mut HashSet<&'src str>,
) -> Option<Cow<'src, str>> {
    if let Some(cached) = memo.get(name) {
        if variables.has_config() && variables.from_config.borrow().contains(name) {
            variables.bump();
        }
        return cached.clone();
    }
    if !visiting.insert(name) {
        // A variable that shadows a config key reads the CONFIG value through
        // its own name (Go parity, as the DAG check accepts it). Without host
        // config this is the cycle it always was.
        return variables.config_value(name);
    }
    let before = variables.config_reads();
    let result = match variables.get(name).copied() {
        Some(expr) => resolve_literal(expr, variables, memo, visiting),
        // The variable-miss fallback: a config name, when the host gave config.
        None => variables.config_value(name),
    };
    visiting.remove(name);
    if variables.config_reads() != before {
        variables.from_config.borrow_mut().insert(name);
    }
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

/// Collects `literal_properties` for one resource, and the paths among them
/// whose literal was read from host config (sorted; empty without a config
/// lookup).
pub(crate) fn collect_literal_properties_sourced<'src>(
    entry: &'src ResourceEntry<'src>,
    variables: &Vars<'src, '_>,
) -> LiteralProperties<'src> {
    let mut out = Vec::new();
    let mut from_config = Vec::new();
    let mut memo = HashMap::new();
    let mut visiting = HashSet::new();
    if let ResourceProperties::Map(props) = &entry.resource.properties {
        let mut sink = LiteralSink {
            out: &mut out,
            from_config: &mut from_config,
        };
        for prop in props {
            collect_literals_rec(
                Cow::Borrowed(prop.key.as_ref()),
                &prop.value,
                variables,
                &mut memo,
                &mut visiting,
                &mut sink,
            );
        }
    }
    out.sort();
    out.dedup_by(|a, b| a.0 == b.0);
    from_config.sort();
    from_config.dedup();
    (out, from_config)
}

/// Literal properties as sorted `(path, value)` pairs, and the sorted paths
/// among them whose value came from host config.
pub(crate) type LiteralProperties<'src> =
    (Vec<(Cow<'src, str>, Cow<'src, str>)>, Vec<Cow<'src, str>>);

/// Where [`collect_literals_rec`] puts what it finds.
pub(crate) struct LiteralSink<'a, 'src> {
    pub out: &'a mut Vec<(Cow<'src, str>, Cow<'src, str>)>,
    pub from_config: &'a mut Vec<Cow<'src, str>>,
}

pub(crate) fn collect_literals_rec<'src>(
    path: Cow<'src, str>,
    expr: &'src Expr<'src>,
    variables: &Vars<'src, '_>,
    memo: &mut HashMap<&'src str, Option<Cow<'src, str>>>,
    visiting: &mut HashSet<&'src str>,
    sink: &mut LiteralSink<'_, 'src>,
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
                        sink,
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
                    sink,
                );
            }
        }
        Expr::Null(_) => {}
        _ => {
            let before = variables.config_reads();
            if let Some(lit) = resolve_literal(expr, variables, memo, visiting) {
                if variables.config_reads() != before {
                    sink.from_config.push(path.clone());
                }
                sink.out.push((path, lit));
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
        let variables = Vars::from_entries(&template.variables);
        let entry = template
            .resources
            .first()
            .expect("template declares a resource");
        collect_literal_properties_sourced(entry, &variables)
            .0
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

    #[test]
    fn a_string_literal_is_borrowed_from_the_template() {
        let yaml = template("  unused: x\n", "a-literal-bucket-name");
        let (tpl, diags) = parse_template(&yaml, None);
        assert!(!diags.has_errors(), "{diags}");
        let tpl: &'static TemplateDecl<'static> = Box::leak(Box::new(tpl));
        let entry = tpl.resources.first().expect("resource");
        let crate::ast::template::ResourceProperties::Map(props) = &entry.resource.properties
        else {
            panic!("map properties");
        };
        let prop = props.iter().find(|p| p.key == "name").expect("name");
        let Expr::String(_, source) = &prop.value else {
            panic!("string literal");
        };
        let mut memo = HashMap::new();
        let mut visiting = HashSet::new();
        let got = resolve_literal(&prop.value, &Vars::empty(), &mut memo, &mut visiting)
            .expect("literal");
        assert!(
            matches!(got, Cow::Borrowed(_)),
            "a literal must not be copied"
        );
        assert!(
            std::ptr::eq(got.as_ptr(), source.as_ptr()),
            "borrowed from the AST"
        );
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

    /// Without host config a config value is dynamic, so an invoke reading
    /// one is unanswerable. (With the host's plaintext config it resolves:
    /// see `config_reaches_an_invoke_argument`.)
    #[test]
    fn an_unresolved_argument_leaves_the_invoke_unresolved() {
        let yaml = "name: proj\nruntime: yaml\nconfig:\n  region:\n    type: string\nvariables:\n  sanitized:\n    fn::str:replace:\n      string: ${region}\n      old: '_'\n      new: '-'\nresources:\n  r:\n    type: gcp:storage:Bucket\n    properties:\n      name: ${sanitized.result}\n";
        assert_eq!(name_of(yaml), None);
    }

    // ---------------------------------------------------------------
    // Host-supplied config
    // ---------------------------------------------------------------

    /// The literal properties of the single resource with host config, and
    /// the paths read from it.
    fn literals_with(yaml: &str, host: &[(&str, &str)]) -> (HashMap<String, String>, Vec<String>) {
        let (template, diags) = parse_template(yaml, None);
        assert!(!diags.has_errors(), "parse failed: {}", diags);
        let template: &'static TemplateDecl<'static> = Box::leak(Box::new(template));
        let host: &'static HostConfig = Box::leak(Box::new(
            host.iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        ));
        let lookup: &'static ConfigLookup<'static> = Box::leak(Box::new(ConfigLookup::new(
            template.name.as_deref().unwrap_or("proj"),
            &template.config,
            host,
        )));
        let variables = Vars::from_entries(&template.variables).with_config(Some(lookup));
        let entry = template.resources.first().expect("a resource");
        let (props, from_config) = collect_literal_properties_sourced(entry, &variables);
        (
            props
                .into_iter()
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect(),
            from_config.into_iter().map(Cow::into_owned).collect(),
        )
    }

    const CONFIG_PROGRAM: &str = "name: proj\nruntime: yaml\nconfig:\n  env:\n    type: string\n  gcp:project:\n    type: string\n  count:\n    type: integer\n  flag:\n    type: boolean\n  token:\n    type: string\n    secret: true\n  tags:\n    type: List<String>\n  region:\n    default: us-central1\n  zone:\n    value: us-east1-b\nvariables:\n  name: analytics_${env}\nresources:\n  r:\n    type: gcp:bigquery:Dataset\n    properties:\n      datasetId: ${name}\n      again: ${name}\n      project: ${gcp:project}\n      count: ${count}\n      flag: ${flag}\n      token: ${token}\n      tags: ${tags}\n      region: ${region}\n      zone: ${zone}\n      plain: literal\n";

    #[test]
    fn config_resolves_the_way_the_deploy_reads_it() {
        let (props, from_config) = literals_with(
            CONFIG_PROGRAM,
            &[
                ("proj:env", "prod"),
                ("gcp:project", "data-proj"),
                ("proj:count", "030"),
                ("flag", "true"),
                ("proj:token", "s3cr3t"),
                ("proj:tags", "[\"a\"]"),
            ],
        );
        assert_eq!(
            props.get("datasetId").map(String::as_str),
            Some("analytics_prod")
        );
        // Read twice through one memoized variable: both are marked.
        assert_eq!(
            props.get("again").map(String::as_str),
            Some("analytics_prod")
        );
        assert_eq!(props.get("project").map(String::as_str), Some("data-proj"));
        // Parsed by the declared type, as the deploy parses it.
        assert_eq!(props.get("count").map(String::as_str), Some("30"));
        assert_eq!(props.get("flag").map(String::as_str), Some("true"));
        // The declaration's own value and default stand in for the host's.
        assert_eq!(props.get("region").map(String::as_str), Some("us-central1"));
        assert_eq!(props.get("zone").map(String::as_str), Some("us-east1-b"));
        assert_eq!(props.get("token"), None, "a secret never resolves");
        assert_eq!(props.get("tags"), None, "a list is not a scalar");
        assert_eq!(props.get("plain").map(String::as_str), Some("literal"));
        assert_eq!(
            from_config,
            vec![
                "again",
                "count",
                "datasetId",
                "flag",
                "project",
                "region",
                "zone"
            ]
        );
    }

    #[test]
    fn without_host_config_nothing_reads_config() {
        let props = literals(CONFIG_PROGRAM);
        for key in [
            "datasetId",
            "project",
            "count",
            "flag",
            "region",
            "zone",
            "token",
        ] {
            assert_eq!(props.get(key), None, "{key}");
        }
        assert_eq!(props.get("plain").map(String::as_str), Some("literal"));
    }

    #[test]
    fn an_unparsable_value_does_not_resolve() {
        let (props, _) = literals_with(
            CONFIG_PROGRAM,
            &[("proj:count", "many"), ("proj:flag", "yes")],
        );
        assert_eq!(props.get("count"), None);
        assert_eq!(props.get("flag"), None);
    }

    #[test]
    fn config_reaches_an_invoke_argument() {
        let yaml = "name: proj\nruntime: yaml\nconfig:\n  region:\n    type: string\nvariables:\n  sanitized:\n    fn::str:replace:\n      string: ${region}\n      old: '_'\n      new: '-'\nresources:\n  r:\n    type: gcp:storage:Bucket\n    properties:\n      name: ${sanitized.result}\n";
        let (props, from_config) = literals_with(yaml, &[("region", "us_east")]);
        assert_eq!(props.get("name").map(String::as_str), Some("us-east"));
        assert_eq!(from_config, vec!["name"]);
    }

    /// `variables: project: ${project}` reads the config value through its
    /// own name, as the deploy does.
    #[test]
    fn a_variable_shadowing_config_reads_the_config() {
        let yaml = "name: proj\nruntime: yaml\nconfig:\n  project:\n    type: string\nvariables:\n  project: ${project}\nresources:\n  r:\n    type: gcp:storage:Bucket\n    properties:\n      name: ${project}-bucket\n";
        let (props, _) = literals_with(yaml, &[("proj:project", "p1")]);
        assert_eq!(props.get("name").map(String::as_str), Some("p1-bucket"));
        assert_eq!(literals(yaml).get("name"), None);
    }

    /// A value for an undeclared key, or one under another project's
    /// namespace, is not the program's config.
    #[test]
    fn only_declared_keys_in_this_namespace_resolve() {
        let yaml = "name: proj\nruntime: yaml\nconfig:\n  env:\n    type: string\nresources:\n  r:\n    type: gcp:storage:Bucket\n    properties:\n      name: ${env}\n      other: ${undeclared}\n";
        let (props, _) = literals_with(yaml, &[("elsewhere:env", "x"), ("undeclared", "y")]);
        assert_eq!(props.get("name"), None);
        assert_eq!(props.get("other"), None);
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
        let variables = Vars::empty();
        let entry = &template.resources[1];
        let props: HashMap<String, String> = collect_literal_properties_sourced(entry, &variables)
            .0
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

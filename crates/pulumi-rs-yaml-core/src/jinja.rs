// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

//! Jinja2 template pre-processing with GAT-based architecture.
//!
//! This module provides a `TemplatePreprocessor` trait with two implementations:
//! - `NoopPreprocessor`: zero-cost passthrough (returns `&'src str`)
//! - `JinjaPreprocessor`: renders Jinja2 syntax via `minijinja`, returning
//!   `Cow::Borrowed` when no Jinja syntax is detected (zero-copy fast path)

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// GAT-based trait (B.1)
// ---------------------------------------------------------------------------

/// GAT-based trait for template preprocessors.
/// The associated types carry the source lifetime, enabling zero-copy passthrough.
pub trait TemplatePreprocessor {
    /// The output type. For NoopPreprocessor: `&'src str`. For Jinja: `Cow<'src, str>`.
    type Output<'src>: AsRef<str>
    where
        Self: 'src;
    /// The error type.
    type Err<'src>: fmt::Display
    where
        Self: 'src;

    fn preprocess<'src>(
        &self,
        source: &'src str,
        filename: &str,
    ) -> Result<Self::Output<'src>, Self::Err<'src>>;
}

/// True zero-cost passthrough. Returns a reference to the input (no allocation).
pub struct NoopPreprocessor;

impl TemplatePreprocessor for NoopPreprocessor {
    type Output<'src> = &'src str;
    type Err<'src> = std::convert::Infallible;

    fn preprocess<'src>(
        &self,
        source: &'src str,
        _filename: &str,
    ) -> Result<&'src str, std::convert::Infallible> {
        Ok(source)
    }
}

// ---------------------------------------------------------------------------
// Rich Error Types (B.2)
// ---------------------------------------------------------------------------

/// Classification of pre-processing errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderErrorKind {
    JinjaSyntax,
    JinjaUndefinedVariable,
    JinjaFilterError,
    JinjaTypeError,
    /// An `{% include %}` or `{% import %}` the loader could not or would not
    /// serve: the file is absent from both roots, or it was refused — by
    /// extension, as an absolute path, or because it resolves outside the
    /// sandbox. The message says which.
    JinjaTemplateNotFound,
    YamlSyntax,
    YamlIndentation,
    YamlDuplicateKey,
    /// A provider-templated block scalar whose extent could not be determined
    /// with certainty. Refusing is deliberate: silently altering a model's SQL
    /// is worse than declining the file.
    ProviderScope,
}

/// Rich diagnostic from template pre-processing.
///
/// `source_line` and `expression` are zero-copy slices of the original
/// source. `column` and `end_column` are 1-based and bound the failing
/// expression on that line — `expression` is exactly
/// `source_line[column - 1..end_column - 1]` — or both are `0` and
/// `expression` is empty when the engine could not say where on the line
/// the fault lies.
pub struct RenderDiagnostic<'src> {
    pub kind: RenderErrorKind,
    pub line: u32,
    pub column: u32,
    pub end_column: u32,
    pub source_line: &'src str,
    pub expression: &'src str,
    pub message: String,
    pub suggestion: Option<&'static str>,
}

impl fmt::Display for RenderDiagnostic<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl fmt::Debug for RenderDiagnostic<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RenderDiagnostic")
            .field("kind", &self.kind)
            .field("line", &self.line)
            .field("column", &self.column)
            .field("end_column", &self.end_column)
            .field("expression", &self.expression)
            .field("message", &self.message)
            .field("suggestion", &self.suggestion)
            .finish()
    }
}

impl RenderDiagnostic<'_> {
    /// Formats as a rich error message with context for stderr output.
    ///
    /// `file:line:column: error: message`, then the source line under a
    /// gutter, then — when the column is known — a caret row under the
    /// failing expression, then the suggestion. The caret is measured in
    /// characters of the source line, not bytes, so it lands under the
    /// token whatever precedes it.
    pub fn format_rich(&self, filename: &str) -> String {
        let mut out = format!(
            "{}:{}:{}: error: {}",
            filename, self.line, self.column, self.message
        );
        if !self.source_line.is_empty() {
            let gutter = format!("  {} | ", self.line);
            out.push('\n');
            out.push_str(&gutter);
            out.push_str(self.source_line);
            if self.column > 0 {
                let byte_col = (self.column - 1) as usize;
                let pad = self
                    .source_line
                    .get(..byte_col)
                    .map_or(byte_col, |prefix| prefix.chars().count());
                let width = self.expression.chars().count().max(1);
                out.push('\n');
                out.push_str(&" ".repeat(gutter.len() - 2));
                out.push_str("| ");
                out.push_str(&" ".repeat(pad));
                out.push_str(&"^".repeat(width));
            }
        }
        if let Some(suggestion) = self.suggestion {
            out.push_str(&format!("\n  suggestion: {}", suggestion));
        }
        out
    }
}

/// Why the template loader declined a name. Built on the failure path only,
/// carried to the render error as the detail of a `BadInclude` — the one
/// kind the VM returns verbatim, where a `TemplateNotFound` from a loader is
/// folded into its own generic message and the reason would be lost.
///
/// A file that is simply absent is not a refusal: the loader answers
/// `Ok(None)` for that, so `{% include "x" ignore missing %}` keeps its
/// meaning, and the VM's `TemplateNotFound` names the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IncludeRefusal {
    /// Absolute paths are never served, wherever they point.
    AbsolutePath { name: String },
    /// A candidate exists but canonicalizes outside both roots.
    EscapesSandbox { name: String },
    /// The file is inside the tree but is not UTF-8 text.
    NotText { name: String },
    /// The file is inside the tree but larger than [`MAX_INCLUDE_BYTES`].
    TooLarge { name: String },
}

/// The most an include inlines: the same bound the SQL lineage reader puts
/// on one statement, because both are "the text a stack keeps beside
/// itself". Decided from the file's size before it is read, so an oversized
/// file costs one `stat` and no allocation.
pub const MAX_INCLUDE_BYTES: u64 = 1024 * 1024;

impl IncludeRefusal {
    /// The stable tag inside the detail text, which is what a classifier
    /// reads back — one spelling here, one reader there.
    pub const PREFIX: &'static str = "include refused";
    pub const TAG_ABSOLUTE: &'static str = "[absolute]";
    pub const TAG_ESCAPE: &'static str = "[escape]";
    pub const TAG_BINARY: &'static str = "[binary]";
    pub const TAG_TOO_LARGE: &'static str = "[too large]";

    fn tag(&self) -> &'static str {
        match self {
            Self::AbsolutePath { .. } => Self::TAG_ABSOLUTE,
            Self::EscapesSandbox { .. } => Self::TAG_ESCAPE,
            Self::NotText { .. } => Self::TAG_BINARY,
            Self::TooLarge { .. } => Self::TAG_TOO_LARGE,
        }
    }

    /// The remedy for each refusal, static because there is one per cause.
    pub fn suggestion_for(detail: &str) -> Option<&'static str> {
        if !detail.starts_with(Self::PREFIX) {
            return None;
        }
        if detail.contains(Self::TAG_ABSOLUTE) {
            Some("use a path relative to the stack directory or the render root")
        } else if detail.contains(Self::TAG_ESCAPE) {
            Some(
                "the file resolves outside both the stack directory and the \
                 render root, so it is not served; move it inside the tree \
                 the render root contains",
            )
        } else if detail.contains(Self::TAG_BINARY) {
            Some(
                "an include inlines text, and this file is not UTF-8; keep the \
                 value as text, or read the file at deploy time with fn::readFile",
            )
        } else if detail.contains(Self::TAG_TOO_LARGE) {
            Some(
                "the file is over 1 MiB, the most an include inlines; keep large \
                 data beside the stack and read it at deploy time with \
                 fn::readFile, or split it",
            )
        } else {
            None
        }
    }
}

impl fmt::Display for IncludeRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::AbsolutePath { name }
            | Self::EscapesSandbox { name }
            | Self::NotText { name }
            | Self::TooLarge { name } => name,
        };
        write!(f, "{} {}: {:?}", Self::PREFIX, self.tag(), name)
    }
}

// ---------------------------------------------------------------------------
// Heuristic Suggestion Engine (B.3)
// ---------------------------------------------------------------------------

/// Classifies a minijinja error and returns a (kind, suggestion) pair.
fn classify_jinja_error(err: &minijinja::Error) -> (RenderErrorKind, Option<&'static str>) {
    let detail = err.detail().unwrap_or("");
    if detail.contains("readFile:") {
        return (
            RenderErrorKind::JinjaFilterError,
            Some("Check the file path. Relative paths are resolved from the project directory."),
        );
    }
    match err.kind() {
        minijinja::ErrorKind::UndefinedError => (
            RenderErrorKind::JinjaUndefinedVariable,
            Some("Check variable name. Available context: config.*, pulumi_*, env.*"),
        ),
        minijinja::ErrorKind::TemplateNotFound => (
            RenderErrorKind::JinjaTemplateNotFound,
            Some(
                "the include was looked for relative to the stack directory and \
                 the render root; check the path, and that the file is in the tree",
            ),
        ),
        minijinja::ErrorKind::BadInclude if detail.starts_with(IncludeRefusal::PREFIX) => (
            RenderErrorKind::JinjaTemplateNotFound,
            IncludeRefusal::suggestion_for(detail),
        ),
        minijinja::ErrorKind::SyntaxError => (
            RenderErrorKind::JinjaSyntax,
            Some("Check Jinja syntax: {{ var }}, {% block %}, {# comment #}"),
        ),
        minijinja::ErrorKind::InvalidOperation => (
            RenderErrorKind::JinjaFilterError,
            Some("Check filter arguments and types"),
        ),
        _ => (RenderErrorKind::JinjaSyntax, None),
    }
}

/// Classifies a serde_yaml error on rendered output and suggests fixes.
fn classify_yaml_error(msg: &str, line_content: &str) -> (RenderErrorKind, Option<&'static str>) {
    if msg.contains("mapping values are not allowed") {
        (
            RenderErrorKind::YamlSyntax,
            Some("Add a space after ':' — YAML requires 'key: value' not 'key:value'"),
        )
    } else if msg.contains("block sequence entries are not allowed") {
        (
            RenderErrorKind::YamlIndentation,
            Some("Check indentation — list items may need more or fewer spaces"),
        )
    } else if msg.contains("found duplicate key") {
        (
            RenderErrorKind::YamlDuplicateKey,
            Some("A Jinja loop may have generated duplicate resource names — use {{ loop.index }}"),
        )
    } else if line_content.contains("{{") && line_content.contains("}}") {
        (
            RenderErrorKind::YamlSyntax,
            Some("Jinja output may need quoting — try wrapping in quotes: \"{{ value }}\""),
        )
    } else {
        (RenderErrorKind::YamlSyntax, None)
    }
}

// ---------------------------------------------------------------------------
// Zero-Copy Jinja Context (B.4)
// ---------------------------------------------------------------------------

/// Controls how unknown Jinja variables are handled.
///
/// - `Strict` (default): all `{{ expr }}` must resolve or error.
/// - `Passthrough`: expressions whose root identifier is NOT a known Pulumi
///   context variable are wrapped in `{% raw %}` before rendering, allowing
///   dbt-style `{{ ref('model') }}`, `{{ config(materialized='view') }}`, etc.
///   Known roots (`config`, `env`, `pulumi_*`, `readFile`) are still evaluated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UndefinedMode {
    #[default]
    Strict,
    Passthrough,
}

/// Jinja rendering context. Borrows ALL data — no cloning, no Arc.
pub struct JinjaContext<'cfg> {
    pub project_name: &'cfg str,
    pub stack_name: &'cfg str,
    pub cwd: &'cfg str,
    pub organization: &'cfg str,
    pub root_directory: &'cfg str,
    pub config: &'cfg HashMap<String, String>,
    pub project_dir: &'cfg str,
    pub undefined: UndefinedMode,
    /// Packages whose block scalars this runtime must not render, from
    /// `runtime.options.providerTemplatedPackages`.
    ///
    /// A dbt model's SQL is addressed to the provider, not to us, and it uses
    /// the same delimiters we do. Listing the package scopes it out by where it
    /// sits rather than by what it says. Empty — the default — leaves rendering
    /// exactly as it has always been.
    pub provider_templated_packages: &'cfg [&'cfg str],
    /// Extra context variables passed through from the caller.
    /// Inserted into the Jinja context BEFORE built-in keys, so built-ins
    /// always win on collision (prevents override attacks).
    pub extra: &'cfg HashMap<String, String>,
}

/// Builds a minijinja context Value from borrowed references.
/// This is the ONLY allocation boundary — minijinja requires owned values internally.
///
/// Extras are inserted FIRST, then built-in keys overwrite them.
/// This prevents extra vars from overriding built-in Pulumi context.
fn build_minijinja_context(ctx: &JinjaContext<'_>) -> minijinja::Value {
    let mut map = std::collections::BTreeMap::<String, minijinja::Value>::new();

    // Insert extras first (built-ins will overwrite on collision)
    for (k, v) in ctx.extra {
        map.insert(k.clone(), minijinja::Value::from(v.as_str()));
    }

    // Built-in keys always win
    map.insert(
        "pulumi_project".into(),
        minijinja::Value::from(ctx.project_name),
    );
    map.insert(
        "pulumi_stack".into(),
        minijinja::Value::from(ctx.stack_name),
    );
    map.insert("pulumi_cwd".into(), minijinja::Value::from(ctx.cwd));
    map.insert(
        "pulumi_organization".into(),
        minijinja::Value::from(ctx.organization),
    );
    map.insert(
        "pulumi_root_directory".into(),
        minijinja::Value::from(ctx.root_directory),
    );
    map.insert("config".into(), build_config_value(ctx.config));
    map.insert("env".into(), build_env_value());

    minijinja::Value::from_serialize(&map)
}

fn build_config_value(config: &HashMap<String, String>) -> minijinja::Value {
    let map: std::collections::BTreeMap<String, minijinja::Value> = config
        .iter()
        .map(|(k, v)| {
            // Strip project namespace prefix (e.g., "project:key" → "key")
            let key = if let Some(pos) = k.find(':') {
                &k[pos + 1..]
            } else {
                k.as_str()
            };
            (key.to_string(), minijinja::Value::from(v.as_str()))
        })
        .collect();
    minijinja::Value::from_serialize(&map)
}

fn build_env_value() -> minijinja::Value {
    let env_vars: std::collections::BTreeMap<String, String> = std::env::vars()
        .filter(|(k, _)| k.starts_with("JINJA_VAR_"))
        .map(|(k, v)| (k.strip_prefix("JINJA_VAR_").unwrap_or("").to_lowercase(), v))
        .collect();
    minijinja::Value::from_serialize(&env_vars)
}

// ---------------------------------------------------------------------------
// JinjaPreprocessor Implementation (B.5)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Passthrough Mode — Pre-Escape Scanner (A.2)
// ---------------------------------------------------------------------------

/// Known root identifiers that exist in the Jinja context.
const KNOWN_ROOTS: &[&str] = &[
    "config",
    "env",
    "pulumi_project",
    "pulumi_stack",
    "pulumi_cwd",
    "pulumi_organization",
    "pulumi_root_directory",
];

/// Roots that are dict-like objects (attribute access should be evaluated).
const DICT_ROOTS: &[&str] = &["config", "env"];

/// Known Pulumi functions (always evaluated).
const KNOWN_FUNCTIONS: &[&str] = &["readFile"];

/// Whether an expression should be evaluated by Jinja or passed through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExprClassification {
    Evaluate,
    Passthrough,
}

/// Extracts the root identifier from a Jinja expression body.
///
/// Returns `(identifier, is_function_call)` where `is_function_call` is true
/// when the identifier is immediately followed by `(`.
///
/// Examples:
///   `"config.region"` → `Some(("config", false))`
///   `"config(materialized='view')"` → `Some(("config", true))`
///   `"ref('model')"` → `Some(("ref", true))`
///   `"amount_cents"` → `Some(("amount_cents", false))`
///   `"\"literal\""` → `None` (string literal, not an identifier)
pub fn extract_root_identifier(expr: &str) -> Option<(&str, bool)> {
    let trimmed = expr.trim();
    if trimmed.is_empty() {
        return None;
    }
    // String literals are not identifiers
    if trimmed.starts_with('"') || trimmed.starts_with('\'') {
        return None;
    }
    // Find the end of the identifier (alphanumeric + underscore)
    let end = trimmed
        .find(|c: char| !c.is_alphanumeric() && c != '_')
        .unwrap_or(trimmed.len());
    if end == 0 {
        return None;
    }
    let ident = &trimmed[..end];
    // Check what follows the identifier
    let rest = trimmed[end..].trim_start();
    let is_function_call = rest.starts_with('(');
    Some((ident, is_function_call))
}

/// Classifies a Jinja expression for passthrough mode.
///
/// Rules:
/// - Unknown root → Passthrough (dbt variables like `ref`, `source`, `amount_cents`)
/// - Known root + function call syntax on a DICT root → Passthrough
///   (e.g. `config(materialized='view')` is dbt, not Pulumi's `config.key`)
/// - Known function → Evaluate (e.g. `readFile('file.sql')`)
/// - Known root + attribute/bare access → Evaluate (catches typos)
pub fn classify_expression(expr: &str) -> ExprClassification {
    let Some((root, is_fn_call)) = extract_root_identifier(expr) else {
        // Can't parse → pass through to be safe
        return ExprClassification::Passthrough;
    };

    // Known Pulumi functions are always evaluated
    if KNOWN_FUNCTIONS.contains(&root) {
        return ExprClassification::Evaluate;
    }

    let is_known = KNOWN_ROOTS.contains(&root);
    let is_dict = DICT_ROOTS.contains(&root);

    if !is_known {
        // Unknown root → passthrough (dbt variable or function)
        return ExprClassification::Passthrough;
    }

    if is_fn_call && is_dict {
        // Dict root used as function call: `config(materialized='view')` → dbt
        return ExprClassification::Passthrough;
    }

    // Known root with attribute access or bare usage → evaluate
    ExprClassification::Evaluate
}

/// Finds the end of a `{{ ... }}` expression, handling nested strings and braces.
/// `start` is the byte offset of the first `{` in `{{`.
/// Returns the byte offset AFTER the closing `}}`, or `None` if not found.
pub(crate) fn find_expression_end(source: &str, start: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let len = bytes.len();
    let mut i = start + 2; // skip opening {{
    let mut depth: u32 = 0; // nested brace depth (not counting the outer {{ }})

    while i < len {
        match bytes[i] {
            b'"' | b'\'' => {
                let quote = bytes[i];
                i += 1;
                while i < len && bytes[i] != quote {
                    if bytes[i] == b'\\' {
                        i += 1; // skip escaped char
                    }
                    i += 1;
                }
                // skip closing quote
            }
            b'{' => {
                depth += 1;
            }
            b'}' => {
                if depth > 0 {
                    depth -= 1;
                } else if i + 1 < len && bytes[i + 1] == b'}' {
                    return Some(i + 2);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Finds the end of a `{% raw %}` block, returning the byte offset AFTER `{% endraw %}`.
fn skip_raw_block(source: &str, start: usize) -> usize {
    // start is at the `{` of `{% raw %}`
    if let Some(pos) = source[start..].find("{% endraw %}") {
        start + pos + "{% endraw %}".len()
    } else if let Some(pos) = source[start..].find("{%- endraw -%}") {
        start + pos + "{%- endraw -%}".len()
    } else if let Some(pos) = source[start..].find("{%- endraw %}") {
        start + pos + "{%- endraw %}".len()
    } else if let Some(pos) = source[start..].find("{% endraw -%}") {
        start + pos + "{% endraw -%}".len()
    } else {
        source.len() // unterminated raw block — skip to end
    }
}

/// Returns true if position `i` starts a `{% raw %}` tag (with optional whitespace trim).
fn is_raw_block_start(source: &str, i: usize) -> bool {
    source[i..].starts_with("{% raw %}")
        || source[i..].starts_with("{%- raw %}")
        || source[i..].starts_with("{% raw -%}")
        || source[i..].starts_with("{%- raw -%}")
}

/// Allocation-free first pass: returns true if ANY expression needs escaping.
fn scan_needs_escaping(source: &str) -> bool {
    let bytes = source.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    while i + 1 < len {
        // Skip existing {% raw %} blocks
        if bytes[i] == b'{' && bytes[i + 1] == b'%' && is_raw_block_start(source, i) {
            i = skip_raw_block(source, i);
            continue;
        }
        if bytes[i] == b'{' && bytes[i + 1] == b'{' {
            if let Some(end) = find_expression_end(source, i) {
                let expr_body = extract_expr_body(source, i, end);
                if classify_expression(expr_body) == ExprClassification::Passthrough {
                    return true;
                }
                i = end;
                continue;
            }
        }
        i += 1;
    }
    false
}

/// Extracts the expression body from a `{{ ... }}` expression span,
/// handling whitespace-trim variants (`{{-` and `-}}`).
fn extract_expr_body(source: &str, start: usize, end: usize) -> &str {
    let bytes = source.as_bytes();
    // Skip opening `{{` or `{{-`
    let body_start = if start + 2 < end && bytes[start + 2] == b'-' {
        start + 3
    } else {
        start + 2
    };
    // Skip closing `}}` or `-}}`
    let body_end = if end >= 3 && bytes[end - 3] == b'-' {
        end - 3
    } else {
        end - 2
    };
    if body_start >= body_end {
        return "";
    }
    &source[body_start..body_end]
}

/// Pre-escapes Jinja expressions for passthrough mode.
///
/// Expressions classified as `Passthrough` are wrapped in `{% raw %}...{% endraw %}`.
/// Expressions classified as `Evaluate` are left untouched.
///
/// Returns `Cow::Borrowed` when no escaping is needed (zero-copy fast path).
pub fn pre_escape_for_passthrough(source: &str) -> Cow<'_, str> {
    if !scan_needs_escaping(source) {
        return Cow::Borrowed(source);
    }

    let bytes = source.as_bytes();
    let len = bytes.len();
    let expr_count = source.matches("{{").count();
    let mut result = String::with_capacity(source.len() + expr_count * 25);
    let mut i = 0;

    while i < len {
        // Skip existing {% raw %} blocks verbatim
        if i + 1 < len && bytes[i] == b'{' && bytes[i + 1] == b'%' && is_raw_block_start(source, i)
        {
            let end = skip_raw_block(source, i);
            result.push_str(&source[i..end]);
            i = end;
            continue;
        }
        if i + 1 < len && bytes[i] == b'{' && bytes[i + 1] == b'{' {
            if let Some(end) = find_expression_end(source, i) {
                let full_expr = &source[i..end];
                let expr_body = extract_expr_body(source, i, end);
                if classify_expression(expr_body) == ExprClassification::Passthrough {
                    result.push_str("{% raw %}");
                    result.push_str(full_expr);
                    result.push_str("{% endraw %}");
                } else {
                    result.push_str(full_expr);
                }
                i = end;
                continue;
            }
        }
        // Safe for ASCII-heavy Jinja templates; handles UTF-8 correctly
        let ch = source[i..].chars().next().unwrap();
        result.push(ch);
        i += ch.len_utf8();
    }

    Cow::Owned(result)
}

/// Jinja preprocessor. Borrows its configuration context.
pub struct JinjaPreprocessor<'cfg> {
    context: &'cfg JinjaContext<'cfg>,
}

impl<'cfg> JinjaPreprocessor<'cfg> {
    pub fn new(context: &'cfg JinjaContext<'cfg>) -> Self {
        Self { context }
    }
}

impl TemplatePreprocessor for JinjaPreprocessor<'_> {
    type Output<'src>
        = Cow<'src, str>
    where
        Self: 'src;
    type Err<'src>
        = RenderDiagnostic<'src>
    where
        Self: 'src;

    fn preprocess<'src>(
        &self,
        source: &'src str,
        filename: &str,
    ) -> Result<Cow<'src, str>, RenderDiagnostic<'src>> {
        self.render(source, filename)
    }
}

impl JinjaPreprocessor<'_> {
    /// The render itself, with the diagnostic's lifetime tied to `source`
    /// alone.
    ///
    /// The trait spells its error as `Err<'src> where Self: 'src`, which
    /// binds a diagnostic to the preprocessor's context as well as to the
    /// source. A caller that builds its context on the stack — the language
    /// binding does — could then never hand the diagnostic out, and the only
    /// way round would be to copy the source into it. This method has no
    /// such bound: what comes back borrows `source` and nothing else, so the
    /// trait delegates here rather than the other way round.
    pub fn render<'src>(
        &self,
        source: &'src str,
        filename: &str,
    ) -> Result<Cow<'src, str>, RenderDiagnostic<'src>> {
        // Zero-copy fast path: no Jinja syntax → return borrowed reference
        if !has_jinja_syntax(source) {
            return Ok(Cow::Borrowed(source));
        }

        // Scope out text addressed to a provider before anything else looks at
        // it. `Unchanged` is the default and costs nothing.
        let protected =
            crate::provider_scope::protect(source, self.context.provider_templated_packages)
                .map_err(|e| scope_diagnostic(source, &e))?;
        let scoped_source = protected.source(source);

        // Passthrough mode: pre-escape unknown expressions before rendering
        let effective_source = if self.context.undefined == UndefinedMode::Passthrough {
            pre_escape_for_passthrough(scoped_source)
        } else {
            Cow::Borrowed(scoped_source)
        };

        // Slow path: render through minijinja
        let mut env = minijinja::Environment::new();
        env.set_undefined_behavior(minijinja::UndefinedBehavior::Strict);
        register_custom_filters(&mut env);

        let cache = Arc::new(Mutex::new(ReadFileCache::new()));
        register_readfile_function(&mut env, self.context.project_dir, Arc::clone(&cache));

        // Register filesystem template loader for {% import %} / {% include %}
        // Resolves paths relative to project_dir with path traversal protection.
        register_template_loader(
            &mut env,
            self.context.project_dir,
            self.context.root_directory,
        );

        let compiled: &str = effective_source.as_ref();
        env.add_template(filename, compiled)
            .map_err(|e| build_render_diagnostic(source, compiled, &e))?;

        let tmpl = env
            .get_template(filename)
            .map_err(|e| build_render_diagnostic(source, compiled, &e))?;

        let mj_ctx = build_minijinja_context(self.context);
        let rendered = tmpl
            .render(&mj_ctx)
            .map_err(|e| build_render_diagnostic(source, compiled, &e))?;

        // Put provider-templated blocks back before readFile markers are
        // resolved, so a restored block cannot be mistaken for one.
        let restored = crate::provider_scope::restore(&rendered, protected.regions())
            .map_err(|e| scope_diagnostic(source, &e))?;

        // Resolve readFile markers with auto-indentation
        let final_output = match resolve_readfile_markers(
            &restored,
            &cache.lock().unwrap_or_else(|e| e.into_inner()),
        ) {
            Some(resolved) => resolved,
            None => restored.into_owned(),
        };

        Ok(Cow::Owned(final_output))
    }
}

/// Quick check for Jinja syntax markers (no allocation).
///
/// Detects expressions (`{{ }}`), blocks (`{% %}`), and comments (`{# #}`).
/// Pulumi YAML's own interpolation is `${...}`, so any of these markers is
/// unambiguously jinjanator syntax that must be rendered before the file
/// reaches the Pulumi CLI's YAML parser.
pub fn has_jinja_syntax(s: &str) -> bool {
    s.contains("{{") || s.contains("{%") || s.contains("{#")
}

// ---------------------------------------------------------------------------
// Block-Level Stripping for exec Wrapper (B.10)
// ---------------------------------------------------------------------------

/// Checks if source contains Jinja block syntax (`{% %}`) on standalone lines.
/// Only detects lines where the trimmed content starts with `{%` and ends with `%}`.
pub fn has_jinja_block_syntax(s: &str) -> bool {
    s.lines().any(|line| {
        let trimmed = line.trim();
        trimmed.starts_with("{%") && trimmed.ends_with("%}")
    })
}

/// Checks if source contains any `{% %}` syntax anywhere (including inline).
///
/// Unlike `has_jinja_block_syntax`, this detects inline blocks like
/// `val: {% if x %}y{% endif %}` which are valid Jinja but not on standalone lines.
pub fn has_any_jinja_block_syntax(s: &str) -> bool {
    s.contains("{%")
}

/// Strips lines containing Jinja block syntax (`{% %}`), preserving everything else.
/// `{{ }}` expressions in quoted strings are untouched.
/// Returns the stripped content with the original trailing newline preserved.
pub fn strip_jinja_blocks(source: &str) -> String {
    let result: Vec<&str> = source
        .lines()
        .filter(|line| {
            let trimmed = line.trim();
            !(trimmed.starts_with("{%") && trimmed.ends_with("%}"))
        })
        .collect();
    let joined = result.join("\n");
    if source.ends_with('\n') {
        joined + "\n"
    } else {
        joined
    }
}

/// Validates Jinja syntax without rendering (no context needed).
/// Catches unclosed blocks, invalid expressions, etc.
/// Returns `Ok(())` if syntax is valid, or a `RenderDiagnostic` with rich error info.
pub fn validate_jinja_syntax<'src>(
    source: &'src str,
    filename: &str,
) -> Result<(), RenderDiagnostic<'src>> {
    if !has_jinja_syntax(source) {
        return Ok(());
    }
    let mut env = minijinja::Environment::new();
    // Use lenient undefined for syntax-only validation (we don't have context yet)
    env.set_undefined_behavior(minijinja::UndefinedBehavior::Lenient);
    env.add_template(filename, source)
        .map_err(|e| build_render_diagnostic(source, source, &e))?;
    Ok(())
}

/// Converts a provider-scope refusal into a RenderDiagnostic.
fn scope_diagnostic<'src>(
    source: &'src str,
    err: &crate::provider_scope::ScopeError,
) -> RenderDiagnostic<'src> {
    use crate::provider_scope::ScopeError;
    let line = match err {
        ScopeError::TabIndent { line }
        | ScopeError::BadIndentIndicator { line }
        | ScopeError::MarkerNotAlone { line }
        | ScopeError::UnknownMarker { line } => *line as u32,
    };
    RenderDiagnostic {
        kind: RenderErrorKind::ProviderScope,
        line,
        column: 0,
        end_column: 0,
        source_line: source
            .lines()
            .nth(line.saturating_sub(1) as usize)
            .unwrap_or(""),
        expression: "",
        message: err.to_string(),
        suggestion: Some(
            "provider-templated block scalars are left unrendered; \
             move the SQL to a file and load it with fn::readFile if this cannot be fixed",
        ),
    }
}

/// Converts a minijinja::Error into a RenderDiagnostic with zero-copy source reference.
///
/// `compiled` is the text minijinja actually compiled — `source` itself on
/// the common path, or the passthrough-escaped copy of it — because the
/// error's byte range indexes that text, not the original. The failing
/// expression is located in `compiled`, then verified against the same line
/// of `source` before it is trusted: only when the bytes agree does the
/// diagnostic carry a column and an expression, and both then borrow from
/// `source`. When they do not — or the engine attached no range — the column
/// is `0` and the expression empty, which is what every caller read before.
///
/// The message is the fault alone: minijinja's `Display` appends
/// `(in <name>:<line>)`, which repeats the location the caller already has.
fn build_render_diagnostic<'src>(
    source: &'src str,
    compiled: &str,
    err: &minijinja::Error,
) -> RenderDiagnostic<'src> {
    let line = err.line().unwrap_or(0) as u32;
    let source_line = source
        .lines()
        .nth(line.saturating_sub(1) as usize)
        .unwrap_or("");
    let (kind, suggestion) = classify_jinja_error(err);
    let (column, end_column, expression) = locate_expression(source_line, compiled, err);
    let message = match err.detail() {
        Some(detail) => detail.to_string(),
        None => err.kind().to_string(),
    };

    RenderDiagnostic {
        kind,
        line,
        column,
        end_column,
        source_line,
        expression,
        message,
        suggestion,
    }
}

/// The failing expression on `source_line`, as `(column, end_column, slice)`,
/// 1-based; `(0, 0, "")` when it cannot be established.
///
/// Every index is checked: the range must lie inside `compiled` on character
/// boundaries, its line must not run past the range, and the bytes it names
/// must be the bytes found at the same offset of `source_line`. No slice is
/// taken unchecked, so a hostile or mismatched input degrades to "no column"
/// rather than to a panic.
fn locate_expression<'src>(
    source_line: &'src str,
    compiled: &str,
    err: &minijinja::Error,
) -> (u32, u32, &'src str) {
    let Some(range) = err.range() else {
        return (0, 0, "");
    };
    if range.start >= range.end
        || range.end > compiled.len()
        || !compiled.is_char_boundary(range.start)
        || !compiled.is_char_boundary(range.end)
    {
        return (0, 0, "");
    }
    let line_start = compiled[..range.start].rfind('\n').map_or(0, |i| i + 1);
    let col = range.start - line_start;
    let Some(expr) = compiled.get(range.start..range.end) else {
        return (0, 0, "");
    };
    if expr.contains('\n') {
        return (0, 0, "");
    }
    // Trust the location only where the compiled text and the source agree.
    match source_line.get(col..col + expr.len()) {
        Some(found) if found == expr => ((col + 1) as u32, (col + expr.len() + 1) as u32, found),
        _ => (0, 0, ""),
    }
}

// ---------------------------------------------------------------------------
// Post-Rendering YAML Validation (B.6)
// ---------------------------------------------------------------------------

/// Validates rendered YAML is parseable. Returns rich diagnostic on failure.
pub fn validate_rendered_yaml<'src>(
    rendered: &'src str,
    _original: &'src str,
    filename: &str,
) -> Result<(), RenderDiagnostic<'src>> {
    // This gate runs before `parse_template` on every Jinja path, so it has to
    // accept what the parser accepts. Shadowing rather than stripping inline
    // keeps `source_line` and the reported column measured against the same
    // text the deserializer read.
    let rendered = crate::encoding::strip_bom(rendered);
    if let Err(e) = serde_yaml::from_str::<serde_yaml::Value>(rendered) {
        let line = e.location().map(|l| l.line()).unwrap_or(0) as u32;
        let col = e.location().map(|l| l.column()).unwrap_or(0) as u32;
        let rendered_line = rendered
            .lines()
            .nth(line.saturating_sub(1) as usize)
            .unwrap_or("");
        let (kind, suggestion) = classify_yaml_error(&e.to_string(), rendered_line);

        return Err(RenderDiagnostic {
            kind,
            line,
            column: col,
            end_column: 0,
            source_line: rendered_line,
            expression: "",
            message: format!(
                "YAML parse error after Jinja rendering at {}:{}:{}: {}",
                filename, line, col, e
            ),
            suggestion,
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// readFile() Support — Marker-Based Deferred Auto-Indentation
// ---------------------------------------------------------------------------

/// Cache of file contents read by `readFile()` during Jinja rendering.
/// Each entry is indexed by a marker ID.
struct ReadFileCache {
    entries: Vec<String>,
}

impl ReadFileCache {
    fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    fn add(&mut self, content: String) -> usize {
        let id = self.entries.len();
        self.entries.push(content);
        id
    }

    fn get(&self, id: usize) -> Option<&str> {
        self.entries.get(id).map(|s| s.as_str())
    }
}

/// Constructs a NUL-delimited marker: `\x00RF:<id>\x00`
fn readfile_marker(id: usize) -> String {
    format!("\x00RF:{}\x00", id)
}

/// Extracts the ID from a marker string like `\x00RF:42\x00`.
fn parse_marker_id(s: &str) -> Option<usize> {
    let s = s.strip_prefix("\x00RF:")?.strip_suffix('\x00')?;
    s.parse().ok()
}

/// Returns true if the trimmed line contains only a single readFile marker.
fn is_single_marker(trimmed: &str) -> bool {
    trimmed.starts_with("\x00RF:")
        && trimmed.ends_with('\x00')
        && trimmed.matches('\x00').count() == 2
}

/// Returns the leading whitespace of a line.
fn leading_whitespace(line: &str) -> &str {
    let trimmed = line.trim_start();
    &line[..line.len() - trimmed.len()]
}

/// Prepends `indent` to all non-empty lines of `content`.
/// Trailing newline from the content is stripped to avoid double-newlines.
fn indent_content(content: &str, indent: &str) -> String {
    let content = content
        .strip_suffix("\r\n")
        .or_else(|| content.strip_suffix('\n'))
        .unwrap_or(content);
    if content.is_empty() {
        return String::new();
    }
    let line_sep = if content.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let mut result = String::new();
    for (i, line) in content.lines().enumerate() {
        if i > 0 {
            result.push_str(line_sep);
        }
        if !line.is_empty() {
            result.push_str(indent);
            result.push_str(line);
        }
    }
    result
}

/// Replaces all markers in a line with their file content (no indentation).
fn replace_inline_markers(line: &str, cache: &ReadFileCache) -> String {
    let mut result = String::new();
    let mut rest = line;
    while let Some(start) = rest.find('\x00') {
        result.push_str(&rest[..start]);
        let after_start = &rest[start..];
        if let Some(end) = after_start[1..].find('\x00') {
            let marker = &after_start[..end + 2];
            if let Some(id) = parse_marker_id(marker) {
                if let Some(content) = cache.get(id) {
                    let stripped = content
                        .strip_suffix("\r\n")
                        .or_else(|| content.strip_suffix('\n'))
                        .unwrap_or(content);
                    result.push_str(stripped);
                } else {
                    result.push_str(marker);
                }
            } else {
                result.push_str(marker);
            }
            rest = &after_start[end + 2..];
        } else {
            result.push_str(after_start);
            rest = "";
        }
    }
    result.push_str(rest);
    result
}

/// Post-processes rendered template output, replacing readFile markers with
/// properly indented file content.
///
/// - **Fast path:** if no NUL bytes are present, returns the input as-is.
/// - **Standalone markers** (only non-whitespace on the line) get auto-indented.
/// - **Inline markers** get simple text replacement.
fn resolve_readfile_markers(rendered: &str, cache: &ReadFileCache) -> Option<String> {
    if !rendered.contains('\x00') {
        return None;
    }

    let mut result = String::with_capacity(rendered.len());
    for raw in rendered.split_inclusive('\n') {
        // Carry the line's own terminator through rather than rebuilding with
        // `\n`: a CRLF file must stay CRLF, and the last line must keep
        // whatever it had.
        let line = raw
            .strip_suffix("\r\n")
            .or_else(|| raw.strip_suffix('\n'))
            .unwrap_or(raw);
        let term = &raw[line.len()..];

        if !line.contains('\x00') {
            result.push_str(raw);
            continue;
        }

        let trimmed = line.trim();
        if is_single_marker(trimmed) {
            let indent = leading_whitespace(line);
            if let Some(id) = parse_marker_id(trimmed) {
                if let Some(content) = cache.get(id) {
                    result.push_str(&indent_content(content, indent));
                }
            }
        } else {
            result.push_str(&replace_inline_markers(line, cache));
        }
        result.push_str(term);
    }
    Some(result)
}

/// SECURITY: resolves `path` relative to `project_dir` with containment.
///
/// Rejects absolute paths; canonicalizes the project directory and the
/// joined candidate (resolving `..` AND symlinks to their real targets
/// BEFORE the containment check — preserve that ordering); rejects
/// anything that escapes the project directory. Shared by the Jinja
/// `readFile()` function and static analyzers reading referenced files
/// (e.g. `fn::readFile` SQL in the lineage exporter).
/// Whether a name carries its own root — `/x` on every platform, and on
/// Windows also `\x`, `C:\x`, `C:x` and `\\server\share\x`.
///
/// `Path::is_absolute` is the wrong test: on Windows it is false for `/x`,
/// which `Path::join` would still treat as rooted and place outside the
/// project directory. A name that supplies any prefix or root component is
/// refused before a path is built from it.
pub(crate) fn is_rooted(name: &str) -> bool {
    matches!(
        Path::new(name).components().next(),
        Some(std::path::Component::Prefix(_) | std::path::Component::RootDir)
    )
}

pub(crate) fn resolve_contained_path(
    project_dir: &str,
    path: &str,
) -> Result<std::path::PathBuf, String> {
    if is_rooted(path) {
        return Err(format!(
            "readFile: absolute paths are not allowed: '{}'",
            path
        ));
    }
    let project_canonical = Path::new(project_dir)
        .canonicalize()
        .map_err(|e| format!("readFile: failed to resolve project directory: {}", e))?;
    let resolved = project_canonical
        .join(path)
        .canonicalize()
        .map_err(|e| format!("readFile: failed to resolve '{}': {}", path, e))?;
    if !resolved.starts_with(&project_canonical) {
        return Err(format!(
            "readFile: path '{}' escapes project directory",
            path
        ));
    }
    Ok(resolved)
}

/// Registers the `readFile(path)` function in the minijinja environment.
///
/// Security: rejects absolute paths and path traversals that escape
/// the project directory (e.g. `../../../etc/passwd`) — see
/// [`resolve_contained_path`].
fn register_readfile_function(
    env: &mut minijinja::Environment<'_>,
    project_dir: &str,
    cache: Arc<Mutex<ReadFileCache>>,
) {
    let project_dir = project_dir.to_string();
    env.add_function(
        "readFile",
        move |path: String| -> Result<String, minijinja::Error> {
            let resolved = resolve_contained_path(&project_dir, &path).map_err(|msg| {
                minijinja::Error::new(minijinja::ErrorKind::InvalidOperation, msg)
            })?;

            let content = std::fs::read_to_string(&resolved).map_err(|e| {
                minijinja::Error::new(
                    minijinja::ErrorKind::InvalidOperation,
                    format!("readFile: failed to read '{}': {}", path, e),
                )
            })?;

            let id = cache.lock().unwrap_or_else(|e| e.into_inner()).add(content);
            Ok(readfile_marker(id))
        },
    );
}

/// Registers a filesystem template loader for `{% import %}` and `{% include %}`.
///
/// Resolves template paths relative to `project_dir` (the directory containing
/// the Pulumi.yaml being processed). Paths with `..` components are resolved
/// naturally — `../environment.j2` from `stacks/bucket1/` finds
/// `stacks/environment.j2` or walks further up the tree.
///
/// The `root_directory` acts as an additional search base: if a template is not
/// found relative to `project_dir`, it is tried relative to `root_directory`.
/// This supports the common pattern where shared templates live at the repo root.
///
/// Containment is the boundary: a candidate is served only when its own
/// canonicalized path lies inside the stack directory or the render root.
/// Within that tree an include is any UTF-8 text file up to
/// [`MAX_INCLUDE_BYTES`]; a rooted name is refused before a path is built.
fn register_template_loader(
    env: &mut minijinja::Environment<'_>,
    project_dir: &str,
    root_directory: &str,
) {
    let base_dir = project_dir.to_string();
    let root_dir = if root_directory.is_empty() {
        project_dir.to_string()
    } else {
        root_directory.to_string()
    };
    // Canonicalize the containment roots ONCE. A resolved template must stay
    // within one of these after its own symlinks are resolved — otherwise a
    // template controlled by an untrusted author (e.g. a PR's Pulumi.yaml)
    // could `{% include '../../../secret.yaml' %}` to read and inline any
    // yaml/j2 file on the host (LFI / secret exfiltration). SECURITY-CRITICAL.
    let allowed_roots: Vec<PathBuf> = [base_dir.as_str(), root_dir.as_str()]
        .iter()
        .filter_map(|d| Path::new(d).canonicalize().ok())
        .collect();

    // What one candidate path turned out to be. `Escaped` is kept apart from
    // `Missing` because they mean opposite things to the author: one file is
    // not there, the other is there and will not be served.
    enum Candidate {
        Missing,
        Escaped,
        NotText,
        TooLarge,
        Content(String),
    }

    // Read a candidate only if it canonicalizes to a path CONTAINED in an
    // allowed root. canonicalize() resolves `..` and symlinks first, so an
    // escaping traversal or a symlink pointing outside the tree is rejected.
    let resolve = move |candidate: PathBuf| -> Candidate {
        let Ok(canonical) = candidate.canonicalize() else {
            return Candidate::Missing;
        };
        if !allowed_roots.iter().any(|root| canonical.starts_with(root)) {
            return Candidate::Escaped;
        }
        // The size is decided from the metadata, before any read, so an
        // oversized file costs one `stat` and never an allocation; a
        // directory is as absent to an include as it always was.
        let Ok(meta) = std::fs::metadata(&canonical) else {
            return Candidate::Missing;
        };
        if !meta.is_file() {
            return Candidate::Missing;
        }
        if meta.len() > MAX_INCLUDE_BYTES {
            return Candidate::TooLarge;
        }
        let Ok(bytes) = std::fs::read(&canonical) else {
            return Candidate::Missing;
        };
        // Validated in place: one allocation, the same as `read_to_string`.
        match String::from_utf8(bytes) {
            Ok(content) => Candidate::Content(content),
            Err(_) => Candidate::NotText,
        }
    };

    // A refusal crosses the render as the detail of a `BadInclude`: the VM
    // returns that kind verbatim, where it folds a loader's `TemplateNotFound`
    // into its own message and the reason would be lost. Built only when
    // refusing — the success path allocates the file's contents and nothing
    // else, as before.
    fn refuse(reason: IncludeRefusal) -> minijinja::Error {
        minijinja::Error::new(minijinja::ErrorKind::BadInclude, reason.to_string())
    }

    env.set_loader(move |name: &str| {
        if is_rooted(name) {
            return Err(refuse(IncludeRefusal::AbsolutePath {
                name: name.to_string(),
            }));
        }

        // Relative to project_dir first (handles local and .. paths that stay
        // inside the tree, e.g. '../environment.j2' at the repo root), then the
        // render root (shared templates), then the name stripped of leading
        // ../ against the render root — '../environment.j2' from a subdir
        // resolving to the project root.
        let stripped = name.trim_start_matches("../").trim_start_matches("..\\");
        let candidates = [
            Some(Path::new(&base_dir).join(name)),
            (root_dir != base_dir).then(|| Path::new(&root_dir).join(name)),
            (stripped != name).then(|| Path::new(&root_dir).join(stripped)),
        ];
        // A file that exists where the author named it answers at once,
        // served or refused: falling through to the render root would serve
        // a different file under the same name, silently.
        let mut escaped = false;
        for candidate in candidates.into_iter().flatten() {
            match resolve(candidate) {
                Candidate::Content(content) => return Ok(Some(content)),
                Candidate::NotText => {
                    return Err(refuse(IncludeRefusal::NotText {
                        name: name.to_string(),
                    }))
                }
                Candidate::TooLarge => {
                    return Err(refuse(IncludeRefusal::TooLarge {
                        name: name.to_string(),
                    }))
                }
                Candidate::Escaped => escaped = true,
                Candidate::Missing => {}
            }
        }
        if escaped {
            return Err(refuse(IncludeRefusal::EscapesSandbox {
                name: name.to_string(),
            }));
        }
        Ok(None) // genuinely absent: the VM names it, and `ignore missing` still applies
    });
}

// ---------------------------------------------------------------------------
// Custom Jinja Filters (B.7)
// ---------------------------------------------------------------------------

fn register_custom_filters(env: &mut minijinja::Environment<'_>) {
    env.add_filter("to_json", |v: minijinja::Value| -> String {
        serde_json::to_string(&v).unwrap_or_default()
    });

    // Jinja2 spells this `tojson` (no underscore); `to_json` above is the
    // Ansible/Salt spelling. Templates ported from Jinja2 — which is all of
    // them — failed with "unknown filter: tojson", and because a render error
    // is reported per file the stack then read as *missing* rather than
    // broken. Same class as the `indent` kwargs gap fixed in 0.5.19: the
    // semantics were already here, under a name real templates never use.
    env.add_filter("tojson", tojson_compat);

    env.add_filter("to_yaml", |v: minijinja::Value| -> String {
        serde_yaml::to_string(&v).unwrap_or_default()
    });
    env.add_filter("base64_encode", |s: String| -> String {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(s.as_bytes())
    });
    env.add_filter(
        "base64_decode",
        |s: String| -> Result<String, minijinja::Error> {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD
                .decode(s.as_bytes())
                .map_err(|e| {
                    minijinja::Error::new(
                        minijinja::ErrorKind::InvalidOperation,
                        format!("base64 decode failed: {}", e),
                    )
                })
                .and_then(|bytes| {
                    String::from_utf8(bytes).map_err(|e| {
                        minijinja::Error::new(
                            minijinja::ErrorKind::InvalidOperation,
                            format!("base64 decode produced invalid UTF-8: {}", e),
                        )
                    })
                })
        },
    );

    // Jinja2 authors write `indent(width=2, first=True)`. minijinja's builtin
    // is positional-only, so the kwargs map lands in `width` and fails with
    // "cannot convert map to usize". Accept both spellings; the positional
    // form keeps the builtin's exact semantics.
    env.add_filter("indent", indent_compat);

    // Jinja2 method calls on dicts/strings (`vars.items()`), which minijinja
    // exposes only as filters. A non-capturing `fn` keeps this monomorphised —
    // no boxed trait object, no captured state, trivially Send + Sync.
    env.set_unknown_method_callback(jinja2_method_compat);
}

/// Jinja2's `tojson`, including its optional `indent`.
///
/// Jinja2 accepts `tojson`, `tojson(2)` and `tojson(indent=2)`; a
/// positional-only registration would reproduce, for this filter, exactly the
/// "cannot convert map to usize" failure that `indent_compat` exists to
/// prevent. `indent=None`/absent yields the compact form, matching both
/// Jinja2 and the `to_json` spelling above, so the two names agree on every
/// input.
///
/// Serialisation cannot fail for a renderable `Value`, but an unserialisable
/// one returns an error rather than an empty string: a silently empty JSON
/// literal is a corrupt template that renders green.
fn tojson_compat(
    value: minijinja::Value,
    args: &[minijinja::Value],
    kwargs: minijinja::value::Kwargs,
) -> Result<String, minijinja::Error> {
    let indent = match args.first() {
        Some(v) => v.as_i64().map(|n| n.max(0) as usize),
        None => kwargs.get::<Option<usize>>("indent")?,
    };
    kwargs.assert_all_used()?;

    let rendered = match indent {
        Some(width) => {
            let mut buf = Vec::new();
            let indent_bytes = " ".repeat(width);
            let fmt = serde_json::ser::PrettyFormatter::with_indent(indent_bytes.as_bytes());
            let mut ser = serde_json::Serializer::with_formatter(&mut buf, fmt);
            serde::Serialize::serialize(&value, &mut ser)
                .map_err(|e| json_error(&e))
                .and_then(|()| {
                    String::from_utf8(buf).map_err(|e| {
                        minijinja::Error::new(
                            minijinja::ErrorKind::InvalidOperation,
                            format!("tojson produced invalid UTF-8: {}", e),
                        )
                    })
                })?
        }
        None => serde_json::to_string(&value).map_err(|e| json_error(&e))?,
    };
    Ok(rendered)
}

fn json_error(e: &serde_json::Error) -> minijinja::Error {
    minijinja::Error::new(
        minijinja::ErrorKind::InvalidOperation,
        format!("tojson could not serialise value: {}", e),
    )
}

/// `indent` accepting Jinja2 keyword arguments as well as positional ones.
///
/// Delegates to the builtin semantics: `width` spaces prepended to every
/// non-blank line, `first` also indenting line 1, `blank` also indenting
/// empty lines.
fn indent_compat(
    value: String,
    args: &[minijinja::Value],
    kwargs: minijinja::value::Kwargs,
) -> Result<String, minijinja::Error> {
    let width = match args.first() {
        Some(v) => usize::try_from(v.as_i64().unwrap_or(4).max(0) as u64).unwrap_or(4),
        None => kwargs.get::<Option<usize>>("width")?.unwrap_or(4),
    };
    let first = match args.get(1) {
        Some(v) => v.is_true(),
        None => kwargs.get::<Option<bool>>("first")?.unwrap_or(false),
    };
    let blank = match args.get(2) {
        Some(v) => v.is_true(),
        None => kwargs.get::<Option<bool>>("blank")?.unwrap_or(false),
    };
    kwargs.assert_all_used()?;

    let prefix = " ".repeat(width);
    let mut out = String::with_capacity(value.len() + width * 4);
    for (idx, line) in value.split('\n').enumerate() {
        if idx > 0 {
            out.push('\n');
        }
        let indentable = if line.trim().is_empty() { blank } else { true };
        if indentable && (idx > 0 || first) {
            out.push_str(&prefix);
        }
        out.push_str(line);
    }
    Ok(out)
}

/// Python-style methods Jinja2 templates call on values minijinja treats as
/// plain maps/sequences/strings.
///
/// Borrows the receiver and arguments throughout — no clone of the underlying
/// container. Unknown names fall through to minijinja's own error so typos are
/// still reported as unknown methods.
fn jinja2_method_compat(
    _state: &minijinja::State<'_, '_>,
    value: &minijinja::Value,
    method: &str,
    args: &[minijinja::Value],
) -> Result<minijinja::Value, minijinja::Error> {
    let no_args = |name: &str| -> Result<(), minijinja::Error> {
        if args.is_empty() {
            Ok(())
        } else {
            Err(minijinja::Error::new(
                minijinja::ErrorKind::TooManyArguments,
                format!("{} takes no arguments", name),
            ))
        }
    };

    match method {
        "items" => {
            no_args("items")?;
            minijinja::filters::items(value)
        }
        "keys" => {
            no_args("keys")?;
            let pairs = minijinja::filters::items(value)?;
            let keys = pairs
                .try_iter()?
                .filter_map(|pair| pair.get_item_by_index(0).ok())
                .collect::<Vec<_>>();
            Ok(minijinja::Value::from(keys))
        }
        "values" => {
            no_args("values")?;
            let pairs = minijinja::filters::items(value)?;
            let values = pairs
                .try_iter()?
                .filter_map(|pair| pair.get_item_by_index(1).ok())
                .collect::<Vec<_>>();
            Ok(minijinja::Value::from(values))
        }
        "get" => {
            let key = args.first().ok_or_else(|| {
                minijinja::Error::new(minijinja::ErrorKind::MissingArgument, "get requires a key")
            })?;
            let found = value.get_item(key).ok().filter(|v| !v.is_undefined());
            // Python's dict.get returns None when absent and no default given.
            Ok(found
                .or_else(|| args.get(1).cloned())
                .unwrap_or_else(|| minijinja::Value::from(())))
        }
        _ => Err(minijinja::Error::from(minijinja::ErrorKind::UnknownMethod)),
    }
}

// ---------------------------------------------------------------------------
// Core-Level API Entry Point (B.9)
// ---------------------------------------------------------------------------

/// Parses a template with a preprocessor applied first (static dispatch, no boxing).
pub fn parse_template_with_preprocessor<P: TemplatePreprocessor>(
    source: &str,
    preprocessor: &P,
    span: Option<crate::syntax::Span>,
) -> (
    crate::ast::template::TemplateDecl<'static>,
    crate::diag::Diagnostics,
) {
    let mut diags = crate::diag::Diagnostics::new();

    let effective_source = match preprocessor.preprocess(source, "Pulumi.yaml") {
        Ok(output) => output,
        Err(e) => {
            diags.error(span, format!("Template pre-processing error: {}", e), "");
            return (crate::ast::template::TemplateDecl::new(), diags);
        }
    };

    let (template, parse_diags) =
        crate::ast::parse::parse_template(effective_source.as_ref(), span);
    diags.extend(parse_diags);
    (template, diags)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- has_jinja_syntax ----

    #[test]
    fn test_has_jinja_syntax_expression() {
        assert!(has_jinja_syntax("{{ var }}"));
    }

    #[test]
    fn test_has_jinja_syntax_block() {
        assert!(has_jinja_syntax("{% if true %}yes{% endif %}"));
    }

    #[test]
    fn test_has_jinja_syntax_comment() {
        assert!(has_jinja_syntax("{# a comment #}"));
    }

    #[test]
    fn test_has_jinja_syntax_plain_yaml() {
        assert!(!has_jinja_syntax("name: test\nruntime: yaml\n"));
    }

    #[test]
    fn test_has_jinja_syntax_pulumi_interp() {
        // Pulumi ${} syntax should NOT trigger Jinja
        assert!(!has_jinja_syntax("name: ${resource.id}"));
    }

    #[test]
    fn test_has_jinja_syntax_single_brace() {
        // Single braces are not Jinja
        assert!(!has_jinja_syntax("{ key: value }"));
    }

    // ---- classify_yaml_error ----

    #[test]
    fn test_classify_yaml_mapping_not_allowed() {
        let (kind, suggestion) =
            classify_yaml_error("mapping values are not allowed here", "foo:bar");
        assert_eq!(kind, RenderErrorKind::YamlSyntax);
        assert!(suggestion.unwrap().contains("space after ':'"));
    }

    #[test]
    fn test_classify_yaml_block_sequence() {
        let (kind, suggestion) = classify_yaml_error(
            "block sequence entries are not allowed in this context",
            "  - item",
        );
        assert_eq!(kind, RenderErrorKind::YamlIndentation);
        assert!(suggestion.unwrap().contains("indentation"));
    }

    #[test]
    fn test_classify_yaml_duplicate_key() {
        let (kind, suggestion) = classify_yaml_error("found duplicate key", "key: value");
        assert_eq!(kind, RenderErrorKind::YamlDuplicateKey);
        assert!(suggestion.unwrap().contains("loop.index"));
    }

    #[test]
    fn test_classify_yaml_jinja_residue() {
        let (kind, suggestion) = classify_yaml_error("some error", "name: {{ var }}");
        assert_eq!(kind, RenderErrorKind::YamlSyntax);
        assert!(suggestion.unwrap().contains("quoting"));
    }

    #[test]
    fn test_classify_yaml_unknown_error() {
        let (kind, suggestion) = classify_yaml_error("something went wrong", "plain line");
        assert_eq!(kind, RenderErrorKind::YamlSyntax);
        assert!(suggestion.is_none());
    }

    // ---- format_rich ----

    #[test]
    fn test_format_rich_basic() {
        let diag = RenderDiagnostic {
            kind: RenderErrorKind::JinjaSyntax,
            line: 5,
            column: 3,
            end_column: 0,
            source_line: "{% bad %}",
            expression: "",
            message: "syntax error".to_string(),
            suggestion: None,
        };
        let formatted = diag.format_rich("Pulumi.yaml");
        assert!(formatted.contains("Pulumi.yaml:5:3: error: syntax error"));
        assert!(formatted.contains("5 | {% bad %}"));
        assert!(!formatted.contains("suggestion:"));
    }

    #[test]
    fn test_format_rich_with_suggestion() {
        let diag = RenderDiagnostic {
            kind: RenderErrorKind::JinjaUndefinedVariable,
            line: 2,
            column: 0,
            end_column: 0,
            source_line: "name: {{ unknown }}",
            expression: "",
            message: "undefined variable".to_string(),
            suggestion: Some("Check variable name"),
        };
        let formatted = diag.format_rich("test.yaml");
        assert!(formatted.contains("suggestion: Check variable name"));
    }

    #[test]
    fn test_format_rich_empty_source_line() {
        let diag = RenderDiagnostic {
            kind: RenderErrorKind::JinjaSyntax,
            line: 1,
            column: 0,
            end_column: 0,
            source_line: "",
            expression: "",
            message: "error".to_string(),
            suggestion: None,
        };
        let formatted = diag.format_rich("test.yaml");
        // Should not contain a source line section
        assert!(!formatted.contains(" | "));
    }

    #[test]
    fn test_format_rich_caret_under_the_expression() {
        let diag = RenderDiagnostic {
            kind: RenderErrorKind::JinjaUndefinedVariable,
            line: 24,
            column: 21,
            end_column: 28,
            source_line: "  mhda_project: {{ mapping[key] }}",
            expression: "mapping",
            message: "undefined value".to_string(),
            suggestion: None,
        };
        let formatted = diag.format_rich("Pulumi.yaml");
        let lines: Vec<&str> = formatted.lines().collect();
        assert_eq!(lines[0], "Pulumi.yaml:24:21: error: undefined value");
        assert_eq!(
            lines[1],
            "  24 | \u{20}\u{20}mhda_project: {{ mapping[key] }}"
        );
        assert_eq!(lines[2], "     |                     ^^^^^^^");
        let caret_at = lines[2].find('^').unwrap_or(0) - lines[2].find('|').unwrap_or(0) - 2;
        assert_eq!(caret_at, 20, "the caret starts under column 21");
    }

    #[test]
    fn test_format_rich_caret_is_measured_in_characters() {
        let diag = RenderDiagnostic {
            kind: RenderErrorKind::JinjaUndefinedVariable,
            line: 1,
            column: 15,
            end_column: 19,
            source_line: "日本語: {{ nope }}",
            expression: "nope",
            message: "undefined value".to_string(),
            suggestion: None,
        };
        let formatted = diag.format_rich("t.yaml");
        let caret_row = formatted.lines().nth(2).unwrap_or("");
        // Three ideographs then ": {{ " — fourteen bytes, eight characters —
        // so the caret sits at character 8, not at byte 14.
        let visual = caret_row.find('^').unwrap_or(0) - caret_row.find('|').unwrap_or(0) - 2;
        assert_eq!(visual, 8);
    }

    // ---- locating the expression ----

    fn strict_ctx<'a>(
        config: &'a HashMap<String, String>,
        extra: &'a HashMap<String, String>,
    ) -> JinjaContext<'a> {
        JinjaContext {
            project_name: "test",
            stack_name: "dev",
            cwd: "/tmp",
            organization: "",
            root_directory: "",
            config,
            project_dir: "",
            undefined: UndefinedMode::Strict,
            provider_templated_packages: &[],
            extra,
        }
    }

    fn fail<'s>(source: &'s str) -> RenderDiagnostic<'s> {
        let config = HashMap::new();
        let extra = HashMap::new();
        let ctx = strict_ctx(&config, &extra);
        let Err(diag) = JinjaPreprocessor::new(&ctx).render(source, "t.yaml") else {
            panic!("the fixture must fail to render")
        };
        diag
    }

    #[test]
    fn an_undefined_name_is_located_to_the_character() {
        let diag = fail("a: 1\nname: {{ unknown_var }}\n");
        assert_eq!(diag.kind, RenderErrorKind::JinjaUndefinedVariable);
        assert_eq!((diag.line, diag.column, diag.end_column), (2, 10, 21));
        assert_eq!(diag.expression, "unknown_var");
        assert_eq!(diag.source_line, "name: {{ unknown_var }}");
        assert_eq!(diag.message, "undefined value", "no location suffix");
    }

    #[test]
    fn a_failed_subscript_names_the_whole_lookup() {
        let diag = fail("x: {{ a.b[c] }}\n");
        assert_eq!(diag.kind, RenderErrorKind::JinjaUndefinedVariable);
        assert!(diag.column > 0);
        let col = (diag.column - 1) as usize;
        assert_eq!(
            &diag.source_line[col..col + diag.expression.len()],
            diag.expression
        );
    }

    #[test]
    fn a_filter_error_is_located_too() {
        let diag = fail("x: {{ 1 | no_such_filter }}\n");
        assert_eq!(diag.line, 1);
        assert!(diag.message.contains("no_such_filter"));
    }

    #[test]
    fn the_first_and_last_lines_locate_alike() {
        let first = fail("{{ nope }}\nb: 2\n");
        assert_eq!((first.line, first.column), (1, 4));
        assert_eq!(first.expression, "nope");
        let last = fail("a: 1\nb: 2\n{{ nope }}");
        assert_eq!((last.line, last.column), (3, 4));
        assert_eq!(last.expression, "nope");
    }

    #[test]
    fn a_multi_byte_prefix_keeps_the_column_a_byte_offset_into_the_line() {
        let diag = fail("é: {{ nope }}\n");
        let col = (diag.column - 1) as usize;
        assert_eq!(&diag.source_line[col..col + 4], "nope");
        assert_eq!(diag.column, 8, "é is two bytes; the column indexes bytes");
    }

    #[test]
    fn the_expression_and_the_line_borrow_the_source() {
        let source = String::from("a: 1\nname: {{ unknown_var }}\n");
        let diag = fail(&source);
        let start = source.as_ptr() as usize;
        let end = start + source.len();
        assert!((start..end).contains(&(diag.source_line.as_ptr() as usize)));
        assert!((start..end).contains(&(diag.expression.as_ptr() as usize)));
    }

    #[test]
    fn a_range_the_source_does_not_agree_with_degrades_to_no_column() {
        // The compiled text is not the source: passthrough escaping rewrote
        // it, so the range indexes different bytes. The line still names the
        // fault; the column and expression must not lie about where.
        let source = "a: 1\nb: {{ nope }}\n";
        let rewritten = "a: 1\nb: {% raw %}{{ ref('m') }}{% endraw %}{{ nope }}\n";
        let mut env = minijinja::Environment::new();
        env.set_undefined_behavior(minijinja::UndefinedBehavior::Strict);
        env.add_template("t", rewritten).expect("compiles");
        let err = env
            .get_template("t")
            .expect("registered")
            .render(minijinja::context! {})
            .expect_err("nope is undefined");
        let diag = build_render_diagnostic(source, rewritten, &err);
        assert_eq!(diag.line, 2);
        assert_eq!((diag.column, diag.end_column), (0, 0));
        assert_eq!(diag.expression, "");
        assert_eq!(diag.source_line, "b: {{ nope }}");
    }

    #[test]
    fn an_error_with_no_range_has_no_column() {
        let err = minijinja::Error::new(minijinja::ErrorKind::InvalidOperation, "x");
        let diag = build_render_diagnostic("a: 1\n", "a: 1\n", &err);
        assert_eq!((diag.line, diag.column, diag.end_column), (0, 0, 0));
        assert_eq!(diag.expression, "");
        assert_eq!(diag.message, "x");
    }

    #[test]
    fn a_message_without_detail_is_the_kind() {
        let err = minijinja::Error::from(minijinja::ErrorKind::UndefinedError);
        let diag = build_render_diagnostic("", "", &err);
        assert_eq!(diag.message, "undefined value");
    }

    // ---- includes ----

    /// A render whose stack directory and render root may differ — the
    /// shape every nested stack has.
    fn render_in_roots(
        project: &std::path::Path,
        root: &std::path::Path,
        source: &str,
    ) -> Result<String, String> {
        let p = project.to_str().expect("utf-8 path").to_string();
        let r = root.to_str().expect("utf-8 path").to_string();
        let config = HashMap::new();
        let extra = HashMap::new();
        let ctx = JinjaContext {
            project_dir: &p,
            root_directory: &r,
            cwd: &p,
            ..strict_ctx(&config, &extra)
        };
        JinjaPreprocessor::new(&ctx)
            .render(source, "Pulumi.yaml")
            .map(|r| r.into_owned())
            .map_err(|e| format!("{:?}|{}", e.kind, e.message))
    }

    fn render_in(dir: &std::path::Path, source: &str) -> Result<String, String> {
        render_in_roots(dir, dir, source)
    }

    /// A repo root with `images/app/VERSION` and a stack three levels down.
    fn nested_stack(root: &std::path::Path) -> std::path::PathBuf {
        std::fs::create_dir_all(root.join("images/app")).expect("mkdir");
        std::fs::write(root.join("images/app/VERSION"), "1.2.3\n").expect("write");
        let project = root.join("stacks/app/bigquery");
        std::fs::create_dir_all(&project).expect("mkdir");
        project
    }

    #[test]
    fn any_text_file_inside_the_tree_is_served() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("schemas")).expect("mkdir");
        std::fs::write(dir.path().join("schemas/t.json"), "[1]").expect("write");
        std::fs::write(dir.path().join("q.sql"), "SELECT 1").expect("write");
        std::fs::write(dir.path().join("VERSION"), "1.2.3\n").expect("write");
        std::fs::write(dir.path().join("notes.txt"), "text").expect("write");
        for (name, body) in [
            ("schemas/t.json", "[1]"),
            ("q.sql", "SELECT 1"),
            ("VERSION", "1.2.3"),
            ("notes.txt", "text"),
        ] {
            assert_eq!(
                render_in(dir.path(), &format!("a: '{{% include \"{name}\" %}}'\n")),
                Ok(format!("a: '{body}'")),
                "{name}"
            );
        }
    }

    #[test]
    fn a_set_block_around_an_include_trims_to_the_version() {
        let root = tempfile::tempdir().expect("tempdir");
        let project = nested_stack(root.path());
        let out = render_in_roots(
            &project,
            root.path(),
            "{% set v %}{% include '../../../images/app/VERSION' %}{% endset -%}\n\
             name: app\ntag: {{ v | trim }}\n",
        )
        .expect("renders");
        assert!(out.ends_with("name: app\ntag: 1.2.3"), "{out:?}");
    }

    #[test]
    fn each_refusal_is_reported_distinctly() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("logo.png"), [0x89, 0x50, 0xff, 0xfe, 0x00]).expect("write");
        let bin = render_in(dir.path(), "a: '{% include \"logo.png\" %}'\n").unwrap_err();
        assert!(
            bin.starts_with("JinjaTemplateNotFound|include refused [binary]"),
            "{bin}"
        );
        std::fs::write(
            dir.path().join("big.txt"),
            vec![b'x'; MAX_INCLUDE_BYTES as usize + 1],
        )
        .expect("write");
        let big = render_in(dir.path(), "a: '{% include \"big.txt\" %}'\n").unwrap_err();
        assert!(
            big.starts_with("JinjaTemplateNotFound|include refused [too large]"),
            "{big}"
        );
        let abs = render_in(dir.path(), "a: '{% include \"/etc/hosts.yaml\" %}'\n").unwrap_err();
        assert!(
            abs.starts_with("JinjaTemplateNotFound|include refused [absolute]"),
            "{abs}"
        );
        let missing = render_in(dir.path(), "a: '{% include \"gone.yaml\" %}'\n").unwrap_err();
        assert!(missing.starts_with("JinjaTemplateNotFound|"), "{missing}");
        assert!(
            missing.contains("gone.yaml") && !missing.contains("refused"),
            "{missing}"
        );
    }

    #[test]
    fn a_rooted_name_is_refused_on_every_platform() {
        assert!(is_rooted("/etc/hosts.yaml"));
        assert!(!is_rooted("a/b.yaml"));
        assert!(!is_rooted("../environment.j2"));
        assert!(!is_rooted(""));
        #[cfg(windows)]
        {
            assert!(is_rooted("\\x.yaml"));
            assert!(is_rooted("C:\\x.yaml"));
            assert!(is_rooted("C:x.yaml"));
            assert!(is_rooted("\\\\server\\share\\x.yaml"));
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let abs = render_in(dir.path(), "a: '{% include \"/etc/hosts.yaml\" %}'\n").unwrap_err();
        assert!(abs.contains("[absolute]"), "{abs}");
        assert_eq!(
            resolve_contained_path(dir.path().to_str().expect("utf-8"), "/etc/hosts").unwrap_err(),
            "readFile: absolute paths are not allowed: '/etc/hosts'"
        );
    }

    #[test]
    fn ignore_missing_still_ignores_an_absent_file_but_never_a_refusal() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            render_in(
                dir.path(),
                "a: '{% include \"gone.yaml\" ignore missing %}'\n"
            ),
            Ok("a: ''".to_string())
        );
        std::fs::write(dir.path().join("logo.png"), [0xff, 0xfe]).expect("write");
        std::fs::write(
            dir.path().join("big.txt"),
            vec![b'x'; MAX_INCLUDE_BYTES as usize + 1],
        )
        .expect("write");
        for name in ["/abs.yaml", "logo.png", "big.txt"] {
            let refused = render_in(
                dir.path(),
                &format!("a: '{{% include \"{name}\" ignore missing %}}'\n"),
            );
            assert!(
                refused.is_err(),
                "{name}: a refusal is not something to ignore"
            );
        }
    }

    #[test]
    fn a_file_at_the_cap_is_served_and_one_byte_over_is_not() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("cap.txt"),
            vec![b'x'; MAX_INCLUDE_BYTES as usize],
        )
        .expect("write");
        let out = render_in(dir.path(), "{% include \"cap.txt\" %}").expect("at the cap");
        assert_eq!(out.len(), MAX_INCLUDE_BYTES as usize);
        std::fs::write(
            dir.path().join("over.txt"),
            vec![b'x'; MAX_INCLUDE_BYTES as usize + 1],
        )
        .expect("write");
        let err = render_in(dir.path(), "{% include \"over.txt\" %}").unwrap_err();
        assert!(err.contains(IncludeRefusal::TAG_TOO_LARGE), "{err}");
        assert!(!err.contains("xxxx"), "no contents in the error");
    }

    #[test]
    fn a_directory_named_as_an_include_is_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("dir.txt")).expect("mkdir");
        let err = render_in(dir.path(), "a: '{% include \"dir.txt\" %}'\n").unwrap_err();
        assert!(err.contains("dir.txt") && !err.contains("refused"), "{err}");
        assert_eq!(
            render_in(
                dir.path(),
                "a: '{% include \"dir.txt\" ignore missing %}'\n"
            ),
            Ok("a: ''".to_string())
        );
    }

    #[test]
    fn a_refusal_on_the_first_candidate_is_not_retried_against_the_root() {
        let root = tempfile::tempdir().expect("tempdir");
        let project = nested_stack(root.path());
        std::fs::write(root.path().join("x.txt"), "root copy").expect("write");
        std::fs::write(project.join("x.txt"), [0xff, 0xfe]).expect("write");
        let err =
            render_in_roots(&project, root.path(), "a: '{% include \"x.txt\" %}'\n").unwrap_err();
        assert!(err.contains(IncludeRefusal::TAG_BINARY), "{err}");
    }

    #[test]
    fn the_root_candidate_serves_a_shared_template() {
        let root = tempfile::tempdir().expect("tempdir");
        let project = nested_stack(root.path());
        std::fs::write(root.path().join("shared.txt"), "S").expect("write");
        assert_eq!(
            render_in_roots(&project, root.path(), "a: '{% include \"shared.txt\" %}'\n"),
            Ok("a: 'S'".to_string())
        );
    }

    #[test]
    fn every_leading_dotdot_is_stripped_against_the_root() {
        let root = tempfile::tempdir().expect("tempdir");
        let project = nested_stack(root.path());
        std::fs::write(root.path().join("shared.txt"), "S").expect("write");
        // Five levels up from a stack three levels down: the first candidate
        // resolves above the root (and does not exist there); the stripped
        // name against the root is what serves it.
        assert_eq!(
            render_in_roots(
                &project,
                root.path(),
                "a: '{% include \"../../../../../shared.txt\" %}'\n"
            ),
            Ok("a: 'S'".to_string())
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_in_root_symlink_is_served() {
        let root = tempfile::tempdir().expect("tempdir");
        let project = nested_stack(root.path());
        std::fs::write(root.path().join("real.txt"), "R").expect("write");
        std::os::unix::fs::symlink(root.path().join("real.txt"), project.join("link.txt"))
            .expect("symlink");
        assert_eq!(
            render_in_roots(&project, root.path(), "a: '{% include \"link.txt\" %}'\n"),
            Ok("a: 'R'".to_string())
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_escaping_first_candidate_does_not_hide_a_served_second() {
        let outside = tempfile::tempdir().expect("outside");
        std::fs::write(outside.path().join("x.txt"), "OUTSIDE").expect("write");
        let root = tempfile::tempdir().expect("tempdir");
        let project = nested_stack(root.path());
        std::os::unix::fs::symlink(outside.path().join("x.txt"), project.join("x.txt"))
            .expect("symlink");
        std::fs::write(root.path().join("x.txt"), "R").expect("write");
        assert_eq!(
            render_in_roots(&project, root.path(), "a: '{% include \"x.txt\" %}'\n"),
            Ok("a: 'R'".to_string())
        );
    }

    #[test]
    fn an_empty_root_directory_means_the_stack_dir_alone() {
        let root = tempfile::tempdir().expect("tempdir");
        let project = nested_stack(root.path());
        std::fs::write(root.path().join("shared.txt"), "S").expect("write");
        let err = render_in_roots(
            &project,
            std::path::Path::new(""),
            "a: '{% include \"shared.txt\" %}'\n",
        )
        .unwrap_err();
        assert!(
            err.contains("shared.txt") && !err.contains("refused"),
            "{err}"
        );
    }

    #[test]
    fn a_root_that_does_not_exist_is_simply_not_searched() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("a.txt"), "A").expect("write");
        assert_eq!(
            render_in_roots(
                dir.path(),
                std::path::Path::new("/nonexistent-render-root"),
                "a: '{% include \"a.txt\" %}'\n"
            ),
            Ok("a: 'A'".to_string())
        );
    }

    #[test]
    fn an_included_file_is_a_template_not_a_verbatim_paste() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("frag.txt"), "{{ 1 + 1 }}").expect("write");
        assert_eq!(
            render_in(dir.path(), "a: '{% include \"frag.txt\" %}'\n"),
            Ok("a: '2'".to_string())
        );
    }

    #[test]
    fn an_included_files_trailing_newline_is_dropped() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("one.txt"), "v\n").expect("write");
        std::fs::write(dir.path().join("two.txt"), "v\n\n").expect("write");
        assert_eq!(
            render_in(dir.path(), "a: '{% include \"one.txt\" %}'"),
            Ok("a: 'v'".to_string())
        );
        assert_eq!(
            render_in(dir.path(), "a: '{% include \"two.txt\" %}'"),
            Ok("a: 'v\n'".to_string()),
            "exactly one trailing newline is dropped"
        );
    }

    #[test]
    fn an_include_reads_the_callers_set_and_import_names() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("m.j2"), "{% set x = 'M' %}").expect("write");
        std::fs::write(dir.path().join("frag.txt"), "{{ y }}-{{ m.x }}").expect("write");
        assert_eq!(
            render_in(
                dir.path(),
                "{% import 'm.j2' as m %}{% set y = 'Y' %}a: '{% include \"frag.txt\" %}'"
            ),
            Ok("a: 'Y-M'".to_string())
        );
    }

    #[test]
    fn a_set_inside_an_include_is_visible_after_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("frag.txt"), "{% set z = 'Z' %}").expect("write");
        assert_eq!(
            render_in(dir.path(), "{% include 'frag.txt' %}a: '{{ z }}'"),
            Ok("a: 'Z'".to_string())
        );
    }

    #[test]
    fn an_import_exports_only_its_own_names() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("m.j2"), "{% set inner = y %}").expect("write");
        assert_eq!(
            render_in(
                dir.path(),
                "{% set y = 'Y' %}{% import 'm.j2' as m %}a: '{{ m.inner }}'"
            ),
            Ok("a: 'Y'".to_string()),
            "an import reads the parent's names"
        );
        let err = render_in(
            dir.path(),
            "{% set y = 'Y' %}{% import 'm.j2' as m %}a: '{{ m.y }}'",
        )
        .unwrap_err();
        assert!(err.starts_with("JinjaUndefinedVariable|"), "{err}");
    }

    #[test]
    fn with_context_is_accepted_and_changes_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("m.j2"), "{% set x = 'M' %}").expect("write");
        let plain = render_in(dir.path(), "{% import 'm.j2' as m %}a: '{{ m.x }}'");
        let with = render_in(
            dir.path(),
            "{% import 'm.j2' as m with context %}a: '{{ m.x }}'",
        );
        assert_eq!(plain, Ok("a: 'M'".to_string()));
        assert_eq!(with, plain);
    }

    #[test]
    fn a_self_including_file_is_an_error_not_an_absence() {
        let dir = tempfile::tempdir().expect("tempdir");
        // The render's own name resolves to the compiled template, not the
        // loader: it is the file's own text, bounded by the recursion limit,
        // and it never reads as a missing file.
        let err = render_in(dir.path(), "{% include 'Pulumi.yaml' %}").unwrap_err();
        assert!(!err.starts_with("JinjaTemplateNotFound|"), "{err}");
    }

    #[test]
    fn extras_never_override_builtins() {
        let config = HashMap::new();
        let mut extra = HashMap::new();
        extra.insert("pulumi_project".to_string(), "evil".to_string());
        let ctx = strict_ctx(&config, &extra);
        let out = JinjaPreprocessor::new(&ctx)
            .render("{{ pulumi_project }}", "Pulumi.yaml")
            .expect("renders");
        assert_ne!(out, "evil");
        assert_eq!(out, ctx.project_name);
    }

    #[test]
    fn crlf_survives_readfile_marker_resolution() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("f.txt"), "X").expect("write");
        let out = render_in(dir.path(), "a: 1\r\nb: {{ readFile('f.txt') }}\r\nc: 3\r\n")
            .expect("renders");
        assert!(out.contains("a: 1\r\nb: X\r\nc: 3"), "{out:?}");
    }

    #[test]
    fn a_base64_decode_failure_is_a_filter_error_not_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = render_in(dir.path(), "a: {{ '!!!' | base64_decode }}").unwrap_err();
        assert!(err.contains("base64"), "{err}");
    }

    #[test]
    fn the_refusal_reads_back_its_own_suggestion() {
        for reason in [
            IncludeRefusal::AbsolutePath {
                name: "/a.yaml".into(),
            },
            IncludeRefusal::EscapesSandbox {
                name: "../a.yaml".into(),
            },
            IncludeRefusal::NotText {
                name: "logo.png".into(),
            },
            IncludeRefusal::TooLarge {
                name: "big.txt".into(),
            },
        ] {
            let text = reason.to_string();
            assert!(text.starts_with(IncludeRefusal::PREFIX));
            assert!(IncludeRefusal::suggestion_for(&text).is_some(), "{text}");
        }
        assert!(IncludeRefusal::suggestion_for("something else").is_none());
    }

    #[test]
    fn the_trait_and_the_inherent_render_are_one_body() {
        let config = HashMap::new();
        let extra = HashMap::new();
        let ctx = strict_ctx(&config, &extra);
        let pre = JinjaPreprocessor::new(&ctx);
        for source in ["a: {{ pulumi_project }}\n", "a: {{ nope }}\n", "plain: 1\n"] {
            let via_trait = pre
                .preprocess(source, "t.yaml")
                .map_err(|e| e.format_rich("t.yaml"));
            let inherent = pre
                .render(source, "t.yaml")
                .map_err(|e| e.format_rich("t.yaml"));
            assert_eq!(via_trait, inherent, "{source}");
        }
    }

    // ---- Display impl ----

    #[test]
    fn test_render_diagnostic_display() {
        let diag = RenderDiagnostic {
            kind: RenderErrorKind::JinjaSyntax,
            line: 1,
            column: 0,
            end_column: 0,
            source_line: "",
            expression: "",
            message: "test message".to_string(),
            suggestion: None,
        };
        assert_eq!(format!("{}", diag), "test message");
    }

    // ---- build_config_value ----

    #[test]
    fn test_build_config_value_strips_namespace() {
        let mut config = HashMap::new();
        config.insert("myproject:region".to_string(), "us-west-2".to_string());
        config.insert("plain_key".to_string(), "value".to_string());
        let val = build_config_value(&config);
        // The key "myproject:region" should be accessible as "region"
        let region = val.get_attr("region").unwrap();
        assert_eq!(region.to_string(), "us-west-2");
        let plain = val.get_attr("plain_key").unwrap();
        assert_eq!(plain.to_string(), "value");
    }

    #[test]
    fn test_build_config_value_empty() {
        let config = HashMap::new();
        let val = build_config_value(&config);
        // Should be truthy (an empty map object, not undefined)
        assert!(!val.is_undefined());
    }

    // ---- validate_rendered_yaml ----

    #[test]
    fn test_validate_rendered_yaml_valid() {
        let yaml = "name: test\nruntime: yaml\n";
        assert!(validate_rendered_yaml(yaml, yaml, "test.yaml").is_ok());
    }

    #[test]
    fn test_validate_rendered_yaml_leading_byte_order_mark() {
        // This gate runs before parse_template on every Jinja path, so a marked
        // render must pass it — and a marked render that is genuinely broken
        // must still be reported at the line the author can see.
        let ok = "\u{feff}name: app\nruntime: yaml\n";
        assert!(validate_rendered_yaml(ok, ok, "Pulumi.yaml").is_ok());

        // Before the strip, a mark turned *every* error in the file into the
        // same phantom "more than one document", with no location at all. The
        // real defect is now named, at the line and column the author can see.
        let broken = "\u{feff}name: app\nruntime: yaml\n  bad: 1\n";
        let Err(d) = validate_rendered_yaml(broken, broken, "Pulumi.yaml") else {
            unreachable!("a mapping value in that position must not parse")
        };
        assert!(
            !d.message.contains("more than one document"),
            "{}",
            d.message
        );
        assert_eq!((d.line, d.column), (3, 6));
        assert_eq!(d.source_line, "  bad: 1");
    }

    #[test]
    fn test_validate_rendered_yaml_invalid() {
        let yaml = ":\n  :\n   :\n    [\n";
        let result = validate_rendered_yaml(yaml, yaml, "test.yaml");
        assert!(result.is_err());
        let diag = result.unwrap_err();
        assert!(diag.message.contains("YAML parse error"));
        assert!(diag.message.contains("test.yaml"));
    }

    // ---- NoopPreprocessor ----

    #[test]
    fn test_noop_returns_same_reference() {
        let source = "hello world";
        let result = NoopPreprocessor.preprocess(source, "test").unwrap();
        assert!(std::ptr::eq(result, source));
    }

    // ---- JinjaPreprocessor fast path ----

    #[test]
    fn test_jinja_fast_path_no_syntax() {
        let config = HashMap::new();
        let ctx = JinjaContext {
            project_name: "test",
            stack_name: "dev",
            cwd: "/tmp",
            organization: "",
            root_directory: "",
            config: &config,
            project_dir: "",
            undefined: UndefinedMode::Strict,
            provider_templated_packages: &[],
            extra: &HashMap::new(),
        };
        let preprocessor = JinjaPreprocessor::new(&ctx);
        let source = "name: test\nruntime: yaml\n";
        let result = preprocessor.preprocess(source, "test.yaml").unwrap();
        assert!(matches!(result, Cow::Borrowed(_)));
    }

    #[test]
    fn test_jinja_renders_expression() {
        let config = HashMap::new();
        let ctx = JinjaContext {
            project_name: "myproject",
            stack_name: "dev",
            cwd: "/tmp",
            organization: "",
            root_directory: "",
            config: &config,
            project_dir: "",
            undefined: UndefinedMode::Strict,
            provider_templated_packages: &[],
            extra: &HashMap::new(),
        };
        let preprocessor = JinjaPreprocessor::new(&ctx);
        let source = "name: {{ pulumi_project }}\n";
        let result = preprocessor.preprocess(source, "test.yaml").unwrap();
        assert!(matches!(result, Cow::Owned(_)));
        assert!(result.as_ref().contains("name: myproject"));
    }

    // ---- tojson (Jinja2 spelling) ----

    /// Render `source` through a default context.
    ///
    /// Both sides are owned: the diagnostic borrows from the context, which
    /// dies with this frame.
    fn render_str(source: &str) -> Result<String, String> {
        let config = HashMap::new();
        let extra = HashMap::new();
        let ctx = JinjaContext {
            project_name: "test",
            stack_name: "dev",
            cwd: "/tmp",
            organization: "",
            root_directory: "",
            config: &config,
            project_dir: "",
            undefined: UndefinedMode::Strict,
            provider_templated_packages: &[],
            extra: &extra,
        };
        match JinjaPreprocessor::new(&ctx).preprocess(source, "test.yaml") {
            Ok(c) => Ok(c.into_owned()),
            Err(d) => Err(format!("{d:?}")),
        }
    }

    #[test]
    fn test_tojson_list() {
        let out = render_str("x: {% set v = [1, 2] %}{{ v | tojson }}\n").unwrap();
        assert!(out.contains("x: [1,2]"), "got: {out}");
    }

    #[test]
    fn test_tojson_map() {
        let out = render_str("x: {% set v = {\"a\": 1} %}{{ v | tojson }}\n").unwrap();
        assert!(out.contains(r#"{"a":1}"#), "got: {out}");
    }

    #[test]
    fn test_tojson_string_is_quoted() {
        let out = render_str("x: {{ \"hi\" | tojson }}\n").unwrap();
        assert!(out.contains(r#"x: "hi""#), "got: {out}");
    }

    #[test]
    fn test_tojson_escapes_embedded_quotes() {
        // The reason a template reaches for tojson at all: emitting a value
        // into YAML without hand-rolling the escaping.
        let out = render_str("x: {{ 'a\"b' | tojson }}\n").unwrap();
        assert!(out.contains(r#""a\"b""#), "got: {out}");
    }

    #[test]
    fn test_tojson_agrees_with_to_json_spelling() {
        // Both names must serialise identically, or a template's meaning would
        // depend on which spelling its author happened to use.
        let a = render_str("x: {% set v = {\"a\": [1, 2]} %}{{ v | tojson }}\n").unwrap();
        let b = render_str("x: {% set v = {\"a\": [1, 2]} %}{{ v | to_json }}\n").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn test_tojson_indent_positional_and_kwarg() {
        // Jinja2 accepts tojson(2) and tojson(indent=2). A positional-only
        // registration would reproduce the "cannot convert map to usize"
        // failure that indent_compat exists to prevent.
        let pos = render_str("{% set v = {\"a\": 1} %}{{ v | tojson(2) }}\n").unwrap();
        let kw = render_str("{% set v = {\"a\": 1} %}{{ v | tojson(indent=2) }}\n").unwrap();
        assert_eq!(pos, kw);
        assert!(
            pos.contains("\n  \"a\": 1"),
            "expected pretty form, got: {pos}"
        );
    }

    #[test]
    fn test_tojson_rejects_unknown_kwarg() {
        // assert_all_used: a typo'd kwarg must fail loudly, not be ignored.
        assert!(render_str("{{ [1] | tojson(indnet=2) }}\n").is_err());
    }

    #[test]
    fn test_jinja_strict_undefined() {
        let config = HashMap::new();
        let ctx = JinjaContext {
            project_name: "test",
            stack_name: "dev",
            cwd: "/tmp",
            organization: "",
            root_directory: "",
            config: &config,
            project_dir: "",
            undefined: UndefinedMode::Strict,
            provider_templated_packages: &[],
            extra: &HashMap::new(),
        };
        let preprocessor = JinjaPreprocessor::new(&ctx);
        let source = "name: {{ nonexistent }}\n";
        let result = preprocessor.preprocess(source, "test.yaml");
        assert!(result.is_err());
        let diag = result.unwrap_err();
        assert_eq!(diag.kind, RenderErrorKind::JinjaUndefinedVariable);
    }

    #[test]
    fn test_jinja_syntax_error() {
        let config = HashMap::new();
        let ctx = JinjaContext {
            project_name: "test",
            stack_name: "dev",
            cwd: "/tmp",
            organization: "",
            root_directory: "",
            config: &config,
            project_dir: "",
            undefined: UndefinedMode::Strict,
            provider_templated_packages: &[],
            extra: &HashMap::new(),
        };
        let preprocessor = JinjaPreprocessor::new(&ctx);
        let source = "{% for %}\n";
        let result = preprocessor.preprocess(source, "test.yaml");
        assert!(result.is_err());
        let diag = result.unwrap_err();
        assert_eq!(diag.kind, RenderErrorKind::JinjaSyntax);
        assert!(diag.suggestion.is_some());
    }

    #[test]
    fn test_jinja_preserves_pulumi_interpolation() {
        let config = HashMap::new();
        let ctx = JinjaContext {
            project_name: "test",
            stack_name: "dev",
            cwd: "/tmp",
            organization: "",
            root_directory: "",
            config: &config,
            project_dir: "",
            undefined: UndefinedMode::Strict,
            provider_templated_packages: &[],
            extra: &HashMap::new(),
        };
        let preprocessor = JinjaPreprocessor::new(&ctx);
        // Jinja {{ }} gets processed but Pulumi ${} passes through
        let source = "name: {{ pulumi_project }}\nref: ${resource.id}\n";
        let result = preprocessor.preprocess(source, "test.yaml").unwrap();
        assert!(result.contains("name: test"));
        assert!(result.contains("${resource.id}"));
    }

    // ---- has_jinja_block_syntax ----

    #[test]
    fn test_has_jinja_block_syntax_for_loop() {
        assert!(has_jinja_block_syntax(
            "resources:\n{% for i in range(3) %}\n  bucket{{ i }}:\n{% endfor %}\n"
        ));
    }

    #[test]
    fn test_has_jinja_block_syntax_if() {
        assert!(has_jinja_block_syntax("{% if true %}\nyes\n{% endif %}\n"));
    }

    #[test]
    fn test_has_jinja_block_syntax_with_indent() {
        assert!(has_jinja_block_syntax("  {% for i in range(3) %}\n"));
    }

    #[test]
    fn test_has_jinja_block_syntax_false_no_blocks() {
        // Only {{ }} expressions, no {% %} blocks
        assert!(!has_jinja_block_syntax("name: {{ var }}\nruntime: yaml\n"));
    }

    #[test]
    fn test_has_jinja_block_syntax_false_plain_yaml() {
        assert!(!has_jinja_block_syntax("name: test\nruntime: yaml\n"));
    }

    #[test]
    fn test_has_jinja_block_syntax_false_inline_block() {
        // {% %} embedded in a non-standalone line should still be detected
        // as long as the trimmed line starts with {% and ends with %}
        assert!(has_jinja_block_syntax("  {% if x %}  \n"));
        // But not if there's content before/after
        assert!(!has_jinja_block_syntax("foo {% if x %} bar\n"));
    }

    // ---- strip_jinja_blocks ----

    #[test]
    fn test_strip_jinja_blocks_for_loop() {
        let source = "resources:\n{% for i in range(3) %}\n  bucket{{ i }}:\n    type: aws:s3:Bucket\n{% endfor %}\noutputs:\n  x: y\n";
        let stripped = strip_jinja_blocks(source);
        assert!(!stripped.contains("{% for"));
        assert!(!stripped.contains("{% endfor"));
        assert!(stripped.contains("bucket{{ i }}"));
        assert!(stripped.contains("resources:"));
        assert!(stripped.contains("outputs:"));
    }

    #[test]
    fn test_strip_jinja_blocks_conditional() {
        let source = "{% if true %}\n  resource:\n    type: test\n{% endif %}\n";
        let stripped = strip_jinja_blocks(source);
        assert!(!stripped.contains("{% if"));
        assert!(!stripped.contains("{% endif"));
        assert!(stripped.contains("resource:"));
    }

    #[test]
    fn test_strip_jinja_blocks_preserves_rest() {
        let source = "name: test\nruntime: yaml\n";
        let stripped = strip_jinja_blocks(source);
        assert_eq!(stripped, source);
    }

    #[test]
    fn test_strip_jinja_blocks_preserves_trailing_newline() {
        let with_newline = "name: test\n{% if x %}\nfoo\n{% endif %}\n";
        let stripped = strip_jinja_blocks(with_newline);
        assert!(stripped.ends_with('\n'));

        let without_newline = "name: test\n{% if x %}\nfoo\n{% endif %}";
        let stripped2 = strip_jinja_blocks(without_newline);
        assert!(!stripped2.ends_with('\n'));
    }

    #[test]
    fn test_strip_jinja_blocks_preserves_expressions() {
        let source = "  \"bucket{{ i }}\":\n    name: \"{{ project }}-{{ i }}\"\n";
        let stripped = strip_jinja_blocks(source);
        assert_eq!(stripped, source);
    }

    // ---- validate_jinja_syntax ----

    #[test]
    fn test_validate_jinja_syntax_valid() {
        let source = "name: {{ var }}\n{% for i in range(3) %}\n  item{{ i }}\n{% endfor %}\n";
        assert!(validate_jinja_syntax(source, "test.yaml").is_ok());
    }

    #[test]
    fn test_validate_jinja_syntax_plain_yaml() {
        let source = "name: test\nruntime: yaml\n";
        assert!(validate_jinja_syntax(source, "test.yaml").is_ok());
    }

    #[test]
    fn test_validate_jinja_syntax_unclosed_for() {
        let source = "{% for i in range(3) %}\n  item{{ i }}\n";
        let result = validate_jinja_syntax(source, "test.yaml");
        assert!(result.is_err());
        let diag = result.unwrap_err();
        assert_eq!(diag.kind, RenderErrorKind::JinjaSyntax);
    }

    #[test]
    fn test_validate_jinja_syntax_unclosed_if() {
        let source = "{% if true %}\nyes\n";
        let result = validate_jinja_syntax(source, "test.yaml");
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_jinja_syntax_invalid_expression() {
        let source = "{{ 1 + }}\n";
        let result = validate_jinja_syntax(source, "test.yaml");
        assert!(result.is_err());
    }

    // ---- Single-line {% set %} tests ----

    #[test]
    fn test_has_jinja_block_syntax_set_variable() {
        assert!(has_jinja_block_syntax("{% set x = 5 %}\nname: test\n"));
    }

    #[test]
    fn test_has_jinja_syntax_detects_expressions_blocks_comments() {
        // The render-before-gate trigger must fire for a Pulumi.yaml that
        // uses ONLY {{ }} interpolation (no {% %} blocks) — otherwise raw
        // Jinja reaches the Pulumi CLI's YAML parser and fails to unmarshal.
        assert!(has_jinja_syntax("name: {{ project }}"));
        assert!(has_jinja_syntax("{% set x = 5 %}\nname: test"));
        assert!(has_jinja_syntax("{# comment #}\nname: test"));
        // Pulumi's own ${...} interpolation is NOT Jinja.
        assert!(!has_jinja_syntax("name: ${project}"));
        assert!(!has_jinja_syntax("name: plain\nruntime: yaml"));
    }

    #[test]
    fn test_strip_jinja_blocks_set_variable() {
        let source = "{% set x = 5 %}\nname: test\nruntime: yaml\n";
        let stripped = strip_jinja_blocks(source);
        assert_eq!(stripped, "name: test\nruntime: yaml\n");
    }

    #[test]
    fn test_single_line_set_and_use() {
        let source = "{% set prefix = \"test\" %}\nname: {{ prefix }}-bucket\nruntime: yaml\n";
        let ctx = JinjaContext {
            project_name: "myproject",
            stack_name: "dev",
            cwd: "/tmp",
            organization: "",
            root_directory: "/tmp",
            config: &std::collections::HashMap::new(),
            project_dir: "/tmp",
            undefined: UndefinedMode::Strict,
            provider_templated_packages: &[],
            extra: &HashMap::new(),
        };
        let preprocessor = JinjaPreprocessor::new(&ctx);
        let result = preprocessor.preprocess(source, "Pulumi.yaml").unwrap();
        // Jinja renders {% set %} to empty, leaving a blank line at the start
        assert!(result.contains("name: test-bucket"), "got: {}", result);
        assert!(result.contains("runtime: yaml"), "got: {}", result);
    }

    #[test]
    fn test_set_with_list() {
        let source =
            "{% set envs = [\"dev\", \"staging\"] %}\nname: {{ envs[0] }}-app\nruntime: yaml\n";
        let ctx = JinjaContext {
            project_name: "myproject",
            stack_name: "dev",
            cwd: "/tmp",
            organization: "",
            root_directory: "/tmp",
            config: &std::collections::HashMap::new(),
            project_dir: "/tmp",
            undefined: UndefinedMode::Strict,
            provider_templated_packages: &[],
            extra: &HashMap::new(),
        };
        let preprocessor = JinjaPreprocessor::new(&ctx);
        let result = preprocessor.preprocess(source, "Pulumi.yaml").unwrap();
        assert!(result.contains("name: dev-app"), "got: {}", result);
        assert!(result.contains("runtime: yaml"), "got: {}", result);
    }

    #[test]
    fn test_set_not_standalone_not_detected() {
        // `{% set x = 1 %}` embedded inline in a YAML value (not standalone) should NOT
        // be detected as block syntax by has_jinja_block_syntax
        assert!(!has_jinja_block_syntax("foo {% set x = 1 %} bar\n"));
        // But standalone set IS detected
        assert!(has_jinja_block_syntax("  {% set x = 1 %}\nname: test\n"));
    }

    // ---- readFile marker helpers ----

    #[test]
    fn test_readfile_marker_format() {
        assert_eq!(readfile_marker(0), "\x00RF:0\x00");
        assert_eq!(readfile_marker(42), "\x00RF:42\x00");
    }

    #[test]
    fn test_parse_marker_id_valid() {
        assert_eq!(parse_marker_id("\x00RF:0\x00"), Some(0));
        assert_eq!(parse_marker_id("\x00RF:42\x00"), Some(42));
        assert_eq!(parse_marker_id("\x00RF:999\x00"), Some(999));
    }

    #[test]
    fn test_parse_marker_id_invalid() {
        assert_eq!(parse_marker_id("not a marker"), None);
        assert_eq!(parse_marker_id("\x00RF:abc\x00"), None);
        assert_eq!(parse_marker_id("RF:0"), None);
        assert_eq!(parse_marker_id("\x00RF:0"), None);
        assert_eq!(parse_marker_id(""), None);
    }

    #[test]
    fn test_is_single_marker_true() {
        assert!(is_single_marker("\x00RF:0\x00"));
        assert!(is_single_marker("\x00RF:42\x00"));
    }

    #[test]
    fn test_is_single_marker_false_prefix() {
        assert!(!is_single_marker("data: \x00RF:0\x00"));
    }

    #[test]
    fn test_is_single_marker_false_suffix() {
        assert!(!is_single_marker("\x00RF:0\x00 extra"));
    }

    #[test]
    fn test_leading_whitespace() {
        assert_eq!(leading_whitespace("    hello"), "    ");
        assert_eq!(leading_whitespace("hello"), "");
        assert_eq!(leading_whitespace("  \thello"), "  \t");
        assert_eq!(leading_whitespace(""), "");
    }

    #[test]
    fn test_indent_content_single_line() {
        assert_eq!(indent_content("hello", "    "), "    hello");
    }

    #[test]
    fn test_indent_content_multi_line() {
        assert_eq!(
            indent_content("line1\nline2\nline3", "  "),
            "  line1\n  line2\n  line3"
        );
    }

    #[test]
    fn test_indent_content_preserves_empty_lines() {
        assert_eq!(indent_content("line1\n\nline3", "  "), "  line1\n\n  line3");
    }

    #[test]
    fn test_indent_content_preserves_relative_indent() {
        let content = "{\n  \"key\": 1\n}";
        assert_eq!(
            indent_content(content, "    "),
            "    {\n      \"key\": 1\n    }"
        );
    }

    #[test]
    fn test_indent_content_empty() {
        assert_eq!(indent_content("", "    "), "");
    }

    #[test]
    fn test_indent_content_trailing_newline_stripped() {
        assert_eq!(indent_content("hello\n", "  "), "  hello");
    }

    #[test]
    fn test_replace_inline_markers_single() {
        let mut cache = ReadFileCache::new();
        cache.add("content".to_string());
        let line = format!("data: {}", readfile_marker(0));
        assert_eq!(replace_inline_markers(&line, &cache), "data: content");
    }

    #[test]
    fn test_replace_inline_markers_multiple() {
        let mut cache = ReadFileCache::new();
        cache.add("aaa".to_string());
        cache.add("bbb".to_string());
        let line = format!("x: {} y: {}", readfile_marker(0), readfile_marker(1));
        assert_eq!(replace_inline_markers(&line, &cache), "x: aaa y: bbb");
    }

    #[test]
    fn test_replace_inline_markers_no_marker() {
        let cache = ReadFileCache::new();
        assert_eq!(
            replace_inline_markers("no markers here", &cache),
            "no markers here"
        );
    }

    #[test]
    fn test_resolve_readfile_markers_fast_path() {
        let cache = ReadFileCache::new();
        assert!(resolve_readfile_markers("no markers here\n", &cache).is_none());
    }

    #[test]
    fn test_resolve_readfile_markers_standalone() {
        let mut cache = ReadFileCache::new();
        cache.add("line1\nline2\n".to_string());
        let input = format!("        {}\n", readfile_marker(0));
        let result = resolve_readfile_markers(&input, &cache).unwrap();
        assert_eq!(result, "        line1\n        line2\n");
    }

    #[test]
    fn test_resolve_readfile_markers_inline() {
        let mut cache = ReadFileCache::new();
        cache.add("1.2.3".to_string());
        let input = format!("version: {}\n", readfile_marker(0));
        let result = resolve_readfile_markers(&input, &cache).unwrap();
        assert_eq!(result, "version: 1.2.3\n");
    }
}

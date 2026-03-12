//! Jinja2 template pre-processing with GAT-based architecture.
//!
//! This module provides a `TemplatePreprocessor` trait with two implementations:
//! - `NoopPreprocessor`: zero-cost passthrough (returns `&'src str`)
//! - `JinjaPreprocessor`: renders Jinja2 syntax via `minijinja`, returning
//!   `Cow::Borrowed` when no Jinja syntax is detected (zero-copy fast path)

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt;
use std::path::Path;
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
    YamlSyntax,
    YamlIndentation,
    YamlDuplicateKey,
}

/// Rich diagnostic from template pre-processing.
/// `source_line` is a zero-copy slice of the original source.
pub struct RenderDiagnostic<'src> {
    pub kind: RenderErrorKind,
    pub line: u32,
    pub column: u32,
    pub source_line: &'src str,
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
            .field("message", &self.message)
            .field("suggestion", &self.suggestion)
            .finish()
    }
}

impl RenderDiagnostic<'_> {
    /// Formats as a rich error message with context for stderr output.
    pub fn format_rich(&self, filename: &str) -> String {
        let mut out = format!(
            "{}:{}:{}: error: {}",
            filename, self.line, self.column, self.message
        );
        if !self.source_line.is_empty() {
            out.push_str(&format!("\n  {} | {}", self.line, self.source_line));
        }
        if let Some(suggestion) = self.suggestion {
            out.push_str(&format!("\n  suggestion: {}", suggestion));
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Heuristic Suggestion Engine (B.3)
// ---------------------------------------------------------------------------

/// Classifies a minijinja error and returns a (kind, suggestion) pair.
fn classify_jinja_error(err: &minijinja::Error) -> (RenderErrorKind, Option<&'static str>) {
    let msg = err.to_string();
    if msg.contains("readFile:") {
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
}

/// Builds a minijinja context Value from borrowed references.
/// This is the ONLY allocation boundary — minijinja requires owned values internally.
fn build_minijinja_context(ctx: &JinjaContext<'_>) -> minijinja::Value {
    // Build config sub-object
    let config_val = build_config_value(ctx.config);
    let env_val = build_env_value();

    minijinja::context! {
        pulumi_project => ctx.project_name,
        pulumi_stack => ctx.stack_name,
        pulumi_cwd => ctx.cwd,
        pulumi_organization => ctx.organization,
        pulumi_root_directory => ctx.root_directory,
        config => config_val,
        env => env_val,
    }
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
fn find_expression_end(source: &str, start: usize) -> Option<usize> {
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
    let mut result = String::with_capacity(source.len() + 64);
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
        // Zero-copy fast path: no Jinja syntax → return borrowed reference
        if !has_jinja_syntax(source) {
            return Ok(Cow::Borrowed(source));
        }

        // Passthrough mode: pre-escape unknown expressions before rendering
        let effective_source = if self.context.undefined == UndefinedMode::Passthrough {
            pre_escape_for_passthrough(source)
        } else {
            Cow::Borrowed(source)
        };

        // Slow path: render through minijinja
        let mut env = minijinja::Environment::new();
        env.set_undefined_behavior(minijinja::UndefinedBehavior::Strict);
        register_custom_filters(&mut env);

        let cache = Arc::new(Mutex::new(ReadFileCache::new()));
        register_readfile_function(&mut env, self.context.project_dir, Arc::clone(&cache));

        env.add_template(filename, effective_source.as_ref())
            .map_err(|e| build_render_diagnostic(source, &e))?;

        let tmpl = env
            .get_template(filename)
            .map_err(|e| build_render_diagnostic(source, &e))?;

        let mj_ctx = build_minijinja_context(self.context);
        let rendered = tmpl
            .render(&mj_ctx)
            .map_err(|e| build_render_diagnostic(source, &e))?;

        // Resolve readFile markers with auto-indentation
        let final_output = match resolve_readfile_markers(&rendered, &cache.lock().unwrap()) {
            Some(resolved) => resolved,
            None => rendered,
        };

        Ok(Cow::Owned(final_output))
    }
}

/// Quick check for Jinja syntax markers (no allocation).
fn has_jinja_syntax(s: &str) -> bool {
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
        .map_err(|e| build_render_diagnostic(source, &e))?;
    Ok(())
}

/// Converts a minijinja::Error into a RenderDiagnostic with zero-copy source reference.
fn build_render_diagnostic<'src>(
    source: &'src str,
    err: &minijinja::Error,
) -> RenderDiagnostic<'src> {
    let line = err.line().unwrap_or(0) as u32;
    let source_line = source
        .lines()
        .nth(line.saturating_sub(1) as usize)
        .unwrap_or("");
    let (kind, suggestion) = classify_jinja_error(err);

    RenderDiagnostic {
        kind,
        line,
        column: 0,
        source_line,
        message: err.to_string(),
        suggestion,
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
            source_line: rendered_line,
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
    let mut result = String::new();
    for (i, line) in content.lines().enumerate() {
        if i > 0 {
            result.push('\n');
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
    for (i, line) in rendered.lines().enumerate() {
        if i > 0 {
            result.push('\n');
        }
        if !line.contains('\x00') {
            result.push_str(line);
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
    }
    if rendered.ends_with('\n') {
        result.push('\n');
    }
    Some(result)
}

/// Registers the `readFile(path)` function in the minijinja environment.
fn register_readfile_function(
    env: &mut minijinja::Environment<'_>,
    project_dir: &str,
    cache: Arc<Mutex<ReadFileCache>>,
) {
    let project_dir = project_dir.to_string();
    env.add_function(
        "readFile",
        move |path: String| -> Result<String, minijinja::Error> {
            let resolved = if Path::new(&path).is_absolute() {
                std::path::PathBuf::from(&path)
            } else {
                Path::new(&project_dir).join(&path)
            };

            let content = std::fs::read_to_string(&resolved).map_err(|e| {
                minijinja::Error::new(
                    minijinja::ErrorKind::InvalidOperation,
                    format!("readFile: failed to read '{}': {}", path, e),
                )
            })?;

            let id = cache.lock().unwrap().add(content);
            Ok(readfile_marker(id))
        },
    );
}

// ---------------------------------------------------------------------------
// Custom Jinja Filters (B.7)
// ---------------------------------------------------------------------------

fn register_custom_filters(env: &mut minijinja::Environment<'_>) {
    env.add_filter("to_json", |v: minijinja::Value| -> String {
        serde_json::to_string(&v).unwrap_or_default()
    });
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
            source_line: "{% bad %}",
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
            source_line: "name: {{ unknown }}",
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
            source_line: "",
            message: "error".to_string(),
            suggestion: None,
        };
        let formatted = diag.format_rich("test.yaml");
        // Should not contain a source line section
        assert!(!formatted.contains(" | "));
    }

    // ---- Display impl ----

    #[test]
    fn test_render_diagnostic_display() {
        let diag = RenderDiagnostic {
            kind: RenderErrorKind::JinjaSyntax,
            line: 1,
            column: 0,
            source_line: "",
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
        };
        let preprocessor = JinjaPreprocessor::new(&ctx);
        let source = "name: {{ pulumi_project }}\n";
        let result = preprocessor.preprocess(source, "test.yaml").unwrap();
        assert!(matches!(result, Cow::Owned(_)));
        assert!(result.as_ref().contains("name: myproject"));
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

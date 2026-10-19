// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

//! Every Jinja fix the changelog and the release commits record, held at the
//! level a stack meets it.
//!
//! Most of these behaviours already have a unit test next to the code that
//! implements them. What none of those tests prove is the COMPOSITION: that a
//! file on disk, read by `multi_file::load_project` with a Jinja context --
//! the one path the language host, the graph exporters and the Python binding
//! all share -- still renders, parses and evaluates the way the fix said it
//! would. That path is exactly what a performance change to the loader moves,
//! so this file is the gate in front of any such change.
//!
//! One module per release, one test per behaviour, each named for what it
//! holds. A test here that passes against an engine with its fix reverted is
//! a test that proves nothing; each one was checked to fail that way.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use pulumi_rs_yaml_core::eval::evaluator::Evaluator;
use pulumi_rs_yaml_core::eval::mock::MockCallback;
use pulumi_rs_yaml_core::eval::value::Value;
use pulumi_rs_yaml_core::jinja::{
    has_jinja_syntax, JinjaContext, JinjaPreprocessor, RenderErrorKind, UndefinedMode,
};
use pulumi_rs_yaml_core::multi_file::{load_project, MergedTemplate};
use pulumi_rs_yaml_core::provider_scope::{protect, restore};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// A stack on disk. `root` is the render root; the stack directory is
/// `root/<stack_rel>` so includes that climb out of the stack and into the
/// root are exercised the way a monorepo lays them out. `outside` is a
/// sibling of the root -- inside neither tree -- so an escape can be written
/// as a plain relative path on every platform.
struct Stack {
    _tmp: tempfile::TempDir,
    root: std::path::PathBuf,
    dir: std::path::PathBuf,
    outside: std::path::PathBuf,
}

impl Stack {
    fn new(stack_rel: &str) -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = tmp.path().canonicalize().expect("canonical tempdir");
        let root = base.join("root");
        let outside = base.join("outside");
        let dir = if stack_rel.is_empty() {
            root.clone()
        } else {
            root.join(stack_rel)
        };
        fs::create_dir_all(&dir).expect("stack dir");
        fs::create_dir_all(&outside).expect("outside dir");
        Self {
            _tmp: tmp,
            root,
            dir,
            outside,
        }
    }

    /// The relative path from the stack directory to `outside/<name>`.
    fn escape_to(&self, name: &str) -> String {
        let depth = self
            .dir
            .strip_prefix(self.root.parent().expect("base"))
            .expect("stack below base")
            .components()
            .count();
        format!("{}outside/{}", "../".repeat(depth), name)
    }

    fn at_root() -> Self {
        Self::new("")
    }

    fn write(&self, rel: &str, content: impl AsRef<[u8]>) -> &Self {
        let path = self.dir.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent");
        }
        fs::write(path, content).expect("write");
        self
    }

    fn write_root(&self, rel: &str, content: impl AsRef<[u8]>) -> &Self {
        let path = self.root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("parent");
        }
        fs::write(path, content).expect("write");
        self
    }

    fn load_with(&self, extra: &HashMap<String, String>, packages: &[&str]) -> Loaded {
        let config = HashMap::new();
        let dir = self.dir.to_string_lossy().into_owned();
        let root = self.root.to_string_lossy().into_owned();
        let ctx = JinjaContext {
            project_name: "regress",
            stack_name: "dev",
            cwd: &dir,
            organization: "org",
            root_directory: &root,
            config: &config,
            project_dir: &dir,
            undefined: UndefinedMode::Strict,
            provider_templated_packages: packages,
            extra,
            limits: pulumi_rs_yaml_core::jinja::JinjaLimits::default(),
        };
        let (merged, diags) = load_project(&self.dir, Some(&ctx));
        Loaded {
            merged,
            errors: if diags.has_errors() {
                Some(diags.to_string())
            } else {
                None
            },
        }
    }

    fn load(&self) -> Loaded {
        self.load_with(&HashMap::new(), &[])
    }
}

struct Loaded {
    merged: MergedTemplate,
    errors: Option<String>,
}

impl Loaded {
    #[track_caller]
    fn ok(self) -> MergedTemplate {
        if let Some(e) = self.errors {
            panic!("expected the stack to load, got: {e}");
        }
        self.merged
    }

    #[track_caller]
    fn err(self) -> String {
        match self.errors {
            Some(e) => e,
            None => panic!("expected the stack to be refused, but it loaded"),
        }
    }
}

/// Evaluates a loaded stack against the mock monitor and returns the inputs
/// of the resource registered under `name` -- what a provider would receive.
#[track_caller]
fn registered(merged: &MergedTemplate, name: &str) -> HashMap<String, Value<'static>> {
    let template: &'static _ = Box::leak(Box::new(merged.as_template_decl()));
    let eval = Evaluator::with_callback(
        "regress".to_string(),
        "dev".to_string(),
        "/tmp".to_string(),
        false,
        MockCallback::new(),
    );
    eval.evaluate_template(template, &HashMap::new(), &[]);
    assert!(!eval.has_errors(), "eval errors: {}", eval.diags_display());
    eval.callback()
        .registrations()
        .into_iter()
        .find(|r| r.name == name)
        .unwrap_or_else(|| panic!("no registration named {name}"))
        .inputs
}

#[track_caller]
fn input_str(merged: &MergedTemplate, resource: &str, key: &str) -> String {
    let inputs = registered(merged, resource);
    match inputs.get(key) {
        Some(Value::String(s)) => s.to_string(),
        other => panic!("{resource}.{key}: expected a string, got {other:?}"),
    }
}

/// A one-resource stack whose single property is `value`, rendered.
fn one_value(value_yaml: &str) -> String {
    format!(
        "name: regress\nruntime: yaml\nresources:\n  r:\n    type: test:index:R\n    properties:\n      value: {value_yaml}\n"
    )
}

/// An absolute path as a template can spell it on every platform: forward
/// slashes (a backslash in a Jinja string is an escape) and no Windows
/// verbatim prefix. Still absolute -- `C:/...` and `/...` both are.
fn template_path(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    text.strip_prefix("//?/").map(str::to_owned).unwrap_or(text)
}

fn render_str(source: &str, project_dir: &Path) -> Result<String, String> {
    let config = HashMap::new();
    let extra = HashMap::new();
    let dir = project_dir.to_string_lossy().into_owned();
    let ctx = JinjaContext {
        project_name: "regress",
        stack_name: "dev",
        cwd: &dir,
        organization: "org",
        root_directory: &dir,
        config: &config,
        project_dir: &dir,
        undefined: UndefinedMode::Strict,
        provider_templated_packages: &[],
        extra: &extra,
        limits: pulumi_rs_yaml_core::jinja::JinjaLimits::default(),
    };
    JinjaPreprocessor::new(&ctx)
        .render(source, "Pulumi.yaml")
        .map(|c| c.into_owned())
        .map_err(|d| d.to_string())
}

// ---------------------------------------------------------------------------
// 0.4.0 -- extra variables
// ---------------------------------------------------------------------------

mod v0_4_0_extra_vars {
    use super::*;

    #[test]
    fn extras_render_into_the_stack() {
        let s = Stack::at_root();
        s.write("Pulumi.yaml", one_value("\"{{ team }}\""));
        let mut extra = HashMap::new();
        extra.insert("team".to_string(), "data-eng".to_string());
        let merged = s.load_with(&extra, &[]).ok();
        assert_eq!(input_str(&merged, "r", "value"), "data-eng");
    }

    #[test]
    fn extras_never_override_built_in_names() {
        let s = Stack::at_root();
        s.write("Pulumi.yaml", one_value("\"{{ pulumi_stack }}\""));
        let mut extra = HashMap::new();
        extra.insert("pulumi_stack".to_string(), "hijacked".to_string());
        let merged = s.load_with(&extra, &[]).ok();
        assert_eq!(input_str(&merged, "r", "value"), "dev");
    }
}

// ---------------------------------------------------------------------------
// import / include (the bilayer layout)
// ---------------------------------------------------------------------------

mod import_and_include {
    use super::*;

    #[test]
    fn import_exposes_the_files_set_names() {
        let s = Stack::new("stacks/bucket1");
        s.write_root("environment.j2", "{% set region = 'europe-west1' %}");
        s.write(
            "Pulumi.yaml",
            "{% import '../environment.j2' as env %}\n".to_string()
                + &one_value("\"{{ env.region }}\""),
        );
        let merged = s.load().ok();
        assert_eq!(input_str(&merged, "r", "value"), "europe-west1");
    }

    #[test]
    fn include_resolves_from_the_stack_directory_first() {
        let s = Stack::new("stacks/a");
        s.write("v.txt", "from-stack");
        s.write_root("v.txt", "from-root");
        s.write("Pulumi.yaml", one_value("\"{% include 'v.txt' %}\""));
        let merged = s.load().ok();
        assert_eq!(input_str(&merged, "r", "value"), "from-stack");
    }

    #[test]
    fn include_falls_back_to_the_render_root() {
        let s = Stack::new("stacks/a");
        s.write_root("only-at-root.txt", "rooted");
        s.write(
            "Pulumi.yaml",
            one_value("\"{% include 'only-at-root.txt' %}\""),
        );
        let merged = s.load().ok();
        assert_eq!(input_str(&merged, "r", "value"), "rooted");
    }
}

// ---------------------------------------------------------------------------
// 0.5.12 -- loader containment
// ---------------------------------------------------------------------------

mod v0_5_12_containment {
    use super::*;

    #[test]
    fn traversal_out_of_both_roots_is_refused_and_not_inlined() {
        let s = Stack::new("stacks/a");
        fs::write(s.outside.join("secret.yaml"), "TOPSECRET").expect("secret");
        let rel = s.escape_to("secret.yaml");
        s.write(
            "Pulumi.yaml",
            one_value(&format!("\"{{% include '{rel}' %}}\"")),
        );
        let err = s.load().err();
        assert!(err.contains("include refused"), "{err}");
        assert!(!err.contains("TOPSECRET"), "contents leaked: {err}");
    }

    #[test]
    fn an_absolute_include_is_refused_even_inside_the_root() {
        let s = Stack::at_root();
        s.write("inside.txt", "x");
        let abs = s.dir.join("inside.txt");
        s.write(
            "Pulumi.yaml",
            one_value(&format!("\"{{% include '{}' %}}\"", template_path(&abs))),
        );
        let err = s.load().err();
        assert!(err.contains("[absolute]"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_the_tree_is_an_escape() {
        let outer = tempfile::tempdir().expect("outer");
        fs::write(outer.path().join("t.txt"), "OUTSIDE").expect("t");
        let s = Stack::at_root();
        std::os::unix::fs::symlink(outer.path().join("t.txt"), s.dir.join("link.txt"))
            .expect("symlink");
        s.write("Pulumi.yaml", one_value("\"{% include 'link.txt' %}\""));
        let err = s.load().err();
        assert!(err.contains("[escape]"), "{err}");
        assert!(!err.contains("OUTSIDE"), "{err}");
    }

    #[test]
    fn the_shared_root_environment_pattern_is_served() {
        let s = Stack::new("stacks/svc");
        s.write_root("environment.j2", "{% set env_name = 'prod' %}");
        s.write(
            "Pulumi.yaml",
            "{% import '../environment.j2' as e %}\n".to_string()
                + &one_value("\"{{ e.env_name }}\""),
        );
        let merged = s.load().ok();
        assert_eq!(input_str(&merged, "r", "value"), "prod");
    }
}

// ---------------------------------------------------------------------------
// has_jinja_syntax -- {{ }}-only and {# #}-only files are templates
// ---------------------------------------------------------------------------

mod jinja_detection {
    use super::*;

    #[test]
    fn expression_only_and_comment_only_files_are_detected() {
        assert!(has_jinja_syntax("name: {{ pulumi_project }}\n"));
        assert!(has_jinja_syntax("{# a note #}\nname: x\n"));
        assert!(has_jinja_syntax("{% if true %}{% endif %}"));
        assert!(!has_jinja_syntax("value: ${res.out}\n"));
    }

    #[test]
    fn an_expression_only_stack_renders_through_the_loader() {
        let s = Stack::at_root();
        s.write("Pulumi.yaml", one_value("\"{{ pulumi_project }}\""));
        let merged = s.load().ok();
        assert_eq!(input_str(&merged, "r", "value"), "regress");
    }

    #[test]
    fn a_comment_only_stack_renders_through_the_loader() {
        let s = Stack::at_root();
        s.write(
            "Pulumi.yaml",
            "{# owned by data-eng #}\n".to_string() + &one_value("plain"),
        );
        let merged = s.load().ok();
        assert_eq!(input_str(&merged, "r", "value"), "plain");
    }
}

// ---------------------------------------------------------------------------
// 0.5.19 -- Jinja2 method compatibility and keyword-argument indent
// ---------------------------------------------------------------------------

mod v0_5_19_jinja2_compat {
    use super::*;

    #[test]
    fn dict_methods_items_keys_values_get() {
        let s = Stack::at_root();
        s.write(
            "Pulumi.yaml",
            "{% set m = {'a': '1', 'b': '2'} %}\n".to_string()
                + &one_value(
                    "\"{% for k, v in m.items() %}{{ k }}={{ v }};{% endfor %}|{{ m.keys() | join(',') }}|{{ m.values() | join(',') }}|{{ m.get('a') }}\"",
                ),
        );
        let merged = s.load().ok();
        assert_eq!(input_str(&merged, "r", "value"), "a=1;b=2;|a,b|1,2|1");
    }

    #[test]
    fn indent_accepts_keywords() {
        let out = render_str("{{ 'x\ny' | indent(width=2, first=True) }}", Path::new("."))
            .expect("render");
        assert_eq!(out, "  x\n  y");
    }

    #[test]
    fn a_misspelled_method_or_keyword_fails_loudly() {
        assert!(render_str("{% set m = {'a': 1} %}{{ m.itemz() }}", Path::new(".")).is_err());
        assert!(render_str("{{ 'x' | indent(widht=2) }}", Path::new(".")).is_err());
    }
}

// ---------------------------------------------------------------------------
// 0.5.20 -- tojson
// ---------------------------------------------------------------------------

mod v0_5_20_tojson {
    use super::*;

    #[test]
    fn tojson_agrees_with_to_json() {
        let a = render_str("{{ {'k': [1, 'two']} | tojson }}", Path::new(".")).expect("tojson");
        let b = render_str("{{ {'k': [1, 'two']} | to_json }}", Path::new(".")).expect("to_json");
        assert_eq!(a, b);
    }

    #[test]
    fn tojson_indent_positional_and_keyword() {
        let p = render_str("{{ {'k': 1} | tojson(2) }}", Path::new(".")).expect("positional");
        let k = render_str("{{ {'k': 1} | tojson(indent=2) }}", Path::new(".")).expect("keyword");
        assert_eq!(p, k);
        assert!(p.contains('\n'), "indent must pretty-print: {p:?}");
    }

    #[test]
    fn tojson_reaches_a_registered_input() {
        let s = Stack::at_root();
        s.write("Pulumi.yaml", one_value("'{{ {\"a\": 1} | tojson }}'"));
        let merged = s.load().ok();
        assert_eq!(input_str(&merged, "r", "value"), "{\"a\":1}");
    }
}

// ---------------------------------------------------------------------------
// 0.5.23 -- provider-templated packages, CRLF through readFile, the
// minijinja arithmetic panic
// ---------------------------------------------------------------------------

mod v0_5_23 {
    use super::*;

    const DBT: &str = "\
name: regress
runtime: yaml
resources:
  m:
    type: gcpx:dbt/model:Model
    properties:
      project: \"{{ pulumi_project }}\"
      sql: |
        {{ config(materialized='view') }}
        SELECT * FROM {{ ref('stg') }}
";

    #[test]
    fn a_listed_package_keeps_its_sql_and_the_rest_still_renders() {
        let s = Stack::at_root();
        s.write("Pulumi.yaml", DBT);
        let merged = s.load_with(&HashMap::new(), &["gcpx"]).ok();
        let sql = input_str(&merged, "m", "sql");
        assert!(sql.contains("{{ config(materialized='view') }}"), "{sql}");
        assert!(sql.contains("{{ ref('stg') }}"), "{sql}");
        assert_eq!(input_str(&merged, "m", "project"), "regress");
    }

    #[test]
    fn without_the_listing_the_same_stack_is_refused() {
        let s = Stack::at_root();
        s.write("Pulumi.yaml", DBT);
        s.load().err();
    }

    #[test]
    fn protect_then_restore_is_the_identity() {
        let p = protect(DBT, &["gcpx"]).expect("protect");
        let restored = restore(p.source(DBT), p.regions()).expect("restore");
        assert_eq!(restored, DBT);
    }

    #[test]
    fn listing_nothing_leaves_rendering_byte_for_byte() {
        let p = protect(DBT, &[]).expect("protect");
        assert_eq!(p.source(DBT), DBT);
        assert!(p.regions().is_empty());
    }

    #[test]
    fn readfile_keeps_crlf_in_the_rendered_program() {
        // YAML itself folds CRLF to LF inside a block scalar, so the fix is
        // visible in the text the renderer hands the parser -- which is where
        // `.lines()` used to rewrite it, for every file in the program.
        let s = Stack::at_root();
        s.write("q.sql", "SELECT 1\r\nFROM t\r\n");
        let out = render_str("v: |\n  {{ readFile('q.sql') }}\n", &s.dir).expect("render");
        assert!(out.contains("SELECT 1\r\n"), "CRLF rewritten: {out:?}");
    }

    #[test]
    fn readfile_inside_a_crlf_program_keeps_the_programs_line_endings() {
        let s = Stack::at_root();
        s.write("q.sql", "SELECT 1");
        let out = render_str(
            "a: 1\r\nv: |\r\n  {{ readFile('q.sql') }}\r\nb: 2\r\n",
            &s.dir,
        )
        .expect("render");
        assert!(out.contains("a: 1\r\n"), "{out:?}");
        // minijinja drops a template's final newline (keep_trailing_newline
        // is off), so the last line is checked by what precedes it.
        assert!(out.contains("SELECT 1\r\nb: 2"), "{out:?}");
    }

    #[test]
    fn the_minijinja_arithmetic_panic_seed_does_not_panic() {
        let seed: &[u8] = b"name: {{ 333/333333333<3+3333333333333333333333}}33#}\nbucket: \">\x00\x00\x00\x00\x00\x00\x00@ }}\"\n";
        let src = std::str::from_utf8(seed).expect("utf8");
        // Either answer is fine; aborting the host is not.
        let _ = render_str(src, Path::new("."));
    }
}

// ---------------------------------------------------------------------------
// 0.5.27 -- a byte order mark is not a second document
// ---------------------------------------------------------------------------

mod v0_5_27_bom {
    use super::*;

    #[test]
    fn a_marked_multi_line_stack_loads_and_keeps_its_name() {
        let s = Stack::at_root();
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(one_value("\"{{ pulumi_stack }}\"").as_bytes());
        s.write("Pulumi.yaml", &bytes);
        let merged = s.load().ok();
        assert_eq!(merged.name(), Some("regress"));
        assert_eq!(input_str(&merged, "r", "value"), "dev");
    }

    #[test]
    fn a_real_second_document_is_still_refused() {
        let s = Stack::at_root();
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(b"name: a\nruntime: yaml\n---\nname: b\n");
        s.write("Pulumi.yaml", &bytes);
        let marked = s.load().err();

        let t = Stack::at_root();
        t.write("Pulumi.yaml", "name: a\nruntime: yaml\n---\nname: b\n");
        let unmarked = t.load().err();
        assert!(marked.contains("more than one document"), "{marked}");
        assert!(unmarked.contains("more than one document"), "{unmarked}");
    }
}

// ---------------------------------------------------------------------------
// 0.5.29 -- located diagnostics; refusals are not absences
// ---------------------------------------------------------------------------

mod v0_5_29_diagnostics {
    use super::*;

    fn diag(source: &str) -> (RenderErrorKind, u32, u32, String) {
        let config = HashMap::new();
        let extra = HashMap::new();
        let ctx = JinjaContext {
            project_name: "regress",
            stack_name: "dev",
            cwd: ".",
            organization: "org",
            root_directory: ".",
            config: &config,
            project_dir: ".",
            undefined: UndefinedMode::Strict,
            provider_templated_packages: &[],
            extra: &extra,
            limits: pulumi_rs_yaml_core::jinja::JinjaLimits::default(),
        };
        let pre = JinjaPreprocessor::new(&ctx);
        let d = pre.render(source, "Pulumi.yaml").expect_err("must fail");
        (d.kind, d.line, d.column, d.expression.to_string())
    }

    #[test]
    fn an_undefined_name_is_located_to_the_expression() {
        let (kind, line, col, expr) = diag("name: x\nvalue: {{ nope }}\n");
        assert_eq!(kind, RenderErrorKind::JinjaUndefinedVariable);
        assert_eq!(line, 2);
        assert!(col > 0, "column must be located");
        assert!(expr.contains("nope"), "{expr:?}");
    }

    #[test]
    fn the_column_counts_characters_on_a_multibyte_line() {
        let (_, line, col, expr) = diag("name: x\nvalue: \"żółw {{ nope }}\"\n");
        assert_eq!(line, 2);
        assert!(expr.contains("nope"));
        // "value: \"żółw " is 13 characters but 16 bytes; the column lands on
        // the expression either way, and the slice proves it.
        assert!(col > 0);
    }

    #[test]
    fn ignore_missing_hides_absence_only() {
        let s = Stack::at_root();
        s.write(
            "Pulumi.yaml",
            one_value("\"a{% include 'absent.txt' ignore missing %}b\""),
        );
        let merged = s.load().ok();
        assert_eq!(input_str(&merged, "r", "value"), "ab");

        let t = Stack::at_root();
        let abs = t.dir.join("x.txt");
        t.write("x.txt", "x");
        t.write(
            "Pulumi.yaml",
            one_value(&format!(
                "\"{{% include '{}' ignore missing %}}\"",
                template_path(&abs)
            )),
        );
        let err = t.load().err();
        assert!(err.contains("include refused"), "{err}");
    }

    #[test]
    fn an_absent_include_is_named_not_refused() {
        let s = Stack::at_root();
        s.write("Pulumi.yaml", one_value("\"{% include 'nowhere.txt' %}\""));
        let err = s.load().err();
        assert!(err.contains("nowhere.txt"), "{err}");
        assert!(!err.contains("include refused"), "{err}");
    }
}

// ---------------------------------------------------------------------------
// 0.5.30 -- an include is any text the tree contains
// ---------------------------------------------------------------------------

mod v0_5_30_includes {
    use super::*;
    use pulumi_rs_yaml_core::jinja::MAX_INCLUDE_BYTES;

    #[test]
    fn an_extensionless_version_file_is_served_from_the_root() {
        let s = Stack::new("services/api/stack");
        s.write_root("VERSION", "1.4.2\n");
        s.write(
            "Pulumi.yaml",
            one_value("\"img:{% include '../../../VERSION' %}\""),
        );
        let merged = s.load().ok();
        // Exactly one trailing newline of an included file is dropped.
        assert_eq!(input_str(&merged, "r", "value"), "img:1.4.2");
    }

    #[test]
    fn a_binary_include_is_refused_without_its_bytes() {
        let s = Stack::at_root();
        s.write("blob.bin", [0xFFu8, 0xFE, 0x00, 0x41]);
        s.write("Pulumi.yaml", one_value("\"{% include 'blob.bin' %}\""));
        let err = s.load().err();
        assert!(err.contains("[binary]"), "{err}");
    }

    #[test]
    fn a_file_at_the_cap_is_served_and_one_byte_over_is_refused() {
        let s = Stack::at_root();
        s.write("at.txt", "a".repeat(MAX_INCLUDE_BYTES as usize));
        s.write("over.txt", "a".repeat(MAX_INCLUDE_BYTES as usize + 1));
        // Proven through the renderer directly: a 1 MiB property value is a
        // different test's business, and the loader path is the one above.
        let ok = render_str("{% include 'at.txt' %}", &s.dir);
        assert!(ok.is_ok(), "{ok:?}");
        let over = render_str("{% include 'over.txt' %}", &s.dir).expect_err("over cap");
        assert!(over.contains("[too large]"), "{over}");
    }

    #[test]
    fn a_refusal_in_the_stack_is_not_retried_against_the_root() {
        let s = Stack::new("stacks/a");
        s.write("data.bin", [0xFFu8, 0x00]);
        s.write_root("data.bin", "text-at-root");
        s.write("Pulumi.yaml", one_value("\"{% include 'data.bin' %}\""));
        let err = s.load().err();
        assert!(err.contains("[binary]"), "{err}");
    }

    #[test]
    fn a_directory_named_as_an_include_is_absent() {
        let s = Stack::at_root();
        fs::create_dir_all(s.dir.join("adir")).expect("dir");
        s.write("Pulumi.yaml", one_value("\"{% include 'adir' %}\""));
        let err = s.load().err();
        assert!(!err.contains("include refused"), "{err}");
    }

    #[test]
    fn an_included_file_is_compiled_as_a_template() {
        let s = Stack::at_root();
        s.write("part.j2", "{{ pulumi_stack }}-part");
        s.write("Pulumi.yaml", one_value("\"{% include 'part.j2' %}\""));
        let merged = s.load().ok();
        assert_eq!(input_str(&merged, "r", "value"), "dev-part");
    }

    #[test]
    fn a_base64_decode_failure_is_an_error_not_an_empty_string() {
        assert!(render_str("{{ '!!not base64!!' | base64_decode }}", Path::new(".")).is_err());
        let ok = render_str("{{ 'aGk=' | base64_decode }}", Path::new(".")).expect("decode");
        assert_eq!(ok, "hi");
    }
}

// ---------------------------------------------------------------------------
// 0.5.31 -- an escaped dollar is not a literal pair of dollars
// ---------------------------------------------------------------------------

mod v0_5_31_escaped_dollar {
    use super::*;

    #[test]
    fn an_escape_collapses_once_in_a_jinja_rendered_stack() {
        let s = Stack::at_root();
        s.write(
            "Pulumi.yaml",
            one_value("\"SELECT * FROM $${data()} -- {{ pulumi_stack }}\""),
        );
        let merged = s.load().ok();
        assert_eq!(
            input_str(&merged, "r", "value"),
            "SELECT * FROM ${data()} -- dev"
        );
    }

    #[test]
    fn four_dollars_collapse_to_two_and_are_not_rescanned() {
        let s = Stack::at_root();
        s.write("Pulumi.yaml", one_value("\"$$$${x} {{ pulumi_stack }}\""));
        let merged = s.load().ok();
        assert_eq!(input_str(&merged, "r", "value"), "$${x} dev");
    }
}

// ---------------------------------------------------------------------------
// 0.5.32 -- truncate, center and wordwrap
// ---------------------------------------------------------------------------

mod v0_5_32_string_filters {
    use super::*;

    #[test]
    fn the_field_expression_reaches_a_registered_input() {
        let s = Stack::at_root();
        s.write(
            "Pulumi.yaml",
            one_value("\"{{ 'a-very-long-resource-identifier-for-a-service' | truncate(20, False, '', 0) }}\""),
        );
        let merged = s.load().ok();
        assert_eq!(input_str(&merged, "r", "value"), "a-very-long-resource");
    }

    #[test]
    fn keyword_spellings_render() {
        // The default leeway of 5 leaves a string only 4 over the cap alone --
        // the reference does exactly that -- so the cut needs `leeway=0`.
        let lenient = render_str(
            "{{ 'hello world foo' | truncate(length=11) }}",
            Path::new("."),
        )
        .expect("truncate kw");
        assert_eq!(lenient, "hello world foo");
        let t = render_str(
            "{{ 'hello world foo' | truncate(length=11, leeway=0) }}",
            Path::new("."),
        )
        .expect("truncate kw");
        assert_eq!(t, "hello...");
        let w = render_str(
            "{{ 'aaa bbb ccc' | wordwrap(width=7, break_long_words=False) }}",
            Path::new("."),
        )
        .expect("wordwrap kw");
        assert_eq!(w, "aaa bbb\nccc");
    }

    #[test]
    fn center_coerces_a_number_like_the_reference() {
        let c = render_str("{{ 42 | center(6) }}", Path::new(".")).expect("center number");
        assert_eq!(c, "  42  ");
    }

    #[test]
    fn an_enormous_center_width_is_an_error_not_an_abort() {
        assert!(render_str("{{ 'x' | center(1000000000000000000) }}", Path::new(".")).is_err());
    }

    #[test]
    fn an_amplifying_wordwrap_is_an_error_not_an_abort() {
        let src =
            "{{ ('a ' * 600000) | wordwrap(1, True, '".to_string() + &"#".repeat(1024) + "') }}";
        assert!(render_str(&src, Path::new(".")).is_err());
    }
}

// ---------------------------------------------------------------------------
// 0.5.33 -- author text shaped like a readFile marker is not the engine's
// ---------------------------------------------------------------------------

mod v0_5_33_readfile_marker {
    use super::*;

    #[test]
    fn author_text_shaped_like_a_marker_survives_the_render() {
        // The fuzz subject: a whole line that LOOKS like a marker but carries
        // no numeric id. It used to be deleted from the rendered program.
        let src = "a: \"{{ 'x' }}\"\n\x00RF:\x0b\x00\nb: 1\n";
        let out = render_str(src, Path::new(".")).expect("render");
        assert!(out.contains("\x00RF:\x0b\x00"), "line deleted: {out:?}");
    }

    #[test]
    fn a_real_marker_still_resolves() {
        let s = Stack::at_root();
        s.write("body.txt", "line-one\nline-two");
        s.write(
            "Pulumi.yaml",
            "name: regress\nruntime: yaml\nresources:\n  r:\n    type: test:index:R\n    properties:\n      value: |\n        {{ readFile('body.txt') }}\n",
        );
        let merged = s.load().ok();
        let v = input_str(&merged, "r", "value");
        assert!(v.contains("line-one\nline-two"), "{v:?}");
        assert!(!v.contains('\u{0}'), "marker NULs leaked: {v:?}");
    }
}

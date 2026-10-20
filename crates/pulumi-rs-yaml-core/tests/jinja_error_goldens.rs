// Author: spicyzhug@gmail.com
// Copyright: lituus-io, all rights reserved

//! The exact text of every render error kind, as an author reads it.
//!
//! Each case renders a small project (with includes on disk where the kind
//! needs them) and compares `format_rich` to a golden under
//! `tests/fixtures/jinja_errors/`. `JINJA_ERROR_BLESS=1` rewrites them.
//! Every `RenderErrorKind` must have at least one case; the list below is
//! checked against the enum by an exhaustive match, so a new kind without a
//! golden does not compile.

use pulumi_rs_yaml_core::jinja::{
    validate_rendered_yaml, JinjaContext, JinjaLimits, JinjaPreprocessor, RenderDiagnostic,
    RenderErrorKind, UndefinedMode,
};
use std::collections::HashMap;
use std::path::Path;

struct Case {
    name: &'static str,
    kind: RenderErrorKind,
    source: &'static str,
    files: &'static [(&'static str, &'static str)],
    limits: fn() -> JinjaLimits,
    /// Rendered output fed to the YAML gate instead (the YAML kinds).
    yaml_gate: bool,
}

fn defaults() -> JinjaLimits {
    JinjaLimits::default()
}

fn tiny() -> JinjaLimits {
    JinjaLimits {
        fuel: 200,
        recursion: 40,
        output_bytes: 64,
        range: 10,
    }
}

/// Forces a compile-time check that `CASES` names every kind.
fn kind_is_covered(kind: RenderErrorKind) -> bool {
    match kind {
        RenderErrorKind::JinjaSyntax
        | RenderErrorKind::JinjaUndefinedVariable
        | RenderErrorKind::JinjaFilterError
        | RenderErrorKind::JinjaTypeError
        | RenderErrorKind::JinjaTemplateNotFound
        | RenderErrorKind::YamlSyntax
        | RenderErrorKind::YamlIndentation
        | RenderErrorKind::YamlDuplicateKey
        | RenderErrorKind::ProviderScope
        | RenderErrorKind::FuelExhausted
        | RenderErrorKind::RecursionLimit
        | RenderErrorKind::OutputTooLarge
        | RenderErrorKind::IncludeCycle
        | RenderErrorKind::RangeTooLarge => CASES.iter().any(|c| c.kind == kind),
    }
}

const ALL_KINDS: [RenderErrorKind; 14] = [
    RenderErrorKind::JinjaSyntax,
    RenderErrorKind::JinjaUndefinedVariable,
    RenderErrorKind::JinjaFilterError,
    RenderErrorKind::JinjaTypeError,
    RenderErrorKind::JinjaTemplateNotFound,
    RenderErrorKind::YamlSyntax,
    RenderErrorKind::YamlIndentation,
    RenderErrorKind::YamlDuplicateKey,
    RenderErrorKind::ProviderScope,
    RenderErrorKind::FuelExhausted,
    RenderErrorKind::RecursionLimit,
    RenderErrorKind::OutputTooLarge,
    RenderErrorKind::IncludeCycle,
    RenderErrorKind::RangeTooLarge,
];

const CASES: &[Case] = &[
    Case {
        name: "syntax_unclosed_block",
        kind: RenderErrorKind::JinjaSyntax,
        source: "name: app\n{% if true %}\nx: 1\n",
        files: &[],
        limits: defaults,
        yaml_gate: false,
    },
    Case {
        name: "syntax_unknown_filter",
        kind: RenderErrorKind::JinjaSyntax,
        source: "name: app\nregion: {{ 'a' | nosuchfilter }}\n",
        files: &[],
        limits: defaults,
        yaml_gate: false,
    },
    Case {
        name: "undefined_names_the_expression",
        kind: RenderErrorKind::JinjaUndefinedVariable,
        source: "name: app\nregion: {{ settings.region }}\n",
        files: &[],
        limits: defaults,
        yaml_gate: false,
    },
    Case {
        name: "undefined_inside_an_include",
        kind: RenderErrorKind::JinjaUndefinedVariable,
        source: "name: app\nlabels:\n{% include 'inc/labels.j2' %}\n",
        files: &[("inc/labels.j2", "  team: data\n  owner: {{ owner }}\n")],
        limits: defaults,
        yaml_gate: false,
    },
    Case {
        name: "error_two_includes_deep",
        kind: RenderErrorKind::JinjaTypeError,
        source: "name: app\n{% include 'inc/outer.j2' %}\n",
        files: &[
            ("inc/outer.j2", "# outer\n{% include 'inc/inner.j2' %}"),
            ("inc/inner.j2", "a: 1\nb: {{ 'x'.upper(1) }}\n"),
        ],
        limits: defaults,
        yaml_gate: false,
    },
    Case {
        name: "error_inside_an_imported_macro",
        kind: RenderErrorKind::JinjaFilterError,
        source: "{% import 'inc/lib.j2' as lib %}\nname: app\nx: {{ lib.bad() }}\n",
        files: &[("inc/lib.j2", "{% macro bad() %}\n{{ 1 + 'a' }}\n{% endmacro %}")],
        limits: defaults,
        yaml_gate: false,
    },
    Case {
        name: "filter_error",
        kind: RenderErrorKind::JinjaFilterError,
        source: "name: app\nv: {{ readFile('/etc/hostname') }}\n",
        files: &[],
        limits: defaults,
        yaml_gate: false,
    },
    Case {
        name: "type_error_argument_count",
        kind: RenderErrorKind::JinjaTypeError,
        source: "name: app\n{% macro m(a) %}{{ a }}{% endmacro %}\nv: {{ m(1, 2) }}\n",
        files: &[],
        limits: defaults,
        yaml_gate: false,
    },
    Case {
        name: "template_not_found",
        kind: RenderErrorKind::JinjaTemplateNotFound,
        source: "name: app\n{% include 'inc/missing.j2' %}\n",
        files: &[],
        limits: defaults,
        yaml_gate: false,
    },
    Case {
        name: "include_refused_escape",
        kind: RenderErrorKind::JinjaTemplateNotFound,
        source: "name: app\n{% include '../outside.j2' %}\n",
        files: &[("../outside.j2", "secret")],
        limits: defaults,
        yaml_gate: false,
    },
    Case {
        name: "yaml_syntax",
        kind: RenderErrorKind::YamlSyntax,
        source: "name: app\nkey:value: [\n",
        files: &[],
        limits: defaults,
        yaml_gate: true,
    },
    Case {
        name: "yaml_indentation",
        kind: RenderErrorKind::YamlIndentation,
        source: "name: app\nlist: - b\n",
        files: &[],
        limits: defaults,
        yaml_gate: true,
    },
    Case {
        name: "yaml_duplicate_key",
        kind: RenderErrorKind::YamlDuplicateKey,
        source: "name: app\nname: other\n",
        files: &[],
        limits: defaults,
        yaml_gate: true,
    },
    Case {
        name: "provider_scope",
        kind: RenderErrorKind::ProviderScope,
        source: "name: app\nruntime:\n  name: yaml\n  options:\n    providerTemplatedPackages: [dbt]\nresources:\n  m:\n    type: dbt:index:Model\n    properties:\n      sql: |\n\tselect {{ ref('x') }}\n",
        files: &[],
        limits: defaults,
        yaml_gate: false,
    },
    Case {
        name: "fuel_exhausted",
        kind: RenderErrorKind::FuelExhausted,
        source: "name: app\n{% for i in range(10) %}{% for j in range(10) %}{{ i }}{% endfor %}{% endfor %}\n",
        files: &[],
        limits: tiny,
        yaml_gate: false,
    },
    Case {
        name: "recursion_limit",
        kind: RenderErrorKind::RecursionLimit,
        source: "name: app\n{% macro r(n) %}{{ r(n + 1) }}{% endmacro %}\nv: {{ r(0) }}\n",
        files: &[],
        limits: tiny,
        yaml_gate: false,
    },
    Case {
        name: "output_too_large",
        kind: RenderErrorKind::OutputTooLarge,
        source: "name: app\nv: {{ 'x' * 100 }}\n",
        files: &[],
        limits: tiny,
        yaml_gate: false,
    },
    Case {
        name: "include_cycle",
        kind: RenderErrorKind::IncludeCycle,
        source: "name: app\n{% include 'inc/a.j2' %}\n",
        files: &[
            ("inc/a.j2", "{% include 'inc/b.j2' %}"),
            ("inc/b.j2", "{% include 'inc/a.j2' %}"),
        ],
        limits: defaults,
        yaml_gate: false,
    },
    Case {
        name: "range_too_large",
        kind: RenderErrorKind::RangeTooLarge,
        source: "name: app\nn: {{ range(11) | length }}\n",
        files: &[],
        limits: tiny,
        yaml_gate: false,
    },
];

fn goldens() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/jinja_errors")
}

fn run(case: &Case, dir: &Path) -> Result<String, (RenderErrorKind, String)> {
    let stack = dir.join("stack");
    std::fs::create_dir_all(&stack).unwrap();
    for (name, body) in case.files {
        let path = stack.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }
    let d = stack.to_string_lossy();
    let config = HashMap::new();
    let extra = HashMap::new();
    let scoped = pulumi_rs_yaml_core::provider_scope::effective_packages(case.source);
    let scoped: Vec<&str> = scoped.iter().map(String::as_str).collect();
    let ctx = JinjaContext {
        project_name: "app",
        stack_name: "dev",
        cwd: &d,
        organization: "org",
        root_directory: &d,
        config: &config,
        project_dir: &d,
        undefined: UndefinedMode::Strict,
        provider_templated_packages: &scoped,
        extra: &extra,
        limits: (case.limits)(),
    };
    let report = |diag: RenderDiagnostic<'_>| (diag.kind, diag.format_rich("Pulumi.yaml"));
    if case.yaml_gate {
        return validate_rendered_yaml(case.source, case.source, "Pulumi.yaml")
            .map(|()| String::new())
            .map_err(report);
    }
    JinjaPreprocessor::new(&ctx)
        .render(case.source, "Pulumi.yaml")
        .map(|c| c.into_owned())
        .map_err(report)
}

#[test]
fn every_error_kind_reads_exactly_as_its_golden() {
    for kind in ALL_KINDS {
        assert!(kind_is_covered(kind), "{kind:?} has no golden case");
    }
    let bless = std::env::var_os("JINJA_ERROR_BLESS").is_some();
    let mut failures = Vec::new();
    for case in CASES {
        let tmp = tempfile::tempdir().unwrap();
        let (kind, text) = match run(case, tmp.path()) {
            Ok(out) => {
                failures.push(format!(
                    "{}: rendered instead of failing: {out:?}",
                    case.name
                ));
                continue;
            }
            Err(e) => e,
        };
        assert!(
            !text.contains(&*tmp.path().to_string_lossy()),
            "{}: a temp path leaked into the message",
            case.name
        );
        if kind != case.kind {
            failures.push(format!(
                "{}: kind {kind:?}, expected {:?}\n{text}",
                case.name, case.kind
            ));
        }
        let path = goldens().join(format!("{}.txt", case.name));
        if bless {
            std::fs::create_dir_all(goldens()).unwrap();
            std::fs::write(&path, format!("{text}\n")).unwrap();
        } else {
            let want = std::fs::read_to_string(&path).unwrap_or_default();
            if want.trim_end_matches('\n') != text {
                failures.push(format!(
                    "{}: message drifted\n--- golden\n{want}--- now\n{text}\n",
                    case.name
                ));
            }
        }
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}

#[test]
fn an_error_inside_an_include_carries_file_line_column_and_chain() {
    let tmp = tempfile::tempdir().unwrap();
    let case = CASES
        .iter()
        .find(|c| c.name == "error_two_includes_deep")
        .unwrap();
    let stack = tmp.path().join("stack");
    std::fs::create_dir_all(stack.join("inc")).unwrap();
    for (name, body) in case.files {
        std::fs::write(stack.join(name), body).unwrap();
    }
    let d = stack.to_string_lossy();
    let (config, extra) = (HashMap::new(), HashMap::new());
    let ctx = JinjaContext {
        project_name: "app",
        stack_name: "dev",
        cwd: &d,
        organization: "org",
        root_directory: &d,
        config: &config,
        project_dir: &d,
        undefined: UndefinedMode::Strict,
        provider_templated_packages: &[],
        extra: &extra,
        limits: JinjaLimits::default(),
    };
    let diag = JinjaPreprocessor::new(&ctx)
        .render(case.source, "Pulumi.yaml")
        .unwrap_err();
    // This file: the include statement.
    assert_eq!((diag.line, diag.column), (2, 4));
    let origin = diag
        .origin
        .as_deref()
        .expect("an origin inside the include");
    assert_eq!(origin.file, "inc/inner.j2");
    assert_eq!(origin.line, 2);
    assert!(origin.column > 0, "{origin:?}");
    assert_eq!(
        &origin.source_line[(origin.column - 1) as usize..(origin.end_column - 1) as usize],
        origin.expression
    );
    let chain: Vec<(&str, u32)> = origin
        .chain
        .iter()
        .map(|s| (s.file.as_str(), s.line))
        .collect();
    assert_eq!(
        chain,
        [("Pulumi.yaml", 2), ("inc/outer.j2", 2), ("inc/inner.j2", 2)]
    );
}

#[test]
fn secrets_never_reach_an_error_excerpt_or_message() {
    // A secret config value interpolated into a line YAML then rejects: the
    // YAML gate's excerpt is the RENDERED line, which holds the value.
    let secret = "s3cr3t-Value-9f2";
    let rendered = format!("name: app\npassword: {secret}: oops\n");
    let mut diag = validate_rendered_yaml(&rendered, &rendered, "Pulumi.yaml").unwrap_err();
    assert!(
        diag.format_rich("Pulumi.yaml").contains(secret),
        "precondition"
    );
    diag.redact(&[secret]);
    let text = diag.format_rich("Pulumi.yaml");
    assert!(!text.contains(secret), "{text}");
    assert_eq!(diag.line, 2, "the location survives redaction");

    // The free function the language host applies to every error it prints.
    use pulumi_rs_yaml_core::jinja::redact_secrets;
    assert_eq!(
        redact_secrets("a s3cr3t b s3cr3t", &["s3cr3t", ""]),
        "a [secret] b [secret]"
    );
    assert!(matches!(
        redact_secrets("clean", &["s3cr3t"]),
        std::borrow::Cow::Borrowed(_)
    ));
}

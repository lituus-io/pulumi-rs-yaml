// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

//! Rendered text is YAML, and is rendered once.
//!
//! - Idempotence: output that holds no Jinja markers is a fixed point of the
//!   renderer, and output that does hold them (a `{% raw %}` block) is
//!   exactly the case `PULUMI_YAML_JINJA=off` exists for: the host then
//!   parses it as written.
//! - YAML safety: a value of any shape, quoted with `tojson`, round-trips
//!   through the YAML gate unchanged; `indent` keeps a multi-line value
//!   inside its block scalar; `to_yaml` emits a document that parses back.
//!   Values are generated, hostile characters first.

use pulumi_rs_yaml_core::jinja::{
    has_jinja_syntax, validate_rendered_yaml_value, JinjaContext, JinjaLimits, JinjaPreprocessor,
    UndefinedMode,
};
use std::collections::HashMap;

fn render(source: &str, extra: &HashMap<String, String>) -> String {
    let config = HashMap::new();
    let ctx = JinjaContext {
        project_name: "app",
        stack_name: "dev",
        cwd: ".",
        organization: "org",
        root_directory: ".",
        config: &config,
        project_dir: ".",
        undefined: UndefinedMode::Strict,
        provider_templated_packages: &[],
        extra,
        limits: JinjaLimits::default(),
    };
    JinjaPreprocessor::new(&ctx)
        .render(source, "Pulumi.yaml")
        .unwrap_or_else(|d| panic!("{source:?}: {}", d.format_rich("Pulumi.yaml")))
        .into_owned()
}

/// Deterministic values: every character YAML gives meaning to, then a
/// stream from a fixed-seed generator over a hostile alphabet.
fn values() -> Vec<String> {
    let mut out: Vec<String> = [
        "",
        " ",
        "a: b",
        "a:b",
        "- item",
        "#comment",
        "a #b",
        "'single'",
        "\"double\"",
        "{flow: map}",
        "[flow, seq]",
        "*alias",
        "&anchor",
        "!tag",
        "%directive",
        "@at",
        "`tick`",
        "|",
        ">",
        "? key",
        "---",
        "...",
        "null",
        "~",
        "true",
        "no",
        "1e3",
        "0x1F",
        "  leading",
        "trailing  ",
        "multi\nline",
        "tab\there",
        "crlf\r\nline",
        "back\\slash",
        "unicode é 日本 🚀",
        "nul?\u{1}",
        "${pulumi.stack}",
        "{{ not jinja }}",
        "{% raw %}",
        "\u{85}nel",
        "\u{2028}ls",
        "\u{feff}bom",
        "del\u{7f}",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let alphabet: Vec<char> = ":-#'\"{}[],&*!|>%@` \n\t\\ab01é日🚀\u{7f}\u{85}\u{2028}"
        .chars()
        .collect();
    let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
    for _ in 0..400 {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let len = (seed % 24) as usize;
        let mut s = String::new();
        let mut x = seed;
        for _ in 0..len {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            s.push(alphabet[(x >> 33) as usize % alphabet.len()]);
        }
        out.push(s);
    }
    out
}

fn yaml_str(doc: &serde_yaml::Value, key: &str) -> String {
    match &doc[key] {
        serde_yaml::Value::String(s) => s.clone(),
        other => panic!("{key}: not a string: {other:?}"),
    }
}

#[test]
fn any_value_quoted_with_tojson_round_trips_through_the_yaml_gate() {
    for v in values() {
        let extra: HashMap<String, String> = [("v".to_string(), v.clone())].into_iter().collect();
        let out = render(
            "name: app\nvalue: {{ v | tojson }}\nlist: [{{ v | tojson }}]\n",
            &extra,
        );
        let doc = validate_rendered_yaml_value(&out, &out, "Pulumi.yaml")
            .unwrap_or_else(|d| panic!("{v:?} broke the YAML: {}\n{out}", d.message));
        assert_eq!(yaml_str(&doc, "value"), v, "{out}");
        assert_eq!(doc["list"][0].as_str(), Some(v.as_str()));
    }
}

#[test]
fn an_unquoted_value_can_change_the_structure_which_is_why_tojson_exists() {
    // The hazard the guidance is about, pinned: an unquoted value holding a
    // newline and a key adds a key.
    let extra: HashMap<String, String> = [("v".to_string(), "x\ninjected: true".to_string())]
        .into_iter()
        .collect();
    let out = render("name: app\nvalue: {{ v }}\n", &extra);
    let doc = validate_rendered_yaml_value(&out, &out, "Pulumi.yaml").unwrap();
    assert_eq!(doc["injected"], serde_yaml::Value::Bool(true));
}

#[test]
fn indent_keeps_a_multi_line_value_inside_its_block_scalar() {
    for v in values() {
        // A literal block keeps text verbatim only when no line is blank-
        // with-spaces at the edges and nothing is tab-led; those are YAML's
        // rules, not the renderer's, so they are filtered rather than fixed.
        let ok = !v.is_empty()
            && !v.contains([
                '\r', '\t', '\u{85}', '\u{2028}', '\u{feff}', '\u{7f}', '\u{1}',
            ])
            && v.lines()
                .all(|l| !l.is_empty() && !l.starts_with(' ') && !l.ends_with(' '))
            && !v.ends_with('\n');
        if !ok {
            continue;
        }
        let extra: HashMap<String, String> = [("v".to_string(), v.clone())].into_iter().collect();
        let out = render(
            "name: app\nscript: |\n  {{ v | indent(2) }}\nafter: 1\n",
            &extra,
        );
        let doc = validate_rendered_yaml_value(&out, &out, "Pulumi.yaml")
            .unwrap_or_else(|d| panic!("{v:?}: {}\n{out}", d.message));
        assert_eq!(yaml_str(&doc, "script"), format!("{v}\n"), "{out}");
        assert_eq!(doc["after"], serde_yaml::Value::from(1));
    }
}

#[test]
fn to_yaml_emits_a_document_that_parses_back() {
    for v in values() {
        let extra: HashMap<String, String> = [("v".to_string(), v.clone())].into_iter().collect();
        let out = render("{{ {'k': v, 'n': [1, v]} | to_yaml }}", &extra);
        let doc: serde_yaml::Value =
            serde_yaml::from_str(&out).unwrap_or_else(|e| panic!("{v:?}: {e}\n{out}"));
        assert_eq!(yaml_str(&doc, "k"), v);
        assert_eq!(doc["n"][1].as_str(), Some(v.as_str()));
    }
}

#[test]
fn output_without_markers_is_a_fixed_point_of_the_renderer() {
    let extra = HashMap::new();
    for src in [
        "name: app\n{% for i in range(3) %}\nb{{ i }}: {{ i * 2 }}\n{% endfor %}\n",
        "name: {{ 'app' | upper }}\nouts:\n  url: ${bucket.url}\n",
        "{# comment #}name: app\n",
    ] {
        let once = render(src, &extra);
        assert!(!has_jinja_syntax(&once), "{once}");
        assert_eq!(
            render(&once, &extra),
            once,
            "a second render changed {src:?}"
        );
    }
}

#[test]
fn output_with_markers_is_exactly_what_the_off_switch_is_for() {
    // A raw block emits real Jinja delimiters. Rendering that output again
    // would read them as an expression; the off switch makes the host parse
    // it as written. Both halves are proven: the second render fails, the
    // as-written parse keeps the text.
    let extra = HashMap::new();
    let once = render(
        "name: app\nsql: \"{% raw %}{{ ref('m') }}{% endraw %}\"\n",
        &extra,
    );
    assert!(has_jinja_syntax(&once));
    let config = HashMap::new();
    let ctx = JinjaContext {
        project_name: "app",
        stack_name: "dev",
        cwd: ".",
        organization: "org",
        root_directory: ".",
        config: &config,
        project_dir: ".",
        undefined: UndefinedMode::Strict,
        provider_templated_packages: &[],
        extra: &extra,
        limits: JinjaLimits::default(),
    };
    assert!(JinjaPreprocessor::new(&ctx)
        .render(&once, "Pulumi.yaml")
        .is_err());

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Pulumi.yaml"),
        format!("{once}\nruntime: yaml\n"),
    )
    .unwrap();
    let (merged, diags) = pulumi_rs_yaml_core::multi_file::load_project(dir.path(), None);
    assert!(!diags.has_errors(), "{diags}");
    assert_eq!(merged.name(), Some("app"));
    let doc: serde_yaml::Value = serde_yaml::from_str(&once).unwrap();
    assert_eq!(yaml_str(&doc, "sql"), "{{ ref('m') }}");
}

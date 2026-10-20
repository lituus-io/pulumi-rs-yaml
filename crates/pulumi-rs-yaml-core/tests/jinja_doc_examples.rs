// Author: spicyzhug@gmail.com
// Copyright: lituus-io, all rights reserved

//! Every example in JINJA.md, executed.
//!
//! A ```jinja block is rendered; the next fenced block must be ```text (the
//! exact output) or ```error (the `RenderErrorKind` raised). A first line
//! `{# context: {...} #}` supplies string variables and is not part of the
//! template. A ```jinja block with no result block after it fails the run,
//! so an example cannot go unchecked.

use pulumi_rs_yaml_core::jinja::{JinjaContext, JinjaLimits, JinjaPreprocessor, UndefinedMode};
use std::collections::HashMap;
use std::path::Path;

struct Block<'a> {
    info: &'a str,
    body: String,
    line: usize,
}

fn blocks(doc: &str) -> Vec<Block<'_>> {
    let mut out = Vec::new();
    let mut lines = doc.lines().enumerate();
    while let Some((n, line)) = lines.next() {
        let Some(info) = line.strip_prefix("```") else {
            continue;
        };
        let mut body = String::new();
        for (_, l) in lines.by_ref() {
            if l == "```" {
                break;
            }
            body.push_str(l);
            body.push('\n');
        }
        out.push(Block {
            info: info.trim(),
            body,
            line: n + 1,
        });
    }
    out
}

fn render(template: &str) -> Result<String, String> {
    let (vars, template) = match template.strip_prefix("{# context: ") {
        Some(rest) => {
            let (json, rest) = rest.split_once(" #}\n").expect("context comment closes");
            let vars: HashMap<String, String> = serde_json::from_str(json).expect("context JSON");
            (vars, rest)
        }
        None => (HashMap::new(), template),
    };
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().to_string_lossy();
    let config = HashMap::new();
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
        extra: &vars,
        limits: JinjaLimits::default(),
    };
    JinjaPreprocessor::new(&ctx)
        .render(template, "Pulumi.yaml")
        .map(|c| c.into_owned())
        .map_err(|d| format!("{:?}", d.kind))
}

#[test]
fn every_example_in_jinja_md_renders_as_documented() {
    let doc = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../JINJA.md"))
        .expect("JINJA.md at the repository root");
    let all = blocks(&doc);
    let mut checked = 0;
    let mut failures = Vec::new();
    for (i, b) in all.iter().enumerate() {
        if b.info != "jinja" {
            continue;
        }
        let Some(result) = all
            .get(i + 1)
            .filter(|r| r.info == "text" || r.info == "error")
        else {
            failures.push(format!(
                "JINJA.md:{}: a jinja example with no text/error block after it",
                b.line
            ));
            continue;
        };
        let got = render(&b.body);
        let ok = match (result.info, &got) {
            ("text", Ok(out)) => out.trim_end_matches('\n') == result.body.trim_end_matches('\n'),
            ("error", Err(kind)) => kind == result.body.trim(),
            _ => false,
        };
        if !ok {
            failures.push(format!(
                "JINJA.md:{}: documented\n{}\n  but got {got:?}",
                b.line, result.body
            ));
        }
        checked += 1;
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
    assert!(
        checked >= 15,
        "only {checked} examples found: is the format still recognised?"
    );
}

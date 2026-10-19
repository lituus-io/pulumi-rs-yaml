// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

//! Python methods on values, held to two references at once.
//!
//! Each case under `tests/jinja_conformance/methods/<id>/expr.txt` is one
//! expression, valid both as Jinja and as Starlark. It is evaluated three
//! ways:
//! 1. pinned Python Jinja2, as `{{ expr }}` (the committed golden);
//! 2. this runtime's Jinja, the same way;
//! 3. this runtime's embedded Starlark (`Globals::standard()`, the dialect
//!    `fn::starlark` runs), as `str(expr)`.
//!
//! The runtime must agree with both, byte for byte -- or refuse where they
//! refuse -- unless `methods/DIVERGENCES.md` lists the case against the
//! engine it differs from (`jinja2`, `starlark` or `both`) with a reason.
//! A listed case that has come to agree fails as stale. One binary offers
//! two template languages; this keeps their methods the same.
//!
//! `JINJA_CONFORMANCE_BLESS=1` pins the runtime's own output for listed
//! cases (`rsyaml.txt` / `rsyaml_error.txt`), as the main corpus does.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use pulumi_rs_yaml_core::jinja::{
    JinjaContext, JinjaLimits, JinjaPreprocessor, RenderErrorKind, UndefinedMode,
};
use starlark::environment::{Globals, Module};
use starlark::eval::Evaluator;
use starlark::syntax::{AstModule, Dialect};

fn corpus() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/jinja_conformance/methods")
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Outcome {
    Text(String),
    Error(String),
}

fn class_of(kind: RenderErrorKind) -> &'static str {
    match kind {
        RenderErrorKind::JinjaSyntax => "syntax",
        RenderErrorKind::JinjaUndefinedVariable => "undefined",
        RenderErrorKind::JinjaTemplateNotFound => "not_found",
        _ => "runtime",
    }
}

fn rsyaml(expr: &str) -> Outcome {
    let (config, extra) = (HashMap::new(), HashMap::new());
    let ctx = JinjaContext {
        project_name: "p",
        stack_name: "s",
        cwd: ".",
        organization: "",
        root_directory: ".",
        config: &config,
        project_dir: ".",
        undefined: UndefinedMode::Strict,
        provider_templated_packages: &[],
        extra: &extra,
        limits: JinjaLimits::default(),
    };
    match JinjaPreprocessor::new(&ctx).render(&format!("{{{{ {expr} }}}}"), "m.j2") {
        Ok(out) => Outcome::Text(out.into_owned()),
        Err(d) => Outcome::Error(class_of(d.kind).to_string()),
    }
}

/// `str(expr)` in the embedded Starlark; any failure is a runtime error.
fn starlark(expr: &str) -> Outcome {
    let run = || -> Result<String, String> {
        let ast = AstModule::parse("m.star", format!("str({expr})"), &Dialect::Standard)
            .map_err(|e| e.to_string())?;
        let globals = Globals::standard();
        let module = Module::new();
        let mut eval = Evaluator::new(&module);
        let v = eval.eval_module(ast, &globals).map_err(|e| e.to_string())?;
        Ok(v.unpack_str()
            .map(str::to_string)
            .unwrap_or_else(|| v.to_str()))
    };
    match run() {
        Ok(s) => Outcome::Text(s),
        Err(_) => Outcome::Error("runtime".into()),
    }
}

fn read(dir: &Path, text: &str, error: &str) -> Option<Outcome> {
    if let Ok(t) = fs::read_to_string(dir.join(text)) {
        return Some(Outcome::Text(t));
    }
    fs::read_to_string(dir.join(error))
        .ok()
        .map(|e| Outcome::Error(e.trim().to_string()))
}

/// Errors agree when both refuse; Starlark's error is not classed.
fn agree(rs: &Outcome, other: &Outcome, classed: bool) -> bool {
    match (rs, other) {
        (Outcome::Error(a), Outcome::Error(b)) => !classed || a == b,
        _ => rs == other,
    }
}

fn divergences() -> BTreeMap<String, String> {
    let text = fs::read_to_string(corpus().join("DIVERGENCES.md")).expect("methods/DIVERGENCES.md");
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("| `") else {
            continue;
        };
        let cells: Vec<&str> = rest.split(" | ").collect();
        assert!(cells.len() >= 3, "malformed row: {line}");
        let engine = cells[1].trim().to_string();
        assert!(
            ["jinja2", "starlark", "both"].contains(&engine.as_str()),
            "{line}: engine must be jinja2, starlark or both"
        );
        assert!(
            !cells[2].trim_end_matches(" |").trim().is_empty(),
            "{line}: no reason"
        );
        out.insert(cells[0].trim_end_matches('`').to_string(), engine);
    }
    out
}

#[test]
fn every_method_agrees_with_jinja2_and_starlark_or_is_listed() {
    let listed = divergences();
    let bless = std::env::var_os("JINJA_CONFORMANCE_BLESS").is_some();
    let mut dirs: Vec<PathBuf> = fs::read_dir(corpus())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    assert!(dirs.len() >= 60, "the method corpus is missing");
    let mut failures = Vec::new();
    for dir in &dirs {
        let id = dir.file_name().unwrap().to_string_lossy().into_owned();
        let expr = fs::read_to_string(dir.join("expr.txt")).unwrap();
        let expr = expr.trim_end_matches('\n');
        let reference = read(dir, "expected.txt", "expected_error.txt")
            .unwrap_or_else(|| panic!("{id}: no golden; run the generator"));
        let (rs, sl) = (rsyaml(expr), starlark(expr));
        let (with_j, with_s) = (agree(&rs, &reference, true), agree(&rs, &sl, false));
        let want = match (with_j, with_s) {
            (true, true) => None,
            (false, true) => Some("jinja2"),
            (true, false) => Some("starlark"),
            (false, false) => Some("both"),
        };
        match (want, listed.get(&id).map(String::as_str)) {
            (None, Some(_)) => failures.push(format!("{id}: STALE -- agrees with both now")),
            (Some(w), None) => failures.push(format!(
                "{id}: differs from {w}\n    jinja2:   {reference:?}\n    starlark: {sl:?}\n    rs-yaml:  {rs:?}"
            )),
            (Some(w), Some(l)) if w != l => failures.push(format!(
                "{id}: listed against {l} but differs from {w}\n    jinja2:   {reference:?}\n    starlark: {sl:?}\n    rs-yaml:  {rs:?}"
            )),
            _ => {}
        }
        let pinned = read(dir, "rsyaml.txt", "rsyaml_error.txt");
        let should = want.map(|_| rs.clone());
        if bless {
            let _ = fs::remove_file(dir.join("rsyaml.txt"));
            let _ = fs::remove_file(dir.join("rsyaml_error.txt"));
            match &should {
                Some(Outcome::Text(t)) => fs::write(dir.join("rsyaml.txt"), t).unwrap(),
                Some(Outcome::Error(e)) => {
                    fs::write(dir.join("rsyaml_error.txt"), format!("{e}\n")).unwrap()
                }
                None => {}
            }
        } else if pinned != should {
            failures.push(format!(
                "{id}: pinned output is stale: {pinned:?} vs {should:?}"
            ));
        }
    }
    for id in listed.keys() {
        if !dirs.iter().any(|d| d.ends_with(id)) {
            failures.push(format!("{id}: listed but no such case"));
        }
    }
    assert!(failures.is_empty(), "\n{}\n", failures.join("\n"));
}

#[test]
fn every_str_method_in_the_table_has_a_case() {
    let names: Vec<String> = fs::read_dir(corpus())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    let mut missing = Vec::new();
    for (kind, method) in pulumi_rs_yaml_core::jinja::method_names() {
        let prefix = match kind {
            "str" => "str_",
            "seq" => "seq_",
            "map" => "map_",
            _ => "num_",
        };
        let stem = format!("{prefix}{method}");
        if !names
            .iter()
            .any(|n| *n == stem || n.starts_with(&format!("{stem}_")))
        {
            missing.push(stem);
        }
    }
    // `encode` returns bytes, which neither reference renders the same way;
    // it is covered by a unit test instead.
    missing.retain(|m| m != "str_encode");
    assert!(missing.is_empty(), "methods with no case: {missing:?}");
}

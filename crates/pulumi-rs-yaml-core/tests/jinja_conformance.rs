// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

//! Conformance of the Jinja engine against pinned reference Jinja2.
//!
//! Every case under `tests/jinja_conformance/cases/<id>/` is rendered through
//! `JinjaPreprocessor` exactly as a project file is, and the result must equal
//! the golden the reference produced (`expected.txt`), byte for byte -- or,
//! where the reference refused (`expected_error.txt`), the engine must refuse
//! too, with the same class of error.
//!
//! A case that differs is allowed only when `DIVERGENCES.md` lists it with a
//! reason, and then its own output is pinned (`rsyaml.txt` /
//! `rsyaml_error.txt`) so the difference cannot drift either. A listed case
//! that has started to agree fails as stale: the allowlist only ever shrinks
//! on purpose.
//!
//! `MATRIX.md` is generated here from the corpus, the reference's registry
//! (`registry.json`, written by the generator) and the engine's own registry,
//! probed through `is filter` / `is test` / `is defined`. A filter, test or
//! global without a case fails the run.
//!
//! `JINJA_CONFORMANCE_BLESS=1` rewrites the pinned outputs and the matrix.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use pulumi_rs_yaml_core::jinja::{JinjaContext, JinjaPreprocessor, RenderErrorKind, UndefinedMode};

fn corpus() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/jinja_conformance")
}

fn bless() -> bool {
    std::env::var_os("JINJA_CONFORMANCE_BLESS").is_some()
}

/// What one render came to: the text, or the class of its refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Outcome {
    Text(String),
    Error(String),
}

/// The class a refusal is compared by -- the same four the generator writes.
fn class_of(kind: RenderErrorKind) -> &'static str {
    match kind {
        RenderErrorKind::JinjaSyntax => "syntax",
        RenderErrorKind::JinjaUndefinedVariable => "undefined",
        RenderErrorKind::JinjaTemplateNotFound => "not_found",
        _ => "runtime",
    }
}

fn render_case(dir: &Path) -> Outcome {
    let source = fs::read_to_string(dir.join("template.j2")).expect("template.j2");
    let extra: HashMap<String, String> =
        serde_json::from_str(&fs::read_to_string(dir.join("context.json")).expect("context.json"))
            .expect("context.json is a map of strings");
    let config: HashMap<String, String> = [("proj:region".to_string(), "north".to_string())]
        .into_iter()
        .collect();
    let path = dir.to_string_lossy();
    let ctx = JinjaContext {
        project_name: "proj",
        stack_name: "dev",
        cwd: &path,
        organization: "org",
        root_directory: &path,
        config: &config,
        project_dir: &path,
        undefined: UndefinedMode::Strict,
        provider_templated_packages: &[],
        extra: &extra,
        limits: pulumi_rs_yaml_core::jinja::JinjaLimits::default(),
    };
    match JinjaPreprocessor::new(&ctx).render(&source, "template.j2") {
        Ok(text) => Outcome::Text(text.into_owned()),
        Err(diag) => Outcome::Error(class_of(diag.kind).to_string()),
    }
}

fn read_outcome(dir: &Path, text: &str, error: &str) -> Option<Outcome> {
    if let Ok(body) = fs::read(dir.join(text)) {
        return Some(Outcome::Text(
            String::from_utf8(body).expect("UTF-8 golden"),
        ));
    }
    fs::read_to_string(dir.join(error))
        .ok()
        .map(|c| Outcome::Error(c.trim().to_string()))
}

fn write_pinned(dir: &Path, outcome: Option<&Outcome>) {
    let _ = fs::remove_file(dir.join("rsyaml.txt"));
    let _ = fs::remove_file(dir.join("rsyaml_error.txt"));
    match outcome {
        Some(Outcome::Text(t)) => fs::write(dir.join("rsyaml.txt"), t).unwrap(),
        Some(Outcome::Error(c)) => {
            fs::write(dir.join("rsyaml_error.txt"), format!("{c}\n")).unwrap()
        }
        None => {}
    }
}

fn cases() -> Vec<(String, PathBuf)> {
    let mut out: Vec<(String, PathBuf)> = fs::read_dir(corpus().join("cases"))
        .expect("cases directory")
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_dir())
        .map(|p| (p.file_name().unwrap().to_string_lossy().into_owned(), p))
        .collect();
    out.sort();
    out
}

/// One allowlisted divergence: why the engine differs, and how.
struct Divergence {
    class: String,
    reason: String,
}

/// Rows of `DIVERGENCES.md` that look like `` | `id` | class | reason | ``.
fn divergences() -> BTreeMap<String, Divergence> {
    let text = fs::read_to_string(corpus().join("DIVERGENCES.md")).expect("DIVERGENCES.md");
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("| `") else {
            continue;
        };
        let cells: Vec<&str> = rest.split(" | ").collect();
        assert!(cells.len() >= 3, "malformed DIVERGENCES row: {line}");
        let id = cells[0].trim_end_matches('`').to_string();
        let class = cells[1].trim().to_string();
        assert!(
            ["intended", "unsupported", "gap", "extension"].contains(&class.as_str()),
            "{id}: class must be intended, unsupported, gap or extension, not {class:?}"
        );
        let reason = cells[2..]
            .join(" | ")
            .trim_end_matches(" |")
            .trim()
            .to_string();
        assert!(!reason.is_empty(), "{id}: a divergence needs a reason");
        assert!(
            out.insert(id.clone(), Divergence { class, reason })
                .is_none(),
            "{id} is listed twice"
        );
    }
    out
}

/// Every case's render beside the reference, computed once per test.
struct Run {
    outcomes: BTreeMap<String, (Outcome, Outcome)>,
    divergences: BTreeMap<String, Divergence>,
}

impl Run {
    fn new() -> Self {
        let mut outcomes = BTreeMap::new();
        for (id, dir) in cases() {
            let reference = read_outcome(&dir, "expected.txt", "expected_error.txt")
                .unwrap_or_else(|| {
                    panic!("{id}: no golden; run scripts/jinja_conformance/generate.py")
                });
            outcomes.insert(id, (render_case(&dir), reference));
        }
        Run {
            outcomes,
            divergences: divergences(),
        }
    }

    fn agrees(&self, id: &str) -> bool {
        let (got, want) = &self.outcomes[id];
        got == want
    }
}

#[test]
fn every_case_matches_jinja2_or_is_a_listed_divergence() {
    let run = Run::new();
    let mut failures = Vec::new();
    for (id, dir) in cases() {
        let (got, want) = &run.outcomes[&id];
        let listed = run.divergences.contains_key(&id);
        let agrees = got == want;
        match (agrees, listed) {
            (true, true) => failures.push(format!(
                "{id}: STALE -- listed in DIVERGENCES.md but now matches Jinja2; remove the entry"
            )),
            (false, false) => failures.push(format!(
                "{id}: differs from Jinja2 and is not listed\n    jinja2: {want:?}\n    engine: {got:?}"
            )),
            _ => {}
        }
        // A divergent case's own output is pinned; an agreeing case has none.
        let pinned = read_outcome(&dir, "rsyaml.txt", "rsyaml_error.txt");
        let should = (!agrees).then_some(got);
        if bless() {
            write_pinned(&dir, should);
        } else if pinned.as_ref() != should {
            failures.push(format!(
                "{id}: pinned engine output is stale\n    pinned: {pinned:?}\n    engine: {should:?}"
            ));
        }
    }
    for id in run.divergences.keys() {
        if !run.outcomes.contains_key(id) {
            failures.push(format!("{id}: listed in DIVERGENCES.md but no such case"));
        }
    }
    assert!(failures.is_empty(), "\n{}\n", failures.join("\n"));
}

// ---------------------------------------------------------------------------
// The capability matrix
// ---------------------------------------------------------------------------

/// Names the engine adds that Jinja2 does not ship; each is probed, so a
/// stale entry here fails rather than lies.
const EXTENSION_FILTERS: &[&str] = &[
    "strptime",
    "to_date",
    "to_datetime",
    "base64_decode",
    "base64_encode",
    "bool",
    "chain",
    "lines",
    "split",
    "to_json",
    "to_yaml",
    "zip",
];
const EXTENSION_TESTS: &[&str] = &["endingwith", "startingwith"];
const EXTENSION_GLOBALS: &[&str] = &[
    "date",
    "datetime",
    "readFile",
    "strptime",
    "timedelta",
    "to_date",
    "to_datetime",
];

/// The operator tests cannot be spelled `x is ==`; their cases go through
/// `select`.
fn test_case_stem(name: &str) -> &str {
    match name {
        "==" => "op_eq",
        "!=" => "op_ne",
        "<" => "op_lt",
        "<=" => "op_le",
        ">" => "op_gt",
        ">=" => "op_ge",
        other => other,
    }
}

fn engine_has(kind: &str, name: &str) -> bool {
    let probe = match kind {
        "filter" => format!("{{{{ '{name}' is filter }}}}"),
        "test" => format!("{{{{ '{name}' is test }}}}"),
        _ => format!("{{{{ {name} is defined }}}}"),
    };
    let config = HashMap::new();
    let extra = HashMap::new();
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
        limits: pulumi_rs_yaml_core::jinja::JinjaLimits::default(),
    };
    JinjaPreprocessor::new(&ctx)
        .render(&probe, "probe.j2")
        .map(|o| o.as_ref() == "True")
        .unwrap_or(false)
}

#[derive(serde::Deserialize)]
struct Registry {
    jinja2: String,
    filters: Vec<String>,
    tests: Vec<String>,
    globals: Vec<String>,
}

fn cases_for<'a>(ids: &'a [String], prefixes: &[String]) -> Vec<&'a str> {
    ids.iter()
        .filter(|id| {
            prefixes
                .iter()
                .any(|p| id.as_str() == p || id.starts_with(&format!("{p}_")))
        })
        .map(String::as_str)
        .collect()
}

/// Whether case `id`'s template calls `name` or filters through it.
fn uses(id: &str, name: &str) -> bool {
    let t =
        fs::read_to_string(corpus().join("cases").join(id).join("template.j2")).unwrap_or_default();
    t.contains(&format!("{name}("))
        || t.contains(&format!("|{name}"))
        || t.contains(&format!("| {name}"))
}

fn status(run: &Run, engine: bool, in_jinja2: bool, cases: &[&str]) -> &'static str {
    if !engine {
        return "unsupported";
    }
    if !in_jinja2 {
        return "extension";
    }
    let classes: Vec<&str> = cases
        .iter()
        .filter(|id| !run.agrees(id))
        .filter_map(|id| run.divergences.get(*id).map(|d| d.class.as_str()))
        .collect();
    if classes.contains(&"unsupported") {
        "unsupported"
    } else if classes.is_empty() {
        "supported"
    } else {
        "partial"
    }
}

fn divergence_cell(run: &Run, cases: &[&str]) -> String {
    let notes: Vec<String> = cases
        .iter()
        .filter_map(|id| {
            run.divergences
                .get(*id)
                .map(|d| format!("{} ({})", d.class, id))
        })
        .collect();
    if notes.is_empty() {
        "-".into()
    } else {
        notes.join("; ")
    }
}

fn build_matrix(run: &Run) -> (String, Vec<String>) {
    let reg: Registry =
        serde_json::from_str(&fs::read_to_string(corpus().join("registry.json")).unwrap())
            .expect("registry.json");
    let ids: Vec<String> = run.outcomes.keys().cloned().collect();
    let mut problems = Vec::new();
    let mut claimed: BTreeSet<&str> = BTreeSet::new();
    let mut totals: BTreeMap<&str, usize> = BTreeMap::new();
    let mut out = String::new();
    out.push_str(&format!(
        "# Jinja capability matrix\n\n\
         Generated by `crates/pulumi-rs-yaml-core/tests/jinja_conformance.rs` from the\n\
         conformance corpus, reference Jinja2 {}'s registry and the engine's own; do not\n\
         edit. Regenerate with `JINJA_CONFORMANCE_BLESS=1 cargo test -p pulumi-rs-yaml-core\n\
         --test jinja_conformance`.\n\n\
         Status: **supported** = every case renders byte-identical to Jinja2;\n\
         **partial** = available, with a listed divergence; **unsupported** = refused;\n\
         **extension** = pulumi-rs-yaml only. Reasons are in `DIVERGENCES.md`.\n",
        reg.jinja2
    ));

    /// One registry section: its title, the kind probed, and the names.
    struct Group<'a> {
        title: &'a str,
        kind: &'a str,
        names: &'a [String],
        extensions: &'a [&'a str],
    }
    let groups = [
        Group {
            title: "Filters",
            kind: "filter",
            names: &reg.filters,
            extensions: EXTENSION_FILTERS,
        },
        Group {
            title: "Tests",
            kind: "test",
            names: &reg.tests,
            extensions: EXTENSION_TESTS,
        },
        Group {
            title: "Globals",
            kind: "global",
            names: &reg.globals,
            extensions: EXTENSION_GLOBALS,
        },
    ];
    let mut body = String::new();
    for Group {
        title,
        kind,
        names,
        extensions,
    } in groups
    {
        let prefix = kind;
        body.push_str(&format!(
            "\n## {title}\n\n| name | status | cases | divergence |\n|---|---|---|---|\n"
        ));
        let mut all: BTreeSet<&str> = names.iter().map(String::as_str).collect();
        all.extend(extensions.iter().copied());
        for name in all {
            let in_jinja2 = names.iter().any(|n| n == name);
            let engine = engine_has(kind, name);
            let stem = if kind == "test" {
                test_case_stem(name)
            } else {
                name
            };
            let ext_kind = if kind == "global" { "fn" } else { kind };
            let prefixes = [
                format!("{prefix}_{stem}"),
                format!("ext_{ext_kind}_{}", stem.to_lowercase()),
            ];
            let mut covered = cases_for(&ids, &prefixes);
            if covered.is_empty() && !in_jinja2 {
                // An extension exercised by cases named for their topic
                // (`date_*`): the cases whose template uses it.
                covered = ids
                    .iter()
                    .filter(|id| uses(id, name))
                    .map(String::as_str)
                    .collect();
            }
            if covered.is_empty() {
                problems.push(format!("{kind} `{name}` has no conformance case"));
            }
            if !in_jinja2 && !engine {
                problems.push(format!("extension {kind} `{name}` is not registered"));
            }
            claimed.extend(covered.iter().copied());
            let st = status(run, engine, in_jinja2, &covered);
            *totals.entry(st).or_default() += 1;
            body.push_str(&format!(
                "| `{}` | {} | {} | {} |\n",
                name.replace('|', "\\|"),
                st,
                covered.join(", "),
                divergence_cell(run, &covered)
            ));
        }
    }

    body.push_str(
        "\n## Statements, expressions, whitespace, undefined, errors, YAML\n\n\
         | case | status | divergence |\n|---|---|---|\n",
    );
    for id in &ids {
        let is_row = [
            "stmt_",
            "expr_",
            "ws_",
            "undef_",
            "err_",
            "yaml_",
            "ext_ctx_",
            "ext_global_",
            "mut_",
            "date_",
        ]
        .iter()
        .any(|p| id.starts_with(p));
        if !is_row {
            continue;
        }
        claimed.insert(id.as_str());
        let in_jinja2 = !id.starts_with("ext_");
        let st = status(run, true, in_jinja2, &[id.as_str()]);
        *totals.entry(st).or_default() += 1;
        body.push_str(&format!(
            "| `{id}` | {st} | {} |\n",
            divergence_cell(run, &[id.as_str()])
        ));
    }
    for id in &ids {
        if !claimed.contains(id.as_str()) {
            problems.push(format!("case `{id}` belongs to no matrix row"));
        }
    }

    // Methods: their corpus is checked three ways by jinja_method_oracle.rs;
    // here each method gets its row.
    let mdir = corpus().join("methods");
    let mdiv = fs::read_to_string(mdir.join("DIVERGENCES.md")).unwrap_or_default();
    let mcases: Vec<String> = fs::read_dir(&mdir)
        .map(|d| {
            d.filter_map(|e| e.ok())
                .filter(|e| e.path().is_dir())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    body.push_str(
        "\n## Methods\n\nEach row's cases are held to reference Jinja2 and to the embedded \
         Starlark (`methods/DIVERGENCES.md`).\n\n| receiver | method | status | cases | divergence |\n|---|---|---|---|---|\n",
    );
    let mut rows: Vec<(&str, &str)> = pulumi_rs_yaml_core::jinja::method_names().collect();
    rows.sort();
    for (kind, method) in rows {
        let prefix = match kind {
            "str" => "str_",
            "seq" => "seq_",
            "map" => "map_",
            _ => "num_",
        };
        let stem = format!("{prefix}{method}");
        let mut cs: Vec<&str> = mcases
            .iter()
            .filter(|n| **n == stem || n.starts_with(&format!("{stem}_")))
            .map(String::as_str)
            .collect();
        cs.sort();
        let notes: Vec<String> = cs
            .iter()
            .filter_map(|c| {
                mdiv.lines()
                    .find(|l| l.starts_with(&format!("| `{c}` |")))
                    .map(|l| format!("{} ({c})", l.split(" | ").nth(1).unwrap_or("").trim()))
            })
            .collect();
        let st = if cs.is_empty() {
            "unit-tested"
        } else if notes
            .iter()
            .any(|n| n.starts_with("jinja2") || n.starts_with("both"))
        {
            "partial"
        } else {
            "supported"
        };
        *totals.entry(st).or_default() += 1;
        body.push_str(&format!(
            "| {kind} | `{method}` | {st} | {} | {} |\n",
            cs.join(", "),
            if notes.is_empty() {
                "-".to_string()
            } else {
                notes.join("; ")
            }
        ));
    }

    let agree = ids.iter().filter(|id| run.agrees(id)).count();
    out.push_str(&format!(
        "\n## Totals\n\n- cases: {} ({} byte-identical to Jinja2, {} listed divergences)\n",
        ids.len(),
        agree,
        ids.len() - agree
    ));
    for (st, n) in &totals {
        out.push_str(&format!("- rows {st}: {n}\n"));
    }
    out.push_str(&body);
    (out, problems)
}

#[test]
fn the_capability_matrix_is_current_and_complete() {
    let run = Run::new();
    let (matrix, problems) = build_matrix(&run);
    assert!(problems.is_empty(), "\n{}\n", problems.join("\n"));
    let path = corpus().join("MATRIX.md");
    if bless() {
        fs::write(&path, &matrix).unwrap();
        return;
    }
    let current = fs::read_to_string(&path).unwrap_or_default();
    assert!(
        current == matrix,
        "MATRIX.md is stale; regenerate with JINJA_CONFORMANCE_BLESS=1"
    );
}

#[test]
fn every_divergence_has_a_class_the_matrix_understands() {
    for (id, d) in divergences() {
        assert!(!d.reason.contains('\n'), "{id}: one-line reasons only");
    }
}

// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

//! A project rendered once by the caller and handed to the runtime with
//! `PULUMI_YAML_JINJA=off` is parsed as written: a literal `{{` that a
//! `{% raw %}` block produced is not rendered a second time.
//!
//! The fixture is rendered with this repository's own renderer, then run
//! through the built `pulumi-language-yaml`: its `graph` subcommand always,
//! and a real `pulumi up` against a local file backend when the `pulumi` CLI
//! is on PATH (the language host proper -- the Run RPC).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use pulumi_rs_yaml_core::jinja::{
    JinjaContext, JinjaPreprocessor, TemplatePreprocessor, UndefinedMode, JINJA_ENV,
};

const HOST: &str = env!("CARGO_BIN_EXE_pulumi-language-yaml");
const STACK: &str = "dev";

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/acceptance/host-prerendered-raw/Pulumi.yaml")
}

/// The fixture rendered once, as a caller that renders first would write it.
fn render_once() -> String {
    let source = std::fs::read_to_string(fixture()).unwrap();
    let config = HashMap::new();
    let extra = HashMap::new();
    let ctx = JinjaContext {
        project_name: "prerendered",
        stack_name: STACK,
        cwd: ".",
        organization: "organization",
        root_directory: ".",
        config: &config,
        project_dir: ".",
        undefined: UndefinedMode::Strict,
        provider_templated_packages: &[],
        extra: &extra,
    };
    JinjaPreprocessor::new(&ctx)
        .preprocess(&source, "Pulumi.yaml")
        .unwrap()
        .into_owned()
}

fn project_dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let rendered = render_once();
    assert!(rendered.contains(r#"greeting: "{{ x }}""#), "{rendered}");
    assert!(rendered.contains(r#"stackName: "dev""#), "{rendered}");
    std::fs::write(dir.join("Pulumi.yaml"), rendered).unwrap();
    dir
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn graph(dir: &Path, jinja: Option<&str>) -> Output {
    let mut cmd = Command::new(HOST);
    cmd.args(["graph", "--stack", STACK, "--dir"]).arg(dir);
    cmd.env_remove(JINJA_ENV);
    if let Some(v) = jinja {
        cmd.env(JINJA_ENV, v);
    }
    cmd.output().unwrap()
}

#[test]
fn graph_parses_rendered_text_as_written_when_the_pass_is_off() {
    let dir = project_dir("prerendered-graph");
    // Red: the default pass renders the text again and fails on `x`.
    let out = graph(&dir, None);
    assert!(!out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("undefined value"), "{}", text(&out));
    let out = graph(&dir, Some("on"));
    assert!(!out.status.success(), "{}", text(&out));
    // Green: with the pass off the file is parsed as written.
    let out = graph(&dir, Some("off"));
    assert!(out.status.success(), "{}", text(&out));
    // Anything else is refused, never guessed.
    let out = graph(&dir, Some("false"));
    assert!(!out.status.success());
    assert!(
        text(&out).contains("expected \"on\" or \"off\""),
        "{}",
        text(&out)
    );
}

fn pulumi(dir: &Path, home: &Path, jinja: Option<&str>, args: &[&str]) -> Output {
    let host_dir = Path::new(HOST).parent().unwrap();
    let path = std::env::join_paths(std::iter::once(host_dir.to_path_buf()).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))
    .unwrap();
    let mut cmd = Command::new("pulumi");
    cmd.args(args)
        .arg("--non-interactive")
        .current_dir(dir)
        .env("PATH", path)
        .env("PULUMI_HOME", home.join("home"))
        .env(
            "PULUMI_BACKEND_URL",
            format!("file://{}", home.join("state").display()),
        )
        .env("PULUMI_CONFIG_PASSPHRASE", "")
        .env("PULUMI_SKIP_UPDATE_CHECK", "1")
        .env_remove(JINJA_ENV)
        .env_remove("PULUMI_YAML_JINJA_SOURCE");
    if let Some(v) = jinja {
        cmd.env(JINJA_ENV, v);
    }
    cmd.output().unwrap()
}

#[test]
fn the_language_host_deploys_rendered_text_when_the_pass_is_off() {
    if Command::new("pulumi").arg("version").output().is_err() {
        eprintln!("skipped: the pulumi CLI is not on PATH");
        return;
    }
    let dir = project_dir("prerendered-host");
    let home = Path::new(env!("CARGO_TARGET_TMPDIR")).join("prerendered-host-home");
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(home.join("state")).unwrap();

    let out = pulumi(&dir, &home, None, &["stack", "init", STACK]);
    assert!(out.status.success(), "{}", text(&out));

    // Red: the host renders the rendered text a second time.
    let out = pulumi(&dir, &home, None, &["preview"]);
    assert!(!out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("undefined value"), "{}", text(&out));

    // Green: with the pass off the literal reaches the stack unchanged.
    let out = pulumi(&dir, &home, Some("off"), &["up", "--yes"]);
    assert!(out.status.success(), "{}", text(&out));
    let out = pulumi(&dir, &home, Some("off"), &["stack", "output", "--json"]);
    assert!(out.status.success(), "{}", text(&out));
    let outputs: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(outputs["greeting"], "{{ x }}");
    assert_eq!(outputs["stackName"], "dev");

    let out = pulumi(&dir, &home, Some("off"), &["destroy", "--yes"]);
    assert!(out.status.success(), "{}", text(&out));
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&home);
}

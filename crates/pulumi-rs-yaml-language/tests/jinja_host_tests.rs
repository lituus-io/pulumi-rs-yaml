// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

//! Jinja through the built language host, on the acceptance fixtures.
//!
//! Each fixture keeps `Pulumi.yaml` plain -- the Pulumi CLI parses that file
//! itself -- and puts its template in `Pulumi.main.yaml`, which only the
//! language host reads.
//!
//! `tests/acceptance/host-jinja-features` exercises includes, an imported
//! macro library, loop controls, a namespace, filters, a backwards slice and
//! tojson; `tests/acceptance/host-jinja-errors/<kind>` holds one stack per
//! render limit and an error two includes deep. Each runs through the host's
//! `graph` subcommand always, and through a real `pulumi preview`/`up` on a
//! local file backend when the `pulumi` CLI is on PATH (the Run RPC).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

const HOST: &str = env!("CARGO_BIN_EXE_pulumi-language-yaml");
const STACK: &str = "dev";
const VARS: [&str; 4] = [
    "PULUMI_YAML_JINJA",
    "PULUMI_YAML_JINJA_LIMITS",
    "PULUMI_YAML_JINJA_UNDEFINED",
    "PULUMI_YAML_JINJA_SOURCE",
];

fn fixture(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/acceptance")
        .join(rel)
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn graph(dir: &Path, env: &[(&str, &str)]) -> (Output, Duration) {
    let mut cmd = Command::new(HOST);
    cmd.args(["graph", "--stack", STACK, "--dir"]).arg(dir);
    for v in VARS {
        cmd.env_remove(v);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    let start = Instant::now();
    let out = cmd.output().unwrap();
    (out, start.elapsed())
}

/// Refused through `graph`, promptly, with every `needle` in the text.
fn refused(rel: &str, env: &[(&str, &str)], needles: &[&str]) {
    let (out, took) = graph(&fixture(rel), env);
    let t = text(&out);
    assert!(!out.status.success(), "{rel} rendered:\n{t}");
    assert!(took < Duration::from_secs(30), "{rel} took {took:?}");
    for n in needles {
        assert!(t.contains(n), "{rel}: missing {n:?} in\n{t}");
    }
}

#[test]
fn the_features_stack_renders_through_graph() {
    let (out, _) = graph(&fixture("host-jinja-features"), &[]);
    assert!(out.status.success(), "{}", text(&out));
}

#[test]
fn every_limit_is_refused_by_kind_at_its_line() {
    refused(
        "host-jinja-errors/fuel",
        &[],
        &[
            "Pulumi.main.yaml:2",
            "ran out of fuel",
            "PULUMI_YAML_JINJA_LIMITS=fuel",
        ],
    );
    refused(
        "host-jinja-errors/recursion",
        &[],
        &["recursion limit exceeded"],
    );
    refused(
        "host-jinja-errors/output",
        &[],
        &["Pulumi.main.yaml:3", "output limit"],
    );
    refused(
        "host-jinja-errors/range",
        &[],
        &[
            "Pulumi.main.yaml:2",
            "range() would produce 5000000 elements",
        ],
    );
    refused(
        "host-jinja-errors/cycle",
        &[],
        &["include cycle: Pulumi.main.yaml -> parts/a.j2 -> parts/b.j2 -> parts/a.j2"],
    );
}

#[test]
fn a_limit_raised_in_the_environment_lets_the_work_through() {
    let (out, _) = graph(
        &fixture("host-jinja-errors/fuel"),
        &[("PULUMI_YAML_JINJA_LIMITS", "fuel=5000000")],
    );
    assert!(out.status.success(), "{}", text(&out));
    let (out, _) = graph(
        &fixture("host-jinja-errors/range"),
        &[("PULUMI_YAML_JINJA_LIMITS", "range=5000000,fuel=50000000")],
    );
    assert!(out.status.success(), "{}", text(&out));
}

#[test]
fn an_error_two_includes_deep_names_the_file_and_the_chain() {
    refused(
        "host-jinja-errors/include-chain",
        &[],
        &[
            "undefined value: `team_owner`",
            "--> in parts/inner.j2:2:",
            "include chain: Pulumi.main.yaml:2 -> parts/outer.j2:2 -> parts/inner.j2:2",
        ],
    );
}

#[test]
fn malformed_jinja_variables_are_refused_by_name() {
    let dir = fixture("host-jinja-features");
    for (var, value, needle) in [
        (
            "PULUMI_YAML_JINJA_LIMITS",
            "fule=1",
            "unknown limit \"fule\"",
        ),
        ("PULUMI_YAML_JINJA_LIMITS", "fuel=0", "positive integer"),
        (
            "PULUMI_YAML_JINJA_UNDEFINED",
            "lenient",
            "expected \"strict\" or \"passthrough\"",
        ),
    ] {
        let (out, _) = graph(&dir, &[(var, value)]);
        assert!(!out.status.success(), "{var}={value} accepted");
        assert!(text(&out).contains(needle), "{var}={value}: {}", text(&out));
    }
    let (out, _) = graph(&dir, &[("PULUMI_YAML_JINJA_UNDEFINED", "strict")]);
    assert!(out.status.success(), "{}", text(&out));
}

// ---------------------------------------------------------------------------
// Through the language host proper, when the pulumi CLI is available
// ---------------------------------------------------------------------------

fn file_url(path: &Path) -> String {
    format!("file://{}", path.display().to_string().replace('\\', "/"))
}

fn pulumi(dir: &Path, home: &Path, env: &[(&str, &str)], args: &[&str]) -> Output {
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
        .env("PULUMI_BACKEND_URL", file_url(&home.join("state")))
        .env("PULUMI_CONFIG_PASSPHRASE", "")
        .env("PULUMI_SKIP_UPDATE_CHECK", "1");
    for v in VARS {
        cmd.env_remove(v);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().unwrap()
}

/// A copy of a fixture in the target directory, with a fresh state home.
fn workspace(rel: &str) -> (PathBuf, PathBuf) {
    let name = rel.replace('/', "-");
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("jinja-host-{name}"));
    let _ = std::fs::remove_dir_all(&root);
    let dir = root.join("project");
    copy_dir(&fixture(rel), &dir);
    let home = root.join("home");
    std::fs::create_dir_all(home.join("state")).unwrap();
    (dir, home)
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn have_pulumi() -> bool {
    let ok = Command::new("pulumi").arg("version").output().is_ok();
    if !ok {
        eprintln!("skipped: the pulumi CLI is not on PATH");
    }
    ok
}

#[test]
fn the_language_host_deploys_the_features_stack() {
    if !have_pulumi() {
        return;
    }
    let (dir, home) = workspace("host-jinja-features");
    let out = pulumi(&dir, &home, &[], &["stack", "init", STACK]);
    assert!(out.status.success(), "{}", text(&out));
    let out = pulumi(&dir, &home, &[], &["up", "--yes"]);
    assert!(out.status.success(), "{}", text(&out));
    let out = pulumi(&dir, &home, &[], &["stack", "output", "--json"]);
    assert!(out.status.success(), "{}", text(&out));
    let o: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(o["stackName"], "dev");
    assert_eq!(o["project"], "jinja-features");
    assert_eq!(
        o["kept"],
        serde_json::json!(["orders", "customers", "events"])
    );
    assert_eq!(
        o["reversed"],
        serde_json::json!(["events", "customers", "orders"])
    );
    assert_eq!(
        o["labels"],
        serde_json::json!({"team": "data", "count": "3", "stack": "dev"})
    );
    assert_eq!(o["names"], "ORDERS,CUSTOMERS,EVENTS,AUDIT");
    assert_eq!(o["sizes"], "1.5 KiB");
    assert_eq!(o["quoted"], "a: b");
    assert_eq!(o["order"], "ORDERS,EVENTS,CUSTOMERS,AUDIT");
    assert_eq!(o["expires"], "2027-01-16");
    assert_eq!(o["weekday"], "Sunday");
    assert_eq!(o["slug"], "data-lake-007");
    let out = pulumi(&dir, &home, &[], &["destroy", "--yes"]);
    assert!(out.status.success(), "{}", text(&out));
}

#[test]
fn the_language_host_refuses_a_runaway_template_and_honours_a_raised_limit() {
    if !have_pulumi() {
        return;
    }
    let (dir, home) = workspace("host-jinja-errors/fuel");
    let out = pulumi(&dir, &home, &[], &["stack", "init", STACK]);
    assert!(out.status.success(), "{}", text(&out));
    let start = Instant::now();
    let out = pulumi(&dir, &home, &[], &["preview"]);
    assert!(!out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("ran out of fuel"), "{}", text(&out));
    assert!(start.elapsed() < Duration::from_secs(120));
    let out = pulumi(
        &dir,
        &home,
        &[("PULUMI_YAML_JINJA_LIMITS", "fuel=5000000")],
        &["preview"],
    );
    assert!(out.status.success(), "{}", text(&out));
    let out = pulumi(
        &dir,
        &home,
        &[("PULUMI_YAML_JINJA_LIMITS", "fuel=nope")],
        &["preview"],
    );
    assert!(!out.status.success());
    assert!(
        text(&out).contains("PULUMI_YAML_JINJA_LIMITS"),
        "{}",
        text(&out)
    );
}

#[test]
fn the_language_host_reports_the_include_chain() {
    if !have_pulumi() {
        return;
    }
    let (dir, home) = workspace("host-jinja-errors/include-chain");
    let out = pulumi(&dir, &home, &[], &["stack", "init", STACK]);
    assert!(out.status.success(), "{}", text(&out));
    let out = pulumi(&dir, &home, &[], &["preview"]);
    assert!(!out.status.success(), "{}", text(&out));
    let t = text(&out);
    assert!(t.contains("parts/inner.j2:2"), "{t}");
    assert!(
        t.contains("include chain: Pulumi.main.yaml:2 -> parts/outer.j2:2 -> parts/inner.j2:2"),
        "{t}"
    );
}

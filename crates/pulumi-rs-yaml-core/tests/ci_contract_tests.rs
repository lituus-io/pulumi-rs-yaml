// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

//! Capabilities that live in CI configuration rather than in the engine,
//! held in place by reading that configuration. Each was shipped as a
//! changelog entry; a workflow edit that quietly removes one fails here.

use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read(rel: &str) -> String {
    std::fs::read_to_string(root().join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
}

/// 0.5.38 "CI": the binary-size job gates growth at +2% per artefact, and
/// proves on itself that the gate fails a 2.1% growth.
#[test]
fn the_binary_size_gate_is_wired_and_self_tested() {
    let ci = read(".github/workflows/ci.yml");
    assert!(ci.contains("binary-size:"), "the binary-size job is gone");
    assert!(
        ci.contains("size_gate.py"),
        "the job no longer runs the gate"
    );
    assert!(
        ci.contains("The gate fails on growth (self-test)"),
        "the self-test is gone"
    );
    let gate = read(".github/scripts/size_gate.py");
    assert!(
        gate.contains("\"--max-growth-pct\", type=float, default=2.0"),
        "the +2% default bound moved"
    );
    let needs = ci.split("ci-gate:").nth(1).expect("the CI gate job");
    assert!(
        needs.contains("- binary-size"),
        "the CI gate no longer requires the size job"
    );
}

/// The version a lockfile resolves for `name`.
fn locked(lock: &str, name: &str) -> Option<String> {
    let mut lines = lock.lines();
    while let Some(l) = lines.next() {
        if l == format!("name = \"{name}\"") {
            return lines
                .next()
                .and_then(|v| v.strip_prefix("version = \""))
                .map(|v| v.trim_end_matches('"').to_string());
        }
    }
    None
}

/// 0.5.34 "The fuzz workspace's lockfile had not moved": the fuzz workspace
/// resolves the same SQL parser (and template engine) the product ships.
#[test]
fn the_fuzz_lockfile_matches_the_product_lockfile() {
    let main = read("Cargo.lock");
    let fuzz = read("fuzz/Cargo.lock");
    for name in ["polyglot-sql", "minijinja", "starlark"] {
        let (m, f) = (locked(&main, name), locked(&fuzz, name));
        assert!(m.is_some(), "{name} is not in Cargo.lock");
        assert_eq!(m, f, "fuzz/Cargo.lock resolves a different {name}");
    }
}

/// 0.5.33 "The weekly fuzz budget was too short to reach its own findings":
/// at least 120 seconds per target, and every registered target in the
/// matrix.
#[test]
fn the_weekly_fuzz_budget_is_at_least_two_minutes() {
    let wf = read(".github/workflows/fuzz.yml");
    let line = wf
        .lines()
        .find(|l| l.trim_start().starts_with("FUZZ_DURATION:"))
        .expect("FUZZ_DURATION is set");
    let default: u64 = line
        .split("'")
        .nth(1)
        .and_then(|v| v.parse().ok())
        .expect("a quoted default duration");
    assert!(default >= 120, "the default budget fell to {default}s");
    let cargo = read("fuzz/Cargo.toml");
    for name in cargo
        .lines()
        .filter_map(|l| l.strip_prefix("name = \""))
        .map(|n| n.trim_end_matches('"'))
        .filter(|n| n.starts_with("fuzz_"))
    {
        assert!(
            wf.contains(&format!("- {name}\n")),
            "{name} is registered but never run"
        );
    }
}

/// 0.5.39 "README badges are checked": the fuzz badge is the registered
/// target count and every target is named in the Fuzz section; the
/// security badge is the `#[test]` count of security_tests.rs -- the
/// numbers the PR fuzz job holds the README to, held here as well so a
/// local `cargo test` catches a stale badge before CI does.
#[test]
fn readme_badges_match_what_they_count() {
    let readme = read("README.md");
    let badge = |label: &str| -> usize {
        let at = readme
            .find(&format!("badge/{label}-"))
            .unwrap_or_else(|| panic!("README has no {label} badge"));
        readme[at + label.len() + 7..]
            .split('-')
            .next()
            .and_then(|n| n.parse().ok())
            .unwrap_or_else(|| panic!("{label} badge has no number"))
    };
    let cargo = read("fuzz/Cargo.toml");
    let targets: Vec<&str> = cargo
        .lines()
        .filter_map(|l| l.strip_prefix("name = \""))
        .map(|n| n.trim_end_matches('"'))
        .filter(|n| n.starts_with("fuzz_"))
        .collect();
    assert_eq!(badge("fuzz%20targets"), targets.len(), "fuzz badge");
    let section = readme.split("## Fuzz").nth(1).expect("a Fuzz section");
    let section = section.split("\n## ").next().unwrap_or(section);
    for t in &targets {
        assert!(
            section.contains(&format!("`{t}`")),
            "{t} is not named in the README's Fuzz section"
        );
    }
    let sec = read("crates/pulumi-rs-yaml-core/tests/security_tests.rs");
    let tests = sec
        .lines()
        .filter(|l| l.trim_start().starts_with("#[test]"))
        .count();
    assert_eq!(badge("security%20tests"), tests, "security-tests badge");
}

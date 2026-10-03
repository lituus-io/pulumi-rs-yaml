// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

//! Every place the version lives must agree.
//!
//! There are six: five crate manifests and the Python package's own
//! `pyproject.toml`. The last one is the version the WHEEL publishes under,
//! which is what a downstream dependency's floor resolves against -- so a
//! release that bumps the crates and forgets it publishes a wheel claiming the
//! previous version, and every consumer pinning the new floor fails to
//! install.
//!
//! This file exists because that happened while preparing 0.5.33: the five
//! manifests moved and `pyproject.toml` did not. Nothing caught it, because
//! nothing was looking.

use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the crate sits two levels below the repo root")
        .to_path_buf()
}

/// The first `version = "..."` in a manifest, which is the package's own.
fn first_version(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("version") {
            let rest = rest.trim_start();
            if let Some(rest) = rest.strip_prefix('=') {
                return Some(rest.trim().trim_matches('"').to_string());
            }
        }
    }
    None
}

fn manifests() -> Vec<(PathBuf, String)> {
    let root = root();
    let mut out = Vec::new();
    let crates = std::fs::read_dir(root.join("crates")).expect("crates/ exists");
    for entry in crates.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        for name in ["Cargo.toml", "pyproject.toml"] {
            let p = dir.join(name);
            if p.exists() {
                let v = first_version(&p)
                    .unwrap_or_else(|| panic!("{} declares no version", p.display()));
                out.push((p, v));
            }
        }
    }
    out
}

#[test]
fn every_manifest_declares_the_same_version() {
    let found = manifests();
    assert!(
        found.len() >= 6,
        "only {} manifests found; the walk is not reaching them all",
        found.len()
    );
    let (ref first_path, ref expected) = found[0];
    let disagreeing: Vec<String> = found
        .iter()
        .filter(|(_, v)| v != expected)
        .map(|(p, v)| format!("{} = {v}", p.display()))
        .collect();
    assert!(
        disagreeing.is_empty(),
        "{} declares {expected}, but:\n  {}",
        first_path.display(),
        disagreeing.join("\n  ")
    );
}

/// The WHEEL's version specifically, called out because it is the one a
/// downstream floor resolves against and the one that was missed.
#[test]
fn the_wheel_version_matches_the_crates() {
    let root = root();
    let wheel = first_version(&root.join("crates/pulumi-rs-yaml-python/pyproject.toml"))
        .expect("the python package declares a version");
    let core = first_version(&root.join("crates/pulumi-rs-yaml-core/Cargo.toml"))
        .expect("core declares a version");
    assert_eq!(
        wheel, core,
        "the wheel would publish as {wheel} while the crates are {core}; a \
         consumer pinning >={core} could not install it"
    );
}

/// A plain release triple, because release artefact names are built from it.
#[test]
fn the_version_is_a_plain_release_triple() {
    let v = first_version(&root().join("crates/pulumi-rs-yaml-core/Cargo.toml"))
        .expect("core declares a version");
    let parts: Vec<&str> = v.split('.').collect();
    assert_eq!(parts.len(), 3, "{v} is not a three-part version");
    for part in parts {
        assert!(
            !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()),
            "{v} carries a non-numeric or empty component"
        );
    }
}

/// The changelog documents the version being released.
#[test]
fn the_changelog_documents_this_version() {
    let v = first_version(&root().join("crates/pulumi-rs-yaml-core/Cargo.toml"))
        .expect("core declares a version");
    let log = std::fs::read_to_string(root().join("CHANGELOG.md")).expect("CHANGELOG.md");
    assert!(
        log.contains(&format!("## {v}")),
        "CHANGELOG.md has no `## {v}` section"
    );
}

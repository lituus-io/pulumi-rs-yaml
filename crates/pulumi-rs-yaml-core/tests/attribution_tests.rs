// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

//! The names that may not appear in this repository.
//!
//! This is a public repository under AGPL. Nothing identifying a downstream
//! user belongs in its tree or its history -- not a company name, not a
//! corporate address, not a project or repository name. Agent-host names do not
//! belong there either.
//!
//! A grep that has to be remembered is a grep that will be forgotten, so it
//! runs here instead. The needles are assembled at run time rather than written
//! as literals, because a test that spelled them out would be found by itself.

use std::path::{Path, PathBuf};

/// The forbidden needles, built so this file does not contain them.
fn needles() -> Vec<String> {
    let corp = format!("{}{}", "TEL", "US");
    vec![
        corp.to_lowercase(),
        format!("{}{}", "@telus", ".com"),
        format!("{}{}", "Cla", "ude"),
        format!("{}{}", "Anthro", "pic"),
        format!("{}{}", "Co-Authored", "-By"),
    ]
}

fn repo_root() -> PathBuf {
    // tests/ -> crate -> crates/ -> repo root
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the crate sits two levels below the repo root")
        .to_path_buf()
}

/// Files whose text is checked. Binary and generated trees are skipped, as is
/// this file, whose whole subject is the needles.
fn is_checked(path: &Path) -> bool {
    let s = path.to_string_lossy();
    if s.contains("/target/") || s.contains("/.git/") || s.contains("/fuzz/corpus/") {
        return false;
    }
    // Generated protobuf code is written by `prost`, not by us, and carries no
    // header of ours. It is still checked for forbidden names below -- only the
    // header requirement is lifted, in `is_ours`.

    if path
        .file_name()
        .is_some_and(|f| f == "attribution_tests.rs")
    {
        return false;
    }
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("rs" | "toml" | "md" | "yaml" | "yml" | "json" | "pp" | "star" | "sh")
    )
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            let name = p
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            if name == "target" || name == ".git" || name == "corpus" {
                continue;
            }
            walk(&p, out);
        } else if is_checked(&p) {
            out.push(p);
        }
    }
}

#[test]
fn no_downstream_or_agent_name_appears_in_the_tree() {
    let root = repo_root();
    let mut files = Vec::new();
    walk(&root, &mut files);
    assert!(
        files.len() > 100,
        "only {} files were walked from {}; the walk is not reaching the tree",
        files.len(),
        root.display()
    );

    let needles = needles();
    let mut findings = Vec::new();
    for file in &files {
        let Ok(text) = std::fs::read_to_string(file) else {
            continue;
        };
        let lower = text.to_lowercase();
        for needle in &needles {
            if lower.contains(&needle.to_lowercase()) {
                findings.push(format!(
                    "{}: contains {:?}",
                    file.strip_prefix(&root).unwrap_or(file).display(),
                    needle
                ));
            }
        }
    }
    assert!(
        findings.is_empty(),
        "forbidden names found:\n  {}",
        findings.join("\n  ")
    );
}

/// True for a Rust file this project authored, as against one a code generator
/// wrote into the tree.
fn is_ours(path: &Path) -> bool {
    !path.to_string_lossy().contains("/src/generated/")
}

/// Every Rust source file this project authored carries the copyright header.
#[test]
fn every_source_file_carries_the_copyright_header() {
    let root = repo_root();
    let mut files = Vec::new();
    walk(&root, &mut files);
    let header = "Copyright (c) 2024-2026 Lituus-io. All rights reserved.";
    let missing: Vec<String> = files
        .iter()
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("rs"))
        .filter(|p| is_ours(p))
        .filter(|p| {
            std::fs::read_to_string(p)
                .map(|t| !t.contains(header))
                .unwrap_or(false)
        })
        .map(|p| p.strip_prefix(&root).unwrap_or(p).display().to_string())
        .collect();
    assert!(
        missing.is_empty(),
        "{} Rust file(s) without the copyright header:\n  {}",
        missing.len(),
        missing.join("\n  ")
    );
}

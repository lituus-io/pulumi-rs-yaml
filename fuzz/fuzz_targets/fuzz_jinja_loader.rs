// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

//! Fuzz target: hostile include, import and readFile names.
//!
//! One tree is built per process: a render root holding a stack, shared
//! templates and a symlink that points out of it, and beside the root a
//! directory whose one file holds a sentinel. Every input becomes a set of
//! names -- traversal, absolute, Windows forms, control characters, the
//! symlink, mixtures -- used by `include`, `include ... ignore missing`,
//! `import`, `from ... import` and `readFile()`.
//!
//! Asserts: no panic, and the sentinel never appears in output or in an
//! error. A file outside the root that was opened and served would carry it;
//! containment is checked where the bytes would surface, so no file system
//! instrumentation is needed to see a breach.

#![no_main]
use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use pulumi_rs_yaml_core::jinja::{JinjaContext, JinjaLimits, JinjaPreprocessor, UndefinedMode};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;

const SENTINEL: &str = "SENTINEL-OUTSIDE-ROOT-4b1d";

struct Tree {
    root: String,
    stack: String,
    outside: PathBuf,
}

fn tree() -> &'static Tree {
    static TREE: OnceLock<Tree> = OnceLock::new();
    TREE.get_or_init(|| {
        let base = std::env::temp_dir().join(format!("rsyaml-fuzz-loader-{}", std::process::id()));
        let root = base.join("repo");
        let stack = root.join("stacks/app");
        let outside = base.join("outside");
        for d in [&stack, &root.join("shared"), &outside] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(outside.join("secret.j2"), SENTINEL).unwrap();
        std::fs::write(outside.join("secret.yaml"), SENTINEL).unwrap();
        std::fs::write(root.join("shared/env.j2"), "{% macro m() %}ok{% endmacro %}env").unwrap();
        std::fs::write(stack.join("local.j2"), "local").unwrap();
        #[cfg(unix)]
        {
            let _ = std::os::unix::fs::symlink(&outside, stack.join("out-dir"));
            let _ = std::os::unix::fs::symlink(outside.join("secret.j2"), root.join("shared/link.j2"));
        }
        Tree {
            root: root.to_string_lossy().into_owned(),
            stack: stack.to_string_lossy().into_owned(),
            outside,
        }
    })
}

#[derive(Arbitrary, Debug)]
enum Piece {
    Up,
    UpBackslash,
    Dot,
    Sep,
    BackSep,
    Outside,
    Secret,
    SecretYaml,
    Shared,
    Env,
    Local,
    OutDir,
    Link,
    Drive,
    Unc,
    Verbatim,
    AbsoluteOutside,
    Nul,
    Control(u8),
    Text(String),
}

#[derive(Arbitrary, Debug)]
struct Input {
    names: Vec<Vec<Piece>>,
    form: u8,
}

fn name(pieces: &[Piece]) -> String {
    let t = tree();
    let mut s = String::new();
    for p in pieces.iter().take(12) {
        match p {
            Piece::Up => s.push_str("../"),
            Piece::UpBackslash => s.push_str("..\\"),
            Piece::Dot => s.push_str("./"),
            Piece::Sep => s.push('/'),
            Piece::BackSep => s.push('\\'),
            Piece::Outside => s.push_str("outside"),
            Piece::Secret => s.push_str("secret.j2"),
            Piece::SecretYaml => s.push_str("secret.yaml"),
            Piece::Shared => s.push_str("shared/"),
            Piece::Env => s.push_str("env.j2"),
            Piece::Local => s.push_str("local.j2"),
            Piece::OutDir => s.push_str("out-dir/"),
            Piece::Link => s.push_str("shared/link.j2"),
            Piece::Drive => s.push_str("C:\\"),
            Piece::Unc => s.push_str("\\\\server\\share\\"),
            Piece::Verbatim => s.push_str("\\\\?\\"),
            Piece::AbsoluteOutside => s.push_str(&t.outside.join("secret.j2").to_string_lossy()),
            Piece::Nul => s.push('\0'),
            Piece::Control(c) => s.push(char::from(c % 32)),
            Piece::Text(x) => s.extend(x.chars().take(8)),
        }
    }
    s
}

/// A Jinja string literal holding `s` exactly.
fn lit(s: &str) -> String {
    let mut out = String::from("'");
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

fuzz_target!(|input: Input| {
    let t = tree();
    let mut source = String::from("a: 1\n");
    for (i, pieces) in input.names.iter().take(4).enumerate() {
        let n = lit(&name(pieces));
        let stmt = match (input.form as usize + i) % 6 {
            0 => format!("{{% include {n} %}}\n"),
            1 => format!("{{% include {n} ignore missing %}}\n"),
            2 => format!("{{% import {n} as m{i} %}}\n"),
            3 => format!("{{% from {n} import m %}}\n"),
            4 => format!("v{i}: {{{{ readFile({n}) }}}}\n"),
            _ => format!("{{% include [{n}, 'local.j2'] %}}\n"),
        };
        source.push_str(&stmt);
    }
    let (config, extra) = (HashMap::new(), HashMap::new());
    let ctx = JinjaContext {
        project_name: "p",
        stack_name: "s",
        cwd: &t.stack,
        organization: "",
        root_directory: &t.root,
        config: &config,
        project_dir: &t.stack,
        undefined: UndefinedMode::Strict,
        provider_templated_packages: &[],
        extra: &extra,
        limits: JinjaLimits::default(),
    };
    match JinjaPreprocessor::new(&ctx).render(&source, "Pulumi.yaml") {
        Ok(out) => assert!(!out.contains(SENTINEL), "served from outside: {source:?}"),
        Err(diag) => {
            let text = diag.format_rich("Pulumi.yaml");
            assert!(!text.contains(SENTINEL), "leaked from outside: {source:?}\n{text}");
        }
    }
});

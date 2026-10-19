// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

//! Every capability a release shipped still has a live test.
//!
//! `CHANGELOG.md` describes each release as `## <version>` sections of
//! `### <heading>` entries; every heading except `Tests` is a capability.
//! `tests/capability_ledger.toml` maps each one to the tests that prove it,
//! as `<file relative to the repo root>::<function>`.
//!
//! A capability can be lost without a single test failing: the test that
//! pinned it is deleted, renamed or moved in a refactor, and the suite stays
//! green because nothing remembers that the test was the proof. This gate is
//! that memory. It fails when:
//!
//! - a changelog entry has no ledger entry, or is marked `missing = true`
//!   (a capability that shipped with no test keeps this red until one exists);
//! - a ledger entry names a heading the changelog does not carry;
//! - a named test no longer exists: a `.rs` file must contain `fn <name>(`,
//!   a `.py` file `def <name>(`;
//! - an entry has no tests and is neither superseded (with a `note` saying
//!   why) nor marked missing.
//!
//! The ledger is a restricted TOML: `[[entry]]` tables of string, boolean and
//! string-array values. There is no TOML crate in the workspace, so it is read
//! by the small parser below, which refuses anything outside that subset
//! rather than guessing at it.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the repository root exists")
}

fn read(rel: &str) -> String {
    let path = root().join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
}

// ---------------------------------------------------------------------------
// The changelog
// ---------------------------------------------------------------------------

/// Every `(version, heading)` capability entry, in file order.
fn changelog_entries(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut version: Option<&str> = None;
    let mut in_fence = false;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        if let Some(v) = line.strip_prefix("## ") {
            version = Some(v.trim());
        } else if let Some(h) = line.strip_prefix("### ") {
            let heading = h.trim();
            if heading == "Tests" {
                continue;
            }
            let v =
                version.unwrap_or_else(|| panic!("`### {heading}` sits under no `## ` version"));
            out.push((v.to_string(), heading.to_string()));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// The ledger
// ---------------------------------------------------------------------------

#[derive(Debug, Default, PartialEq)]
struct Entry {
    line: usize,
    version: String,
    heading: String,
    tests: Vec<String>,
    superseded_by: Option<String>,
    note: Option<String>,
    missing: bool,
}

#[derive(Debug, PartialEq)]
enum Value {
    Str(String),
    Bool(bool),
    List(Vec<String>),
}

/// A cursor over the ledger text that knows its line for error messages.
struct Cursor<'a> {
    rest: &'a str,
    line: usize,
}

impl<'a> Cursor<'a> {
    fn err(&self, msg: &str) -> String {
        format!("capability_ledger.toml:{}: {msg}", self.line)
    }

    fn bump(&mut self, n: usize) {
        self.line += self.rest[..n].matches('\n').count();
        self.rest = &self.rest[n..];
    }

    /// Skips spaces, tabs, and (when `newlines`) line breaks and comments.
    fn skip_ws(&mut self, newlines: bool) {
        loop {
            let t = self.rest.trim_start_matches([' ', '\t']);
            self.bump(self.rest.len() - t.len());
            if self.rest.starts_with('#') {
                let n = self.rest.find('\n').unwrap_or(self.rest.len());
                self.bump(n);
            } else if newlines && (self.rest.starts_with('\n') || self.rest.starts_with("\r\n")) {
                let n = if self.rest.starts_with('\n') { 1 } else { 2 };
                self.bump(n);
            } else {
                return;
            }
        }
    }

    fn eat(&mut self, s: &str) -> bool {
        if self.rest.starts_with(s) {
            self.bump(s.len());
            true
        } else {
            false
        }
    }

    /// The end of a key/value line: nothing but a comment before the break.
    fn end_of_line(&mut self) -> Result<(), String> {
        self.skip_ws(false);
        if self.rest.is_empty() || self.eat("\n") || self.eat("\r\n") {
            Ok(())
        } else {
            Err(self.err("expected the end of the line"))
        }
    }

    fn key(&mut self) -> Result<&'a str, String> {
        let n = self
            .rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(self.rest.len());
        if n == 0 {
            return Err(self.err("expected a key"));
        }
        let k = &self.rest[..n];
        self.bump(n);
        Ok(k)
    }

    /// A basic string: `"..."` with `\"`, `\\`, `\n` and `\t` escapes.
    fn string(&mut self) -> Result<String, String> {
        if !self.eat("\"") {
            return Err(self.err("expected a string"));
        }
        let mut out = String::new();
        let mut chars = self.rest.char_indices();
        while let Some((i, c)) = chars.next() {
            match c {
                '"' => {
                    self.bump(i + 1);
                    return Ok(out);
                }
                '\\' => match chars.next() {
                    Some((_, '"')) => out.push('"'),
                    Some((_, '\\')) => out.push('\\'),
                    Some((_, 'n')) => out.push('\n'),
                    Some((_, 't')) => out.push('\t'),
                    _ => return Err(self.err("unsupported escape in a string")),
                },
                '\n' => return Err(self.err("unterminated string")),
                c => out.push(c),
            }
        }
        Err(self.err("unterminated string"))
    }

    fn value(&mut self) -> Result<Value, String> {
        if self.rest.starts_with('"') {
            return self.string().map(Value::Str);
        }
        if self.eat("true") {
            return Ok(Value::Bool(true));
        }
        if self.eat("false") {
            return Ok(Value::Bool(false));
        }
        if !self.eat("[") {
            return Err(self.err("expected a string, a boolean or an array"));
        }
        let mut items = Vec::new();
        loop {
            self.skip_ws(true);
            if self.eat("]") {
                return Ok(Value::List(items));
            }
            items.push(self.string()?);
            self.skip_ws(true);
            if self.eat("]") {
                return Ok(Value::List(items));
            }
            if !self.eat(",") {
                return Err(self.err("expected `,` or `]` in an array"));
            }
        }
    }
}

fn parse_ledger(text: &str) -> Result<Vec<Entry>, String> {
    let mut cur = Cursor {
        rest: text,
        line: 1,
    };
    let mut entries: Vec<Entry> = Vec::new();
    loop {
        cur.skip_ws(true);
        if cur.rest.is_empty() {
            break;
        }
        let line = cur.line;
        if cur.eat("[[entry]]") {
            cur.end_of_line()?;
            entries.push(Entry {
                line,
                ..Entry::default()
            });
            continue;
        }
        let key = cur.key()?;
        cur.skip_ws(false);
        if !cur.eat("=") {
            return Err(cur.err("expected `=` after a key"));
        }
        cur.skip_ws(false);
        let value = cur.value()?;
        cur.end_of_line()?;
        let at = |msg: String| format!("capability_ledger.toml:{line}: {msg}");
        let entry = entries
            .last_mut()
            .ok_or_else(|| at(format!("`{key}` outside an `[[entry]]` table")))?;
        match (key, value) {
            ("version", Value::Str(s)) => entry.version = s,
            ("heading", Value::Str(s)) => entry.heading = s,
            ("tests", Value::List(l)) => entry.tests = l,
            ("superseded_by", Value::Str(s)) => entry.superseded_by = Some(s),
            ("note", Value::Str(s)) => entry.note = Some(s),
            ("missing", Value::Bool(b)) => entry.missing = b,
            (k, v) => return Err(at(format!("unexpected key or value type: {k} = {v:?}"))),
        }
    }
    Ok(entries)
}

fn ledger() -> Vec<Entry> {
    parse_ledger(&read("tests/capability_ledger.toml")).unwrap_or_else(|e| panic!("{e}"))
}

// ---------------------------------------------------------------------------
// Test references
// ---------------------------------------------------------------------------

/// Why a `<file>::<name>` reference does not point at a live test, if it
/// does not.
fn dangling(reference: &str) -> Option<String> {
    let Some((file, name)) = reference.rsplit_once("::") else {
        return Some("not of the form `<file>::<name>`".into());
    };
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Some(format!("`{name}` is not a function name"));
    }
    let needle = if file.ends_with(".rs") {
        format!("fn {name}(")
    } else if file.ends_with(".py") {
        format!("def {name}(")
    } else {
        return Some(format!("`{file}` is neither a .rs nor a .py file"));
    };
    match std::fs::read_to_string(root().join(file)) {
        Err(e) => Some(format!("`{file}`: {e}")),
        Ok(text) if !text.contains(&needle) => Some(format!("`{file}` has no `{needle}`")),
        Ok(_) => None,
    }
}

// ---------------------------------------------------------------------------
// The gate
// ---------------------------------------------------------------------------

fn fail_with(title: &str, problems: &[String]) {
    if problems.is_empty() {
        return;
    }
    let mut msg = format!("{title} ({}):\n", problems.len());
    for p in problems {
        let _ = writeln!(msg, "  - {p}");
    }
    panic!("{msg}");
}

/// The ledger agrees with the changelog: every heading it names exists, each
/// appears once, and every test it names is a live function.
#[test]
fn the_ledger_names_real_headings_and_live_tests() {
    let changelog = changelog_entries(&read("CHANGELOG.md"));
    let known: BTreeSet<(&str, &str)> = changelog
        .iter()
        .map(|(v, h)| (v.as_str(), h.as_str()))
        .collect();
    let versions: BTreeSet<&str> = changelog.iter().map(|(v, _)| v.as_str()).collect();
    let mut seen: BTreeMap<(&str, &str), usize> = BTreeMap::new();
    let mut problems = Vec::new();

    let entries = ledger();
    for e in &entries {
        let id = format!("ledger line {}: {} / {:?}", e.line, e.version, e.heading);
        let key = (e.version.as_str(), e.heading.as_str());
        if e.version.is_empty() || e.heading.is_empty() {
            problems.push(format!("{id}: `version` and `heading` are both required"));
        } else if !known.contains(&key) {
            problems.push(format!(
                "{id}: no such `### ` heading under `## {}`",
                e.version
            ));
        }
        if let Some(first) = seen.insert(key, e.line) {
            problems.push(format!("{id}: duplicates the entry at line {first}"));
        }
        if e.tests.is_empty() && e.superseded_by.is_none() && !e.missing {
            problems.push(format!(
                "{id}: no tests; name them, or mark `missing = true` or `superseded_by`"
            ));
        }
        if let Some(by) = &e.superseded_by {
            if !versions.contains(by.as_str()) {
                problems.push(format!(
                    "{id}: superseded_by {by:?} is not a changelog version"
                ));
            }
            if e.note.as_deref().is_none_or(str::is_empty) {
                problems.push(format!(
                    "{id}: a superseded entry needs a `note` saying why"
                ));
            }
        }
        if e.missing && !e.tests.is_empty() {
            problems.push(format!("{id}: `missing = true` but tests are named"));
        }
        for t in &e.tests {
            if let Some(why) = dangling(t) {
                problems.push(format!("{id}: {t}: {why}"));
            }
        }
    }
    fail_with("the capability ledger is out of step", &problems);
}

/// Every capability the changelog records maps to at least one live test.
#[test]
fn every_changelog_entry_maps_to_a_live_test() {
    let changelog = changelog_entries(&read("CHANGELOG.md"));
    assert!(
        !changelog.is_empty(),
        "CHANGELOG.md carries no `### ` entries"
    );
    let entries = ledger();
    let by_key: BTreeMap<(&str, &str), &Entry> = entries
        .iter()
        .map(|e| ((e.version.as_str(), e.heading.as_str()), e))
        .collect();

    let mut problems = Vec::new();
    for (version, heading) in &changelog {
        match by_key.get(&(version.as_str(), heading.as_str())) {
            None => problems.push(format!("{version} / {heading:?}: not in the ledger")),
            Some(e) if e.missing => problems.push(format!(
                "{version} / {heading:?}: marked missing -- no test proves it yet{}",
                e.note
                    .as_deref()
                    .map(|n| format!(" ({n})"))
                    .unwrap_or_default()
            )),
            Some(e) if e.tests.is_empty() && e.superseded_by.is_none() => {
                problems.push(format!("{version} / {heading:?}: names no tests"))
            }
            Some(e) => {
                if let Some(t) = e.tests.iter().find(|t| dangling(t).is_some()) {
                    problems.push(format!("{version} / {heading:?}: {t} is not a live test"));
                }
            }
        }
    }
    fail_with("changelog capabilities without a live test", &problems);
}

// ---------------------------------------------------------------------------
// The readers themselves
// ---------------------------------------------------------------------------

#[test]
fn the_changelog_reader_skips_tests_and_fenced_text() {
    let text = "# Changelog\n\n## 1.2.0\n\n### One\n\n```\n### Not a heading\n```\n\n\
                ### Tests\n\n## 1.1.0\n\n### Two `code`\n";
    assert_eq!(
        changelog_entries(text),
        [
            ("1.2.0".to_string(), "One".to_string()),
            ("1.1.0".to_string(), "Two `code`".to_string()),
        ]
    );
}

#[test]
fn the_ledger_reader_reads_its_subset_and_refuses_the_rest() {
    let text = "# header\n[[entry]]\nversion = \"1.0\" # trailing\n\
                heading = \"A \\\"quoted\\\" `x`\"\ntests = [\n  \"a.rs::f\", # c\n  \"b.py::g\",\n]\n\
                \n[[entry]]\nversion = \"0.9\"\nheading = \"Old\"\ntests = []\n\
                superseded_by = \"1.0\"\nnote = \"replaced\"\nmissing = false\n";
    let got = parse_ledger(text).expect("parses");
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].heading, "A \"quoted\" `x`");
    assert_eq!(got[0].tests, ["a.rs::f", "b.py::g"]);
    assert_eq!(got[0].line, 2);
    assert_eq!(got[1].superseded_by.as_deref(), Some("1.0"));
    assert_eq!(got[1].note.as_deref(), Some("replaced"));
    assert!(got[1].tests.is_empty() && !got[1].missing);

    for bad in [
        "version = \"1\"\n",
        "[[entry]]\nversion = 1\n",
        "[[entry]]\nunknown = \"x\"\n",
        "[[entry]]\ntests = \"a.rs::f\"\n",
        "[[entry]]\nheading = \"open\n",
        "[[entry]]\ntests = [\"a\" \"b\"]\n",
        "[[entry]]\nversion = \"1\" extra\n",
    ] {
        assert!(parse_ledger(bad).is_err(), "accepted: {bad:?}");
    }
}

#[test]
fn a_reference_must_name_a_function_that_exists() {
    assert!(dangling("crates/pulumi-rs-yaml-core/tests/capability_ledger.rs::dangling").is_none());
    assert!(
        dangling("crates/pulumi-rs-yaml-core/tests/capability_ledger.rs::no_such_fn").is_some()
    );
    assert!(dangling("crates/pulumi-rs-yaml-core/tests/capability_ledger.rs").is_some());
    assert!(dangling("crates/no/such/file.rs::f").is_some());
    assert!(dangling("CHANGELOG.md::f").is_some());
    assert!(dangling("crates/pulumi-rs-yaml-core/tests/capability_ledger.rs::a b").is_some());
}

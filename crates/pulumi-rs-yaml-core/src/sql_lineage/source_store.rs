// Author: spicyzhug@gmail.com
// Copyright: lituus-io, all rights reserved

//! The text of every file a program names with `fn::readFile`, read once and
//! held by the caller, so lineage can borrow SQL instead of copying it.
//!
//! The lineage exporter used to read a referenced file at each occurrence,
//! keep the text for as long as one resource took to process, and drop it --
//! so a file named twice was read twice, and nothing the export produced could
//! point into it. A borrow needs text that outlives the export, and the export
//! cannot fill a store it is borrowing from, so this is two-phase:
//! [`SqlSourceStore::collect`] walks the program and reads every contained
//! file first; the export then borrows from the store for its whole lifetime.
//!
//! The store is an optimisation, never a second source of truth. A file it
//! does not hold -- one that could not be resolved, read, or decoded, or that
//! is larger than [`MAX_SQL_FILE_BYTES`] -- is read the way it always was, so
//! the answer and every warning are what they were without it.

#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::ast::expr::Expr;
use crate::ast::template::{ResourceEntry, ResourceProperties, TemplateDecl, VariableEntry};
use crate::jinja::resolve_contained_path;
use crate::literal_resolve::{resolve_literal, Vars};

/// The largest file the store holds. Decided from the file's size before it
/// is read, and enforced again while reading, so a file that grows between
/// the two costs no more than the cap. Anything larger is left to the
/// exporter's own read.
pub const MAX_SQL_FILE_BYTES: u64 = 16 * 1024 * 1024;

/// File texts keyed by the path literal a program wrote.
///
/// Each file is read once however many literals name it: `q.sql` and
/// `./q.sql` are one canonical path, and share one text.
#[derive(Debug, Default)]
pub struct SqlSourceStore {
    texts: Vec<Box<str>>,
    by_literal: HashMap<Box<str>, usize>,
}

impl SqlSourceStore {
    /// Reads every file `template` names with `fn::readFile` in a resource
    /// property, at any depth, in the top-level scope and in every component
    /// body, resolving each path literal against its own scope's variables.
    /// Reads are contained to `project_dir` by the same rule the exporter
    /// applies.
    pub fn collect(template: &TemplateDecl<'_>, project_dir: &Path) -> Self {
        let mut store = Self::default();
        let mut by_path: HashMap<PathBuf, usize> = HashMap::new();
        let dir = project_dir.to_string_lossy();
        store.collect_scope(&template.resources, &template.variables, &dir, &mut by_path);
        for component in &template.components {
            store.collect_scope(
                &component.component.resources,
                &component.component.variables,
                &dir,
                &mut by_path,
            );
        }
        store
    }

    fn collect_scope<'src>(
        &mut self,
        resources: &'src [ResourceEntry<'src>],
        variables: &'src [VariableEntry<'src>],
        dir: &str,
        by_path: &mut HashMap<PathBuf, usize>,
    ) {
        let variables = Vars::from_entries(variables);
        let mut literals: Vec<std::borrow::Cow<'src, str>> = Vec::new();
        for entry in resources {
            match &entry.resource.properties {
                ResourceProperties::Map(props) => {
                    for prop in props {
                        readfile_literals(&prop.value, &variables, &mut literals);
                    }
                }
                ResourceProperties::Expr(expr) => {
                    readfile_literals(expr, &variables, &mut literals)
                }
            }
        }
        for literal in literals {
            if self.by_literal.contains_key(literal.as_ref()) {
                continue;
            }
            let Ok(canonical) = resolve_contained_path(dir, &literal) else {
                continue; // the exporter's own read reports why
            };
            let index = match by_path.get(&canonical) {
                Some(&i) => i,
                None => {
                    let Some(text) = read_capped(&canonical) else {
                        continue;
                    };
                    let i = self.texts.len();
                    self.texts.push(text);
                    by_path.insert(canonical, i);
                    i
                }
            };
            self.by_literal.insert(literal.as_ref().into(), index);
        }
    }

    /// The text of the file `literal` names, if the store holds it.
    pub fn get(&self, literal: &str) -> Option<&str> {
        self.by_literal
            .get(literal)
            .map(|&i| self.texts[i].as_ref())
    }

    /// How many distinct files the store holds.
    pub fn len(&self) -> usize {
        self.texts.len()
    }

    /// Whether the store holds no files.
    pub fn is_empty(&self) -> bool {
        self.texts.is_empty()
    }

    /// The total size of the texts held, in bytes.
    pub fn total_bytes(&self) -> usize {
        self.texts.iter().map(|t| t.len()).sum()
    }
}

/// Collects the path literal of every `fn::readFile` in `expr`, descending
/// through objects and lists -- the shapes a property path walks.
fn readfile_literals<'src>(
    expr: &'src Expr<'src>,
    variables: &Vars<'src, '_>,
    out: &mut Vec<std::borrow::Cow<'src, str>>,
) {
    match expr {
        Expr::ReadFile(_, inner) => {
            let mut memo = HashMap::new();
            let mut visiting = std::collections::HashSet::new();
            if let Some(literal) = resolve_literal(inner, variables, &mut memo, &mut visiting) {
                out.push(literal);
            }
        }
        Expr::Object(_, props) => {
            for prop in props {
                readfile_literals(&prop.value, variables, out);
            }
        }
        Expr::List(_, items) => {
            for item in items {
                readfile_literals(item, variables, out);
            }
        }
        _ => {}
    }
}

/// The file's text, or `None` when it is larger than [`MAX_SQL_FILE_BYTES`],
/// unreadable, or not UTF-8 -- every case the exporter's own read handles.
fn read_capped(path: &Path) -> Option<Box<str>> {
    let file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    if len > MAX_SQL_FILE_BYTES {
        return None;
    }
    let mut text = String::with_capacity(len as usize);
    file.take(MAX_SQL_FILE_BYTES + 1)
        .read_to_string(&mut text)
        .ok()?;
    if text.len() as u64 > MAX_SQL_FILE_BYTES {
        return None;
    }
    Some(text.into_boxed_str())
}

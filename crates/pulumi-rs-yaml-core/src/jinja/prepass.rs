// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

//! A pass over a template before it compiles, for the two template shapes
//! that crash minijinja 2.24 rather than fail. Release builds abort on panic
//! and nothing catches a stack overflow, so each took down the language host
//! or the Python process rendering the file. Both were found by
//! `fuzz_jinja_template`; 2.24 is the last 2.x release.
//!
//! **Backwards slices.** A negative-step slice of an empty list or string
//! (`{{ [][::-1] }}`) indexes past the end in minijinja's backwards range,
//! and in debug builds a stop past the start (`[1, 2][0:1:-1]`) overflows an
//! unsigned subtraction. Every slice whose step is not a positive integer
//! literal is rewritten into a call of [`slice`], which computes it exactly
//! as Python's `slice.indices` does. The rewrite keeps every newline, so
//! line numbers in errors are unchanged.
//!
//! **A namespace holding itself.** `{% set ns.v = ns %}` builds a value that
//! contains itself; printing it, serialising it or comparing it then
//! recurses without end. A namespace attribute assigned a value that holds a
//! namespace -- the same one by name, or any name bound to one -- is refused
//! with the line of the assignment. Reading an attribute (`ns.total + 1`)
//! holds nothing and is untouched.
//!
//! Both are found with minijinja's own parser. A byte scan rules out
//! templates with neither shape before anything is parsed, so the common
//! case costs nothing.

use std::borrow::Cow;

use minijinja::machinery::ast::{BinOpKind, CallArg, Expr, Stmt};
use minijinja::machinery::{parse, WhitespaceConfig};
use minijinja::value::{Value, ValueKind};
use minijinja::{Error, ErrorKind};

/// The global the rewrite calls. Not a name a template would use.
pub(super) const SLICE_FN: &str = "__rsyaml_slice";

/// How far a byte scan looks past a `[` for the colons of a slice.
const SCAN_WINDOW: usize = 512;

/// Whether `source` may hold a slice with a step: a `[` followed, within its
/// own brackets, by two colons. Allocation-free; false positives only cost a
/// parse.
fn may_have_stepped_slice(source: &str) -> bool {
    let bytes = source.as_bytes();
    let mut i = 0;
    while let Some(off) = bytes[i..].iter().position(|&b| b == b'[') {
        let open = i + off;
        let mut depth = 0usize;
        let mut colons = 0usize;
        for &b in bytes[open + 1..].iter().take(SCAN_WINDOW) {
            match b {
                b'[' | b'(' | b'{' => depth += 1,
                b']' | b')' | b'}' if depth > 0 => depth -= 1,
                b']' => break,
                b':' if depth == 0 => {
                    colons += 1;
                    if colons == 2 {
                        return true;
                    }
                }
                _ => {}
            }
        }
        i = open + 1;
    }
    false
}

/// Whether `source` may assign a namespace attribute: `set`, a name, a dot.
fn may_set_attribute(source: &str) -> bool {
    let mut rest = source;
    while let Some(at) = rest.find("set ") {
        let after = rest[at + 4..].trim_start();
        let name_len = after
            .find(|c: char| !(c.is_alphanumeric() || c == '_'))
            .unwrap_or(after.len());
        if name_len > 0 && after[name_len..].starts_with('.') {
            return true;
        }
        rest = &rest[at + 4..];
    }
    false
}

/// The functions rewritten `+` and `-` call (date arithmetic).
pub(super) const ADD_FN: &str = "__rsyaml_add";
pub(super) const SUB_FN: &str = "__rsyaml_sub";

/// The method calls that make a template's containers mutable.
fn may_mutate(source: &str) -> bool {
    super::containers::MUTATORS.iter().any(|m| {
        source.match_indices(m).any(|(i, _)| {
            i > 0 && source.as_bytes()[i - 1] == b'.' && source[i + m.len()..].starts_with('(')
        })
    })
}

/// Names that bring dates into a template.
fn may_use_dates(source: &str) -> bool {
    [
        "timedelta(",
        "to_date",
        "to_datetime",
        "strptime",
        "date(",
        "datetime(",
    ]
    .iter()
    .any(|n| source.contains(n))
}

/// What a rewrite looks for.
#[derive(Clone, Copy)]
struct Want {
    slices: bool,
    containers: bool,
    arith: bool,
    /// `/`, `//` and `%` with Python's semantics.
    math: bool,
    /// Loops over strings, for `loop.length`.
    loops: bool,
    /// `include ... without context`.
    isolated: bool,
    /// Macros that take `varargs`/`kwargs`.
    variadic: bool,
    /// `.cycle()` with nothing to cycle.
    cycle: bool,
}

impl Want {
    fn any(&self) -> bool {
        self.slices
            || self.containers
            || self.arith
            || self.math
            || self.loops
            || self.isolated
            || self.variadic
            || self.cycle
    }
}

/// `/`, `%` or `*` inside an expression or a statement's tag.
fn may_divide(source: &str) -> bool {
    let mut rest = source;
    while let Some(i) = rest.find('{') {
        let tail = &rest[i..];
        let close = if tail.starts_with("{{") {
            "}}"
        } else if tail.starts_with("{%") {
            "%}"
        } else {
            rest = &tail[1..];
            continue;
        };
        let body_end = tail[2..].find(close).map_or(tail.len(), |e| e + 2);
        let body = &tail[2..body_end];
        if body.contains(['/', '*']) || body.trim_end_matches('-').contains('%') {
            return true;
        }
        rest = &tail[body_end.min(tail.len())..];
    }
    false
}

/// One rewrite: the text `whole` becomes `func(part, part, ...)`, a missing
/// part written `none`. Byte ranges of the source.
struct Site {
    whole: (usize, usize),
    func: &'static str,
    parts: [Option<(usize, usize)>; 4],
    n: usize,
    /// Text written in place of `whole` as it is, for statement rewrites.
    literal: Option<String>,
}

impl Site {
    fn new(whole: (usize, usize), func: &'static str, parts: &[Option<(usize, usize)>]) -> Self {
        let mut p = [None; 4];
        p[..parts.len()].copy_from_slice(parts);
        // A node's span can start inside its first part (after the first
        // name of a dotted chain); the text to replace starts at the earlier.
        let first = parts.iter().flatten().map(|r| r.0).min().unwrap_or(whole.0);
        Site {
            whole: (whole.0.min(first), whole.1),
            func,
            parts: p,
            n: parts.len(),
            literal: None,
        }
    }

    fn literal(whole: (usize, usize), text: String) -> Self {
        Site {
            whole,
            func: "",
            parts: [None; 4],
            n: 0,
            literal: Some(text),
        }
    }
}

/// The bytes an expression's text covers. minijinja starts a postfix or
/// filter node's span at its operator (`.`, `(`, `|`) in some cases; the
/// text starts with the leftmost operand, so that is found by descending.
fn range(e: &Expr<'_>) -> (usize, usize) {
    let s = e.span();
    let first = match e {
        Expr::GetAttr(g) => Some(&g.expr),
        Expr::GetItem(g) => Some(&g.expr),
        Expr::Slice(x) => Some(&x.expr),
        Expr::Call(c) => Some(&c.expr),
        Expr::Filter(f) => f.expr.as_ref(),
        Expr::Test(t) => Some(&t.expr),
        Expr::BinOp(b) => Some(&b.left),
        Expr::Compare(c) => Some(&c.expr),
        _ => None,
    };
    let start = first.map_or(s.start_offset as usize, |x| {
        range(x).0.min(s.start_offset as usize)
    });
    (start, s.end_offset as usize)
}

/// A step that cannot be negative: a positive integer literal.
fn is_positive_literal(e: &Expr<'_>) -> bool {
    match e {
        Expr::Const(c) => c.value.as_i64().is_some_and(|n| n > 0),
        _ => false,
    }
}

struct Walker<'s> {
    source: &'s str,
    want: Want,
    out: Vec<Site>,
}

impl Walker<'_> {
    fn expr(&mut self, e: &Expr<'_>) {
        match e {
            Expr::Var(_) | Expr::Const(_) => {}
            Expr::Slice(s) => {
                if let Some(step) = &s.step {
                    if self.want.slices && !is_positive_literal(step) {
                        self.out.push(Site::new(
                            range(e),
                            SLICE_FN,
                            &[
                                Some(range(&s.expr)),
                                s.start.as_ref().map(range),
                                s.stop.as_ref().map(range),
                                Some(range(step)),
                            ],
                        ));
                    }
                }
                self.expr(&s.expr);
                for part in [&s.start, &s.stop, &s.step].into_iter().flatten() {
                    self.expr(part);
                }
            }
            Expr::UnaryOp(u) => self.expr(&u.expr),
            Expr::BinOp(b) => {
                let math = match b.op {
                    BinOpKind::Div => Some(super::arith::DIV_FN),
                    BinOpKind::FloorDiv => Some(super::arith::FLOORDIV_FN),
                    BinOpKind::Rem => Some(super::arith::MOD_FN),
                    BinOpKind::Mul => Some(super::arith::MUL_FN),
                    BinOpKind::Pow => Some(super::arith::POW_FN),
                    _ => None,
                };
                if let (true, Some(f)) = (self.want.math, math) {
                    self.out.push(Site::new(
                        range(e),
                        f,
                        &[Some(range(&b.left)), Some(range(&b.right))],
                    ));
                }
                if self.want.arith && matches!(b.op, BinOpKind::Add | BinOpKind::Sub) {
                    let f = if matches!(b.op, BinOpKind::Add) {
                        ADD_FN
                    } else {
                        SUB_FN
                    };
                    self.out.push(Site::new(
                        range(e),
                        f,
                        &[Some(range(&b.left)), Some(range(&b.right))],
                    ));
                }
                self.expr(&b.left);
                self.expr(&b.right);
            }
            Expr::Compare(c) => {
                self.expr(&c.expr);
                for op in &c.ops {
                    self.expr(&op.expr);
                }
            }
            Expr::IfExpr(i) => {
                self.expr(&i.test_expr);
                self.expr(&i.true_expr);
                if let Some(f) = &i.false_expr {
                    self.expr(f);
                }
            }
            Expr::Filter(f) => {
                // A filter returns a new object in Python; in a template that
                // changes containers, it is one the template may change.
                if let (true, Some(x)) = (self.want.containers, &f.expr) {
                    // A filter node's span starts at the `|`; its text starts
                    // with the value it filters.
                    let _ = x;
                    self.out.push(Site::new(
                        range(e),
                        super::containers::OWN_FN,
                        &[Some(range(e))],
                    ));
                }
                if let Some(x) = &f.expr {
                    self.expr(x);
                }
                self.args(&f.args);
            }
            Expr::Test(t) => {
                self.expr(&t.expr);
                self.args(&t.args);
            }
            Expr::GetAttr(g) => self.expr(&g.expr),
            Expr::GetItem(g) => {
                self.expr(&g.expr);
                self.expr(&g.subscript_expr);
            }
            Expr::Call(c) => {
                if let (true, true, Expr::GetAttr(g)) =
                    (self.want.cycle, c.args.is_empty(), &c.expr)
                {
                    if g.name == "cycle" {
                        self.out.push(Site::new(
                            range(e),
                            super::arith::CYCLE_FN,
                            &[Some(range(&g.expr)), None],
                        ));
                    }
                }
                if self.want.containers && matches!(&c.expr, Expr::Var(v) if v.id == "dict") {
                    self.out.push(Site::new(
                        range(e),
                        super::containers::OWN_FN,
                        &[Some(range(e))],
                    ));
                }
                self.expr(&c.expr);
                self.args(&c.args);
            }
            Expr::List(l) => {
                if self.want.containers {
                    self.out.push(Site::new(
                        range(e),
                        super::containers::LIST_FN,
                        &[Some(range(e))],
                    ));
                }
                l.items.iter().for_each(|x| self.expr(x));
            }
            Expr::Map(m) => {
                if self.want.containers {
                    self.out.push(Site::new(
                        range(e),
                        super::containers::DICT_FN,
                        &[Some(range(e))],
                    ));
                }
                m.keys.iter().for_each(|x| self.expr(x));
                m.values.iter().for_each(|x| self.expr(x));
            }
        }
    }

    fn args(&mut self, args: &[CallArg<'_>]) {
        for a in args {
            match a {
                CallArg::Pos(e)
                | CallArg::Kwarg(_, e)
                | CallArg::PosSplat(e)
                | CallArg::KwargSplat(e) => self.expr(e),
            }
        }
    }

    fn body(&mut self, stmts: &[Stmt<'_>]) {
        stmts.iter().for_each(|s| self.stmt(s));
    }

    /// Assignment targets (`for a, b in`, `set x`, macro parameters, import
    /// names) are names, not values, and are never rewritten.
    fn stmt(&mut self, s: &Stmt<'_>) {
        match s {
            Stmt::Template(t) => self.body(&t.children),
            Stmt::EmitExpr(e) => self.expr(&e.expr),
            Stmt::EmitRaw(_) | Stmt::Continue(_) | Stmt::Break(_) => {}
            Stmt::ForLoop(f) => {
                if self.want.loops {
                    let r = range(&f.iter);
                    self.out
                        .push(Site::new(r, super::arith::ITER_FN, &[Some(r)]));
                }
                self.expr(&f.iter);
                if let Some(x) = &f.filter_expr {
                    self.expr(x);
                }
                self.body(&f.body);
                self.body(&f.else_body);
            }
            Stmt::IfCond(i) => {
                self.expr(&i.expr);
                self.body(&i.true_body);
                self.body(&i.false_body);
            }
            Stmt::WithBlock(w) => {
                for (_, b) in &w.assignments {
                    self.expr(b);
                }
                self.body(&w.body);
            }
            Stmt::Set(x) => {
                if let Expr::GetAttr(g) = &x.target {
                    self.expr(&g.expr);
                }
                self.expr(&x.expr);
            }
            Stmt::SetBlock(x) => {
                if let Some(f) = &x.filter {
                    self.expr(f);
                }
                self.body(&x.body);
            }
            Stmt::AutoEscape(a) => {
                self.expr(&a.enabled);
                self.body(&a.body);
            }
            Stmt::FilterBlock(f) => {
                self.expr(&f.filter);
                self.body(&f.body);
            }
            Stmt::Block(b) => self.body(&b.body),
            Stmt::Import(i) => self.expr(&i.expr),
            Stmt::FromImport(f) => self.expr(&f.expr),
            Stmt::Extends(e) => self.expr(&e.name),
            Stmt::Include(i) => {
                if self.want.isolated {
                    self.isolated_include(i.span(), &i.name, i.ignore_missing);
                }
                self.expr(&i.name)
            }
            Stmt::Macro(m) => {
                if self.want.variadic {
                    self.variadic_macro(m.span(), m.name, &m.args);
                }
                m.defaults.iter().for_each(|x| self.expr(x));
                self.body(&m.body);
            }
            Stmt::CallBlock(c) => {
                self.expr(&c.call.expr);
                self.args(&c.call.args);
                c.macro_decl.defaults.iter().for_each(|x| self.expr(x));
                self.body(&c.macro_decl.body);
            }
            Stmt::Do(d) => {
                self.expr(&d.call.expr);
                self.args(&d.call.args);
            }
        }
    }
}

impl Walker<'_> {
    /// The tag around a statement: where its `{%` starts and its `%}` ends,
    /// and whether each side trims whitespace (`-`).
    fn tag(&self, span: minijinja::machinery::Span) -> Option<((usize, bool), (usize, bool))> {
        let (start, end) = (span.start_offset as usize, span.end_offset as usize);
        let open = self.source[..start].rfind("{%")?;
        let ltrim = self.source[open + 2..].starts_with('-');
        let after = &self.source[end..];
        let ws = after.len() - after.trim_start().len();
        let rest = &after[ws..];
        let rtrim = rest.starts_with('-');
        let rest = rest.strip_prefix(['-', '+']).unwrap_or(rest);
        rest.starts_with("%}").then_some(())?;
        let close = self.source.len() - rest.len() + 2;
        Some(((open, ltrim), (close, rtrim)))
    }

    /// `{% include x without context %}` becomes an expression calling the
    /// isolated include, keeping the tag's whitespace control.
    fn isolated_include(
        &mut self,
        span: minijinja::machinery::Span,
        name: &Expr<'_>,
        ignore: bool,
    ) {
        let text = &self.source[span.start_offset as usize..span.end_offset as usize];
        if !text.contains("without context") {
            return;
        }
        let Some(((open, l), (close, r))) = self.tag(span) else {
            return;
        };
        let (a, b) = range(name);
        let call = format!(
            "{{{{{} {}({}, {}) {}}}}}",
            if l { "-" } else { "" },
            super::arith::INCLUDE_FN,
            &self.source[a..b],
            if ignore { "true" } else { "false" },
            if r { "-" } else { "" },
        );
        self.out.push(Site::literal((open, close), call));
    }

    /// A macro whose body names `varargs` or `kwargs` is renamed, given those
    /// two parameters, and bound under its own name to a wrapper that
    /// collects extra arguments into them.
    fn variadic_macro(&mut self, span: minijinja::machinery::Span, name: &str, args: &[Expr<'_>]) {
        let (start, end) = (span.start_offset as usize, span.end_offset as usize);
        let text = &self.source[start..end];
        let word = |w: &str| {
            text.match_indices(w).any(|(i, _)| {
                let before = text[..i].chars().next_back();
                let after = text[i + w.len()..].chars().next();
                !before.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '.')
                    && !after.is_some_and(|c| c.is_alphanumeric() || c == '_')
            })
        };
        let (v, k) = (word("varargs"), word("kwargs"));
        if !v && !k {
            return;
        }
        let Some(((_, _), (close, rtrim))) = self.tag(span) else {
            return;
        };
        // `macro <name>(`: the name after the keyword, then its parameters.
        let Some(after_kw) = text.strip_prefix("macro") else {
            return;
        };
        let name_at = start + 5 + (after_kw.len() - after_kw.trim_start().len());
        if !self.source[name_at..].starts_with(name) {
            return;
        }
        let Some(open_paren) = self.source[name_at..].find('(').map(|i| name_at + i) else {
            return;
        };
        let mut depth = 0usize;
        let mut close_paren = None;
        for (i, c) in self.source[open_paren..].char_indices() {
            match c {
                '(' | '[' | '{' => depth += 1,
                ')' | ']' | '}' => {
                    depth -= 1;
                    if depth == 0 {
                        close_paren = Some(open_paren + i);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(close_paren) = close_paren else {
            return;
        };
        let inner = format!("__rsyaml_variadic_{name}");
        self.out.push(Site::literal(
            (name_at, name_at + name.len()),
            inner.clone(),
        ));
        let empty = self.source[open_paren + 1..close_paren].trim().is_empty();
        self.out.push(Site::literal(
            (close_paren, close_paren),
            format!("{}varargs=none, kwargs=none", if empty { "" } else { ", " }),
        ));
        let params: Vec<String> = args
            .iter()
            .filter_map(|a| match a {
                Expr::Var(v) => Some(format!(", '{}'", v.id)),
                _ => None,
            })
            .collect();
        self.out.push(Site::literal(
            (close, close),
            format!(
                "{{% set {name} = {}({inner}, '{}{}'{}) {}%}}",
                super::arith::VARIADIC_FN,
                if v { "v" } else { "" },
                if k { "k" } else { "" },
                params.concat(),
                if rtrim { "-" } else { "" },
            ),
        ));
    }
}

/// Writes `source[from..to]` with every site inside it replaced, outermost
/// first; `sites` is sorted by start, wider first on a tie, so the sites a
/// site contains follow it.
fn emit(
    source: &str,
    sites: &[&Site],
    from: usize,
    to: usize,
    out: &mut String,
    mut edits: Option<&mut Vec<Edit>>,
) {
    let mut at = from;
    for (i, site) in sites.iter().enumerate() {
        if site.whole.0 < at || site.whole.1 > to {
            continue;
        }
        out.push_str(&source[at..site.whole.0]);
        let written = out.len();
        if let Some(text) = &site.literal {
            out.push_str(text);
            at = site.whole.1;
            if let Some(edits) = edits.as_deref_mut() {
                edits.push(Edit {
                    out: (written, out.len()),
                    src: site.whole,
                });
            }
            continue;
        }
        let inner: Vec<&Site> = sites[i + 1..]
            .iter()
            .copied()
            .filter(|s| s.whole.1 <= site.whole.1)
            .collect();
        let parts = &site.parts[..site.n];
        out.push_str(site.func);
        out.push('(');
        for (n, part) in parts.iter().enumerate() {
            if n > 0 {
                out.push_str(", ");
            }
            match part {
                Some((a, b)) => emit(source, &inner, *a, *b, out, None),
                None => out.push_str("none"),
            }
        }
        out.push(')');
        // Keep the line count: a newline between the parts (an expression
        // written across lines) is carried after the call.
        let all = source[site.whole.0..site.whole.1].matches('\n').count();
        let kept: usize = parts
            .iter()
            .flatten()
            .map(|(a, b)| source[*a..*b].matches('\n').count())
            .sum();
        out.extend(std::iter::repeat_n('\n', all.saturating_sub(kept)));
        at = site.whole.1;
        if let Some(edits) = edits.as_deref_mut() {
            edits.push(Edit {
                out: (written, out.len()),
                src: site.whole,
            });
        }
    }
    out.push_str(&source[at..to]);
}

/// One outermost rewrite: the bytes `out` of the prepared text stand for
/// the bytes `src` of the source. Text between rewrites is the source's,
/// shifted by what the rewrites before it changed.
#[derive(Clone, Copy)]
pub(super) struct Edit {
    out: (usize, usize),
    src: (usize, usize),
}

/// The source range a range of the prepared text stands for: shifted
/// where it lies between rewrites, the whole original expression where it
/// falls inside one -- so an error raised in a routed call is placed at
/// what the author wrote.
pub(super) fn to_source(edits: &[Edit], start: usize, end: usize) -> (usize, usize) {
    let map = |p: usize, is_end: bool| -> usize {
        let mut shift: isize = 0;
        for e in edits {
            let past = if is_end { p > e.out.1 } else { p >= e.out.1 };
            let inside = if is_end { p > e.out.0 } else { p >= e.out.0 };
            if past {
                shift += (e.src.1 - e.src.0) as isize - (e.out.1 - e.out.0) as isize;
            } else if inside {
                return if is_end { e.src.1 } else { e.src.0 };
            } else {
                break;
            }
        }
        (p as isize + shift).max(0) as usize
    };
    (map(start, false), map(end, true))
}

/// The detail a namespace refusal carries, read back by the classifier.
pub(super) const NAMESPACE_TAG: &str = "a namespace would hold itself";

/// A template refused before it compiled: the line, and why.
pub(super) struct Refusal {
    pub(super) line: u32,
    pub(super) message: String,
    /// A misplaced `break`/`continue` is a syntax error, as Jinja2's
    /// compiler reports it; a namespace cycle is a type error.
    pub(super) syntax: bool,
}

/// `source` ready to compile: every slice that could take a negative step
/// routed through [`slice`] (borrowed when there is none), or a refusal for
/// a namespace that would hold itself. A template that does not parse is
/// returned as it is, for the compiler to report.
pub(super) fn prepare<'s>(source: &'s str, name: &str) -> Result<Cow<'s, str>, Refusal> {
    prepare_mapped(source, name, &mut Vec::new())
}

/// [`prepare`], recording in `edits` where the prepared text was rewritten.
pub(super) fn prepare_mapped<'s>(
    source: &'s str,
    name: &str,
    edits: &mut Vec<Edit>,
) -> Result<Cow<'s, str>, Refusal> {
    // Text with no Jinja delimiters -- most included files -- holds no
    // expression at all; three substring searches settle it.
    if !super::has_jinja_syntax(source) {
        return Ok(Cow::Borrowed(source));
    }
    let want = Want {
        slices: may_have_stepped_slice(source),
        containers: may_mutate(source),
        arith: may_use_dates(source),
        math: may_divide(source),
        loops: source.contains("loop."),
        isolated: source.contains("without context"),
        variadic: source.contains("varargs") || source.contains("kwargs"),
        cycle: source.contains("cycle("),
    };
    let attributes = may_set_attribute(source);
    let controls = source.contains("continue") || source.contains("break");
    if !want.any() && !attributes && !controls {
        return Ok(Cow::Borrowed(source));
    }
    let Ok(ast) = parse(
        source,
        name,
        minijinja::syntax::SyntaxConfig,
        WhitespaceConfig::default(),
    ) else {
        return Ok(Cow::Borrowed(source));
    };
    if attributes {
        if let Some(refusal) = namespace_cycle(&ast) {
            return Err(refusal);
        }
    }
    if controls {
        if let Some(refusal) = stray_loop_control(std::slice::from_ref(&ast), false) {
            return Err(refusal);
        }
    }
    let mut walker = Walker {
        source,
        want,
        out: Vec::new(),
    };
    if want.any() {
        walker.stmt(&ast);
    }
    if walker.out.is_empty() {
        return Ok(Cow::Borrowed(source));
    }
    // Outermost first: by start, then the longer first, then in the order
    // found (a stable order matters: two rewrites of one span nest in the
    // order the walker met them). The walk finds them almost in order, so
    // each is inserted after its equals near the end -- no sort is
    // instantiated for a list this short and this nearly sorted.
    let mut sites: Vec<Site> = Vec::with_capacity(walker.out.len());
    for site in walker.out {
        let key = |s: &Site| (s.whole.0, std::cmp::Reverse(s.whole.1));
        let at = sites.partition_point(|t| key(t) <= key(&site));
        sites.insert(at, site);
    }
    let refs: Vec<&Site> = sites.iter().collect();
    let mut out = String::with_capacity(source.len() + sites.len() * 24);
    emit(source, &refs, 0, source.len(), &mut out, Some(edits));
    Ok(Cow::Owned(out))
}

#[cfg(test)]
fn rewrite<'s>(source: &'s str, name: &str) -> Cow<'s, str> {
    prepare(source, name).unwrap_or(Cow::Borrowed(source))
}

// ---------------------------------------------------------------------------
// Namespaces that would hold themselves
// ---------------------------------------------------------------------------

/// A `break` or `continue` with no loop of its own to act on. Jinja2
/// compiles a template to Python, where a loop's `else` runs after the loop
/// and a macro or call block is a function, so neither is inside the loop
/// around it, and the compiler refuses the statement there. minijinja
/// accepted it inside a macro defined in a loop and, at run time, popped a
/// frame that was not a loop's: an engine panic, found by
/// fuzz_jinja_template.
fn stray_loop_control(stmts: &[Stmt<'_>], in_loop: bool) -> Option<Refusal> {
    let refuse = |what: &str, line: u16| Refusal {
        line: u32::from(line),
        message: format!("'{what}' not properly in loop"),
        syntax: true,
    };
    stmts.iter().find_map(|s| match s {
        Stmt::Continue(c) if !in_loop => Some(refuse("continue", c.span().start_line)),
        Stmt::Break(b) if !in_loop => Some(refuse("break", b.span().start_line)),
        Stmt::Template(t) => stray_loop_control(&t.children, in_loop),
        Stmt::ForLoop(l) => {
            stray_loop_control(&l.body, true).or_else(|| stray_loop_control(&l.else_body, in_loop))
        }
        Stmt::IfCond(c) => stray_loop_control(&c.true_body, in_loop)
            .or_else(|| stray_loop_control(&c.false_body, in_loop)),
        Stmt::WithBlock(w) => stray_loop_control(&w.body, in_loop),
        Stmt::SetBlock(b) => stray_loop_control(&b.body, in_loop),
        Stmt::AutoEscape(a) => stray_loop_control(&a.body, in_loop),
        Stmt::FilterBlock(b) => stray_loop_control(&b.body, in_loop),
        Stmt::Block(b) => stray_loop_control(&b.body, in_loop),
        Stmt::Macro(m) => stray_loop_control(&m.body, false),
        Stmt::CallBlock(c) => stray_loop_control(&c.macro_decl.body, false),
        _ => None,
    })
}

/// Every statement of a template, depth first, with a callback.
fn each_stmt<'a, F: FnMut(&'a Stmt<'a>)>(stmts: &'a [Stmt<'a>], f: &mut F) {
    for s in stmts {
        f(s);
        match s {
            Stmt::Template(t) => each_stmt(&t.children, f),
            Stmt::ForLoop(l) => {
                each_stmt(&l.body, f);
                each_stmt(&l.else_body, f);
            }
            Stmt::IfCond(c) => {
                each_stmt(&c.true_body, f);
                each_stmt(&c.false_body, f);
            }
            Stmt::WithBlock(w) => each_stmt(&w.body, f),
            Stmt::SetBlock(b) => each_stmt(&b.body, f),
            Stmt::AutoEscape(a) => each_stmt(&a.body, f),
            Stmt::FilterBlock(b) => each_stmt(&b.body, f),
            Stmt::Block(b) => each_stmt(&b.body, f),
            Stmt::Macro(m) => each_stmt(&m.body, f),
            Stmt::CallBlock(c) => each_stmt(&c.macro_decl.body, f),
            _ => {}
        }
    }
}

/// Whether evaluating `e` can yield a value that holds a variable named in
/// `names` -- the variable itself, or a container or call result built from
/// it. Reading an attribute or item of it, testing or comparing it, holds
/// nothing.
fn holds(e: &Expr<'_>, names: &[&str]) -> bool {
    match e {
        Expr::Var(v) => names.contains(&v.id),
        Expr::Const(_) | Expr::GetAttr(_) | Expr::GetItem(_) | Expr::Slice(_) => false,
        Expr::Test(_) | Expr::Compare(_) | Expr::UnaryOp(_) => false,
        Expr::BinOp(b) => match b.op {
            BinOpKind::Eq
            | BinOpKind::Ne
            | BinOpKind::Lt
            | BinOpKind::Lte
            | BinOpKind::Gt
            | BinOpKind::Gte
            | BinOpKind::In => false,
            _ => holds(&b.left, names) || holds(&b.right, names),
        },
        Expr::IfExpr(i) => {
            holds(&i.true_expr, names) || i.false_expr.as_ref().is_some_and(|f| holds(f, names))
        }
        Expr::Filter(f) => {
            f.expr.as_ref().is_some_and(|x| holds(x, names)) || args_hold(&f.args, names)
        }
        Expr::Call(c) => args_hold(&c.args, names),
        Expr::List(l) => l.items.iter().any(|x| holds(x, names)),
        Expr::Map(m) => m.keys.iter().chain(&m.values).any(|x| holds(x, names)),
    }
}

fn args_hold(args: &[CallArg<'_>], names: &[&str]) -> bool {
    args.iter().any(|a| match a {
        CallArg::Pos(e) | CallArg::Kwarg(_, e) | CallArg::PosSplat(e) | CallArg::KwargSplat(e) => {
            holds(e, names)
        }
    })
}

/// Whether `e` creates a namespace: a call of `namespace(...)`.
fn makes_namespace(e: &Expr<'_>) -> bool {
    match e {
        Expr::Call(c) => matches!(&c.expr, Expr::Var(v) if v.id == "namespace"),
        Expr::IfExpr(i) => {
            makes_namespace(&i.true_expr) || i.false_expr.as_ref().is_some_and(makes_namespace)
        }
        _ => false,
    }
}

/// The names a binding target introduces.
fn target_names<'a>(e: &'a Expr<'a>, out: &mut Vec<&'a str>) {
    match e {
        Expr::Var(v) => out.push(v.id),
        Expr::List(l) => l.items.iter().for_each(|x| target_names(x, out)),
        _ => {}
    }
}

/// Adds the names `target` binds when `value` holds one of `names`.
fn bind<'a>(target: &'a Expr<'a>, value: &Expr<'_>, names: &[&str], found: &mut Vec<&'a str>) {
    if holds(value, names) {
        let mut t = Vec::new();
        target_names(target, &mut t);
        found.extend(t.into_iter().filter(|n| !names.contains(n)));
    }
}

/// The first assignment that would make a namespace hold one.
fn namespace_cycle<'a>(ast: &'a Stmt<'a>) -> Option<Refusal> {
    let all = std::slice::from_ref(ast);
    // Every name that may be bound to a namespace: created by `namespace()`,
    // then (to a fixed point) any name bound from an expression holding one.
    let mut names: Vec<&'a str> = Vec::new();
    each_stmt(all, &mut |s| {
        if let Stmt::Set(x) = s {
            if makes_namespace(&x.expr) {
                target_names(&x.target, &mut names);
            }
        }
    });
    loop {
        let before = names.len();
        let mut found: Vec<&'a str> = Vec::new();
        each_stmt(all, &mut |s| match s {
            Stmt::Set(x) => bind(&x.target, &x.expr, &names, &mut found),
            Stmt::ForLoop(l) => bind(&l.target, &l.iter, &names, &mut found),
            Stmt::WithBlock(w) => w
                .assignments
                .iter()
                .for_each(|(t, v)| bind(t, v, &names, &mut found)),
            _ => {}
        });
        found.sort_unstable();
        found.dedup();
        names.extend(found);
        if names.len() == before {
            break;
        }
    }
    let mut refusal = None;
    each_stmt(all, &mut |s| {
        if refusal.is_some() {
            return;
        }
        let Stmt::Set(x) = s else { return };
        let Expr::GetAttr(target) = &x.target else {
            return;
        };
        let Expr::Var(root) = &target.expr else {
            return;
        };
        // The namespace itself, by any name, or any name bound to one.
        let mut held: Vec<&str> = names.clone();
        held.push(root.id);
        if holds(&x.expr, &held) {
            refusal = Some(Refusal {
                line: u32::from(x.target.span().start_line),
                message: format!(
                    "{NAMESPACE_TAG}: `{}.{}` is assigned a value holding a namespace, and \
                     printing or serialising a namespace that contains itself recurses \
                     without end",
                    root.id, target.name
                ),
                syntax: false,
            });
        }
    });
    refusal
}

/// Python's `slice.indices(len)`: the first index, the bound, and the step.
fn indices(len: i64, start: Option<i64>, stop: Option<i64>, step: i64) -> (i64, i64) {
    let (lower, upper) = if step < 0 { (-1, len - 1) } else { (0, len) };
    let clamp = |v: i64| {
        let v = if v < 0 { v.saturating_add(len) } else { v };
        v.clamp(lower, upper)
    };
    let start = start.map_or(if step < 0 { upper } else { lower }, clamp);
    let stop = stop.map_or(if step < 0 { lower } else { upper }, clamp);
    (start, stop)
}

/// The positions a Python slice visits over a sequence of `len` items.
fn positions(len: usize, start: Option<i64>, stop: Option<i64>, step: i64) -> Vec<usize> {
    let (mut i, stop) = indices(len as i64, start, stop, step);
    let mut out = Vec::new();
    while (step > 0 && i < stop) || (step < 0 && i > stop) {
        out.push(i as usize);
        i = match i.checked_add(step) {
            Some(n) => n,
            None => break,
        };
    }
    out
}

fn bound(v: &Value, what: &str) -> Result<Option<i64>, Error> {
    if v.is_none() || v.is_undefined() {
        return Ok(None);
    }
    i64::try_from(v.clone()).map(Some).map_err(|_| {
        Error::new(
            ErrorKind::InvalidOperation,
            format!("slice {what} must be an integer"),
        )
    })
}

/// `value[start:stop:step]` with Python's semantics, for every step.
pub(super) fn slice(value: Value, start: Value, stop: Value, step: Value) -> Result<Value, Error> {
    let step = bound(&step, "step")?.unwrap_or(1);
    if step == 0 {
        return Err(Error::new(
            ErrorKind::InvalidOperation,
            "cannot slice by step size of 0",
        ));
    }
    let (start, stop) = (bound(&start, "start")?, bound(&stop, "stop")?);
    match value.kind() {
        ValueKind::String => {
            let chars: Vec<char> = value.as_str().unwrap_or_default().chars().collect();
            Ok(Value::from(
                positions(chars.len(), start, stop, step)
                    .into_iter()
                    .map(|i| chars[i])
                    .collect::<String>(),
            ))
        }
        ValueKind::Bytes => {
            let bytes = value.as_bytes().unwrap_or_default();
            Ok(Value::from_bytes(
                positions(bytes.len(), start, stop, step)
                    .into_iter()
                    .map(|i| bytes[i])
                    .collect(),
            ))
        }
        ValueKind::Seq | ValueKind::Iterable => {
            let items: Vec<Value> = value.try_iter()?.collect();
            Ok(Value::from(
                positions(items.len(), start, stop, step)
                    .into_iter()
                    .map(|i| items[i].clone())
                    .collect::<Vec<_>>(),
            ))
        }
        ValueKind::None | ValueKind::Undefined => Ok(Value::from(Vec::<Value>::new())),
        kind => Err(Error::new(
            ErrorKind::InvalidOperation,
            format!("value of type {kind} cannot be sliced"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_scan_finds_stepped_slices_only() {
        assert!(may_have_stepped_slice("{{ x[::-1] }}"));
        assert!(may_have_stepped_slice("{{ x[1:2:n] }}"));
        assert!(may_have_stepped_slice("{{ x[a[0]:b:c] }}"));
        assert!(!may_have_stepped_slice("{{ x[1:2] }}"));
        assert!(!may_have_stepped_slice("{{ x[0] }} {{ d['a:b'] }}"));
        assert!(!may_have_stepped_slice("plain: yaml\nlist: [a, b]\n"));
    }

    #[test]
    fn python_slice_positions() {
        let p = |len, a, b, c| positions(len, a, b, c);
        assert_eq!(p(0, None, None, -1), Vec::<usize>::new());
        assert_eq!(p(3, None, None, -1), [2, 1, 0]);
        assert_eq!(p(3, Some(0), Some(2), -1), Vec::<usize>::new());
        assert_eq!(p(5, Some(10), None, -2), [4, 2, 0]);
        assert_eq!(p(5, Some(-1), Some(-4), -1), [4, 3, 2]);
        assert_eq!(p(5, None, None, 2), [0, 2, 4]);
        assert_eq!(p(5, Some(-100), Some(100), 1), [0, 1, 2, 3, 4]);
        assert_eq!(p(3, None, None, i64::MIN), [2]);
        assert_eq!(p(3, None, None, i64::MAX), [0]);
    }

    #[test]
    fn rewrite_routes_only_unsafe_steps() {
        assert!(matches!(rewrite("{{ x[1:2] }}", "t"), Cow::Borrowed(_)));
        assert!(matches!(rewrite("{{ x[::2] }}", "t"), Cow::Borrowed(_)));
        assert_eq!(
            rewrite("{{ x[::-1] }}", "t"),
            format!("{{{{ {SLICE_FN}(x, none, none, -1) }}}}")
        );
        assert_eq!(
            rewrite("{{ a.b[1:n:s][0] }}", "t"),
            format!("{{{{ {SLICE_FN}(a.b, 1, n, s)[0] }}}}")
        );
        // Nested: the inner slice inside the outer's target.
        assert_eq!(
            rewrite("{{ x[::-1][::-1] }}", "t"),
            format!("{{{{ {SLICE_FN}({SLICE_FN}(x, none, none, -1), none, none, -1) }}}}")
        );
        // Unparseable input is left for the compiler to report.
        assert!(matches!(rewrite("{{ x[::-1 }}", "t"), Cow::Borrowed(_)));
    }

    #[test]
    fn the_attribute_scan() {
        assert!(may_set_attribute("{% set ns.v = 1 %}"));
        assert!(may_set_attribute("{%- set  ns_2.v = 1 %}"));
        assert!(!may_set_attribute("{% set v = 1 %}"));
        assert!(!may_set_attribute("reset: 1\nset x = y.z"));
    }

    fn refused(src: &str) -> bool {
        prepare(src, "t").is_err()
    }

    #[test]
    fn a_namespace_holding_itself_is_refused() {
        for src in [
            "{% set ns = namespace() %}{% set ns.v = ns %}",
            "{% set ns = namespace() %}{% set ns.v = [ns] %}",
            "{% set ns = namespace() %}{% set ns.v = {'k': ns} %}",
            "{% set ns = namespace() %}{% set a = ns %}{% set ns.v = a %}",
            "{% set ns = namespace() %}{% for x in [ns] %}{% set ns.v = x %}{% endfor %}",
            "{% set ns = namespace() %}{% set ns.v = ns if true else 1 %}",
            "{% set ns = namespace(items=[]) %}{% set ns.items = ns.items + [ns] %}",
            "{% macro m(n) %}{% set n.v = n %}{% endmacro %}",
            "{% set a = namespace() %}{% set b = namespace() %}{% set a.v = b %}",
        ] {
            assert!(refused(src), "{src}");
        }
    }

    #[test]
    fn reading_a_namespace_is_not_holding_it() {
        for src in [
            "{% set ns = namespace(total=0) %}{% set ns.total = ns.total + 1 %}",
            "{% set ns = namespace(items=[]) %}{% set ns.items = ns.items + ['x'] %}",
            "{% set ns = namespace(seen=false) %}{% set ns.seen = ns.seen or 'x' in ns.items %}",
            "{% set ns = namespace(n=0) %}{% set ns.n = ns.n if ns is defined else 0 %}",
            "{% set ns = namespace() %}{% set ns.v = 'ns' %}",
        ] {
            assert!(!refused(src), "{src}");
        }
    }

    #[test]
    fn a_refusal_names_the_line() {
        let Err(r) = prepare("a\n{% set ns = namespace() %}\n\n{% set ns.v = ns %}", "t") else {
            panic!("refused")
        };
        assert_eq!(r.line, 4);
        assert!(r.message.starts_with(NAMESPACE_TAG) && r.message.contains("`ns.v`"));
    }

    #[test]
    fn rewrite_keeps_every_line() {
        let src = "a: 1\nb: {{ x[\n ::-1] }}\nc: {{ y[::-1] }}\n";
        let out = rewrite(src, "t");
        assert_eq!(
            out.matches('\n').count(),
            src.matches('\n').count(),
            "{out}"
        );
        assert!(out.contains(&format!("c: {{{{ {SLICE_FN}(y")));
    }
}

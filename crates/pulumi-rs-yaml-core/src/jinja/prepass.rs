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

/// One slice to rewrite, as byte ranges of the source.
struct Site {
    whole: (usize, usize),
    target: (usize, usize),
    start: Option<(usize, usize)>,
    stop: Option<(usize, usize)>,
    step: (usize, usize),
}

fn range(e: &Expr<'_>) -> (usize, usize) {
    let s = e.span();
    (s.start_offset as usize, s.end_offset as usize)
}

/// A step that cannot be negative: a positive integer literal.
fn is_positive_literal(e: &Expr<'_>) -> bool {
    match e {
        Expr::Const(c) => c.value.as_i64().is_some_and(|n| n > 0),
        _ => false,
    }
}

fn walk_expr(e: &Expr<'_>, out: &mut Vec<Site>) {
    match e {
        Expr::Var(_) | Expr::Const(_) => {}
        Expr::Slice(s) => {
            if let Some(step) = &s.step {
                if !is_positive_literal(step) {
                    // The node's span can start inside its target (after
                    // the first name of a dotted chain); the target's own
                    // start is where the text to replace begins.
                    let (whole, target) = (range(e), range(&s.expr));
                    out.push(Site {
                        whole: (whole.0.min(target.0), whole.1),
                        target,
                        start: s.start.as_ref().map(range),
                        stop: s.stop.as_ref().map(range),
                        step: range(step),
                    });
                }
            }
            walk_expr(&s.expr, out);
            for part in [&s.start, &s.stop, &s.step].into_iter().flatten() {
                walk_expr(part, out);
            }
        }
        Expr::UnaryOp(u) => walk_expr(&u.expr, out),
        Expr::BinOp(b) => {
            walk_expr(&b.left, out);
            walk_expr(&b.right, out);
        }
        Expr::Compare(c) => {
            walk_expr(&c.expr, out);
            for op in &c.ops {
                walk_expr(&op.expr, out);
            }
        }
        Expr::IfExpr(i) => {
            walk_expr(&i.test_expr, out);
            walk_expr(&i.true_expr, out);
            if let Some(f) = &i.false_expr {
                walk_expr(f, out);
            }
        }
        Expr::Filter(f) => {
            if let Some(x) = &f.expr {
                walk_expr(x, out);
            }
            walk_args(&f.args, out);
        }
        Expr::Test(t) => {
            walk_expr(&t.expr, out);
            walk_args(&t.args, out);
        }
        Expr::GetAttr(g) => walk_expr(&g.expr, out),
        Expr::GetItem(g) => {
            walk_expr(&g.expr, out);
            walk_expr(&g.subscript_expr, out);
        }
        Expr::Call(c) => {
            walk_expr(&c.expr, out);
            walk_args(&c.args, out);
        }
        Expr::List(l) => l.items.iter().for_each(|x| walk_expr(x, out)),
        Expr::Map(m) => {
            m.keys.iter().for_each(|x| walk_expr(x, out));
            m.values.iter().for_each(|x| walk_expr(x, out));
        }
    }
}

fn walk_args(args: &[CallArg<'_>], out: &mut Vec<Site>) {
    for a in args {
        match a {
            CallArg::Pos(e)
            | CallArg::Kwarg(_, e)
            | CallArg::PosSplat(e)
            | CallArg::KwargSplat(e) => walk_expr(e, out),
        }
    }
}

fn walk_body(stmts: &[Stmt<'_>], out: &mut Vec<Site>) {
    stmts.iter().for_each(|s| walk_stmt(s, out));
}

fn walk_stmt(s: &Stmt<'_>, out: &mut Vec<Site>) {
    match s {
        Stmt::Template(t) => walk_body(&t.children, out),
        Stmt::EmitExpr(e) => walk_expr(&e.expr, out),
        Stmt::EmitRaw(_) | Stmt::Continue(_) | Stmt::Break(_) => {}
        Stmt::ForLoop(f) => {
            walk_expr(&f.target, out);
            walk_expr(&f.iter, out);
            if let Some(x) = &f.filter_expr {
                walk_expr(x, out);
            }
            walk_body(&f.body, out);
            walk_body(&f.else_body, out);
        }
        Stmt::IfCond(i) => {
            walk_expr(&i.expr, out);
            walk_body(&i.true_body, out);
            walk_body(&i.false_body, out);
        }
        Stmt::WithBlock(w) => {
            for (a, b) in &w.assignments {
                walk_expr(a, out);
                walk_expr(b, out);
            }
            walk_body(&w.body, out);
        }
        Stmt::Set(x) => {
            walk_expr(&x.target, out);
            walk_expr(&x.expr, out);
        }
        Stmt::SetBlock(x) => {
            walk_expr(&x.target, out);
            if let Some(f) = &x.filter {
                walk_expr(f, out);
            }
            walk_body(&x.body, out);
        }
        Stmt::AutoEscape(a) => {
            walk_expr(&a.enabled, out);
            walk_body(&a.body, out);
        }
        Stmt::FilterBlock(f) => {
            walk_expr(&f.filter, out);
            walk_body(&f.body, out);
        }
        Stmt::Block(b) => walk_body(&b.body, out),
        Stmt::Import(i) => {
            walk_expr(&i.expr, out);
            walk_expr(&i.name, out);
        }
        Stmt::FromImport(f) => {
            walk_expr(&f.expr, out);
            for (a, b) in &f.names {
                walk_expr(a, out);
                if let Some(b) = b {
                    walk_expr(b, out);
                }
            }
        }
        Stmt::Extends(e) => walk_expr(&e.name, out),
        Stmt::Include(i) => walk_expr(&i.name, out),
        Stmt::Macro(m) => {
            m.args.iter().for_each(|x| walk_expr(x, out));
            m.defaults.iter().for_each(|x| walk_expr(x, out));
            walk_body(&m.body, out);
        }
        Stmt::CallBlock(c) => {
            walk_expr(&c.call.expr, out);
            walk_args(&c.call.args, out);
            m_body(&c.macro_decl, out);
        }
        Stmt::Do(d) => {
            walk_expr(&d.call.expr, out);
            walk_args(&d.call.args, out);
        }
    }
}

fn m_body(m: &minijinja::machinery::ast::Macro<'_>, out: &mut Vec<Site>) {
    m.args.iter().for_each(|x| walk_expr(x, out));
    m.defaults.iter().for_each(|x| walk_expr(x, out));
    walk_body(&m.body, out);
}

/// Writes `source[from..to]` with every site inside it replaced, outermost
/// first; `sites` is sorted by start, wider first on a tie, so the sites a
/// site contains follow it.
fn emit(source: &str, sites: &[&Site], from: usize, to: usize, out: &mut String) {
    let mut at = from;
    for (i, site) in sites.iter().enumerate() {
        if site.whole.0 < at || site.whole.1 > to {
            continue;
        }
        out.push_str(&source[at..site.whole.0]);
        let inner: Vec<&Site> = sites[i + 1..]
            .iter()
            .copied()
            .filter(|s| s.whole.1 <= site.whole.1)
            .collect();
        let parts = [Some(site.target), site.start, site.stop, Some(site.step)];
        out.push_str(SLICE_FN);
        out.push('(');
        for (n, part) in parts.iter().enumerate() {
            if n > 0 {
                out.push_str(", ");
            }
            match part {
                Some((a, b)) => emit(source, &inner, *a, *b, out),
                None => out.push_str("none"),
            }
        }
        out.push(')');
        // Keep the line count: a newline between the parts (a slice written
        // across lines) is carried after the call.
        let all = source[site.whole.0..site.whole.1].matches('\n').count();
        let kept: usize = parts
            .iter()
            .flatten()
            .map(|(a, b)| source[*a..*b].matches('\n').count())
            .sum();
        out.extend(std::iter::repeat_n('\n', all.saturating_sub(kept)));
        at = site.whole.1;
    }
    out.push_str(&source[at..to]);
}

/// The detail a namespace refusal carries, read back by the classifier.
pub(super) const NAMESPACE_TAG: &str = "a namespace would hold itself";

/// A template refused before it compiled: the line, and why.
pub(super) struct Refusal {
    pub(super) line: u32,
    pub(super) message: String,
}

/// `source` ready to compile: every slice that could take a negative step
/// routed through [`slice`] (borrowed when there is none), or a refusal for
/// a namespace that would hold itself. A template that does not parse is
/// returned as it is, for the compiler to report.
pub(super) fn prepare<'s>(source: &'s str, name: &str) -> Result<Cow<'s, str>, Refusal> {
    let slices = may_have_stepped_slice(source);
    let attributes = may_set_attribute(source);
    if !slices && !attributes {
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
    let mut sites = Vec::new();
    if slices {
        walk_stmt(&ast, &mut sites);
    }
    if sites.is_empty() {
        return Ok(Cow::Borrowed(source));
    }
    sites.sort_by(|a, b| a.whole.0.cmp(&b.whole.0).then(b.whole.1.cmp(&a.whole.1)));
    let refs: Vec<&Site> = sites.iter().collect();
    let mut out = String::with_capacity(source.len() + sites.len() * 24);
    emit(source, &refs, 0, source.len(), &mut out);
    Ok(Cow::Owned(out))
}

#[cfg(test)]
fn rewrite<'s>(source: &'s str, name: &str) -> Cow<'s, str> {
    prepare(source, name).unwrap_or(Cow::Borrowed(source))
}

// ---------------------------------------------------------------------------
// Namespaces that would hold themselves
// ---------------------------------------------------------------------------

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

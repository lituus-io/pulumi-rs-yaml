// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

//! The single polyglot-sql integration boundary. Every parser call
//! lives here so an API bump touches one file. All functions degrade
//! (returning `None`/empty + caller warnings) rather than erroring.

use polyglot_sql::guard::ComplexityGuardOptions;
use polyglot_sql::query_analysis::{analyze_query, AnalyzeQueryOptions};
use std::collections::HashMap;
use std::ops::Range;

use polyglot_sql::{DialectType, Expression};

/// SQL larger than this is skipped (defense-in-depth under
/// `panic = "abort"` — no unwinding safety net exists).
pub(crate) const MAX_SQL_BYTES: usize = 1024 * 1024;

/// Stack size for the parser worker thread.
///
/// SECURITY: polyglot's `stacker` feature grows the parser's stack on
/// demand, so deeply nested SQL no longer overflows (verified to nesting
/// depth 500 in unoptimized builds, where the default stack failed
/// around depth 14). Parsing still runs on the deep-stack worker
/// ([`crate::deep_stack`]) so that any residual deep recursion is isolated
/// from the caller's stack -- under `panic = "abort"` an overflow would abort
/// the whole process.
///
/// A parser job. Only owned data crosses to the worker.
pub(crate) enum Job {
    /// Each entry is an owned statement, or the reason it was refused;
    /// `columns` asks for column lineage of the statements that have it.
    Statements {
        entries: Vec<Result<String, String>>,
        columns: bool,
    },
    Select(String),
}

/// A parser answer. Only owned, `Send` fact structs -- parser AST nodes never
/// leave the worker, which is the whole point of the boundary.
pub(crate) enum Reply {
    Statements(Vec<Result<Vec<StatementFacts>, String>>),
    Select(Result<SelectFacts, String>),
}

impl Job {
    /// Runs on the deep-stack worker.
    pub(crate) fn execute(self) -> Reply {
        match self {
            Job::Statements { entries, columns } => Reply::Statements(
                entries
                    .into_iter()
                    .map(|entry| {
                        let sql = entry?;
                        polyglot_sql::parse(&sql, DialectType::BigQuery)
                            .map_err(|e| e.to_string())
                            .map(|stmts| {
                                stmts
                                    .iter()
                                    .map(|stmt| {
                                        let mut facts = statement_facts(stmt, &sql);
                                        if columns {
                                            facts.columns = column_facts(stmt).unwrap_or_default();
                                        }
                                        facts
                                    })
                                    .collect()
                            })
                    })
                    .collect(),
            ),
            Job::Select(sql) => Reply::Select(analyze_select_inner(&sql)),
        }
    }
}

/// Runs one parser job on this thread's deep-stack worker.
fn on_parser_thread(job: Job) -> Result<Reply, String> {
    match crate::deep_stack::run(crate::deep_stack::Job::Sql(job))? {
        crate::deep_stack::Reply::Sql(reply) => Ok(reply),
        crate::deep_stack::Reply::Render(_) => unreachable!("a SQL job answers SQL"),
    }
}

/// Pre-flight guard shared by every parser entry point.
fn guard(sql: &str) -> Result<(), String> {
    if sql.len() > MAX_SQL_BYTES {
        return Err(format!(
            "SQL exceeds {} bytes; skipped by static lineage",
            MAX_SQL_BYTES
        ));
    }
    Ok(())
}

/// What a statement does, as far as lineage is concerned.
///
/// Serialized snake_case. New variants go at the END: the enum's order is not
/// part of any contract, but appending keeps every existing comparison stable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StatementKind {
    Select,
    Insert,
    Update,
    Delete,
    Merge,
    CreateTable,
    CreateTableAsSelect,
    CreateView,
    CreateMaterializedView,
    CreateFunction,
    CreateProcedure,
    Drop,
    Truncate,
    Alter,
    ExportData,
    LoadData,
    Call,
    /// Procedural scripting: `DECLARE`, `SET`, `IF`, `BEGIN`, loops, ...
    Scripting,
    #[default]
    Other,
}

/// Table-level facts for one statement.
#[derive(Debug, Default)]
pub(crate) struct StatementFacts {
    /// What the statement does.
    pub kind: StatementKind,
    /// Raw referenced table names (reads), as written.
    pub reads: Vec<String>,
    /// Raw names of tables the statement changes: INSERT/MERGE/UPDATE/DELETE
    /// targets, CREATE TABLE/VIEW (with or without AS SELECT), and the
    /// objects of DROP, TRUNCATE and ALTER. Those last three used to fall
    /// through to the reads, so `DROP TABLE x` was reported as reading `x`.
    pub writes: Vec<String>,
    /// Raw routine names invoked via CALL.
    pub calls: Vec<String>,
    /// External locations read: the URIs a `LOAD DATA` loads from.
    pub external_reads: Vec<String>,
    /// External locations written: the URI an `EXPORT DATA` writes to.
    pub external_writes: Vec<String>,
    /// Column lineage of the table the statement writes: (target column,
    /// [(source table, source column)]). Filled only when asked for, and only
    /// where the target's columns are known -- see [`column_facts`].
    pub columns: ColumnLineage,
}

/// (output column, [(source table as written, source column)]), one entry
/// per output column.
pub(crate) type ColumnLineage = Vec<(String, Vec<(String, String)>)>;

/// Column-level facts for a SELECT-shaped statement.
#[derive(Debug)]
pub(crate) struct SelectFacts {
    /// Raw base-table names.
    pub reads: Vec<String>,
    /// (output column name, [(source table raw name, source column)]).
    pub columns: Vec<(String, Vec<(String, String)>)>,
    /// True when a `*` projection could not be expanded.
    pub has_unexpanded_star: bool,
}

/// Table-level facts for EVERY statement of a script, on ONE parser thread.
///
/// The per-statement entry point spawned a thread -- with a
/// `DEEP_STACK_BYTES` stack -- for each statement, and nothing bounds how
/// many statements a script has: `MAX_SQL_BYTES` bounds the TEXT, and
/// `split_statements` only drops the EMPTY ones, so 1 MiB of
/// `select 1 from t;` is ~65,000 non-empty statements and so ~65,000 thread
/// spawns at 64 MiB of stack reservation each.
///
/// Batching them removes that entirely: one spawn and one stack for a whole
/// script, however many statements it holds. It is also strictly less work on
/// every real stack -- a twenty-view program paid twenty-odd spawns for no
/// reason -- so this is a cost removed rather than a bound bolted on.
///
/// Returns one result per input statement, in order, so a caller can still
/// fall back per statement. The isolation contract is unchanged: parser AST
/// nodes never leave the worker, only owned `Send` fact structs.
#[cfg(test)]
pub(crate) fn statement_facts_for_all(
    statements: &[&str],
) -> Vec<Result<Vec<StatementFacts>, String>> {
    statement_facts_with_columns(statements, false)
}

/// [`statement_facts_for_all`], with column lineage for every statement that
/// writes a table from a query when `columns` is set.
pub(crate) fn statement_facts_with_columns(
    statements: &[&str],
    columns: bool,
) -> Vec<Result<Vec<StatementFacts>, String>> {
    if statements.is_empty() {
        return Vec::new();
    }
    // Oversized statements are refused here rather than inside the worker, so
    // the guard's verdict does not depend on where it is checked.
    let owned: Vec<Result<String, String>> = statements
        .iter()
        .map(|s| guard(s).map(|()| (*s).to_string()))
        .collect();

    match on_parser_thread(Job::Statements {
        entries: owned,
        columns,
    }) {
        Ok(Reply::Statements(out)) => out,
        Ok(Reply::Select(_)) => unreachable!("a Statements job answers Statements"),
        Err(e) => statements.iter().map(|_| Err(e.clone())).collect(),
    }
}

/// Analyzes a single SELECT-shaped statement for table + column facts.
pub(crate) fn analyze_select(sql: &str) -> Result<SelectFacts, String> {
    guard(sql)?;
    match on_parser_thread(Job::Select(sql.to_string()))? {
        Reply::Select(out) => out,
        Reply::Statements(_) => unreachable!("a Select job answers Select"),
    }
}

fn analyze_select_inner(sql: &str) -> Result<SelectFacts, String> {
    let analysis = analyze_query(
        sql,
        AnalyzeQueryOptions {
            // EXPLICITLY Some. `None` disables every limit, and the limits are
            // the whole reason this version is pinned: 0.6.2 did not terminate
            // on `CP.:q(nc:e` -- ten characters of malformed SQL from an
            // author-controlled `view.query`. The guard refuses by INPUT
            // SHAPE, so a refusal is a deterministic function of the text,
            // which a wall-clock budget could not be.
            complexity_guard: Some(ComplexityGuardOptions::default()),
            dialect: DialectType::BigQuery,
            schema: None,
        },
    )
    .map_err(|e| e.to_string())?;

    let reads: Vec<String> = analysis
        .base_tables
        .iter()
        .map(|r| r.name.clone())
        .collect();
    let mut has_unexpanded_star = false;
    let mut columns = Vec::new();
    for projection in &analysis.projections {
        if projection.is_star {
            has_unexpanded_star = true;
            continue;
        }
        let Some(name) = projection.name.clone() else {
            continue;
        };
        let upstream: Vec<(String, String)> = projection
            .upstream
            .iter()
            .filter_map(|u| {
                let table = u.table.clone().or_else(|| u.source_name.clone())?;
                Some((table, u.column.clone()))
            })
            .collect();
        columns.push((name, upstream));
    }
    Ok(SelectFacts {
        reads,
        columns,
        has_unexpanded_star,
    })
}

/// Every table `expr` names, fully qualified as written.
///
/// `polyglot_sql::get_table_names` returns a table's BARE name for an
/// unquoted qualified reference: `p.raw.events` comes back as `events`, while
/// the backticked `` `p.raw.events` `` keeps its qualifiers. Every unquoted
/// read in a routine, job or scheduled query was then resolved against the
/// default project and dataset -- lineage pointing at a different table, with
/// nothing to say so. It still decides WHICH tables count (it already leaves
/// out CTE names, for one); each bare name it returns is expanded here to
/// every qualified spelling of it the statement contains.
fn qualified_table_names(expr: &Expression) -> Vec<String> {
    use polyglot_sql::expressions::TableRef;
    use polyglot_sql::traversal::ExpressionWalk;

    let bare = polyglot_sql::get_table_names(expr);
    if bare.is_empty() {
        return bare;
    }
    fn note<'e>(t: &'e TableRef, qualified: &mut HashMap<&'e str, Vec<String>>) {
        let full = tableref_name(t);
        let spellings = qualified.entry(t.name.name.as_str()).or_default();
        if !spellings.contains(&full) {
            spellings.push(full);
        }
    }
    let mut qualified: HashMap<&str, Vec<String>> = HashMap::new();
    for node in expr.dfs() {
        match node {
            Expression::Table(t) => note(t, &mut qualified),
            Expression::Insert(i) => note(&i.table, &mut qualified),
            Expression::Update(u) => {
                note(&u.table, &mut qualified);
                for t in &u.extra_tables {
                    note(t, &mut qualified);
                }
            }
            Expression::Delete(d) => {
                note(&d.table, &mut qualified);
                for t in d.using.iter().chain(&d.tables) {
                    note(t, &mut qualified);
                }
            }
            Expression::CreateTable(c) => note(&c.name, &mut qualified),
            _ => {}
        }
    }
    let mut out = Vec::with_capacity(bare.len());
    for name in &bare {
        match qualified.get(name.as_str()) {
            Some(spellings) => {
                for full in spellings {
                    if !out.contains(full) {
                        out.push(full.clone());
                    }
                }
            }
            None => {
                if !out.contains(name) {
                    out.push(name.clone());
                }
            }
        }
    }
    out
}

/// Column lineage for a statement that writes a table from a query.
///
/// The query is regenerated as SQL and analysed exactly as a view's is --
/// `analyze_query` takes only a query, not the statement around it.
///
/// - `CREATE TABLE ... AS SELECT` and `CREATE VIEW`: the projections ARE the
///   new object's columns, by name.
/// - `INSERT INTO t (a, b) SELECT ...`: projections map onto the listed
///   columns by position, and only when the counts agree.
///
/// Anything whose target columns cannot be known statically answers `None`
/// rather than a guess: an INSERT without a column list (its order is the
/// table's schema, which is not in the SQL), a `*` projection, or a CTE the
/// parser keeps outside the query, which the regenerated text would lack.
fn column_facts(stmt: &Expression) -> Option<ColumnLineage> {
    let (query, insert_columns) = match stmt {
        Expression::CreateTable(c) if c.with_cte.is_none() => (c.as_select.as_ref()?, None),
        Expression::CreateView(v) if v.columns.is_empty() => (&v.query, None),
        Expression::Insert(i) if i.with.is_none() && !i.columns.is_empty() => {
            (i.query.as_ref()?, Some(&i.columns))
        }
        _ => return None,
    };
    let sql = polyglot_sql::generate(query, DialectType::BigQuery).ok()?;
    guard(&sql).ok()?;
    let facts = analyze_select_inner(&sql).ok()?;
    if facts.has_unexpanded_star {
        return None;
    }
    match insert_columns {
        None => Some(facts.columns),
        Some(targets) => {
            if targets.len() != facts.columns.len() {
                return None;
            }
            Some(
                targets
                    .iter()
                    .zip(facts.columns)
                    .map(|(target, (_, upstream))| (target.name.clone(), upstream))
                    .collect(),
            )
        }
    }
}

fn tableref_name(t: &polyglot_sql::expressions::TableRef) -> String {
    let mut parts = Vec::new();
    if let Some(c) = &t.catalog {
        parts.push(c.name.clone());
    }
    if let Some(s) = &t.schema {
        parts.push(s.name.clone());
    }
    parts.push(t.name.name.clone());
    parts.join(".")
}

/// Extracts table-level facts from one parsed statement. `raw` is the
/// statement's source text (used for CALL detection, which parses as an
/// opaque command).
fn statement_facts(stmt: &Expression, raw: &str) -> StatementFacts {
    use Expression as E;
    let mut facts = StatementFacts::default();

    facts.kind = match stmt {
        E::Select(_)
        | E::Union(_)
        | E::Intersect(_)
        | E::Except(_)
        | E::SetOperation(_)
        | E::With(_)
        | E::Values(_) => StatementKind::Select,
        E::Insert(insert) => {
            facts.writes.push(tableref_name(&insert.table));
            StatementKind::Insert
        }
        E::Update(update) => {
            facts.writes.push(tableref_name(&update.table));
            StatementKind::Update
        }
        E::Delete(delete) => {
            facts.writes.push(tableref_name(&delete.table));
            StatementKind::Delete
        }
        E::Merge(merge) => {
            facts.writes.extend(qualified_table_names(&merge.this));
            StatementKind::Merge
        }
        E::CreateTable(create) => {
            facts.writes.push(tableref_name(&create.name));
            if create.as_select.is_some() {
                StatementKind::CreateTableAsSelect
            } else {
                StatementKind::CreateTable
            }
        }
        E::CreateView(create) => {
            facts.writes.push(tableref_name(&create.name));
            if create.materialized {
                StatementKind::CreateMaterializedView
            } else {
                StatementKind::CreateView
            }
        }
        E::DropTable(drop) => {
            facts.writes.extend(drop.names.iter().map(tableref_name));
            StatementKind::Drop
        }
        E::DropView(drop) => {
            facts.writes.push(tableref_name(&drop.name));
            StatementKind::Drop
        }
        E::DropFunction(_) | E::DropProcedure(_) | E::DropSchema(_) => StatementKind::Drop,
        E::Truncate(truncate) => {
            facts.writes.push(tableref_name(&truncate.table));
            facts.writes.extend(
                truncate
                    .extra_tables
                    .iter()
                    .map(|e| tableref_name(&e.table)),
            );
            StatementKind::Truncate
        }
        E::TruncateTable(truncate) => {
            for e in &truncate.expressions {
                facts.writes.extend(qualified_table_names(e));
            }
            StatementKind::Truncate
        }
        E::AlterTable(alter) => {
            facts.writes.push(tableref_name(&alter.name));
            StatementKind::Alter
        }
        E::AlterView(alter) => {
            facts.writes.push(tableref_name(&alter.name));
            StatementKind::Alter
        }
        E::Export(_) => {
            facts.external_writes = option_strings(raw, b"uri");
            StatementKind::ExportData
        }
        E::LoadData(load) => {
            facts.writes.extend(qualified_table_names(&load.table));
            facts.external_reads.push(load.inpath.clone());
            StatementKind::LoadData
        }
        E::CreateFunction(_) => StatementKind::CreateFunction,
        E::CreateProcedure(_) => StatementKind::CreateProcedure,
        E::Call(_) => StatementKind::Call,
        E::Declare(_)
        | E::DeclareItem(_)
        | E::Set(_)
        | E::SetStatement(_)
        | E::Transaction(_)
        | E::Commit(_)
        | E::Rollback(_)
        | E::ReturnStmt(_) => StatementKind::Scripting,
        // Commands the dialect keeps opaque -- BigQuery's CALL among them --
        // and anything else: classified by their leading keywords.
        E::Command(command) => classify_statement_text(&command.this),
        _ => classify_statement_text(raw),
    };

    // CALL parses dialect-dependently; detect textually.
    // Compared as bytes: `trimmed[..4]` panicked when byte 4 fell inside a
    // character. Once the four bytes are ASCII `call`, byte 4 is a boundary.
    let trimmed = raw.trim_start();
    if trimmed.len() >= 5 && trimmed.as_bytes()[..4].eq_ignore_ascii_case(b"call") {
        if let Some(name) = trimmed[4..]
            .trim_start()
            .split(['(', ' ', '\n', ';'])
            .next()
        {
            if !name.is_empty() {
                facts.calls.push(name.replace('`', ""));
            }
        }
    }

    let all = qualified_table_names(stmt);
    for name in all {
        if !facts.writes.contains(&name) {
            facts.reads.push(name);
        }
    }
    facts
}

/// Splits a script into top-level statements, respecting quotes,
/// backticks, and `--` / `/* */` comments. Strips one surrounding
/// `BEGIN` … `END;` wrapper when present.
pub fn split_statements(script: &str) -> Vec<&str> {
    split_statement_spans(script)
        .into_iter()
        .map(|r| &script[r])
        .collect()
}

/// [`split_statements`] as byte ranges into `script` itself, so a caller
/// can hand a statement on -- or point at where it sits -- without copying
/// it. Every range is non-empty, trimmed, on `char` boundaries, increasing
/// and non-overlapping.
///
/// Boundaries are found by scanning BYTES for ASCII delimiters. An ASCII
/// byte never occurs inside a multi-byte UTF-8 sequence, so every position
/// the scan stops at is a `char` boundary by construction, and slicing
/// there cannot panic -- which matters under `panic = "abort"`, where the
/// script is author text.
pub fn split_statement_spans(script: &str) -> Vec<Range<usize>> {
    let body_span = strip_begin_end_span(script);
    let base = body_span.start;
    let body = &script[body_span];
    let bytes = body.as_bytes();
    let n = bytes.len();
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    // A byte that is not a delimiter costs one table lookup. A quoted
    // string or a comment is crossed by a search for its terminator, which
    // reads a short span inline and hands a long one to memchr, so neither
    // dense statements nor long literals pay for the other's case.
    while let Some(off) = bytes[i..].iter().position(|&b| SPLIT_STOP[b as usize]) {
        i += off;
        let b = bytes[i];
        i = match b {
            b'\'' | b'"' | b'`' => find_byte(b, &bytes[i + 1..]).map_or(n, |k| i + k + 2),
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                find_byte(b'\n', &bytes[i + 2..]).map_or(n, |k| i + 2 + k)
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                find_comment_close(&bytes[i + 2..]).map_or(n, |k| i + k + 4)
            }
            b';' => {
                push_trimmed(body, base, start..i, &mut out);
                start = i + 1;
                i + 1
            }
            _ => i + 1,
        };
    }
    push_trimmed(body, base, start..n, &mut out);
    out
}

/// The bytes [`split_statement_spans`] must stop at outside a quote or a
/// comment: quote openers, the first byte of both comment openers, and `;`.
static SPLIT_STOP: [bool; 256] = {
    let mut t = [false; 256];
    t[b'\'' as usize] = true;
    t[b'"' as usize] = true;
    t[b'`' as usize] = true;
    t[b'-' as usize] = true;
    t[b'/' as usize] = true;
    t[b';' as usize] = true;
    t
};

/// Spans shorter than this are searched inline: below it a call into
/// memchr costs more than the bytes it would skip.
const INLINE_SCAN: usize = 16;

/// The first `needle` in `hay`. The head is read inline, where most
/// terminators in ordinary SQL sit; the rest goes to memchr's vector scan.
#[inline(always)]
fn find_byte(needle: u8, hay: &[u8]) -> Option<usize> {
    let head = hay.len().min(INLINE_SCAN);
    if let Some(k) = hay[..head].iter().position(|&c| c == needle) {
        return Some(k);
    }
    memchr::memchr(needle, &hay[head..]).map(|k| k + head)
}

/// The offset of the first `*/` in `hay`.
#[inline(always)]
fn find_comment_close(hay: &[u8]) -> Option<usize> {
    let mut j = 0;
    while let Some(k) = find_byte(b'*', &hay[j..]) {
        let p = j + k;
        if hay.get(p + 1) == Some(&b'/') {
            return Some(p);
        }
        j = p + 1;
    }
    None
}

/// Pushes `body[r]`, trimmed, as a range offset by `base` -- unless it is
/// empty after trimming.
fn push_trimmed(body: &str, base: usize, r: Range<usize>, out: &mut Vec<Range<usize>>) {
    let piece = &body[r.clone()];
    let lead = piece.len() - piece.trim_start().len();
    let trimmed_len = piece.trim().len();
    if trimmed_len > 0 {
        let s = base + r.start + lead;
        out.push(s..s + trimmed_len);
    }
}

/// The span of `script` inside one surrounding `BEGIN` … `END[;]`, or the
/// whole trimmed script when there is no such wrapper. Allocates nothing.
///
/// This used to lowercase the body and then slice the ORIGINAL with an
/// offset found in the lowercased copy. Lowercasing changes byte lengths --
/// `İ` is two bytes and lowercases to three, the Kelvin sign is three and
/// lowercases to one -- so the offset could land inside a character or past
/// the end, and `BEGIN İ END` aborted the process. It also sliced
/// `trimmed[..5]` before knowing byte 5 was a boundary, so any body whose
/// fifth byte fell inside a character aborted it too. Both comparisons are
/// on bytes now, against ASCII needles, which no non-ASCII text can match.
fn strip_begin_end_span(script: &str) -> Range<usize> {
    let lead = script.len() - script.trim_start().len();
    let trimmed = script.trim();
    let whole = lead..lead + trimmed.len();
    let bytes = trimmed.as_bytes();
    if bytes.len() < 5 || !bytes[..5].eq_ignore_ascii_case(b"begin") {
        return whole;
    }
    // `begin` is ASCII, so byte 5 is a boundary.
    let after_begin = &trimmed[5..];
    let inner_off = 5 + (after_begin.len() - after_begin.trim_start().len());
    let inner = &trimmed[inner_off..];
    let Some(pos) = rfind_ascii_ci(inner.as_bytes(), b"end") else {
        return whole;
    };
    // `end` is ASCII, so `pos` and `pos + 3` are boundaries.
    let after = inner[pos + 3..].trim();
    if after.is_empty() || after == ";" {
        let s = lead + inner_off;
        s..s + pos
    } else {
        whole
    }
}

/// Last occurrence of an ASCII `needle` in `hay`, compared ASCII
/// case-insensitively, as a byte offset.
fn rfind_ascii_ci(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    (0..=hay.len() - needle.len())
        .rev()
        .find(|&i| hay[i..i + needle.len()].eq_ignore_ascii_case(needle))
}

/// The kind of a statement read from its leading keywords alone, for text
/// the parser kept opaque or could not parse. Skips leading whitespace and
/// comments; allocates nothing.
pub fn classify_statement_text(sql: &str) -> StatementKind {
    let mut words = Keywords::new(sql);
    let Some(first) = words.next() else {
        return StatementKind::Other;
    };
    let is = |w: &str, k: &str| w.eq_ignore_ascii_case(k);
    if is(first, "select") || is(first, "with") || first.starts_with('(') {
        return StatementKind::Select;
    }
    for (keyword, kind) in [
        ("insert", StatementKind::Insert),
        ("update", StatementKind::Update),
        ("delete", StatementKind::Delete),
        ("merge", StatementKind::Merge),
        ("drop", StatementKind::Drop),
        ("truncate", StatementKind::Truncate),
        ("alter", StatementKind::Alter),
        ("export", StatementKind::ExportData),
        ("load", StatementKind::LoadData),
        ("call", StatementKind::Call),
    ] {
        if is(first, keyword) {
            return kind;
        }
    }
    if is(first, "create") {
        // CREATE [OR REPLACE] [TEMP|TEMPORARY] [MATERIALIZED|TABLE|AGGREGATE]
        // ... -- the first word that names the object decides.
        for word in words {
            if is(word, "table") {
                return StatementKind::CreateTable;
            }
            if is(word, "materialized") {
                return StatementKind::CreateMaterializedView;
            }
            if is(word, "view") {
                return StatementKind::CreateView;
            }
            if is(word, "function") {
                return StatementKind::CreateFunction;
            }
            if is(word, "procedure") {
                return StatementKind::CreateProcedure;
            }
            if !(is(word, "or")
                || is(word, "replace")
                || is(word, "temp")
                || is(word, "temporary")
                || is(word, "aggregate")
                || is(word, "if")
                || is(word, "not")
                || is(word, "exists"))
            {
                break;
            }
        }
        return StatementKind::Other;
    }
    const SCRIPTING: &[&str] = &[
        "declare", "set", "if", "begin", "end", "loop", "while", "repeat", "for", "return",
        "raise", "execute", "leave", "continue", "iterate", "break", "assert", "commit",
        "rollback", "case",
    ];
    if SCRIPTING.iter().any(|k| is(first, k)) {
        return StatementKind::Scripting;
    }
    StatementKind::Other
}

/// The leading words of a statement: runs of ASCII alphanumerics and `_`,
/// or a single `(`, with whitespace and `--` / `/* */` comments skipped. Every
/// boundary is an ASCII byte, so every word is a valid slice.
struct Keywords<'a> {
    rest: &'a str,
}

impl<'a> Keywords<'a> {
    fn new(sql: &'a str) -> Self {
        Self { rest: sql }
    }
}

impl<'a> Iterator for Keywords<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<&'a str> {
        loop {
            let trimmed = self.rest.trim_start();
            if let Some(after) = trimmed.strip_prefix("--") {
                self.rest = after.find('\n').map_or("", |i| &after[i..]);
                continue;
            }
            if let Some(after) = trimmed.strip_prefix("/*") {
                self.rest = after.find("*/").map_or("", |i| &after[i + 2..]);
                continue;
            }
            self.rest = trimmed;
            break;
        }
        let bytes = self.rest.as_bytes();
        let first = *bytes.first()?;
        let len = if first == b'(' {
            1
        } else {
            bytes
                .iter()
                .position(|b| !(b.is_ascii_alphanumeric() || *b == b'_'))
                .unwrap_or(bytes.len())
        };
        if len == 0 {
            return None;
        }
        let (word, rest) = self.rest.split_at(len);
        self.rest = rest;
        Some(word)
    }
}

/// The string values given to `key` in a statement's options, e.g.
/// `uri = 'gs://b/*.csv'` or `uris = ['gs://a', "gs://b"]`. The key matches
/// ASCII case-insensitively at a word boundary. Text inside quotes or
/// comments is never mistaken for a key.
pub(crate) fn option_strings(sql: &str, key: &[u8]) -> Vec<String> {
    let bytes = sql.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' | b'"' | b'`' => i = skip_quoted(bytes, i),
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
            }
            _ => {
                let boundary_before =
                    i == 0 || !(bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
                let end = i + key.len();
                let matches = boundary_before
                    && end <= bytes.len()
                    && bytes[i..end].eq_ignore_ascii_case(key)
                    && bytes
                        .get(end)
                        .is_none_or(|b| !(b.is_ascii_alphanumeric() || *b == b'_'));
                if matches {
                    let mut j = end;
                    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                        j += 1;
                    }
                    if bytes.get(j) == Some(&b'=') {
                        j += 1;
                        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                            j += 1;
                        }
                        i = read_option_values(sql, j, &mut out);
                        continue;
                    }
                }
                i += 1;
            }
        }
    }
    out
}

/// Reads one quoted string, or a `[...]` list of them, starting at `at`;
/// returns where reading stopped.
fn read_option_values(sql: &str, at: usize, out: &mut Vec<String>) -> usize {
    let bytes = sql.as_bytes();
    let mut i = at;
    let list = bytes.get(i) == Some(&b'[');
    if list {
        i += 1;
    }
    loop {
        while i < bytes.len() && (bytes[i].is_ascii_whitespace() || bytes[i] == b',') {
            i += 1;
        }
        match bytes.get(i) {
            Some(&q) if q == b'\'' || q == b'"' => {
                let end = skip_quoted(bytes, i);
                // `skip_quoted` stops after the closing quote, or at the end.
                let close = if end <= bytes.len() && end > i + 1 && bytes[end - 1] == q {
                    end - 1
                } else {
                    end
                };
                out.push(sql[i + 1..close.max(i + 1)].to_string());
                i = end;
                if !list {
                    return i;
                }
            }
            Some(b']') if list => return i + 1,
            _ => return i,
        }
    }
}

/// Index just past the quoted run starting at `start` (whose byte is the
/// quote), honouring backslash escapes; the end of input if unterminated.
fn skip_quoted(bytes: &[u8], start: usize) -> usize {
    let q = bytes[start];
    let mut i = start + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b if b == q => return i + 1,
            _ => i += 1,
        }
    }
    bytes.len()
}

/// The target of a BigQuery `LOAD DATA {INTO|OVERWRITE} [TEMP TABLE] name`,
/// which the parser does not accept. `None` when the text is not that shape.
pub(crate) fn load_data_target(sql: &str) -> Option<String> {
    let mut words = Keywords::new(sql);
    let is = |w: Option<&str>, k: &str| w.is_some_and(|w| w.eq_ignore_ascii_case(k));
    if !is(words.next(), "load") || !is(words.next(), "data") {
        return None;
    }
    let mode = words.next();
    if !(is(mode, "into") || is(mode, "overwrite")) {
        return None;
    }
    // What follows the mode keyword: an optional TEMP/TEMPORARY TABLE, then
    // the name -- backticked, or a dotted run of identifier characters.
    let mut rest = words.rest.trim_start();
    for _ in 0..2 {
        let mut probe = Keywords::new(rest);
        match probe.next() {
            Some(w)
                if w.eq_ignore_ascii_case("temp")
                    || w.eq_ignore_ascii_case("temporary")
                    || w.eq_ignore_ascii_case("table") =>
            {
                rest = probe.rest.trim_start();
            }
            _ => break,
        }
    }
    let bytes = rest.as_bytes();
    let name = if bytes.first() == Some(&b'`') {
        let end = rest[1..].find('`')? + 1;
        &rest[1..end]
    } else {
        let len = bytes
            .iter()
            .position(|b| !(b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-')))
            .unwrap_or(bytes.len());
        &rest[..len]
    };
    (!name.is_empty()).then(|| name.to_string())
}

/// Heuristic fallback when parsing fails: scans comment- and
/// string-stripped SQL for backticked or bare three-part dotted names.
pub(crate) fn heuristic_table_refs(sql: &str) -> Vec<String> {
    let stripped = strip_comments_and_strings(sql);
    let mut out = Vec::new();
    // Backticked names survive stripping as placeholders — collect from
    // the original within backticks first.
    let bytes = sql.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'`' {
            if let Some(end) = sql[i + 1..].find('`') {
                let inner = &sql[i + 1..i + 1 + end];
                if inner.matches('.').count() == 2 && !inner.contains(' ') {
                    out.push(inner.to_string());
                }
                i += end + 2;
                continue;
            }
        }
        i += 1;
    }
    // Bare dotted three-part names.
    for token in stripped.split(|c: char| c.is_whitespace() || "(),;".contains(c)) {
        if token.matches('.').count() == 2
            && token
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-')
            && !token.starts_with('.')
            && !token.ends_with('.')
        {
            out.push(token.to_string());
        }
    }
    out.sort();
    out.dedup();
    out
}

fn strip_comments_and_strings(sql: &str) -> String {
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\'' | b'"' | b'`' => {
                let q = bytes[i];
                i += 1;
                while i < bytes.len() && bytes[i] != q {
                    i += 1;
                }
                i += 1;
                out.push(' ');
            }
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
            }
            b => {
                out.push(b as char);
                i += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    /// The input the scheduled fuzz run found, which used to never return.
    ///
    /// `polyglot_sql::parse("CP.:q(nc:e", BigQuery)` did not terminate on
    /// 0.6.2 -- ten characters, reduced from the 34 the fuzzer found, and a
    /// single unclosed paren after an identifier carrying both `.` and `:` is
    /// enough. The SQL reaching here comes from an author's `view.query`, so
    /// it is untrusted by construction.
    ///
    /// 0.6.3 still hangs; 0.13.1 reports it as the parse error it always was.
    /// A test that takes a second to fail is still a pass, so what is asserted
    /// is that it RETURNS -- the bug was unboundedness, not a wrong answer.
    ///
    /// The fourth shape is a SECOND, independently found reproducer: the
    /// weekly fuzz budget was raised to 120 seconds and the next run drew a
    /// 25-byte input out of the same family -- a non-ASCII byte and a CRLF in
    /// front, but the same identifier carrying `.` and `:` ahead of an
    /// unclosed paren. Replayed here, 0.6.2 was still running after 90
    /// seconds with resident memory climbing; 0.13.1 refuses it in 0.01s.
    /// Two independent discoveries of one shape is why the pin is the fix
    /// rather than a filter on the input.
    #[test]
    fn the_fuzz_found_input_terminates() {
        for sql in [
            "CP.:q(nc:e",
            "ECP.:qrotableIdjrotedc((nc:e\n    ",
            "CP.:q((nc:e",
            "tgcabyamleId\u{ef}gcn.:em\r\n( ",
        ] {
            let r = one_statement(sql);
            assert!(r.is_err(), "{sql:?} is malformed and must be refused");
        }
    }

    /// The complexity guard is ENABLED. `complexity_guard: None` disables every
    /// limit, so this pins that the analysis path refuses a pathological shape
    /// rather than attempting it.
    #[test]
    fn a_pathological_nesting_depth_is_refused() {
        let deep = format!("SELECT {}1{}", "(".repeat(600), ")".repeat(600));
        let err = analyze_select(&deep).expect_err("600 levels must be refused");
        assert!(
            err.contains("GUARD") || err.to_lowercase().contains("depth"),
            "the refusal should name the guard that fired, got {err:?}"
        );
    }

    /// Valid SQL is unaffected by the guard -- the case that matters most,
    /// since a guard that refused real queries would be worse than the hang.
    #[test]
    fn ordinary_sql_still_parses_under_the_guard() {
        let facts = analyze_select("SELECT a.id, b.v FROM `p.d.t` a JOIN `p.d.u` b ON a.id = b.id")
            .expect("ordinary SQL parses");
        assert_eq!(
            facts.reads.len(),
            2,
            "both tables are read: {:?}",
            facts.reads
        );
    }

    /// One parser thread for a whole script, not one per statement.
    ///
    /// Nothing bounds the statement count -- `MAX_SQL_BYTES` bounds the text
    /// and `split_statements` only drops the EMPTY ones -- so the per-statement
    /// entry point let 1 MiB of `select 1 from t;` demand ~65,000 thread
    /// spawns at 64 MiB of stack each. Counted, not timed: the batch call
    /// returns one result per statement from a single worker.
    #[test]
    fn a_whole_script_is_parsed_on_one_worker() {
        let stmts: Vec<String> = (0..500)
            .map(|i| format!("SELECT {i} FROM `p.d.t{i}`"))
            .collect();
        let refs: Vec<&str> = stmts.iter().map(String::as_str).collect();
        let out = statement_facts_for_all(&refs);
        assert_eq!(out.len(), refs.len(), "one result per statement, in order");
        assert!(
            out.iter().all(|r| r.is_ok()),
            "every statement is ordinary SQL and must parse"
        );
    }

    /// An oversized statement is refused per statement, so one bad statement
    /// in a batch does not cost the others their results.
    #[test]
    fn one_oversized_statement_does_not_poison_the_batch() {
        let big = "SELECT ".to_string() + &"a,".repeat(MAX_SQL_BYTES / 2 + 8) + "1";
        let good = "SELECT 1 FROM `p.d.t`";
        let out = statement_facts_for_all(&[good, big.as_str(), good]);
        assert_eq!(out.len(), 3);
        assert!(out[0].is_ok(), "{:?}", out[0]);
        assert!(out[1].is_err(), "the oversized one is refused");
        assert!(out[2].is_ok(), "{:?}", out[2]);
    }

    use crate::deep_stack::current_worker;

    /// One worker serves every call on a thread: the second parse reaches
    /// the thread the first one spawned.
    #[test]
    fn the_parser_worker_is_reused_across_calls() {
        assert!(statement_facts_for_all(&["SELECT 1"])[0].is_ok());
        let first = current_worker().expect("a worker after the first parse");
        for _ in 0..5 {
            assert!(statement_facts_for_all(&["SELECT 2 FROM `p.d.t`"])[0].is_ok());
        }
        assert_eq!(
            current_worker(),
            Some(first),
            "the same worker, not a new one per call"
        );
    }

    /// A dead worker is cleared from its slot, so the next call spawns a
    /// fresh one instead of failing forever.
    #[test]
    fn a_dead_parser_worker_is_cleared_and_replaced() {
        let dead = crate::deep_stack::install_dead_worker();
        assert!(
            statement_facts_for_all(&["SELECT 1"])[0].is_err(),
            "the dead worker is reported"
        );
        assert!(current_worker().is_none(), "and cleared from the slot");
        assert!(
            statement_facts_for_all(&["SELECT 1"])[0].is_ok(),
            "the next call spawns anew"
        );
        assert_ne!(current_worker(), Some(dead));
    }

    /// An empty batch spawns nothing at all.
    #[test]
    fn an_empty_batch_spawns_no_worker() {
        assert!(statement_facts_for_all(&[]).is_empty());
    }

    /// One statement through the batch entry point, so these cases keep
    /// reading as single-statement tests without a second implementation
    /// existing to drift from it.
    fn one_statement(sql: &str) -> Result<Vec<super::StatementFacts>, String> {
        super::statement_facts_for_all(&[sql])
            .into_iter()
            .next()
            .expect("one input yields one result")
    }

    use super::*;

    #[test]
    fn analyze_select_columns_and_reads() {
        let facts = analyze_select(
            "SELECT o.user_id, SUM(o.amount) AS total FROM `p.raw.orders` o GROUP BY o.user_id",
        )
        .expect("analyzed");
        assert_eq!(facts.reads, vec!["p.raw.orders"]);
        assert_eq!(facts.columns.len(), 2);
        assert_eq!(facts.columns[0].0, "user_id");
        assert_eq!(
            facts.columns[0].1,
            vec![("p.raw.orders".to_string(), "user_id".to_string())]
        );
        assert!(!facts.has_unexpanded_star);
    }

    #[test]
    fn analyze_select_star_flagged() {
        let facts = analyze_select("SELECT * FROM `p.d.t`").expect("analyzed");
        assert!(facts.has_unexpanded_star);
        assert_eq!(facts.reads, vec!["p.d.t"]);
    }

    #[test]
    fn statement_facts_insert_and_merge() {
        let facts = one_statement("INSERT INTO `p.mart.daily` SELECT dt FROM `p.raw.events`")
            .expect("parsed");
        assert_eq!(facts[0].writes, vec!["p.mart.daily"]);
        assert_eq!(facts[0].reads, vec!["p.raw.events"]);
    }

    #[test]
    fn statement_facts_call() {
        let raw = "CALL `p.ds.refresh_mart`()";
        if let Ok(facts) = one_statement(raw) {
            if let Some(first) = facts.first() {
                assert_eq!(first.calls, vec!["p.ds.refresh_mart"]);
                return;
            }
        }
        // If the dialect rejects CALL entirely, the heuristic path still
        // surfaces the routine name.
        assert!(heuristic_table_refs(raw).contains(&"p.ds.refresh_mart".to_string()));
    }

    #[test]
    fn split_statements_quotes_comments_begin_end() {
        let script = "BEGIN\nINSERT INTO `p.d.a` VALUES ('x;y'); -- trailing; comment\n/* block; */ DELETE FROM `p.d.b` WHERE id = 1;\nEND;";
        let stmts = split_statements(script);
        assert_eq!(stmts.len(), 2);
        assert!(stmts[0].starts_with("INSERT"));
        assert!(stmts[1].contains("DELETE"));
    }

    #[test]
    fn heuristic_refs_skip_comments_and_strings() {
        let refs = heuristic_table_refs(
            "SELECT x FROM `p.d.real` -- p.d.commented\nWHERE name = 'p.d.string' AND y IN (SELECT z FROM other.ds.bare)",
        );
        assert_eq!(refs, vec!["other.ds.bare", "p.d.real"]);
    }

    #[test]
    fn oversized_sql_skipped() {
        let big = format!("SELECT '{}'", "x".repeat(MAX_SQL_BYTES));
        assert!(one_statement(&big).is_err());
        assert!(analyze_select(&big).is_err());
    }

    #[test]
    fn deeply_nested_sql_parses_without_overflow() {
        // SECURITY REGRESSION TEST: this input aborted the process before
        // `stacker` + the parser worker thread were in place (unoptimized
        // builds overflowed around nesting depth 14).
        let deep = format!("SELECT {}1{}", "(".repeat(400), ")".repeat(400));
        assert!(analyze_select(&deep).is_ok(), "deep nesting must not abort");
        let ok = "SELECT ((((1)))) AS v FROM `p.d.t`";
        assert!(analyze_select(ok).is_ok());
    }

    /// The splitter as it shipped through 0.5.33, kept as an oracle. It is
    /// only fed inputs it could not panic on (ASCII), where the new splitter
    /// must agree with it exactly.
    fn split_statements_oracle(script: &str) -> Vec<&str> {
        fn strip(script: &str) -> &str {
            let trimmed = script.trim();
            if trimmed.len() >= 5 && trimmed[..5].eq_ignore_ascii_case("begin") {
                let inner = trimmed[5..].trim_start();
                let lower = inner.to_lowercase();
                if let Some(pos) = lower.rfind("end") {
                    let after = inner[pos + 3..].trim();
                    if after.is_empty() || after == ";" {
                        return &inner[..pos];
                    }
                }
            }
            trimmed
        }
        let body = strip(script);
        let bytes = body.as_bytes();
        let mut out = Vec::new();
        let (mut start, mut i, mut quote) = (0usize, 0usize, None::<u8>);
        while i < bytes.len() {
            let b = bytes[i];
            match quote {
                Some(q) => {
                    if b == q {
                        quote = None;
                    }
                    i += 1;
                }
                None => match b {
                    b'\'' | b'"' | b'`' => {
                        quote = Some(b);
                        i += 1;
                    }
                    b'-' if bytes.get(i + 1) == Some(&b'-') => {
                        while i < bytes.len() && bytes[i] != b'\n' {
                            i += 1;
                        }
                    }
                    b'/' if bytes.get(i + 1) == Some(&b'*') => {
                        i += 2;
                        while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                            i += 1;
                        }
                        i = (i + 2).min(bytes.len());
                    }
                    b';' => {
                        let stmt = body[start..i].trim();
                        if !stmt.is_empty() {
                            out.push(stmt);
                        }
                        start = i + 1;
                        i += 1;
                    }
                    _ => i += 1,
                },
            }
        }
        let tail = body[start..].trim();
        if !tail.is_empty() {
            out.push(tail);
        }
        out
    }

    #[test]
    fn spans_agree_with_the_shipped_splitter_on_ascii() {
        let corpus = [
            "",
            "   ",
            ";;;",
            "SELECT 1",
            "SELECT 1;",
            " SELECT 1 ; SELECT 2 ;; ",
            "BEGIN\nINSERT INTO `p.d.a` VALUES ('x;y'); -- c;\n/* b; */ DELETE FROM `p.d.b`;\nEND;",
            "begin select 1; select 2 end",
            "BEGIN select 1; END extra",
            "BEGIN",
            "BEGINEND",
            "BEGIN END;",
            "SELECT 'it''s; fine'; SELECT \"a;b\"",
            "SELECT 1 -- no newline; at end",
            "SELECT 1 /* unterminated ; comment",
            "SELECT 'unterminated ; quote",
            "BEGIN\r\n  SELECT 1;\r\n  SELECT 2;\r\nEND;\r\n",
            "BEGIN SELECT 'end'; END",
            "BEGIN SELECT legend; END",
        ];
        for script in corpus {
            let spans = split_statement_spans(script);
            let sliced: Vec<&str> = spans.iter().map(|r| &script[r.clone()]).collect();
            assert_eq!(sliced, split_statements_oracle(script), "script {script:?}");
            assert_eq!(split_statements(script), sliced);
            let mut prev_end = 0;
            for r in &spans {
                assert!(r.start >= prev_end && r.start < r.end && r.end <= script.len());
                prev_end = r.end;
            }
        }
    }

    #[test]
    fn begin_end_with_dotted_capital_i_does_not_panic() {
        // SECURITY REGRESSION: `İ` is two bytes and lowercases to three, so an
        // offset found in the lowercased body sliced the original past its end.
        let script = "BEGIN İ END";
        let stmts = split_statements(script);
        assert_eq!(stmts, vec!["İ"]);
    }

    #[test]
    fn kelvin_sign_before_end_does_not_panic() {
        // The Kelvin sign is three bytes and lowercases to one: the offset
        // landed before the real `END`, inside the character.
        let script = "BEGIN SELECT '\u{212A}' FROM t; END";
        let stmts = split_statements(script);
        assert_eq!(stmts, vec!["SELECT '\u{212A}' FROM t"]);
        let script = "BEGIN \u{212A}\u{212A}\u{212A}\u{212A} END;";
        assert_eq!(
            split_statements(script),
            vec!["\u{212A}\u{212A}\u{212A}\u{212A}"]
        );
    }

    #[test]
    fn a_multibyte_fifth_byte_does_not_panic() {
        // `trimmed[..5]` was sliced before byte 5 was known to be a boundary.
        for script in ["ąąą", "aąąx SELECT 1", "żółw; SELECT 1", "日本語のテキスト"]
        {
            let spans = split_statement_spans(script);
            for r in spans {
                assert!(script.is_char_boundary(r.start) && script.is_char_boundary(r.end));
            }
        }
    }

    #[test]
    fn call_detection_with_a_multibyte_fourth_byte_does_not_panic() {
        // `trimmed[..4]` panicked when byte 4 fell inside a character.
        // `aąą x` parses (as an aliased identifier), so it reaches CALL
        // detection. A panic there killed the worker, and the dead worker was
        // indistinguishable from a parse error -- the facts were just gone.
        let facts = statement_facts_for_all(&["aąą x"]);
        assert_eq!(facts.len(), 1);
        assert!(
            facts[0].is_ok(),
            "worker died: {:?}",
            facts[0].as_ref().err()
        );
    }

    fn facts_of(sql: &str) -> StatementFacts {
        let mut out = statement_facts_for_all(&[sql]);
        let mut stmts = out
            .remove(0)
            .unwrap_or_else(|e| panic!("{sql:?} did not parse: {e}"));
        assert_eq!(stmts.len(), 1, "{sql:?}");
        stmts.remove(0)
    }

    #[test]
    fn drop_truncate_alter_change_their_table_and_read_nothing() {
        for (sql, kind) in [
            ("DROP TABLE `p.d.t`", StatementKind::Drop),
            ("DROP VIEW `p.d.t`", StatementKind::Drop),
            ("TRUNCATE TABLE `p.d.t`", StatementKind::Truncate),
            (
                "ALTER TABLE `p.d.t` ADD COLUMN c INT64",
                StatementKind::Alter,
            ),
        ] {
            let f = facts_of(sql);
            assert_eq!(f.kind, kind, "{sql}");
            assert_eq!(f.writes, vec!["p.d.t".to_string()], "{sql}");
            assert!(f.reads.is_empty(), "{sql} reported reads {:?}", f.reads);
        }
    }

    #[test]
    fn dml_and_ddl_kinds() {
        for (sql, kind) in [
            ("SELECT 1", StatementKind::Select),
            (
                "WITH a AS (SELECT 1) SELECT * FROM a",
                StatementKind::Select,
            ),
            (
                "INSERT INTO `p.d.t` SELECT * FROM `p.d.s`",
                StatementKind::Insert,
            ),
            ("UPDATE `p.d.t` SET a = 1 WHERE TRUE", StatementKind::Update),
            ("DELETE FROM `p.d.t` WHERE TRUE", StatementKind::Delete),
            (
                "MERGE `p.d.t` T USING `p.d.s` S ON T.id = S.id WHEN MATCHED THEN DELETE",
                StatementKind::Merge,
            ),
            ("CREATE TABLE `p.d.t` (a INT64)", StatementKind::CreateTable),
            (
                "CREATE TABLE `p.d.t` AS SELECT * FROM `p.d.s`",
                StatementKind::CreateTableAsSelect,
            ),
            (
                "CREATE VIEW `p.d.v` AS SELECT * FROM `p.d.s`",
                StatementKind::CreateView,
            ),
            (
                "CREATE MATERIALIZED VIEW `p.d.v` AS SELECT * FROM `p.d.s`",
                StatementKind::CreateMaterializedView,
            ),
        ] {
            assert_eq!(facts_of(sql).kind, kind, "{sql}");
        }
    }

    #[test]
    fn ctas_writes_its_table_and_reads_its_source() {
        let f = facts_of("CREATE OR REPLACE TABLE `p.d.t` AS SELECT a FROM `p.d.s`");
        assert_eq!(f.writes, vec!["p.d.t".to_string()]);
        assert_eq!(f.reads, vec!["p.d.s".to_string()]);
    }

    #[test]
    fn export_data_reads_its_query_and_writes_its_uri() {
        let f = facts_of(
            "EXPORT DATA OPTIONS (uri = 'gs://bucket/out/*.csv', format = 'CSV') AS SELECT a FROM `p.d.s`",
        );
        assert_eq!(f.kind, StatementKind::ExportData);
        assert_eq!(f.reads, vec!["p.d.s".to_string()]);
        assert!(f.writes.is_empty());
        assert_eq!(f.external_writes, vec!["gs://bucket/out/*.csv".to_string()]);
    }

    #[test]
    fn bigquery_call_is_a_call() {
        let f = facts_of("CALL `p.d.proc`(1)");
        assert_eq!(f.kind, StatementKind::Call);
        assert_eq!(f.calls, vec!["p.d.proc".to_string()]);
    }

    #[test]
    fn classify_statement_text_reads_leading_keywords() {
        for (sql, kind) in [
            ("", StatementKind::Other),
            ("   -- only a comment", StatementKind::Other),
            ("-- note\n/* block */ select 1", StatementKind::Select),
            ("(SELECT 1)", StatementKind::Select),
            (
                "load data into p.d.t from files (uris = ['gs://b/x'])",
                StatementKind::LoadData,
            ),
            (
                "CREATE OR REPLACE TEMP TABLE t AS SELECT 1",
                StatementKind::CreateTable,
            ),
            (
                "create materialized view v as select 1",
                StatementKind::CreateMaterializedView,
            ),
            (
                "CREATE AGGREGATE FUNCTION f()",
                StatementKind::CreateFunction,
            ),
            (
                "CREATE PROCEDURE p() BEGIN END",
                StatementKind::CreateProcedure,
            ),
            ("CREATE SCHEMA s", StatementKind::Other),
            ("DECLARE x INT64", StatementKind::Scripting),
            ("IF x THEN SELECT 1; END IF", StatementKind::Scripting),
            (
                "EXPORT DATA OPTIONS(uri='gs://x') AS SELECT 1",
                StatementKind::ExportData,
            ),
            ("ąąą", StatementKind::Other),
        ] {
            assert_eq!(classify_statement_text(sql), kind, "{sql:?}");
        }
    }

    #[test]
    fn option_strings_reads_values_and_skips_quotes_and_comments() {
        assert_eq!(
            option_strings("OPTIONS (URI = 'gs://a/*', format='CSV')", b"uri"),
            vec!["gs://a/*".to_string()]
        );
        assert_eq!(
            option_strings(
                "FROM FILES (format = 'CSV', uris = ['gs://a/1', \"gs://a/2\"])",
                b"uris"
            ),
            vec!["gs://a/1".to_string(), "gs://a/2".to_string()]
        );
        // `uri` inside a string or a comment, or as part of a longer word, is
        // not the option.
        assert!(option_strings("SELECT 'uri = x' -- uri = 'y'\n", b"uri").is_empty());
        assert!(option_strings("myuri = 'z'", b"uri").is_empty());
        // Unterminated input ends the scan rather than reading past it.
        assert_eq!(
            option_strings("uri = 'gs://open", b"uri"),
            vec!["gs://open".to_string()]
        );
    }

    #[test]
    fn load_data_target_reads_the_bigquery_shapes() {
        for (sql, want) in [
            (
                "LOAD DATA INTO p.d.t FROM FILES (uris=['gs://b/x'])",
                Some("p.d.t"),
            ),
            (
                "load data overwrite `p.d.t` from files(uris=['x'])",
                Some("p.d.t"),
            ),
            (
                "LOAD DATA INTO TEMP TABLE t FROM FILES (uris=['x'])",
                Some("t"),
            ),
            ("LOAD DATA LOCAL INPATH 'x' INTO TABLE t", None),
            ("SELECT 1", None),
        ] {
            assert_eq!(load_data_target(sql).as_deref(), want, "{sql}");
        }
    }

    #[test]
    fn unquoted_qualified_names_keep_their_qualifiers() {
        // `get_table_names` answers `events` for an unquoted `p.raw.events`;
        // the reads must not lose the project and dataset.
        let f = facts_of("SELECT id FROM p.raw.events JOIN raw.users USING (id)");
        assert_eq!(
            f.reads,
            vec!["p.raw.events".to_string(), "raw.users".to_string()]
        );
        let f = facts_of("INSERT INTO marts.t SELECT * FROM raw.s");
        assert_eq!(f.writes, vec!["marts.t".to_string()]);
        assert_eq!(f.reads, vec!["raw.s".to_string()]);
        let f = facts_of("MERGE marts.t T USING raw.s S ON T.id = S.id WHEN MATCHED THEN DELETE");
        assert_eq!(f.writes, vec!["marts.t".to_string()]);
        assert_eq!(f.reads, vec!["raw.s".to_string()]);
    }

    #[test]
    fn two_tables_sharing_a_bare_name_are_both_read() {
        let f = facts_of("SELECT * FROM a.d.t JOIN b.d.t USING (id)");
        assert_eq!(f.reads, vec!["a.d.t".to_string(), "b.d.t".to_string()]);
    }

    #[test]
    fn a_cte_is_not_a_table() {
        let f = facts_of("WITH x AS (SELECT * FROM p.raw.s) SELECT * FROM x");
        assert_eq!(f.reads, vec!["p.raw.s".to_string()]);
    }

    fn columns_of(sql: &str) -> ColumnLineage {
        let mut out = statement_facts_with_columns(&[sql], true);
        let mut stmts = out.remove(0).unwrap_or_else(|e| panic!("{sql:?}: {e}"));
        stmts.remove(0).columns
    }

    fn sources(cols: &ColumnLineage, col: &str) -> Vec<(String, String)> {
        cols.iter()
            .find(|(c, _)| c == col)
            .map(|(_, s)| s.clone())
            .unwrap_or_else(|| panic!("no column {col}; have {cols:?}"))
    }

    #[test]
    fn ctas_columns_are_its_projections() {
        let cols = columns_of(
            "CREATE TABLE `p.m.t` AS SELECT a.id, b.v AS value FROM `p.r.a` a JOIN `p.r.b` b ON a.id = b.id",
        );
        assert_eq!(
            sources(&cols, "id"),
            vec![("p.r.a".to_string(), "id".to_string())]
        );
        assert_eq!(
            sources(&cols, "value"),
            vec![("p.r.b".to_string(), "v".to_string())]
        );
    }

    #[test]
    fn insert_with_a_column_list_maps_by_position() {
        let cols = columns_of("INSERT INTO `p.m.t` (k, total) SELECT id, amount FROM `p.r.s`");
        assert_eq!(
            sources(&cols, "k"),
            vec![("p.r.s".to_string(), "id".to_string())]
        );
        assert_eq!(
            sources(&cols, "total"),
            vec![("p.r.s".to_string(), "amount".to_string())]
        );
    }

    #[test]
    fn create_view_in_a_script_has_column_lineage() {
        let cols = columns_of("CREATE VIEW `p.m.v` AS SELECT id FROM `p.r.s`");
        assert_eq!(
            sources(&cols, "id"),
            vec![("p.r.s".to_string(), "id".to_string())]
        );
    }

    #[test]
    fn unknowable_targets_answer_nothing_rather_than_a_guess() {
        for sql in [
            // No column list: the target order is the table's schema.
            "INSERT INTO `p.m.t` SELECT id, amount FROM `p.r.s`",
            // A star cannot be expanded without a schema.
            "CREATE TABLE `p.m.t` AS SELECT * FROM `p.r.s`",
            // Counts disagree.
            "INSERT INTO `p.m.t` (a, b, c) SELECT id, amount FROM `p.r.s`",
            // Not a query-backed write at all.
            "DELETE FROM `p.m.t` WHERE TRUE",
            "UPDATE `p.m.t` SET a = 1 WHERE TRUE",
        ] {
            assert!(columns_of(sql).is_empty(), "{sql}");
        }
    }

    #[test]
    fn columns_are_only_computed_when_asked_for() {
        let mut out = statement_facts_with_columns(
            &["CREATE TABLE `p.m.t` AS SELECT id FROM `p.r.s`"],
            false,
        );
        assert!(out.remove(0).expect("parsed")[0].columns.is_empty());
    }
}

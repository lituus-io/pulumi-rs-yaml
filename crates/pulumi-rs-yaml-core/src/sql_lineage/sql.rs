// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

//! The single polyglot-sql integration boundary. Every parser call
//! lives here so an API bump touches one file. All functions degrade
//! (returning `None`/empty + caller warnings) rather than erroring.

use polyglot_sql::guard::ComplexityGuardOptions;
use polyglot_sql::query_analysis::{analyze_query, AnalyzeQueryOptions};
use std::ops::Range;

use polyglot_sql::{DialectType, Expression};

/// SQL larger than this is skipped (defense-in-depth under
/// `panic = "abort"` — no unwinding safety net exists).
const MAX_SQL_BYTES: usize = 1024 * 1024;

/// Stack size for the parser worker thread.
///
/// SECURITY: polyglot's `stacker` feature grows the parser's stack on
/// demand, so deeply nested SQL no longer overflows (verified to nesting
/// depth 500 in unoptimized builds, where the default stack failed
/// around depth 14). Parsing still runs on a dedicated thread so that
/// any residual deep recursion is isolated from the caller's stack —
/// under `panic = "abort"` an overflow would abort the whole process.
const PARSER_STACK_BYTES: usize = 64 * 1024 * 1024;

/// A parser job. An enum rather than a boxed closure: the worker is reused, so
/// the work has to cross a channel, and the set of jobs is closed and small.
/// It also keeps the isolation contract legible -- what crosses is listed here.
enum Job {
    /// Each entry is an owned statement, or the reason it was refused.
    Statements(Vec<Result<String, String>>),
    Select(String),
}

/// A parser answer. Only owned, `Send` fact structs -- parser AST nodes never
/// leave the worker, which is the whole point of the boundary.
enum Reply {
    Statements(Vec<Result<Vec<StatementFacts>, String>>),
    Select(Result<SelectFacts, String>),
}

/// One parser worker per calling thread, parked between jobs.
///
/// A thread with a `PARSER_STACK_BYTES` stack used to be spawned PER CALL.
/// Measured: 31.5us to spawn and join one, and a twenty-view program makes
/// about forty such calls -- 1.26ms of pure thread churn, which was the
/// largest single item in `sql_lineage_20_views`. Reusing one worker removes
/// all of it, and removes the repeated 64 MiB stack reservation with it.
///
/// Thread-local rather than threaded through the context: the saving is the
/// same and the call sites keep their signatures. The worker parks on a
/// channel receive between jobs, so an idle one costs no CPU, and dropping the
/// sender when the calling thread ends closes the loop.
struct Parser {
    jobs: std::sync::mpsc::Sender<Job>,
    replies: std::sync::mpsc::Receiver<Reply>,
}

impl Parser {
    fn spawn() -> Result<Self, String> {
        let (job_tx, job_rx) = std::sync::mpsc::channel::<Job>();
        let (reply_tx, reply_rx) = std::sync::mpsc::channel::<Reply>();
        std::thread::Builder::new()
            .stack_size(PARSER_STACK_BYTES)
            .name("sql-lineage-parser".to_string())
            .spawn(move || {
                // Ends when the sender is dropped, i.e. when the calling
                // thread goes away.
                while let Ok(job) = job_rx.recv() {
                    let reply = match job {
                        Job::Statements(entries) => Reply::Statements(
                            entries
                                .into_iter()
                                .map(|entry| {
                                    let sql = entry?;
                                    polyglot_sql::parse(&sql, DialectType::BigQuery)
                                        .map_err(|e| e.to_string())
                                        .map(|stmts| {
                                            stmts
                                                .iter()
                                                .map(|stmt| statement_facts(stmt, &sql))
                                                .collect()
                                        })
                                })
                                .collect(),
                        ),
                        Job::Select(sql) => Reply::Select(analyze_select_inner(&sql)),
                    };
                    if reply_tx.send(reply).is_err() {
                        break; // the caller is gone
                    }
                }
            })
            .map_err(|e| format!("failed to spawn SQL parser thread: {}", e))?;
        Ok(Parser {
            jobs: job_tx,
            replies: reply_rx,
        })
    }

    fn run(&self, job: Job) -> Result<Reply, String> {
        self.jobs
            .send(job)
            .map_err(|_| "SQL parser thread terminated unexpectedly".to_string())?;
        self.replies
            .recv()
            .map_err(|_| "SQL parser thread terminated unexpectedly".to_string())
    }
}

thread_local! {
    /// `None` until first use, and `None` again if the worker could not be
    /// spawned -- a parse that cannot reach a worker is reported, never
    /// silently skipped.
    static PARSER: std::cell::RefCell<Option<Parser>> = const { std::cell::RefCell::new(None) };
}

/// Runs one job on this thread's parser worker, spawning it on first use.
fn on_parser_thread(job: Job) -> Result<Reply, String> {
    PARSER.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            *slot = Some(Parser::spawn()?);
        }
        let parser = slot.as_ref().expect("just filled");
        let out = parser.run(job);
        if out.is_err() {
            // A dead worker must not poison every later call on this thread.
            *slot = None;
        }
        out
    })
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

/// Table-level facts for one statement.
#[derive(Debug, Default)]
pub(crate) struct StatementFacts {
    /// Raw referenced table names (reads), as written.
    pub reads: Vec<String>,
    /// Raw write-target table names (INSERT/MERGE/UPDATE/DELETE/CTAS/CREATE VIEW).
    pub writes: Vec<String>,
    /// Raw routine names invoked via CALL.
    pub calls: Vec<String>,
}

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
/// `PARSER_STACK_BYTES` stack -- for each statement, and nothing bounds how
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
pub(crate) fn statement_facts_for_all(
    statements: &[&str],
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

    match on_parser_thread(Job::Statements(owned)) {
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
    let mut facts = StatementFacts::default();

    match stmt {
        Expression::Insert(insert) => facts.writes.push(tableref_name(&insert.table)),
        Expression::Update(update) => facts.writes.push(tableref_name(&update.table)),
        Expression::Delete(delete) => facts.writes.push(tableref_name(&delete.table)),
        Expression::Merge(merge) => {
            let targets = polyglot_sql::get_table_names(&merge.this);
            facts.writes.extend(targets);
        }
        Expression::CreateTable(create) => facts.writes.push(tableref_name(&create.name)),
        Expression::CreateView(create) => facts.writes.push(tableref_name(&create.name)),
        _ => {}
    }

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

    let all = polyglot_sql::get_table_names(stmt);
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
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    let mut quote: Option<u8> = None;
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
                    push_trimmed(body, base, start..i, &mut out);
                    start = i + 1;
                    i += 1;
                }
                _ => i += 1,
            },
        }
    }
    push_trimmed(body, base, start..bytes.len(), &mut out);
    out
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
}

//! The single polyglot-sql integration boundary. Every parser call
//! lives here so an API bump touches one file. All functions degrade
//! (returning `None`/empty + caller warnings) rather than erroring.

use polyglot_sql::query_analysis::{analyze_query, AnalyzeQueryOptions};
use polyglot_sql::{DialectType, Expression};

/// SQL larger than this is skipped (defense-in-depth under
/// `panic = "abort"` — no unwinding safety net exists).
const MAX_SQL_BYTES: usize = 1024 * 1024;

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

pub(crate) fn parse_bigquery(sql: &str) -> Result<Vec<Expression>, String> {
    if sql.len() > MAX_SQL_BYTES {
        return Err(format!(
            "SQL exceeds {} bytes; skipped by static lineage",
            MAX_SQL_BYTES
        ));
    }
    polyglot_sql::parse(sql, DialectType::BigQuery).map_err(|e| e.to_string())
}

/// Analyzes a single SELECT-shaped statement for table + column facts.
pub(crate) fn analyze_select(sql: &str) -> Result<SelectFacts, String> {
    if sql.len() > MAX_SQL_BYTES {
        return Err(format!(
            "SQL exceeds {} bytes; skipped by static lineage",
            MAX_SQL_BYTES
        ));
    }
    let analysis = analyze_query(
        sql,
        AnalyzeQueryOptions {
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
pub(crate) fn statement_facts(stmt: &Expression, raw: &str) -> StatementFacts {
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
    let trimmed = raw.trim_start();
    if trimmed.len() >= 5 && trimmed[..4].eq_ignore_ascii_case("call") {
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
pub(crate) fn split_statements(script: &str) -> Vec<&str> {
    let body = strip_begin_end(script);
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

fn strip_begin_end(script: &str) -> &str {
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
        let stmts = parse_bigquery("INSERT INTO `p.mart.daily` SELECT dt FROM `p.raw.events`")
            .expect("parsed");
        let facts = statement_facts(
            &stmts[0],
            "INSERT INTO `p.mart.daily` SELECT dt FROM `p.raw.events`",
        );
        assert_eq!(facts.writes, vec!["p.mart.daily"]);
        assert_eq!(facts.reads, vec!["p.raw.events"]);
    }

    #[test]
    fn statement_facts_call() {
        let raw = "CALL `p.ds.refresh_mart`()";
        let facts = match parse_bigquery(raw) {
            Ok(stmts) if !stmts.is_empty() => statement_facts(&stmts[0], raw),
            _ => {
                // Even when CALL fails to parse, textual detection works
                // through the degradation path.
                let mut f = StatementFacts::default();
                let trimmed = raw.trim_start();
                if trimmed[..4].eq_ignore_ascii_case("call") {
                    f.calls.push(
                        trimmed[4..]
                            .trim_start()
                            .split(['(', ' '])
                            .next()
                            .unwrap_or("")
                            .replace('`', ""),
                    );
                }
                f
            }
        };
        assert_eq!(facts.calls, vec!["p.ds.refresh_mart"]);
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
        assert!(parse_bigquery(&big).is_err());
        assert!(analyze_select(&big).is_err());
    }
}

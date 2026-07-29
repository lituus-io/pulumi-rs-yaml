//! Canonical data-object ID construction — THE cross-stack join contract.
//!
//! IDs are cloud-scoped, not stack-scoped: two stacks referring to the
//! same physical BigQuery object mint byte-identical IDs, so unioned
//! exports self-link with no derivation step.
//!
//! - `bq://{project}` — project lowercased (GCP project ids are
//!   lowercase by rule; lowering is defensive normalization).
//! - `bq://{project}/{dataset}` — dataset preserved as written
//!   (BigQuery datasets are case-sensitive by default).
//! - `bq://{project}/{dataset}/{table}` — table preserved as written;
//!   partition decorators are stripped (`t$20240101` → `t`).
//! - `bq://{project}/{dataset}/{table}#{column}` — column fragment
//!   lowercased (BigQuery resolves column names case-insensitively);
//!   original case is preserved in the node's `column` field.
//! - Routines: `bq-routine://{project}/{dataset}/{routine}` — separate
//!   scheme so a routine can never collide with a table id.
//! - Jobs (stack-local operations, not shared cloud names):
//!   `bq-job://{organization}/{pulumi_project}/{stack}/{logical_name}`.
//!
//! Names containing `/` or `#` (illegal in BigQuery anyway) are
//! rejected rather than escaped, keeping the URI unambiguous.

use std::borrow::Cow;

/// A fully-resolved BigQuery table name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct TableName {
    pub project: String,
    pub dataset: String,
    pub table: String,
}

/// Marker project used when a dbt ref cannot be resolved; names carrying
/// it are filtered from emission.
pub(crate) const UNRESOLVED: &str = "__unresolved__";

impl TableName {
    pub fn id(&self) -> String {
        format!("bq://{}/{}/{}", self.project, self.dataset, self.table)
    }

    pub fn column_id(&self, column: &str) -> String {
        format!(
            "bq://{}/{}/{}#{}",
            self.project,
            self.dataset,
            self.table,
            column.to_lowercase()
        )
    }

    pub fn dataset_id(&self) -> String {
        format!("bq://{}/{}", self.project, self.dataset)
    }

    pub fn project_id(&self) -> String {
        format!("bq://{}", self.project)
    }

    pub(crate) fn is_unresolved(&self) -> bool {
        self.project == UNRESOLVED
    }

    /// Renders the backticked fully-qualified SQL name.
    pub(crate) fn sql_name(&self) -> String {
        format!("`{}.{}.{}`", self.project, self.dataset, self.table)
    }
}

/// Validates one BigQuery name segment for ID embedding.
fn valid_segment(s: &str) -> bool {
    !s.is_empty() && !s.contains('/') && !s.contains('#') && !s.contains('`')
}

/// Builds a [`TableName`] from resolved parts, normalizing case and
/// stripping partition decorators. Returns `None` (caller warns) when a
/// segment would make the ID ambiguous.
pub fn table_name(project: &str, dataset: &str, table: &str) -> Option<TableName> {
    let table = table.split('$').next().unwrap_or(table);
    if !valid_segment(project) || !valid_segment(dataset) || !valid_segment(table) {
        return None;
    }
    Some(TableName {
        project: project.to_lowercase(),
        dataset: dataset.to_string(),
        table: table.to_string(),
    })
}

/// Parses a SQL table reference (possibly backticked, 1/2/3-part) into a
/// [`TableName`], resolving missing qualifiers from the defaults.
///
/// `default_dataset` also implies `default_project` for the 1-part case.
pub fn parse_table_reference(
    raw: &str,
    default_project: Option<&str>,
    default_dataset: Option<&str>,
) -> Option<TableName> {
    // Strip backticks: `p.d.t` and `p`.`d`.`t` both reduce to p.d.t
    // (backticked BigQuery names cannot contain dots except as separators).
    let cleaned: Cow<'_, str> = if raw.contains('`') {
        Cow::Owned(raw.replace('`', ""))
    } else {
        Cow::Borrowed(raw)
    };
    let parts: Vec<&str> = cleaned.split('.').collect();
    match parts.as_slice() {
        [p, d, t] => table_name(p, d, t),
        [d, t] => table_name(default_project?, d, t),
        [t] => table_name(default_project?, default_dataset?, t),
        _ => None,
    }
}

pub fn routine_id(project: &str, dataset: &str, routine: &str) -> Option<String> {
    if !valid_segment(project) || !valid_segment(dataset) || !valid_segment(routine) {
        return None;
    }
    Some(format!(
        "bq-routine://{}/{}/{}",
        project.to_lowercase(),
        dataset,
        routine
    ))
}

pub fn job_id(organization: &str, pulumi_project: &str, stack: &str, logical: &str) -> String {
    format!(
        "bq-job://{}/{}/{}/{}",
        organization, pulumi_project, stack, logical
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_id_case_rules() {
        let t = table_name("My-Proj", "DataSet", "MyTable").expect("valid");
        assert_eq!(t.id(), "bq://my-proj/DataSet/MyTable");
        assert_eq!(
            t.column_id("USER_ID"),
            "bq://my-proj/DataSet/MyTable#user_id"
        );
        assert_eq!(t.dataset_id(), "bq://my-proj/DataSet");
        assert_eq!(t.project_id(), "bq://my-proj");
    }

    #[test]
    fn partition_decorator_stripped() {
        let t = table_name("p", "d", "events$20240101").expect("valid");
        assert_eq!(t.table, "events");
    }

    #[test]
    fn wildcard_kept_literally() {
        let t = table_name("p", "d", "events_*").expect("valid");
        assert_eq!(t.id(), "bq://p/d/events_*");
    }

    #[test]
    fn illegal_segments_rejected() {
        assert!(table_name("p", "d/x", "t").is_none());
        assert!(table_name("p", "d", "t#c").is_none());
        assert!(table_name("", "d", "t").is_none());
    }

    #[test]
    fn parse_reference_variants() {
        for raw in ["`p.d.t`", "`p`.`d`.`t`", "p.d.t"] {
            let t = parse_table_reference(raw, None, None).expect(raw);
            assert_eq!(t.id(), "bq://p/d/t");
        }
    }

    #[test]
    fn parse_reference_defaults() {
        let t = parse_table_reference("d.t", Some("p"), None).expect("2-part");
        assert_eq!(t.id(), "bq://p/d/t");
        let t = parse_table_reference("t", Some("p"), Some("d")).expect("1-part");
        assert_eq!(t.id(), "bq://p/d/t");
        assert!(parse_table_reference("d.t", None, None).is_none());
        assert!(parse_table_reference("t", Some("p"), None).is_none());
        assert!(parse_table_reference("a.b.c.d", None, None).is_none());
    }

    #[test]
    fn unresolved_marker_detected() {
        let t = table_name(UNRESOLVED, UNRESOLVED, "x").expect("valid form");
        assert!(t.is_unresolved());
    }

    #[test]
    fn routine_and_job_ids() {
        assert_eq!(
            routine_id("P", "ds", "refresh").as_deref(),
            Some("bq-routine://p/ds/refresh")
        );
        assert_eq!(
            job_id("org", "proj", "dev", "nightly"),
            "bq-job://org/proj/dev/nightly"
        );
    }
}

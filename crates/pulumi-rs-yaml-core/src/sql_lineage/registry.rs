// Author: spicyzhug@gmail.com
// Copyright: lituus-io, all rights reserved

//! Where SQL lives on resources: a const registry of
//! (type token, property dot-path, role). Enum-driven — no dynamic
//! dispatch. Callers extend via `SqlLineageOptions::extra_sql_sources`
//! (caller entries win on duplicate token+path).

use serde::Serialize;

/// Semantic role of a SQL-bearing property.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SqlRole {
    View,
    MaterializedView,
    RoutineBody,
    JobQuery,
    ScheduledQuery,
    SqlScript,
    DbtModel,
}

/// One SQL source location.
#[derive(Debug, Clone, Copy)]
pub struct SqlSourceSpec<'a> {
    /// Short type token form (e.g. `gcp:bigquery:Table`); matching also
    /// accepts the canonical expanded form (`gcp:bigquery/table:Table`).
    pub type_token: &'a str,
    /// Dot-path to the SQL property (same convention as
    /// `literal_properties` paths).
    pub sql_path: &'a str,
    pub role: SqlRole,
}

/// Built-in sources. The standard `gcp:` provider is the primary path;
/// gcpx entries are secondary coverage.
pub(crate) const BUILTIN_SOURCES: &[SqlSourceSpec<'static>] = &[
    SqlSourceSpec {
        type_token: "gcp:bigquery:Table",
        sql_path: "view.query",
        role: SqlRole::View,
    },
    SqlSourceSpec {
        type_token: "gcp:bigquery:Table",
        sql_path: "materializedView.query",
        role: SqlRole::MaterializedView,
    },
    SqlSourceSpec {
        type_token: "gcp:bigquery:Routine",
        sql_path: "definitionBody",
        role: SqlRole::RoutineBody,
    },
    SqlSourceSpec {
        type_token: "gcp:bigquery:Job",
        sql_path: "query.query",
        role: SqlRole::JobQuery,
    },
    SqlSourceSpec {
        type_token: "gcp:bigquery:DataTransferConfig",
        sql_path: "params.query",
        role: SqlRole::ScheduledQuery,
    },
    SqlSourceSpec {
        type_token: "gcpx:bigquery:Table",
        sql_path: "view.query",
        role: SqlRole::View,
    },
    SqlSourceSpec {
        type_token: "gcpx:bigquery:Table",
        sql_path: "materializedView.query",
        role: SqlRole::MaterializedView,
    },
    SqlSourceSpec {
        type_token: "gcpx:dbt:Model",
        sql_path: "sql",
        role: SqlRole::DbtModel,
    },
    SqlSourceSpec {
        type_token: "gcpx:scheduler:SqlJob",
        sql_path: "sql",
        role: SqlRole::SqlScript,
    },
];

/// Expands a short token to the canonical slashed form the evaluator
/// produces (`gcp:bigquery:Table` → `gcp:bigquery/table:Table`), so
/// registry entries match resources whichever form the template used.
pub(crate) fn matches_token(spec_token: &str, resource_token: &str) -> bool {
    if spec_token == resource_token {
        return true;
    }
    let canonical = crate::packages::canonicalize_type_token(spec_token);
    canonical == resource_token
}

/// The identity inputs a BigQuery resource's provider echoes unchanged as
/// outputs: name inputs fixed at creation (changing one replaces the
/// resource), so `${ds.datasetId}` is the `datasetId` the dataset was given.
/// Static resolution follows a reference to exactly these; every other
/// property, and every other type, stays dynamic.
pub(crate) fn echoed_identity_inputs(resource_token: &str) -> &'static [&'static str] {
    const DATASET: &[&str] = &["datasetId", "project"];
    const TABLE: &[&str] = &["datasetId", "tableId", "project"];
    const ROUTINE: &[&str] = &["datasetId", "routineId", "project"];
    const TYPES: &[(&str, &[&str])] = &[
        ("gcp:bigquery:Dataset", DATASET),
        ("gcpx:bigquery:Dataset", DATASET),
        ("gcp:bigquery:Table", TABLE),
        ("gcpx:bigquery:Table", TABLE),
        ("gcp:bigquery:Routine", ROUTINE),
    ];
    TYPES
        .iter()
        .find(|(token, _)| matches_token(token, resource_token))
        .map_or(&[], |(_, props)| props)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn echoed_identity_inputs_cover_only_bigquery_names() {
        assert_eq!(
            echoed_identity_inputs("gcp:bigquery:Dataset"),
            &["datasetId", "project"]
        );
        assert_eq!(
            echoed_identity_inputs("gcp:bigquery/table:Table"),
            &["datasetId", "tableId", "project"]
        );
        assert_eq!(
            echoed_identity_inputs("gcp:bigquery:Routine"),
            &["datasetId", "routineId", "project"]
        );
        assert!(echoed_identity_inputs("gcp:storage:Bucket").is_empty());
        assert!(echoed_identity_inputs("gcp:bigquery:Job").is_empty());
    }

    #[test]
    fn token_matching_accepts_both_forms() {
        assert!(matches_token("gcp:bigquery:Table", "gcp:bigquery:Table"));
        assert!(matches_token(
            "gcp:bigquery:Table",
            "gcp:bigquery/table:Table"
        ));
        assert!(!matches_token(
            "gcp:bigquery:Table",
            "gcp:storage/bucket:Bucket"
        ));
    }
}

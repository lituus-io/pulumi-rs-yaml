// Copyright (c) 2024-2026 Lituus-io. All rights reserved.

//! SQL lineage extraction — a data-object graph layered on the
//! infrastructure graph.
//!
//! [`export_sql_lineage`] statically extracts BigQuery
//! project/dataset/table/column entities and their lineage from a
//! template: entity identity from resource properties, columns and
//! descriptions from table schemas, table- and column-level derivation
//! from SQL carried by views, materialized views, routines, jobs,
//! scheduled queries, and dbt models (inline or `fn::readFile`), plus
//! caller-declared lineage via `lineage`-named outputs.
//!
//! # Cross-stack contract
//!
//! Node IDs are cloud-scoped (see [`ids`]): stack A creating
//! `bq://p/d/t` and stack B reading it mint the same ID, so unioned
//! exports self-link at project, table, and column level with no
//! derivation step. `defined_by` edges join data objects to the
//! infrastructure graph's URNs.
//!
//! # Degradation ladder (never errors)
//!
//! L0 parse+schema → column lineage · L1 parse → explicit projections ·
//! L2 parse fails → heuristic name scan (`resolution: heuristic`) ·
//! L3 SQL unresolvable → entity nodes only · L4 identity unresolvable →
//! warning only. DAG validation failure returns an empty graph with the
//! diagnostics, mirroring `resource_graph`.
//!
//! # Declared lineage
//!
//! An output named `lineage` (or `*Lineage`) whose value statically
//! resolves to a JSON object `{ "produces": [...], "consumes": [...],
//! "columnLineage": [{"output": "...", "from": [...]}] }` contributes
//! edges with `resolution: declared` — the strongest precedence. Table
//! references inside use `project.dataset.table` strings (2-part forms
//! resolve against the default project).

mod dbt;
pub mod ids;
pub mod registry;
mod resolve;
mod source_store;
mod sql;

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use serde::Serialize;

use crate::ast::expr::Expr;
use crate::ast::template::{OutputEntry, ResourceEntry, TemplateDecl, VariableEntry};
use crate::diag::Diagnostics;
use crate::eval::graph;
use crate::literal_resolve::{resolve_literal, ConfigLookup, HostConfig, Vars};
use crate::resource_graph::ResourceGraph;

pub use ids::TableName;
pub use registry::{SqlRole, SqlSourceSpec};
pub use resolve::SqlProvenance;

pub use source_store::{SqlSourceStore, MAX_SQL_FILE_BYTES};
pub use sql::StatementKind;
/// The statement splitter, exposed for the fuzz harness and for callers that
/// want to point at a statement inside a carrier without copying it. Not a
/// stable interface.
#[doc(hidden)]
pub use sql::{classify_statement_text, split_statement_spans, split_statements};

/// Lineage export schema version (independent of the infra graph's).
pub const SCHEMA_VERSION: u32 = 1;

const MAX_COMPONENT_DEPTH: usize = 32;
const ORG_PLACEHOLDER: &str = "organization";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DataNodeKind {
    Project,
    Dataset,
    Table,
    View,
    MaterializedView,
    Column,
    Routine,
    Job,
    /// A location outside BigQuery a statement reads or writes -- the `uri`
    /// of an `EXPORT DATA`, the `uris` of a `LOAD DATA`. Its id is the URI.
    ExternalLocation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DataEdgeKind {
    Contains,
    DerivesFrom,
    ColumnDerivesFrom,
    WritesTo,
    Calls,
    DefinedBy,
    RenamedFrom,
    /// Carrier (view, routine, job, dbt model) → one of its statements.
    HasStatement,
    /// Statement → a table or location it reads: its upstream.
    Reads,
    /// Statement → a table or location it creates or changes: its downstream.
    Writes,
}

/// How an edge was established; dedup keeps the strongest
/// (declared > parsed > structural > heuristic).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Resolution {
    Declared,
    Parsed,
    Structural,
    Heuristic,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DataNode<'src> {
    pub id: Cow<'src, str>,
    pub kind: DataNodeKind,
    /// Original-case leaf name (table, column, routine, …).
    pub name: Cow<'src, str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bq_project: Option<Cow<'src, str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dataset: Option<Cow<'src, str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub table: Option<Cow<'src, str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<Cow<'src, str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_type: Option<Cow<'src, str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<Cow<'src, str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<Cow<'src, str>>,
    /// Infrastructure URN of the declaring resource (also a
    /// `defined_by` edge).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub defined_by_urn: Option<Cow<'src, str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_file: Option<&'src str>,
    pub organization: Cow<'src, str>,
    pub project: Cow<'src, str>,
    pub stack: Cow<'src, str>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DataEdge<'src> {
    pub source_id: Cow<'src, str>,
    pub target_id: Cow<'src, str>,
    pub relationship: DataEdgeKind,
    pub resolution: Resolution,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sql_role: Option<SqlRole>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sql_provenance: Option<SqlProvenance>,
    /// Mediating routine/job id for table→table edges established
    /// through DML.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub via: Option<Cow<'src, str>>,
    pub organization: Cow<'src, str>,
    pub project: Cow<'src, str>,
    pub stack: Cow<'src, str>,
}

/// How a statement's text came to be: borrowed as written, or built.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TextOrigin {
    /// A string literal in the program, borrowed from the parsed template.
    Literal,
    /// The text of a `fn::readFile` file, borrowed from the source store.
    File,
    /// Assembled from a literal-only interpolation; necessarily owned.
    Interpolated,
    /// A dbt model after `ref`/`source`/`this`/`config` substitution; owned,
    /// and its offsets index the rendered text rather than the source.
    DbtRendered,
}

/// One distinct SQL statement, keyed by the hash of its exact text: the same
/// statement written in three places is one row with three occurrences.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SqlStatement<'src> {
    /// `sql:` followed by the lowercase hex SHA-256 of the statement's bytes.
    pub id: Cow<'src, str>,
    pub statement_kind: StatementKind,
    /// The statement as written. Omitted above the 1 MiB the parser accepts;
    /// the id is still the hash of the full text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<Cow<'src, str>>,
    pub byte_len: usize,
    pub organization: Cow<'src, str>,
    pub project: Cow<'src, str>,
    pub stack: Cow<'src, str>,
}

/// One place a statement is written.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SqlStatementOccurrence<'src> {
    pub statement_id: Cow<'src, str>,
    /// The view, materialized view, routine, job or dbt model whose SQL holds
    /// the statement.
    pub carrier_id: Cow<'src, str>,
    /// Position among the carrier's statements, from zero.
    pub statement_index: u32,
    /// Where the statement sits in the carrier's text, in bytes.
    pub byte_offset: usize,
    pub byte_len: usize,
    pub sql_role: SqlRole,
    pub sql_provenance: SqlProvenance,
    pub text_origin: TextOrigin,
    /// The `fn::readFile` path the carrier's text came from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub carrier_file: Option<Cow<'src, str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_file: Option<&'src str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub defined_by_urn: Option<Cow<'src, str>>,
    pub organization: Cow<'src, str>,
    pub project: Cow<'src, str>,
    pub stack: Cow<'src, str>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SqlLineageGraph<'src> {
    pub schema_version: u32,
    pub organization: Cow<'src, str>,
    pub project: Cow<'src, str>,
    pub stack: Cow<'src, str>,
    pub nodes: Vec<DataNode<'src>>,
    pub edges: Vec<DataEdge<'src>>,
    /// Present only with [`SqlLineageOptions::include_statements`].
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub statements: Vec<SqlStatement<'src>>,
    /// Present only with [`SqlLineageOptions::include_statements`].
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub statement_occurrences: Vec<SqlStatementOccurrence<'src>>,
}

const _: () = {
    fn _assert_send_sync<T: Send + Sync>() {}
    fn _check() {
        _assert_send_sync::<SqlLineageGraph<'static>>();
    }
};

impl SqlLineageGraph<'_> {
    /// Serializes the whole graph as pretty JSON with a trailing newline.
    pub fn to_json(&self) -> Result<String, String> {
        serde_json::to_string_pretty(self)
            .map(|mut s| {
                s.push('\n');
                s
            })
            .map_err(|e| format!("failed to serialize lineage graph: {}", e))
    }
}

#[derive(Default)]
pub struct SqlLineageOptions<'src> {
    pub organization: &'src str,
    pub project: &'src str,
    pub stack: &'src str,
    /// Project directory for contained `fn::readFile` resolution;
    /// `None` disables file reads (with a warning per occurrence).
    pub project_dir: Option<&'src Path>,
    /// Default BigQuery project for 2-part table names; falls back to
    /// the literal `gcp:project` config value.
    pub default_bq_project: Option<&'src str>,
    pub source_map: Option<&'src HashMap<String, String>>,
    /// Additional SQL sources; caller entries win on duplicate
    /// (token, path).
    pub extra_sql_sources: &'src [SqlSourceSpec<'src>],
    /// File texts read up front by [`SqlSourceStore::collect`]. SQL read
    /// from a file the store holds is BORROWED from it -- and a statement row
    /// can then point into it -- instead of being read and copied at each
    /// occurrence. `None` reads files as they are met, as before; the output
    /// is the same either way.
    pub sql_sources: Option<&'src SqlSourceStore>,
    /// Adds the statement layer: one [`SqlStatement`] per distinct statement
    /// text, one [`SqlStatementOccurrence`] per place it is written, and
    /// `has_statement` / `reads` / `writes` / `calls` edges from each
    /// statement. Off by default, so existing output does not move -- and
    /// because statement text can carry secrets a caller did not ask to
    /// export.
    pub include_statements: bool,
    /// Column lineage through scripts: `INSERT ... (cols) SELECT`, `CREATE
    /// TABLE ... AS SELECT` and `CREATE VIEW` in routine bodies, jobs,
    /// scheduled queries and SQL jobs gain `column_derives_from` edges from
    /// the written table's columns to the source columns, `via` the script.
    /// Off by default, so existing output does not move.
    pub script_column_lineage: bool,
    /// The stack's plaintext config (`Pulumi.<stack>.yaml` keys, `secure:`
    /// values already dropped by the host). Config-valued table identities
    /// and SQL then resolve the way the deploy resolves them. `None` is the
    /// output without it. See [`crate::literal_resolve`].
    pub config: Option<&'src HostConfig>,
}

/// Extracts the data-object lineage graph. Pure and deterministic;
/// `infra` supplies the URNs that `defined_by` edges join to.
pub fn export_sql_lineage<'src>(
    template: &'src TemplateDecl<'src>,
    infra: &'src ResourceGraph<'src>,
    opts: &SqlLineageOptions<'src>,
) -> (SqlLineageGraph<'src>, Diagnostics) {
    let mut diags = Diagnostics::new();
    let (_, sort_diags) = graph::topological_sort_with_sources(template, opts.source_map);
    let failed = sort_diags.has_errors();
    diags.extend(sort_diags);
    if failed {
        return (empty_graph(opts), diags);
    }

    let org = if opts.organization.is_empty() {
        ORG_PLACEHOLDER
    } else {
        opts.organization
    };

    // Host config, for the top-level scope only. The program's own name
    // namespaces its keys.
    let config = opts.config.map(|host| {
        ConfigLookup::new(
            template.name.as_deref().unwrap_or(opts.project),
            &template.config,
            host,
        )
    });

    // Default BigQuery project: explicit option, else the `gcp:project`
    // config value -- the host's when it gave config, else the literal the
    // program declares.
    let config_project = match &config {
        Some(lookup) => lookup.get("gcp:project"),
        None => template
            .config
            .iter()
            .find(|c| c.key == "gcp:project")
            .and_then(|c| c.param.value.as_ref().or(c.param.default.as_ref()))
            .and_then(|expr| {
                resolve_literal(
                    expr,
                    &Vars::empty(),
                    &mut HashMap::new(),
                    &mut HashSet::new(),
                )
            }),
    };
    let default_project: Option<String> = opts
        .default_bq_project
        .map(str::to_string)
        .or(config_project.map(Cow::into_owned));

    // Infra URN join map: logical_name → node id.
    let infra_urns: HashMap<&str, &str> = infra
        .nodes
        .iter()
        .map(|n| (n.logical_name.as_ref(), n.id.as_ref()))
        .collect();

    let mut ctx = Ctx {
        opts,
        org,
        default_project,
        infra_urns,
        registry: build_registry(opts.extra_sql_sources),
        nodes: BTreeMap::new(),
        edges: BTreeMap::new(),
        statements: BTreeMap::new(),
        occurrences: Vec::new(),
        diags: Diagnostics::new(),
    };

    let mut stack_guard: Vec<&str> = Vec::new();
    process_scope(
        &mut ctx,
        template,
        &template.resources,
        &template.variables,
        &template.outputs,
        None,
        config.as_ref(),
        &mut stack_guard,
    );

    diags.extend(std::mem::take(&mut ctx.diags));
    let graph = finalize(ctx, opts, org);
    (graph, diags)
}

// ---------- internal ----------

type EdgeKey = (String, String, DataEdgeKind);

struct EdgeVal<'src> {
    resolution: Resolution,
    sql_role: Option<SqlRole>,
    sql_provenance: Option<SqlProvenance>,
    via: Option<Cow<'src, str>>,
}

struct Ctx<'src, 'o> {
    opts: &'o SqlLineageOptions<'src>,
    org: &'src str,
    default_project: Option<String>,
    infra_urns: HashMap<&'src str, &'src str>,
    registry: Vec<SqlSourceSpec<'o>>,
    nodes: BTreeMap<String, DataNode<'src>>,
    edges: BTreeMap<EdgeKey, EdgeVal<'src>>,
    /// Statement rows by id; filled only with `include_statements`.
    statements: BTreeMap<String, SqlStatement<'src>>,
    occurrences: Vec<SqlStatementOccurrence<'src>>,
    diags: Diagnostics,
}

/// The most statements one carrier contributes rows for. 1 MiB of
/// `select 1 from t;` is ~65,000 statements; the lineage edges are still
/// derived for all of them, only the rows stop.
const MAX_STATEMENTS_PER_CARRIER: usize = 10_000;

/// A SQL-bearing resource, as the statement layer records it.
struct Carrier<'src, 'a> {
    /// The lineage id the statements belong to: a view's table id, a
    /// routine's, a job's, a dbt model's.
    id: &'a str,
    /// The carrier's whole SQL text; statement spans index it.
    text: &'a Cow<'src, str>,
    origin: TextOrigin,
    file: Option<&'a Cow<'src, str>>,
    role: SqlRole,
    provenance: SqlProvenance,
    /// The declaring resource's logical name, for its URN.
    logical: &'a str,
    source_file: Option<&'src str>,
}

impl<'src> Carrier<'src, '_> {
    /// The statement at `span`, borrowed when the carrier's text is.
    fn slice(&self, span: std::ops::Range<usize>) -> Cow<'src, str> {
        match self.text {
            Cow::Borrowed(text) => Cow::Borrowed(&text[span]),
            Cow::Owned(text) => Cow::Owned(text[span].to_owned()),
        }
    }
}

/// How a carrier's text came to be, from whether it is `borrowed` (as
/// written) and where it was read from.
fn text_origin(borrowed: bool, provenance: SqlProvenance) -> TextOrigin {
    match (borrowed, provenance) {
        (_, SqlProvenance::File) => TextOrigin::File,
        (true, SqlProvenance::Inline) => TextOrigin::Literal,
        (false, SqlProvenance::Inline) => TextOrigin::Interpolated,
    }
}

/// A column known only by name, for an edge endpoint that needs a node.
fn column_def(name: &str) -> resolve::ColumnDef {
    resolve::ColumnDef {
        name: name.to_owned(),
        data_type: None,
        mode: None,
        description: None,
        renamed_from: None,
    }
}

/// The whole of `text`, trimmed, as a span: a view is one statement.
fn trimmed_span(text: &str) -> std::ops::Range<usize> {
    let start = text.len() - text.trim_start().len();
    start..start + text.trim().len()
}

fn build_registry<'o>(extra: &'o [SqlSourceSpec<'o>]) -> Vec<SqlSourceSpec<'o>> {
    let mut out: Vec<SqlSourceSpec<'o>> = Vec::new();
    for spec in registry::BUILTIN_SOURCES {
        if !extra
            .iter()
            .any(|e| e.type_token == spec.type_token && e.sql_path == spec.sql_path)
        {
            out.push(*spec);
        }
    }
    out.extend_from_slice(extra);
    out
}

impl<'src> Ctx<'src, '_> {
    /// Records the statement at `span` of `carrier` -- a row for its text if
    /// this is the first time it is seen, an occurrence either way, and the
    /// carrier's `has_statement` edge -- and returns its id. `None` when the
    /// statement layer is off, the span is empty, or the carrier is past
    /// [`MAX_STATEMENTS_PER_CARRIER`].
    fn statement(
        &mut self,
        carrier: &Carrier<'src, '_>,
        index: usize,
        span: std::ops::Range<usize>,
        kind: StatementKind,
    ) -> Option<String> {
        if !self.opts.include_statements || span.is_empty() {
            return None;
        }
        if index >= MAX_STATEMENTS_PER_CARRIER {
            if index == MAX_STATEMENTS_PER_CARRIER {
                self.diags.warning(
                    None,
                    format!(
                        "{}: more than {} statements; rows recorded for the first {} only",
                        carrier.id, MAX_STATEMENTS_PER_CARRIER, MAX_STATEMENTS_PER_CARRIER
                    ),
                    "table-level lineage still covers every statement",
                );
            }
            return None;
        }
        let text = carrier.slice(span.clone());
        let mut id = String::with_capacity(4 + 64);
        id.push_str("sql:");
        id.push_str(&crate::sha256::hex(&crate::sha256::digest(text.as_bytes())));
        let byte_len = text.len();
        if !self.statements.contains_key(&id) {
            let row = SqlStatement {
                id: Cow::Owned(id.clone()),
                statement_kind: kind,
                text: (byte_len <= sql::MAX_SQL_BYTES).then_some(text),
                byte_len,
                organization: Cow::Borrowed(self.org),
                project: Cow::Borrowed(self.opts.project),
                stack: Cow::Borrowed(self.opts.stack),
            };
            self.statements.insert(id.clone(), row);
        }
        let urn = self.infra_urns.get(carrier.logical).copied();
        self.occurrences.push(SqlStatementOccurrence {
            statement_id: Cow::Owned(id.clone()),
            carrier_id: Cow::Owned(carrier.id.to_owned()),
            statement_index: u32::try_from(index).unwrap_or(u32::MAX),
            byte_offset: span.start,
            byte_len,
            sql_role: carrier.role,
            sql_provenance: carrier.provenance,
            text_origin: carrier.origin,
            carrier_file: carrier.file.cloned(),
            source_file: carrier.source_file,
            defined_by_urn: urn.map(Cow::Borrowed),
            organization: Cow::Borrowed(self.org),
            project: Cow::Borrowed(self.opts.project),
            stack: Cow::Borrowed(self.opts.stack),
        });
        let val = EdgeVal {
            resolution: Resolution::Structural,
            sql_role: Some(carrier.role),
            sql_provenance: Some(carrier.provenance),
            via: None,
        };
        self.edge(
            carrier.id.to_owned(),
            id.clone(),
            DataEdgeKind::HasStatement,
            val,
        );
        Some(id)
    }

    /// An external location (a `gs://` URI) as a node of its own.
    fn external_location(&mut self, uri: &str) {
        self.node(DataNode {
            id: Cow::Owned(uri.to_owned()),
            kind: DataNodeKind::ExternalLocation,
            name: Cow::Owned(uri.to_owned()),
            bq_project: None,
            dataset: None,
            table: None,
            column: None,
            data_type: None,
            mode: None,
            description: None,
            defined_by_urn: None,
            source_file: None,
            organization: Cow::Borrowed(self.org),
            project: Cow::Borrowed(self.opts.project),
            stack: Cow::Borrowed(self.opts.stack),
        });
    }

    fn node(&mut self, node: DataNode<'src>) {
        use std::collections::btree_map::Entry;
        match self.nodes.entry(node.id.to_string()) {
            Entry::Vacant(v) => {
                v.insert(node);
            }
            Entry::Occupied(mut o) => {
                // Merge: fill missing metadata; existing values win.
                let existing = o.get_mut();
                macro_rules! fill {
                    ($field:ident) => {
                        if existing.$field.is_none() {
                            existing.$field = node.$field;
                        }
                    };
                }
                fill!(bq_project);
                fill!(dataset);
                fill!(table);
                fill!(column);
                fill!(data_type);
                fill!(mode);
                fill!(description);
                fill!(defined_by_urn);
                fill!(source_file);
            }
        }
    }

    fn edge(
        &mut self,
        source: String,
        target: String,
        relationship: DataEdgeKind,
        val: EdgeVal<'src>,
    ) {
        use std::collections::btree_map::Entry;
        match self.edges.entry((source, target, relationship)) {
            Entry::Vacant(v) => {
                v.insert(val);
            }
            Entry::Occupied(mut o) => {
                if val.resolution < o.get().resolution {
                    o.insert(val);
                }
            }
        }
    }

    fn structural(&self) -> EdgeVal<'src> {
        EdgeVal {
            resolution: Resolution::Structural,
            sql_role: None,
            sql_provenance: None,
            via: None,
        }
    }

    /// Emits a table (or view) node plus its project/dataset containment
    /// chain and infra join.
    fn emit_table_entity(
        &mut self,
        fq: &TableName,
        kind: DataNodeKind,
        logical: Option<&str>,
        source_file: Option<&'src str>,
    ) {
        if fq.is_unresolved() {
            return;
        }
        let urn = logical.and_then(|l| self.infra_urns.get(l).copied());
        self.node(DataNode {
            id: Cow::Owned(fq.project_id()),
            kind: DataNodeKind::Project,
            name: Cow::Owned(fq.project.clone()),
            bq_project: Some(Cow::Owned(fq.project.clone())),
            dataset: None,
            table: None,
            column: None,
            data_type: None,
            mode: None,
            description: None,
            defined_by_urn: None,
            source_file: None,
            organization: Cow::Borrowed(self.org),
            project: Cow::Borrowed(self.opts.project),
            stack: Cow::Borrowed(self.opts.stack),
        });
        self.node(DataNode {
            id: Cow::Owned(fq.dataset_id()),
            kind: DataNodeKind::Dataset,
            name: Cow::Owned(fq.dataset.clone()),
            bq_project: Some(Cow::Owned(fq.project.clone())),
            dataset: Some(Cow::Owned(fq.dataset.clone())),
            table: None,
            column: None,
            data_type: None,
            mode: None,
            description: None,
            defined_by_urn: None,
            source_file: None,
            organization: Cow::Borrowed(self.org),
            project: Cow::Borrowed(self.opts.project),
            stack: Cow::Borrowed(self.opts.stack),
        });
        self.node(DataNode {
            id: Cow::Owned(fq.id()),
            kind,
            name: Cow::Owned(fq.table.clone()),
            bq_project: Some(Cow::Owned(fq.project.clone())),
            dataset: Some(Cow::Owned(fq.dataset.clone())),
            table: Some(Cow::Owned(fq.table.clone())),
            column: None,
            data_type: None,
            mode: None,
            description: None,
            defined_by_urn: urn.map(|u| Cow::Owned(u.to_string())),
            source_file,
            organization: Cow::Borrowed(self.org),
            project: Cow::Borrowed(self.opts.project),
            stack: Cow::Borrowed(self.opts.stack),
        });
        let structural = self.structural();
        self.edge(
            fq.project_id(),
            fq.dataset_id(),
            DataEdgeKind::Contains,
            structural,
        );
        let structural = self.structural();
        self.edge(fq.dataset_id(), fq.id(), DataEdgeKind::Contains, structural);
        if let Some(urn) = urn {
            let structural = self.structural();
            self.edge(
                fq.id(),
                urn.to_string(),
                DataEdgeKind::DefinedBy,
                structural,
            );
        }
    }

    fn emit_columns(&mut self, fq: &TableName, columns: &[resolve::ColumnDef]) {
        for col in columns {
            let id = fq.column_id(&col.name);
            self.node(DataNode {
                id: Cow::Owned(id.clone()),
                kind: DataNodeKind::Column,
                name: Cow::Owned(col.name.clone()),
                bq_project: Some(Cow::Owned(fq.project.clone())),
                dataset: Some(Cow::Owned(fq.dataset.clone())),
                table: Some(Cow::Owned(fq.table.clone())),
                column: Some(Cow::Owned(col.name.clone())),
                data_type: col.data_type.clone().map(Cow::Owned),
                mode: col.mode.clone().map(Cow::Owned),
                description: col.description.clone().map(Cow::Owned),
                defined_by_urn: None,
                source_file: None,
                organization: Cow::Borrowed(self.org),
                project: Cow::Borrowed(self.opts.project),
                stack: Cow::Borrowed(self.opts.stack),
            });
            let structural = self.structural();
            self.edge(fq.id(), id.clone(), DataEdgeKind::Contains, structural);
            if let Some(old) = &col.renamed_from {
                let structural = self.structural();
                self.edge(id, fq.column_id(old), DataEdgeKind::RenamedFrom, structural);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn process_scope<'src>(
    ctx: &mut Ctx<'src, '_>,
    template: &'src TemplateDecl<'src>,
    resources: &'src [ResourceEntry<'src>],
    variables: &'src [VariableEntry<'src>],
    outputs: &'src [OutputEntry<'src>],
    logical_prefix: Option<&str>,
    config: Option<&ConfigLookup<'src>>,
    stack_guard: &mut Vec<&'src str>,
) {
    let var_map = Vars::from_entries(variables).with_config(config);

    let dbt_env = dbt::DbtEnv::build_scope(resources, &var_map, &mut ctx.diags);

    for entry in resources {
        let logical_owned;
        let logical: &str = match logical_prefix {
            Some(prefix) => {
                logical_owned = format!("{}.{}", prefix, entry.logical_name);
                &logical_owned
            }
            None => entry.logical_name.as_ref(),
        };
        process_resource(ctx, entry, logical, &var_map, &dbt_env);
    }

    // dbt structural modelRefs edges + model entities.
    for (model_logical, model) in &dbt_env.models {
        let Some(fq) = &model.fq else { continue };
        let display = match logical_prefix {
            Some(prefix) => format!("{}.{}", prefix, model_logical),
            None => model_logical.clone(),
        };
        ctx.emit_table_entity(fq, DataNodeKind::Table, Some(&display), None);
        for target_logical in model.model_refs.values() {
            if let Some(target_fq) = dbt_env
                .models
                .get(target_logical)
                .and_then(|m| m.fq.as_ref())
            {
                let val = ctx.structural();
                ctx.edge(fq.id(), target_fq.id(), DataEdgeKind::DerivesFrom, val);
            }
        }
    }

    // Declared lineage via outputs.
    for output in outputs {
        let key = output.key.as_ref();
        if key == "lineage" || key.ends_with("Lineage") {
            process_declared_lineage(ctx, output, &var_map);
        }
    }

    // Component bodies.
    let pkg = template.name.as_deref().unwrap_or("yaml-components");
    for entry in resources {
        let raw = entry.resource.type_.as_ref();
        let comp = template.components.iter().find(|c| {
            let key = c.key.as_ref();
            raw == key
                || raw == format!("{}:{}", pkg, key)
                || raw == format!("{}:index:{}", pkg, key)
        });
        let Some(comp) = comp else { continue };
        let key = comp.key.as_ref();
        if stack_guard.contains(&key) || stack_guard.len() >= MAX_COMPONENT_DEPTH {
            ctx.diags.warning(
                None,
                format!("component '{}' expansion truncated in lineage", key),
                "recursive or too-deep component instantiation",
            );
            continue;
        }
        stack_guard.push(key);
        let instance_prefix = match logical_prefix {
            Some(prefix) => format!("{}.{}", prefix, entry.logical_name),
            None => entry.logical_name.to_string(),
        };
        process_scope(
            ctx,
            template,
            &comp.component.resources,
            &comp.component.variables,
            &comp.component.outputs,
            Some(&instance_prefix),
            // A component body's free names are its inputs, never config.
            None,
            stack_guard,
        );
        stack_guard.pop();
    }
}

fn process_resource<'src>(
    ctx: &mut Ctx<'src, '_>,
    entry: &'src ResourceEntry<'src>,
    logical: &str,
    var_map: &Vars<'src, '_>,
    dbt_env: &dbt::DbtEnv,
) {
    let token = entry.resource.type_.as_ref();
    let source_file = ctx
        .opts
        .source_map
        .and_then(|m| m.get(entry.logical_name.as_ref()))
        .map(String::as_str);

    let identity =
        |ctx: &Ctx<'src, '_>, keys: &[&str], default_ds: Option<&str>| -> Option<TableName> {
            let project = resolve::literal_prop(entry, var_map, &["project", "gcpProject"])
                .map(Cow::into_owned)
                .or_else(|| ctx.default_project.clone())?;
            let dataset = resolve::literal_prop(entry, var_map, &["datasetId", "dataset"])
                .map(Cow::into_owned)
                .or_else(|| default_ds.map(str::to_string))?;
            let table = resolve::literal_prop(entry, var_map, keys).map(Cow::into_owned)?;
            ids::table_name(&project, &dataset, &table)
        };

    // Datasets.
    if registry::matches_token("gcp:bigquery:Dataset", token)
        || registry::matches_token("gcpx:bigquery:Dataset", token)
    {
        let project = resolve::literal_prop(entry, var_map, &["project", "gcpProject"])
            .map(Cow::into_owned)
            .or_else(|| ctx.default_project.clone());
        let dataset = resolve::literal_prop(entry, var_map, &["datasetId", "dataset"]);
        if let (Some(p), Some(d)) = (project, dataset) {
            if let Some(fq) = ids::table_name(&p, d.as_ref(), "_") {
                let description = resolve::literal_prop(entry, var_map, &["description"]);
                let urn = ctx.infra_urns.get(logical).copied();
                ctx.node(DataNode {
                    id: Cow::Owned(fq.dataset_id()),
                    kind: DataNodeKind::Dataset,
                    name: Cow::Owned(fq.dataset.clone()),
                    bq_project: Some(Cow::Owned(fq.project.clone())),
                    dataset: Some(Cow::Owned(fq.dataset.clone())),
                    table: None,
                    column: None,
                    data_type: None,
                    mode: None,
                    description: description.map(|d| Cow::Owned(d.into_owned())),
                    defined_by_urn: urn.map(|u| Cow::Owned(u.to_string())),
                    source_file,
                    organization: Cow::Borrowed(ctx.org),
                    project: Cow::Borrowed(ctx.opts.project),
                    stack: Cow::Borrowed(ctx.opts.stack),
                });
                ctx.node(DataNode {
                    id: Cow::Owned(fq.project_id()),
                    kind: DataNodeKind::Project,
                    name: Cow::Owned(fq.project.clone()),
                    bq_project: Some(Cow::Owned(fq.project.clone())),
                    dataset: None,
                    table: None,
                    column: None,
                    data_type: None,
                    mode: None,
                    description: None,
                    defined_by_urn: None,
                    source_file: None,
                    organization: Cow::Borrowed(ctx.org),
                    project: Cow::Borrowed(ctx.opts.project),
                    stack: Cow::Borrowed(ctx.opts.stack),
                });
                let val = ctx.structural();
                ctx.edge(
                    fq.project_id(),
                    fq.dataset_id(),
                    DataEdgeKind::Contains,
                    val,
                );
                if let Some(urn) = urn {
                    let val = ctx.structural();
                    ctx.edge(
                        fq.dataset_id(),
                        urn.to_string(),
                        DataEdgeKind::DefinedBy,
                        val,
                    );
                }
            }
        } else {
            ctx.diags.warning(
                None,
                format!("dataset '{}' identity not statically resolvable", logical),
                "project or datasetId is dynamic; lineage node omitted",
            );
        }
        return;
    }

    // Tables (and their views / schemas).
    if registry::matches_token("gcp:bigquery:Table", token)
        || registry::matches_token("gcpx:bigquery:Table", token)
    {
        let Some(fq) = identity(ctx, &["tableId", "table"], None) else {
            ctx.diags.warning(
                None,
                format!("table '{}' identity not statically resolvable", logical),
                "project/dataset/table is dynamic; lineage limited",
            );
            return;
        };
        let has_view =
            resolve::get_property_by_path(&entry.resource.properties, "view.query").is_some();
        let has_mview =
            resolve::get_property_by_path(&entry.resource.properties, "materializedView.query")
                .is_some();
        let kind = if has_mview {
            DataNodeKind::MaterializedView
        } else if has_view {
            DataNodeKind::View
        } else {
            DataNodeKind::Table
        };
        ctx.emit_table_entity(&fq, kind, Some(logical), source_file);

        // Columns from the schema property (JSON string or readFile).
        if let Some(schema_expr) =
            resolve::get_property_by_path(&entry.resource.properties, "schema")
        {
            if let Some(resolve::SqlText { text, .. }) = resolve::resolve_sql_text(
                schema_expr,
                var_map,
                ctx.opts.project_dir,
                ctx.opts.sql_sources,
                &format!("{}.schema", logical),
                &mut ctx.diags,
            ) {
                match resolve::parse_schema_json(text.as_ref()) {
                    Some(cols) => ctx.emit_columns(&fq, &cols),
                    None => ctx.diags.warning(
                        None,
                        format!("table '{}' schema is not a JSON column array", logical),
                        "columns omitted from lineage",
                    ),
                }
            }
        }

        // SQL-bearing view/materialized view.
        for (path, role) in [
            ("view.query", SqlRole::View),
            ("materializedView.query", SqlRole::MaterializedView),
        ] {
            if let Some(expr) = resolve::get_property_by_path(&entry.resource.properties, path) {
                extract_select_lineage(
                    ctx,
                    entry,
                    expr,
                    var_map,
                    &fq,
                    role,
                    logical,
                    path,
                    source_file,
                );
            }
        }
        return;
    }

    // TableSchema (gcpx): structural columns incl. renames.
    if registry::matches_token("gcpx:bigquery:TableSchema", token) {
        if let Some(fq) = identity(ctx, &["tableId", "table"], None) {
            ctx.emit_table_entity(&fq, DataNodeKind::Table, Some(logical), source_file);
            for path in ["schema", "columns"] {
                if let Some(expr) = resolve::get_property_by_path(&entry.resource.properties, path)
                {
                    if let Some(cols) = resolve::parse_schema_yaml(expr, var_map) {
                        ctx.emit_columns(&fq, &cols);
                        break;
                    }
                }
            }
        }
        return;
    }

    // Routines.
    if registry::matches_token("gcp:bigquery:Routine", token) {
        process_routine(ctx, entry, logical, var_map, source_file);
        return;
    }

    // Jobs / scheduled queries / SQL scripts / dbt models via registry.
    let specs: Vec<SqlSourceSpec<'_>> = ctx
        .registry
        .iter()
        .filter(|s| registry::matches_token(s.type_token, token))
        .copied()
        .collect();
    for spec in specs {
        let Some(expr) = resolve::get_property_by_path(&entry.resource.properties, spec.sql_path)
        else {
            continue;
        };
        match spec.role {
            SqlRole::DbtModel => {
                let Some(model) = dbt_env.models.get(entry.logical_name.as_ref()) else {
                    continue;
                };
                let Some(fq) = model.fq.clone() else { continue };
                ctx.emit_table_entity(&fq, DataNodeKind::Table, Some(logical), source_file);
                let context = format!("{}.{}", logical, spec.sql_path);
                if let Some(sql) = resolve::resolve_sql_text(
                    expr,
                    var_map,
                    ctx.opts.project_dir,
                    ctx.opts.sql_sources,
                    &context,
                    &mut ctx.diags,
                ) {
                    let substituted: Cow<'src, str> = Cow::Owned(dbt::substitute(
                        dbt_env,
                        model,
                        sql.text.as_ref(),
                        &context,
                        &mut ctx.diags,
                    ));
                    let carrier_id = fq.id();
                    let carrier = Carrier {
                        id: &carrier_id,
                        text: &substituted,
                        origin: TextOrigin::DbtRendered,
                        file: sql.file.as_ref(),
                        role: SqlRole::DbtModel,
                        provenance: sql.provenance,
                        logical,
                        source_file,
                    };
                    select_lineage_from_text(ctx, &carrier, &fq, &context);
                }
            }
            SqlRole::JobQuery | SqlRole::ScheduledQuery | SqlRole::SqlScript => {
                process_job_like(ctx, entry, logical, var_map, expr, spec.role, source_file);
            }
            SqlRole::View | SqlRole::MaterializedView | SqlRole::RoutineBody => {
                // Handled by the dedicated passes above for built-in
                // types; extra_sql_sources with these roles attach to
                // the resource's own table identity when resolvable.
                if let Some(fq) = identity(ctx, &["tableId", "table", "name"], None) {
                    let kind = match spec.role {
                        SqlRole::MaterializedView => DataNodeKind::MaterializedView,
                        _ => DataNodeKind::View,
                    };
                    ctx.emit_table_entity(&fq, kind, Some(logical), source_file);
                    extract_select_lineage(
                        ctx,
                        entry,
                        expr,
                        var_map,
                        &fq,
                        spec.role,
                        logical,
                        spec.sql_path,
                        source_file,
                    );
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn extract_select_lineage<'src>(
    ctx: &mut Ctx<'src, '_>,
    _entry: &'src ResourceEntry<'src>,
    expr: &'src Expr<'src>,
    var_map: &Vars<'src, '_>,
    self_fq: &TableName,
    role: SqlRole,
    logical: &str,
    path: &str,
    source_file: Option<&'src str>,
) {
    let context = format!("{}.{}", logical, path);
    let Some(sql) = resolve::resolve_sql_text(
        expr,
        var_map,
        ctx.opts.project_dir,
        ctx.opts.sql_sources,
        &context,
        &mut ctx.diags,
    ) else {
        return;
    };
    let carrier_id = self_fq.id();
    let carrier = Carrier {
        id: &carrier_id,
        text: &sql.text,
        origin: text_origin(matches!(sql.text, Cow::Borrowed(_)), sql.provenance),
        file: sql.file.as_ref(),
        role,
        provenance: sql.provenance,
        logical,
        source_file,
    };
    select_lineage_from_text(ctx, &carrier, self_fq, &context);
}

/// L0–L2 ladder for a SELECT-shaped SQL text deriving `self_fq`.
fn select_lineage_from_text<'src>(
    ctx: &mut Ctx<'src, '_>,
    carrier: &Carrier<'src, '_>,
    self_fq: &TableName,
    context: &str,
) {
    let text: &str = carrier.text.as_ref();
    let (role, provenance) = (carrier.role, carrier.provenance);
    let default_project = self_fq.project.clone();
    let default_dataset = self_fq.dataset.clone();
    let analysis = sql::analyze_select(text);
    // A view's query is one statement, and the object it produces is the view.
    let kind = if analysis.is_ok() {
        sql::StatementKind::Select
    } else {
        sql::classify_statement_text(text)
    };
    let statement = ctx.statement(carrier, 0, trimmed_span(text), kind);
    if let Some(statement) = &statement {
        let val = EdgeVal {
            resolution: Resolution::Structural,
            sql_role: Some(role),
            sql_provenance: Some(provenance),
            via: None,
        };
        ctx.edge(statement.clone(), self_fq.id(), DataEdgeKind::Writes, val);
    }
    match analysis {
        Ok(facts) => {
            for raw in &facts.reads {
                let Some(src) =
                    ids::parse_table_reference(raw, Some(&default_project), Some(&default_dataset))
                else {
                    continue;
                };
                if src.is_unresolved() {
                    continue;
                }
                ctx.emit_table_entity(&src, DataNodeKind::Table, None, None);
                ctx.edge(
                    self_fq.id(),
                    src.id(),
                    DataEdgeKind::DerivesFrom,
                    EdgeVal {
                        resolution: Resolution::Parsed,
                        sql_role: Some(role),
                        sql_provenance: Some(provenance),
                        via: None,
                    },
                );
                if let Some(statement) = &statement {
                    let val = EdgeVal {
                        resolution: Resolution::Parsed,
                        sql_role: Some(role),
                        sql_provenance: Some(provenance),
                        via: None,
                    };
                    ctx.edge(statement.clone(), src.id(), DataEdgeKind::Reads, val);
                }
            }
            for (out_col, upstream) in &facts.columns {
                for (raw_table, src_col) in upstream {
                    let Some(src) = ids::parse_table_reference(
                        raw_table,
                        Some(&default_project),
                        Some(&default_dataset),
                    ) else {
                        continue;
                    };
                    if src.is_unresolved() {
                        continue;
                    }
                    ctx.edge(
                        self_fq.column_id(out_col),
                        src.column_id(src_col),
                        DataEdgeKind::ColumnDerivesFrom,
                        EdgeVal {
                            resolution: Resolution::Parsed,
                            sql_role: Some(role),
                            sql_provenance: Some(provenance),
                            via: None,
                        },
                    );
                    // Ensure endpoint column nodes exist.
                    let col_def = resolve::ColumnDef {
                        name: out_col.clone(),
                        data_type: None,
                        mode: None,
                        description: None,
                        renamed_from: None,
                    };
                    ctx.emit_columns(self_fq, std::slice::from_ref(&col_def));
                    let src_def = resolve::ColumnDef {
                        name: src_col.clone(),
                        data_type: None,
                        mode: None,
                        description: None,
                        renamed_from: None,
                    };
                    ctx.emit_columns(&src, std::slice::from_ref(&src_def));
                }
            }
            if facts.has_unexpanded_star {
                ctx.diags.warning(
                    None,
                    format!("{}: SELECT * limits column lineage", context),
                    "star projections without schema resolve at table level only",
                );
            }
        }
        Err(err) => {
            let refs = sql::heuristic_table_refs(text);
            if refs.is_empty() {
                ctx.diags.warning(
                    None,
                    format!("{}: SQL not parseable; no lineage extracted", context),
                    err,
                );
                return;
            }
            ctx.diags.warning(
                None,
                format!("{}: SQL not parseable; heuristic table refs used", context),
                err,
            );
            for raw in refs {
                let Some(src) = ids::parse_table_reference(
                    &raw,
                    Some(&default_project),
                    Some(&default_dataset),
                ) else {
                    continue;
                };
                if src.is_unresolved() || src == *self_fq {
                    continue;
                }
                ctx.emit_table_entity(&src, DataNodeKind::Table, None, None);
                ctx.edge(
                    self_fq.id(),
                    src.id(),
                    DataEdgeKind::DerivesFrom,
                    EdgeVal {
                        resolution: Resolution::Heuristic,
                        sql_role: Some(role),
                        sql_provenance: Some(provenance),
                        via: None,
                    },
                );
                if let Some(statement) = &statement {
                    let val = EdgeVal {
                        resolution: Resolution::Heuristic,
                        sql_role: Some(role),
                        sql_provenance: Some(provenance),
                        via: None,
                    };
                    ctx.edge(statement.clone(), src.id(), DataEdgeKind::Reads, val);
                }
            }
        }
    }
}

fn process_routine<'src>(
    ctx: &mut Ctx<'src, '_>,
    entry: &'src ResourceEntry<'src>,
    logical: &str,
    var_map: &Vars<'src, '_>,
    source_file: Option<&'src str>,
) {
    let project = resolve::literal_prop(entry, var_map, &["project", "gcpProject"])
        .map(Cow::into_owned)
        .or_else(|| ctx.default_project.clone());
    let dataset = resolve::literal_prop(entry, var_map, &["datasetId", "dataset"]);
    let routine = resolve::literal_prop(entry, var_map, &["routineId", "routine", "name"]);
    let (Some(project), Some(dataset), Some(routine)) = (project, dataset, routine) else {
        ctx.diags.warning(
            None,
            format!("routine '{}' identity not statically resolvable", logical),
            "project/dataset/routineId is dynamic; lineage node omitted",
        );
        return;
    };
    let Some(routine_id) = ids::routine_id(&project, dataset.as_ref(), routine.as_ref()) else {
        return;
    };
    let urn = ctx.infra_urns.get(logical).copied();
    ctx.node(DataNode {
        id: Cow::Owned(routine_id.clone()),
        kind: DataNodeKind::Routine,
        name: Cow::Owned(routine.clone().into_owned()),
        bq_project: Some(Cow::Owned(project.to_lowercase())),
        dataset: Some(Cow::Owned(dataset.clone().into_owned())),
        table: None,
        column: None,
        data_type: None,
        mode: None,
        description: resolve::literal_prop(entry, var_map, &["description"])
            .map(|d| Cow::Owned(d.into_owned())),
        defined_by_urn: urn.map(|u| Cow::Owned(u.to_string())),
        source_file,
        organization: Cow::Borrowed(ctx.org),
        project: Cow::Borrowed(ctx.opts.project),
        stack: Cow::Borrowed(ctx.opts.stack),
    });
    if let Some(urn) = urn {
        let val = ctx.structural();
        ctx.edge(
            routine_id.clone(),
            urn.to_string(),
            DataEdgeKind::DefinedBy,
            val,
        );
    }

    let Some(body_expr) =
        resolve::get_property_by_path(&entry.resource.properties, "definitionBody")
    else {
        return;
    };
    let context = format!("{}.definitionBody", logical);
    let Some(sql) = resolve::resolve_sql_text(
        body_expr,
        var_map,
        ctx.opts.project_dir,
        ctx.opts.sql_sources,
        &context,
        &mut ctx.diags,
    ) else {
        return;
    };

    let carrier = Carrier {
        id: &routine_id,
        text: &sql.text,
        origin: text_origin(matches!(sql.text, Cow::Borrowed(_)), sql.provenance),
        file: sql.file.as_ref(),
        role: SqlRole::RoutineBody,
        provenance: sql.provenance,
        logical,
        source_file,
    };
    script_lineage(ctx, &carrier, Some((&project, dataset.as_ref())), &context);
}

fn process_job_like<'src>(
    ctx: &mut Ctx<'src, '_>,
    entry: &'src ResourceEntry<'src>,
    logical: &str,
    var_map: &Vars<'src, '_>,
    sql_expr: &'src Expr<'src>,
    role: SqlRole,
    source_file: Option<&'src str>,
) {
    let job_id = ids::job_id(ctx.org, ctx.opts.project, ctx.opts.stack, logical);
    let urn = ctx.infra_urns.get(logical).copied();
    ctx.node(DataNode {
        id: Cow::Owned(job_id.clone()),
        kind: DataNodeKind::Job,
        name: Cow::Owned(logical.to_string()),
        bq_project: None,
        dataset: None,
        table: None,
        column: None,
        data_type: None,
        mode: None,
        description: None,
        defined_by_urn: urn.map(|u| Cow::Owned(u.to_string())),
        source_file,
        organization: Cow::Borrowed(ctx.org),
        project: Cow::Borrowed(ctx.opts.project),
        stack: Cow::Borrowed(ctx.opts.stack),
    });
    if let Some(urn) = urn {
        let val = ctx.structural();
        ctx.edge(
            job_id.clone(),
            urn.to_string(),
            DataEdgeKind::DefinedBy,
            val,
        );
    }

    // Explicit destinations.
    let mut destination: Option<TableName> = None;
    let mut default_dataset: Option<String> = None;
    let mut scheduled_project: Option<String> = None;
    if role == SqlRole::JobQuery {
        let dest_project =
            resolve::literal_prop(entry, var_map, &["query.destinationTable.projectId"])
                .map(Cow::into_owned)
                .or_else(|| ctx.default_project.clone());
        let dest_dataset =
            resolve::literal_prop(entry, var_map, &["query.destinationTable.datasetId"]);
        let dest_table = resolve::literal_prop(entry, var_map, &["query.destinationTable.tableId"]);
        default_dataset =
            resolve::literal_prop(entry, var_map, &["query.defaultDataset.datasetId"])
                .map(Cow::into_owned);
        if let (Some(p), Some(d), Some(t)) = (dest_project, dest_dataset, dest_table) {
            destination = ids::table_name(&p, d.as_ref(), t.as_ref());
        }
    } else if role == SqlRole::ScheduledQuery {
        default_dataset =
            resolve::literal_prop(entry, var_map, &["destinationDatasetId"]).map(Cow::into_owned);
        // A transfer config runs -- and writes -- in its own `project`. Only
        // the stack's default project was consulted, so a config declared for
        // another project put its destination, and every unqualified table
        // its query names, in the wrong one.
        scheduled_project = resolve::literal_prop(entry, var_map, &["project"])
            .map(Cow::into_owned)
            .or_else(|| ctx.default_project.clone());
        let template_name =
            resolve::literal_prop(entry, var_map, &["params.destination_table_name_template"]);
        if let (Some(p), Some(d)) = (scheduled_project.clone(), default_dataset.clone()) {
            match template_name {
                Some(t) if !t.contains('{') => {
                    destination = ids::table_name(&p, &d, t.as_ref());
                }
                Some(_) => {
                    if let Some(ds_fq) = ids::table_name(&p, &d, "_") {
                        ctx.diags.warning(
                            None,
                            format!("{}: templated destination table", logical),
                            "dataset-level writes_to edge emitted instead of table-level",
                        );
                        let val = EdgeVal {
                            resolution: Resolution::Structural,
                            sql_role: Some(role),
                            sql_provenance: None,
                            via: None,
                        };
                        ctx.edge(
                            job_id.clone(),
                            ds_fq.dataset_id(),
                            DataEdgeKind::WritesTo,
                            val,
                        );
                    }
                }
                None => {}
            }
        }
    }

    if let Some(dest) = &destination {
        ctx.emit_table_entity(dest, DataNodeKind::Table, None, None);
        let val = EdgeVal {
            resolution: Resolution::Structural,
            sql_role: Some(role),
            sql_provenance: None,
            via: None,
        };
        ctx.edge(job_id.clone(), dest.id(), DataEdgeKind::WritesTo, val);
    }

    let context = format!("{}.sql", logical);
    let Some(sql) = resolve::resolve_sql_text(
        sql_expr,
        var_map,
        ctx.opts.project_dir,
        ctx.opts.sql_sources,
        &context,
        &mut ctx.diags,
    ) else {
        return;
    };

    let project_for_refs = scheduled_project.or_else(|| ctx.default_project.clone());
    let provenance = sql.provenance;
    let carrier = Carrier {
        id: &job_id,
        text: &sql.text,
        origin: text_origin(matches!(sql.text, Cow::Borrowed(_)), sql.provenance),
        file: sql.file.as_ref(),
        role,
        provenance,
        logical,
        source_file,
    };
    script_lineage(
        ctx,
        &carrier,
        project_for_refs
            .as_deref()
            .map(|p| (p, default_dataset.as_deref().unwrap_or(""))),
        &context,
    );

    // Destination derives from every read, at the strength the read itself
    // was established. This was hard-coded `Parsed`, so a read the heuristic
    // scanner found -- from SQL the parser could not read -- reappeared on
    // the destination claiming it had been parsed.
    if let Some(dest) = destination {
        let reads: Vec<(String, Resolution)> = ctx
            .edges
            .iter()
            .filter(|((s, _, k), _)| s == &job_id && *k == DataEdgeKind::DerivesFrom)
            .map(|((_, t, _), v)| (t.clone(), v.resolution))
            .collect();
        for (read, resolution) in reads {
            let val = EdgeVal {
                resolution,
                sql_role: Some(role),
                sql_provenance: Some(provenance),
                via: Some(Cow::Owned(job_id.clone())),
            };
            ctx.edge(dest.id(), read, DataEdgeKind::DerivesFrom, val);
        }
    }
}

/// Statement-by-statement lineage for scripts/procedure bodies:
/// reads become `derives_from` (subject ← source), DML targets become
/// `writes_to`, `CALL` becomes `calls`.
fn script_lineage<'src>(
    ctx: &mut Ctx<'src, '_>,
    carrier: &Carrier<'src, '_>,
    defaults: Option<(&str, &str)>,
    context: &str,
) {
    let subject_id = carrier.id;
    let (role, provenance) = (carrier.role, carrier.provenance);
    let (default_project, default_dataset) = match defaults {
        Some((p, d)) => (
            Some(p.to_string()),
            if d.is_empty() {
                None
            } else {
                Some(d.to_string())
            },
        ),
        None => (None, None),
    };
    let text: &str = carrier.text.as_ref();
    let spans = sql::split_statement_spans(text);
    let statements: Vec<&str> = spans.iter().map(|r| &text[r.clone()]).collect();
    // One parser thread for the whole script, not one per statement: nothing
    // bounds how many statements a script has, and each used to cost a thread
    // with a 64 MiB stack.
    let parsed = sql::statement_facts_with_columns(&statements, ctx.opts.script_column_lineage);
    let mut any_parsed = false;
    for (index, ((span, stmt_text), result)) in
        spans.iter().zip(&statements).zip(parsed).enumerate()
    {
        let kind = match &result {
            Ok(all_facts) => all_facts
                .first()
                .map_or(sql::StatementKind::Other, |f| f.kind),
            Err(_) => sql::classify_statement_text(stmt_text),
        };
        let statement = ctx.statement(carrier, index, span.clone(), kind);
        match result {
            Ok(all_facts) => {
                any_parsed = true;
                for facts in &all_facts {
                    emit_statement_facts(
                        ctx,
                        facts,
                        subject_id,
                        default_project.as_deref(),
                        default_dataset.as_deref(),
                        role,
                        provenance,
                        Resolution::Parsed,
                        statement.as_deref(),
                    );
                }
            }
            Err(_) => {
                // BigQuery's `LOAD DATA ... FROM FILES` is not accepted by the
                // parser; its target is still plain to read, and it is a WRITE
                // -- left to the scan below it would be reported as a read.
                let load_target = (sql::classify_statement_text(stmt_text)
                    == sql::StatementKind::LoadData)
                    .then(|| sql::load_data_target(stmt_text))
                    .flatten();
                if let Some(raw) = &load_target {
                    if let Some(target) = ids::parse_table_reference(
                        raw,
                        default_project.as_deref(),
                        default_dataset.as_deref(),
                    )
                    .filter(|t| !t.is_unresolved())
                    {
                        ctx.emit_table_entity(&target, DataNodeKind::Table, None, None);
                        let val = EdgeVal {
                            resolution: Resolution::Heuristic,
                            sql_role: Some(role),
                            sql_provenance: Some(provenance),
                            via: None,
                        };
                        ctx.edge(
                            subject_id.to_string(),
                            target.id(),
                            DataEdgeKind::WritesTo,
                            val,
                        );
                        if let Some(statement) = &statement {
                            let val = EdgeVal {
                                resolution: Resolution::Heuristic,
                                sql_role: Some(role),
                                sql_provenance: Some(provenance),
                                via: None,
                            };
                            ctx.edge(statement.clone(), target.id(), DataEdgeKind::Writes, val);
                        }
                    }
                }
                let refs = sql::heuristic_table_refs(stmt_text);
                for raw in refs {
                    if load_target.as_deref() == Some(raw.as_str()) {
                        continue;
                    }
                    if let Some(src) = ids::parse_table_reference(
                        &raw,
                        default_project.as_deref(),
                        default_dataset.as_deref(),
                    ) {
                        if src.is_unresolved() {
                            continue;
                        }
                        ctx.emit_table_entity(&src, DataNodeKind::Table, None, None);
                        let val = EdgeVal {
                            resolution: Resolution::Heuristic,
                            sql_role: Some(role),
                            sql_provenance: Some(provenance),
                            via: None,
                        };
                        ctx.edge(
                            subject_id.to_string(),
                            src.id(),
                            DataEdgeKind::DerivesFrom,
                            val,
                        );
                        if let Some(statement) = &statement {
                            let val = EdgeVal {
                                resolution: Resolution::Heuristic,
                                sql_role: Some(role),
                                sql_provenance: Some(provenance),
                                via: None,
                            };
                            ctx.edge(statement.clone(), src.id(), DataEdgeKind::Reads, val);
                        }
                    }
                }
            }
        }
    }
    if !any_parsed && !statements.is_empty() {
        ctx.diags.warning(
            None,
            format!("{}: no statement parsed; heuristic lineage only", context),
            "table-level references were scanned textually",
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_statement_facts(
    ctx: &mut Ctx<'_, '_>,
    facts: &sql::StatementFacts,
    subject_id: &str,
    default_project: Option<&str>,
    default_dataset: Option<&str>,
    role: SqlRole,
    provenance: SqlProvenance,
    resolution: Resolution,
    statement: Option<&str>,
) {
    let resolve_name = |raw: &str| -> Option<TableName> {
        let name = ids::parse_table_reference(raw, default_project, default_dataset)?;
        if name.is_unresolved() {
            None
        } else {
            Some(name)
        }
    };
    for raw in &facts.writes {
        if let Some(target) = resolve_name(raw) {
            ctx.emit_table_entity(&target, DataNodeKind::Table, None, None);
            let val = EdgeVal {
                resolution,
                sql_role: Some(role),
                sql_provenance: Some(provenance),
                via: None,
            };
            ctx.edge(
                subject_id.to_string(),
                target.id(),
                DataEdgeKind::WritesTo,
                val,
            );
            if let Some(statement) = statement {
                let val = EdgeVal {
                    resolution,
                    sql_role: Some(role),
                    sql_provenance: Some(provenance),
                    via: None,
                };
                ctx.edge(statement.to_owned(), target.id(), DataEdgeKind::Writes, val);
            }
            // Target derives from each read via the subject.
            for read_raw in &facts.reads {
                if let Some(src) = resolve_name(read_raw) {
                    ctx.emit_table_entity(&src, DataNodeKind::Table, None, None);
                    let val = EdgeVal {
                        resolution,
                        sql_role: Some(role),
                        sql_provenance: Some(provenance),
                        via: Some(Cow::Owned(subject_id.to_string())),
                    };
                    ctx.edge(target.id(), src.id(), DataEdgeKind::DerivesFrom, val);
                }
            }
        }
    }
    for raw in &facts.reads {
        if let Some(src) = resolve_name(raw) {
            ctx.emit_table_entity(&src, DataNodeKind::Table, None, None);
            let val = EdgeVal {
                resolution,
                sql_role: Some(role),
                sql_provenance: Some(provenance),
                via: None,
            };
            ctx.edge(
                subject_id.to_string(),
                src.id(),
                DataEdgeKind::DerivesFrom,
                val,
            );
            if let Some(statement) = statement {
                let val = EdgeVal {
                    resolution,
                    sql_role: Some(role),
                    sql_provenance: Some(provenance),
                    via: None,
                };
                ctx.edge(statement.to_owned(), src.id(), DataEdgeKind::Reads, val);
            }
        }
    }
    // Column lineage onto the one table the statement writes. `column_facts`
    // only answers for statements with a single, known target.
    if let [write] = facts.writes.as_slice() {
        if let Some(target) = resolve_name(write).filter(|_| !facts.columns.is_empty()) {
            for (out_col, upstream) in &facts.columns {
                for (raw_table, src_col) in upstream {
                    let Some(src) = resolve_name(raw_table) else {
                        continue;
                    };
                    let val = EdgeVal {
                        resolution,
                        sql_role: Some(role),
                        sql_provenance: Some(provenance),
                        via: Some(Cow::Owned(subject_id.to_string())),
                    };
                    ctx.edge(
                        target.column_id(out_col),
                        src.column_id(src_col),
                        DataEdgeKind::ColumnDerivesFrom,
                        val,
                    );
                    ctx.emit_columns(&target, std::slice::from_ref(&column_def(out_col)));
                    ctx.emit_columns(&src, std::slice::from_ref(&column_def(src_col)));
                }
            }
        }
    }
    // Locations outside BigQuery exist only in the statement layer: without
    // it there is no node to hang them on, and the default output stays as
    // it was.
    if let Some(statement) = statement {
        for (uris, kind) in [
            (&facts.external_reads, DataEdgeKind::Reads),
            (&facts.external_writes, DataEdgeKind::Writes),
        ] {
            for uri in uris {
                ctx.external_location(uri);
                let val = EdgeVal {
                    resolution,
                    sql_role: Some(role),
                    sql_provenance: Some(provenance),
                    via: None,
                };
                ctx.edge(statement.to_owned(), uri.clone(), kind, val);
            }
        }
    }
    for raw in &facts.calls {
        let parts: Vec<&str> = raw.split('.').collect();
        if let [p, d, r] = parts.as_slice() {
            if let Some(routine_id) = ids::routine_id(p, d, r) {
                let val = EdgeVal {
                    resolution,
                    sql_role: Some(role),
                    sql_provenance: Some(provenance),
                    via: None,
                };
                if let Some(statement) = statement {
                    let val = EdgeVal {
                        resolution,
                        sql_role: Some(role),
                        sql_provenance: Some(provenance),
                        via: None,
                    };
                    ctx.edge(
                        statement.to_owned(),
                        routine_id.clone(),
                        DataEdgeKind::Calls,
                        val,
                    );
                }
                ctx.edge(subject_id.to_string(), routine_id, DataEdgeKind::Calls, val);
            }
        }
    }
}

/// Declared-lineage output JSON shape.
#[derive(serde::Deserialize)]
struct DeclaredLineage {
    #[serde(default)]
    produces: Vec<DeclaredEntity>,
    #[serde(default)]
    consumes: Vec<DeclaredEntity>,
    #[serde(default, rename = "columnLineage")]
    column_lineage: Vec<DeclaredColumnLineage>,
}

#[derive(serde::Deserialize)]
struct DeclaredEntity {
    project: Option<String>,
    dataset: String,
    table: String,
    #[serde(default)]
    columns: Vec<DeclaredColumn>,
}

#[derive(serde::Deserialize)]
struct DeclaredColumn {
    name: String,
    #[serde(rename = "type")]
    data_type: Option<String>,
    description: Option<String>,
}

#[derive(serde::Deserialize)]
struct DeclaredColumnLineage {
    output: String,
    #[serde(default)]
    from: Vec<String>,
}

fn process_declared_lineage<'src>(
    ctx: &mut Ctx<'src, '_>,
    output: &'src OutputEntry<'src>,
    var_map: &Vars<'src, '_>,
) {
    let mut memo = HashMap::new();
    let mut visiting = HashSet::new();
    let Some(text) = resolve_literal(&output.value, var_map, &mut memo, &mut visiting) else {
        ctx.diags.warning(
            None,
            format!(
                "output '{}' is not a literal; declared lineage skipped",
                output.key
            ),
            "runtime hooks can publish dynamic lineage post-deploy using the same ids",
        );
        return;
    };
    let declared: DeclaredLineage = match serde_json::from_str(text.as_ref()) {
        Ok(d) => d,
        Err(e) => {
            ctx.diags.warning(
                None,
                format!("output '{}' is not valid declared-lineage JSON", output.key),
                e.to_string(),
            );
            return;
        }
    };

    let resolve_entity = |ctx: &Ctx<'src, '_>, e: &DeclaredEntity| -> Option<TableName> {
        let project = e.project.clone().or_else(|| ctx.default_project.clone())?;
        ids::table_name(&project, &e.dataset, &e.table)
    };
    let mut produced: Vec<TableName> = Vec::new();
    for entity in &declared.produces {
        if let Some(fq) = resolve_entity(ctx, entity) {
            ctx.emit_table_entity(&fq, DataNodeKind::Table, None, None);
            let cols: Vec<resolve::ColumnDef> = entity
                .columns
                .iter()
                .map(|c| resolve::ColumnDef {
                    name: c.name.clone(),
                    data_type: c.data_type.clone(),
                    mode: None,
                    description: c.description.clone(),
                    renamed_from: None,
                })
                .collect();
            ctx.emit_columns(&fq, &cols);
            produced.push(fq);
        }
    }
    let mut consumed: Vec<TableName> = Vec::new();
    for entity in &declared.consumes {
        if let Some(fq) = resolve_entity(ctx, entity) {
            ctx.emit_table_entity(&fq, DataNodeKind::Table, None, None);
            consumed.push(fq);
        }
    }
    for prod in &produced {
        for cons in &consumed {
            let val = EdgeVal {
                resolution: Resolution::Declared,
                sql_role: None,
                sql_provenance: None,
                via: None,
            };
            ctx.edge(prod.id(), cons.id(), DataEdgeKind::DerivesFrom, val);
        }
    }
    let default_project = ctx.default_project.clone();
    let parse_col = |raw: &str| -> Option<(TableName, String)> {
        let (table_part, col) = raw.rsplit_once('.')?;
        let fq = ids::parse_table_reference(table_part, default_project.as_deref(), None)?;
        Some((fq, col.to_string()))
    };
    for cl in &declared.column_lineage {
        let Some((out_fq, out_col)) = parse_col(&cl.output) else {
            ctx.diags.warning(
                None,
                format!("declared columnLineage output '{}' unparseable", cl.output),
                "use project.dataset.table.column",
            );
            continue;
        };
        for from in &cl.from {
            if let Some((src_fq, src_col)) = parse_col(from) {
                let val = EdgeVal {
                    resolution: Resolution::Declared,
                    sql_role: None,
                    sql_provenance: None,
                    via: None,
                };
                ctx.edge(
                    out_fq.column_id(&out_col),
                    src_fq.column_id(&src_col),
                    DataEdgeKind::ColumnDerivesFrom,
                    val,
                );
            }
        }
    }
}

fn empty_graph<'src>(opts: &SqlLineageOptions<'src>) -> SqlLineageGraph<'src> {
    let org = if opts.organization.is_empty() {
        ORG_PLACEHOLDER
    } else {
        opts.organization
    };
    SqlLineageGraph {
        schema_version: SCHEMA_VERSION,
        organization: Cow::Borrowed(org),
        project: Cow::Borrowed(opts.project),
        stack: Cow::Borrowed(opts.stack),
        nodes: Vec::new(),
        edges: Vec::new(),
        statements: Vec::new(),
        statement_occurrences: Vec::new(),
    }
}

fn finalize<'src>(
    ctx: Ctx<'src, '_>,
    opts: &SqlLineageOptions<'src>,
    org: &'src str,
) -> SqlLineageGraph<'src> {
    let nodes: Vec<DataNode<'src>> = ctx.nodes.into_values().collect();
    let mut occurrences = ctx.occurrences;
    occurrences.sort_by(|a, b| {
        (a.carrier_id.as_ref(), a.statement_index, a.byte_offset).cmp(&(
            b.carrier_id.as_ref(),
            b.statement_index,
            b.byte_offset,
        ))
    });
    let edges: Vec<DataEdge<'src>> = ctx
        .edges
        .into_iter()
        .map(|((source_id, target_id, relationship), val)| DataEdge {
            source_id: Cow::Owned(source_id),
            target_id: Cow::Owned(target_id),
            relationship,
            resolution: val.resolution,
            sql_role: val.sql_role,
            sql_provenance: val.sql_provenance,
            via: val.via,
            organization: Cow::Borrowed(org),
            project: Cow::Borrowed(opts.project),
            stack: Cow::Borrowed(opts.stack),
        })
        .collect();
    SqlLineageGraph {
        schema_version: SCHEMA_VERSION,
        organization: Cow::Borrowed(org),
        project: Cow::Borrowed(opts.project),
        stack: Cow::Borrowed(opts.stack),
        nodes,
        edges,
        statements: ctx.statements.into_values().collect(),
        statement_occurrences: occurrences,
    }
}

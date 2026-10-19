# Graph and lineage export

Reference for the two graph layers `pulumi-rs-yaml` can export from a YAML
program — the infrastructure dependency graph and the BigQuery SQL lineage
layer built on top of it — including ID contracts, BigQuery schemas, loading
and refresh flow, property-graph definitions, and sample GQL queries.

Both exports are pure static analysis of the program: no state file, network,
or plugin access. See the `resource_graph` and `sql_lineage` module docs in
`pulumi-rs-yaml-core` for the authoritative specifications.

## Dependency graph export

Export a stack's static resource dependency graph — nodes and typed edges keyed
by engine-format URNs — for loading into a shared graph store such as
[BigQuery Graph](https://cloud.google.com/bigquery/docs/graph-intro). Each
stack exports independently; exports union into the same tables and join
cross-stack via the ID contract (see the `resource_graph` module docs in
`pulumi-rs-yaml-core` for the full specification).

```bash
# Single JSON document to stdout
pulumi-language-yaml graph --stack prod --organization my-org --dir ./my-project

# BigQuery-ingestable NDJSON (nodes.ndjson + edges.ndjson)
pulumi-language-yaml graph --stack prod --organization my-org \
  --format ndjson --out ./graph-export
```

```python
from pulumi_yaml_rs import export_dependency_graph
graph = export_dependency_graph("./my-project", "prod", "my-org")
```

### Loading into BigQuery

```sql
CREATE TABLE IF NOT EXISTS infra.nodes (
  id STRING NOT NULL,
  kind STRING, logical_name STRING, name STRING,
  type_token STRING, package STRING,
  parent_id STRING, component_id STRING, source_file STRING,
  literal_properties JSON,
  organization STRING, project STRING, stack STRING,
  stack_uri STRING, referenced_stack_uri STRING, resource_uri STRING,
  PRIMARY KEY (id) NOT ENFORCED
);

CREATE TABLE IF NOT EXISTS infra.edges (
  source_id STRING NOT NULL,
  target_id STRING NOT NULL,
  relationship STRING NOT NULL,
  property_paths ARRAY<STRING>,
  organization STRING, project STRING, stack STRING,
  PRIMARY KEY (source_id, target_id, relationship) NOT ENFORCED,
  FOREIGN KEY (source_id) REFERENCES infra.nodes (id) NOT ENFORCED
);
```

Refresh one stack's subgraph by deleting its rows (`WHERE stack = ... AND
project = ...`) then loading the new NDJSON:

```bash
bq load --source_format=NEWLINE_DELIMITED_JSON infra.nodes graph-export/nodes.ndjson
bq load --source_format=NEWLINE_DELIMITED_JSON infra.edges graph-export/edges.ndjson
```

`--compress gzip` writes `*.ndjson.gz` instead; `bq load` reads gzip NDJSON
directly (`bq load ... infra.nodes graph-export/nodes.ndjson.gz`).

### Where state lives: stack and resource URIs

Given the backend the stacks live in, the export says where each one's state
is. Pass it with `--backend-url` or `backend_url=` from Python. Without it,
both use one resolver, in Pulumi's own order: `PULUMI_BACKEND_URL`, then
`backend.url` in `Pulumi.yaml`, then the Pulumi workspace's current backend
(`current` in `$PULUMI_HOME/credentials.json`, default `~/.pulumi`; nothing
else in that file is read). The graph's `backend_source` says which applied --
`argument`, `env`, `project` or `workspace` -- and is absent when none did.
From Python, `backend_url=""` turns every fallback off:

```bash
pulumi-language-yaml graph --stack prod --organization my-org \
  --backend-url gs://my-state-bucket --format ndjson --out ./graph-export
```

| Field | On | Pulumi Cloud | Self-managed (`gs://`, `s3://`, `azblob://`, `file://`) |
|---|---|---|---|
| `stack_uri` | the stack node, and the graph | `https://app.pulumi.com/{org}/{project}/{stack}` | `{backend}/.pulumi/stacks/{project}/{stack}.json` |
| `referenced_stack_uri` | each StackReference with a literal name | the same, for the referenced stack | the same, for the referenced stack |
| `resource_uri` | every resource-like node | `stack_uri` `#` URN | `stack_uri` `#` URN |

- A StackReference resolves on the backend of the stack that declares it, so
  the referenced stack's location follows from the same URL.
- `resource_uri` is a locator -- the stack's state plus the resource's URN,
  percent-encoded, as the fragment -- not a console deep link; Pulumi
  documents no URL for a single resource.
- The self-managed path is the project-scoped layout of backends created by
  Pulumi 3.61 or later. A backend still on the older global layout
  (`.pulumi/stacks/{stack}.json`) needs `pulumi state upgrade` for the path to
  match.
- A query string or fragment on a self-managed URL (`?region=`, `?profile=`)
  is dropped; a URL with credentials (`user@`), whitespace or control
  characters is refused. A self-hosted Pulumi Cloud, an unknown scheme, or no
  backend at all adds no URI fields, and the output is what it always was.
- Pulumi Cloud URIs are omitted when the organization is not known, since they
  would name the wrong one.

#### Host identity, exact references, organization

- `project_override` / `stack_override` (Python) export the stack under the
  identity a host deploys it with -- a path-canonical project and stack, say.
  Every id and URI uses it; config keys stay namespaced by the program's own
  `name`.
- `reference_backends={"org/project/stack": "gs://bucket/prefix", ...}` gives
  each referenced stack's exact backend (keys normalized like the reference:
  `stack`, `project/stack` or `org/project/stack`). A StackReference found
  there carries `"resolved": "exact"`; one not found falls back to the
  declaring stack's own backend with `"resolved": "literal"`. A backend in
  the map that this exporter does not recognise yields no URI rather than a
  guess. Without the map there is no `resolved` key.
- On a self-managed backend there is no organization (Pulumi calls it
  `organization`), so every stack on one mints `stackoutput::organization/...`
  ids whatever organization a caller passes, and a three-part reference's
  organization is read as that too. Exporter and consumer ids line up by
  construction; no warning is printed. Pulumi Cloud keeps the real
  organization.

#### Cross-stack resolution

`resolve_cross_stack([graph, ...])` (Rust: `cross_stack::resolve_cross_stack`)
joins every `consumes_stack_output` edge to the resources the exporting
stack's `exports` edges name, in linear time:

```json
[{"consumer_edge": {"source_id": "urn:pulumi:...::Table::t",
                    "target_id": "stackoutput::organization/producer/prod::datasetId"},
  "exporter_resource_id": "urn:pulumi:...::Dataset::ds",
  "output_key": "datasetId"}]
```

Tables created before these columns existed take them with
`bq load --schema_update_option=ALLOW_FIELD_ADDITION`, or an `ALTER TABLE ...
ADD COLUMN`.

### Cross-stack identity linking

Explicit `pulumi:pulumi:StackReference` consumption is exported directly as
`consumes_stack_output` edges targeting the producer stack's output-node ids.
For resources linked only by cloud naming (e.g. a BigQuery dataset declared in
stack A and tables in stacks B/C that reference it by literal
`project` + `datasetId`), derive edges from `literal_properties` — the match
rules per resource family live in SQL, not in the exporter:

```sql
MERGE infra.edges e
USING (
  SELECT t.id AS source_id, d.id AS target_id,
         'references' AS relationship, ['derived:identity'] AS property_paths,
         t.organization, t.project, t.stack
  FROM infra.nodes t
  JOIN infra.nodes d
    ON d.type_token = 'gcp:bigquery/dataset:Dataset'
   AND t.type_token = 'gcp:bigquery/table:Table'
   AND JSON_VALUE(t.literal_properties, '$.project')  = JSON_VALUE(d.literal_properties, '$.project')
   AND JSON_VALUE(t.literal_properties, '$.datasetId') = JSON_VALUE(d.literal_properties, '$.datasetId')
) s
ON e.source_id = s.source_id AND e.target_id = s.target_id AND e.relationship = s.relationship
WHEN NOT MATCHED THEN
  INSERT (source_id, target_id, relationship, property_paths, organization, project, stack)
  VALUES (s.source_id, s.target_id, s.relationship, s.property_paths, s.organization, s.project, s.stack)
```

## SQL lineage layer

`--lineage` adds a second graph layer extracting BigQuery data objects and
their lineage from SQL carried by views, materialized views, routines, jobs,
scheduled queries, and dbt models — inline or via `fn::readFile`:

```bash
pulumi-language-yaml graph --stack prod --lineage \
  --format ndjson --out ./graph-export
# writes lineage_nodes.ndjson + lineage_edges.ndjson alongside nodes/edges
```

```python
from pulumi_yaml_rs import export_sql_lineage
lineage = export_sql_lineage("./my-project", "prod", "my-org")
```

Node ids are **cloud-scoped** — `bq://{project}/{dataset}/{table}` and
`bq://{project}/{dataset}/{table}#{column}` — so a table declared in stack A
and read by SQL in stacks B/C carries the same id from every side and the
union self-links at project, table, and column level with no derivation step.
`defined_by` edges join data objects to the infrastructure graph's URNs.
Edges carry `resolution` (`declared` > `parsed` > `structural` > `heuristic`)
and column nodes carry type/mode/description from table schemas.

Column lineage through scripts is opt-in: `--script-column-lineage` (Python:
`script_column_lineage=True`) traces `INSERT ... (cols) SELECT`, `CREATE TABLE
... AS SELECT` and `CREATE VIEW` statements in routines, jobs and scheduled
queries to their source columns, `via` the script. Only columns the SQL names
are recorded; an `INSERT` without a column list, a `*` projection, or a CTE
outside the query records none.

Components may declare lineage directly through an output named `lineage`
(or `*Lineage`) containing JSON
`{"produces": [...], "consumes": [...], "columnLineage": [...]}` — the same
contract runtime hooks can publish to post-deploy.

```sql
CREATE TABLE IF NOT EXISTS infra.lineage_nodes (
  id STRING NOT NULL,
  kind STRING, name STRING,
  bq_project STRING, dataset STRING, `table` STRING, `column` STRING,
  data_type STRING, mode STRING, description STRING,
  defined_by_urn STRING, source_file STRING,
  organization STRING, project STRING, stack STRING,
  PRIMARY KEY (id) NOT ENFORCED
);

CREATE TABLE IF NOT EXISTS infra.lineage_edges (
  source_id STRING NOT NULL,
  target_id STRING NOT NULL,
  relationship STRING NOT NULL,
  resolution STRING, sql_role STRING, sql_provenance STRING, via STRING,
  organization STRING, project STRING, stack STRING,
  PRIMARY KEY (source_id, target_id, relationship) NOT ENFORCED,
  FOREIGN KEY (source_id) REFERENCES infra.lineage_nodes (id) NOT ENFORCED
);
```

Refresh per stack with the same `DELETE WHERE stack/project` + `bq load` flow
as the infrastructure tables.

### Statement layer

`--sql-statements` (Python: `include_statements=True`) adds every SQL
statement the program carries -- its exact text, what it does, and where it is
written -- with statement-level edges to everything it reads and writes:

```bash
pulumi-language-yaml graph --stack prod --lineage --sql-statements \
  --format ndjson --out ./graph-export
# adds lineage_statements.ndjson + lineage_statement_occurrences.ndjson
```

- A **statement** is keyed by the content of its text,
  `sql:{sha256 hex of the exact text}`, so the same statement written in
  three places is one row. `statement_kind` is one of `select`, `insert`,
  `update`, `delete`, `merge`, `create_table`, `create_table_as_select`,
  `create_view`, `create_materialized_view`, `create_function`,
  `create_procedure`, `drop`, `truncate`, `alter`, `export_data`, `load_data`,
  `call`, `scripting`, `other`. `text` is omitted above 1 MiB; the id still
  hashes the full text.
- An **occurrence** is one place a statement is written: the carrier (a
  view's table id, a routine's, a job's, a dbt model's), the statement's
  index and byte range inside the carrier's SQL, its role and provenance, the
  `fn::readFile` path it came from, and the declaring resource's URN.
  `text_origin` says whether the text is as written (`literal`, `file`) or was
  built (`interpolated`, `dbt_rendered` -- a dbt model's offsets index the
  rendered SQL).
- **Edges**: `has_statement` from carrier to statement; `reads` from a
  statement to each table or external location it reads (its upstream);
  `writes` to each it creates or changes (its downstream -- a view's query
  writes the view; `DROP`, `TRUNCATE` and `ALTER` write their table);
  `calls` to each routine it calls. `EXPORT DATA` and `LOAD DATA` locations
  appear as `external_location` nodes whose id is the URI.

The layer is off by default -- statement text can carry secrets a caller did
not ask to export -- and with it off the output is byte-identical to a build
without it.

```sql
CREATE TABLE IF NOT EXISTS infra.lineage_statements (
  id STRING NOT NULL,
  statement_kind STRING, text STRING, byte_len INT64,
  organization STRING, project STRING, stack STRING,
  PRIMARY KEY (id) NOT ENFORCED
);

CREATE TABLE IF NOT EXISTS infra.lineage_statement_occurrences (
  statement_id STRING NOT NULL,
  carrier_id STRING NOT NULL,
  statement_index INT64, byte_offset INT64, byte_len INT64,
  sql_role STRING, sql_provenance STRING, text_origin STRING,
  carrier_file STRING, source_file STRING, defined_by_urn STRING,
  organization STRING, project STRING, stack STRING,
  FOREIGN KEY (statement_id) REFERENCES infra.lineage_statements (id) NOT ENFORCED
);
```

Statement rows are shared across stacks by construction -- the same text
hashes to the same id everywhere -- so refresh `lineage_statements` with a
`MERGE` on `id` rather than a per-stack delete, and per-stack delete only the
occurrences.

```sql
-- Every statement that writes a table, with its text and where it lives.
SELECT s.statement_kind, s.text, o.carrier_id, o.carrier_file, o.stack
FROM infra.lineage_edges e
JOIN infra.lineage_statements s ON s.id = e.source_id
JOIN infra.lineage_statement_occurrences o ON o.statement_id = s.id
WHERE e.relationship = 'writes' AND e.target_id = 'bq://data-proj/marts/daily';
```

## Host-supplied config

A config value is dynamic to the program alone, so by default it is never a
literal. A host that knows the stack's PLAINTEXT config passes it as
`config={"project:key": "value", "gcp:project": "...", ...}` (Python; Rust:
`GraphExportOptions::config` / `SqlLineageOptions::config`) -- the keys of
`Pulumi.<stack>.yaml`, with every `secure:` value dropped first. The Python
binding refuses a `{"secure": ...}` value outright, and a key the program
declares `secret: true` never resolves whatever is passed.

Resolution is the deploy's: the declared key by its own name or with the
project namespace stripped (`${k}`, `${proj:k}`, `${gcp:k}`); the value under
`{project}:{key}`, then `{key}`, then the declaration's `value:`, then its
`default:`; parsed by the declared type (`integer` `030` is `30`). Only
scalar types resolve, only at the top level (inside a component body a free
name is an input), and a value read from config is listed in the node's
`config_properties` (`"source": "config"`). Without `config` the output is
byte-identical.

`Pulumi.<stack>.yaml` itself is never read as a program file: a file in the
project directory with `config:` and nothing else is skipped whole, and one
with only an `encryptionsalt:` or an empty `config: {}` merges nothing.

## IAM graph

`export_iam_graph` (Python, same arguments as `export_dependency_graph`; Rust:
`iam_graph::export_iam_graph`) is a projection of the infrastructure walk, so
its resource ids are the infra graph's URNs.

| Row | Id | Notes |
|---|---|---|
| `principal` node | `principal::{member}` | `principal_type`: `user`, `group`, `serviceAccount`, `domain`, `principal`, `principalSet`, `special`, ... The email of a user, group, service account or domain is lowercased. |
| `role` node | `role::{role}` | |
| `has_role` edge | principal (or resource) -> resource URN | `role_id`, `binding_id` (the IAM resource's URN), `property_path`, `member_source`: `literal`, `config` or `resource` |

- A binding is any resource whose type name ends `IAMMember`/`IamMember`
  (`member`) or `IAMBinding`/`IamBinding` (`members`), plus
  `gcp:bigquery:DatasetAccess` (`userByEmail`, `groupByEmail`, `domain`,
  `iamMember`, `specialGroup`). `*IAMPolicy` is not read.
- The target is every in-program resource the binding's other properties
  reference; with none (a literal project or dataset) it is the binding
  itself, whose `literal_properties` name the target.
- A member that is a resource output (`serviceAccount:${sa.email}`,
  `${sa.member}`) is an edge FROM that resource.
- `summary` accounts for every member: `member_resolved` (literal or config,
  of which `member_from_config`), `member_from_resource`, and
  `member_unresolved` -- each unresolved one also a warning naming it.

## Resource keys

One join key between the static graph and everything Google records about a
deployed resource: its full resource name,
`//{service}.googleapis.com/{relative-name}`. `resource_key(type,
provider_id)` builds it from a Pulumi type and the provider id the deploy
recorded; `resource_key_from_properties(type, props)` from literal
properties when the id is a bare name; `resource_key_with_properties(type,
provider_id, props)` from both -- the provider id first, the properties
filling in what it lacks, then the properties alone; `bq_to_resource_key(id)` from a
lineage id (`bq://p/d/t`, `bq-routine://p/d/r`) or a SQL reference
(`p.d.t`); `bq_job_resource_key(p, location, job_id)` for a BigQuery job.

| Pulumi type | Provider id (= relative name) | Key |
|---|---|---|
| `gcp:bigquery:Dataset` | `projects/{p}/datasets/{d}` | `//bigquery.googleapis.com/projects/{p}/datasets/{d}` |
| `gcp:bigquery:Table` | `projects/{p}/datasets/{d}/tables/{t}` | `//bigquery.googleapis.com/projects/{p}/datasets/{d}/tables/{t}` |
| `gcp:bigquery:Routine` | `projects/{p}/datasets/{d}/routines/{r}` | `//bigquery.googleapis.com/projects/{p}/datasets/{d}/routines/{r}` |
| `gcp:bigquery:Job` | `projects/{p}/jobs/{j}` + `location` property | `//bigquery.googleapis.com/projects/{p}/locations/{l}/jobs/{j}` |
| `gcp:bigquery:DataTransferConfig` | `projects/{p}/locations/{l}/transferConfigs/{id}` | `//bigquerydatatransfer.googleapis.com/projects/{p}/locations/{l}/transferConfigs/{id}` |
| `gcp:workflows:Workflow` | `projects/{p}/locations/{l}/workflows/{w}` | `//workflows.googleapis.com/projects/{p}/locations/{l}/workflows/{w}` |
| `gcp:cloudscheduler:Job` | `projects/{p}/locations/{l}/jobs/{j}` | `//cloudscheduler.googleapis.com/projects/{p}/locations/{l}/jobs/{j}` |
| `gcp:cloudrun:Service` (v1) | `locations/{l}/namespaces/{p}/services/{s}` | `//run.googleapis.com/projects/{p}/locations/{l}/services/{s}` |
| `gcp:cloudrunv2:Service` | `projects/{p}/locations/{l}/services/{s}` | `//run.googleapis.com/projects/{p}/locations/{l}/services/{s}` |
| `gcp:cloudrunv2:Job` | `projects/{p}/locations/{l}/jobs/{j}` | `//run.googleapis.com/projects/{p}/locations/{l}/jobs/{j}` |
| `gcp:storage:Bucket` | `{name}` | `//storage.googleapis.com/projects/_/buckets/{name}` |
| `gcp:pubsub:Topic` | `projects/{p}/topics/{t}` | `//pubsub.googleapis.com/projects/{p}/topics/{t}` |
| `gcp:pubsub:Subscription` | `projects/{p}/subscriptions/{s}` | `//pubsub.googleapis.com/projects/{p}/subscriptions/{s}` |
| `gcp:dataproc:Batch` | `projects/{p}/locations/{l}/batches/{b}` | `//dataproc.googleapis.com/projects/{p}/locations/{l}/batches/{b}` |
| `gcp:batch:Job` | `projects/{p}/locations/{l}/jobs/{j}` | `//batch.googleapis.com/projects/{p}/locations/{l}/jobs/{j}` |
| `gcp:serviceaccount:Account` | `projects/{p}/serviceAccounts/{email}` | `//iam.googleapis.com/projects/{p}/serviceAccounts/{email}` |
| `gcp:serviceaccount:Account` (properties) | `accountId` + `project` | `//iam.googleapis.com/projects/{p}/serviceAccounts/{accountId}@{p}.iam.gserviceaccount.com` |
| (BigQuery job, runtime only) | `projects/{p}/jobs/{id}` + location | `//bigquery.googleapis.com/projects/{p}/locations/{l}/jobs/{id}` |

Normalization (the same as the lineage ids): the project is lowercased, a
domain-scoped project (`example.com:proj`) stays one segment, a table's
`$partition` decorator is stripped, every other segment keeps its case. A
segment that is empty or holds `/`, `#`, `?`, whitespace or a control
character gives no key. The project is the project ID, never the number.

Three types need more than the id:

- A Cloud Run v1 service is the same resource as a v2 one, so it has the v2
  key. Its namespace is the project; a namespace that is a project number
  gives no key.
- A BigQuery job's provider id holds no location, which the key needs. It
  comes from the job's `location` property (`resource_key_with_properties`);
  without one there is no key, never a guessed default. The key is the one
  `bq_job_resource_key` builds from an audit log.
- A service account's key built from properties uses `email` when present,
  otherwise `{accountId}@{p}.iam.gserviceaccount.com` -- the email form Cloud
  Asset Inventory accepts, and the same key as the provider-id path. A
  domain-scoped project `example.com:proj` gives
  `{accountId}@proj.example.com.iam.gserviceaccount.com`.

The same templates in BigQuery SQL, for a view or a Workflow that builds keys
from logs:

```sql
-- From a provider id that is already the relative name:
CONCAT('//workflows.googleapis.com/', REGEXP_REPLACE(id, r'^projects/([^/]+)',
       CONCAT('projects/', LOWER(REGEXP_EXTRACT(id, r'^projects/([^/]+)')))))
-- Cloud Scheduler, from the log's jsonPayload.jobName:
CONCAT('//cloudscheduler.googleapis.com/', JSON_VALUE(jsonPayload.jobName))
-- Dataproc batch, from protoPayload.resourceName:
CONCAT('//dataproc.googleapis.com/', protoPayload.resourceName)
-- Cloud Run job, from resource labels:
FORMAT('//run.googleapis.com/projects/%s/locations/%s/jobs/%s',
       LOWER(resource.labels.project_id), resource.labels.location, resource.labels.job_name)
-- BigQuery table, from a lineage id bq://p/d/t:
FORMAT('//bigquery.googleapis.com/projects/%s/datasets/%s/tables/%s',
       LOWER(SPLIT(SUBSTR(bq_id, 6), '/')[OFFSET(0)]),
       SPLIT(SUBSTR(bq_id, 6), '/')[OFFSET(1)],
       SPLIT(SPLIT(SUBSTR(bq_id, 6), '/')[OFFSET(2)], '#')[OFFSET(0)])
-- BigQuery job:
FORMAT('//bigquery.googleapis.com/projects/%s/locations/%s/jobs/%s',
       LOWER(project_id), location, job_id)
```

## Property graph and GQL

```sql
CREATE PROPERTY GRAPH infra.InfraGraph
  NODE TABLES (
    infra.nodes KEY (id)
      LABEL Entity PROPERTIES (id, kind, logical_name, type_token, project, stack)
  )
  EDGE TABLES (
    infra.edges KEY (source_id, target_id, relationship)
      SOURCE KEY (source_id) REFERENCES nodes (id)
      DESTINATION KEY (target_id) REFERENCES nodes (id)
      LABEL depends PROPERTIES (relationship, project, stack)
  );
```

With the lineage layer loaded, extend the graph to span both layers so one
GQL query traverses from a column through its table into the infrastructure
URN and onward:

```sql
CREATE PROPERTY GRAPH infra.FullGraph
  NODE TABLES (
    infra.nodes KEY (id)
      LABEL Entity PROPERTIES (id, kind, logical_name, type_token, project, stack),
    infra.lineage_nodes KEY (id)
      LABEL DataObject PROPERTIES (id, kind, name, bq_project, dataset, stack)
  )
  EDGE TABLES (
    infra.edges KEY (source_id, target_id, relationship)
      SOURCE KEY (source_id) REFERENCES nodes (id)
      DESTINATION KEY (target_id) REFERENCES nodes (id)
      LABEL depends PROPERTIES (relationship, project, stack),
    infra.lineage_edges KEY (source_id, target_id, relationship)
      SOURCE KEY (source_id) REFERENCES lineage_nodes (id)
      DESTINATION KEY (target_id) REFERENCES lineage_nodes (id)
      LABEL flows PROPERTIES (relationship, resolution, stack)
  );
```

With the statement layer loaded, statements become a third node table, so a
traversal can stop at the SQL that moves data between two tables:

```sql
CREATE PROPERTY GRAPH infra.StatementGraph
  NODE TABLES (
    infra.lineage_nodes KEY (id)
      LABEL DataObject PROPERTIES (id, kind, name, bq_project, dataset, stack),
    infra.lineage_statements KEY (id)
      LABEL Statement PROPERTIES (id, statement_kind, text)
  )
  EDGE TABLES (
    infra.lineage_edges AS reads KEY (source_id, target_id, relationship)
      SOURCE KEY (source_id) REFERENCES lineage_statements (id)
      DESTINATION KEY (target_id) REFERENCES lineage_nodes (id)
      LABEL reads PROPERTIES (relationship, resolution),
    infra.lineage_edges AS writes KEY (source_id, target_id, relationship)
      SOURCE KEY (source_id) REFERENCES lineage_statements (id)
      DESTINATION KEY (target_id) REFERENCES lineage_nodes (id)
      LABEL writes PROPERTIES (relationship, resolution)
  );
```

```sql
-- The statements that read raw.events and what each one writes.
GRAPH infra.StatementGraph
MATCH (src:DataObject)<-[r:reads WHERE r.relationship = 'reads']-(s:Statement)
      -[w:writes WHERE w.relationship = 'writes']->(dst:DataObject)
WHERE src.id = 'bq://data-proj/raw/events'
RETURN s.statement_kind, s.text, dst.id
```

```sql
-- Cross-stack column lineage: everything a column ultimately derives from.
GRAPH infra.FullGraph
MATCH (c:DataObject)-[f:flows WHERE f.relationship = 'column_derives_from']->{1,6}(src:DataObject)
WHERE c.id = 'bq://data-proj/marts/refined_revenue#boosted'
RETURN src.id, src.stack
```

```sql
-- Everything (in any stack) that reaches stack A's dataset:
GRAPH infra.InfraGraph
MATCH (consumer:Entity)-[d:depends]->{1,4}(ds:Entity)
WHERE ds.type_token = 'gcp:bigquery/dataset:Dataset' AND ds.stack = 'prod'
RETURN consumer.id, consumer.stack, ds.id
```

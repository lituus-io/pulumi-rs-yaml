# pulumi-rs-yaml

Rust implementation of the [Pulumi](https://www.pulumi.com/) YAML language runtime. Drop-in replacement for the Go-based `pulumi-yaml` with 1:1 compatibility.

## Architecture

5-crate workspace:

| Crate | Purpose |
|-------|---------|
| `pulumi-rs-yaml-proto` | Pre-generated protobuf/gRPC stubs |
| `pulumi-rs-yaml-core` | Parser, AST, evaluator, Jinja, type checker, PCL codegen |
| `pulumi-rs-yaml-language` | gRPC language host (`pulumi-language-yaml`) |
| `pulumi-rs-yaml-converter` | Converter plugin (`pulumi-converter-yaml`) |
| `pulumi-rs-yaml-python` | PyO3 bindings (`pulumi-rs-yaml` on PyPI) |

## Install

**Binary** (from [GitHub Releases](https://github.com/lituus-io/pulumi-rs-yaml/releases)):

```bash
# Replace with your platform: linux-amd64, linux-arm64, darwin-amd64, darwin-arm64, windows-amd64
curl -sSL https://github.com/lituus-io/pulumi-rs-yaml/releases/latest/download/pulumi-yaml-linux-amd64.tar.gz | tar xz
```

**Python**:

```bash
pip install pulumi-rs-yaml
```

This installs the PyO3 bindings and bundles `pulumi-language-yaml` and `pulumi-converter-yaml` as console scripts.

## Build from source

```bash
cargo build --release
```

Binaries are at `target/release/pulumi-language-yaml` and `target/release/pulumi-converter-yaml`.

## Test

```bash
cargo test --workspace
```

## Benchmark

```bash
cargo bench --workspace
```

## Fuzz

```bash
cd fuzz
cargo +nightly fuzz run fuzz_yaml_parser -- -max_total_time=60
```

Targets: `fuzz_yaml_parser`, `fuzz_interpolation`, `fuzz_jinja`, `fuzz_builtins`, `fuzz_converter`, `fuzz_yaml_bomb`, `fuzz_extra_context`, `fuzz_starlark`, `fuzz_parallel_eval`, `fuzz_resource_graph`, `fuzz_sql_lineage`.

## Dependency graph export (BigQuery Graph)

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

### SQL lineage layer

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

### Property graph and GQL

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

## Security

See [SECURITY.md](SECURITY.md) for vulnerability reporting and security details.

## License

Copyright (c) 2024-2026 Lituus-io. Dual-licensed under AGPL-3.0-or-later and a commercial license. See [LICENSE](LICENSE) for details.

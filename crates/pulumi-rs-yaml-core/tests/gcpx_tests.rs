//! End-to-end tests exercising gcpx provider resources through the Pulumi YAML evaluator.
//!
//! These tests define Pulumi YAML programs using all 6 gcpx resource types and run them
//! through the Rust YAML runtime with MockCallback to verify correct parsing, dependency
//! ordering, object passthrough, nested property access, and cross-resource output chaining.

use std::borrow::Cow;
use std::collections::HashMap;

use pulumi_rs_yaml_core::ast::parse::parse_template;
use pulumi_rs_yaml_core::eval::callback::RegisterResponse;
use pulumi_rs_yaml_core::eval::evaluator::Evaluator;
use pulumi_rs_yaml_core::eval::mock::MockCallback;
use pulumi_rs_yaml_core::eval::value::Value;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn eval_with_mock(source: &str, mock: MockCallback) -> (Evaluator<'static, MockCallback>, bool) {
    eval_with_mock_and_config(source, mock, HashMap::new(), &[])
}

fn eval_with_mock_and_config(
    source: &str,
    mock: MockCallback,
    raw_config: HashMap<String, String>,
    secret_keys: &[String],
) -> (Evaluator<'static, MockCallback>, bool) {
    let (template, parse_diags) = parse_template(source, None);
    if parse_diags.has_errors() {
        panic!("parse errors: {}", parse_diags);
    }
    let template: &'static _ = Box::leak(Box::new(template));
    let mut eval = Evaluator::with_callback(
        "test".to_string(),
        "dev".to_string(),
        "/tmp".to_string(),
        false,
        mock,
    );
    eval.evaluate_template(template, &raw_config, secret_keys);
    let has_errors = eval.diags.has_errors();
    (eval, has_errors)
}

fn eval_multifile(
    main_src: &str,
    extras: Vec<(&str, &str)>,
    mock: MockCallback,
) -> (Evaluator<'static, MockCallback>, bool) {
    use pulumi_rs_yaml_core::multi_file::merge_templates;

    let (main_template, main_diags) = parse_template(main_src, None);
    if main_diags.has_errors() {
        panic!("main parse errors: {}", main_diags);
    }

    let mut additional = Vec::new();
    for (name, src) in extras {
        let (tmpl, diags) = parse_template(src, None);
        if diags.has_errors() {
            panic!("parse errors in {}: {}", name, diags);
        }
        additional.push((name.to_string(), tmpl));
    }

    let (merged, merge_diags) = merge_templates(main_template, "Pulumi.yaml", additional);
    if merge_diags.has_errors() {
        panic!("merge errors: {}", merge_diags);
    }

    let template = merged.as_template_decl();
    let template: &'static _ = Box::leak(Box::new(template));

    let mut eval = Evaluator::with_callback(
        "test".to_string(),
        "dev".to_string(),
        "/tmp".to_string(),
        false,
        mock,
    );
    let raw_config = HashMap::new();
    eval.evaluate_template(template, &raw_config, &[]);
    let has_errors = eval.diags.has_errors();
    (eval, has_errors)
}

fn s(v: &str) -> Value<'static> {
    Value::String(Cow::Owned(v.to_string()))
}

fn obj(entries: Vec<(&str, Value<'static>)>) -> Value<'static> {
    Value::Object(
        entries
            .into_iter()
            .map(|(k, v)| (Cow::Owned(k.to_string()), v))
            .collect(),
    )
}

fn list(items: Vec<Value<'static>>) -> Value<'static> {
    Value::List(items)
}

fn resp(urn: &str, id: &str, outputs: HashMap<String, Value<'static>>) -> RegisterResponse {
    RegisterResponse {
        urn: urn.to_string(),
        id: id.to_string(),
        outputs,
        stables: Vec::new(),
    }
}

fn hmap(entries: Vec<(&str, Value<'static>)>) -> HashMap<String, Value<'static>> {
    entries
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
}

/// Extract a named field from a `Value::Object`, panicking on wrong type or missing key.
fn obj_field<'a>(val: &'a Value<'static>, key: &str) -> &'a Value<'static> {
    match val {
        Value::Object(fields) => {
            &fields
                .iter()
                .find(|(k, _)| k.as_ref() == key)
                .unwrap_or_else(|| panic!("field '{}' not found in object {:?}", key, val))
                .1
        }
        _ => panic!("expected Object for field '{}', got {:?}", key, val),
    }
}

// ---------------------------------------------------------------------------
// Tests — Original 13
// ---------------------------------------------------------------------------

#[test]
fn test_gcpx_bigquery_table_basic() {
    let source = r#"
name: test
runtime: yaml
resources:
  eventsTable:
    type: gcpx:bigquery:Table
    properties:
      project: my-gcp-project
      dataset: analytics
      table: raw_events
      description: Raw event data
      labels:
        env: production
        team: data
      partitioning:
        type: DAY
        field: event_date
outputs:
  tableType: ${eventsTable.tableType}
"#;

    let mock = MockCallback::with_register_responses(vec![resp(
        "urn:pulumi:test::test::gcpx:bigquery/table:Table::eventsTable",
        "projects/my-gcp-project/datasets/analytics/tables/raw_events",
        hmap(vec![
            ("project", s("my-gcp-project")),
            ("dataset", s("analytics")),
            ("table", s("raw_events")),
            ("tableType", s("TABLE")),
            (
                "labels",
                obj(vec![("env", s("production")), ("team", s("data"))]),
            ),
        ]),
    )]);

    let (eval, has_errors) = eval_with_mock(source, mock);
    assert!(!has_errors, "errors: {}", eval.diags);

    let regs = eval.callback().registrations();
    assert_eq!(regs.len(), 1);
    assert_eq!(regs[0].type_token, "gcpx:bigquery/table:Table");
    assert_eq!(regs[0].name, "eventsTable");
    assert!(regs[0].custom);
    assert_eq!(
        regs[0].inputs.get("project").and_then(|v| v.as_str()),
        Some("my-gcp-project")
    );
    assert_eq!(
        regs[0].inputs.get("dataset").and_then(|v| v.as_str()),
        Some("analytics")
    );
    assert_eq!(
        regs[0].inputs.get("table").and_then(|v| v.as_str()),
        Some("raw_events")
    );
    assert_eq!(
        eval.outputs.get("tableType").and_then(|v| v.as_str()),
        Some("TABLE")
    );
}

#[test]
fn test_gcpx_bigquery_view() {
    let source = r#"
name: test
runtime: yaml
resources:
  activeUsers:
    type: gcpx:bigquery:Table
    properties:
      project: my-gcp-project
      dataset: analytics
      table: active_users_view
      view:
        query: "SELECT user_id FROM `analytics.events` WHERE active = true"
outputs:
  viewType: ${activeUsers.tableType}
"#;

    let mock = MockCallback::with_register_responses(vec![resp(
        "urn:pulumi:test::test::gcpx:bigquery/table:Table::activeUsers",
        "projects/my-gcp-project/datasets/analytics/tables/active_users_view",
        hmap(vec![
            ("project", s("my-gcp-project")),
            ("dataset", s("analytics")),
            ("table", s("active_users_view")),
            ("tableType", s("VIEW")),
        ]),
    )]);

    let (eval, has_errors) = eval_with_mock(source, mock);
    assert!(!has_errors, "errors: {}", eval.diags);

    let regs = eval.callback().registrations();
    assert_eq!(regs.len(), 1);
    assert_eq!(regs[0].type_token, "gcpx:bigquery/table:Table");
    let view = regs[0].inputs.get("view").expect("view input missing");
    assert_eq!(
        obj_field(view, "query").as_str(),
        Some("SELECT user_id FROM `analytics.events` WHERE active = true")
    );
    assert_eq!(
        eval.outputs.get("viewType").and_then(|v| v.as_str()),
        Some("VIEW")
    );
}

#[test]
fn test_gcpx_bigquery_table_schema() {
    let source = r#"
name: test
runtime: yaml
resources:
  eventsSchema:
    type: gcpx:bigquery:TableSchema
    properties:
      project: my-gcp-project
      dataset: analytics
      table: raw_events
      columns:
        - name: user_id
          type: STRING
          mode: REQUIRED
          description: Unique user identifier
        - name: event_name
          type: STRING
          alter: rename
          alterFrom: event_type
        - name: event_date
          type: DATE
outputs:
  fingerprint: ${eventsSchema.schemaFingerprint}
"#;

    let mock = MockCallback::with_register_responses(vec![resp(
        "urn:pulumi:test::test::gcpx:bigquery/tableSchema:TableSchema::eventsSchema",
        "schema-001",
        hmap(vec![
            ("project", s("my-gcp-project")),
            ("dataset", s("analytics")),
            ("table", s("raw_events")),
            ("schemaFingerprint", s("abc123def456")),
        ]),
    )]);

    let (eval, has_errors) = eval_with_mock(source, mock);
    assert!(!has_errors, "errors: {}", eval.diags);

    let regs = eval.callback().registrations();
    assert_eq!(regs.len(), 1);
    assert_eq!(regs[0].type_token, "gcpx:bigquery/tableSchema:TableSchema");
    match regs[0].inputs.get("columns").expect("columns missing") {
        Value::List(items) => assert_eq!(items.len(), 3),
        other => panic!("columns should be a List, got {:?}", other),
    }
    assert_eq!(
        eval.outputs.get("fingerprint").and_then(|v| v.as_str()),
        Some("abc123def456")
    );
}

#[test]
fn test_gcpx_dbt_project() {
    let source = r#"
name: test
runtime: yaml
resources:
  myProject:
    type: gcpx:dbt:Project
    properties:
      project: my-gcp-project
      dataset: analytics
      sources:
        events_src:
          dataset: raw_data
          tables:
            - events
            - users
      declaredModels:
        - stg_events
        - mart_daily
      declaredMacros:
        - cents_to_dollars
outputs:
  gcpProject: ${myProject.context.gcpProject}
  dataset: ${myProject.context.dataset}
"#;

    let mock = MockCallback::with_register_responses(vec![resp(
        "urn:pulumi:test::test::gcpx:dbt/project:Project::myProject",
        "project-001",
        hmap(vec![
            ("project", s("my-gcp-project")),
            ("dataset", s("analytics")),
            (
                "context",
                obj(vec![
                    ("gcpProject", s("my-gcp-project")),
                    ("dataset", s("analytics")),
                    (
                        "sources",
                        obj(vec![(
                            "events_src",
                            obj(vec![
                                ("dataset", s("raw_data")),
                                ("tables", list(vec![s("events"), s("users")])),
                            ]),
                        )]),
                    ),
                ]),
            ),
        ]),
    )]);

    let (eval, has_errors) = eval_with_mock(source, mock);
    assert!(!has_errors, "errors: {}", eval.diags);

    let regs = eval.callback().registrations();
    assert_eq!(regs.len(), 1);
    assert_eq!(regs[0].type_token, "gcpx:dbt/project:Project");
    assert_eq!(regs[0].name, "myProject");
    assert_eq!(
        eval.outputs.get("gcpProject").and_then(|v| v.as_str()),
        Some("my-gcp-project")
    );
    assert_eq!(
        eval.outputs.get("dataset").and_then(|v| v.as_str()),
        Some("analytics")
    );
}

#[test]
fn test_gcpx_dbt_macro() {
    let source = r#"
name: test
runtime: yaml
resources:
  centsToDollars:
    type: gcpx:dbt:Macro
    properties:
      name: cents_to_dollars
      sql: "{{ amount_cents }} / 100.0"
      args:
        - amount_cents
outputs:
  macroName: ${centsToDollars.macroOutput.name}
  macroSql: ${centsToDollars.macroOutput.sql}
"#;

    let mock = MockCallback::with_register_responses(vec![resp(
        "urn:pulumi:test::test::gcpx:dbt/macro:Macro::centsToDollars",
        "macro-001",
        hmap(vec![
            ("name", s("cents_to_dollars")),
            (
                "macroOutput",
                obj(vec![
                    ("name", s("cents_to_dollars")),
                    ("sql", s("{{ amount_cents }} / 100.0")),
                ]),
            ),
        ]),
    )]);

    let (eval, has_errors) = eval_with_mock(source, mock);
    assert!(!has_errors, "errors: {}", eval.diags);

    let regs = eval.callback().registrations();
    assert_eq!(regs.len(), 1);
    assert_eq!(regs[0].type_token, "gcpx:dbt/macro:Macro");
    assert_eq!(
        eval.outputs.get("macroName").and_then(|v| v.as_str()),
        Some("cents_to_dollars")
    );
    assert_eq!(
        eval.outputs.get("macroSql").and_then(|v| v.as_str()),
        Some("{{ amount_cents }} / 100.0")
    );
}

#[test]
fn test_gcpx_dbt_model_standalone() {
    let source = r#"
name: test
runtime: yaml
resources:
  stgEvents:
    type: gcpx:dbt:Model
    properties:
      name: stg_events
      sql: "SELECT * FROM {{ source('events_src', 'events') }}"
      materialization: view
      context:
        gcpProject: my-gcp-project
        dataset: analytics
outputs:
  resolvedSql: ${stgEvents.modelOutput.resolvedSql}
  tableRef: ${stgEvents.modelOutput.tableRef}
  materialization: ${stgEvents.modelOutput.materialization}
"#;

    let mock = MockCallback::with_register_responses(vec![resp(
        "urn:pulumi:test::test::gcpx:dbt/model:Model::stgEvents",
        "model-001",
        hmap(vec![
            ("name", s("stg_events")),
            (
                "modelOutput",
                obj(vec![
                    ("resolvedSql", s("SELECT * FROM `my-gcp-project.raw_data.events`")),
                    ("tableRef", s("`my-gcp-project.analytics.stg_events`")),
                    ("materialization", s("view")),
                    ("resolvedDdl", s("CREATE VIEW `my-gcp-project.analytics.stg_events` AS SELECT * FROM `my-gcp-project.raw_data.events`")),
                ]),
            ),
        ]),
    )]);

    let (eval, has_errors) = eval_with_mock(source, mock);
    assert!(!has_errors, "errors: {}", eval.diags);

    let regs = eval.callback().registrations();
    assert_eq!(regs.len(), 1);
    assert_eq!(regs[0].type_token, "gcpx:dbt/model:Model");
    assert_eq!(
        eval.outputs.get("resolvedSql").and_then(|v| v.as_str()),
        Some("SELECT * FROM `my-gcp-project.raw_data.events`")
    );
    assert_eq!(
        eval.outputs.get("tableRef").and_then(|v| v.as_str()),
        Some("`my-gcp-project.analytics.stg_events`")
    );
    assert_eq!(
        eval.outputs.get("materialization").and_then(|v| v.as_str()),
        Some("view")
    );
}

/// Project -> Macro -> Model dependency chain with entire object passthrough.
/// Registration order: myMacro (no deps) -> myProject (no deps) -> myModel (depends on both).
#[test]
fn test_gcpx_dbt_pipeline() {
    let source = r#"
name: test
runtime: yaml
resources:
  myProject:
    type: gcpx:dbt:Project
    properties:
      project: my-gcp-project
      dataset: analytics
  myMacro:
    type: gcpx:dbt:Macro
    properties:
      name: cents_to_dollars
      sql: "{{ amount_cents }} / 100.0"
      args:
        - amount_cents
  myModel:
    type: gcpx:dbt:Model
    properties:
      name: stg_events
      sql: "SELECT * FROM events"
      context: ${myProject.context}
      macros:
        cents_to_dollars: ${myMacro.macroOutput}
outputs:
  tableRef: ${myModel.modelOutput.tableRef}
"#;

    let mock = MockCallback::with_register_responses(vec![
        resp(
            "urn:pulumi:test::test::gcpx:dbt/macro:Macro::myMacro",
            "macro-001",
            hmap(vec![
                ("name", s("cents_to_dollars")),
                (
                    "macroOutput",
                    obj(vec![
                        ("name", s("cents_to_dollars")),
                        ("sql", s("{{ amount_cents }} / 100.0")),
                    ]),
                ),
            ]),
        ),
        resp(
            "urn:pulumi:test::test::gcpx:dbt/project:Project::myProject",
            "project-001",
            hmap(vec![
                ("project", s("my-gcp-project")),
                ("dataset", s("analytics")),
                (
                    "context",
                    obj(vec![
                        ("gcpProject", s("my-gcp-project")),
                        ("dataset", s("analytics")),
                    ]),
                ),
            ]),
        ),
        resp(
            "urn:pulumi:test::test::gcpx:dbt/model:Model::myModel",
            "model-001",
            hmap(vec![
                ("name", s("stg_events")),
                (
                    "modelOutput",
                    obj(vec![
                        ("resolvedSql", s("SELECT * FROM events")),
                        ("tableRef", s("`my-gcp-project.analytics.stg_events`")),
                        ("materialization", s("view")),
                    ]),
                ),
            ]),
        ),
    ]);

    let (eval, has_errors) = eval_with_mock(source, mock);
    assert!(!has_errors, "errors: {}", eval.diags);

    let regs = eval.callback().registrations();
    assert_eq!(regs.len(), 3);
    assert_eq!(regs[0].name, "myMacro");
    assert_eq!(regs[0].type_token, "gcpx:dbt/macro:Macro");
    assert_eq!(regs[1].name, "myProject");
    assert_eq!(regs[1].type_token, "gcpx:dbt/project:Project");
    assert_eq!(regs[2].name, "myModel");
    assert_eq!(regs[2].type_token, "gcpx:dbt/model:Model");

    // myModel received entire context object from myProject
    assert_eq!(
        obj_field(
            regs[2].inputs.get("context").expect("context missing"),
            "gcpProject"
        )
        .as_str(),
        Some("my-gcp-project")
    );

    // myModel received macroOutput object from myMacro
    let macros_input = regs[2].inputs.get("macros").expect("macros missing");
    assert_eq!(
        obj_field(obj_field(macros_input, "cents_to_dollars"), "name").as_str(),
        Some("cents_to_dollars")
    );

    assert_eq!(
        eval.outputs.get("tableRef").and_then(|v| v.as_str()),
        Some("`my-gcp-project.analytics.stg_events`")
    );
}

/// Multi-model pipeline: Project -> stgModel -> martModel where mart references staging output.
#[test]
fn test_gcpx_dbt_multi_model_pipeline() {
    let source = r#"
name: test
runtime: yaml
resources:
  dbtProject:
    type: gcpx:dbt:Project
    properties:
      project: my-gcp-project
      dataset: analytics
  stgModel:
    type: gcpx:dbt:Model
    properties:
      name: stg_events
      sql: "SELECT * FROM events"
      context: ${dbtProject.context}
  martModel:
    type: gcpx:dbt:Model
    properties:
      name: mart_daily
      sql: "SELECT date, count(*) FROM {{ ref('stg_events') }}"
      context: ${dbtProject.context}
      modelRefs:
        stg_events: ${stgModel.modelOutput}
outputs:
  martTableRef: ${martModel.modelOutput.tableRef}
"#;

    let mock = MockCallback::with_register_responses(vec![
        resp(
            "urn:pulumi:test::test::gcpx:dbt/project:Project::dbtProject",
            "project-001",
            hmap(vec![
                ("project", s("my-gcp-project")),
                (
                    "context",
                    obj(vec![
                        ("gcpProject", s("my-gcp-project")),
                        ("dataset", s("analytics")),
                    ]),
                ),
            ]),
        ),
        resp(
            "urn:pulumi:test::test::gcpx:dbt/model:Model::stgModel",
            "model-stg-001",
            hmap(vec![
                ("name", s("stg_events")),
                (
                    "modelOutput",
                    obj(vec![
                        ("resolvedSql", s("SELECT * FROM events")),
                        ("tableRef", s("`my-gcp-project.analytics.stg_events`")),
                        ("materialization", s("view")),
                    ]),
                ),
            ]),
        ),
        resp(
            "urn:pulumi:test::test::gcpx:dbt/model:Model::martModel",
            "model-mart-001",
            hmap(vec![
                ("name", s("mart_daily")),
                (
                    "modelOutput",
                    obj(vec![
                        (
                            "resolvedSql",
                            s("SELECT date, count(*) FROM `my-gcp-project.analytics.stg_events`"),
                        ),
                        ("tableRef", s("`my-gcp-project.analytics.mart_daily`")),
                        ("materialization", s("table")),
                    ]),
                ),
            ]),
        ),
    ]);

    let (eval, has_errors) = eval_with_mock(source, mock);
    assert!(!has_errors, "errors: {}", eval.diags);

    let regs = eval.callback().registrations();
    assert_eq!(regs.len(), 3);
    assert_eq!(regs[0].name, "dbtProject");
    assert_eq!(regs[1].name, "stgModel");
    assert_eq!(regs[2].name, "martModel");

    // martModel received stgModel's modelOutput in its modelRefs
    let mart_refs = regs[2].inputs.get("modelRefs").expect("modelRefs missing");
    assert_eq!(
        obj_field(obj_field(mart_refs, "stg_events"), "tableRef").as_str(),
        Some("`my-gcp-project.analytics.stg_events`")
    );

    assert_eq!(
        eval.outputs.get("martTableRef").and_then(|v| v.as_str()),
        Some("`my-gcp-project.analytics.mart_daily`")
    );
}

#[test]
fn test_gcpx_scheduler_sql_job() {
    let source = r#"
name: test
runtime: yaml
resources:
  refreshJob:
    type: gcpx:scheduler:SqlJob
    properties:
      project: my-gcp-project
      name: daily_refresh
      schedule: "0 2 * * *"
      sql: "CALL `my-gcp-project.analytics.refresh_mart`()"
      retryCount: 3
      paused: false
outputs:
  workflowName: ${refreshJob.workflowName}
  state: ${refreshJob.state}
"#;

    let mock = MockCallback::with_register_responses(vec![resp(
        "urn:pulumi:test::test::gcpx:scheduler/sqlJob:SqlJob::refreshJob",
        "job-001",
        hmap(vec![
            ("project", s("my-gcp-project")),
            ("name", s("daily_refresh")),
            ("schedule", s("0 2 * * *")),
            (
                "workflowName",
                s("projects/my-gcp-project/locations/us/workflows/daily_refresh"),
            ),
            ("state", s("ENABLED")),
        ]),
    )]);

    let (eval, has_errors) = eval_with_mock(source, mock);
    assert!(!has_errors, "errors: {}", eval.diags);

    let regs = eval.callback().registrations();
    assert_eq!(regs.len(), 1);
    assert_eq!(regs[0].type_token, "gcpx:scheduler/sqlJob:SqlJob");
    assert_eq!(regs[0].name, "refreshJob");
    assert_eq!(
        regs[0].inputs.get("retryCount").and_then(|v| v.as_number()),
        Some(3.0)
    );
    assert_eq!(
        regs[0].inputs.get("paused").and_then(|v| v.as_bool()),
        Some(false)
    );
    assert_eq!(
        regs[0].inputs.get("schedule").and_then(|v| v.as_str()),
        Some("0 2 * * *")
    );
    assert_eq!(
        eval.outputs.get("workflowName").and_then(|v| v.as_str()),
        Some("projects/my-gcp-project/locations/us/workflows/daily_refresh")
    );
    assert_eq!(
        eval.outputs.get("state").and_then(|v| v.as_str()),
        Some("ENABLED")
    );
}

/// Grand integration: Project -> Macro -> Model -> SqlJob (4-resource chain).
/// SqlJob uses `sql: ${dbtModel.modelOutput.resolvedDdl}` proving the full chain.
#[test]
fn test_gcpx_full_end_to_end() {
    let ddl = "CREATE TABLE `my-gcp-project.analytics.revenue_mart` AS SELECT user_id, amount / 100.0 as revenue FROM orders";
    let source = r#"
name: test
runtime: yaml
resources:
  dbtCtx:
    type: gcpx:dbt:Project
    properties:
      project: my-gcp-project
      dataset: analytics
  dbtMacro:
    type: gcpx:dbt:Macro
    properties:
      name: cents_to_dollars
      sql: "{{ amount_cents }} / 100.0"
      args:
        - amount_cents
  dbtModel:
    type: gcpx:dbt:Model
    properties:
      name: revenue_mart
      sql: "SELECT user_id, {{ cents_to_dollars('amount') }} as revenue FROM orders"
      context: ${dbtCtx.context}
      macros:
        cents_to_dollars: ${dbtMacro.macroOutput}
  refreshJob:
    type: gcpx:scheduler:SqlJob
    properties:
      project: my-gcp-project
      name: refresh_revenue
      schedule: "0 3 * * *"
      sql: ${dbtModel.modelOutput.resolvedDdl}
outputs:
  jobSql: ${refreshJob.sql}
"#;

    let mock = MockCallback::with_register_responses(vec![
        resp(
            "urn:pulumi:test::test::gcpx:dbt/project:Project::dbtCtx",
            "project-001",
            hmap(vec![
                ("project", s("my-gcp-project")),
                (
                    "context",
                    obj(vec![
                        ("gcpProject", s("my-gcp-project")),
                        ("dataset", s("analytics")),
                    ]),
                ),
            ]),
        ),
        resp(
            "urn:pulumi:test::test::gcpx:dbt/macro:Macro::dbtMacro",
            "macro-001",
            hmap(vec![
                ("name", s("cents_to_dollars")),
                (
                    "macroOutput",
                    obj(vec![
                        ("name", s("cents_to_dollars")),
                        ("sql", s("{{ amount_cents }} / 100.0")),
                    ]),
                ),
            ]),
        ),
        resp(
            "urn:pulumi:test::test::gcpx:dbt/model:Model::dbtModel",
            "model-001",
            hmap(vec![
                ("name", s("revenue_mart")),
                (
                    "modelOutput",
                    obj(vec![
                        (
                            "resolvedSql",
                            s("SELECT user_id, amount / 100.0 as revenue FROM orders"),
                        ),
                        ("tableRef", s("`my-gcp-project.analytics.revenue_mart`")),
                        ("resolvedDdl", s(ddl)),
                        ("materialization", s("table")),
                    ]),
                ),
            ]),
        ),
        resp(
            "urn:pulumi:test::test::gcpx:scheduler/sqlJob:SqlJob::refreshJob",
            "job-001",
            hmap(vec![
                ("project", s("my-gcp-project")),
                ("name", s("refresh_revenue")),
                ("sql", s(ddl)),
                (
                    "workflowName",
                    s("projects/my-gcp-project/locations/us/workflows/refresh_revenue"),
                ),
            ]),
        ),
    ]);

    let (eval, has_errors) = eval_with_mock(source, mock);
    assert!(!has_errors, "errors: {}", eval.diags);

    let regs = eval.callback().registrations();
    assert_eq!(regs.len(), 4);
    assert_eq!(regs[0].name, "dbtCtx");
    assert_eq!(regs[1].name, "dbtMacro");
    assert_eq!(regs[2].name, "dbtModel");
    assert_eq!(regs[3].name, "refreshJob");
    assert_eq!(
        regs[3].inputs.get("sql").and_then(|v| v.as_str()),
        Some(ddl)
    );
    assert_eq!(
        eval.outputs.get("jobSql").and_then(|v| v.as_str()),
        Some(ddl)
    );
}

#[test]
fn test_gcpx_mixed_table_and_dbt() {
    let source = r#"
name: test
runtime: yaml
resources:
  eventsTable:
    type: gcpx:bigquery:Table
    properties:
      project: my-gcp-project
      dataset: analytics
      table: raw_events
  dbtProject:
    type: gcpx:dbt:Project
    properties:
      project: my-gcp-project
      dataset: analytics
"#;

    let mock = MockCallback::new();
    let (eval, has_errors) = eval_with_mock(source, mock);
    assert!(!has_errors, "errors: {}", eval.diags);

    let regs = eval.callback().registrations();
    assert_eq!(regs.len(), 2);
    let types: Vec<&str> = regs.iter().map(|r| r.type_token.as_str()).collect();
    assert!(types.contains(&"gcpx:dbt/project:Project"));
    assert!(types.contains(&"gcpx:bigquery/table:Table"));
}

#[test]
fn test_gcpx_config_driven() {
    let source = r#"
name: test
runtime: yaml
config:
  gcpProject:
    type: string
  schedule:
    type: string
    default: "0 2 * * *"
resources:
  refreshJob:
    type: gcpx:scheduler:SqlJob
    properties:
      project: ${gcpProject}
      name: daily_refresh
      schedule: ${schedule}
      sql: "CALL refresh()"
"#;

    let mut raw_config = HashMap::new();
    raw_config.insert("test:gcpProject".to_string(), "config-project".to_string());

    let mock = MockCallback::new();
    let (eval, has_errors) = eval_with_mock_and_config(source, mock, raw_config, &[]);
    assert!(!has_errors, "errors: {}", eval.diags);

    let regs = eval.callback().registrations();
    assert_eq!(regs.len(), 1);
    assert_eq!(regs[0].type_token, "gcpx:scheduler/sqlJob:SqlJob");
    assert_eq!(
        regs[0].inputs.get("project").and_then(|v| v.as_str()),
        Some("config-project")
    );
    assert_eq!(
        regs[0].inputs.get("schedule").and_then(|v| v.as_str()),
        Some("0 2 * * *")
    );
}

#[test]
fn test_gcpx_outputs_deep_access() {
    let source = r#"
name: test
runtime: yaml
resources:
  proj:
    type: gcpx:dbt:Project
    properties:
      project: my-gcp-project
      dataset: analytics
      sources:
        src1:
          dataset: raw_data
outputs:
  deepDataset: ${proj.context.sources.src1.dataset}
  projId: ${proj.id}
  projUrn: ${proj.urn}
"#;

    let mock = MockCallback::with_register_responses(vec![resp(
        "urn:pulumi:test::test::gcpx:dbt/project:Project::proj",
        "project-deep-001",
        hmap(vec![
            ("project", s("my-gcp-project")),
            (
                "context",
                obj(vec![
                    ("gcpProject", s("my-gcp-project")),
                    ("dataset", s("analytics")),
                    (
                        "sources",
                        obj(vec![("src1", obj(vec![("dataset", s("raw_data"))]))]),
                    ),
                ]),
            ),
        ]),
    )]);

    let (eval, has_errors) = eval_with_mock(source, mock);
    assert!(!has_errors, "errors: {}", eval.diags);

    assert_eq!(eval.callback().registrations().len(), 1);
    assert_eq!(
        eval.outputs.get("deepDataset").and_then(|v| v.as_str()),
        Some("raw_data")
    );
    assert_eq!(
        eval.outputs.get("projId").and_then(|v| v.as_str()),
        Some("project-deep-001")
    );
    assert_eq!(
        eval.outputs.get("projUrn").and_then(|v| v.as_str()),
        Some("urn:pulumi:test::test::gcpx:dbt/project:Project::proj")
    );
}

// ---------------------------------------------------------------------------
// Tests — New: Table Advanced Features (B1–B6)
// ---------------------------------------------------------------------------

/// B1: Table with timePartitioning — verify nested object passes through.
#[test]
fn test_gcpx_table_time_partitioning() {
    let source = r#"
name: test
runtime: yaml
resources:
  eventsTable:
    type: gcpx:bigquery:Table
    properties:
      project: my-gcp-project
      dataset: analytics
      tableId: events_daily
      timePartitioning:
        type: DAY
        field: event_date
outputs:
  tableType: ${eventsTable.tableType}
"#;

    let mock = MockCallback::with_register_responses(vec![resp(
        "urn:pulumi:test::test::gcpx:bigquery/table:Table::eventsTable",
        "projects/my-gcp-project/datasets/analytics/tables/events_daily",
        hmap(vec![
            ("tableType", s("TABLE")),
            ("tableId", s("events_daily")),
        ]),
    )]);

    let (eval, has_errors) = eval_with_mock(source, mock);
    assert!(!has_errors, "errors: {}", eval.diags);

    let regs = eval.callback().registrations();
    assert_eq!(regs.len(), 1);
    let tp = regs[0]
        .inputs
        .get("timePartitioning")
        .expect("timePartitioning missing");
    assert_eq!(obj_field(tp, "type").as_str(), Some("DAY"));
    assert_eq!(obj_field(tp, "field").as_str(), Some("event_date"));
    assert_eq!(
        eval.outputs.get("tableType").and_then(|v| v.as_str()),
        Some("TABLE")
    );
}

/// B2: Table with rangePartitioning — verify nested range object structure.
#[test]
fn test_gcpx_table_range_partitioning() {
    let source = r#"
name: test
runtime: yaml
resources:
  usersTable:
    type: gcpx:bigquery:Table
    properties:
      project: my-gcp-project
      dataset: analytics
      tableId: users_range
      rangePartitioning:
        field: user_id
        range:
          start: 0
          end: 1000000
          interval: 10000
outputs:
  tableType: ${usersTable.tableType}
"#;

    let mock = MockCallback::with_register_responses(vec![resp(
        "urn:pulumi:test::test::gcpx:bigquery/table:Table::usersTable",
        "projects/my-gcp-project/datasets/analytics/tables/users_range",
        hmap(vec![
            ("tableType", s("TABLE")),
            ("tableId", s("users_range")),
        ]),
    )]);

    let (eval, has_errors) = eval_with_mock(source, mock);
    assert!(!has_errors, "errors: {}", eval.diags);

    let regs = eval.callback().registrations();
    assert_eq!(regs.len(), 1);
    let rp = regs[0]
        .inputs
        .get("rangePartitioning")
        .expect("rangePartitioning missing");
    assert_eq!(obj_field(rp, "field").as_str(), Some("user_id"));
    let range = obj_field(rp, "range");
    assert_eq!(obj_field(range, "start").as_number(), Some(0.0));
    assert_eq!(obj_field(range, "end").as_number(), Some(1000000.0));
    assert_eq!(obj_field(range, "interval").as_number(), Some(10000.0));
}

/// B3: Table with clusterings array + timePartitioning.
#[test]
fn test_gcpx_table_clustering() {
    let source = r#"
name: test
runtime: yaml
resources:
  eventsTable:
    type: gcpx:bigquery:Table
    properties:
      project: my-gcp-project
      dataset: analytics
      tableId: events_clustered
      timePartitioning:
        type: DAY
        field: event_date
      clusterings:
        - region
        - category
        - event_type
outputs:
  tableType: ${eventsTable.tableType}
"#;

    let mock = MockCallback::with_register_responses(vec![resp(
        "urn:pulumi:test::test::gcpx:bigquery/table:Table::eventsTable",
        "projects/my-gcp-project/datasets/analytics/tables/events_clustered",
        hmap(vec![("tableType", s("TABLE"))]),
    )]);

    let (eval, has_errors) = eval_with_mock(source, mock);
    assert!(!has_errors, "errors: {}", eval.diags);

    let regs = eval.callback().registrations();
    assert_eq!(regs.len(), 1);

    // Verify clusterings is a list of strings
    match regs[0]
        .inputs
        .get("clusterings")
        .expect("clusterings missing")
    {
        Value::List(items) => {
            assert_eq!(items.len(), 3);
            assert_eq!(items[0].as_str(), Some("region"));
            assert_eq!(items[1].as_str(), Some("category"));
            assert_eq!(items[2].as_str(), Some("event_type"));
        }
        other => panic!("clusterings should be a List, got {:?}", other),
    }

    // Verify timePartitioning is also present
    let tp = regs[0]
        .inputs
        .get("timePartitioning")
        .expect("timePartitioning missing");
    assert_eq!(obj_field(tp, "type").as_str(), Some("DAY"));
}

/// B4: Materialized view with query, enableRefresh, refreshIntervalMs.
#[test]
fn test_gcpx_materialized_view() {
    let source = r#"
name: test
runtime: yaml
resources:
  activeUsersMV:
    type: gcpx:bigquery:Table
    properties:
      project: my-gcp-project
      dataset: analytics
      tableId: active_users_mv
      materializedView:
        query: "SELECT user_id, COUNT(*) as cnt FROM `analytics.events` GROUP BY user_id"
        enableRefresh: true
        refreshIntervalMs: 3600000
outputs:
  tableType: ${activeUsersMV.tableType}
"#;

    let mock = MockCallback::with_register_responses(vec![resp(
        "urn:pulumi:test::test::gcpx:bigquery/table:Table::activeUsersMV",
        "projects/my-gcp-project/datasets/analytics/tables/active_users_mv",
        hmap(vec![
            ("tableType", s("MATERIALIZED_VIEW")),
            ("tableId", s("active_users_mv")),
        ]),
    )]);

    let (eval, has_errors) = eval_with_mock(source, mock);
    assert!(!has_errors, "errors: {}", eval.diags);

    let regs = eval.callback().registrations();
    assert_eq!(regs.len(), 1);
    let mv = regs[0]
        .inputs
        .get("materializedView")
        .expect("materializedView missing");
    assert_eq!(
        obj_field(mv, "query").as_str(),
        Some("SELECT user_id, COUNT(*) as cnt FROM `analytics.events` GROUP BY user_id")
    );
    assert_eq!(obj_field(mv, "enableRefresh").as_bool(), Some(true));
    assert_eq!(
        obj_field(mv, "refreshIntervalMs").as_number(),
        Some(3600000.0)
    );
    assert_eq!(
        eval.outputs.get("tableType").and_then(|v| v.as_str()),
        Some("MATERIALIZED_VIEW")
    );
}

/// B5: TableSchema with nested STRUCT column containing sub-fields.
#[test]
fn test_gcpx_table_schema_nested_struct() {
    let source = r#"
name: test
runtime: yaml
resources:
  eventsSchema:
    type: gcpx:bigquery:TableSchema
    properties:
      project: my-gcp-project
      dataset: analytics
      table: events
      columns:
        - name: event_id
          type: STRING
          mode: REQUIRED
        - name: metadata
          type: STRUCT
          fields:
            - name: source
              type: STRING
            - name: version
              type: INT64
            - name: tags
              type: STRING
              mode: REPEATED
        - name: amount
          type: FLOAT64
outputs:
  fingerprint: ${eventsSchema.schemaFingerprint}
"#;

    let mock = MockCallback::with_register_responses(vec![resp(
        "urn:pulumi:test::test::gcpx:bigquery/tableSchema:TableSchema::eventsSchema",
        "schema-nested-001",
        hmap(vec![("schemaFingerprint", s("nested-fp-xyz"))]),
    )]);

    let (eval, has_errors) = eval_with_mock(source, mock);
    assert!(!has_errors, "errors: {}", eval.diags);

    let regs = eval.callback().registrations();
    assert_eq!(regs.len(), 1);
    assert_eq!(regs[0].type_token, "gcpx:bigquery/tableSchema:TableSchema");

    match regs[0].inputs.get("columns").expect("columns missing") {
        Value::List(items) => {
            assert_eq!(items.len(), 3);
            // Second column is the STRUCT with nested fields
            let metadata_col = &items[1];
            assert_eq!(obj_field(metadata_col, "name").as_str(), Some("metadata"));
            assert_eq!(obj_field(metadata_col, "type").as_str(), Some("STRUCT"));
            match obj_field(metadata_col, "fields") {
                Value::List(sub_fields) => {
                    assert_eq!(sub_fields.len(), 3);
                    assert_eq!(obj_field(&sub_fields[0], "name").as_str(), Some("source"));
                    assert_eq!(obj_field(&sub_fields[1], "name").as_str(), Some("version"));
                    assert_eq!(obj_field(&sub_fields[2], "mode").as_str(), Some("REPEATED"));
                }
                other => panic!("expected List for fields, got {:?}", other),
            }
        }
        other => panic!("columns should be a List, got {:?}", other),
    }
    assert_eq!(
        eval.outputs.get("fingerprint").and_then(|v| v.as_str()),
        Some("nested-fp-xyz")
    );
}

/// B6: Table + TableSchema with dependsOn — verify registration order.
#[test]
fn test_gcpx_table_plus_schema() {
    let source = r#"
name: test
runtime: yaml
resources:
  eventsTable:
    type: gcpx:bigquery:Table
    properties:
      project: my-gcp-project
      dataset: analytics
      tableId: raw_events
  eventsSchema:
    type: gcpx:bigquery:TableSchema
    properties:
      project: my-gcp-project
      dataset: analytics
      table: raw_events
      columns:
        - name: user_id
          type: STRING
          mode: REQUIRED
        - name: event_date
          type: DATE
    options:
      dependsOn:
        - ${eventsTable}
outputs:
  tableType: ${eventsTable.tableType}
  fingerprint: ${eventsSchema.schemaFingerprint}
"#;

    let mock = MockCallback::with_register_responses(vec![
        resp(
            "urn:pulumi:test::test::gcpx:bigquery/table:Table::eventsTable",
            "table-001",
            hmap(vec![
                ("tableType", s("TABLE")),
                ("tableId", s("raw_events")),
            ]),
        ),
        resp(
            "urn:pulumi:test::test::gcpx:bigquery/tableSchema:TableSchema::eventsSchema",
            "schema-001",
            hmap(vec![("schemaFingerprint", s("fp-after-table"))]),
        ),
    ]);

    let (eval, has_errors) = eval_with_mock(source, mock);
    assert!(!has_errors, "errors: {}", eval.diags);

    let regs = eval.callback().registrations();
    assert_eq!(regs.len(), 2);
    // Table must be registered first (schema depends on it)
    assert_eq!(regs[0].name, "eventsTable");
    assert_eq!(regs[0].type_token, "gcpx:bigquery/table:Table");
    assert_eq!(regs[1].name, "eventsSchema");
    assert_eq!(regs[1].type_token, "gcpx:bigquery/tableSchema:TableSchema");
    assert_eq!(
        eval.outputs.get("tableType").and_then(|v| v.as_str()),
        Some("TABLE")
    );
    assert_eq!(
        eval.outputs.get("fingerprint").and_then(|v| v.as_str()),
        Some("fp-after-table")
    );
}

// ---------------------------------------------------------------------------
// Tests — New: Multi-file dbt (B7–B9)
// ---------------------------------------------------------------------------

/// B7: Multi-file — main + extra file, resources from both merge and evaluate.
#[test]
fn test_gcpx_dbt_multifile_plain() {
    let main_src = r#"
name: test
runtime: yaml
outputs:
  modelTableRef: ${stgModel.modelOutput.tableRef}
"#;
    let extra_src = r#"
resources:
  dbtProject:
    type: gcpx:dbt:Project
    properties:
      project: my-gcp-project
      dataset: analytics
  stgModel:
    type: gcpx:dbt:Model
    properties:
      name: stg_events
      sql: "SELECT 1 as id"
      context: ${dbtProject.context}
"#;

    let mock = MockCallback::with_register_responses(vec![
        resp(
            "urn:pulumi:test::test::gcpx:dbt/project:Project::dbtProject",
            "project-001",
            hmap(vec![
                ("project", s("my-gcp-project")),
                (
                    "context",
                    obj(vec![
                        ("gcpProject", s("my-gcp-project")),
                        ("dataset", s("analytics")),
                    ]),
                ),
            ]),
        ),
        resp(
            "urn:pulumi:test::test::gcpx:dbt/model:Model::stgModel",
            "model-001",
            hmap(vec![
                ("name", s("stg_events")),
                (
                    "modelOutput",
                    obj(vec![
                        ("resolvedSql", s("SELECT 1 as id")),
                        ("tableRef", s("`my-gcp-project.analytics.stg_events`")),
                        ("materialization", s("view")),
                    ]),
                ),
            ]),
        ),
    ]);

    let (eval, has_errors) = eval_multifile(main_src, vec![("Pulumi.dbt.yaml", extra_src)], mock);
    assert!(!has_errors, "errors: {}", eval.diags);

    let regs = eval.callback().registrations();
    assert_eq!(regs.len(), 2);
    assert_eq!(
        eval.outputs.get("modelTableRef").and_then(|v| v.as_str()),
        Some("`my-gcp-project.analytics.stg_events`")
    );
}

/// B8: Multi-file with cross-file ${} references — resource in file 2 depends on resource in file 1.
#[test]
fn test_gcpx_dbt_multifile_cross_file_deps() {
    let main_src = r#"
name: test
runtime: yaml
outputs:
  tableRef: ${stgModel.modelOutput.tableRef}
"#;
    let dbt_src = r#"
resources:
  dbtProject:
    type: gcpx:dbt:Project
    properties:
      project: my-gcp-project
      dataset: analytics
"#;
    let models_src = r#"
resources:
  stgModel:
    type: gcpx:dbt:Model
    properties:
      name: stg_events
      sql: "SELECT 1 as id"
      context: ${dbtProject.context}
"#;

    let mock = MockCallback::with_register_responses(vec![
        resp(
            "urn:pulumi:test::test::gcpx:dbt/project:Project::dbtProject",
            "project-001",
            hmap(vec![
                ("project", s("my-gcp-project")),
                (
                    "context",
                    obj(vec![
                        ("gcpProject", s("my-gcp-project")),
                        ("dataset", s("analytics")),
                    ]),
                ),
            ]),
        ),
        resp(
            "urn:pulumi:test::test::gcpx:dbt/model:Model::stgModel",
            "model-001",
            hmap(vec![
                ("name", s("stg_events")),
                (
                    "modelOutput",
                    obj(vec![
                        ("tableRef", s("`my-gcp-project.analytics.stg_events`")),
                        ("materialization", s("view")),
                    ]),
                ),
            ]),
        ),
    ]);

    let (eval, has_errors) = eval_multifile(
        main_src,
        vec![
            ("Pulumi.dbt.yaml", dbt_src),
            ("Pulumi.models.yaml", models_src),
        ],
        mock,
    );
    assert!(!has_errors, "errors: {}", eval.diags);

    let regs = eval.callback().registrations();
    assert_eq!(regs.len(), 2);
    // dbtProject must come before stgModel (dependency via ${dbtProject.context})
    assert_eq!(regs[0].name, "dbtProject");
    assert_eq!(regs[1].name, "stgModel");

    // Cross-file reference resolved correctly
    assert_eq!(
        obj_field(
            regs[1].inputs.get("context").expect("context missing"),
            "gcpProject"
        )
        .as_str(),
        Some("my-gcp-project")
    );
    assert_eq!(
        eval.outputs.get("tableRef").and_then(|v| v.as_str()),
        Some("`my-gcp-project.analytics.stg_events`")
    );
}

/// B9: Three-file layout — main + dbt + models with full dependency chain.
#[test]
fn test_gcpx_dbt_multifile_three_files() {
    let main_src = r#"
name: test
runtime: yaml
outputs:
  macroName: ${ctdMacro.macroOutput.name}
  modelTableRef: ${stgModel.modelOutput.tableRef}
"#;
    let dbt_src = r#"
resources:
  dbtProject:
    type: gcpx:dbt:Project
    properties:
      project: my-gcp-project
      dataset: analytics
  ctdMacro:
    type: gcpx:dbt:Macro
    properties:
      name: cents_to_dollars
      sql: "CAST(amount AS FLOAT64) / 100.0"
      args:
        - amount
"#;
    let models_src = r#"
resources:
  stgModel:
    type: gcpx:dbt:Model
    properties:
      name: stg_orders
      sql: "SELECT 1 as id, 1999 as amount_cents"
      context: ${dbtProject.context}
      macros:
        cents_to_dollars: ${ctdMacro.macroOutput}
"#;

    // ctdMacro sorts before dbtProject alphabetically, so it registers first
    let mock = MockCallback::with_register_responses(vec![
        resp(
            "urn:pulumi:test::test::gcpx:dbt/macro:Macro::ctdMacro",
            "macro-001",
            hmap(vec![
                ("name", s("cents_to_dollars")),
                (
                    "macroOutput",
                    obj(vec![
                        ("name", s("cents_to_dollars")),
                        ("sql", s("CAST(amount AS FLOAT64) / 100.0")),
                    ]),
                ),
            ]),
        ),
        resp(
            "urn:pulumi:test::test::gcpx:dbt/project:Project::dbtProject",
            "project-001",
            hmap(vec![
                ("project", s("my-gcp-project")),
                (
                    "context",
                    obj(vec![
                        ("gcpProject", s("my-gcp-project")),
                        ("dataset", s("analytics")),
                    ]),
                ),
            ]),
        ),
        resp(
            "urn:pulumi:test::test::gcpx:dbt/model:Model::stgModel",
            "model-001",
            hmap(vec![
                ("name", s("stg_orders")),
                (
                    "modelOutput",
                    obj(vec![
                        ("resolvedSql", s("SELECT 1 as id, 1999 as amount_cents")),
                        ("tableRef", s("`my-gcp-project.analytics.stg_orders`")),
                        ("materialization", s("view")),
                    ]),
                ),
            ]),
        ),
    ]);

    let (eval, has_errors) = eval_multifile(
        main_src,
        vec![
            ("Pulumi.dbt.yaml", dbt_src),
            ("Pulumi.models.yaml", models_src),
        ],
        mock,
    );
    assert!(!has_errors, "errors: {}", eval.diags);

    let regs = eval.callback().registrations();
    assert_eq!(regs.len(), 3);
    // ctdMacro and dbtProject have no deps (alphabetical order), stgModel depends on both
    assert_eq!(regs[0].name, "ctdMacro");
    assert_eq!(regs[1].name, "dbtProject");
    assert_eq!(regs[2].name, "stgModel");
    assert_eq!(
        eval.outputs.get("macroName").and_then(|v| v.as_str()),
        Some("cents_to_dollars")
    );
    assert_eq!(
        eval.outputs.get("modelTableRef").and_then(|v| v.as_str()),
        Some("`my-gcp-project.analytics.stg_orders`")
    );
}

// ---------------------------------------------------------------------------
// Tests — New: dbt syntax passthrough (B10–B11)
// ---------------------------------------------------------------------------

/// B10: Model SQL with {{ ref('...') }} passes through as literal string.
#[test]
fn test_gcpx_dbt_ref_passthrough() {
    let source = r#"
name: test
runtime: yaml
resources:
  martModel:
    type: gcpx:dbt:Model
    properties:
      name: mart_daily
      sql: "SELECT date, count(*) as cnt FROM {{ ref('stg_events') }} GROUP BY date"
      context:
        gcpProject: my-gcp-project
        dataset: analytics
outputs:
  resolvedSql: ${martModel.modelOutput.resolvedSql}
"#;

    let mock = MockCallback::with_register_responses(vec![resp(
        "urn:pulumi:test::test::gcpx:dbt/model:Model::martModel",
        "model-mart-001",
        hmap(vec![
            ("name", s("mart_daily")),
            ("modelOutput", obj(vec![
                ("resolvedSql", s("SELECT date, count(*) as cnt FROM `my-gcp-project.analytics.stg_events` GROUP BY date")),
                ("tableRef", s("`my-gcp-project.analytics.mart_daily`")),
            ])),
        ]),
    )]);

    let (eval, has_errors) = eval_with_mock(source, mock);
    assert!(!has_errors, "errors: {}", eval.diags);

    let regs = eval.callback().registrations();
    assert_eq!(regs.len(), 1);
    // The {{ ref(...) }} should pass through to the provider as a literal string
    assert_eq!(
        regs[0].inputs.get("sql").and_then(|v| v.as_str()),
        Some("SELECT date, count(*) as cnt FROM {{ ref('stg_events') }} GROUP BY date")
    );
}

/// B11: Model SQL with {{ source('...', '...') }} passes through as literal string.
#[test]
fn test_gcpx_dbt_source_passthrough() {
    let source = r#"
name: test
runtime: yaml
resources:
  stgModel:
    type: gcpx:dbt:Model
    properties:
      name: stg_events
      sql: "SELECT * FROM {{ source('raw', 'events') }} WHERE active = true"
      context:
        gcpProject: my-gcp-project
        dataset: analytics
outputs:
  resolvedSql: ${stgModel.modelOutput.resolvedSql}
"#;

    let mock = MockCallback::with_register_responses(vec![resp(
        "urn:pulumi:test::test::gcpx:dbt/model:Model::stgModel",
        "model-stg-001",
        hmap(vec![
            ("name", s("stg_events")),
            (
                "modelOutput",
                obj(vec![
                    (
                        "resolvedSql",
                        s("SELECT * FROM `my-gcp-project.raw.events` WHERE active = true"),
                    ),
                    ("tableRef", s("`my-gcp-project.analytics.stg_events`")),
                ]),
            ),
        ]),
    )]);

    let (eval, has_errors) = eval_with_mock(source, mock);
    assert!(!has_errors, "errors: {}", eval.diags);

    let regs = eval.callback().registrations();
    assert_eq!(regs.len(), 1);
    // The {{ source(...) }} should pass through to the provider as a literal string
    assert_eq!(
        regs[0].inputs.get("sql").and_then(|v| v.as_str()),
        Some("SELECT * FROM {{ source('raw', 'events') }} WHERE active = true")
    );
}

// ---------------------------------------------------------------------------
// Tests — New: Multi-SQL dbt project (B12–B14)
// ---------------------------------------------------------------------------

/// B12: Multi-file multi-model — 3 models under one dbt:Project, with macro + modelRefs.
/// Tests the full dependency chain: Project + Macro → stgOrders, stgUsers → martRevenue.
#[test]
fn test_gcpx_dbt_multi_sql_multi_model() {
    let main_src = r#"
name: test
runtime: yaml
outputs:
  stgOrdersRef: ${stgOrders.modelOutput.tableRef}
  stgUsersRef: ${stgUsers.modelOutput.tableRef}
  martRevenueRef: ${martRevenue.modelOutput.tableRef}
"#;
    let dbt_src = r#"
resources:
  dbtProject:
    type: gcpx:dbt:Project
    properties:
      project: my-gcp-project
      dataset: analytics
      sources:
        raw_src:
          dataset: raw_data
          tables:
            - orders
            - users
  centsToDollars:
    type: gcpx:dbt:Macro
    properties:
      name: cents_to_dollars
      sql: "CAST(amount_cents AS FLOAT64) / 100.0"
      args:
        - amount_cents
"#;
    let models_src = r#"
resources:
  stgOrders:
    type: gcpx:dbt:Model
    properties:
      name: stg_orders
      sql: "SELECT order_id, user_id, amount_cents FROM orders WHERE status != 'cancelled'"
      context: ${dbtProject.context}
  stgUsers:
    type: gcpx:dbt:Model
    properties:
      name: stg_users
      sql: "SELECT user_id, email, region FROM users WHERE email IS NOT NULL"
      context: ${dbtProject.context}
  martRevenue:
    type: gcpx:dbt:Model
    properties:
      name: mart_revenue
      sql: "SELECT region, SUM(amount) as total FROM stg_orders o JOIN stg_users u ON o.user_id = u.user_id GROUP BY region"
      context: ${dbtProject.context}
      macros:
        cents_to_dollars: ${centsToDollars.macroOutput}
      modelRefs:
        stg_orders: ${stgOrders.modelOutput}
        stg_users: ${stgUsers.modelOutput}
"#;

    let mock = MockCallback::with_register_responses(vec![
        // centsToDollars (c) before dbtProject (d) alphabetically
        resp(
            "urn:pulumi:test::test::gcpx:dbt/macro:Macro::centsToDollars",
            "macro-001",
            hmap(vec![
                ("name", s("cents_to_dollars")),
                ("macroOutput", obj(vec![
                    ("name", s("cents_to_dollars")),
                    ("sql", s("CAST(amount_cents AS FLOAT64) / 100.0")),
                ])),
            ]),
        ),
        resp(
            "urn:pulumi:test::test::gcpx:dbt/project:Project::dbtProject",
            "project-001",
            hmap(vec![
                ("project", s("my-gcp-project")),
                ("context", obj(vec![
                    ("gcpProject", s("my-gcp-project")),
                    ("dataset", s("analytics")),
                    ("sources", obj(vec![
                        ("raw_src", obj(vec![
                            ("dataset", s("raw_data")),
                            ("tables", list(vec![s("orders"), s("users")])),
                        ])),
                    ])),
                ])),
            ]),
        ),
        // stgOrders and stgUsers depend on dbtProject only (not on macro/each other)
        // martRevenue depends on stgOrders + stgUsers + centsToDollars
        // Expected order: stgOrders, stgUsers (both depend only on dbtProject, alphabetical)
        resp(
            "urn:pulumi:test::test::gcpx:dbt/model:Model::stgOrders",
            "model-orders-001",
            hmap(vec![
                ("name", s("stg_orders")),
                ("modelOutput", obj(vec![
                    ("resolvedSql", s("SELECT order_id, user_id, amount_cents FROM orders WHERE status != 'cancelled'")),
                    ("tableRef", s("`my-gcp-project.analytics.stg_orders`")),
                    ("materialization", s("view")),
                ])),
            ]),
        ),
        resp(
            "urn:pulumi:test::test::gcpx:dbt/model:Model::stgUsers",
            "model-users-001",
            hmap(vec![
                ("name", s("stg_users")),
                ("modelOutput", obj(vec![
                    ("resolvedSql", s("SELECT user_id, email, region FROM users WHERE email IS NOT NULL")),
                    ("tableRef", s("`my-gcp-project.analytics.stg_users`")),
                    ("materialization", s("view")),
                ])),
            ]),
        ),
        resp(
            "urn:pulumi:test::test::gcpx:dbt/model:Model::martRevenue",
            "model-revenue-001",
            hmap(vec![
                ("name", s("mart_revenue")),
                ("modelOutput", obj(vec![
                    ("resolvedSql", s("SELECT region, SUM(amount) as total FROM stg_orders o JOIN stg_users u ON o.user_id = u.user_id GROUP BY region")),
                    ("tableRef", s("`my-gcp-project.analytics.mart_revenue`")),
                    ("materialization", s("table")),
                ])),
            ]),
        ),
    ]);

    let (eval, has_errors) = eval_multifile(
        main_src,
        vec![
            ("Pulumi.dbt.yaml", dbt_src),
            ("Pulumi.models.yaml", models_src),
        ],
        mock,
    );
    assert!(!has_errors, "errors: {}", eval.diags);

    let regs = eval.callback().registrations();
    assert_eq!(regs.len(), 5);

    // centsToDollars + dbtProject first (no deps), then stg models, then mart last
    assert_eq!(regs[0].name, "centsToDollars");
    assert_eq!(regs[1].name, "dbtProject");
    assert_eq!(regs[2].name, "stgOrders");
    assert_eq!(regs[3].name, "stgUsers");
    assert_eq!(regs[4].name, "martRevenue");

    // martRevenue received both modelRefs
    let mart_refs = regs[4].inputs.get("modelRefs").expect("modelRefs missing");
    assert_eq!(
        obj_field(obj_field(mart_refs, "stg_orders"), "tableRef").as_str(),
        Some("`my-gcp-project.analytics.stg_orders`")
    );
    assert_eq!(
        obj_field(obj_field(mart_refs, "stg_users"), "tableRef").as_str(),
        Some("`my-gcp-project.analytics.stg_users`")
    );

    // martRevenue received macro
    let mart_macros = regs[4].inputs.get("macros").expect("macros missing");
    assert_eq!(
        obj_field(obj_field(mart_macros, "cents_to_dollars"), "name").as_str(),
        Some("cents_to_dollars")
    );

    // Outputs resolve correctly
    assert_eq!(
        eval.outputs.get("stgOrdersRef").and_then(|v| v.as_str()),
        Some("`my-gcp-project.analytics.stg_orders`")
    );
    assert_eq!(
        eval.outputs.get("stgUsersRef").and_then(|v| v.as_str()),
        Some("`my-gcp-project.analytics.stg_users`")
    );
    assert_eq!(
        eval.outputs.get("martRevenueRef").and_then(|v| v.as_str()),
        Some("`my-gcp-project.analytics.mart_revenue`")
    );
}

/// B13: Multi-file multi-SQL with readFile() — SQL loaded from .sql files via Jinja.
/// Uses temp directory with actual .sql files on disk + Jinja preprocessor.
#[test]
fn test_gcpx_dbt_multi_sql_readfile() {
    use pulumi_rs_yaml_core::jinja::{JinjaContext, UndefinedMode};
    use pulumi_rs_yaml_core::multi_file::load_project;
    use std::fs;

    let dir = tempfile::tempdir().unwrap();
    let sql_dir = dir.path().join("sql");
    fs::create_dir(&sql_dir).unwrap();

    // Write Pulumi YAML files
    fs::write(
        dir.path().join("Pulumi.yaml"),
        r#"name: test
runtime: yaml
outputs:
  stgOrdersRef: ${stgOrders.modelOutput.tableRef}
  martRef: ${martRevenue.modelOutput.tableRef}
"#,
    )
    .unwrap();

    fs::write(
        dir.path().join("Pulumi.dbt.yaml"),
        r#"resources:
  dbtProject:
    type: gcpx:dbt:Project
    properties:
      project: my-gcp-project
      dataset: analytics
  ctdMacro:
    type: gcpx:dbt:Macro
    properties:
      name: cents_to_dollars
      sql: "CAST(amount_cents AS FLOAT64) / 100.0"
      args:
        - amount_cents
"#,
    )
    .unwrap();

    fs::write(
        dir.path().join("Pulumi.models.yaml"),
        r#"resources:
  stgOrders:
    type: gcpx:dbt:Model
    properties:
      name: stg_orders
      sql: |
        {{ readFile("sql/stg_orders.sql") }}
      context: ${dbtProject.context}
  martRevenue:
    type: gcpx:dbt:Model
    properties:
      name: mart_revenue
      sql: |
        {{ readFile("sql/mart_revenue.sql") }}
      context: ${dbtProject.context}
      macros:
        cents_to_dollars: ${ctdMacro.macroOutput}
      modelRefs:
        stg_orders: ${stgOrders.modelOutput}
"#,
    )
    .unwrap();

    // Write SQL files
    fs::write(
        sql_dir.join("stg_orders.sql"),
        "SELECT order_id, user_id, amount_cents FROM raw.orders WHERE status != 'cancelled'",
    )
    .unwrap();

    fs::write(
        sql_dir.join("mart_revenue.sql"),
        "SELECT region, SUM(amount) as total FROM stg_orders GROUP BY region",
    )
    .unwrap();

    // Load project with Jinja preprocessing (which resolves readFile)
    let config = HashMap::new();
    let ctx = JinjaContext {
        project_name: "test",
        stack_name: "dev",
        cwd: dir.path().to_str().unwrap(),
        organization: "",
        root_directory: dir.path().to_str().unwrap(),
        config: &config,
        project_dir: dir.path().to_str().unwrap(),
        undefined: UndefinedMode::Strict,
    };

    let (merged, diags) = load_project(dir.path(), Some(&ctx));
    assert!(!diags.has_errors(), "load_project errors: {}", diags);
    assert_eq!(merged.resource_count(), 4); // dbtProject, ctdMacro, stgOrders, martRevenue

    // Now evaluate with mock callback
    let template = merged.as_template_decl();
    let template: &'static _ = Box::leak(Box::new(template));

    // ctdMacro (c) before dbtProject (d) alphabetically
    let mock = MockCallback::with_register_responses(vec![
        resp(
            "urn:pulumi:test::test::gcpx:dbt/macro:Macro::ctdMacro",
            "macro-001",
            hmap(vec![
                ("name", s("cents_to_dollars")),
                (
                    "macroOutput",
                    obj(vec![
                        ("name", s("cents_to_dollars")),
                        ("sql", s("CAST(amount_cents AS FLOAT64) / 100.0")),
                    ]),
                ),
            ]),
        ),
        resp(
            "urn:pulumi:test::test::gcpx:dbt/project:Project::dbtProject",
            "project-001",
            hmap(vec![
                ("project", s("my-gcp-project")),
                (
                    "context",
                    obj(vec![
                        ("gcpProject", s("my-gcp-project")),
                        ("dataset", s("analytics")),
                    ]),
                ),
            ]),
        ),
        resp(
            "urn:pulumi:test::test::gcpx:dbt/model:Model::stgOrders",
            "model-orders-001",
            hmap(vec![
                ("name", s("stg_orders")),
                (
                    "modelOutput",
                    obj(vec![
                        ("tableRef", s("`my-gcp-project.analytics.stg_orders`")),
                        ("materialization", s("view")),
                    ]),
                ),
            ]),
        ),
        resp(
            "urn:pulumi:test::test::gcpx:dbt/model:Model::martRevenue",
            "model-revenue-001",
            hmap(vec![
                ("name", s("mart_revenue")),
                (
                    "modelOutput",
                    obj(vec![
                        ("tableRef", s("`my-gcp-project.analytics.mart_revenue`")),
                        ("materialization", s("table")),
                    ]),
                ),
            ]),
        ),
    ]);

    let mut eval = Evaluator::with_callback(
        "test".to_string(),
        "dev".to_string(),
        dir.path().to_str().unwrap().to_string(),
        false,
        mock,
    );
    eval.evaluate_template(template, &config, &[]);
    assert!(
        !eval.diags.has_errors(),
        "evaluation errors: {}",
        eval.diags
    );

    let regs = eval.callback().registrations();
    assert_eq!(regs.len(), 4);

    // stgOrders SQL should contain the contents from the .sql file (readFile resolved)
    let stg_orders_reg = regs.iter().find(|r| r.name == "stgOrders").unwrap();
    let stg_sql = stg_orders_reg
        .inputs
        .get("sql")
        .and_then(|v| v.as_str())
        .unwrap();
    assert!(
        stg_sql.contains("SELECT order_id"),
        "stgOrders sql should contain readFile content, got: {}",
        stg_sql
    );
    assert!(
        stg_sql.contains("raw.orders"),
        "stgOrders sql should reference raw.orders, got: {}",
        stg_sql
    );

    // martRevenue SQL should contain contents from mart_revenue.sql
    let mart_reg = regs.iter().find(|r| r.name == "martRevenue").unwrap();
    let mart_sql = mart_reg.inputs.get("sql").and_then(|v| v.as_str()).unwrap();
    assert!(
        mart_sql.contains("SUM(amount)"),
        "martRevenue sql should contain readFile content, got: {}",
        mart_sql
    );

    // martRevenue received modelRefs from stgOrders
    let mart_refs = mart_reg.inputs.get("modelRefs").expect("modelRefs missing");
    assert_eq!(
        obj_field(obj_field(mart_refs, "stg_orders"), "tableRef").as_str(),
        Some("`my-gcp-project.analytics.stg_orders`")
    );

    // Outputs resolve correctly
    assert_eq!(
        eval.outputs.get("stgOrdersRef").and_then(|v| v.as_str()),
        Some("`my-gcp-project.analytics.stg_orders`")
    );
    assert_eq!(
        eval.outputs.get("martRef").and_then(|v| v.as_str()),
        Some("`my-gcp-project.analytics.mart_revenue`")
    );
}

/// B14: Multi-file with fn::readFile (YAML builtin, not Jinja) for SQL loading.
#[test]
fn test_gcpx_dbt_multi_sql_fn_readfile() {
    use std::fs;

    let dir = tempfile::tempdir().unwrap();
    let sql_dir = dir.path().join("sql");
    fs::create_dir(&sql_dir).unwrap();

    // Write SQL file
    fs::write(
        sql_dir.join("stg_events.sql"),
        "SELECT event_id, user_id, event_type FROM raw.events WHERE event_type IS NOT NULL",
    )
    .unwrap();

    let sql_path = sql_dir.join("stg_events.sql");
    let source = format!(
        r#"
name: test
runtime: yaml
resources:
  dbtProject:
    type: gcpx:dbt:Project
    properties:
      project: my-gcp-project
      dataset: analytics
  stgEvents:
    type: gcpx:dbt:Model
    properties:
      name: stg_events
      sql:
        fn::readFile: {}
      context: ${{dbtProject.context}}
outputs:
  tableRef: ${{stgEvents.modelOutput.tableRef}}
"#,
        sql_path.display()
    );

    let mock = MockCallback::with_register_responses(vec![
        resp(
            "urn:pulumi:test::test::gcpx:dbt/project:Project::dbtProject",
            "project-001",
            hmap(vec![
                ("project", s("my-gcp-project")),
                (
                    "context",
                    obj(vec![
                        ("gcpProject", s("my-gcp-project")),
                        ("dataset", s("analytics")),
                    ]),
                ),
            ]),
        ),
        resp(
            "urn:pulumi:test::test::gcpx:dbt/model:Model::stgEvents",
            "model-001",
            hmap(vec![
                ("name", s("stg_events")),
                (
                    "modelOutput",
                    obj(vec![
                        ("tableRef", s("`my-gcp-project.analytics.stg_events`")),
                        ("materialization", s("view")),
                    ]),
                ),
            ]),
        ),
    ]);

    let (template, parse_diags) = parse_template(&source, None);
    assert!(!parse_diags.has_errors(), "parse errors: {}", parse_diags);
    let template: &'static _ = Box::leak(Box::new(template));

    let mut eval = Evaluator::with_callback(
        "test".to_string(),
        "dev".to_string(),
        dir.path().to_str().unwrap().to_string(),
        false,
        mock,
    );
    let raw_config = HashMap::new();
    eval.evaluate_template(template, &raw_config, &[]);
    assert!(
        !eval.diags.has_errors(),
        "evaluation errors: {}",
        eval.diags
    );

    let regs = eval.callback().registrations();
    assert_eq!(regs.len(), 2);

    let stg_reg = regs.iter().find(|r| r.name == "stgEvents").unwrap();
    let stg_sql = stg_reg.inputs.get("sql").and_then(|v| v.as_str()).unwrap();
    assert!(
        stg_sql.contains("SELECT event_id"),
        "sql should contain file content, got: {}",
        stg_sql
    );
    assert!(
        stg_sql.contains("raw.events"),
        "sql should reference raw.events, got: {}",
        stg_sql
    );

    assert_eq!(
        eval.outputs.get("tableRef").and_then(|v| v.as_str()),
        Some("`my-gcp-project.analytics.stg_events`")
    );
}

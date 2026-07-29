"""Tests for export_sql_lineage."""

import pytest

from pulumi_yaml_rs import export_sql_lineage

PRODUCER = """\
name: data-platform
runtime: yaml
config:
  gcp:project:
    value: data-proj
resources:
  base:
    type: gcp:bigquery:Table
    properties:
      datasetId: analytics
      tableId: orders
      schema: '[{"name":"order_id","type":"STRING","description":"Order key"}]'
  view:
    type: gcp:bigquery:Table
    properties:
      datasetId: analytics
      tableId: revenue_view
      view:
        query: "SELECT o.order_id FROM `data-proj.analytics.orders` o"
"""


class TestLineageStructure:
    def test_top_level_keys(self, tmp_project):
        graph = export_sql_lineage(tmp_project(PRODUCER), "prod", "org")
        for key in (
            "schema_version",
            "organization",
            "project",
            "stack",
            "nodes",
            "edges",
            "diagnostics",
        ):
            assert key in graph
        assert graph["schema_version"] == 1

    def test_cloud_scoped_ids_and_columns(self, tmp_project):
        graph = export_sql_lineage(tmp_project(PRODUCER), "prod", "org")
        ids = {n["id"] for n in graph["nodes"]}
        assert "bq://data-proj/analytics/orders" in ids
        assert "bq://data-proj/analytics/revenue_view" in ids
        col = next(
            n for n in graph["nodes"] if n["id"] == "bq://data-proj/analytics/orders#order_id"
        )
        assert col["kind"] == "column"
        assert col["description"] == "Order key"

    def test_view_derivation_edges(self, tmp_project):
        graph = export_sql_lineage(tmp_project(PRODUCER), "prod", "org")
        assert any(
            e["relationship"] == "derives_from"
            and e["source_id"] == "bq://data-proj/analytics/revenue_view"
            and e["target_id"] == "bq://data-proj/analytics/orders"
            and e["resolution"] == "parsed"
            for e in graph["edges"]
        )
        assert any(
            e["relationship"] == "column_derives_from"
            and e["source_id"] == "bq://data-proj/analytics/revenue_view#order_id"
            for e in graph["edges"]
        )

    def test_defined_by_joins_infra_urns(self, tmp_project):
        graph = export_sql_lineage(tmp_project(PRODUCER), "prod", "org")
        view = next(
            n for n in graph["nodes"] if n["id"] == "bq://data-proj/analytics/revenue_view"
        )
        assert view["defined_by_urn"].startswith("urn:pulumi:prod::data-platform::")
        assert any(
            e["relationship"] == "defined_by" and e["target_id"] == view["defined_by_urn"]
            for e in graph["edges"]
        )


class TestErrors:
    def test_missing_stack_raises(self, tmp_project):
        with pytest.raises(ValueError):
            export_sql_lineage(tmp_project(PRODUCER), "")

    def test_invalid_project_raises(self):
        with pytest.raises(ValueError):
            export_sql_lineage("/nonexistent/path", "dev")

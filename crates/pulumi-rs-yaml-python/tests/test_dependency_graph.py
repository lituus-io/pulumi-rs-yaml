"""Tests for export_dependency_graph."""

import pytest

from pulumi_yaml_rs import export_dependency_graph

BASIC = """\
name: proj
runtime: yaml
resources:
  bucket:
    type: gcp:storage:Bucket
    properties:
      location: US
outputs:
  bucketId: ${bucket.id}
"""


class TestGraphStructure:
    def test_top_level_keys(self, tmp_project):
        project_dir = tmp_project(BASIC)
        graph = export_dependency_graph(project_dir, "dev", "org")
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
        assert graph["organization"] == "org"
        assert graph["project"] == "proj"
        assert graph["stack"] == "dev"

    def test_node_ids_are_urns(self, tmp_project):
        project_dir = tmp_project(BASIC)
        graph = export_dependency_graph(project_dir, "dev", "org")
        bucket = next(n for n in graph["nodes"] if n["logical_name"] == "bucket")
        assert bucket["id"] == "urn:pulumi:dev::proj::gcp:storage/bucket:Bucket::bucket"
        assert bucket["kind"] == "resource"
        assert bucket["literal_properties"] == {"location": "US"}

    def test_output_and_stack_nodes(self, tmp_project):
        project_dir = tmp_project(BASIC)
        graph = export_dependency_graph(project_dir, "dev", "org")
        kinds = {n["kind"] for n in graph["nodes"]}
        assert {"stack", "resource", "output"} <= kinds
        out = next(n for n in graph["nodes"] if n["kind"] == "output")
        assert out["id"] == "stackoutput::org/proj/dev::bucketId"

    def test_typed_edges(self, tmp_project):
        project_dir = tmp_project(
            """\
name: proj
runtime: yaml
resources:
  a:
    type: test:mod:A
  b:
    type: test:mod:B
    properties:
      ref: ${a.id}
    options:
      dependsOn:
        - ${a}
"""
        )
        graph = export_dependency_graph(project_dir, "dev", "org")
        rels = {
            (e["relationship"], tuple(e["property_paths"]))
            for e in graph["edges"]
            if "test:mod/b:B" in e["source_id"]
        }
        assert ("references", ("ref",)) in rels
        assert ("depends_on", ()) in rels


class TestStackParam:
    def test_stack_param_wins_over_context(self, tmp_project):
        project_dir = tmp_project(BASIC)
        graph = export_dependency_graph(
            project_dir, "prod", "org", {"stack_name": "ignored"}
        )
        assert graph["stack"] == "prod"

    def test_missing_stack_raises(self, tmp_project):
        project_dir = tmp_project(BASIC)
        with pytest.raises(ValueError):
            export_dependency_graph(project_dir, "")

    def test_invalid_project_raises(self):
        with pytest.raises(ValueError):
            export_dependency_graph("/nonexistent/path", "dev")


class TestCrossStack:
    def test_consumes_stack_output_id_contract(self, tmp_project):
        producer_dir = tmp_project(
            """\
name: producer
runtime: yaml
resources:
  ds:
    type: gcp:bigquery:Dataset
    properties:
      datasetId: analytics
outputs:
  datasetId: ${ds.datasetId}
"""
        )
        producer = export_dependency_graph(producer_dir, "prod", "org")
        output_id = next(
            n["id"] for n in producer["nodes"] if n["kind"] == "output"
        )
        # producer's project name comes from its own Pulumi.yaml
        assert output_id == "stackoutput::org/producer/prod::datasetId"

    def test_consumer_edge_targets_producer_output(self, tmp_project):
        consumer_dir = tmp_project(
            """\
name: consumer
runtime: yaml
resources:
  up:
    type: pulumi:pulumi:StackReference
    properties:
      name: org/producer/prod
  t:
    type: gcp:bigquery:Table
    properties:
      ds: ${up.outputs.datasetId}
"""
        )
        consumer = export_dependency_graph(consumer_dir, "dev", "org")
        targets = [
            e["target_id"]
            for e in consumer["edges"]
            if e["relationship"] == "consumes_stack_output"
        ]
        assert targets == ["stackoutput::org/producer/prod::datasetId"]


class TestComponents:
    def test_component_children(self, tmp_project):
        project_dir = tmp_project(
            """\
name: proj
runtime: yaml
components:
  Widget:
    inputs:
      size:
        type: string
    resources:
      inner:
        type: test:mod:Inner
resources:
  w:
    type: proj:index:Widget
    properties:
      size: large
"""
        )
        graph = export_dependency_graph(project_dir, "dev", "org")
        w = next(n for n in graph["nodes"] if n["logical_name"] == "w")
        child = next(n for n in graph["nodes"] if n["logical_name"] == "w.inner")
        assert w["kind"] == "component"
        assert child["kind"] == "component_child"
        assert child["component_id"] == w["id"]
        assert any(
            e["relationship"] == "contains"
            and e["source_id"] == w["id"]
            and e["target_id"] == child["id"]
            for e in graph["edges"]
        )

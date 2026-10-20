# Author: spicyzhug@gmail.com
# Copyright: lituus-io, all rights reserved

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


STR_INVOKE = """\
name: proj
runtime: yaml
config:
  env:
    type: string
    default: dev
variables:
  process_nm: geo_fence
  sanitized_process_nm:
    Fn::str:replace:
      string: ${process_nm}
      old: '_'
      new: '-'
  trimmed:
    fn::str:trimSuffix:
      string: ${sanitized_process_nm.result}
      suffix: '-fence'
  from_config:
    fn::str:replace:
      string: ${env}
      old: 'd'
      new: 'p'
resources:
  bucket:
    type: gcp:storage:Bucket
    properties:
      name: acme-bkt-${sanitized_process_nm.result}
      location: US
  dataset:
    type: gcp:bigquery:Dataset
    properties:
      datasetId: ${trimmed.result}
  dynamic:
    type: gcp:storage:Bucket
    properties:
      name: ${from_config.result}
"""


class TestStrInvokeDerivedLiterals:
    """Names built from `str` functions are literals in the exported graph."""

    def test_replace_derived_name(self, tmp_project):
        graph = export_dependency_graph(tmp_project(STR_INVOKE), "dev", "org")
        bucket = next(n for n in graph["nodes"] if n["logical_name"] == "bucket")
        assert bucket["literal_properties"]["name"] == "acme-bkt-geo-fence"
        # Plain literals on the same resource are unaffected.
        assert bucket["literal_properties"]["location"] == "US"

    def test_chained_invoke_derived_id(self, tmp_project):
        graph = export_dependency_graph(tmp_project(STR_INVOKE), "dev", "org")
        dataset = next(n for n in graph["nodes"] if n["logical_name"] == "dataset")
        assert dataset["literal_properties"]["datasetId"] == "geo"

    def test_unresolvable_name_is_still_absent(self, tmp_project):
        graph = export_dependency_graph(tmp_project(STR_INVOKE), "dev", "org")
        dynamic = next(n for n in graph["nodes"] if n["logical_name"] == "dynamic")
        assert "name" not in dynamic["literal_properties"]


class TestMarkedProject:
    """The observable the incident was reported through.

    A ``Pulumi.yaml`` behind a byte order mark loaded nameless, so every URN in
    the exported graph — and the ``project`` field itself — described a project
    called ``unknown``. The bytes are written raw so no formatter can remove the
    thing under test.
    """

    def test_project_name_survives_a_byte_order_mark(self, tmp_path):
        (tmp_path / "Pulumi.yaml").write_bytes(b"\xef\xbb\xbf" + BASIC.encode("utf-8"))
        assert (tmp_path / "Pulumi.yaml").read_bytes()[:3] == b"\xef\xbb\xbf"

        graph = export_dependency_graph(str(tmp_path), "dev", "org")
        assert graph["project"] == "proj"
        assert graph["diagnostics"] == []
        bucket = next(n for n in graph["nodes"] if n["logical_name"] == "bucket")
        assert bucket["id"] == "urn:pulumi:dev::proj::gcp:storage/bucket:Bucket::bucket"


class TestGilRelease:
    def test_a_graph_export_releases_the_gil(self, tmp_project):
        """Another thread keeps running while a large project exports -- the
        observable form of the GIL being released around the load and the
        export."""
        import threading
        import time

        resources = "".join(
            f"  r{i}:\n    type: gcp:storage:Bucket\n    properties:\n"
            f"      name: b-{i}\n      labels:\n        prev: ${{r{max(i - 1, 0)}.id}}\n"
            for i in range(1, 3000)
        )
        project_dir = tmp_project(
            "name: big\nruntime: yaml\nresources:\n  r0:\n    type: gcp:storage:Bucket\n"
            + resources
        )
        counter = 0
        stop = threading.Event()

        def spin():
            nonlocal counter
            while not stop.is_set():
                counter += 1

        worker = threading.Thread(target=spin, daemon=True)
        worker.start()
        try:
            time.sleep(0.05)
            before = counter
            started = time.perf_counter()
            graph = export_dependency_graph(project_dir, "dev", "org")
            elapsed = time.perf_counter() - started
            during = counter - before
            # The same length of time with the GIL certainly released.
            before = counter
            time.sleep(elapsed)
            baseline = counter - before
        finally:
            stop.set()
            worker.join(timeout=5)
        assert len(graph["nodes"]) > 3000
        assert elapsed > 0.01, "the export was too fast to prove anything"
        # The spinner also gets switch-interval slices in the Python code on
        # either side of the call, so `during > 0` proves nothing. Measured:
        # a GIL-holding export advances it ~8% of the baseline, a releasing
        # one ~63%.
        assert during > 0.3 * baseline, (
            f"the counter advanced {during} during the export against {baseline} "
            "while sleeping as long: the GIL was held"
        )

    def test_dict_keys_are_sorted_as_they_always_were(self, tmp_project):
        graph = export_dependency_graph(tmp_project(BASIC), "dev", "org")
        top = [k for k in graph if k != "diagnostics"]
        assert top == sorted(top)
        node = graph["nodes"][0]
        assert list(node) == sorted(node)


class TestBackendUris:
    WITH_BACKEND = """\
name: proj
runtime: yaml
backend:
  url: gs://from-project
resources:
  up:
    type: pulumi:pulumi:StackReference
    properties:
      name: acme/producer/prod
  bucket:
    type: gcp:storage:Bucket
"""

    def _node(self, graph, logical):
        return next(n for n in graph["nodes"] if n["logical_name"] == logical)

    def test_absent_without_a_backend(self, tmp_project):
        graph = export_dependency_graph(tmp_project(BASIC), "dev", "org")
        assert "stack_uri" not in graph
        assert all("resource_uri" not in n for n in graph["nodes"])

    def test_project_backend_is_the_fallback(self, tmp_project):
        graph = export_dependency_graph(tmp_project(self.WITH_BACKEND), "dev", "acme")
        assert graph["stack_uri"] == "gs://from-project/.pulumi/stacks/proj/dev.json"
        assert (
            self._node(graph, "up")["referenced_stack_uri"]
            == "gs://from-project/.pulumi/stacks/producer/prod.json"
        )
        assert self._node(graph, "bucket")["resource_uri"].startswith(
            "gs://from-project/.pulumi/stacks/proj/dev.json#urn:pulumi:dev::proj::"
        )

    def test_the_keyword_wins_and_empty_turns_it_off(self, tmp_project):
        d = tmp_project(self.WITH_BACKEND)
        cloud = export_dependency_graph(d, "dev", "acme", backend_url="https://api.pulumi.com")
        assert cloud["stack_uri"] == "https://app.pulumi.com/acme/proj/dev"
        off = export_dependency_graph(d, "dev", "acme", backend_url="")
        assert "stack_uri" not in off

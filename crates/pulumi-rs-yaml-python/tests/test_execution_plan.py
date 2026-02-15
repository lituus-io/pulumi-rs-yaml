"""Tests for create_execution_plan() — DAG-based execution planning."""

import pytest
from pulumi_yaml_rs import create_execution_plan


class TestPlanBasicStructure:
    def test_plan_basic_structure(self, tmp_project):
        d = tmp_project("""\
            name: plan-test
            runtime: yaml
            resources:
              bucket:
                type: gcp:storage:Bucket
                properties:
                  name: my-bucket
        """)
        plan = create_execution_plan(d)
        assert "project_name" in plan
        assert "nodes" in plan
        assert "outputs" in plan
        assert "source_map" in plan
        assert "diagnostics" in plan
        assert "levels" in plan

    def test_plan_project_name(self, tmp_project):
        d = tmp_project("""\
            name: my-awesome-project
            runtime: yaml
        """)
        plan = create_execution_plan(d)
        assert plan["project_name"] == "my-awesome-project"


class TestPlanNodes:
    def test_plan_config_nodes(self, tmp_project):
        d = tmp_project("""\
            name: cfg-plan
            runtime: yaml
            config:
              greeting:
                default: hello
              count:
                type: integer
        """)
        plan = create_execution_plan(d)
        config_nodes = [n for n in plan["nodes"] if n["kind"] == "config"]
        assert len(config_nodes) == 2

    def test_plan_variable_nodes(self, tmp_project):
        d = tmp_project("""\
            name: var-plan
            runtime: yaml
            variables:
              encoded:
                fn::toBase64: hello
        """)
        plan = create_execution_plan(d)
        var_nodes = [n for n in plan["nodes"] if n["kind"] == "variable"]
        assert len(var_nodes) == 1
        assert var_nodes[0]["name"] == "encoded"
        assert var_nodes[0]["value"] is not None

    def test_plan_resource_nodes(self, tmp_project):
        d = tmp_project("""\
            name: res-plan
            runtime: yaml
            resources:
              bucket:
                type: gcp:storage:Bucket
                properties:
                  name: my-bucket
        """)
        plan = create_execution_plan(d)
        res_nodes = [n for n in plan["nodes"] if n["kind"] == "resource"]
        assert len(res_nodes) == 1
        assert res_nodes[0]["name"] == "bucket"

    def test_plan_type_token_canonicalized(self, tmp_project):
        d = tmp_project("""\
            name: token-plan
            runtime: yaml
            resources:
              bucket:
                type: gcp:storage:Bucket
                properties:
                  name: my-bucket
        """)
        plan = create_execution_plan(d)
        res = [n for n in plan["nodes"] if n["kind"] == "resource"][0]
        # gcp:storage:Bucket → gcp:storage/bucket:Bucket
        assert res["type_token"] == "gcp:storage/bucket:Bucket"


class TestPlanResourceDetails:
    def test_plan_resource_properties(self, tmp_project):
        d = tmp_project("""\
            name: props-plan
            runtime: yaml
            resources:
              bucket:
                type: gcp:storage:Bucket
                properties:
                  name: my-bucket
                  location: US
        """)
        plan = create_execution_plan(d)
        res = [n for n in plan["nodes"] if n["kind"] == "resource"][0]
        props = res["properties"]
        # Properties are a list of {k, v} dicts
        assert isinstance(props, list)
        prop_keys = [p["k"] for p in props]
        assert "name" in prop_keys
        assert "location" in prop_keys

    def test_plan_resource_options(self, tmp_project):
        d = tmp_project("""\
            name: opts-plan
            runtime: yaml
            resources:
              bucketA:
                type: gcp:storage:Bucket
                properties:
                  name: bucket-a
              bucketB:
                type: gcp:storage:Bucket
                properties:
                  name: bucket-b
                options:
                  protect: true
                  dependsOn:
                    - ${bucketA}
        """)
        plan = create_execution_plan(d)
        res_b = [n for n in plan["nodes"] if n["name"] == "bucketB"][0]
        opts = res_b["options"]
        assert opts is not None
        assert opts.get("protect") is not None

    def test_plan_resource_get(self, tmp_project):
        d = tmp_project("""\
            name: get-plan
            runtime: yaml
            resources:
              existing:
                type: gcp:storage:Bucket
                get:
                  id: existing-bucket-id
        """)
        plan = create_execution_plan(d)
        res = [n for n in plan["nodes"] if n["kind"] == "resource"][0]
        assert res.get("get") is not None


class TestPlanOutputsAndTopology:
    def test_plan_outputs_serialized(self, tmp_project):
        d = tmp_project("""\
            name: out-plan
            runtime: yaml
            resources:
              bucket:
                type: gcp:storage:Bucket
                properties:
                  name: my-bucket
            outputs:
              bucketName: ${bucket.name}
              bucketUrl: ${bucket.url}
        """)
        plan = create_execution_plan(d)
        outputs = plan["outputs"]
        assert isinstance(outputs, list)
        assert len(outputs) == 2
        output_names = [o["name"] for o in outputs]
        assert "bucketName" in output_names
        assert "bucketUrl" in output_names

    def test_plan_topological_levels(self, tmp_project):
        d = tmp_project("""\
            name: topo-plan
            runtime: yaml
            config:
              greeting:
                default: hello
            resources:
              bucket:
                type: gcp:storage:Bucket
                properties:
                  name: my-bucket
        """)
        plan = create_execution_plan(d)
        levels = plan["levels"]
        assert isinstance(levels, list)
        assert len(levels) > 0
        # Each level is a list of node names
        for level in levels:
            assert isinstance(level, list)

    def test_plan_source_map_multi_file(self, tmp_project):
        d = tmp_project(
            """\
            name: multi-plan
            runtime: yaml
            """,
            extras={
                "Pulumi.storage.yaml": """\
resources:
  storageBucket:
    type: gcp:storage:Bucket
    properties:
      name: storage-bucket
"""
            },
        )
        plan = create_execution_plan(d)
        source_map = plan["source_map"]
        assert "storageBucket" in source_map
        assert "Pulumi.storage.yaml" in source_map["storageBucket"]


class TestPlanJinja:
    def test_plan_with_jinja_context(self, tmp_project, jinja_context):
        d = tmp_project("""\
            name: jinja-plan
            runtime: yaml
            resources:
              bucket:
                type: gcp:storage:Bucket
                properties:
                  name: "{{ pulumi_project }}-bucket"
                  location: US
        """)
        plan = create_execution_plan(d, jinja_context)
        assert plan["project_name"] == "jinja-plan"
        res_nodes = [n for n in plan["nodes"] if n["kind"] == "resource"]
        assert len(res_nodes) == 1


class TestPlanErrors:
    def test_plan_missing_dir_error(self):
        with pytest.raises(ValueError):
            create_execution_plan("/nonexistent/path/to/project")

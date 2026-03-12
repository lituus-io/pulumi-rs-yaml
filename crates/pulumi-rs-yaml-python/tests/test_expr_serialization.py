"""Tests for expression serialization (expr_to_py in convert.rs).

Exercises expression dict output via create_execution_plan() which returns
serialized AST expressions in node values, properties, and outputs.
"""

import pytest
from pulumi_yaml_rs import create_execution_plan


def get_plan(tmp_project, yaml_content, **kwargs):
    d = tmp_project(yaml_content)
    return create_execution_plan(d, **kwargs)


def get_variable_value(plan, name):
    for node in plan["nodes"]:
        if node["kind"] == "variable" and node["name"] == name:
            return node["value"]
    raise KeyError(f"Variable {name} not found")


def get_resource_property(plan, resource_name, prop_key):
    for node in plan["nodes"]:
        if node["kind"] == "resource" and node["name"] == resource_name:
            props = node["properties"]
            if isinstance(props, list):
                for p in props:
                    if p["k"] == prop_key:
                        return p["v"]
    raise KeyError(f"Property {prop_key} not found on {resource_name}")


def get_output_value(plan, name):
    for out in plan["outputs"]:
        if out["name"] == name:
            return out["value"]
    raise KeyError(f"Output {name} not found")


class TestExprLiterals:
    def test_expr_null(self, tmp_project):
        plan = get_plan(tmp_project, """\
            name: test
            runtime: yaml
            variables:
              v:
                fn::toJSON: null
        """)
        # The variable value is an expression representing fn::toJSON(null)
        val = get_variable_value(plan, "v")
        assert val["t"] == "toJSON"
        assert val["arg"]["t"] == "null"

    def test_expr_bool(self, tmp_project):
        plan = get_plan(tmp_project, """\
            name: test
            runtime: yaml
            resources:
              bucket:
                type: gcp:storage:Bucket
                properties:
                  forceDestroy: true
        """)
        val = get_resource_property(plan, "bucket", "forceDestroy")
        assert val["t"] == "bool"
        assert val["v"] is True

    def test_expr_number(self, tmp_project):
        plan = get_plan(tmp_project, """\
            name: test
            runtime: yaml
            variables:
              v:
                fn::abs: -42
        """)
        val = get_variable_value(plan, "v")
        assert val["t"] == "abs"
        assert val["arg"]["t"] == "number"
        assert val["arg"]["v"] == -42

    def test_expr_string(self, tmp_project):
        plan = get_plan(tmp_project, """\
            name: test
            runtime: yaml
            resources:
              bucket:
                type: gcp:storage:Bucket
                properties:
                  location: US
        """)
        val = get_resource_property(plan, "bucket", "location")
        assert val["t"] == "string"
        assert val["v"] == "US"


class TestExprComplex:
    def test_expr_symbol(self, tmp_project):
        plan = get_plan(tmp_project, """\
            name: test
            runtime: yaml
            resources:
              bucket:
                type: gcp:storage:Bucket
                properties:
                  name: my-bucket
            outputs:
              bucketName: ${bucket.name}
        """)
        val = get_output_value(plan, "bucketName")
        assert val["t"] == "sym"
        assert "a" in val  # accessor list

    def test_expr_interpolate(self, tmp_project):
        plan = get_plan(tmp_project, """\
            name: test
            runtime: yaml
            config:
              prefix:
                default: hello
            resources:
              bucket:
                type: gcp:storage:Bucket
                properties:
                  name: "prefix-${prefix}-suffix"
        """)
        # The name property should be an interpolation expression
        val = get_resource_property(plan, "bucket", "name")
        assert val["t"] == "interp"
        assert "parts" in val

    def test_expr_invoke(self, tmp_project):
        plan = get_plan(tmp_project, """\
            name: test
            runtime: yaml
            variables:
              info:
                fn::invoke:
                  function: gcp:organizations:getProject
                  arguments: {}
        """)
        val = get_variable_value(plan, "info")
        assert val["t"] == "invoke"
        assert "tok" in val

    def test_expr_builtin_fn(self, tmp_project):
        plan = get_plan(tmp_project, """\
            name: test
            runtime: yaml
            variables:
              encoded:
                fn::toBase64: hello
        """)
        val = get_variable_value(plan, "encoded")
        assert val["t"] == "toBase64"
        assert "arg" in val

    def test_expr_asset(self, tmp_project):
        plan = get_plan(tmp_project, """\
            name: test
            runtime: yaml
            resources:
              bucket:
                type: gcp:storage:Bucket
                properties:
                  content:
                    fn::stringAsset: "file content"
        """)
        val = get_resource_property(plan, "bucket", "content")
        assert val["t"] == "stringAsset"
        assert "arg" in val

    def test_expr_list_and_object(self, tmp_project):
        plan = get_plan(tmp_project, """\
            name: test
            runtime: yaml
            variables:
              joined:
                fn::join:
                  - ","
                  - - a
                    - b
        """)
        val = get_variable_value(plan, "joined")
        assert val["t"] == "join"

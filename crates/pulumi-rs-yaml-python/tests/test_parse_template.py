"""Tests for parse_template() — YAML template parsing and analysis."""

import pytest
from pulumi_yaml_rs import parse_template


class TestParseMinimal:
    def test_parse_minimal_template(self, simple_yaml):
        result = parse_template(simple_yaml)
        assert result["name"] == "test"
        assert result["resource_count"] == 0
        assert result["variable_count"] == 0
        assert result["output_count"] == 0
        assert result["has_errors"] is False

    def test_parse_name_and_description(self, multi_resource_yaml):
        result = parse_template(multi_resource_yaml)
        assert result["name"] == "multi-test"
        assert result["description"] == "Multi-resource test"

    def test_parse_no_name(self):
        result = parse_template("runtime: yaml\n")
        assert result["name"] is None


class TestParseCounts:
    def test_parse_resources_counted(self, multi_resource_yaml):
        result = parse_template(multi_resource_yaml)
        assert result["resource_count"] == 2
        assert sorted(result["resource_names"]) == ["bucketA", "bucketB"]

    def test_parse_variables_counted(self, multi_resource_yaml):
        result = parse_template(multi_resource_yaml)
        assert result["variable_count"] == 1
        assert result["variable_names"] == ["greeting"]

    def test_parse_outputs_counted(self, multi_resource_yaml):
        result = parse_template(multi_resource_yaml)
        assert result["output_count"] == 2
        assert sorted(result["output_names"]) == ["nameA", "nameB"]

    def test_parse_config_counted(self):
        yaml = """\
name: cfg-test
runtime: yaml
config:
  aws:region:
    value: us-east-1
  name:
    default: hello
  count:
    type: integer
"""
        result = parse_template(yaml)
        assert result["config_count"] == 3

    def test_parse_components_counted(self):
        yaml = """\
name: comp-test
runtime: yaml
components:
  MyComponent:
    type: my:component:Type
    properties:
      foo: bar
"""
        result = parse_template(yaml)
        assert result["component_count"] == 1


class TestParseDiagnostics:
    def test_parse_diagnostics_no_errors(self, simple_yaml):
        result = parse_template(simple_yaml)
        assert result["has_errors"] is False
        assert result["diagnostics"] == []

    def test_parse_invalid_yaml_syntax(self):
        result = parse_template("{{{")
        assert result["has_errors"] is True
        assert len(result["diagnostics"]) > 0

    def test_parse_non_mapping_toplevel(self):
        result = parse_template("- item1\n- item2\n")
        assert result["has_errors"] is True


class TestParseRealFixture:
    def test_parse_real_acceptance_fixture(self, acceptance_dir):
        content = (acceptance_dir / "gcp-bucket" / "Pulumi.yaml").read_text()
        result = parse_template(content)
        assert result["name"] == "gcp-bucket-test"
        assert result["resource_count"] == 1
        assert result["resource_names"] == ["testBucket"]
        assert result["has_errors"] is False

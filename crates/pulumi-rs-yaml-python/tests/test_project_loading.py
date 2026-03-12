"""Tests for load_project() and discover_project_files()."""

import pytest
from pulumi_yaml_rs import discover_project_files, load_project


class TestDiscoverProjectFiles:
    def test_discover_single_file_project(self, tmp_project):
        d = tmp_project("name: test\nruntime: yaml\n")
        result = discover_project_files(d)
        assert result["file_count"] == 1
        assert "Pulumi.yaml" in result["main_file"]
        assert result["additional_files"] == []

    def test_discover_multi_file_project(self, tmp_project):
        d = tmp_project(
            "name: test\nruntime: yaml\n",
            extras={"Pulumi.storage.yaml": "resources:\n  b:\n    type: a:b:C\n"},
        )
        result = discover_project_files(d)
        assert result["file_count"] == 2
        additional = result["additional_files"]
        assert len(additional) == 1
        assert "Pulumi.storage.yaml" in additional[0]

    def test_discover_no_pulumi_yaml_error(self, tmp_path):
        with pytest.raises(ValueError):
            discover_project_files(str(tmp_path))

    def test_discover_real_multi_file(self, acceptance_dir):
        d = str(acceptance_dir / "gcp-multi-file")
        result = discover_project_files(d)
        assert result["file_count"] >= 2
        assert len(result["additional_files"]) >= 1


class TestLoadProject:
    def test_load_single_file_project(self, tmp_project):
        d = tmp_project("""\
            name: test
            runtime: yaml
            resources:
              bucket:
                type: gcp:storage:Bucket
                properties:
                  name: my-bucket
            variables:
              v1:
                fn::toBase64: hello
        """)
        result = load_project(d)
        assert result["resource_count"] == 1
        assert result["variable_count"] == 1
        assert result["has_errors"] is False

    def test_load_multi_file_project(self, tmp_project):
        d = tmp_project(
            """\
            name: multi
            runtime: yaml
            """,
            extras={
                "Pulumi.storage.yaml": """\
resources:
  bucketA:
    type: gcp:storage:Bucket
    properties:
      name: bucket-a
"""
            },
        )
        result = load_project(d)
        assert result["resource_count"] == 1
        assert "bucketA" in result["resource_names"]
        assert result["file_count"] >= 2

    def test_load_source_map_tracks_origin(self, tmp_project):
        d = tmp_project(
            """\
            name: multi
            runtime: yaml
            resources:
              mainRes:
                type: a:b:C
                properties:
                  name: main
            """,
            extras={
                "Pulumi.storage.yaml": """\
resources:
  storageRes:
    type: a:b:C
    properties:
      name: storage
"""
            },
        )
        result = load_project(d)
        source_map = result["source_map"]
        assert "storageRes" in source_map
        assert "Pulumi.storage.yaml" in source_map["storageRes"]

    def test_load_missing_directory_error(self):
        with pytest.raises(ValueError):
            load_project("/nonexistent/path/to/project")

    def test_load_has_errors_for_invalid_yaml(self, tmp_project):
        d = tmp_project("{{{invalid yaml")
        result = load_project(d)
        assert result["has_errors"] is True

    def test_load_real_gcp_bucket(self, acceptance_dir):
        d = str(acceptance_dir / "gcp-bucket")
        result = load_project(d)
        assert result["resource_count"] == 1
        assert "testBucket" in result["resource_names"]
        assert result["has_errors"] is False

    def test_load_real_multi_file_jinja(self, acceptance_dir):
        d = str(acceptance_dir / "gcp-multi-file")
        result = load_project(d)
        assert result["file_count"] >= 2
        assert result["resource_count"] >= 2

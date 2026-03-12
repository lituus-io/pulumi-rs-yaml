"""Integration tests using real acceptance test fixtures."""

import os

import pytest
from pulumi_yaml_rs import create_execution_plan, load_project


def all_acceptance_dirs(acceptance_dir):
    """Yield all subdirectories in the acceptance test fixtures."""
    for entry in sorted(acceptance_dir.iterdir()):
        if entry.is_dir() and (entry / "Pulumi.yaml").exists():
            yield entry


class TestAllAcceptanceProjects:
    def test_all_acceptance_projects_parse(self, acceptance_dir):
        dirs = list(all_acceptance_dirs(acceptance_dir))
        assert len(dirs) >= 15, f"Expected ≥15 acceptance dirs, got {len(dirs)}"
        for d in dirs:
            result = load_project(str(d))
            assert "has_errors" in result, f"Missing has_errors for {d.name}"

    def test_all_acceptance_projects_plan(self, acceptance_dir):
        dirs = list(all_acceptance_dirs(acceptance_dir))
        # Skip Jinja projects that need context and multi-phase projects
        # whose Pulumi.phase*.yaml files are standalone projects (not multi-file extras)
        jinja_dirs = {"gcp-jinja-bucket", "gcp-jinja-single-line", "gcp-multi-file-jinja", "gcp-exec-jinja-bucket", "gcp-get-resource"}
        for d in dirs:
            if d.name in jinja_dirs:
                continue
            plan = create_execution_plan(str(d))
            assert "nodes" in plan, f"Missing nodes for {d.name}"
            assert "levels" in plan, f"Missing levels for {d.name}"


class TestSpecificFixtures:
    def test_gcp_builtins_plan_has_resources(self, acceptance_dir):
        d = str(acceptance_dir / "gcp-builtins")
        plan = create_execution_plan(d)
        resource_names = [
            n["name"] for n in plan["nodes"] if n["kind"] == "resource"
        ]
        assert "builtinsBucket" in resource_names
        assert "labelBucket" in resource_names

    def test_multi_file_plan_source_map(self, acceptance_dir):
        d = str(acceptance_dir / "gcp-multi-file")
        plan = create_execution_plan(d)
        source_map = plan["source_map"]
        # Resources from different files should have different source entries
        assert len(source_map) >= 2
        sources = set(source_map.values())
        assert len(sources) >= 2, "Expected resources from multiple files"

    def test_jinja_project_with_context(self, acceptance_dir):
        d = str(acceptance_dir / "gcp-jinja-bucket")
        context = {
            "project_name": "gcp-jinja-bucket-test",
            "stack_name": "dev",
        }
        plan = create_execution_plan(d, context)
        assert plan["project_name"] == "gcp-jinja-bucket-test"
        resource_names = [
            n["name"] for n in plan["nodes"] if n["kind"] == "resource"
        ]
        assert "jinjaBucket0" in resource_names
        assert "jinjaBucket1" in resource_names

    def test_new_builtins_plan(self, acceptance_dir):
        d = str(acceptance_dir / "new-builtins")
        plan = create_execution_plan(d)
        var_nodes = {
            n["name"]: n for n in plan["nodes"] if n["kind"] == "variable"
        }
        # Should have math builtin variables
        assert "absResult" in var_nodes
        assert "floorResult" in var_nodes
        assert "ceilResult" in var_nodes
        # The variable values should be expression dicts with builtin types
        assert var_nodes["absResult"]["value"]["t"] == "abs"
        assert var_nodes["floorResult"]["value"]["t"] == "floor"
        assert var_nodes["ceilResult"]["value"]["t"] == "ceil"

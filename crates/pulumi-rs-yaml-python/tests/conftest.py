"""Shared fixtures for pulumi_yaml_rs test suite."""

import os
import textwrap
from pathlib import Path

import pytest


@pytest.fixture
def acceptance_dir():
    """Path to the acceptance test fixtures (17 real project directories)."""
    p = Path(__file__).resolve().parent.parent.parent.parent / "tests" / "acceptance"
    assert p.is_dir(), f"acceptance dir not found: {p}"
    return p


@pytest.fixture
def tmp_project(tmp_path):
    """Factory that creates temp directories with Pulumi.yaml content.

    Usage:
        project_dir = tmp_project("name: test\\nruntime: yaml")
        project_dir = tmp_project(main="...", extras={"Pulumi.storage.yaml": "..."})
    """

    def _create(main: str, extras: dict[str, str] | None = None):
        (tmp_path / "Pulumi.yaml").write_text(textwrap.dedent(main))
        if extras:
            for name, content in extras.items():
                (tmp_path / name).write_text(textwrap.dedent(content))
        return str(tmp_path)

    return _create


@pytest.fixture
def jinja_context():
    """Default JinjaContext dict for Jinja tests."""
    return {
        "project_name": "test-project",
        "stack_name": "dev",
    }


@pytest.fixture
def simple_yaml():
    """Minimal valid YAML template string."""
    return textwrap.dedent("""\
        name: test
        runtime: yaml
    """)


@pytest.fixture
def multi_resource_yaml():
    """YAML with 2 resources, a variable, and outputs."""
    return textwrap.dedent("""\
        name: multi-test
        runtime: yaml
        description: Multi-resource test
        variables:
          greeting:
            fn::toBase64: hello
        resources:
          bucketA:
            type: gcp:storage:Bucket
            properties:
              name: bucket-a
              location: US
          bucketB:
            type: gcp:storage:Bucket
            properties:
              name: bucket-b
              location: US
        outputs:
          nameA: ${bucketA.name}
          nameB: ${bucketB.name}
    """)


@pytest.fixture
def jinja_yaml():
    """YAML with {{ }} expressions (no block-level Jinja)."""
    return textwrap.dedent("""\
        name: jinja-expr-test
        runtime: yaml
        resources:
          bucket:
            type: gcp:storage:Bucket
            properties:
              name: "{{ pulumi_project }}-{{ pulumi_stack }}-bucket"
              location: US
    """)


@pytest.fixture
def jinja_block_yaml():
    """YAML with {% %} blocks."""
    return textwrap.dedent("""\
        name: jinja-block-test
        runtime: yaml
        resources:
        {% for i in range(2) %}
          bucket{{ i }}:
            type: gcp:storage:Bucket
            properties:
              name: "bucket-{{ i }}"
              location: US
        {% endfor %}
    """)

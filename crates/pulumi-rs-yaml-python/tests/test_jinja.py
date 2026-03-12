"""Tests for Jinja functions: has_jinja_blocks, strip_jinja_blocks, validate_jinja, preprocess_jinja."""

import pytest
from pulumi_yaml_rs import (
    has_jinja_blocks,
    preprocess_jinja,
    strip_jinja_blocks,
    validate_jinja,
)


class TestHasJinjaBlocks:
    def test_has_jinja_blocks_true(self):
        source = """\
resources:
{% for i in range(3) %}
  bucket{{ i }}:
    type: gcp:storage:Bucket
{% endfor %}
"""
        assert has_jinja_blocks(source) is True

    def test_has_jinja_blocks_false_expression_only(self):
        source = """\
resources:
  bucket:
    type: gcp:storage:Bucket
    properties:
      name: "{{ pulumi_project }}-bucket"
"""
        assert has_jinja_blocks(source) is False

    def test_has_jinja_blocks_false_plain(self):
        assert has_jinja_blocks("name: test\nruntime: yaml\n") is False


class TestStripJinjaBlocks:
    def test_strip_removes_block_lines(self):
        source = """\
name: test
{% for i in range(2) %}
  bucket{{ i }}:
    type: gcp:storage:Bucket
{% endfor %}
"""
        stripped = strip_jinja_blocks(source)
        assert "{% for" not in stripped
        assert "{% endfor" not in stripped
        assert "name: test" in stripped

    def test_strip_preserves_expression_lines(self):
        source = """\
name: "{{ pulumi_project }}"
runtime: yaml
"""
        stripped = strip_jinja_blocks(source)
        assert "{{ pulumi_project }}" in stripped

    def test_strip_plain_yaml_unchanged(self, simple_yaml):
        assert strip_jinja_blocks(simple_yaml) == simple_yaml


class TestValidateJinja:
    def test_validate_valid_syntax(self, jinja_block_yaml):
        # Should not raise
        validate_jinja(jinja_block_yaml, "test.yaml")

    def test_validate_unclosed_block_error(self):
        source = "{% for x in items %}\nhello\n"
        with pytest.raises(ValueError):
            validate_jinja(source, "test.yaml")

    def test_validate_plain_yaml_passes(self, simple_yaml):
        # No Jinja syntax at all — should pass
        validate_jinja(simple_yaml, "test.yaml")


class TestPreprocessJinja:
    def test_preprocess_substitutes_variables(self, jinja_context):
        source = 'name: "{{ pulumi_project }}"\nruntime: yaml\n'
        result = preprocess_jinja(source, "test.yaml", jinja_context)
        assert "test-project" in result

    def test_preprocess_config_variables(self):
        source = 'env: "{{ config.env }}"\n'
        context = {
            "project_name": "test",
            "stack_name": "dev",
            "config.env": "prod",
        }
        result = preprocess_jinja(source, "test.yaml", context)
        assert "prod" in result

    def test_preprocess_loop_expansion(self, jinja_context):
        source = """\
resources:
{% for i in range(2) %}
  bucket{{ i }}:
    type: gcp:storage:Bucket
{% endfor %}
"""
        result = preprocess_jinja(source, "test.yaml", jinja_context)
        assert "bucket0" in result
        assert "bucket1" in result
        assert "{% for" not in result

    def test_preprocess_missing_context_key(self):
        source = 'name: "{{ unknown_var }}"\n'
        context = {"project_name": "test", "stack_name": "dev"}
        with pytest.raises(ValueError):
            preprocess_jinja(source, "test.yaml", context)

    def test_preprocess_real_jinja_fixture(self, acceptance_dir):
        fixture = acceptance_dir / "gcp-jinja-bucket" / "Pulumi.yaml"
        source = fixture.read_text()
        context = {"project_name": "gcp-jinja-bucket-test", "stack_name": "dev"}
        result = preprocess_jinja(source, "Pulumi.yaml", context)
        # Jinja expressions should be rendered
        assert "{{ pulumi_project }}" not in result
        assert "{{ pulumi_stack }}" not in result
        # The rendered values should appear
        assert "gcp-jinja-bucket-test" in result
        assert "dev" in result

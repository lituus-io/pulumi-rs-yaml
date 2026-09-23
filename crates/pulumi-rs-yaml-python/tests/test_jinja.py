# Copyright (c) 2024-2026 Lituus-io. All rights reserved.

"""Tests for Jinja functions: has_jinja_blocks, strip_jinja_blocks, validate_jinja, preprocess_jinja."""

import pytest
from pulumi_yaml_rs import (
    has_jinja_blocks,
    preprocess_jinja,
    preprocess_jinja_diag,
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

    def test_has_jinja_blocks_true_expression_only(self):
        # Expression-only templates ({{ }} with no {% %} blocks) are still
        # Jinja and MUST be detected — otherwise a consumer's render-before-
        # gate skips them and raw Jinja reaches the Pulumi YAML parser.
        source = """\
resources:
  bucket:
    type: gcp:storage:Bucket
    properties:
      name: "{{ pulumi_project }}-bucket"
"""
        assert has_jinja_blocks(source) is True

    def test_has_jinja_blocks_true_comment_only(self):
        assert has_jinja_blocks("{# note #}\nname: test\n") is True

    def test_has_jinja_blocks_false_plain(self):
        assert has_jinja_blocks("name: test\nruntime: yaml\n") is False

    def test_has_jinja_blocks_false_pulumi_interpolation(self):
        # Pulumi's own ${...} interpolation is not Jinja.
        assert has_jinja_blocks("name: ${project}-bucket\n") is False


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


class TestPreprocessJinjaDiag:
    """The structured surface: the same render, its fault handed over as
    fields rather than folded into a sentence."""

    CTX = {"project_name": "test", "stack_name": "dev"}

    def test_a_clean_render_answers_with_the_text(self):
        out = preprocess_jinja_diag('name: "{{ pulumi_project }}"\n', "t.yaml", self.CTX)
        assert set(out) == {"rendered"}
        assert "test" in out["rendered"]

    def test_an_undefined_name_is_located_to_the_character(self):
        source = "a: 1\nname: {{ unknown_var }}\n"
        out = preprocess_jinja_diag(source, "t.yaml", self.CTX)
        assert set(out) == {"diagnostic"}
        d = out["diagnostic"]
        assert d["kind"] == "jinja_undefined_variable"
        assert d["line"] == 2
        assert d["column"] == 10
        assert d["end_column"] == 21
        assert d["source_line"] == "name: {{ unknown_var }}"
        assert d["expression"] == "unknown_var"
        assert d["message"] == "undefined value"
        assert "(in " not in d["message"], "the location is a field, not a suffix"
        assert d["suggestion"]

    def test_the_expression_sits_at_its_column(self):
        source = "x: {{ a.b[c] }}\n"
        d = preprocess_jinja_diag(source, "t.yaml", self.CTX)["diagnostic"]
        col = d["column"] - 1
        assert d["source_line"][col : col + len(d["expression"])] == d["expression"]

    def test_a_refused_include_says_why(self, tmp_path):
        (tmp_path / "logo.png").write_bytes(b"\x89PNG\xff\xfe")
        ctx = {**self.CTX, "project_dir": str(tmp_path), "root_directory": str(tmp_path)}
        d = preprocess_jinja_diag(
            "a: '{% include \"logo.png\" %}'\n", "t.yaml", ctx)["diagnostic"]
        assert d["kind"] == "jinja_template_not_found"
        assert d["message"] == 'include refused [binary]: "logo.png"'
        assert "PNG" not in d["suggestion"]
        assert d["suggestion"]

    def test_an_extensionless_text_file_is_served(self, tmp_path):
        (tmp_path / "VERSION").write_text("1.2.3\n")
        ctx = {**self.CTX, "project_dir": str(tmp_path), "root_directory": str(tmp_path)}
        out = preprocess_jinja_diag(
            "{% set v %}{% include 'VERSION' %}{% endset -%}\ntag: {{ v | trim }}\n",
            "t.yaml", ctx)
        assert out["rendered"] == "tag: 1.2.3"

    def test_an_oversized_include_is_refused_by_name(self, tmp_path):
        (tmp_path / "big.txt").write_bytes(b"x" * (1024 * 1024 + 1))
        ctx = {**self.CTX, "project_dir": str(tmp_path), "root_directory": str(tmp_path)}
        d = preprocess_jinja_diag(
            "a: '{% include \"big.txt\" %}'\n", "t.yaml", ctx)["diagnostic"]
        assert d["message"] == 'include refused [too large]: "big.txt"'
        assert "xxxx" not in d["suggestion"]

    def test_a_render_releases_the_gil(self):
        """Another thread keeps running while a render is in progress —
        the observable form of the GIL being released around it."""
        import threading
        import time

        counter = 0
        stop = threading.Event()

        def spin():
            nonlocal counter
            while not stop.is_set():
                counter += 1

        # A render big enough to dominate the call.
        # `range` is capped per call, so the work is two nested loops.
        source = ("{% for i in range(1000) %}{% for j in range(200) %}"
                  "k{{ i }}_{{ j }}: {{ i * j }}\n{% endfor %}{% endfor %}")
        worker = threading.Thread(target=spin, daemon=True)
        worker.start()
        try:
            time.sleep(0.05)
            before = counter
            started = time.perf_counter()
            out = preprocess_jinja_diag(source, "t.yaml", self.CTX)
            elapsed = time.perf_counter() - started
            during = counter - before
        finally:
            stop.set()
            worker.join(timeout=5)
        assert "rendered" in out
        assert elapsed > 0.01, "the render was too fast to prove anything"
        assert during > 0, "the counter did not advance while the template rendered: the GIL was held"

    def test_a_json_include_now_renders(self, tmp_path):
        (tmp_path / "schemas").mkdir()
        (tmp_path / "schemas" / "table.json").write_text('[{"name": "id"}]')
        ctx = {**self.CTX, "project_dir": str(tmp_path), "root_directory": str(tmp_path)}
        out = preprocess_jinja_diag(
            "schema: '{% include \"schemas/table.json\" %}'\n", "t.yaml", ctx)
        # The engine drops a template's trailing newline, as it always has.
        assert out["rendered"] == "schema: '[{\"name\": \"id\"}]'"

    def test_an_absent_include_is_not_found_by_name(self, tmp_path):
        ctx = {**self.CTX, "project_dir": str(tmp_path), "root_directory": str(tmp_path)}
        d = preprocess_jinja_diag(
            "a: '{% include \"schemas/missing.json\" %}'\n", "t.yaml", ctx)["diagnostic"]
        assert d["kind"] == "jinja_template_not_found"
        assert "schemas/missing.json" in d["message"]
        assert "include refused" not in d["message"]

    def test_the_string_surface_carries_the_same_facts(self):
        source = "name: {{ unknown_var }}\n"
        with pytest.raises(ValueError) as exc:
            preprocess_jinja(source, "t.yaml", self.CTX)
        text = str(exc.value)
        assert text.startswith("Jinja preprocessing error: t.yaml:1:10: error: undefined value")
        assert "\n  1 | name: {{ unknown_var }}" in text
        assert "^^^^^^^^^^^" in text, "a caret row under the expression"
        assert "(in " not in text

    def test_a_bad_context_is_this_calls_error_not_a_diagnostic(self):
        with pytest.raises(TypeError):
            preprocess_jinja_diag("a: 1\n", "t.yaml", {"k": object()})


class TestStringFilters:
    """`truncate`, `center` and `wordwrap` through the Python binding.

    The binding is a surface of its own: a filter registered in the Rust
    environment but unreachable from here would pass every Rust test and still
    fail the callers that render through this module. Expected values are the
    reference implementation's, and the Rust side holds the full 2,992-case
    differential corpus.
    """

    def _render(self, expr, jinja_context):
        src = f"name: t\nruntime: yaml\ndescription: <{{{{ {expr} }}}}>\n"
        out = preprocess_jinja(src, "Pulumi.yaml", jinja_context)
        line = [ln for ln in out.splitlines() if ln.startswith("description:")][0]
        # Strip exactly one marker at each end, so a value containing one is not
        # eaten along with it.
        body = line.split("description: ", 1)[1].strip()
        assert body.startswith("<") and body.endswith(">")
        return body[1:-1]

    @pytest.mark.parametrize(("expr", "expected"), [
        # The field expression's cap: no end string, no leeway, a hard 60.
        ("'ran-agnostic-kpi-table' | truncate(60, False, '', 0)", "ran-agnostic-kpi-table"),
        ("'abcdefghijklmnop' | truncate(6, False, '', 0)", "abcdef"),
        # `end` counts inside `length`, and the partial word is dropped.
        ("'hello world how are you' | truncate(11, False, '...', 0)", "hello..."),
        ("'hello world how are you' | truncate(11, True, '...', 0)", "hello wo..."),
        # leeway defaults to 5, so an 11-character string survives truncate(10).
        ("'abcdefghijk' | truncate(10)", "abcdefghijk"),
    ])
    def test_truncate(self, expr, expected, jinja_context):
        assert self._render(expr, jinja_context) == expected

    @pytest.mark.parametrize(("expr", "expected"), [
        # Asymmetric, and not in the direction one would guess.
        ("'x' | center(4)", " x  "),
        ("'ab' | center(5)", "  ab "),
        ("'abc' | center(3)", "abc"),
    ])
    def test_center(self, expr, expected, jinja_context):
        assert self._render(expr, jinja_context) == expected

    @pytest.mark.parametrize(("expr", "expected"), [
        ("'aaa bbb ccc' | wordwrap(7) | replace('\\n', '|')", "aaa bbb|ccc"),
        # A hyphen between two letters is a break opportunity ...
        ("'well-known thing' | wordwrap(6) | replace('\\n', '|')", "well-|known|thing"),
        # ... unless the caller turns that off.
        ("'well-known thing' | wordwrap(6, True, None, False) | replace('\\n', '|')",
         "well-k|nown|thing"),
    ])
    def test_wordwrap(self, expr, expected, jinja_context):
        assert self._render(expr, jinja_context) == expected

    def test_a_width_whose_padding_is_absurd_is_refused_not_fatal(self, jinja_context):
        """`center`'s width is author-controlled and it allocates from it."""
        with pytest.raises(ValueError) as excinfo:
            self._render("'x' | center(999999999999999999)", jinja_context)
        message = str(excinfo.value)
        # Asserted on the BOUND, not merely on the filter's name: an engine that
        # does not carry `center` at all raises "unknown filter: center", which
        # also contains it -- so a looser assertion passed on a version where the
        # bound does not exist.
        assert "would pad" in message and "1048576" in message, message

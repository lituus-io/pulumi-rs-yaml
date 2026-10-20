# Author: spicyzhug@gmail.com
# Copyright: lituus-io, all rights reserved

"""Every Jinja fix the changelog records, held at the Python surface.

The Rust side keeps the same matrix in
``crates/pulumi-rs-yaml-core/tests/jinja_changelog_regression.rs``. This file
holds the composition a Python caller meets: a stack on disk, rendered and
exported through ``export_dependency_graph`` -- the project pre-pass, the
loader, the parse and the export -- with each rendered value read back from the
node's ``literal_properties``.
"""

import pytest

from pulumi_yaml_rs import (
    export_dependency_graph,
    has_jinja_blocks,
    preprocess_jinja,
    preprocess_jinja_diag,
)


def one_value(value_yaml: str) -> str:
    return (
        "name: regress\nruntime: yaml\nresources:\n  r:\n"
        "    type: test:index:R\n    properties:\n"
        f"      value: {value_yaml}\n"
    )


def rendered_value(project_dir: str, context: dict | None = None) -> str:
    graph = export_dependency_graph(
        project_dir, "dev", "org", jinja_context=context or {}
    )
    node = next(n for n in graph["nodes"] if n["logical_name"] == "r")
    return node["literal_properties"]["value"]


def write(root, rel: str, content) -> None:
    path = root / rel
    path.parent.mkdir(parents=True, exist_ok=True)
    if isinstance(content, bytes):
        path.write_bytes(content)
    else:
        path.write_text(content)


class TestExtraVars_0_4_0:
    def test_extras_render(self, tmp_path):
        write(tmp_path, "Pulumi.yaml", one_value('"{{ team }}"'))
        assert rendered_value(str(tmp_path), {"team": "data-eng"}) == "data-eng"

    def test_extras_never_override_built_ins(self, tmp_path):
        write(tmp_path, "Pulumi.yaml", one_value('"{{ pulumi_stack }}"'))
        assert rendered_value(str(tmp_path), {"pulumi_stack": "hijacked"}) == "dev"


class TestImportInclude:
    def test_import_exposes_set_names(self, tmp_path):
        write(tmp_path, "environment.j2", "{% set region = 'europe-west1' %}")
        stack = tmp_path / "stacks" / "a"
        write(
            stack,
            "Pulumi.yaml",
            "{% import '../../environment.j2' as env %}\n"
            + one_value('"{{ env.region }}"'),
        )
        ctx = {"root_directory": str(tmp_path)}
        assert rendered_value(str(stack), ctx) == "europe-west1"


class TestDetection:
    def test_expression_and_comment_only(self):
        assert has_jinja_blocks("name: {{ pulumi_project }}\n")
        assert has_jinja_blocks("{# note #}\nname: x\n")
        assert not has_jinja_blocks("v: ${a.b}\n")


class TestCompat_0_5_19:
    def test_dict_methods(self, tmp_path):
        write(
            tmp_path,
            "Pulumi.yaml",
            "{% set m = {'a': '1', 'b': '2'} %}\n"
            + one_value('"{% for k, v in m.items() %}{{ k }}={{ v }};{% endfor %}"'),
        )
        assert rendered_value(str(tmp_path)) == "a=1;b=2;"

    def test_indent_keywords(self):
        out = preprocess_jinja(
            "{{ 'x\ny' | indent(width=2, first=True) }}", "t.yaml", {}
        )
        assert out == "  x\n  y"


class TestTojson_0_5_20:
    def test_tojson_matches_to_json(self):
        a = preprocess_jinja("{{ {'k': [1, 'two']} | tojson }}", "t.yaml", {})
        b = preprocess_jinja("{{ {'k': [1, 'two']} | to_json }}", "t.yaml", {})
        assert a == b


class TestProviderScopeAndReadFile_0_5_23:
    def test_readfile_keeps_crlf(self, tmp_path):
        write(tmp_path, "q.sql", b"SELECT 1\r\nFROM t\r\n")
        out = preprocess_jinja(
            "v: |\n  {{ readFile('q.sql') }}\n",
            "t.yaml",
            {"project_dir": str(tmp_path)},
        )
        assert "SELECT 1\r\n" in out


class TestBom_0_5_27:
    def test_marked_stack_keeps_its_name(self, tmp_path):
        write(
            tmp_path,
            "Pulumi.yaml",
            b"\xef\xbb\xbf" + one_value('"{{ pulumi_stack }}"').encode(),
        )
        graph = export_dependency_graph(str(tmp_path), "dev", "org", jinja_context={})
        assert graph["project"] == "regress"


class TestDiagnostics_0_5_29:
    def test_undefined_is_located(self):
        d = preprocess_jinja_diag("name: x\nvalue: {{ nope }}\n", "t.yaml", {})
        diag = d["diagnostic"]
        assert diag["line"] == 2
        assert diag["column"] > 0
        assert "nope" in diag["expression"]


class TestIncludes_0_5_30:
    def test_version_file_served_from_root(self, tmp_path):
        write(tmp_path, "VERSION", "1.4.2\n")
        stack = tmp_path / "svc" / "stack"
        write(stack, "Pulumi.yaml", one_value("\"img:{% include '../../VERSION' %}\""))
        ctx = {"root_directory": str(tmp_path)}
        assert rendered_value(str(stack), ctx) == "img:1.4.2"

    def test_binary_include_refused(self, tmp_path):
        write(tmp_path, "blob.bin", b"\xff\xfe\x00A")
        write(tmp_path, "Pulumi.yaml", one_value("\"{% include 'blob.bin' %}\""))
        with pytest.raises(ValueError, match=r"\[binary\]"):
            rendered_value(str(tmp_path))


class TestEscapedDollar_0_5_31:
    def test_escape_collapses_once_after_render(self, tmp_path):
        write(
            tmp_path,
            "Pulumi.yaml",
            one_value('"SELECT * FROM $${data()} -- {{ pulumi_stack }}"'),
        )
        assert rendered_value(str(tmp_path)) == "SELECT * FROM ${data()} -- dev"


class TestStringFilters_0_5_32:
    def test_truncate_field_expression(self, tmp_path):
        write(
            tmp_path,
            "Pulumi.yaml",
            one_value(
                "\"{{ 'a-very-long-resource-identifier-for-a-service' "
                "| truncate(20, False, '', 0) }}\""
            ),
        )
        assert rendered_value(str(tmp_path)) == "a-very-long-resource"

    def test_enormous_center_is_an_error(self):
        with pytest.raises(ValueError):
            preprocess_jinja("{{ 'x' | center(1000000000000000000) }}", "t.yaml", {})


class TestReadFileMarker_0_5_33:
    def test_marker_shaped_author_text_survives(self):
        out = preprocess_jinja('a: "{{ \'x\' }}"\n\x00RF:\x0b\x00\nb: 1\n', "t.yaml", {})
        assert "\x00RF:\x0b\x00" in out

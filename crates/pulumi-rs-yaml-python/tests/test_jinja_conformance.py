# Copyright (c) 2024-2026 Lituus-io. All rights reserved.

"""The Jinja conformance corpus, rendered through the Python binding.

The Rust runner holds the engine to reference Jinja2; this holds the binding
to the engine: every case renders through ``preprocess_jinja_diag`` to the
same text, or the same class of error, as the Rust runner pins -- the golden
where the engine agrees with Jinja2, the case's pinned ``rsyaml*`` file
where DIVERGENCES.md lists it. Then the binding's own surface: ``limits=``,
the error kinds of the limits, and ``origin`` for faults in included files.
"""

import json
import pathlib

import pytest
from pulumi_yaml_rs import preprocess_jinja, preprocess_jinja_diag

CORPUS = pathlib.Path(__file__).resolve().parents[3] / "tests" / "jinja_conformance" / "cases"
CASES = sorted(p for p in CORPUS.iterdir() if p.is_dir())

CLASS = {
    "jinja_syntax": "syntax",
    "jinja_undefined_variable": "undefined",
    "jinja_template_not_found": "not_found",
}


def expected(case: pathlib.Path):
    for text, error in (("rsyaml.txt", "rsyaml_error.txt"), ("expected.txt", "expected_error.txt")):
        if (case / text).exists():
            return ("text", (case / text).read_bytes().decode("utf-8"))
        if (case / error).exists():
            return ("error", (case / error).read_text().strip())
    raise AssertionError(f"{case.name}: no golden")


def context(case: pathlib.Path) -> dict:
    ctx = json.loads((case / "context.json").read_text())
    ctx.update(
        {
            "project_name": "proj",
            "stack_name": "dev",
            "organization": "org",
            "cwd": str(case),
            "root_directory": str(case),
            "project_dir": str(case),
            "config.region": "north",
        }
    )
    return ctx


def test_the_corpus_is_present():
    assert len(CASES) >= 250


@pytest.mark.parametrize("case", CASES, ids=[c.name for c in CASES])
def test_case_renders_as_the_engine_pins(case):
    source = (case / "template.j2").read_bytes().decode("utf-8")
    out = preprocess_jinja_diag(source, "template.j2", context(case))
    kind, want = expected(case)
    if kind == "text":
        assert out.get("rendered") == want, out
    else:
        assert "diagnostic" in out, out
        got = CLASS.get(out["diagnostic"]["kind"], "runtime")
        assert got == want, out["diagnostic"]


class TestLimits:
    CTX = {"project_name": "p", "stack_name": "s"}

    def test_limits_kwarg_is_applied(self):
        out = preprocess_jinja_diag("{{ range(6)|length }}", "t.yaml", self.CTX, limits="range=5")
        assert out["diagnostic"]["kind"] == "range_too_large"
        out = preprocess_jinja_diag("{{ range(5)|length }}", "t.yaml", self.CTX, limits="range=5")
        assert out == {"rendered": "5"}

    @pytest.mark.parametrize(
        "source, spec, kind",
        [
            ("{% for i in range(100) %}{{ i }}{% endfor %}", "fuel=50", "fuel_exhausted"),
            ("{{ 'x' * 100 }}", "output=10", "output_too_large"),
            ("{% macro r() %}{{ r() }}{% endmacro %}{{ r() }}", "recursion=20", "recursion_limit"),
        ],
    )
    def test_each_limit_has_its_kind(self, source, spec, kind):
        out = preprocess_jinja_diag(source, "t.yaml", self.CTX, limits=spec)
        assert out["diagnostic"]["kind"] == kind
        assert out["diagnostic"]["line"] == 1

    def test_a_malformed_spec_is_a_value_error(self):
        with pytest.raises(ValueError, match="PULUMI_YAML_JINJA_LIMITS"):
            preprocess_jinja("{{ 1 }}", "t.yaml", self.CTX, limits="fule=1")

    def test_the_environment_variable_is_read_when_no_kwarg(self, monkeypatch):
        monkeypatch.setenv("PULUMI_YAML_JINJA_LIMITS", "range=3")
        out = preprocess_jinja_diag("{{ range(4)|list }}", "t.yaml", self.CTX)
        assert out["diagnostic"]["kind"] == "range_too_large"
        monkeypatch.setenv("PULUMI_YAML_JINJA_LIMITS", "nonsense")
        with pytest.raises(ValueError):
            preprocess_jinja_diag("{{ 1 }}", "t.yaml", self.CTX)


class TestOrigin:
    def test_a_fault_in_an_include_names_the_included_file(self, tmp_path):
        (tmp_path / "inc").mkdir()
        (tmp_path / "inc" / "labels.j2").write_text("  team: data\n  owner: {{ owner }}\n")
        ctx = {"project_dir": str(tmp_path), "root_directory": str(tmp_path)}
        out = preprocess_jinja_diag("name: app\nlabels:\n{% include 'inc/labels.j2' %}\n", "Pulumi.yaml", ctx)
        d = out["diagnostic"]
        assert d["kind"] == "jinja_undefined_variable"
        assert d["message"] == "undefined value: `owner`"
        assert d["line"] == 3
        o = d["origin"]
        assert (o["file"], o["line"], o["expression"]) == ("inc/labels.j2", 2, "owner")
        assert o["source_line"] == "  owner: {{ owner }}"
        assert o["chain"] == [("Pulumi.yaml", 3), ("inc/labels.j2", 2)]
        with pytest.raises(ValueError, match=r"--> in inc/labels\.j2:2:13"):
            preprocess_jinja("name: app\nlabels:\n{% include 'inc/labels.j2' %}\n", "Pulumi.yaml", ctx)

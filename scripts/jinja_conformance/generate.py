#!/usr/bin/env python3
# Copyright (c) 2024-2026 Lituus-io. All rights reserved.
"""Generates the Jinja conformance goldens with pinned reference Jinja2.

Dev-time only: nothing at runtime imports Jinja2. Each case directory under
``tests/jinja_conformance/cases/<id>/`` holds ``template.j2``, a
``context.json`` (a flat map of strings, the shape the runtime's extra
context takes) and optionally ``includes/``. The reference render is written
to ``expected.txt``; a render Jinja2 refuses writes ``expected_error.txt``
holding the error class instead.

The environment mirrors the runtime's: StrictUndefined, autoescape off,
trailing newline dropped, ``do`` and loop controls enabled.

    python3 scripts/jinja_conformance/generate.py           # write goldens
    python3 scripts/jinja_conformance/generate.py --check   # fail on drift

``registry.json`` beside the cases records every filter, test and global the
reference ships; the Rust runner builds the capability matrix from it.
"""
import importlib.metadata
import json
import pathlib
import sys

import jinja2

PINNED = {"jinja2": "3.1.6", "markupsafe": "3.0.3"}
CORPUS = pathlib.Path(__file__).resolve().parents[2] / "tests" / "jinja_conformance"


def error_class(exc: BaseException) -> str:
    """The four classes the Rust runner compares, by exception type."""
    if isinstance(exc, (jinja2.TemplateNotFound, jinja2.TemplatesNotFound)):
        return "not_found"
    if isinstance(exc, jinja2.TemplateSyntaxError):
        return "syntax"
    if isinstance(exc, jinja2.UndefinedError):
        return "undefined"
    return "runtime"


def environment(loader=None) -> jinja2.Environment:
    return jinja2.Environment(
        loader=loader,
        undefined=jinja2.StrictUndefined,
        autoescape=False,
        keep_trailing_newline=False,
        extensions=["jinja2.ext.do", "jinja2.ext.loopcontrols"],
    )


def registry() -> str:
    """Every filter, test and global the reference ships: the matrix rows."""
    env = environment()
    doc = {
        "jinja2": PINNED["jinja2"],
        "filters": sorted(env.filters),
        "tests": sorted(env.tests),
        "globals": sorted(env.globals),
    }
    return json.dumps(doc, indent=2, sort_keys=True) + "\n"


def render(case: pathlib.Path):
    env = environment(jinja2.FileSystemLoader(str(case)))
    ctx = json.loads((case / "context.json").read_text())
    source = (case / "template.j2").read_bytes().decode("utf-8")
    try:
        out = env.from_string(source).render(**ctx)
    except Exception as exc:  # noqa: BLE001 -- every failure is a golden
        return "expected_error.txt", error_class(exc) + "\n"
    return "expected.txt", out


def main() -> int:
    for dist, want in PINNED.items():
        have = importlib.metadata.version(dist)
        if have != want:
            print(f"{dist} {have} is not the pinned {want}", file=sys.stderr)
            return 2
    check = "--check" in sys.argv[1:]
    drift = []
    reg = CORPUS / "registry.json"
    if not reg.exists() or reg.read_text() != registry():
        drift.append("registry.json")
        if not check:
            reg.write_text(registry())
    for case in sorted(p for p in (CORPUS / "cases").iterdir() if p.is_dir()):
        name, body = render(case)
        other = "expected.txt" if name == "expected_error.txt" else "expected_error.txt"
        target = case / name
        current = target.read_bytes().decode("utf-8") if target.exists() else None
        if current != body or (case / other).exists():
            drift.append(case.name)
            if not check:
                target.write_bytes(body.encode("utf-8"))
                (case / other).unlink(missing_ok=True)
    if check and drift:
        print("goldens drifted from jinja2 " + PINNED["jinja2"] + ": " + ", ".join(drift), file=sys.stderr)
        return 1
    print(f"{'checked' if check else 'generated'}: {len(drift)} changed")
    return 0


if __name__ == "__main__":
    sys.exit(main())

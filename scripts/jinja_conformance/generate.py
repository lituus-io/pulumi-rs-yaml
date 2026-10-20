#!/usr/bin/env python3
# Author: spicyzhug@gmail.com
# Copyright: lituus-io, all rights reserved
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

``methods/<id>/expr.txt`` holds one expression, valid in Jinja and in
Starlark; it is rendered as ``{{ expr }}``. ``registry.json`` beside the cases records every filter, test and global the
reference ships; the Rust runner builds the capability matrix from it.
"""
import datetime
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


def to_date(v):
    if isinstance(v, datetime.datetime):
        return v.date()
    if isinstance(v, datetime.date):
        return v
    return datetime.date.fromisoformat(v[:10])


def to_datetime(v):
    if isinstance(v, datetime.datetime):
        return v
    if isinstance(v, datetime.date):
        return datetime.datetime(v.year, v.month, v.day)
    return datetime.datetime.fromisoformat(v)


def environment(loader=None) -> jinja2.Environment:
    env = jinja2.Environment(
        loader=loader,
        undefined=jinja2.StrictUndefined,
        autoescape=False,
        keep_trailing_newline=False,
        extensions=["jinja2.ext.do", "jinja2.ext.loopcontrols"],
    )
    return env


def with_dates(env: jinja2.Environment) -> jinja2.Environment:
    """The date helpers the runtime adds, given to the reference as Python's
    own datetime, so date cases compare against Python's semantics."""
    strptime = datetime.datetime.strptime
    env.globals.update(date=datetime.date, datetime=datetime.datetime,
                       timedelta=datetime.timedelta, to_date=to_date,
                       to_datetime=to_datetime, strptime=strptime)
    env.filters.update(to_date=to_date, to_datetime=to_datetime, strptime=strptime)
    return env


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
    env = with_dates(environment(jinja2.FileSystemLoader(str(case))))
    if (case / "expr.txt").exists():
        source = "{{ " + (case / "expr.txt").read_text().rstrip("\n") + " }}"
    else:
        source = (case / "template.j2").read_bytes().decode("utf-8")
    ctxfile = case / "context.json"
    ctx = json.loads(ctxfile.read_text()) if ctxfile.exists() else {}
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
    cases = [p for d in ("cases", "methods") for p in (CORPUS / d).iterdir() if p.is_dir()]
    for case in sorted(cases):
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

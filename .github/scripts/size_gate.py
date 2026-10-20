#!/usr/bin/env python3
# Author: spicyzhug@gmail.com
# Copyright: lituus-io, all rights reserved
"""Fail when any release artefact grew more than the allowed share.

    size_gate.py PREVIOUS.json CURRENT.json [--max-growth-pct 2] [--summary FILE]

Each JSON file maps an artefact name to its size in bytes. An artefact with no
previous size is reported as new and never fails; one that shrank never fails.
The table goes to stdout and, with --summary, is appended to that file (the
GitHub step summary).
"""

import argparse
import json
import sys


def compare(previous, current, max_growth_pct):
    rows, failed = [], []
    for name in sorted(current):
        now = current[name]
        before = previous.get(name)
        if before is None:
            rows.append((name, "-", now, "new", "ok"))
            continue
        growth = (now - before) * 100.0 / before if before else 0.0
        verdict = "FAIL" if growth > max_growth_pct else "ok"
        if verdict == "FAIL":
            failed.append(name)
        rows.append((name, before, now, f"{growth:+.2f}%", verdict))
    return rows, failed


def render(rows, previous_label, max_growth_pct):
    out = [
        f"### Release binary sizes (gate: +{max_growth_pct:g}% against {previous_label})",
        "",
        "| Artefact | Previous | Current | Change | Gate |",
        "|---|---:|---:|---:|---|",
    ]
    out += [f"| {n} | {b} | {c} | {g} | {v} |" for n, b, c, g, v in rows]
    return "\n".join(out) + "\n"


def main(argv=None):
    p = argparse.ArgumentParser()
    p.add_argument("previous")
    p.add_argument("current")
    p.add_argument("--max-growth-pct", type=float, default=2.0)
    p.add_argument("--label", default="the previous release")
    p.add_argument("--summary")
    a = p.parse_args(argv)
    with open(a.previous) as f:
        previous = json.load(f)
    with open(a.current) as f:
        current = json.load(f)
    rows, failed = compare(previous, current, a.max_growth_pct)
    table = render(rows, a.label, a.max_growth_pct)
    sys.stdout.write(table)
    if a.summary:
        with open(a.summary, "a") as f:
            f.write(table)
    if failed:
        print(f"::error::grew more than {a.max_growth_pct:g}%: {', '.join(failed)}")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())

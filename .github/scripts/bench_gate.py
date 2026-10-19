#!/usr/bin/env python3
# Copyright (c) 2024-2026 Lituus-io. All rights reserved.
"""Fail when a benchmark regressed beyond the noise threshold.

    bench_gate.py BASE.json HEAD.json --base-version X.Y.Z
                  [--rebaseline FILE] [--threshold 1.25] [--min-delta-ns 100]

BASE and HEAD map a benchmark name to its mean in nanoseconds. A benchmark
fails when HEAD exceeds BASE by more than the threshold AND by more than the
absolute noise floor (nanosecond-scale benches shift by fractions of a ns
from code alignment alone).

The threshold is the same for every benchmark. A reviewed entry in the
re-baseline file can only move one benchmark's EXPECTATION, never the
threshold: a re-baselined bench fails beyond `expected_ratio x threshold`,
so a further regression is still caught relative to the new expectation.
Entries name the release they apply to (`valid_for`) and are refused once
the merge-base is at or past it, so none outlives the change it explains;
an entry naming a benchmark that does not exist is refused too.
"""

import argparse
import json
import sys

try:
    import tomllib
except ModuleNotFoundError:  # Python < 3.11
    tomllib = None


def version(text):
    return tuple(int(p) for p in text.strip().split("."))


def load_rebaseline(path):
    if path is None:
        return []
    if tomllib is None:
        sys.exit("bench_gate: reading the re-baseline file needs Python 3.11+ (tomllib)")
    with open(path, "rb") as f:
        return tomllib.load(f).get("bench", [])


def check(base, head, entries, base_version, threshold, min_delta_ns):
    errors, regressions, notes = [], [], []
    expected = {}
    for e in entries:
        name = e.get("name")
        for field in ("name", "valid_for", "expected_ratio", "measured", "reason"):
            if not e.get(field):
                errors.append(f"re-baseline entry {name!r} has no {field}")
        if not name or not e.get("valid_for") or not e.get("expected_ratio"):
            continue
        if version(base_version) >= version(e["valid_for"]):
            errors.append(
                f"stale re-baseline entry {name!r}: valid for {e['valid_for']}, and the "
                f"merge-base is already {base_version}; remove it"
            )
            continue
        if name not in base:
            errors.append(f"re-baseline entry {name!r} names no benchmark")
            continue
        expected[name] = float(e["expected_ratio"])
    for name, base_mean in sorted(base.items()):
        head_mean = head.get(name)
        if head_mean is None:
            continue
        ratio = expected.get(name, 1.0)
        limit = base_mean * ratio * threshold
        if head_mean > limit and head_mean - base_mean * ratio > min_delta_ns:
            what = f" (re-baselined to {ratio:.2f}x)" if name in expected else ""
            regressions.append(
                f"{name}: {base_mean:.0f}ns -> {head_mean:.0f}ns "
                f"({head_mean / base_mean:.2f}x){what}"
            )
        elif name in expected:
            notes.append(
                f"{name}: {head_mean / base_mean:.2f}x against a re-baselined "
                f"expectation of {ratio:.2f}x (fails beyond {ratio * threshold:.2f}x)"
            )
    return errors, regressions, notes


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("base")
    ap.add_argument("head")
    ap.add_argument("--base-version", required=True)
    ap.add_argument("--rebaseline")
    ap.add_argument("--threshold", type=float, default=1.25)
    ap.add_argument("--min-delta-ns", type=float, default=100.0)
    args = ap.parse_args()
    with open(args.base) as f:
        base = json.load(f)
    with open(args.head) as f:
        head = json.load(f)
    errors, regressions, notes = check(
        base,
        head,
        load_rebaseline(args.rebaseline),
        args.base_version,
        args.threshold,
        args.min_delta_ns,
    )
    for n in notes:
        print(n)
    if errors:
        print("re-baseline file refused:")
        print("\n".join(errors))
    if regressions:
        print(f"benchmark regressions beyond {round((args.threshold - 1) * 100)}% threshold:")
        print("\n".join(regressions))
    if errors or regressions:
        sys.exit(1)
    print(f"no regressions beyond {round((args.threshold - 1) * 100)}% across {len(base)} benchmarks")


if __name__ == "__main__":
    main()

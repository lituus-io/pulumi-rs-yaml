#!/usr/bin/env python3
# Author: spicyzhug@gmail.com
# Copyright: lituus-io, all rights reserved
"""Every authored file carries the two header lines, in its own comment syntax.

    Author: spicyzhug@gmail.com
    Copyright: lituus-io, all rights reserved

within its first lines (after a shebang). Files are the tracked ones
(`git ls-files`); a file is held to the header when its type takes comments
and it is not one of the excluded categories below -- files whose bytes are
data under test or are written by a tool, where a header would change what
is tested or be overwritten.

    python3 scripts/check_headers.py          # fail on a missing header
    python3 scripts/check_headers.py --fix    # write the missing headers

Run by the CI lint job. Standard library only.
"""

import pathlib
import subprocess
import sys

AUTHOR = "Author: spicyzhug@gmail.com"
COPYRIGHT = "Copyright: lituus-io, all rights reserved"
# The single line the pair replaced; --fix rewrites it in place.
FORMER = "Copyright (c) 2024-2026 Lituus-io. All rights reserved."
# How far into a file the pair may sit (a shebang and a blank line first).
WITHIN = 4

# Comment syntax by suffix (or by whole name for files without one).
LINE = {
    ".rs": "//",
    ".py": "#",
    ".pyi": "#",
    ".sh": "#",
    ".ps1": "#",
    ".toml": "#",
    ".yml": "#",
    ".yaml": "#",
}
NAMES = {"Dockerfile": "#", ".gitignore": "#", ".gitattributes": "#"}
BLOCK = {".md": ("<!-- ", " -->")}

# Excluded categories: a header there would change bytes a test reads, or be
# overwritten by the tool that writes the file.
EXCLUDED = (
    # Generated: protobuf code written by prost; the capability matrix
    # written by the conformance runner and compared byte for byte.
    "crates/pulumi-rs-yaml-proto/src/generated/",
    "tests/jinja_conformance/",
    # Fuzz corpora and crash artifacts: inputs, byte for byte.
    "fuzz/corpus/",
    "fuzz/artifacts/",
    # Fixtures, golden outputs and test data.
    "crates/pulumi-rs-yaml-core/tests/fixtures/",
    "crates/pulumi-rs-yaml-converter/tests/testdata/",
    # Vendored upstream protocol definitions.
    "proto/",
)
# Test programs the acceptance suites deploy are data; their harness is not.
DATA_UNDER = {"tests/acceptance/": (".yaml", ".yml")}


def syntax(path: str):
    p = pathlib.PurePosixPath(path)
    if any(path.startswith(e) for e in EXCLUDED):
        return None
    for prefix, suffixes in DATA_UNDER.items():
        if path.startswith(prefix) and p.suffix in suffixes:
            return None
    if p.name in NAMES:
        return (NAMES[p.name] + " ", "")
    if p.suffix in LINE:
        return (LINE[p.suffix] + " ", "")
    if p.suffix in BLOCK:
        return BLOCK[p.suffix]
    return None


def header(open_, close):
    return [f"{open_}{AUTHOR}{close}", f"{open_}{COPYRIGHT}{close}"]


def has_header(lines, want):
    head = [l.rstrip("\r") for l in lines[: WITHIN + 1]]
    return any(head[i : i + 2] == want for i in range(len(head)))


def fix(text: str, want):
    newline = "\r\n" if "\r\n" in text else "\n"
    lines = text.split(newline)
    for i, line in enumerate(lines[:WITHIN]):
        if FORMER in line:
            lines[i : i + 1] = want
            return newline.join(lines)
    at = 1 if lines and lines[0].startswith("#!") else 0
    lines[at:at] = want + ([""] if at < len(lines) and lines[at].strip() else [])
    return newline.join(lines)


def main() -> int:
    write = "--fix" in sys.argv[1:]
    root = pathlib.Path(
        subprocess.run(
            ["git", "rev-parse", "--show-toplevel"], capture_output=True, text=True, check=True
        ).stdout.strip()
    )
    files = subprocess.run(
        ["git", "ls-files", "-z"], cwd=root, capture_output=True, text=True, check=True
    ).stdout.split("\0")
    missing = []
    checked = 0
    for path in filter(None, files):
        form = syntax(path)
        if form is None:
            continue
        full = root / path
        if not full.is_file():
            continue
        checked += 1
        text = full.read_bytes().decode("utf-8")
        want = header(*form)
        if has_header(text.splitlines(), want):
            continue
        if write:
            full.write_bytes(fix(text, want).encode("utf-8"))
        else:
            missing.append(path)
    if missing:
        print(f"{len(missing)} file(s) without the header pair:", file=sys.stderr)
        for m in missing:
            print(f"  {m}", file=sys.stderr)
        print("run: python3 scripts/check_headers.py --fix", file=sys.stderr)
        return 1
    print(f"headers: {checked} files {'fixed' if write else 'checked'}")
    return 0


if __name__ == "__main__":
    sys.exit(main())

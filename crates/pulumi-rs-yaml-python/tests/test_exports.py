# Copyright (c) 2024-2026 Lituus-io. All rights reserved.

"""Keep the public re-export surface in sync with the native module.

Every #[pyfunction] registered in _native must be re-exported from the
package root, so consumers never have to reach into pulumi_yaml_rs._native.
"""

import inspect

import pulumi_yaml_rs
from pulumi_yaml_rs import _native


def _native_functions():
    return [
        name
        for name, obj in inspect.getmembers(_native, callable)
        if not name.startswith("_")
    ]


def test_all_native_functions_are_reexported():
    missing = [
        name for name in _native_functions() if name not in pulumi_yaml_rs.__all__
    ]
    assert not missing, f"native functions missing from __all__: {missing}"


# export_sql_lineage is gated behind the optional `sql-lineage` build
# feature, so __all__ may legitimately list it while _native lacks it.
OPTIONAL_EXPORTS = {"export_sql_lineage"}


def test_all_entries_are_importable():
    for name in pulumi_yaml_rs.__all__:
        if name in OPTIONAL_EXPORTS and not hasattr(_native, name):
            continue
        assert hasattr(pulumi_yaml_rs, name), f"__all__ lists {name} but it is not importable"

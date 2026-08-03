# Copyright (c) 2024-2026 Lituus-io. All rights reserved.

from pulumi_yaml_rs._native import (
    parse_template,
    load_project,
    discover_project_files,
    has_jinja_blocks,
    strip_jinja_blocks,
    validate_jinja,
    preprocess_jinja,
    evaluate_builtin,
    create_execution_plan,
    export_dependency_graph,
    validate_and_classify,
    type_check_project,
    complete_properties,
    get_resource_schema,
)

try:  # optional: requires the `sql-lineage` build feature
    from pulumi_yaml_rs._native import export_sql_lineage
except ImportError:  # pragma: no cover - feature-disabled builds
    export_sql_lineage = None

from pulumi_yaml_rs._find_binary import find_language_binary, find_converter_binary

__all__ = [
    "parse_template",
    "load_project",
    "discover_project_files",
    "has_jinja_blocks",
    "strip_jinja_blocks",
    "validate_jinja",
    "preprocess_jinja",
    "evaluate_builtin",
    "create_execution_plan",
    "export_dependency_graph",
    "export_sql_lineage",
    "validate_and_classify",
    "type_check_project",
    "complete_properties",
    "get_resource_schema",
    "find_language_binary",
    "find_converter_binary",
]

"""Tests for Python ↔ Rust type conversion round-trips.

Exercises py_to_value and value_to_py via evaluate_builtin("select", [0, [value]]).
"""

import pytest
from pulumi_yaml_rs import evaluate_builtin


def roundtrip(value):
    """Send a value through Python → Rust → Python via select(0, [value])."""
    return evaluate_builtin("select", [0, [value]])


class TestTypeConversions:
    def test_none_roundtrip(self):
        assert roundtrip(None) is None

    def test_bool_roundtrip(self):
        assert roundtrip(True) is True
        assert roundtrip(False) is False

    def test_int_roundtrip(self):
        result = roundtrip(42)
        assert result == 42
        assert isinstance(result, int)

    def test_float_roundtrip(self):
        result = roundtrip(3.14)
        assert abs(result - 3.14) < 1e-9
        assert isinstance(result, float)

    def test_string_roundtrip(self):
        result = roundtrip("hello")
        assert result == "hello"
        assert isinstance(result, str)

    def test_list_roundtrip(self):
        result = roundtrip([1, "a", True])
        assert result == [1, "a", True]

    def test_dict_roundtrip(self):
        result = roundtrip({"key": "value"})
        assert result == {"key": "value"}

    def test_nested_roundtrip(self):
        value = {"a": [1, {"b": "c"}]}
        result = roundtrip(value)
        assert result == value

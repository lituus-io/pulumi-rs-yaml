"""Tests for evaluate_builtin() — all builtin function evaluation."""

import json
import re

import pytest
from pulumi_yaml_rs import evaluate_builtin


class TestMathBuiltins:
    def test_abs_positive(self):
        assert evaluate_builtin("abs", -5) == 5

    def test_abs_float(self):
        result = evaluate_builtin("abs", -3.14)
        assert abs(result - 3.14) < 1e-9

    def test_floor(self):
        assert evaluate_builtin("floor", 3.7) == 3

    def test_ceil(self):
        assert evaluate_builtin("ceil", 3.2) == 4

    def test_max(self):
        assert evaluate_builtin("max", [1, 5, 3]) == 5

    def test_min(self):
        assert evaluate_builtin("min", [1, 5, 3]) == 1


class TestStringBuiltins:
    def test_join(self):
        result = evaluate_builtin("join", [",", ["a", "b", "c"]])
        assert result == "a,b,c"

    def test_split(self):
        result = evaluate_builtin("split", [",", "a,b,c"])
        assert result == ["a", "b", "c"]

    def test_select(self):
        result = evaluate_builtin("select", [1, ["a", "b", "c"]])
        assert result == "b"

    def test_string_len(self):
        result = evaluate_builtin("stringLen", "hello")
        assert result == 5

    def test_substring(self):
        result = evaluate_builtin("substring", ["hello world", 6, 5])
        assert result == "world"

    def test_to_json(self):
        result = evaluate_builtin("toJSON", {"a": 1})
        parsed = json.loads(result)
        assert parsed == {"a": 1}


class TestEncodingBuiltins:
    def test_to_base64(self):
        result = evaluate_builtin("toBase64", "hello")
        assert result == "aGVsbG8="

    def test_from_base64(self):
        result = evaluate_builtin("fromBase64", "aGVsbG8=")
        assert result == "hello"


class TestSecretBuiltin:
    def test_secret(self):
        result = evaluate_builtin("secret", "password")
        assert result["__secret"] is True
        assert result["value"] == "password"


class TestTimeRandomBuiltins:
    def test_uuid(self):
        result = evaluate_builtin("uuid", "")
        uuid_pattern = re.compile(
            r"^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$",
            re.IGNORECASE,
        )
        assert uuid_pattern.match(result), f"Not a valid UUID: {result}"

    def test_random_string(self):
        result = evaluate_builtin("randomString", 16)
        assert isinstance(result, str)
        assert len(result) == 16

    def test_time_utc(self):
        # Go-style time format reference: "2006-01-02T15:04:05Z07:00"
        result = evaluate_builtin("timeUtc", "2006-01-02T15:04:05Z07:00")
        assert isinstance(result, str)
        # Should contain a 4-digit year
        assert re.search(r"\d{4}", result)


class TestBuiltinErrors:
    def test_unknown_builtin_error(self):
        with pytest.raises(ValueError):
            evaluate_builtin("nonexistent", "arg")

    def test_join_wrong_args_error(self):
        with pytest.raises((ValueError, TypeError)):
            evaluate_builtin("join", "not-a-list")

    def test_abs_string_error(self):
        with pytest.raises((ValueError, TypeError)):
            evaluate_builtin("abs", "not-a-number")

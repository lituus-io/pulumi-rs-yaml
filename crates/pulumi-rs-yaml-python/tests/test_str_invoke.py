# Copyright (c) 2024-2026 Lituus-io. All rights reserved.

"""Tests for evaluate_str_invoke() — the in-process `str` package evaluator.

The semantics under test are Go's, because that is what a deploy produces:
`strings.ReplaceAll` / `TrimPrefix` / `TrimSuffix` and RE2 for the `regexp`
functions, including the empty-pattern rule and Go's replacement template
(`$1`, `$$`). The binding is a conversion layer over the evaluator the engine
itself uses, so these cases are the evaluator's own contract seen from Python.
"""

import time

import pytest

from pulumi_yaml_rs import evaluate_str_invoke


class TestAllSixFunctions:
    """One case per function, in the spelling templates actually write."""

    @pytest.mark.parametrize(
        "token,args,expected",
        [
            (
                "str:replace",
                {"string": "geo_fence", "old": "_", "new": "-"},
                {"result": "geo-fence"},
            ),
            (
                "str:trimPrefix",
                {"string": "raw-orders", "prefix": "raw-"},
                {"result": "orders"},
            ),
            (
                "str:trimSuffix",
                {"string": "orders_tmp", "suffix": "_tmp"},
                {"result": "orders"},
            ),
            (
                "str:regexp:replace",
                {"string": "geo_fence", "old": "_(.*)$", "new": "-$1"},
                {"result": "geo-fence"},
            ),
            (
                "str:regexp:match",
                {"string": "geo_fence", "pattern": "^geo"},
                {"matches": True},
            ),
            (
                "str:regexp:split",
                {"string": "a,b,c", "on": ","},
                {"result": ["a", "b", "c"]},
            ),
        ],
    )
    def test_function(self, token, args, expected):
        assert evaluate_str_invoke(token, args) == expected

    def test_match_answers_matches_not_result(self):
        out = evaluate_str_invoke("str:regexp:match", {"string": "x", "pattern": "y"})
        assert out == {"matches": False}
        assert "result" not in out


class TestTokenSpellings:
    """Every spelling a template may write reaches the same function."""

    @pytest.mark.parametrize(
        "token",
        ["str:replace", "str:index:replace", "str:index/replace:replace"],
    )
    def test_replace_spellings(self, token):
        out = evaluate_str_invoke(token, {"string": "a_b", "old": "_", "new": "-"})
        assert out == {"result": "a-b"}

    @pytest.mark.parametrize(
        "token",
        ["str:regexp:replace", "str:regexp/replace:replace"],
    )
    def test_regexp_replace_spellings(self, token):
        out = evaluate_str_invoke(token, {"string": "a_b", "old": "_", "new": "-"})
        assert out == {"result": "a-b"}

    @pytest.mark.parametrize(
        "token",
        ["str:trimSuffix", "str:index:trimSuffix", "str:index/trimSuffix:trimSuffix"],
    )
    def test_trim_suffix_spellings(self, token):
        out = evaluate_str_invoke(token, {"string": "a_b", "suffix": "_b"})
        assert out == {"result": "a"}


class TestGoSemantics:
    """Rows taken from the evaluator's own Go-conformance cases."""

    def test_empty_old_gives_k_plus_one_replacements(self):
        out = evaluate_str_invoke("str:replace", {"string": "abc", "old": "", "new": "-"})
        assert out == {"result": "-a-b-c-"}

    def test_empty_old_splits_at_utf8_boundaries(self):
        out = evaluate_str_invoke("str:replace", {"string": "héllo", "old": "", "new": "."})
        assert out == {"result": ".h.é.l.l.o."}

    def test_replace_is_non_overlapping_left_to_right(self):
        out = evaluate_str_invoke("str:replace", {"string": "aaa", "old": "aa", "new": "b"})
        assert out == {"result": "ba"}

    def test_trim_prefix_only_trims_a_prefix(self):
        assert evaluate_str_invoke(
            "str:trimPrefix", {"string": "abcabc", "prefix": "abc"}
        ) == {"result": "abc"}
        assert evaluate_str_invoke(
            "str:trimPrefix", {"string": "xabc", "prefix": "abc"}
        ) == {"result": "xabc"}

    def test_regexp_replace_expands_numbered_group(self):
        out = evaluate_str_invoke(
            "str:regexp:replace",
            {"string": "2026-09-01", "old": r"^(\d{4})-(\d{2})", "new": "$2/$1"},
        )
        assert out == {"result": "09/2026-01"}

    def test_regexp_replace_expands_named_group(self):
        out = evaluate_str_invoke(
            "str:regexp:replace",
            {"string": "geo_fence", "old": "(?P<head>geo)_", "new": "${head}-"},
        )
        assert out == {"result": "geo-fence"}

    def test_double_dollar_is_a_literal_dollar(self):
        out = evaluate_str_invoke(
            "str:regexp:replace", {"string": "cost", "old": "^", "new": "$$"}
        )
        assert out == {"result": "$cost"}

    def test_split_on_the_empty_pattern(self):
        # Go yields one element per rune, with no leading or trailing "".
        out = evaluate_str_invoke("str:regexp:split", {"string": "foo", "on": ""})
        assert out == {"result": ["f", "o", "o"]}

    def test_split_of_an_empty_subject(self):
        out = evaluate_str_invoke("str:regexp:split", {"string": "", "on": ","})
        assert out == {"result": [""]}

    def test_split_count_caps_the_parts(self):
        out = evaluate_str_invoke(
            "str:regexp:split", {"string": "a,b,c", "on": ",", "count": 2}
        )
        assert out == {"result": ["a", "b,c"]}


class TestNotAnswered:
    """`None` means "not answered here", never "the empty string"."""

    @pytest.mark.parametrize(
        "token",
        [
            "str:unknown",
            "str:index:unknown",
            "gcp:compute:getNetwork",
            "std:index:join",
            "",
            "str",
            "a:b:c:d",
        ],
    )
    def test_unknown_token_is_none(self, token):
        assert evaluate_str_invoke(token, {"string": "a", "old": "b", "new": "c"}) is None

    def test_missing_argument_is_none(self):
        assert evaluate_str_invoke("str:replace", {"string": "a_b", "old": "_"}) is None

    def test_no_arguments_at_all_is_none(self):
        assert evaluate_str_invoke("str:replace", {}) is None

    @pytest.mark.parametrize("bad", [1, 1.5, True, None, ["a"], {"k": "v"}])
    def test_non_string_argument_is_none(self, bad):
        args = {"string": "a_b", "old": "_", "new": "-"}
        args["old"] = bad
        assert evaluate_str_invoke("str:replace", args) is None

    def test_non_string_key_is_ignored_not_raised(self):
        # A non-string key can never name a `str` argument; it leaves the
        # argument missing rather than raising.
        assert evaluate_str_invoke("str:replace", {1: "x"}) is None

    def test_uncompilable_pattern_is_none(self):
        assert (
            evaluate_str_invoke(
                "str:regexp:replace", {"string": "a", "old": "(unclosed", "new": "b"}
            )
            is None
        )

    def test_backreference_pattern_is_none(self):
        # RE2 has no backreferences; declining beats guessing a wrong string.
        assert (
            evaluate_str_invoke(
                "str:regexp:match", {"string": "aa", "pattern": r"(a)\1"}
            )
            is None
        )

    def test_non_positive_split_count_is_none(self):
        assert (
            evaluate_str_invoke(
                "str:regexp:split", {"string": "a,b", "on": ",", "count": 0}
            )
            is None
        )


class TestArgumentValidation:
    @pytest.mark.parametrize("args", [["string", "a"], "string=a", 42, None, ("a", "b")])
    def test_non_dict_args_raise_value_error(self, args):
        with pytest.raises(ValueError):
            evaluate_str_invoke("str:replace", args)

    def test_value_error_message_names_the_expectation(self):
        with pytest.raises(ValueError, match="dict"):
            evaluate_str_invoke("str:replace", ["a"])


class TestPerformance:
    """RE2 is linear, and the binding's conversion must stay cheap."""

    def test_one_mib_subject_with_a_nested_quantifier(self):
        subject = "a" * (1024 * 1024)
        start = time.perf_counter()
        out = evaluate_str_invoke(
            "str:regexp:match", {"string": subject, "pattern": "(a+)+b"}
        )
        elapsed = time.perf_counter() - start
        assert out == {"matches": False}
        assert elapsed < 1.0, f"1 MiB match took {elapsed:.3f}s"

    def test_one_mib_replace_stays_linear(self):
        subject = "a_b" * 350_000
        start = time.perf_counter()
        out = evaluate_str_invoke(
            "str:regexp:replace", {"string": subject, "old": "_", "new": "-"}
        )
        elapsed = time.perf_counter() - start
        assert out["result"].count("-") == 350_000
        assert elapsed < 1.0, f"1 MiB replace took {elapsed:.3f}s"

    def test_ten_thousand_calls_stay_within_budget(self):
        # A generous ceiling: this is a conversion-regression tripwire, not a
        # throughput target.
        args = {"string": "geo_fence", "old": "_", "new": "-"}
        start = time.perf_counter()
        for _ in range(10_000):
            evaluate_str_invoke("str:replace", args)
        elapsed = time.perf_counter() - start
        assert elapsed < 2.0, f"10 000 calls took {elapsed:.3f}s"

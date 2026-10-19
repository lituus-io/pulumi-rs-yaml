# Method divergences

Method cases whose result differs from pinned Python Jinja2, from the
embedded Starlark (`Globals::standard()`), or from both. `engine` names
which; the runtime's own output for a listed case is pinned beside it.

| case | engine | reason |
|---|---|---|
| `map_items` | both | each engine prints the pairs its own way: Python `dict_items([('a', 1)])`, Starlark tuples, this runtime JSON-style lists; the pairs are the same |
| `map_keys` | both | a map literal's keys are kept in key order here (see the main DIVERGENCES.md); both references keep insertion order |
| `map_copy` | starlark | Starlark dicts have no `copy` |
| `num_as_integer_ratio` | starlark | Starlark numbers have no methods |
| `num_bit_length` | starlark | Starlark numbers have no methods |
| `num_conjugate` | starlark | Starlark numbers have no methods |
| `num_is_integer` | starlark | Starlark numbers have no methods |
| `seq_copy` | starlark | Starlark lists have no `copy` |
| `seq_count` | starlark | Starlark lists have no `count` |
| `str_casefold` | starlark | Starlark strings have no `casefold` |
| `str_center` | starlark | Starlark strings have no `center` |
| `str_endswith` | starlark | Starlark's `endswith` takes no start/end arguments |
| `str_expandtabs` | starlark | Starlark strings have no `expandtabs` |
| `str_format_comma_hex` | starlark | Starlark's `format` has no format-spec mini-language |
| `str_format_spec` | starlark | Starlark's `format` has no format-spec mini-language |
| `str_isdecimal` | starlark | Starlark strings have no `isdecimal` |
| `str_isidentifier` | starlark | Starlark strings have no `isidentifier` |
| `str_isnumeric` | starlark | Starlark strings have no `isnumeric` |
| `str_ljust` | starlark | Starlark strings have no `ljust` |
| `str_rjust` | starlark | Starlark strings have no `rjust` |
| `str_split_empty_sep` | starlark | Starlark splits on an empty separator; Python refuses it, as this runtime does |
| `str_startswith` | starlark | Starlark's `startswith` takes no start/end arguments |
| `str_swapcase` | starlark | Starlark strings have no `swapcase` |
| `str_zfill` | starlark | Starlark strings have no `zfill` |

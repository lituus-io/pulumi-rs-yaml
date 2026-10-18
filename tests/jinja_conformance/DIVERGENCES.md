# Jinja2 divergences

Every conformance case whose render differs from pinned reference Jinja2 is
listed here with a reason; the runner fails on an unlisted difference AND on a
listed case that has started to match. The engine's own output for each listed
case is pinned beside it (`rsyaml.txt` / `rsyaml_error.txt`), so a divergence
cannot drift either.

Classes:
- **intended** -- a deliberate difference, kept because changing it would
  change the text already-deployed stacks render (and so their resources);
- **extension** -- pulumi-rs-yaml only; Jinja2 refuses the name;
- **unsupported** -- the engine refuses a construct Jinja2 accepts;
- **gap** -- a known difference with no stability reason to keep it.

| case | class | reason |
|---|---|---|
| `err_macro_too_many_args` | gap | a call with too many arguments is classed as a syntax error, Jinja2 a runtime TypeError |
| `expr_dict_methods` | intended | a list or map renders JSON-style (`["a"]`, `{"k": 1}`; pairs as lists) where Jinja2 prints Python reprs (`['a']`, `('k', 1)`); the values are identical, both are valid YAML flow, and stacks already deploy the JSON spelling -- use `tojson` for portable text |
| `expr_div_zero_error` | gap | `1 / 0` renders `inf`; Jinja2 raises ZeroDivisionError |
| `expr_filter_pipeline` | extension | pulumi-rs-yaml only; Jinja2 does not define the name |
| `expr_float_render` | intended | floats never use exponent notation (`1e20` renders `100000000000000000000.0`, Jinja2 `1e+20`) |
| `expr_list_render_strings` | intended | a list or map renders JSON-style (`["a"]`, `{"k": 1}`; pairs as lists) where Jinja2 prints Python reprs (`['a']`, `('k', 1)`); the values are identical, both are valid YAML flow, and stacks already deploy the JSON spelling -- use `tojson` for portable text |
| `expr_literal_dict` | intended | a list or map renders JSON-style (`["a"]`, `{"k": 1}`; pairs as lists) where Jinja2 prints Python reprs (`['a']`, `('k', 1)`); the values are identical, both are valid YAML flow, and stacks already deploy the JSON spelling -- use `tojson` for portable text |
| `expr_literal_floats` | intended | floats never use exponent notation (`1.0e-5` renders `0.00001`, Jinja2 `1e-05`) |
| `expr_literal_list` | intended | a list or map renders JSON-style (`["a"]`, `{"k": 1}`; pairs as lists) where Jinja2 prints Python reprs (`['a']`, `('k', 1)`); the values are identical, both are valid YAML flow, and stacks already deploy the JSON spelling -- use `tojson` for portable text |
| `expr_literal_tuple` | intended | tuples are lists in the engine, so `(1, 2)` renders `[1, 2]` |
| `expr_mod` | gap | `%` with a negative divisor takes the dividend's sign (`7 % -3` is `1`; Python `-2`) |
| `expr_nested_data` | intended | a list or map renders JSON-style (`["a"]`, `{"k": 1}`; pairs as lists) where Jinja2 prints Python reprs (`['a']`, `('k', 1)`); the values are identical, both are valid YAML flow, and stacks already deploy the JSON spelling -- use `tojson` for portable text |
| `expr_percent_format` | unsupported | `'%s' % x` string formatting; use the `format` filter |
| `expr_str_format_named` | gap | `str.format` is not available |
| `expr_str_methods` | gap | Python string methods (`.upper()`, `.split()`, ...) are not available |
| `expr_str_methods_more` | gap | Python string methods (`.join()`, `.format()`, ...) are not available |
| `expr_str_methods_predicates` | intended | a list or map renders JSON-style (`["a"]`, `{"k": 1}`; pairs as lists) where Jinja2 prints Python reprs (`['a']`, `('k', 1)`); the values are identical, both are valid YAML flow, and stacks already deploy the JSON spelling -- use `tojson` for portable text |
| `expr_str_methods_split_ws` | intended | a list or map renders JSON-style (`["a"]`, `{"k": 1}`; pairs as lists) where Jinja2 prints Python reprs (`['a']`, `('k', 1)`); the values are identical, both are valid YAML flow, and stacks already deploy the JSON spelling -- use `tojson` for portable text |
| `expr_str_title_method` | gap | `str.title` is not available |
| `ext_ctx_config` | extension | pulumi-rs-yaml only; Jinja2 does not define the name |
| `ext_ctx_pulumi` | extension | pulumi-rs-yaml only; Jinja2 does not define the name |
| `ext_filter_base64_decode` | extension | pulumi-rs-yaml only; Jinja2 does not define the name |
| `ext_filter_base64_encode` | extension | pulumi-rs-yaml only; Jinja2 does not define the name |
| `ext_filter_bool` | extension | pulumi-rs-yaml only; Jinja2 does not define the name |
| `ext_filter_chain` | extension | pulumi-rs-yaml only; Jinja2 does not define the name |
| `ext_filter_lines` | extension | pulumi-rs-yaml only; Jinja2 does not define the name |
| `ext_filter_split` | extension | pulumi-rs-yaml only; Jinja2 does not define the name |
| `ext_filter_to_json` | extension | pulumi-rs-yaml only; Jinja2 does not define the name |
| `ext_filter_to_yaml` | extension | pulumi-rs-yaml only; Jinja2 does not define the name |
| `ext_filter_zip` | extension | pulumi-rs-yaml only; Jinja2 does not define the name |
| `ext_fn_readfile` | extension | pulumi-rs-yaml only; Jinja2 does not define the name |
| `ext_global_debug_removed` | gap | `debug()` is a minijinja global that dumps the whole context, config included |
| `ext_test_endingwith` | extension | pulumi-rs-yaml only; Jinja2 does not define the name |
| `ext_test_startingwith` | extension | pulumi-rs-yaml only; Jinja2 does not define the name |
| `filter_attr` | intended | `attr` falls back to item lookup, so a map key is found; Jinja2 looks up Python attributes only |
| `filter_batch` | intended | a list or map renders JSON-style (`["a"]`, `{"k": 1}`; pairs as lists) where Jinja2 prints Python reprs (`['a']`, `('k', 1)`); the values are identical, both are valid YAML flow, and stacks already deploy the JSON spelling -- use `tojson` for portable text |
| `filter_dictsort` | intended | a list or map renders JSON-style (`["a"]`, `{"k": 1}`; pairs as lists) where Jinja2 prints Python reprs (`['a']`, `('k', 1)`); the values are identical, both are valid YAML flow, and stacks already deploy the JSON spelling -- use `tojson` for portable text |
| `filter_dictsort_case_sensitive` | gap | positional `case_sensitive` is refused |
| `filter_escape` | intended | HTML entities spelled `&quot;`/`&#x27;`/`&#x2f;` where Jinja2 uses `&#34;`/`&#39;`; equivalent HTML, and output is YAML, never auto-escaped |
| `filter_filesizeformat` | gap | not implemented |
| `filter_float` | gap | a value that is not a number fails instead of yielding the default |
| `filter_forceescape` | unsupported | HTML-only; output here is YAML |
| `filter_int` | gap | a value that is not a number fails instead of yielding the default; no `base` |
| `filter_items` | intended | a list or map renders JSON-style (`["a"]`, `{"k": 1}`; pairs as lists) where Jinja2 prints Python reprs (`['a']`, `('k', 1)`); the values are identical, both are valid YAML flow, and stacks already deploy the JSON spelling -- use `tojson` for portable text |
| `filter_join` | gap | no `attribute=` |
| `filter_list` | intended | a list or map renders JSON-style (`["a"]`, `{"k": 1}`; pairs as lists) where Jinja2 prints Python reprs (`['a']`, `('k', 1)`); the values are identical, both are valid YAML flow, and stacks already deploy the JSON spelling -- use `tojson` for portable text |
| `filter_map_filter` | intended | a list or map renders JSON-style (`["a"]`, `{"k": 1}`; pairs as lists) where Jinja2 prints Python reprs (`['a']`, `('k', 1)`); the values are identical, both are valid YAML flow, and stacks already deploy the JSON spelling -- use `tojson` for portable text |
| `filter_max` | gap | no `attribute=`; compares case-sensitively |
| `filter_pprint` | intended | a debugging aid: multi-line JSON-ish layout, not Python's pprint |
| `filter_random` | unsupported | refused by design: a deploy must render the same text every time |
| `filter_replace` | gap | no `count` |
| `filter_round` | gap | no `method`; ties round away from zero |
| `filter_select` | intended | a list or map renders JSON-style (`["a"]`, `{"k": 1}`; pairs as lists) where Jinja2 prints Python reprs (`['a']`, `('k', 1)`); the values are identical, both are valid YAML flow, and stacks already deploy the JSON spelling -- use `tojson` for portable text |
| `filter_selectattr` | intended | a list or map renders JSON-style (`["a"]`, `{"k": 1}`; pairs as lists) where Jinja2 prints Python reprs (`['a']`, `('k', 1)`); the values are identical, both are valid YAML flow, and stacks already deploy the JSON spelling -- use `tojson` for portable text |
| `filter_sort` | intended | a list or map renders JSON-style (`["a"]`, `{"k": 1}`; pairs as lists) where Jinja2 prints Python reprs (`['a']`, `('k', 1)`); the values are identical, both are valid YAML flow, and stacks already deploy the JSON spelling -- use `tojson` for portable text |
| `filter_striptags` | unsupported | HTML-only; output here is YAML |
| `filter_sum` | gap | no `attribute=`/`start=` |
| `filter_title` | gap | capitalises after `_` and `'` (`it's` -> `It'S`) |
| `filter_tojson` | intended | compact JSON with no HTML escaping (`{"a":1}`), the same text `to_json` writes and deployed properties carry; Jinja2 writes `{"a": 1}` and escapes `<>&'` |
| `filter_unique` | intended | a list or map renders JSON-style (`["a"]`, `{"k": 1}`; pairs as lists) where Jinja2 prints Python reprs (`['a']`, `('k', 1)`); the values are identical, both are valid YAML flow, and stacks already deploy the JSON spelling -- use `tojson` for portable text |
| `filter_urlencode` | gap | not implemented |
| `filter_urlize` | unsupported | HTML-only; output here is YAML |
| `filter_wordcount` | gap | not implemented |
| `filter_xmlattr` | unsupported | HTML-only; output here is YAML |
| `global_cycler` | gap | not implemented |
| `global_dict` | intended | a list or map renders JSON-style (`["a"]`, `{"k": 1}`; pairs as lists) where Jinja2 prints Python reprs (`['a']`, `('k', 1)`); the values are identical, both are valid YAML flow, and stacks already deploy the JSON spelling -- use `tojson` for portable text |
| `global_joiner` | gap | not implemented |
| `global_lipsum` | unsupported | refused by design: random text makes a render non-deterministic |
| `stmt_do` | unsupported | `{% do %}` works, but values are immutable: there is no `list.append`; use `namespace()` |
| `stmt_for_break_continue` | gap | `break`/`continue` refused (minijinja `loop_controls` off) |
| `stmt_for_dict_items` | intended | a map literal iterates in key order; Jinja2 keeps insertion order -- use a list of pairs when order matters |
| `stmt_for_dict_iter_keys` | intended | a map literal iterates in key order; Jinja2 keeps insertion order |
| `stmt_for_loop_index` | gap | iterating a string has no known length, so `loop.length`/`revindex` are undefined (strict error); `'abc'|list` works |
| `stmt_include_without_context` | gap | `without context` is ignored; the include still sees the caller's variables |
| `stmt_macro_kwargs` | unsupported | a macro cannot take keyword arguments it does not declare (`kwargs`) |
| `stmt_macro_varargs` | unsupported | a macro cannot take more positional arguments than it declares (`varargs`) |
| `test_callable` | gap | not implemented |
| `test_sequence` | gap | false for strings and maps |
| `ws_crlf` | intended | line endings are preserved byte for byte; Jinja2 rewrites CRLF to LF |
| `ws_trailing_newline_stripped` | intended | text with no Jinja syntax is returned exactly as written (the zero-copy fast path), keeping its final newline |
| `ws_two_trailing_newlines` | intended | text with no Jinja syntax is returned exactly as written (the zero-copy fast path) |

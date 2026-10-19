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
| `expr_dict_methods` | intended | a list or map renders JSON-style (`["a"]`, `{"k": 1}`; pairs as lists) where Jinja2 prints Python reprs (`['a']`, `('k', 1)`); the values are identical, both are valid YAML flow, and stacks already deploy the JSON spelling -- use `tojson` for portable text |
| `expr_filter_pipeline` | extension | pulumi-rs-yaml only; Jinja2 does not define the name |
| `expr_float_render` | intended | floats never use exponent notation (`1e20` renders `100000000000000000000.0`, Jinja2 `1e+20`) |
| `expr_list_render_strings` | intended | a list or map renders JSON-style (`["a"]`, `{"k": 1}`; pairs as lists) where Jinja2 prints Python reprs (`['a']`, `('k', 1)`); the values are identical, both are valid YAML flow, and stacks already deploy the JSON spelling -- use `tojson` for portable text |
| `expr_literal_dict` | intended | a list or map renders JSON-style (`["a"]`, `{"k": 1}`; pairs as lists) where Jinja2 prints Python reprs (`['a']`, `('k', 1)`); the values are identical, both are valid YAML flow, and stacks already deploy the JSON spelling -- use `tojson` for portable text |
| `expr_literal_floats` | intended | floats never use exponent notation (`1.0e-5` renders `0.00001`, Jinja2 `1e-05`) |
| `expr_literal_list` | intended | a list or map renders JSON-style (`["a"]`, `{"k": 1}`; pairs as lists) where Jinja2 prints Python reprs (`['a']`, `('k', 1)`); the values are identical, both are valid YAML flow, and stacks already deploy the JSON spelling -- use `tojson` for portable text |
| `expr_literal_tuple` | intended | tuples are lists in the engine, so `(1, 2)` renders `[1, 2]` |
| `expr_nested_data` | intended | a list or map renders JSON-style (`["a"]`, `{"k": 1}`; pairs as lists) where Jinja2 prints Python reprs (`['a']`, `('k', 1)`); the values are identical, both are valid YAML flow, and stacks already deploy the JSON spelling -- use `tojson` for portable text |
| `expr_str_methods` | intended | a list or map renders JSON-style (`["a"]`, `{"k": 1}`; pairs as lists) where Jinja2 prints Python reprs (`['a']`, `('k', 1)`); the values are identical, both are valid YAML flow, and stacks already deploy the JSON spelling -- use `tojson` for portable text |
| `expr_str_methods_predicates` | intended | a list or map renders JSON-style (`["a"]`, `{"k": 1}`; pairs as lists) where Jinja2 prints Python reprs (`['a']`, `('k', 1)`); the values are identical, both are valid YAML flow, and stacks already deploy the JSON spelling -- use `tojson` for portable text |
| `expr_str_methods_split_ws` | intended | a list or map renders JSON-style (`["a"]`, `{"k": 1}`; pairs as lists) where Jinja2 prints Python reprs (`['a']`, `('k', 1)`); the values are identical, both are valid YAML flow, and stacks already deploy the JSON spelling -- use `tojson` for portable text |
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
| `ext_test_endingwith` | extension | pulumi-rs-yaml only; Jinja2 does not define the name |
| `ext_test_startingwith` | extension | pulumi-rs-yaml only; Jinja2 does not define the name |
| `filter_attr` | intended | `attr` falls back to item lookup, so a map key is found; Jinja2 looks up Python attributes only |
| `filter_batch` | intended | a list or map renders JSON-style (`["a"]`, `{"k": 1}`; pairs as lists) where Jinja2 prints Python reprs (`['a']`, `('k', 1)`); the values are identical, both are valid YAML flow, and stacks already deploy the JSON spelling -- use `tojson` for portable text |
| `filter_dictsort` | intended | a list or map renders JSON-style (`["a"]`, `{"k": 1}`; pairs as lists) where Jinja2 prints Python reprs (`['a']`, `('k', 1)`); the values are identical, both are valid YAML flow, and stacks already deploy the JSON spelling -- use `tojson` for portable text |
| `filter_dictsort_case_sensitive` | intended | a list or map renders JSON-style (`["a"]`, `{"k": 1}`; pairs as lists) where Jinja2 prints Python reprs (`['a']`, `('k', 1)`); the values are identical, both are valid YAML flow, and stacks already deploy the JSON spelling -- use `tojson` for portable text |
| `filter_escape` | intended | HTML entities spelled `&quot;`/`&#x27;`/`&#x2f;` where Jinja2 uses `&#34;`/`&#39;`; equivalent HTML, and output is YAML, never auto-escaped |
| `filter_forceescape` | unsupported | HTML-only: forces HTML escaping, which output here never has (auto-escape is off: it is YAML) |
| `filter_items` | intended | a list or map renders JSON-style (`["a"]`, `{"k": 1}`; pairs as lists) where Jinja2 prints Python reprs (`['a']`, `('k', 1)`); the values are identical, both are valid YAML flow, and stacks already deploy the JSON spelling -- use `tojson` for portable text |
| `filter_list` | intended | a list or map renders JSON-style (`["a"]`, `{"k": 1}`; pairs as lists) where Jinja2 prints Python reprs (`['a']`, `('k', 1)`); the values are identical, both are valid YAML flow, and stacks already deploy the JSON spelling -- use `tojson` for portable text |
| `filter_map_filter` | intended | a list or map renders JSON-style (`["a"]`, `{"k": 1}`; pairs as lists) where Jinja2 prints Python reprs (`['a']`, `('k', 1)`); the values are identical, both are valid YAML flow, and stacks already deploy the JSON spelling -- use `tojson` for portable text |
| `filter_max` | intended | a list or map renders JSON-style (`["a"]`, `{"k": 1}`; pairs as lists) where Jinja2 prints Python reprs (`['a']`, `('k', 1)`); the values are identical, both are valid YAML flow, and stacks already deploy the JSON spelling -- use `tojson` for portable text |
| `filter_pprint` | intended | a debugging aid: multi-line JSON-ish layout, not Python's pprint |
| `filter_random` | unsupported | refused by design: a program must render the same text on every deploy, or every preview shows a change and every deploy replaces what it touches; a fixed choice belongs in config |
| `filter_select` | intended | a list or map renders JSON-style (`["a"]`, `{"k": 1}`; pairs as lists) where Jinja2 prints Python reprs (`['a']`, `('k', 1)`); the values are identical, both are valid YAML flow, and stacks already deploy the JSON spelling -- use `tojson` for portable text |
| `filter_selectattr` | intended | a list or map renders JSON-style (`["a"]`, `{"k": 1}`; pairs as lists) where Jinja2 prints Python reprs (`['a']`, `('k', 1)`); the values are identical, both are valid YAML flow, and stacks already deploy the JSON spelling -- use `tojson` for portable text |
| `filter_sort` | intended | a list or map renders JSON-style (`["a"]`, `{"k": 1}`; pairs as lists) where Jinja2 prints Python reprs (`['a']`, `('k', 1)`); the values are identical, both are valid YAML flow, and stacks already deploy the JSON spelling -- use `tojson` for portable text |
| `filter_striptags` | unsupported | HTML-only: strips markup and decodes HTML entities; a YAML program carries no markup to strip |
| `filter_tojson` | intended | compact JSON with no HTML escaping (`{"a":1}`), the same text `to_json` writes and deployed properties carry; Jinja2 writes `{"a": 1}` and escapes `<>&'` |
| `filter_unique` | intended | a list or map renders JSON-style (`["a"]`, `{"k": 1}`; pairs as lists) where Jinja2 prints Python reprs (`['a']`, `('k', 1)`); the values are identical, both are valid YAML flow, and stacks already deploy the JSON spelling -- use `tojson` for portable text |
| `filter_urlize` | unsupported | HTML-only: turns URLs into <a> tags; a YAML program has no HTML to produce, and output here is never HTML-escaped |
| `filter_xmlattr` | unsupported | HTML-only: builds HTML attribute strings; a YAML program has no HTML to produce |
| `global_dict` | intended | a list or map renders JSON-style (`["a"]`, `{"k": 1}`; pairs as lists) where Jinja2 prints Python reprs (`['a']`, `('k', 1)`); the values are identical, both are valid YAML flow, and stacks already deploy the JSON spelling -- use `tojson` for portable text |
| `global_lipsum` | unsupported | refused by design: placeholder text has no place in an infrastructure program, and it is random, so a render would never be repeatable |
| `stmt_for_dict_items` | intended | a map literal iterates in key order; Jinja2 keeps insertion order -- use a list of pairs when order matters |
| `stmt_for_dict_iter_keys` | intended | a map literal iterates in key order; Jinja2 keeps insertion order |
| `stmt_namespace_self_reference` | intended | a namespace attribute may not be assigned a value holding a namespace: printing one that contains itself recursed until the stack overflowed, so it is refused at the assignment |
| `ws_crlf` | intended | line endings are preserved byte for byte; Jinja2 rewrites CRLF to LF |
| `ws_trailing_newline_stripped` | intended | text with no Jinja syntax is returned exactly as written (the zero-copy fast path), keeping its final newline |
| `ws_two_trailing_newlines` | intended | text with no Jinja syntax is returned exactly as written (the zero-copy fast path) |

# Changelog

Releases before 0.5.25 are described in their release commits and in the
GitHub releases; this file starts here.

## 0.5.25

### The `str` evaluator is reachable from Python

`evaluate_str_invoke(token, args) -> dict | None` exposes the in-process `str`
evaluator the engine has used since 0.5.21. Static tooling can now resolve a
name written as `${sanitized.result}` — a bucket named from `str:replace`, say
— without loading a provider or reimplementing Go's `strings` and RE2
replacement templates and hoping the reimplementation agrees at the edges.

The token accepts every spelling a template may write and is canonicalized
exactly as the evaluator canonicalizes it: `str:replace`, `str:index:replace`
and the slashed schema form all reach the same function. `None` means "not
answered here", never the empty string: an unknown token, a missing or
non-string argument, a pattern this crate cannot compile. Only a non-`dict`
`args` raises, since that is a bug in the call rather than an unanswerable
invoke.

### The static literal resolver answers pure `str` invokes

`resolve_literal`, which feeds the dependency-graph and SQL-lineage exporters,
answered `None` for every invoke. A resource whose name is built with
`str:replace` therefore had no name in the exported graph, even though the
program determines the string completely.

`str` is the one package where answering is not evaluation: pure string
manipulation, no state, no I/O, no provider, already evaluated in process on
the deploy path, and every argument must itself resolve to a literal first. So
the resolver answers a `str` invoke whose arguments are all literals, including
chains through variables and through other invokes, and the `${var.output}`
form that reads one of its outputs. `matches` renders `true`/`false`; a bare
invoke (an object) and `split` (a list) are not scalars and stay unresolved.

Everything else is unchanged: every other provider's invoke, `fn::readFile`,
config values and resource outputs still resolve to `None`, and the module's
"never a guess" contract holds.

**Exporter effect — additive only.** `export_dependency_graph` gains entries in
`literal_properties`, and `export_sql_lineage` resolves dataset and table ids
that were previously dynamic; nothing that resolved before changes. A
preservation test pins the pre-existing surface against a fixed map.

### Tests

Seven new security tests (87 → 94) hold the boundary: `fn::readFile` is never
read, directly or as an argument; nine non-`str` tokens across five providers
are never evaluated; a config-backed argument yields no name; a 1 MiB subject
against `(a+)+b` stays linear; a 200-deep invoke chain does not overflow the
stack; and a resolved literal carrying template markup is emitted verbatim
rather than re-rendered.

`fuzz_resource_graph` now also builds a template from its input in which every
resource name derives from a `str` invoke — both `fn::` spellings, the long
form with `return:`, chained invokes, an object-valued argument and a mutual
cycle — so fuzzer-chosen patterns reach the new arms through the exporter. No
new fuzz target.

Criterion gains `resolve_literal_plain_variable_chain` (the untouched path,
kept as the regression guard), `resolve_literal_invoke_derived_name` and
`resolve_literal_wide_template_200_invokes`.

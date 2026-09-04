# Changelog

Releases before 0.5.25 are described in their release commits and in the
GitHub releases; this file starts here.

## 0.5.26

### A checkpoint index read from borrowed bytes

`index_checkpoint(data) -> {"shape": str, "entries": [(id, urn), ...]}` reads a
Pulumi checkpoint into the pairs it records, and `index_checkpoints(docs)` reads
a whole state backend's worth of them. A stack's checkpoint is the only record
of which physical resources that stack manages, so a tool asking "does another
stack already own this id?" has to read its siblings — and the answer decides
whether a resource may be deleted.

That makes the failure mode the whole design. The same deployment is written
two ways: `checkpoint.latest.resources` in the blob a backend stores, and
`deployment.resources` in what an export writes. A reader that knows only one
of them does not give a partial answer, it gives a confident empty one, which
to an ownership gate is indistinguishable from "owns nothing" and authorises
the delete. So both encodings are read, a document carrying both keys is
rejected rather than resolved in favour of one, and `version` is held to 3 — a
later version that relocates `resources` would otherwise deserialise into a
well-formed lie.

Every failure is named and raised, prefixed `Not a Pulumi checkpoint:`. There
is no input for which this answers with an empty result it is not sure about;
that is deliberately the opposite contract from `evaluate_str_invoke`, whose
`None` means "not answered here". `shape` distinguishes `"empty"` — a
`checkpoint.latest` that is absent or null, a stack that has never deployed —
from `"resources"` with no entries, a deployed stack that manages nothing.

Bytes in, `Cow` out: a JSON string carrying no escape is borrowed straight out
of the caller's buffer, and only the retained pairs become Python strings. A
37 KiB checkpoint of a hundred resources allocates one vector of pointer pairs
and nothing else. Two serde defaults would have punched holes in the refusal
contract and both are closed: a derived struct visitor also accepts a JSON
array, filling absent fields from their defaults, so `"latest": []` read as a
deployment managing nothing; and `borrow` covers `Cow<str>` but not
`Option<Cow<str>>`, which quietly copied every id.

`targets` keeps only the ids a caller asked about, each written in full or as
its leaf name alone, since a caller knows the name it declared rather than the
provider-assembled id. Omitting it keeps everything; an empty list keeps
nothing.

### Reads release the GIL

Both entry points detach for the parse — the single-document call too, and that
is the point of it. A caller reads a state backend from a many-threaded pool,
and a parse holding the GIL would queue every one of those threads behind one
another while the concurrency still looked fine from the outside. The
detach/re-attach pair costs microseconds against a parse two orders of
magnitude longer.

The batch runs on a rayon pool scoped to the call and capped at
`min(parallel, len)`, so a batch of three never starts thirty-two threads;
`parallel=0` asks for the machine's available parallelism, `1` is sequential,
and input order is preserved at every setting. Each document gets its own
result slot: one that is not a checkpoint yields
`{"error": "Not a Pulumi checkpoint: ..."}` rather than raising, so one bad file
in a backend never hides the rest, and a caller that must fail closed checks for
the key.

The graph and lineage exporters are unchanged; nothing here reads a file or the
network. The `_native.pyi` stub, which had drifted — `evaluate_str_invoke`
shipped in 0.5.25 with no entry — is now held to `__all__` by a test.

### Tests

Fourteen new security tests (94 -> 108) hold the refusal contract against
hostile bytes: truncated and non-JSON input, invalid UTF-8 inside a string, an
empty object, a version with no body, an unrecognised version, a `resources`
that is not a list, a `latest` that is a list, a nesting spiral and a bracket
bomb, 64 MiB of open brackets refused in under a second, duplicate ids kept in
order, hostile urn and id strings returned byte-for-byte, full-id and leaf-only
targets, an empty target set, and a bad document that fails only its own slot.
Two of them found the array-as-struct hole described above.

One new fuzz target, `fuzz_checkpoint` (18 -> 19), asserts more than "does not
panic": the reader is deterministic, an `Ok` implies the bytes scan as JSON,
filtering changes which entries survive and nothing else, and four batched
reads of one document agree with reading it once. Its first run drew out a
boundary worth stating: a byte that is not UTF-8 inside a field the reader
never returns is not an error, because that field is never decoded — the same
answer serde_json's own structural scan gives. The strings that do come back
are `str` and valid by construction, and invalid UTF-8 in a `urn` or an `id`
is still an error. The reader was right; the invariant was wrong, and both
halves are now pinned.

Four benches: `index_checkpoint_37kb` at 41.6 us, `index_checkpoint_filtered_37kb`
at 46.3 us, and 3,000 documents at 1.51 ms on eight threads against
6.76 ms sequentially.

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

# Changelog

Releases before 0.5.25 are described in their release commits and in the
GitHub releases; this file starts here.

## 0.5.32

### Three string filters a template may already be written against

`truncate`, `center` and `wordwrap` are Jinja2 filters. minijinja, which this
engine renders with, carries most of Jinja2's set and not those three, so a
template that names one fails its render — and because a render fault is
reported per file, the stack reads as unrenderable rather than as naming a
filter the engine lacks. A stack computing a resource id as
`{{ ... | truncate(60, False, "", 0) }}` stopped every one of its deploys on
`filter truncate is unknown`. Same shape as the `tojson` gap closed in 0.5.20:
the template was correct and the engine was short.

None of the three is what its documentation suggests. `truncate` counts its
`end` string INSIDE `length`, and carries a `leeway` that suppresses truncation
entirely for a string only a little over — so `truncate(10)` returns an
eleven-character string unchanged. `center` pads asymmetrically, and not in the
direction anyone would guess: `'x'.center(4)` is `" x  "` but `'ab'.center(5)`
is `"  ab "`. And `wordwrap` is not a greedy scan at all; Jinja2 delegates to
Python's `textwrap`, which splits a line into chunks first — whitespace runs are
chunks of their own, and a hyphen ends a chunk only when two letters precede it
or letter-hyphen-letter does, AND a letter follows — and then fills lines from
those. That rule is why `well-known` breaks at width 6 while `a-b-c-d` breaks
only after `a-b-`.

So none of this was implemented from the prose. A greedy `wordwrap` was written
first, agreed with the reference on realistic widths, and then disagreed on
hyphenated words and narrow ones — which is worse than not having the filter,
because a template would render differently here than under every other Jinja
toolchain and do it silently. Both `textwrap` stages are reproduced instead.

Performance is in the pass-through, because these run once per templated value
in a program and nearly always leave it alone. A subject inside the cap is
returned by MOVING the value through, so it costs no allocation and no copy of
its bytes, and `truncate`'s walk stops as soon as the string is known to be too
long: measured flat at 20.8–21.4µs across subjects from 8 characters to 256 KiB
under the same 60-character cap, a 32,000x range. `wordwrap`'s chunks borrow the
line, so no word is copied, and the chunk buffer is reused for the whole render
rather than allocated per line. No regex, no dynamic dispatch, no `unsafe`.

Two of the three could grow without bound from arguments a template chooses, and
both were found by asking the question rather than by an incident.

`center` allocates from its `width`. Unbounded, `center(10**18)` asked the
allocator for that many bytes and ABORTED THE PROCESS -- found by the security
suite before any of this shipped. A width whose padding would exceed 1 MiB is a
render error naming the template instead.

`wordwrap` is worse, because it amplifies: `wrapstring` is author-controlled and
goes in once per line, and the line count is the subject over `width`, so the
output is the product of two things a template picks rather than a function of
its own size. Measured: an 11 KiB template with `width=1` and a 1 KiB separator
produced 5 MB, an amplification of 452x, scaling linearly in both factors. The
output is now bounded at 8 MiB, checked as it grows rather than estimated, with
an error naming BOTH factors so an author knows which to change. 64 KiB of prose
at width 79 still wraps, which is the case the bound must not reach.

Neither `truncate` nor `end` can amplify: `end` is appended once, so the output
is bounded by the input plus it, and `length` and `leeway` are only ever compared
against. That audit is itself a test, so an argument added later has somewhere to
declare its bound.

Held to the reference rather than to an opinion: 2,992 differential cases —
every combination of 22 subjects (empty, whitespace-only, multi-line,
hyphenated, non-ASCII, CJK) against the parameter space of all three filters —
captured from the reference implementation and stored as a fixture, including
the 88 it REFUSES, which the engine must refuse too rather than answer. Seven
unit tests name the rules that behaviour rests on; six regression tests hold the
field expression and its precedence; eight security tests cover enormous and
negative widths, multi-byte subjects at every width, a 256 KiB subject, a
pathological hyphen run, and that wrapping never invents a character; one
integration test carries a computed id through to a registered resource input; a
new fuzz target found a flaw in its own harness and then ran 459,078 cases
clean; five benches, all measured with the subject passed through the context so
the numbers are the filter rather than minijinja's lexer.

Every suite was checked against an engine with the three filters unregistered,
and each one fails there.

## 0.5.31

### An escaped dollar is not a literal pair of dollars

`$$` has been the escape for a literal `$` since the interpolation parser was
written, and the parser collapsed it correctly. The guard in front of the
parser did not. `needs_interpolation_pass` — then named `has_interpolations` —
looked for `${`, stepped over `$$` on its way, and answered false for a string
whose only marker was the escape. Its caller reads that answer as "this is a
literal" and hands the source text through untouched, so `$${data()}` left the
engine with both dollars intact.

The strings this reached are the ones that name another system's placeholder
syntax and have to keep it: a data-quality rule whose SQL says
`FROM $${data()}` so that the scanning service substitutes the table, a price
written `$$100`, any value where `${` is text rather than a reference. Each
arrived at its provider in a spelling nobody wrote, and where the receiving
service parsed the value, it failed there rather than here — a data scan
answered `Syntax error: Unexpected "$"` at the column of the first dollar,
which is the same column the escaped form occupies, so the position said
nothing about which spelling had been sent.

The guard now admits a string carrying either marker, and says why in its
name: `${` is a reference to resolve, `$$` is an escape to collapse, and a
string with neither is the only kind that is already its own value. The scan
returns on the first marker instead of stepping past escapes, so it is also
shorter than the one it replaces; nothing else about the fast path changes,
and a string with no marker still allocates nothing.

Two tests disagreed about this and both were green, because nothing exercised
the composition: `parse_interpolation` was proven to collapse `$${not.interp}`
in one file while the guard was proven to turn that same string away in
another. The behaviour they compose to is now pinned at every level — the
guard, `parse_expr`, an evaluated registration, and the plan the Python
binding returns.

`has_interpolations` is renamed rather than kept beside the new predicate: it
had one caller, and its name is what made the wrong question look like the
right one.

Four unit tests on the guard and five on `parse_expr`, including the field
statement as a block scalar inside a sequence inside a nested map; one
integration test asserting the value a provider is registered with; four
security tests, covering collapse-exactly-once (so a collapsed `$$$${x}` is
never rescanned into a reference to `x`), the escape that precedes a real
reference, guard/parser agreement, and a 200k-dollar run that stays linear
(133 -> 137); the fuzz target now asserts the guard only turns away strings
the parser would return unchanged; one bench on the guard's worst admitted
case. One integration test rewritten: `test_dollar_dollar_is_literal` asserted
the defect and is now `test_dollar_dollar_escapes_one_dollar`.

## 0.5.30

### An include is any text the tree contains

The template loader served a file by the extension in its name — `.j2`,
`.jinja`, `.jinja2`, `.yaml`, `.yml`, and since 0.5.29 `.json` and `.sql` —
and answered every other name as if the file were not there. A `VERSION`
kept beside a container image, the one line a stack inlines to name the
image it deploys, was refused before the filesystem was consulted, and the
refusal read as absence. The list was never the boundary; containment is.
A candidate is served only when its own canonicalized path, symlinks and
`..` resolved first, lies inside the stack directory or the render root —
unchanged, and re-proven below. Within that tree an include is now any
UTF-8 text file up to `MAX_INCLUDE_BYTES` (1 MiB, the same bound the SQL
lineage reader puts on one statement, because both are the text a stack
keeps beside itself).

Two refusals replace the extension list, each distinct from absence and
none of them something `ignore missing` can hide: `include refused
[binary]` for a file inside the tree that is not UTF-8, and `include
refused [too large]` for one over the cap. The size is decided from the
file's metadata before any read, so an oversized file costs one `stat` and
never an allocation; the bytes of a served file are validated in place,
one allocation, the same as the read they replace. A file that exists
where the author named it answers at once, served or refused — the loader
does not fall through to the render root to serve a different file under
the same name. A directory named as an include is as absent as it always
was. `ExtensionNotAllowed` and its `[extension]` tag are gone.

What this does not change: rooted names are refused before a path is
built; a traversal or a symlink out of both roots is an escape and the
target's contents never appear in the error; a genuinely absent file is
still not found, by name; `readFile()` stays contained to the stack
directory alone. A template's own name resolves to the compiled template,
never to the loader, and a self-include is an error, not an absence. The
checkout credential a CI runner leaves in `.git/config` is reachable by
`fn::readFile` at deploy time already and by a workflow step regardless;
it is a workflow setting (`persist-credentials: false`), not a loader rule,
and no denylist is added here.

### Tests

Twenty new unit tests on the loader, most of them pinning behaviour that
existed and nothing tested: any text file inside the tree served (`.json`,
`.sql`, `VERSION`, `.txt`); the fleet's exact shape — a `{% set %}` around
an include from a stack three levels below the root, trimmed; a file at
the cap served and one byte over refused; a directory absent; a refusal on
the first candidate not retried against the root; the root candidate and
the stripped `../` candidate each serving on their own; an in-root symlink
served and an escaping first candidate not hiding a served second; an
empty render root meaning the stack directory alone; a root that does not
exist simply not searched; an included file compiled as a template, not
pasted; exactly one trailing newline of an included file dropped; an
include reading the caller's `{% set %}` and `{% import %}` names and a
`{% set %}` inside it visible after; an import reading the parent's names
and exporting only its own; `with context` accepted and changing nothing;
a self-include an error, not an absence; extras never overriding built-in
names (deterministic, where only a fuzz target asserted it); CRLF
surviving `readFile` marker resolution (the 0.5.23 fix, untested until
now); a `base64_decode` failure an error, not an empty string. Three
existing tests rewritten to say what is now true, one integration test
renamed for the same reason (`test_include_serves_any_text_file_inside_the_root`),
one added for a nested stack reading a `VERSION` from the repo root. Four
security tests (130 -> 133): a binary include refused with its bytes absent
from the error, a file over the cap refused by name, a refused include
never ignored as missing, and an extensionless escape still an escape. The
fuzz target reads any input back as a refusal detail without panicking and
requires every refusal to carry one of the four tags. One bench,
`jinja_preprocessor_include_at_cap`, pins the largest served include.
Four Python tests: a `VERSION` served, an oversized include refused by
name, the refusal rewritten to the binary case, and a render proven to
release the GIL by a thread that keeps counting while it runs; two
integration tests drive the nested-stack include through the Python
surface, served inside the root and refused as an escape outside it.

## 0.5.29

### A template that will not render says where, and why

A render that failed reported its line and a column of `0`, a message that
ended by repeating the file and line it had just been given, and — for an
include — a sentence that was not true. The diagnostic carried a copy of
the source line and a suggestion; the column was hardcoded, the expression
that failed was never named, and the loader answered every refusal with
the same `None` it gave a file that was not there.

The diagnostic is now located. `minijinja` attaches the byte range of the
failing expression to every error it raises, whatever the debug setting;
`build_render_diagnostic` reads it, finds the line it falls on in the text
the engine compiled, and takes the expression as a slice. The slice is then
checked against the same offset of the original source line, and only
where the two agree does the diagnostic carry a column, an end column and
an expression — both borrowed from the source, as the line already was.
Where they disagree, or no range was attached, the column is `0` and the
expression empty, which is what every caller read before. The message is
the fault alone: the `(in <name>:<line>)` suffix `Display` appended is
gone, because the caller already holds all three. `format_rich` prints the
column, and a caret row under the expression, measured in characters so
it lands where the eye does.

### An include that is refused says it was refused

The loader would not serve a name whose extension it did not know, an
absolute path, or a file that resolved outside both roots — and for all
three it returned `Ok(None)`, the answer for a file that does not exist.
The render then said `tried to include non-existing template`, and the
author went looking for a file that was sitting where they had put it.

Three refusals are now three errors, each naming its cause, and each is
distinct from absence. A loader cannot carry a reason inside a
`TemplateNotFound`: the VM folds that kind into its own message. It returns
verbatim any other kind, so a refusal travels as the detail of a
`BadInclude` — `include refused [extension]: "notes.txt"`,
`[absolute]`, `[escape]` — and the classifier reads the tag back into a
`JinjaTemplateNotFound` with the remedy for that cause. A file that is
simply absent still answers `Ok(None)`: the VM names it, and
`{% include "x" ignore missing %}` keeps its meaning — for absence only.
A refusal is not something `ignore missing` can hide; a path that escapes
the sandbox is not a missing file.

`.json` and `.sql` join the extensions the loader serves. The boundary was
never the extension list; it is containment, unchanged: canonicalize, then
`starts_with` on the two roots, so a traversal or a symlink out of the tree
is refused as an escape and the file's contents never appear in the error.
A schema or a query is exactly the text a stack keeps beside itself, and
`readFile` — contained to the stack directory alone — could not reach a
sibling directory's copy of it.

### One body, two surfaces, no GIL

`JinjaPreprocessor::render` is the render, with the diagnostic's lifetime
tied to `source` alone; the trait method delegates to it. The trait spells
its error as `Err<'src> where Self: 'src`, which also binds the diagnostic
to the preprocessor's context — and a caller that builds that context on
the stack could never hand the diagnostic out without copying the source
into it. The binding does exactly that, so the bound is lifted at the one
place it mattered.

`preprocess_jinja_diag` answers in structure: `{"rendered": str}`, or
`{"diagnostic": {kind, line, column, end_column, message, source_line,
expression, suggestion}}` — the same facts `preprocess_jinja` folds into
its error text, as fields, so a caller can place a caret rather than parse
a sentence. Both entry points share `render_jinja`, and both release the
GIL for the render: the context dictionary is read under it, everything
after borrows, and a caller's other threads keep going while a template
renders. `preprocess_jinja`'s signature is unchanged; its error text gains
the column and the caret row and loses the redundant suffix.

### Tests

Nineteen new unit tests on the diagnostic (the caret in bytes and in
characters; an undefined name, a failed subscript and a filter error
located to the character on the first, last and a multi-byte line; the
line and expression proven to borrow the source by pointer range; a range
the source disagrees with, and an error with no range, both degrading to
no column; a message with no detail; `.json` and `.sql` served; each
refusal distinct; `ignore missing` still ignoring absence and never a
refusal; the trait and the inherent render agreeing on every input).
Seven new security tests (123 -> 130): an escaping `.json` refused and its
contents absent from the error, an absolute include refused inside the
root, a disallowed extension refused whether or not the file exists, a
symlink out of both roots refused as an escape, a genuinely absent include
not found by name, a fault on the last line of sixty-four mebibytes
borrowing rather than copying, and multi-byte prefixes keeping the caret
honest. `fuzz_jinja` gains a strict render with the location properties —
no new target, so the matrix guard is unchanged at nineteen. One new
bench, `jinja_preprocessor_render_fails_undefined`, pins the diagnostic's
cost; the render benches are unchanged. Eight new Python tests for the
structured surface and the string surface's new text.

## 0.5.28

### An id that belongs to the parent

Some providers give every member of a parent's array the parent's own id. A
dataset's access list is the clearest case: each entry is a resource in its
own right, with a URN and a place in a checkpoint, and its id is the dataset's
path — because the API has no smaller name for it. An index keyed on ids
therefore reports that two stacks manage "the same" resource when they manage
two different elements of one array, and a gate reading that index refuses a
removal it should have allowed. The distinction is real; it is just not in the
id. It is in the resource's declared `inputs`.

`index_checkpoint_with_elements` reads it out. A caller names the resource
types it cares about and, per type, the top-level `inputs` keys that make up
one element — that is `ElementSpec`, built once per scan like `IdFilter` — and
each entry of a named type comes back with those keys projected into
`Entry::element`. Everything else is the scan it always was: an unnamed type
keeps its `inputs` unread, an unnamed key inside a named type is stepped over
without being captured, and `index_checkpoint` itself, which passes no spec,
returns exactly what it returned in 0.5.27. `Index::entries` is now a
`Vec<Entry>` rather than a `Vec<(id, urn)>`; that is the one signature that
moved, and `Entry` carries the same two strings under the same names.

### Captured, not parsed

The reader still does not build a value for any field it does not return.
`inputs` is captured as a `RawValue` — a borrowed slice of the caller's
buffer, taken for every resource because JSON puts no order on `type` and
`inputs` and the decision to project can only be made once both are in hand.
Capturing costs a UTF-8 check and a structural scan, both of which
`serde_json` was already performing; it allocates nothing. The projection
runs only for a resource whose type was asked for, after the id filter has
had its say, and reads keys through the same borrowed newtype the ids use —
a `HashMap<Cow<str>, &RawValue>` would have been shorter and would have copied
every key, because serde's borrow special-case does not reach map keys. The
values stay raw. A caller comparing two checkpoints compares text; the Python
binding parses each kept value once, on the way into a dict, through the same
converter every other value on that boundary uses.

One boundary moved and is pinned by a test on each side: a byte sequence that
is not UTF-8 inside `inputs` — or inside `type`, which is now read — is an
error, where before it was invisible. A checkpoint whose inputs are not UTF-8
is corrupt, and this is the direction to be wrong in. `outputs` and
`dependencies` stay unread, and a bad byte inside them stays invisible.

The Python surface gains one keyword on `index_checkpoint` and
`index_checkpoints`, `element_types`, a mapping from type token to key list.
Given it, entries are `(id, urn, element)` throughout — a dict for a resource
that carries an element, `None` for one that does not, and never a shape that
changes per row. Omitted, entries are the `(id, urn)` pairs they have always
been, so a caller written against 0.5.26 or 0.5.27 sees no difference at all.

### Tests

Thirteen new unit tests on the reader (elements returned, told apart, absent,
empty, borrowed rather than copied; unrequested types unread; the filter
before the projection; a batch agreeing with a single read). Seven new
security tests (116 -> 123): an element only for a type that was asked for,
hostile values passed through and hostile keys never matched by accident, an
element that cannot be projected is an error and not an empty one, a bomb
inside `inputs` costs what skipping it always cost — serde_json's ignore path
is iterative by design, so the scan is linear and is compared with serde_json's
own pass over the same bytes rather than with a clock — a batch projecting
identically on every thread, and the UTF-8 boundary above. `fuzz_checkpoint`
gains the projection properties — same entries, subset of the requested keys,
every value a slice of the input, a spec naming nothing is the scan that asked
for nothing — with no new target, so the matrix guard is unchanged at
nineteen. Two new benches: the 37 KiB document read by a scan that asks for
an element it does not contain, which is the cost every resource pays and must
stay next to the unfiltered read; and a hundred entries that all carry one.
Twelve new Python tests for the binding's shapes. `raw_value` is enabled on
`serde_json`; no other dependency changes.

## 0.5.27

### A byte order mark is not a second document

A `Pulumi.yaml` that begins with a UTF-8 byte order mark — three bytes an
editor writes and nobody types — did not parse. The error said
`deserializing from YAML containing more than one document is not supported`,
which is a complaint about a defect the file does not have, and the template
came back with no name at all, so everything keyed on the project name
described a project called `unknown`.

The mechanism is worth stating, because the message points away from it.
`serde_yaml` sits on libyaml, which skips a leading mark — but skips it as a
character, advancing the scanner's column to 1. The first key therefore opens
the root block mapping at indent 1; line 2's key at column 0 is shallower, so
it unrolls that mapping and closes the document, and every remaining line
starts a second implicit one. Hence the signature that made this so hard to
read: a one-line file parsed, a file of two lines or more did not, and the
reason named was never the reason. Through the Jinja path the same three bytes
surfaced instead as `found character that cannot start any token`.

YAML 1.2 permits a mark at the start of a stream, so the file was legal and
this is a correctness fix rather than a workaround. `encoding::strip_bom` and
`strip_bom_bytes` remove exactly one mark, at offset zero, and return a
subslice of their argument — no allocation and no copy on any parse. A mark
anywhere else is content and comes back byte for byte. UTF-16 marks are left
alone: those are a different encoding, not a leading `U+FEFF` in a UTF-8
stream, and dropping the two bytes would hand the parser a NUL-riddled buffer
that fails further from its cause.

The strip is silent. The input conforms to the spec, so there is nothing to
warn about, and there is no honest span to attach to bytes the spec says are
not part of the document. Removing the mark from the file on disk belongs to
whichever tool owns the file.

### Six boundaries, not every read

The strip happens where text becomes a value, not where a file is read: the
reported case arrived as a `str` from a caller that never touched a file, and a
read-site fix would have missed it entirely. Six deserializer entries, each the
single funnel for a family of readers — `parse_template`, which every reader in
the workspace reaches; `validate_rendered_yaml`, which runs before it on every
Jinja path; `try_parse_package_lock`, whose `.ok()?` had made a marked lock file
invisible rather than rejected; `SchemaStore::load`, on bytes, because a 56 MB
provider schema should not pay for a UTF-8 pass to lose three bytes;
`packages_from_source`, where a mark makes the first key `\u{feff}runtime` and
the provider scope is lost with no error at all; and the `exec` wrapper's
pre-spawn gate, which has to accept what the runtime accepts.

`strip_jinja_blocks` and `provider_scope`'s `protect`/`restore` are deliberately
untouched: their output is written back over the user's file, and
`fuzz_scope_roundtrip` asserts they hand every byte back. Every public signature
from 0.5.26 is unchanged, and the Python `__all__` gains nothing — every symptom
on that side, the `unknown` project name included, is fixed transitively.

Shadowing rather than stripping inline where a diagnostic is built turned out to
matter more than the parse. A mark did not merely break a file; it collapsed
*every* error in that file into the same locationless phantom. With the strip in
place, a marked file with a genuine indentation error is reported at its line
and column again.

### Tests

Eight new security tests (108 -> 116). The one that carries the design is the
anti-masking case: a file that genuinely holds two documents holds two after the
mark is removed, and is refused with byte-identical wording either way — a strip
cannot hide a real defect, it can only stop inventing one. Around it: the
reported repro at one line and at several; a mark alone, which must fail as
"expected a YAML mapping" rather than as a phantom second document; a mark
inside a scalar, which survives into the value; a doubled mark, where exactly
one is a stream marker; a UTF-16 mark, diagnosed rather than guessed at; a
marked package lock; a marked schema store; and a marked project loaded with and
without a Jinja context.

Every fixture that must carry a mark is written as raw bytes. A mark is
invisible, and a formatter, an editor save or a helpful string literal would
normalise it away, leaving tests that pass while proving nothing — the file
fixtures assert their own first three bytes before the test that depends on them
runs.

`fuzz_yaml_parser` gains the differential property rather than a new target
(nineteen, unchanged): the same bytes with and without a leading mark are the
same document, and must parse to the same template and the same diagnostics.
Stated that way it is two-sided — it fails if the mark is not removed, and
equally if removing it changes anything else.

Eleven in-module unit tests, two of them address-level: unmarked text is handed
straight back (`ptr::eq`), and a stripped slice starts three bytes into its
input. Four Python tests write their fixtures with `write_bytes`. The bench pair
`parse_simple_template` and `parse_simple_template_with_bom` measures 7.07 us
against 6.91 us — the strip is a three-byte comparison, and the pair shows it
costs nothing measurable.

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

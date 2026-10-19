# Changelog

Releases before 0.5.25 are described in their release commits and in the
GitHub releases; this file starts here.

## 0.5.40

Jinja is now a measured, bounded, documented capability. **[JINJA.md](JINJA.md)**
is the reference; every example in it is executed by the test suite.

### Conformance with reference Jinja2

A corpus of 345 cases -- every statement, expression form, whitespace rule
and error class, and one case per filter, test and global Jinja2 3.1.6 ships
-- renders through the runtime and must equal pinned Jinja2's output byte for
byte. 286 do; the 59 that differ are listed with reasons in
`tests/jinja_conformance/DIVERGENCES.md` (value formatting deployed stacks
already depend on, extensions, refusals by design), with the runtime's own
output pinned. `MATRIX.md` is generated from the corpus and both registries.
A CI job regenerates the goldens from the hash-pinned reference
(`scripts/jinja_conformance/`, the only use of Python Jinja2) and fails on
drift; the Python binding runs the same corpus.

Gaps closed: `int`/`float` defaults and `base`, `round` methods and
ties-to-even, `replace` count, `attribute=` on `join`/`sum`/`max`/`min`,
`sum` start, case-insensitive `max`/`min`, positional `dictsort`, Jinja2's
`title`; new `wordcount`, `filesizeformat`, `urlencode`, `callable`,
`cycler`, `joiner`, Python string methods, and `break`/`continue`;
`sequence` is true for strings and maps. Rendering 4,583 real project files
before and after gives byte-identical output.

### Python methods on values

Every `str`, list, dict and number method a template reaches for -- case
mapping, strip/replace, find/index/count with bounds, split/rsplit/
splitlines/partition, the padding family, the `is*` predicates,
removeprefix/suffix, join, and `str.format` with the format-spec
mini-language -- from one static, sorted table per receiver kind (no
allocation to dispatch, no trait objects). 0.5.39 had `items`, `keys`,
`values` and `get`. A method that can grow a value checks the output limit
before allocating. 73 method cases are held three ways: to pinned Jinja2, to
this runtime's own Starlark, and to each other (`methods/DIVERGENCES.md`
lists the Starlark gaps and two sorted-map differences).

### Python's arithmetic, `%` formatting, varargs and isolated includes

No known behavioural gap against Jinja2 remains:
- `/`, `//` and `%` by zero are a `ZeroDivisionError`-class render error
  (minijinja rendered `1 / 0` as `inf`); `%` takes the divisor's sign
  (`7 % -3` is `-2`) and `//` floors;
- `'%s-%05.2f' % (a, x)` and `'%(k)s' % d` are Python's printf formatting
  (`s r d i u x X o c f F e E %`, flags, width, precision), through the same
  core as `str.format` and bounded by the output limit -- which also now
  bounds a `str.format` precision. The `format` filter is that core too:
  minijinja's own sliced its input by bytes and panicked on a multi-byte
  character in a `%(...)` key (found by fuzz_jinja_conformance);
- a loop over a string knows its length (`loop.length`, `revindex`, `last`);
- `{% include x without context %}` renders `x` with none of the caller's
  values (it used to pass them), with its own bounded nesting depth;
- a macro naming `varargs` or `kwargs` takes extra arguments into them
  (including through `call` and imports); one that does not still refuses.

`*` and `**` are Python's too: a string or list times a count repeats it
(nothing for a count of zero or less), and an integer to a negative power
is a float, `0 ** -1` a `ZeroDivisionError`.

What stays different is by design: random/lipsum (a deploy must render
repeatably), the HTML filters (output is YAML), and the value formatting
deployed stacks already depend on.

### Lists and dicts a template can change

`{% do xs.append(x) %}`, `d.update(...)`, `setdefault`, `pop`, `sort`, and
the rest of the list and dict mutators, with Python's sharing. In a template
that mutates, its literals and filter results are mutable objects; values
the host passes in stay read-only (an error says to copy them); changing a
container while a loop walks it is an error; growth is bounded by fuel.

### Dates

`date`, `datetime` and `timedelta`, naive, in pure Rust (proleptic
Gregorian, no timezone database), with `to_date`/`to_datetime`/`strptime`,
`strftime`, `isoformat`, attributes, comparison and `+`/`-`, checked against
Python's `datetime`.

### A capability ledger

`tests/capability_ledger.toml` maps every changelog entry since 0.5.25 to
the tests that prove it, and a test fails when one is unmapped or names a
test that no longer exists. Five entries had no test and now do (the
reused SQL parser worker, Starlark's load-time execution, and three CI
settings read from the workflow files).

### Resource limits

Every render runs under `JinjaLimits`: fuel 100,000 instructions (the
heaviest real file measured used 6,575), recursion 500, output 16 MiB
(`readFile()` included), `range()` 100,000 elements. Each refusal is its own
`RenderErrorKind` -- `FuelExhausted`, `RecursionLimit`, `OutputTooLarge`,
`IncludeCycle`, `RangeTooLarge` -- at the offending line, never a hang or an
out-of-memory. Override with `PULUMI_YAML_JINJA_LIMITS=fuel=..,recursion=..,
output=..,range=..` or the Python `limits=` argument; a malformed value is
refused by name. `render_with_stats` reports the fuel used.

### Every builtin, held to hostile counts and widths

The fuzzer found minijinja's `divisibleby` test computing a remainder
unchecked: `9 is divisibleby(false)` panicked, which in a release build is
a process abort. An exhaustive audit followed -- every filter, test,
method, global and operator, called with zero, `False`, negatives and the
i64 and i128 extremes, 1.13 million calls -- and found three more of the
class: `tojson(indent)` and `indent(width)` allocated their width unchecked
(an allocation abort), `[] * 9223372036854775807` spun for ever, and
`slice(9223372036854775807)` was a capacity panic. Each now answers as
Python does or refuses with a typed error:
- `divisibleby` is `value % num == 0`, so a zero divisor is a
  `ZeroDivisionError`, as in Jinja2;
- `slice` and `batch` are Jinja2's (a zero `slice` count raises, a
  negative one yields nothing, `batch(0)` is `[[], [...]]`); the number of
  slices is bounded by the range limit;
- a repeated list is bounded by the range limit and a repeated string by
  four times the output limit (the formatter, which knows the line, refuses
  one that is written); an empty repeat answers at once;
- `tojson` and `indent` refuse a width past the output limit and stop
  their text growing past it;
- `round` with a huge precision answers as Python does (`round(2.5, 10**9)`
  is `2.5`, `round(2.5, -10**9)` is `0.0`), where it formatted a
  billion-digit string; `ceil`/`floor` raise Python's errors there;
- `i128::MIN // -1` is exact and `i128::MIN % -1` is `0`, where the
  machine division overflowed; a float past 2**127 through `int` is
  refused, never saturated to a wrong number;
- `wordwrap` refuses a width below 1 and `truncate` reports the length it
  was given, as Jinja2 does.

Every input is a conformance case, a security test and a fuzz seed.

`dictsort` is Jinja2's own (stable, Python's reverse, an unknown `by`
refused with Jinja2's message), and an operator the prepass routes answers
for non-numbers by the engine's rules and messages without compiling an
expression per call; a joined list is a list, so it can be indexed.

The binary stays inside the +2% size gate on every artefact with all of
this: the render environment starts empty and registers only the engine
builtins that are not replaced (the replaced ones no longer ship, unused),
filters register through one closure per signature, and `list.sort`,
`dictsort` and the prepass sort through the engine's existing sort of
values rather than instantiating their own.

### Loop controls and `cycle` in the places Jinja2 refuses them

Five-minute fuzz runs found two more engine panics. `loop.cycle()` with
nothing to cycle computed `index % 0`; it is now Jinja2's `TypeError` (no
items for cycling given) when the call runs. A `continue` or `break` in a
loop's `else` -- which runs after the loop -- inside a macro defined in a
loop was accepted, and popped a frame that was not a loop's; Jinja2's
compiler refuses a loop control with no loop of its own to act on (a loop's
`else` and a macro's body are outside the loop around them), and so does
this one now, as a syntax error at the line.

### Recursion reaches its limit on any stack

Windows gives a main thread 1 MiB of stack, and the language host
overflowed it ("has overflowed its stack") before a self-recursive macro
reached the recursion limit -- an abort where a typed error belongs.
Rendering now runs on the engine's deep-stack worker, the 64 MiB thread SQL
parsing already used; the two share it rather than each keeping one. A
render crosses to it as owned data and its diagnostic comes back re-sliced
from the caller's source, so no signature changed. A test renders to the
limit from a 1 MiB thread.

### One error, one kind

A refused include whose name contained the text `readFile:` was reported as
a readFile failure: the classifier matched that tag anywhere in the message,
and a refusal quotes the name it was given. Tags are now matched where they
are written, at the start (found by fuzz_jinja).

Errors raised inside an expression the prepass rewrote -- `/`, `%`, `*`,
dates -- now carry their column again: the rewrite records where it changed
the text and the diagnostic maps back through it, so the caret is under what
the author wrote. An undefined operand there is an undefined variable, not
a type error.

### Two engine crashes, found by fuzzing

minijinja 2.24 panics on a negative-step slice of an empty list or string
(`{{ [][::-1] }}`), and overflows the stack printing a namespace that holds
itself (`{% set ns.v = ns %}`); release builds abort, taking the language
host or the Python process with them. Backwards slices are now computed as
Python computes them; a namespace assigned a value holding a namespace is
refused at the assignment. minijinja is pinned to `=2.24.0` (the parser this
needs is not semver-stable).

### Errors

A fault inside an included or imported template names that file, line and
column, with its excerpt and the include chain (`origin` on
`RenderDiagnostic`, and in `preprocess_jinja_diag`); the excerpt was missing
from release builds. Errors are classified by their root cause; an undefined
name is named (`undefined value: \`x\``); `YamlDuplicateKey` is produced
again. The language host removes secret config values from error text. Every
kind's exact text is golden-tested.

### Policy and hardening

- Every render is strict; `PULUMI_YAML_JINJA_UNDEFINED` takes `strict` or
  `passthrough` and refuses anything else (it silently read typos as
  strict). `graph` honours it as the host does.
- `tojson`/`to_json` escape the characters YAML misreads (DEL, C1 controls,
  U+0085, U+2028/2029, BOM), so a quoted value always round-trips.
- Include names holding control characters are refused (`[bad name]`);
  containment is proven against every spelling of "outside".
- Auto-escaping is stated (none: output is YAML); the `debug()` global,
  which dumped the context into the output, is removed.

### Tests

Unit (limits at their boundaries, policies, prepass, method tables, the
calendar), conformance (292 cases and 73 method cases, Rust and Python; the
methods also against Starlark), integration (`host-jinja-features` deployed with a
real `pulumi up`; one stack per limit and an include chain through `graph`
and `pulumi preview`), security (DoS shapes, containment, engine crashes,
secret redaction), fuzz (`fuzz_jinja_template`, `fuzz_jinja_loader`,
`fuzz_jinja_conformance`, `fuzz_jinja_methods`), regression (the
capability ledger), performance (typical stack, large loop,
include-heavy benches; fuel counted, not timed).

## 0.5.39

### Already-rendered projects: `PULUMI_YAML_JINJA=off`

A caller that renders a project with this crate and hands the language host
the result could not deploy text a `{% raw %}` block produced: the host
rendered the file a second time and read the literal `{{ x }}` as an
expression ("undefined value"). `PULUMI_YAML_JINJA=off` turns the runtime's
own pass off: the language host, `exec` and `graph` parse every project file
as written. Unset, empty or `on` is the old behaviour, byte for byte; any
other value is refused with the value named; `off` with `exec`'s
`PULUMI_YAML_JINJA_SOURCE` is refused as a contradiction. New:
`jinja::JINJA_ENV`, `jinja::jinja_enabled`, `jinja::jinja_enabled_from`.
Proven through a real `pulumi up` on a local backend
(`tests/acceptance/host-prerendered-raw`).

### Resource keys for three more shapes

- `gcp:cloudrun:Service` (Cloud Run v1): the provider id
  `locations/{l}/namespaces/{p}/services/{s}` keys as the v2 service it is,
  `//run.googleapis.com/projects/{p}/locations/{l}/services/{s}`. A namespace
  that is a project number gives no key.
- `gcp:bigquery:Job`: the provider id `projects/{p}/jobs/{j}` holds no
  location. `resource_key_with_properties(type, provider_id, props)` (new,
  Python too) takes it from the `location` property, giving the key
  `bq_job_resource_key` builds; no location, no key.
- `gcp:serviceaccount:Account` from properties: `accountId` + `project` give
  `{accountId}@{p}.iam.gserviceaccount.com`, the same key as the provider id.
- `KeyTemplate` gains `id_form` (`IdForm`), how a type's provider id spells
  its relative name. Every id segment is still validated before it is
  written; a property-built key is held to the same rule.

### Statement splitting: faster, and no longer at the mercy of code layout

`split_statement_spans` dispatched on every byte of a script, and its speed
depended on where the compiler placed that loop: on x86-64 runners the same
machine code measured 1.36 ms per MiB in one build and 1.87 ms in another
after unrelated code moved it. A plain byte now costs one table lookup, and
quotes and comments are crossed by a search for their terminator (inline for
short spans, `memchr` beyond). 1 MiB of dense statements: 1.36 to 1.12 ms on
AMD EPYC 7763, 1.24 to 0.99 ms on Intel Xeon 6, 1.58 to 0.85 ms on Apple
M-series; 1 MiB of long literals and comments now takes 0.17 to 0.23 ms
(new bench `split_statement_spans_1mib_long_literals`). Output is unchanged:
checked against the previous splitter on 200,000 random scripts.

### README badges are checked

- The fuzz-targets badge said 26; 25 targets are registered.
  `fuzz/fuzz_targets/scope_grammar.rs` is a shared generator, not a target,
  and 0.5.38 counted files. The README's target list also missed five
  targets and said "nineteen".
- The fuzz workflow's registration check now also fails when the fuzz badge
  differs from the `[[bin]]` count in `fuzz/Cargo.toml`, when a registered
  target is missing from the README's Fuzz section, or when the
  security-tests badge differs from the `#[test]` count in
  `security_tests.rs`. It runs on pull requests that touch any of them;
  before, it ran only on the weekly schedule and never read the README.

## 0.5.38

### Where a referenced stack really is

A graph's `stack_uri` was built from the identity the program declares, and a
StackReference's `referenced_stack_uri` guessed that the other stack lives on
the same backend. Both are wrong when a host deploys under another identity or
keeps one state prefix per stack. All additive, all optional; without them the
output is byte-identical to 0.5.37.

- `GraphExportOptions::project_override` / `stack_override` (Python
  `project_override=`, `stack_override=`): the identity the stack is deployed
  as. Every id and URI uses it.
- `reference_backends` (Python `reference_backends={name: backend}`): the
  exact backend of each referenced stack. Such a reference carries
  `"resolved": "exact"`; one not in the map `"resolved": "literal"`.
- On a self-managed backend (`gs://`, `s3://`, `azblob://`, `file://`) there is
  no organization, so every stack mints `stackoutput::organization/...` ids
  whatever organization a caller passed, and the "organization not set"
  warning is gone there. Exporter and consumer ids now line up by
  construction. Pulumi Cloud is unchanged.
- `cross_stack::resolve_cross_stack` (Python `resolve_cross_stack([graph,
  ...])`): joins every `consumes_stack_output` edge to the resources the
  exporting stack's `exports` edges name. Linear, borrowed, sorted.

### One backend resolver

The CLI read `PULUMI_BACKEND_URL`; the Python binding did not. Both now call
`stack_uri::discover_backend`: the argument, `PULUMI_BACKEND_URL`, the
project's `backend.url`, then the Pulumi workspace's current backend
(`current` in `$PULUMI_HOME/credentials.json`; nothing else in it is read).
The graph carries `backend_source` (`argument`, `env`, `project`,
`workspace`) when one applied. `backend_url=""` still turns every fallback
off. **Behaviour change:** a Python caller that passes no backend now gets
URIs when the environment or workspace names one, as the CLI always did.

### Host-supplied plaintext config

`GraphExportOptions::config` and `SqlLineageOptions::config` (Python
`config={...}` on all three exporters) take the stack's plaintext config. A
name that is not a variable then falls back to it exactly as the deploy reads
it: `${k}`, `${proj:k}` and `${gcp:k}`; `{project}:{key}`, then `{key}`, then
`value:`, then `default:`; parsed by the declared type with the deploy's own
parser. A `secret: true` key never resolves, the binding refuses a
`{"secure": ...}` value, and a config-sourced literal is listed in the node's
`config_properties`. Top-level scope only. Without `config` nothing reads
config, and the two tests that pinned "config is never a literal" now pin it
"without host config".

A fixture settles that a `Pulumi.<stack>.yaml` beside the program is never
read as a resource file: with `config:` and nothing else it is skipped whole,
and with only `encryptionsalt:` or `config: {}` it merges nothing.

### IAM graph

`iam_graph::export_iam_graph` (Python `export_iam_graph`): `principal` and
`role` nodes and `has_role` edges to the resource each role is held on, from
every `*IAMMember`, `*IAMBinding` and `gcp:bigquery:DatasetAccess`. A member
that is a resource output is an edge from that resource. `summary` accounts
for every member (`member_resolved`, `member_from_config`,
`member_from_resource`, `member_unresolved`). A projection of the infra walk,
so its URNs are the infra graph's.

### Resource keys

`resource_key::resource_key(type, provider_id)` gives
`//{service}.googleapis.com/{relative-name}` for BigQuery datasets, tables,
routines and transfer configs, Workflows, Cloud Scheduler jobs, Cloud Run
services and jobs, buckets, Pub/Sub topics and subscriptions, Dataproc
batches, Batch jobs and service accounts; `resource_key_from_properties` builds
one from literal properties; `bq_to_resource_key` maps a lineage id or SQL
reference onto the same key; `bq_job_resource_key` keys a BigQuery job. The
table is in `GRAPH.md` with its SQL. Lineage ids now accept a domain-scoped
project (`example.com:proj.d.t`), which used to fail to parse.

### CI

The `binary-size` job now FAILS when any artefact -- both host builds, the
converter, the Python extension -- grows more than 2% against the previous
release, built in the same job.

### Tests

- Unit: config resolution (types, namespaces, value/default, secrets,
  shadowing, invoke arguments), overrides, reference backends, organization,
  backend order, credentials file, cross-stack join, IAM shapes and principal
  kinds, every resource-key template and the keys the runtime capture builds.
- Integration: `graph-multistack-gcs` (three stacks, one GCS prefix each:
  exact URIs, four cross-stack links) and `graph-iam-bindings` (67 bindings:
  48 literal, 15 config, 4 resource output; 63/67 resolved = 94.0%, the rest
  resource edges, none unaccounted for).
- Security (174 -> 178): credentialed reference backends, hostile override
  names, secret config in every export, default output byte-identical.
- Fuzz (23 -> 26 targets): `fuzz_stack_uri`, `fuzz_cross_stack`,
  `fuzz_iam_graph`; `fuzz_resource_graph` exports with every host option.
- Bench: `export_resource_graph_200_nodes_uris_config`,
  `cross_stack_join_{100,1000}_stacks`, `iam_graph_{67,670}_bindings`.

## 0.5.37

### Column lineage through scripts

A view's columns were traced to their sources; a routine's were not. A
procedure that rebuilds a mart with `CREATE OR REPLACE TABLE ... AS SELECT`,
or appends to it with `INSERT ... SELECT`, said which tables it read and
wrote and nothing about which column came from where. `--script-column-lineage`
(Python: `script_column_lineage=True`) adds `column_derives_from` edges for
those statements in routine bodies, jobs, scheduled queries and SQL jobs,
`via` the script, with column nodes at both ends.

The parser's column analysis takes a query and nothing else, so the worker
regenerates the statement's query as SQL and analyses it exactly as it
analyses a view; only the answer crosses back. What is recorded is only what
the SQL says:

- `CREATE TABLE ... AS SELECT` and `CREATE VIEW`: the projections are the new
  object's columns, by name.
- `INSERT INTO t (a, b) SELECT ...`: projections map onto the listed columns
  by position, and only when the counts agree -- an unnamed projection is
  dropped by the analysis, and a count that no longer agrees is the signal.

Everything whose target columns the SQL does not state is left out rather
than guessed: an `INSERT` with no column list, whose order is the table's
schema; a `*` projection; a CTE the parser keeps outside the query, which the
regenerated text would lack; a statement writing more than one table.

Off by default, and the default output is byte-identical to 0.5.36. It is not
free: regenerating and re-analysing every query-backed statement takes the
fifty-statement routine bench from 1.5 ms to 6.7 ms, which is why it is a
flag.

### Tests

Five unit tests: CTAS columns by name, an INSERT column list by position,
CREATE VIEW in a script, five statements that must answer nothing rather
than a guess, and no columns computed unless asked for. Two lineage tests:
a routine's CTAS and INSERT traced column to column `via` the routine, and the
flag off giving output identical to before. One CLI test, two Python tests.
`fuzz_sql_lineage` runs the column path in its statement-layer pass. One
bench, `sql_lineage_routine_50_statements_columns`.

## 0.5.36

### Where a stack's state lives, and where every resource is

The graph said which stack a node belongs to and never where that stack is.
Given the backend -- `--backend-url`, else `PULUMI_BACKEND_URL`, else
`backend.url` in `Pulumi.yaml`; from Python, `backend_url=`, else
`backend.url` -- the export now says:

- `stack_uri` on the stack node and the graph:
  `https://app.pulumi.com/{org}/{project}/{stack}` for Pulumi Cloud,
  `{backend}/.pulumi/stacks/{project}/{stack}.json` for a `gs://`, `s3://`,
  `azblob://` or `file://` backend.
- `referenced_stack_uri` on every StackReference whose name is a literal. A
  StackReference resolves on the backend of the stack that declares it, so
  the referenced stack's location follows from the same URL -- the one piece
  of cross-stack information a static export can state as fact rather than
  infer.
- `resource_uri` on every resource-like node: the stack's URI with the URN,
  percent-encoded, as the fragment. It is a LOCATOR, not a console deep link
  -- Pulumi documents no URL for a single resource, and this does not invent
  one.

A backend the crate does not recognise -- a self-hosted Pulumi Cloud, an
unknown scheme -- adds nothing, and neither does no backend: the output is
byte-identical to 0.5.35. A Pulumi Cloud URI is omitted when the organization
is not known, because it would name the wrong one. The self-managed path is
the project-scoped layout of backends created by 3.61 or later; a backend on
the older global layout needs `pulumi state upgrade` to match it.

A backend URL is caller input that ends up in a shared store, so it is held
to what a location is. A query or fragment on a self-managed URL is dropped --
`?profile=`, `?region=` and their kind are settings, and some are secrets. A
URL carrying userinfo, whitespace or a control character is refused. Every
author-chosen name -- organization, project, stack, URN, a StackReference's --
is percent-encoded, so none can add a segment, a query or a fragment, and a
name that is exactly `.` or `..` has its dots escaped so it cannot become a
dot-segment a resolver would collapse. Formatting writes straight into the
output through `fmt::Write`; with no backend nothing is formatted at all.

The fuzzer found the first version's flaw within a minute: the percent
encoder escaped a multi-byte character byte by byte and sliced the name
between those bytes -- an empty slice, but one starting inside the character,
which Rust refuses. Any resource named in Polish or Japanese, or with an em
dash, aborted an export that had a backend. The encoder now slices only runs
of plain ASCII, the crash input is a corpus seed, and the case is a security
test.

### Tests

Seven unit tests on the module: every backend form and what it yields, the
organization rule, encoding of segments and URNs, multi-byte names, dot
segments, precedence, and reading `backend.url` from a project file. Four
graph tests: no backend adds no field, a self-managed backend locating the
stack, both references and a resource, Pulumi Cloud with and without an
organization, and a dynamic StackReference name. Five security tests
(169 -> 174): query credentials never exported, userinfo and control
characters refused, a hostile name unable to change a URI's shape, a hostile
StackReference name kept inside its segments, and a multi-byte resource name
not aborting the export. `fuzz_resource_graph` exports every input with two
backends and holds every URI under its backend, free of whitespace, control
characters, queries and dot-segments; no new target (22). One CLI test for the
flag over the project file, three Python tests for the keyword, the project
fallback and `""`. One bench, `export_resource_graph_200_nodes_backend_url`:
5% over the export without a backend.

## 0.5.35

### Every SQL statement, with what it reads and what it writes

The lineage graph said which tables a view or a routine depends on, and never
showed the SQL that made it so. `--sql-statements` (Python:
`include_statements=True`) adds the statement layer: every statement a program
carries, its exact text, what it does, where it is written, and edges from it
to everything it reads and everything it writes.

A statement is keyed by what it says -- `sql:` and the SHA-256 of its exact
text, from the crate's own implementation -- so the same `MERGE` written in
three routines is one row with three occurrences. An occurrence records the
carrier (a view's table, a routine, a job, a dbt model), the statement's index
and byte range inside the carrier's SQL, its role and provenance, the
`fn::readFile` path it came from, and the declaring resource's URN. Edges:
`has_statement` from the carrier, `reads` to each upstream table or location,
`writes` to each table it creates or changes -- a view's query writes the view
-- and `calls` to each routine. The URIs of `EXPORT DATA` and `LOAD DATA`
become `external_location` nodes.

The text is borrowed, not copied. A statement is a slice of its carrier, and
the carrier is a slice of the parsed template whenever the SQL was written
inline, which is why the splitter now hands out byte ranges. SQL read from a
file comes from `SqlSourceStore`: every file the program names with
`fn::readFile` is read once, before the export, under the same containment the
exporter applies, and the graph borrows from it for as long as it lives. A file
named by two spellings of one path is read once. Text that had to be built --
a literal-only interpolation, a dbt model after substitution -- is owned, and
`text_origin` says so. The store is an optimisation and never a second source
of truth: a file it does not hold is read the way it always was, with the same
warnings, and the export with and without it is byte-identical.

Off by default. Statement text can carry what an author never meant to
publish, and with the layer off nothing is hashed, no row is built, and the
output is byte-identical to 0.5.34 -- every acceptance golden is unchanged.
Measured on a program of twenty views and a fifty-statement routine, the layer
costs 4% of the export.

### What a statement changes is a write

The script path classified statements by a match whose catch-all counted
every table a statement named as a read. `DROP TABLE x` was reported as
reading `x`; so were `TRUNCATE`, `ALTER` and the target of a Hive `LOAD DATA`.
Each statement now has a kind -- `select`, `insert`, `update`, `delete`,
`merge`, `create_table`, `create_table_as_select`, `create_view`,
`create_materialized_view`, `create_function`, `create_procedure`, `drop`,
`truncate`, `alter`, `export_data`, `load_data`, `call`, `scripting`, `other`
-- and the objects of `DROP`, `TRUNCATE` and `ALTER` are writes.

Two shapes the parser does not take are read from their text, each pinned by
a test. BigQuery's `LOAD DATA ... FROM FILES` does not parse in 0.13.1; its
target is a heuristic write, where the fallback scan would have reported it as
a read. BigQuery's `CALL` arrives as an opaque command, so its detection stays
textual. The `uri` of an `EXPORT DATA` and the `uris` of a `LOAD DATA` are read
from the options text by a scanner that steps over quotes and comments.

### An unquoted qualified table name lost its qualifiers

`polyglot_sql::get_table_names` answers a table's BARE name for an unquoted
qualified reference: `other.raw.events` comes back as `events`, while the
backticked form keeps everything. Every unquoted read in a routine, a job or a
scheduled query was therefore resolved against the default project and
dataset -- lineage pointing at a table that does not exist, or at a different
one that does, with nothing to say so. Views were never affected: they go
through `analyze_query`, which keeps the qualifiers. The bare names are still
what decides which tables count -- they already leave out CTE names -- and each
is expanded to every qualified spelling of it the statement contains.

### Scheduled queries run in their own project

A `DataTransferConfig`'s destination, and every unqualified table its query
named, was resolved against the stack's default project and never against the
config's own `project`. And the destination's `derives_from` edges were
hard-coded `parsed`, so a read the heuristic scanner found in SQL the parser
could not read reappeared on the destination claiming it had been parsed. Both
are fixed; a destination's edges carry the strength of the read they repeat.

### The graph exports release the GIL

`export_dependency_graph` and `export_sql_lineage` held the GIL through the
whole of the work -- discovering, reading and rendering the project, the
export, the serialization. The context dictionary is read under it now and
everything after runs released, the same split the render and checkpoint
paths have had since 0.5.29. Dict keys come out sorted, as they always have.

The test that was meant to prove the render path released the GIL could not
fail: the thread it watched also advances in the Python code on either side of
the call. Both tests now compare the thread's progress during the call with its
progress during an equal sleep. Measured, a GIL-holding call advances it about
8% of that baseline and a releasing one about 63%; the bound is 30%. The graph
test was checked against a build with the GIL held, and fails there.

### Compressed NDJSON

`--compress gzip` (with `--format ndjson`) writes every table as
`*.ndjson.gz`, streamed through a gzip encoder -- the compression `bq load`
reads natively, and the right one for a statement table that carries every
query a program holds. The encoder is `flate2` on its pure-Rust backend
(`miniz_oxide`), behind a `gzip` feature that is on by default; a build
without it refuses the flag rather than writing uncompressed files under a
compressed name. Every write, the final flush and the gzip trailer are
checked, so a failure is an error and never a truncated table. The bytes a
gzip table decompresses to are exactly the plain table's.

### Tests

Eleven new unit tests on the classifier, the option scanner, the `LOAD DATA`
target reader, qualified names, CTEs, `EXPORT DATA` and `CALL`. Eighteen new
lineage integration tests (11 -> 29): DROP, TRUNCATE and ALTER as writes; the
heuristic `LOAD DATA` write; CTAS, MERGE and CALL in routines; a job's
destination; a scheduled query's project and the strength of its destination
edges; a `gcpx:scheduler:SqlJob` script; the unquoted qualified read; and the
statement layer -- absent by default, a view as one statement, a script's
offsets, kinds and edges, two occurrences of one text, interpolated text owned,
an external location, file text borrowed from the store by pointer, and the
store changing nothing but where the text lives. Six new security tests
(163 -> 169): the store refuses a traversal, an absolute path, a symlink out of
the tree, a file over the cap without reading it, and a non-UTF-8 file; and
statement text never appears in the default output. `fuzz_sql_lineage` runs the
statement layer and holds every id to the hash of its text and every statement
edge to a statement that exists; `fuzz_sql_statements` drives the classifier;
no new target (22). Four CLI tests -- the statement files, the flag's
dependency on `--lineage`, `--compress` only with NDJSON, and gzip tables
decompressing to the plain bytes -- and four Python tests. Three benches:
`sql_lineage_70_statements_layer_off`, `..._layer_on` and
`split_statement_spans_1mib`.

## 0.5.34

### A statement split that sliced author text inside a character

Routine bodies, job queries and scheduled queries are split into statements
before anything parses them, and the splitter sliced the author's text at byte
offsets it had not proven were character boundaries. Three ways in, all
reachable from a `definitionBody`, and under `panic = "abort"` each one took the
host down:

- `strip_begin_end` lowercased the body and then sliced the ORIGINAL with an
  offset found in the lowercased copy. Lowercasing changes byte lengths -- `İ`
  is two bytes and lowercases to three, the Kelvin sign is three and lowercases
  to one -- so `BEGIN İ END` sliced past the end of the string.
- The same function sliced `trimmed[..5]` before knowing byte 5 was a
  boundary, so a body as plain as `ąąą` was enough.
- CALL detection, on the parser worker, sliced `trimmed[..4]` the same way. A
  panic there killed the worker, and a dead worker was indistinguishable from a
  parse error: the whole script fell back to the heuristic scan and every write
  it made was silently lost.

The fuzzer never reached any of them. `fuzz_sql_lineage` wraps its input as a
VIEW query, and a view is analysed as one statement -- it never meets the
splitter. The target now also wraps its input as a routine body.

The splitter scans bytes for ASCII delimiters now, and an ASCII byte never
occurs inside a multi-byte sequence, so every position it stops at is a
boundary by construction. It also returns its statements as byte ranges into
the carrier (`split_statement_spans`) rather than as new strings -- the ground
the statement-level lineage of the next release is built on. On ASCII input it
agrees exactly with the splitter it replaces, which is kept as a test oracle.

### Faster without moving a byte

Every acceptance stack is now exported by a golden test -- infrastructure
graph, lineage graph, the CLI's combined document and the diagnostics -- and
compared with a stored copy byte for byte. Everything below was held to it.

- `literal_resolve` borrowed nothing: the parser stores every literal as an
  owned `Cow`, and `s.clone()` copied the whole string -- a view's SQL, a
  schema -- per resolution and again per memo hit. It is a borrow now.
- `MergedTemplate::as_template_decl` deep-cloned every expression tree in the
  program, and every caller dropped the original. `into_parts` moves it.
  `merge_templates` moves additional files' items instead of cloning them.
- Every Jinja-rendered file was deserialized twice, once by the validator and
  once by the parser. The validator now hands its document on.
- The CLI and both Python exporters parsed the whole program just to read its
  `name`, then read `Pulumi.yaml` a second time for the provider-templated
  packages. `project_prepass` does both from one read, with a name reader that
  never builds the template.
- Graph edges were keyed by two cloned URN strings per insert. They are keyed
  by interned ids, the topological walk runs on indices rather than hashed
  names, evaluation dispatches through one name map and one rayon pool instead
  of scanning the template per node and building a pool per level, and
  default providers are found once rather than rescanned per resource.
- NDJSON rows stream through a buffered writer; `graph` and `exec` no longer
  start a tokio runtime they never use.

Measured against 0.5.33 on one machine in one session: topological sort of a
100-node chain -31%, merging ten files -23%, evaluating 200 resources -23%,
literal resolution across 200 invokes -20%, Jinja render -15%, the graph export
of 200 resources -10%. Nothing is slower than the benchmark gate allows.

One fix fell out of the reading. An undefined reference's "did you mean"
suggestion was the closest known name by edit distance, with ties left in
`HashMap` order -- which is randomised per process, so the same typo could be
told to mean a different name on each run. Ties now go to the name that sorts
first.

### The fuzz workspace's lockfile had not moved

The fuzz crate is its own workspace, and its lockfile still resolved
`polyglot-sql` to 0.6.2 -- the version that does not terminate on a
ten-character query, which 0.5.33 moved off. Both lockfiles resolve 0.13.1 now.

### Tests

A Jinja changelog regression matrix: forty-seven Rust tests, one module per
release from 0.4.0 to 0.5.33, each driving `multi_file::load_project` with a
Jinja context -- the loader path the optimisations above move -- and eighteen
Python tests driving the same fixes through `export_dependency_graph`. Every
matrix test was checked to fail with its fix reverted; reverting the escaped
dollar guard to the one 0.5.30 shipped fails the two tests written for it.

Goldens for all thirty-two acceptance stacks, re-blessed only with
`PULUMI_RS_YAML_BLESS=1`. Six unit tests on the splitter (the oracle agreement,
`İ`, the Kelvin sign, a multi-byte fifth byte, CALL on the worker) and four
security tests through the full export (159 -> 163), each failing against the
splitter that shipped. A new fuzz target, `fuzz_sql_statements` (21 -> 22),
holds every range in bounds, on boundaries, trimmed, ordered and agreeing with
`split_statements`. One unit test each for borrowed literals, the name reader
against the full parser, and a deterministic suggestion; one for streamed
NDJSON being byte-identical. Three new benches: `export_resource_graph_200_nodes`,
`eval_200_resource_wide_level` and `sql_lineage_routine_50_statements`.

## 0.5.33

### A derived string the engine can resolve before the deploy

`fn::randomString` has been in this engine since the builtins were written, and
it is a trap. It draws from `thread_rng` on every evaluation, so it answers
differently on each one -- including between a preview and the update that
follows it. A resource named from it is therefore replaced on every single
deploy, and its name is unknowable until the resource exists. That is strictly
worse than the `random` provider's own resource, which at least persists its
value in state, and it is why nothing in any corpus we can see uses the builtin.

What authors actually want from it is a short stable suffix, and the reason they
want it is naming: a service, a bucket and a workflow that must not collide.
`fn::deriveString` computes exactly that shape of value as a pure function of a
seed. Same seed, same answer, on every evaluation, in every environment and on
a first deploy -- so the value is known at preview and a redeploy plans no
change.

The case that prompted it: a stack computing a Cloud Run service name from a
random suffix, and embedding that name in a workflow body so the workflow could
call the service. A policy check that has to read the body cannot read it at
all on a first deploy, because the name is not there yet, and a check that
cannot read a value it is responsible for has to refuse. The same stack with a
derived suffix reads as a literal everywhere.

### The reason it is a builtin and not a Starlark snippet

`fn::starlark` could express this today -- the derivation is pure integer and
string work, and Starlark is deterministic by construction, which is the same
property that makes it safe to run an author's script during a render. It would
be the wrong home for one decisive reason: `literal_resolve` cannot see through
it. That module answers plain literals, literal-only interpolations, chains
through variables, and pure `str` invokes; its match ends in `_ => None`, and a
Starlark call falls in that arm. So a Starlark-derived name would be as opaque
to an import, a graph export and a shift-left policy check as a random one --
which forfeits the whole point of making the value stable.

`fn::deriveString` gets an arm in that module instead, and the arm is the
feature. A derived name now reaches `literal_properties` in the graph export,
so a tool can know a resource's name without evaluating the program. The
conversion path gains the same thing: a derived value lowers to the literal it
computes rather than the `null` the engine's other non-PCL builtins emit, so a
converted program keeps the names the YAML produced. A seed that is not itself
a literal still resolves to nothing, never a guess, which is that module's
standing contract.

Losing the arm is **silent** -- the catch-all absorbs it and the workspace
compiles with zero errors -- so two graph cases exist to redden when it goes.

### The draw is unbiased, and that had to be measured

`256 % 36 == 4`, so a plain `byte % 36` over the default alphabet maps bytes
252..=255 onto its first four characters on top of their fair share: 8/256 of
the draw each instead of 7/256. Rejection sampling discards that band. When the
digest is spent it is extended with a counter, which keeps the sequence a pure
function of the seed, and the extension count is bounded so termination is a
fact rather than a probability.

The first test written for this asserted that every character of the alphabet
gets drawn. It passed against a plain modulo draw, because a modulo draw is
biased and not incomplete -- it still produces all thirty-six characters. The
share of the favoured four is measured instead: over 25,600 draws rejection
lands at 0.11133 against an ideal 4/36 = 0.11111, and modulo at 0.12410. The
bound sits between them. Two independent implementations of the biased variant
agreed on 0.1241 to five decimal places, which is how the threshold was chosen
rather than guessed.

### A ten-character query that never came back

The `Fuzz` workflow reports `fuzz_sql_lineage` as a timeout on one run and an
out-of-memory on the next. Both are the same thing: on
`polyglot-sql` 0.6.2, `parse("CP.:q(nc:e", BigQuery)` **does not terminate**.
Ten characters -- reduced from the 34 the fuzzer found, and a single unclosed
paren after an identifier carrying both `.` and `:` is enough. Measured on the
pipeline: flat RSS, 100% of a core, ten minutes and counting. The SQL reaching
that parser is an author's `view.query`, so the input is untrusted by
construction.

The engine pin moves to 0.13.1, where the same input is reported as the parse
error it always was, in 873us. The shape was then found a SECOND time,
independently: the weekly fuzz budget was raised to 120 seconds and the next run
drew a 25-byte input out of the same family -- a non-ASCII byte and a CRLF in
front, the same identifier carrying `.` and `:` ahead of an unclosed paren.
Replayed locally, 0.6.2 was still running after 90 seconds with resident memory
climbing; 0.13.1 refuses it in 0.01s. Two independent discoveries of one shape
is the reason the pin is the fix rather than a filter on the input, and both
are now corpus seeds. 0.6.3 still hangs, so a patch bump is not the
fix; the API cost of the seven-minor-version jump is one field, which is what
keeping the integration in one file was for. The full suite passes unchanged --
no lineage result moved, which was the risk worth checking rather than assuming.

That field is `complexity_guard`, and it is set to `Some(..default())`
deliberately. `None` disables every limit, which would be a silent fail-open.
It bounds parser depth, AST depth and node count, parenthesis and function-call
depth, token count and input size, all before an AST exists -- 600 levels of
nesting are refused in 148us naming the limit and the value that exceeded it.
Our own 1 MiB pre-flight stays, being stricter than the 16 MiB default and
cheaper to check.

A wall-clock budget was designed first and then rejected on a property the
guard has and it does not: a refusal by input SHAPE is a deterministic function
of the text, while a timeout depends on machine load -- and this target asserts
that two exports of one input are identical, so a budget would have traded a
hang for a flake. It would also have needed a detached thread burning a core,
since a thread cannot be cancelled under `panic = "abort"`.

### One parser worker, reused, instead of a thread per call

A thread with a 64 MiB stack was spawned PER PARSER CALL. Measured: 31.5us to
spawn and join one, and a twenty-view program makes about forty such calls --
**1.26ms of pure thread churn**, which turned out to be the largest single item
in `sql_lineage_20_views`. The worker is now one per calling thread, parked on
a channel receive between jobs, so an idle one costs no CPU and the repeated
64 MiB reservation is gone. `in_parser_thread` is deleted rather than kept
beside it.

Jobs cross as an explicit enum rather than a boxed closure -- there is no `dyn`
in this crate -- which has the side benefit of making the isolation contract
legible: `Reply` lists exactly what leaves the worker, and it is owned fact
structs, never AST nodes. A worker that dies is cleared from its slot so one
failure cannot poison every later call on that thread, and a parse that cannot
reach a worker is reported rather than silently skipped.

The numbers, and they are the reason this is in the same release as the pin:

| | `sql_lineage_20_views` |
|---|---|
| before any of this (0.6.2, thread per call) | 3.423 ms |
| 0.13.1 alone | 4.690 ms (+37%, and the gate refused it) |
| 0.13.1 + reused worker | **2.077 ms (-39%)** |

0.13.1 costs 1.9x more per call -- `analyze_query` 29.8us -> 56.3us, `parse`
7.6us -> 13.5us, of which the guard is about a third -- and the path is still
39% faster, because the churn was always the dominant term. It was being paid
before this release too; nothing here created it, and the benchmark gate is
what found it rather than any claim made for the change.

### One parser thread for a script, not one per statement

Found while reading the path the hang took, and ours rather than upstream's.
`MAX_SQL_BYTES` bounds the TEXT at 1 MiB and `split_statements` drops only the
EMPTY statements, so 1 MiB of `select 1 from t;` is about 65,000 non-empty
statements -- and each one spawned a thread with a 64 MiB stack. Nothing
bounded the count.

`statement_facts_for_all` parses a whole script on one worker, returning one
result per statement in order so a caller can still fall back per statement.
The isolation contract is unchanged: AST nodes never leave the worker, only
owned fact structs. `statement_facts_for` is deleted rather than kept beside
it, and its three cases now run through the batch entry point.

What this buys, stated honestly, because a first draft of this entry claimed
more and the benchmark refused it: batching bounds the WITHIN-ONE-SOURCE
amplification, which is the 65,000 above. It does not speed up the fleet's
actual shape. A view holds one statement, so a twenty-view program makes twenty
one-statement batches and the per-statement saving is zero -- which is why
`sql_lineage_20_views` moved 3.423 ms -> 4.690 ms on this change plus the pin,
and the gate was right to refuse it. The twenty-view cost is a thread per CALL,
not per statement, and it is the next section that removes it.

### Author text shaped like a readFile marker is not the engine's

The scheduled fuzz run of 2026-09-30 failed `fuzz_string_filters` on the
subject `"\n\n\x00RF:\x0b\x00\n"`, which came back `"\n\n\n"`. The target
noticed because wrapping a subject may only move characters between lines,
never remove them -- and a line had been deleted.

Not a filter defect, and older than the filters. `\x00RF:<id>\x00` is the
Jinja module's own in-band marker for a `readFile()` result, substituted after
the render. The whole-line branch of `resolve_readfile_markers` matched the
marker SHAPE, failed to parse `\x0b` as an id, and then pushed nothing at all
-- there was no `else`. So any author text of that shape was silently deleted
from the rendered program, as was a marker whose id was numeric but absent from
the cache.

`replace_inline_markers` already got the same case right: on a parse failure or
a cache miss it pushes the marker back verbatim. The two paths disagreeing
about one situation is the whole of the defect, and the inline one is correct --
a marker this engine wrote always carries a numeric id that is in the cache, so
anything else is author text and is carried through unchanged.

Four cases pin it: the fuzz subject itself, a numeric id that is not cached at
three indents, the agreement between the two paths, and that a real marker
still resolves and still consumes its NULs, which the fix must not break. The
crashing input is committed as a corpus seed, so every future run takes the
same path to it rather than relying on rediscovery.

### The converter's entry point is narrow, and that is worth a megabyte

The first version of the conversion support called the general
`literal_resolve` entry. That function reaches `resolve_str_invoke`, which
reaches `native_str`, which links `regex` -- and the converter had never
depended on regex. Measured against the release binaries: the converter grew
**+1,075,648 bytes, 62% on a 1.7 MB binary**, for the convenience of keeping a
derived name in a converted program. The language host, which already carries
regex, grew 16 KB.

So the public entry is now one function that resolves `fn::deriveString` with
inline literal arguments and nothing else. The converter grows **16 bytes**.
Nothing is lost by the narrowing: the general entry it replaced passed an empty
variable map, so a reference never resolved through it either, and anything
other than inline literals still emits what it emitted before.

Worth stating as a rule, not an anecdote: a `pub` on a module in this crate is
a link-time decision as much as an API one, because the evaluator's paths pull
in the parsing and matching machinery behind them.

### SHA-256, in-crate, because the value is a compatibility contract

A derived name goes into cloud state. Move the algorithm and every resource
named through it is renamed, which a provider carries out as a delete and a
create. So the hash cannot be something that may change underneath us.
`std`'s `DefaultHasher` is SipHash-1-3 today and the standard library declines
to promise it stays that way, so a toolchain bump would rename a fleet; a
seeded `rand` generator reserves the same right between minor versions; a
hand-rolled FNV-1a would be stable, being ours, but has nothing to be checked
against and poor avalanche on exactly the input shape this sees -- short seeds
differing in their last bytes.

FIPS 180-4 has neither problem, and a published standard can be checked against
something that is not this code. `sha256.rs` is one function over one slice: no
streaming pair, no trait, no HMAC, no allocation, and **no new dependency** --
`rand` and `uuid` stay where they were, used only by the two older builtins.

### Arguments

`from` (any expression), `length` (1..=64, default 8) and `alphabet`
(lowercase alphanumeric by default, which is what the random-provider calls
this replaces ask for). The scalar shorthand `fn::deriveString: my-seed` means
`{from: my-seed, length: 8}`, and the two spellings are pinned equal so an
author moving between them cannot silently rename a resource.

An unknown argument is **refused by name**, not defaulted: `len:` instead of
`length:` is an error naming the three valid arguments, because a silently
defaulted typo is how a stack gets a name nobody intended. An `Unknown` seed
propagates `Unknown` rather than erroring -- the `has_unknown` guard that
`fn::randomString` and `fn::uuid` are the only builtins to skip. A `Secret`
seed is refused rather than unwrapped, since unwrapping would put a
secret-derived value into a plain resource name without saying so.

`from` is an ordinary expression, so a `${...}` in it is resolved -- or refused
-- by the engine before the draw sees anything. The default alphabet can begin
with a digit, which is fine for a suffix; a value used as a whole name where
the provider demands a leading letter wants an explicit `alphabet` or a prefix.

### Bounds, each one audited rather than assumed

`length` is the only argument that sizes the output, and it is capped at 64. An
enormous `from` cannot amplify what is allocated: it is hashed, so the output is
a function of `length` alone -- a 1 MiB seed still yields four characters.

It can, however, cost time, and the distinction is worth stating because the
first draft of the bench comment got it wrong. Measured: a 4-character suffix
is 334ns and the 64-character worst case 1.03us -- the same order, since the
extension loop adds compressions rather than changing the shape of the work --
while a 64 KiB seed takes 278us, ~830x the short case, because the whole seed
is hashed. Seeds in practice are resource names of a few tens of bytes.

The draw indexes an ASCII alphabet's BYTES rather than walking its characters.
`alphabet.chars().nth(i)` decodes UTF-8 to reach the nth character, ~33ns each,
and on the 64-character case that walk was the whole of the difference from the
4-character case. The default alphabet is ASCII and so is every realistic
naming scheme, so the walk survives only for a multi-byte alphabet. Measured
by criterion at p < 0.05: -22.6% on the 4-character case, -34.1% on the
default 8, **-56.9% on the 64-character worst case**. The differential corpus
carries non-ASCII alphabets, so the fallback is exercised and the fast path is
proven behaviour-preserving rather than assumed to be. An alphabet over
256 characters is refused, because the draw takes one byte per character and
could not address the tail, so the bias would be silent. An empty alphabet is
refused rather than looped over. That audit is itself a test, so an argument
added later has somewhere to declare its bound.

### `fuzz_starlark` was asserting a property that is not true

The target fed arbitrary bytes to the `starlark:` block as a script and ran
them. It reported an out-of-memory -- 2202 MB against libFuzzer's 2048 MB
limit -- on `'   '*333333333`, which is a correct Starlark program run
correctly. The engine was not at fault and neither was the input. The
assertion was: that running an arbitrary script leaves the process alive. It
does not, if the script says otherwise.

**Nothing in this process can refuse the allocation.** starlark-rust 0.13
exposes no heap limit -- there is no `set_max_*` on the evaluator and
`Heap::alloc` is infallible, so there is no failure to handle -- and the
allocation happens inside ONE expression, where `str_type.rs` reaches
`String::with_capacity(self.len() * l)` directly. The only hook offered is
`before_stmt_for_dap`, which never fires between the multiplication and the
reservation; a wall-clock budget cannot observe a deadline the memory is taken
before; and an allocator returning null only moves the abort into
`handle_alloc_error`.

**Two lexical filters were tried, and both were wrong.** This is the part
worth recording, because the first looked sufficient and passed a full run.
Skipping a source that named a large numeric literal rejected the reproducer,
and the target went green. The next run produced `'   '*3332323*3332323` --
two literals under the bound, multiplied -- and AddressSanitizer refused a
33 TB request. Widening the bound does not help either, because the growth
needs no large literal at all: `s = 'x'` followed by
`for _ in range(45): s = s + s` reaches the same size with two digits and no
`*`. Source text cannot be screened for what it will allocate, so no filter
is shipped.

**And `compile` is not a parse.** It calls `eval_module`, which EXECUTES the
module's top-level statements -- that is how the `def` becomes a binding. So
an expression at the top level of an author's `starlark:` block runs when the
program is loaded, whether or not `fn::starlark` is ever invoked. That is why
handing arbitrary bytes to `compile` was never safe, and it sharpens the step
limit still owed below: a bound has to cover module execution, not only a
call.

So the target is now split by what can hold each input safely:

| half | input | property |
|---|---|---|
| template parser | arbitrary bytes | this crate's own parser, which evaluates nothing, must not panic or overflow the stack |
| compile | hostile text inside a string LITERAL | the parser sees arbitrary text through a skeleton that cannot express an operator, so there is nothing for the module to compute |
| call | hostile DATA through a fixed script | the value bridge must not panic in either direction, and must answer the same twice |
| diagnostics | fixed scripts that fail | the four refusal paths, unreached once arbitrary bytes stop reaching the compiler |

The data half is the realistic threat model, which the original target did not
have: in a `Pulumi.yaml` the script is the author's own text, while the
`input:` carries a config value or a resource output -- the part the author
does not control. Fourteen fixed scripts cover every return type the bridge
carries, a read of each input shape, an out-of-range index, a type error
raised inside Starlark and a `fail()`; the fuzzer chooses the script by the
first byte, so a reproducer stays tied to one, and supplies the data,
including as an object KEY, which has its own conversion path.

Writing the refusal half turned up a second behaviour worth pinning. Of seven
scripts assumed not to compile, two do: `compile` keys the frozen module on
the DECLARED name rather than on what the script defines, so a script defining
`other`, or defining nothing at all, leaves `has_function("fuzz_func")`
answering true and the mismatch surfacing at call time instead. The two cases
are now asserted separately -- `NON_COMPILING` must be reported at compile
time, `COMPILES_BUT_NOT_CALLABLE` must compile cleanly, claim the name, and
report at the call -- so neither half can drift into the other unnoticed.

What is given up is fuzzing the execution of arbitrary author text. That was
never a property this engine could hold, and three findings in two runs are
the proof. What is gained is that a finding from this target is a finding.

The five corpus seeds are the shapes that defeated the filters, kept so the
parser halves keep seeing them.

A step or fuel limit on `fn::starlark` is still owed and still out of scope:
an author's `while True:` appears to hang a render today, and by the above it
can do so at load time rather than on invocation. That wants its own change
with its own security suite.

### The weekly fuzz budget was too short to reach its own findings

`FUZZ_DURATION` moves from 60 seconds to 120. Three separate latent defects
were found at 120 seconds that the weekly run at 60 had never reached -- the
non-terminating parse above among them, sitting unnoticed for days. A target
that stops before it explores is a green check that means nothing, and doubling
the budget is the cheapest available fix for that. The `workflow_dispatch`
input default moves with it so a manual run explores as far as the scheduled
one.

### Tests

722 differential cases against a separate implementation of the algorithm,
written from the rule rather than from this code: every seed shape (empty,
non-ASCII, CJK, whitespace, 200 bytes) against thirteen lengths and nine
alphabets, including the five argument combinations that must be REFUSED rather
than answered, all driven through the real evaluator -- a builtin that passed
here while unregistered in the parser would satisfy every assertion and still
fail every build. Three values are frozen in the source with a note that
changing them renames deployed infrastructure.

The encoding half is checked a third time by a Starlark implementation in the
same program, so the digest is held to the specification and the mapping to an
independent reading of the rule. Starlark has no `while` -- it is deliberately
non-Turing-complete -- so the oracle is a bounded `for` over the digest bytes.

Ten unit cases on the argument shape and the bounds; eleven security cases on
arguments nobody chose, including termination of the draw for every alphabet
size from 1 to 256 and that no character ever escapes the alphabet offered;
five graph cases pinning static resolution from both sides -- a derived name
reaches `literal_properties`, and a random one, an unresolvable seed and a
mistyped argument each reach no literal at all; one converter golden carrying
both the literal and the dynamic-seed branch; one integration case proving the
value reaches a provider and that two evaluations register identical inputs;
four benches; and a new fuzz target over all three arguments asserting
totality, exact character count, alphabet containment and purity.

Every suite was checked against an engine with the builtin unregistered, the
`literal_resolve` arm deleted, the rejection band removed, the `has_unknown`
guard dropped, the length cap raised and the unknown-argument refusal relaxed,
and each one reddens where it should.

Security tests 148 -> 159; fuzz targets 20 -> 21. Both README badges were stale
before this release -- they read 133 and 19 against a tree that had 148 and 20 --
and now carry the true counts.

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

Two gaps were found by auditing the implementation against the corpus rather
than by a failure, and one of them was a divergence rather than a hole. The
generated sweep is positional, so it reached none of the eleven KEYWORD-argument
branches — and Jinja2 authors write `truncate(length=60)` and
`wordwrap(width=40, break_long_words=False)`, which is the same reason `indent`
needed a compatibility shim in 0.5.19. Those branches turned out correct, but
untested. And `center` REFUSED a non-string while the reference coerces it:
`center` is `soft_str(value).center(width)`, so `{{ count | center(8) }}` centres
a number under every other Jinja toolchain and would have failed here — the exact
failure mode this release exists to remove. `truncate` and `wordwrap` genuinely
raise on a non-string in the reference, verified against it, so those keep their
errors.

One divergence is deliberate and recorded rather than chased. The reference
answers `{'a':1} | truncate(6, False, '', 0)` with `{'a': 1}` — but only because
`len()` of a one-key dict is 1, so its leeway check short-circuits before the
string operation; a seven-key dict raises `KeyError`, because slicing a dict is a
key lookup. That is Python's dynamic typing producing an answer, not the filter's
semantics, and reproducing it would mean implementing `len()` per type in order
to inherit a crash. A container is refused, with a test saying why.

Held to the reference rather than to an opinion: 3,154 differential cases —
every combination of 22 subjects (empty, whitespace-only, multi-line,
hyphenated, non-ASCII, CJK) against the parameter space of all three filters,
plus 120 keyword-argument spellings and 42 non-string subjects — captured from
the reference implementation and stored as a fixture, including the cases it
REFUSES, which the engine must refuse too rather than answer. Seven
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

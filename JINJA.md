# Jinja in pulumi-rs-yaml

Every Pulumi YAML project file this runtime reads is first rendered as a
Jinja template, then parsed. This is the reference: what is supported, what
each name means, the limits a render runs under, where includes may reach,
how errors read, and where the engine differs from Python Jinja2.

Every example on this page is executed by `tests/jinja_doc_examples.rs`: a
`jinja` block is rendered and its output must equal the `text` block that
follows it (or the `error` block must name the kind of error raised). If this
page and the engine ever disagree, CI fails.

**Contents:** [Overview](#overview) · [Syntax](#syntax) ·
[Context](#context-variables) · [Filters, tests and globals](#filters-tests-and-globals) ·
[Methods](#methods) · [Changing lists and dicts](#changing-lists-and-dicts) · [Dates](#dates) ·
[Undefined names](#undefined-names) · [Limits](#limits) ·
[Includes and imports](#includes-and-imports) · [Errors](#errors) ·
[Divergences from Jinja2](#divergences-from-jinja2) ·
[Environment variables](#environment-variables) · [FAQ](#faq)

## Overview

The engine is [minijinja](https://github.com/mitsuhiko/minijinja) 2, extended
and configured to behave as Jinja2 does, and held to it: the conformance
corpus (`tests/jinja_conformance/`) renders 336 cases and 73 method
cases through this runtime
and compares each byte for byte with what pinned reference Jinja2 produces.
The generated [capability matrix](tests/jinja_conformance/MATRIX.md) lists
every statement, filter, test and global with its status and the cases that
prove it.

Where it runs:

- the **language host** renders every `Pulumi.yaml` / `Pulumi.<name>.yaml`
  of a project before parsing it;
- **`exec`** validates the files and hands their original text to the host;
- **`graph`** renders the same way to build the resource graph;
- the **Python binding** (`preprocess_jinja`, `preprocess_jinja_diag`)
  renders text for a caller that runs Pulumi itself.

**One pass, never two.** Rendered output can legitimately contain Jinja
delimiters (a `{% raw %}` block emits them). A caller that has already
rendered the project sets `PULUMI_YAML_JINJA=off`, and the runtime then
parses every file exactly as written. Text that contains no Jinja syntax at
all is never rendered: it is returned as it is, byte for byte, without a copy.

```jinja
name: {{ 'app' | upper }}
{# a comment is removed #}
region: {{ 'north' }}
```

```text
name: APP

region: north
```

## Syntax

Everything Jinja2 offers, except what [Divergences](#divergences-from-jinja2)
lists:

- statements: `if`/`elif`/`else`; `for` with `else`, filtering `if`,
  `recursive`, `loop.index`/`index0`/`revindex`/`revindex0`/`first`/`last`/
  `length`/`cycle`/`changed`/`depth`/`previtem`/`nextitem`, `break` and
  `continue`; `set` (inline, multiple targets, block, block with filters);
  `macro` with defaults and `caller`; `call`; `include` (with `ignore
  missing`, lists of names); `import`, `from ... import` (with context);
  `extends`, `block`, `super()`, `self`; `filter` blocks; `with`; `raw`;
  `do`; comments;
- whitespace control: `{%-`, `-%}`, `{{-`, `-}}`, `{#-`, `-#}` and `{%+`;
  `trim_blocks` and `lstrip_blocks` are off, as in Jinja2;
- expressions: `+ - * / // % **`, comparisons (chained too), `and or not`,
  `in`/`not in`, `is`/`is not`, `~`, attribute and item access, slicing
  with steps, conditional expressions with or without `else`, and literals
  (`1_000`, `0x1F`, `0o17`, `0b101`, `1.5e3`, strings with escapes, lists,
  tuples, dicts, `true`/`True`, `none`/`None`).

```jinja
{% for name in ['alpha', 'beta', 'gamma'] if name != 'beta' -%}
{{ loop.index }}:{{ name }}{{ ',' if not loop.last }}
{%- endfor %}
```

```text
1:alpha,2:gamma
```

```jinja
{% macro bucket(name, location='EU') -%}
{{ name }}: {location: {{ location }}}
{%- endmacro -%}
{{ bucket('logs') }}
{{ bucket('data', location='US') }}
```

```text
logs: {location: EU}
data: {location: US}
```

```jinja
{% set ns = namespace(total=0) -%}
{% for size in [10, 20, 30] %}{% set ns.total = ns.total + size %}{% endfor -%}
total: {{ ns.total }}
```

```text
total: 60
```

Python's methods work on values, as in Jinja2 (see [Methods](#methods)):

```jinja
{{ 'a,b,c'.split(',') | join('+') }} {{ 'Data Lake'.lower().replace(' ', '-') }} {{ '{}-{}'.format('app', 3) }}
```

```text
a+b+c data-lake app-3
```

Python's arithmetic, including its corner cases: `/` is true division,
`//` floors and `%` takes the divisor's sign, and dividing by zero is an
error (`ZeroDivisionError`), never `inf`. `%` on a string is Python's printf
formatting (`s r d i x X o c f F e E %`, flags, width, precision, `%(name)s`
with a mapping), rendered by the same core as `str.format` and bounded by
the output limit.

```jinja
{{ 7 % -3 }} {{ -7 // 2 }} {{ 7 / 2 }} {{ '%s-%03d|%-6s|%5.2f' % ('a', 7, 'ab', 3.14159) }} {{ '%(env)s/%(n)d' % {'env': 'dev', 'n': 2} }}
```

```text
-2 -4 3.5 a-007|ab    | 3.14 dev/2
```

`*` repeats a string or list by a count (nothing for zero or less), and
`**` takes a negative exponent to a float. Counts given to filters behave as
in Jinja2: `slice(n)` makes `n` lists, `batch(n)` lists of `n`, and
`9 is divisibleby(0)` raises, as `9 % 0` does.

```jinja
{{ 'ab' * 2 }} {{ [0] * 3 }} {{ 'x' * -1 }}| {{ 2 ** -1 }} {{ [1, 2, 3, 4, 5] | slice(2) | list }} {{ [1, 2, 3] | batch(2) | list }}
```

```text
abab [0, 0, 0] | 0.5 [[1, 2, 3], [4, 5]] [[1, 2], [3]]
```

```jinja
{{ 1 / 0 }}
```

```error
JinjaFilterError
```

A macro that names `varargs` or `kwargs` takes extra positional and keyword
arguments into them, as in Jinja2; one that does not refuses extras.
`{% include ... without context %}` renders the template with none of the
caller's values.

```jinja
{% macro tag(name) -%}
<{{ name }}{% for k in kwargs | sort %} {{ k }}="{{ kwargs[k] }}"{% endfor %}>{{ varargs | join(' ') }}
{%- endmacro %}{{ tag('p', 'hello', 'world', id='x') }}
```

```text
<p id="x">hello world
```

## Context variables

| Name | Value |
|---|---|
| `pulumi_project` | the project name |
| `pulumi_stack` | the stack name |
| `pulumi_organization` | the organization |
| `pulumi_cwd` | the working directory |
| `pulumi_root_directory` | the program directory |
| `config.<key>` | the stack's config value for `<project>:<key>` (the namespace prefix is dropped); always a string |
| `env.<name>` | the process variable `JINJA_VAR_<NAME>`, name lowercased |
| `readFile(path)` | the text of a file under the project directory, spliced in with the indentation of the line it sits on |
| extra context | names a caller passes (the Python binding's context map); a built-in name always wins over an extra one of the same name |

Config values are strings. Convert before arithmetic:

```jinja
{# context: {"replicas": "3"} #}
replicas: {{ replicas | int * 2 }}
```

```text
replicas: 6
```

## Filters, tests and globals

Every Jinja2 builtin is available except the five refused by design (below);
[MATRIX.md](tests/jinja_conformance/MATRIX.md) lists each one with its
status. Behaviour is Jinja2's, including its less obvious choices: `round`
rounds ties to even, `int` and `float` fall back to their default instead of
failing, `max`/`min` compare strings case-insensitively, `title` starts a
word only after whitespace, `-` or a bracket.

```jinja
{{ 2.5 | round }} {{ 'x' | int(7) }} {{ ['b', 'A'] | max }} {{ "it's bar_baz" | title }} {{ 1536 | filesizeformat(true) }}
```

```text
2.0 7 b It's Bar_baz 1.5 KiB
```

Added by this runtime:

| Name | What it does |
|---|---|
| `tojson` (`to_json`) | compact JSON, safe to write into YAML for any value (see [FAQ](#faq)); `tojson(2)` indents |
| `to_yaml` | a YAML document |
| `base64_encode`, `base64_decode` | Base64 of UTF-8 text |
| `split`, `lines`, `chain`, `zip`, `bool` | minijinja builtins Jinja2 lacks |
| `startingwith`, `endingwith` | tests |
| `readFile()` | see [Context variables](#context-variables) |

```jinja
{{ {'team': 'data', 'tier': 1} | tojson }} {{ 'hello' | base64_encode }} {{ 'a b' | split | list | length }}
```

```text
{"team":"data","tier":1} aGVsbG8= 2
```

## Methods

Values answer Python's methods, as they do in Jinja2. Every one is held to
reference Jinja2 and to this runtime's own Starlark by the method corpus
(`tests/jinja_conformance/methods/`); [MATRIX.md](tests/jinja_conformance/MATRIX.md)
lists them with their cases.

| Receiver | Methods |
|---|---|
| `str` | `upper` `lower` `title` `capitalize` `swapcase` `casefold` · `strip` `lstrip` `rstrip` `replace` `removeprefix` `removesuffix` · `startswith` `endswith` (a tuple too) `find` `rfind` `index` `rindex` `count` (with start/end) · `split` `rsplit` `splitlines` `partition` `rpartition` · `zfill` `ljust` `rjust` `center` `expandtabs` · `isdigit` `isalpha` `isalnum` `isspace` `isupper` `islower` `istitle` `isdecimal` `isnumeric` `isidentifier` · `join` `format` `encode` (UTF-8) |
| list | `index` `count` `copy`, and the mutators below |
| dict | `items` `keys` `values` `get` `copy`, and the mutators below |
| int, float | `is_integer` `bit_length` `as_integer_ratio` `conjugate` |

`str.format` takes positional, numbered and named fields and the format-spec
mini-language (`{:>8}`, `{:05d}`, `{:.2f}`, `{:,}`, `{:#x}`, `{:.0%}`);
conversions (`!r`) and field indexing (`{0.name}`) are refused. `index`
raises an error naming what was not found. A method that can grow a value
(`ljust`, `zfill`, `center`, `expandtabs`, `replace`, `format`) is refused
before it allocates past the output limit.

```jinja
{{ '{}-{:03d}'.format('Data Lake'.lower().replace(' ', '-'), 7) }} {{ 'a,b,c'.rsplit(',', 1) | join('|') }} {{ 'x'.center(5, '*') }} {{ 'abc'.find('c') }}
```

```text
data-lake-007 a,b|c **x** 2
```

## Changing lists and dicts

A list or dict the template builds can be changed, with Python's methods and
Python's sharing: two names for one list see the same changes. `{% do %}`
calls a method for its effect; a mutating method returns `None`.

- list: `append` `extend` `insert` `pop` `remove` `clear` `sort` (`key=`,
  `reverse=`) `reverse`
- dict: `update` (a mapping or keywords) `setdefault` `pop` `popitem` `clear`

```jinja
{% set seen = [] %}{% set alias = seen -%}
{% for name in ['b', 'a', 'c'] %}{% do seen.append(name) %}{% endfor -%}
{% do seen.sort() %}{{ alias | join(',') }} {{ seen.pop() }}
```

```text
a,b,c c
```

What a filter returns is a new value the template owns, as in Python. Values
the program is given -- `config`, the context, a value the host passes in --
are **read-only**: changing one is an error that says to copy it first
(`x.copy()`, or `x | list`). Changing a list or dict while a loop walks it is
an error too (Python allows it for lists; here it would loop without end or
skip silently). A dict keeps its keys in key order, as every map literal here
does.

```jinja
{% do config.update(region='south') %}
```

```error
JinjaFilterError
```

## Dates

Naive dates and times, with Python's `datetime` semantics: `date(y, m, d)`,
`datetime(y, m, d[, h, mi, s, us])`, `timedelta(days=, seconds=,
microseconds=, milliseconds=, minutes=, hours=, weeks=)`, and the converters
`to_date`, `to_datetime` and `strptime(format)`, usable as filters or
functions. Dates and timedeltas compare, add and subtract; they have
`strftime`, `isoformat`, `date`, `weekday`, `isoweekday`, `toordinal`,
`replace`, `total_seconds`, and the attributes `year` ... `microsecond`,
`days`, `seconds`, `microseconds`. A host's `current_date` string becomes a
date with `current_date | to_date`.

```jinja
{# context: {"current_date": "2026-10-18"} #}
{% set today = current_date | to_date -%}
{{ (today + timedelta(days=90)).isoformat() }} {{ today.strftime('%A %d %B') }} {{ (to_date('2027-01-01') - today).days }}
```

```text
2027-01-16 Sunday 18 October 75
```

The calendar is proleptic Gregorian over years 1-9999. There is no timezone
database: values are naive, `%z`/`%Z` are empty, and an ISO string with an
offset is refused rather than guessed.

## Undefined names

**Every render is strict.** A name that does not resolve is an error that
names it -- never an empty string, because an empty bucket name rendered
silently into a program deploys the wrong thing. Jinja2's lenient default is
not offered. `is defined`, `default` and `.get()` exist to probe:

```jinja
{{ owner | default('platform') }} {{ owner is defined }}
```

```text
platform False
```

```jinja
owner: {{ owner }}
```

```error
JinjaUndefinedVariable
```

**Passthrough** (`PULUMI_YAML_JINJA_UNDEFINED=passthrough`) is for text
addressed to another Jinja engine, such as dbt models: an expression whose
root is not a name this runtime defines is left exactly as written. The
render is still strict otherwise -- a typo in `config.*`, or an undefined
name in a statement, still fails. Scoping a whole package with
`runtime.options.providerTemplatedPackages` is usually better; see the
README.

Syntax validation (`validate_jinja_syntax`, used by `exec`) compiles without
rendering, so it evaluates nothing and cannot report an undefined name.

## Limits

Each file's render runs under four limits. A render that crosses one stops
with an error of its own kind, naming the line and the limit -- never a hang
or an out-of-memory.

| Limit | Default | Kind | Bounds |
|---|---|---|---|
| `fuel` | 100,000 instructions | `FuelExhausted` | time: every loop, macro and filter call spends fuel |
| `recursion` | 500 (the engine maximum) | `RecursionLimit`, `IncludeCycle` | nesting: a macro call costs about 4, an include 10; reached on any caller's stack (rendering runs on a 64 MiB worker) |
| `output` | 16 MiB | `OutputTooLarge` | the rendered file, `readFile()` contents included; also any one value a filter builds from a width (`indent`, `tojson`, `center`), and a repeated string at four times this |
| `range` | 100,000 elements | `RangeTooLarge` | one `range()` call (Jinja2's own limit), a repeated list, and `slice`'s count |

The defaults were set from measurement: the heaviest of 4,583 production
project files used 6,575 instructions and rendered 61 KB, so the budget is
over ten times what any real stack needs; a 200-resource loop uses about
5,200.

Override them with `PULUMI_YAML_JINJA_LIMITS` (language host, `graph`, Python
binding) or the Python `limits=` argument, both in the same spelling:

```bash
PULUMI_YAML_JINJA_LIMITS=fuel=1000000,output=33554432 pulumi up
```

An unknown key, a repeated key, a zero, or a recursion above 500 is refused
by name. A runaway loop:

```jinja
{% for a in range(100000) %}{% for b in range(100000) %}{% endfor %}{% endfor %}
```

```error
FuelExhausted
```

```jinja
{{ range(1000000) | length }}
```

```error
RangeTooLarge
```

What the limits cannot see: a value built in memory and never written --
`{% set s = s ~ s %}` repeated inside a namespace loop -- grows until fuel
runs out, and each doubling costs only a few instructions. Every `{{ }}`
that writes such a value is charged against the output limit (also inside
`{% set %}` blocks and macro bodies), so it is stopped as soon as it is
emitted.

Two template shapes crash minijinja 2.24 itself rather than fail, and a
crash cannot be caught in a release build. Both are handled before the
template compiles. A slice with a step that may be negative is computed as
Python computes it (minijinja indexed past the end of an empty list):

```jinja
{% set items = [] %}[{{ items[::-1] | join(',') }}] {{ 'abc'[::-1] }} {{ [1, 2, 3][0:2:-1] }}
```

```text
[] cba []
```

And a namespace attribute may not be assigned a value holding a namespace --
printing a namespace that contains itself recursed until the stack
overflowed. Reading its attributes is unaffected:

```jinja
{% set ns = namespace(v=0) %}{% set ns.v = ns %}
```

```error
JinjaTypeError
```

## Includes and imports

`include`, `import`, `from ... import` and `extends` load templates by a name
relative to the stack directory, then the render root (the program
directory). The boundary is where a name **resolves**, not how it is spelled:

- a name that resolves -- after `..` and symlinks -- inside the stack
  directory or the render root is served; `../../shared/env.j2` from a
  nested stack is the common pattern;
- one that resolves outside both is refused (`include refused [escape]`),
  whatever spelling, symlink or case variant got it there;
- an absolute name -- `/x`, and on Windows `C:\x`, `C:x`, `\\server\share`,
  `\\?\...`, `\x` -- is refused before a path is built (`[absolute]`);
- a name holding a NUL or another control character is refused (`[bad name]`);
- a file that is not UTF-8 (`[binary]`) or is over 1 MiB (`[too large]`) is
  refused;
- a file that is simply absent is "not found", so `ignore missing` keeps its
  meaning; a refused file is never treated as missing.

`readFile()` follows the same containment, relative to the project directory.

```jinja
a{% include 'nope.j2' ignore missing %}b
```

```text
ab
```

```jinja
{% include '/etc/passwd' %}
```

```error
JinjaTemplateNotFound
```

## Errors

A render error names the file, line and column, shows the line with a caret
under the failing expression, and ends with a one-line suggestion:

```text
Pulumi.yaml:2:12: error: undefined value: `settings.region`
  2 | region: {{ settings.region }}
    |            ^^^^^^^^^^^^^^^
  suggestion: Check variable name. Available context: config.*, pulumi_*, env.*
```

A fault inside an included or imported template is located in that file,
with the chain of includes that led to it:

```text
Pulumi.yaml:2:4: error: upper takes no arguments
  2 | {% include 'inc/outer.j2' %}
    |    ^^^^^^^^^^^^^^^^^^^^^^
  --> in inc/inner.j2:2:10
  2 | b: {{ 'x'.upper(1) }}
    |          ^^^^^^^^^
  include chain: Pulumi.yaml:2 -> inc/outer.j2:2 -> inc/inner.j2:2
  suggestion: Check the arguments, and the type of the value they apply to
```

The kinds (`RenderErrorKind`, and as snake_case names in the Python
binding's `preprocess_jinja_diag`): `JinjaSyntax`, `JinjaUndefinedVariable`,
`JinjaFilterError`, `JinjaTypeError`, `JinjaTemplateNotFound`,
`FuelExhausted`, `RecursionLimit`, `OutputTooLarge`, `IncludeCycle`,
`RangeTooLarge`, `ProviderScope`, and, for rendered text YAML then rejects,
`YamlSyntax`, `YamlIndentation`, `YamlDuplicateKey`. The exact text of each
is golden-tested (`crates/pulumi-rs-yaml-core/tests/fixtures/jinja_errors/`).
The language host removes secret config values from every error it prints.

## Divergences from Jinja2

[DIVERGENCES.md](tests/jinja_conformance/DIVERGENCES.md) lists every case
whose output differs from reference Jinja2, with the reason; the conformance
run fails if one is added without a reason, or if a listed one starts to
agree. In short:

- **Kept for stability** -- changing them would change text deployed stacks
  already render: lists and maps print JSON-style (`["a"]`, `{"k": 1}`; tuples
  as lists) where Jinja2 prints Python reprs; a map literal iterates in key
  order; `tojson` is compact and does not HTML-escape; floats never use
  exponent notation; `escape` spells entities differently; CRLF line endings
  are preserved.
- **Refused by design**: `random` and `lipsum` (a deploy must render the
  same text every time); `urlize`, `xmlattr`, `striptags`, `forceescape`
  (HTML only; output here is YAML); a namespace holding itself (see
  [Limits](#limits)).
- **Not supported**: line statements (off in Jinja2 too); a timezone
  database. No known gaps remain against Jinja2's behaviour.

```jinja
{{ ['a', 'b'] }} {{ {'k': 1} }} {{ 'x' | tojson }}
```

```text
["a", "b"] {"k": 1} "x"
```

## Environment variables

| Variable | Values | Effect |
|---|---|---|
| `PULUMI_YAML_JINJA` | unset, empty, `on` / `off` | `off`: no render; files are parsed as written |
| `PULUMI_YAML_JINJA_UNDEFINED` | unset, empty, `strict` / `passthrough` | the [undefined-name mode](#undefined-names) of the language host |
| `PULUMI_YAML_JINJA_LIMITS` | `fuel=N,recursion=N,output=N,range=N` (any subset) | the [limits](#limits) |
| `PULUMI_YAML_JINJA_SOURCE` | a directory | set by `exec` for the host it spawns: where the original sources are |
| `JINJA_VAR_<NAME>` | any | exposed as `env.<name>` |

Every one of them refuses a value it does not understand, naming it.

## FAQ

**How do I write a value into YAML safely?** Quote it with `tojson`. A value
holding `: `, a newline, a `#`, or a character YAML treats specially changes
the document's structure if written bare; JSON is YAML, and `tojson` escapes
everything YAML would misread.

```jinja
{# context: {"description": "a: b\ninjected: true"} #}
description: {{ description | tojson }}
```

```text
description: "a: b\ninjected: true"
```

**How do I keep literal `{{ }}` in the output?** Wrap it in `{% raw %}`. If
something else renders the output again, have it set `PULUMI_YAML_JINJA=off`
for the runtime.

```jinja
sql: "{% raw %}select * from {{ ref('orders') }}{% endraw %}"
```

```text
sql: "select * from {{ ref('orders') }}"
```

**What about Pulumi's own `${...}`?** It is not Jinja syntax and passes
through untouched, to be resolved by Pulumi after rendering.

```jinja
url: ${bucket.url}/{{ 'index.html' }}
```

```text
url: ${bucket.url}/index.html
```

**How do I indent a multi-line value under a block scalar?** `indent`, with
the block's indentation; the first line is placed by the template.

```jinja
{# context: {"body": "line one\nline two"} #}
script: |
  {{ body | indent(2) }}
```

```text
script: |
  line one
  line two
```

**Comments are templates too.** Rendering happens before YAML is parsed, so a
YAML comment containing `{{ ... }}` is evaluated. Write the construct without
its delimiters, or wrap it in `{% raw %}`.

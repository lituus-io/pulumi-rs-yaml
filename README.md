# pulumi-rs-yaml

[![CI](https://github.com/lituus-io/pulumi-rs-yaml/actions/workflows/ci.yml/badge.svg)](https://github.com/lituus-io/pulumi-rs-yaml/actions/workflows/ci.yml)
[![Security](https://github.com/lituus-io/pulumi-rs-yaml/actions/workflows/security.yml/badge.svg)](https://github.com/lituus-io/pulumi-rs-yaml/actions/workflows/security.yml)
[![Fuzz](https://github.com/lituus-io/pulumi-rs-yaml/actions/workflows/fuzz.yml/badge.svg)](https://github.com/lituus-io/pulumi-rs-yaml/actions/workflows/fuzz.yml)
[![Benchmark](https://github.com/lituus-io/pulumi-rs-yaml/actions/workflows/benchmark.yml/badge.svg)](https://github.com/lituus-io/pulumi-rs-yaml/actions/workflows/benchmark.yml)
[![fuzz targets](https://img.shields.io/badge/fuzz%20targets-19-blue)](fuzz/fuzz_targets)
[![security tests](https://img.shields.io/badge/security%20tests-133-blue)](crates/pulumi-rs-yaml-core/tests/security_tests.rs)
[![License](https://img.shields.io/badge/license-AGPL--3.0--or--later-blue)](LICENSE)

Rust implementation of the [Pulumi](https://www.pulumi.com/) YAML language runtime. Drop-in replacement for the Go-based `pulumi-yaml` with 1:1 compatibility.

## Architecture

5-crate workspace:

| Crate | Purpose |
|-------|---------|
| `pulumi-rs-yaml-proto` | Pre-generated protobuf/gRPC stubs |
| `pulumi-rs-yaml-core` | Parser, AST, evaluator, Jinja, type checker, PCL codegen |
| `pulumi-rs-yaml-language` | gRPC language host (`pulumi-language-yaml`) |
| `pulumi-rs-yaml-converter` | Converter plugin (`pulumi-converter-yaml`) |
| `pulumi-rs-yaml-python` | PyO3 bindings (`pulumi-rs-yaml` on PyPI) |

## Install

**Binary** (from [GitHub Releases](https://github.com/lituus-io/pulumi-rs-yaml/releases)):

```bash
# Replace with your platform: linux-amd64, linux-arm64, darwin-amd64, darwin-arm64, windows-amd64
curl -sSL https://github.com/lituus-io/pulumi-rs-yaml/releases/latest/download/pulumi-yaml-linux-amd64.tar.gz | tar xz
```

**Python**:

```bash
pip install pulumi-rs-yaml
```

This installs the PyO3 bindings and bundles `pulumi-language-yaml` and `pulumi-converter-yaml` as console scripts.

## Build from source

```bash
cargo build --release
```

Binaries are at `target/release/pulumi-language-yaml` and `target/release/pulumi-converter-yaml`.

Release binaries are size-tuned: the SQL parser and Starlark are compiled at
`opt-level = "z"`/`"s"` while the evaluator stays at `opt-level = 3`.

| Binary | Size |
|---|---|
| `pulumi-language-yaml` | ~9.5 MB |
| `pulumi-language-yaml --no-default-features` | ~6.9 MB |
| `pulumi-converter-yaml` | ~1.6 MB |

The SQL lineage layer accounts for the difference; build without it when size
matters more than the `graph --lineage` flag:

```bash
cargo build --release -p pulumi-rs-yaml-language --no-default-features
```

The `release-small` profile (`opt-level = "z"` everywhere, ~8.2 MB full-featured)
trades evaluator throughput for a further reduction.

## Provider-templated blocks

Some providers render their own templates. A dbt model's SQL is written in
Jinja and resolved by the provider, per resource, long after this runtime has
rendered the stack file — but both layers use `{{ }}` and `{% %}`, so text meant
for the provider is evaluated here first, by a renderer that knows neither
`ref('x')` nor `is_incremental()`.

Listing the package scopes that text out. The rule is positional, not lexical:
a block scalar inside a resource of a listed package is not rendered.

```yaml
runtime:
  name: yaml
  options:
    providerTemplatedPackages: [gcpx]
```

```yaml
resources:
  dailyRevenue:
    type: gcpx:dbt/model:Model
    properties:
      project: {{ config.gcpProject }}     # rendered here, as always
      sql: |                               # not rendered here at all
        {{ config(materialized='incremental', unique_key='outage_id') }}
        SELECT * FROM {{ ref('stg_outages') }}
        {% if is_incremental() %}
        WHERE updated_at > (SELECT MAX(updated_at) FROM {{ this }})
        {% endif %}
```

`PULUMI_YAML_PROVIDER_TEMPLATED_PACKAGES` (comma separated) overrides the
project file, for turning the scope on or off without editing a stack.

Scope and limits:

- The unit is the **block scalar**, whose extent YAML defines exactly. A plain
  scalar in the same resource still renders, and single-line SQL
  (`sql: "SELECT {{ ref('x') }}"`) is not covered — passthrough mode already
  handles the expression-shaped constructs a one-liner holds.
- Resources **generated** by a `{% for %}` loop are not covered: the text does
  not exist when the pre-pass runs.
- Where an extent cannot be determined with certainty — a tab in the
  indentation, an indicator that disagrees with its content — the file is
  refused with the line named, rather than rendered on a guess. Silently
  altered SQL is the failure worth avoiding; a rejected file is not.
- Listing nothing, the default, leaves rendering byte-for-byte as it was.

**Comments are not exempt.** Rendering happens over the file as text, before
anything is parsed, so a YAML comment is just more text. `# see {{ ref('x') }}`
is a template the runtime will try to evaluate, and `# wrap it in {% raw %}`
opens a raw block that swallows everything after it. Scoping does not help:
a comment sits outside the block scalar it describes. Write the construct
without its delimiters, or put the comment inside the scoped scalar.

## Test

```bash
cargo test --workspace
```

## Benchmark

```bash
cargo bench --workspace
```

## Fuzz

```bash
cd fuzz
cargo +nightly fuzz run fuzz_yaml_parser -- -max_total_time=60
```

Core targets: `fuzz_yaml_parser`, `fuzz_interpolation`, `fuzz_jinja`, `fuzz_builtins`, `fuzz_converter`, `fuzz_yaml_bomb`, `fuzz_extra_context`, `fuzz_starlark`, `fuzz_parallel_eval`, `fuzz_resource_graph`, `fuzz_sql_lineage`, `fuzz_native_str`, `fuzz_checkpoint`.

Provider-scope targets, which check the extent detection above:
`fuzz_scope_roundtrip` (protect and restore are exact inverses),
`fuzz_scope_oracle` (extents match what `serde_yaml` sees),
`fuzz_scope_render_identity` (protected bytes survive a real render),
`fuzz_scope_unlisted_noop` (an unlisted package changes nothing),
`fuzz_scope_settings` (the option reader cannot over-read),
`fuzz_scope_never_panics`.

Nineteen targets in all — the two lists above together are exactly the `[[bin]]`
entries in `fuzz/Cargo.toml`, which the badge counts and which a CI job holds to
the workflow matrix. `scope_grammar.rs` sits alongside them as a shared input
generator rather than a target, which is why it is not registered.

`SCOPE_FUZZ_TRACE=1` makes `fuzz_scope_oracle` report how many inputs reach
each stage, so its coverage can be checked rather than assumed.

## Checkpoint index

A stack's checkpoint records which physical resources that stack manages, so
a tool asking "does another stack already own this id?" has to read sibling
checkpoints out of the shared state backend. `index_checkpoint` reads one
document into its `(id, urn)` pairs without copying it, and
`index_checkpoints` reads a whole backend's worth on a scoped thread pool.

```python
from pulumi_yaml_rs import index_checkpoint, index_checkpoints

blob = open("app-dev.json", "rb").read()
index_checkpoint(blob)
# {"shape": "resources", "entries": [("projects/p/locations/l/workflows/w", "urn:pulumi:dev::app::gcp:workflows/workflow:Workflow::w")]}

# Only the ids you care about; a target may be the full id or its leaf alone.
index_checkpoint(blob, ["w"])

# One dict per input, in input order. A document that is not a checkpoint
# occupies its own slot as {"error": "Not a Pulumi checkpoint: ..."}, so one
# bad file never hides the rest. parallel=0 asks for available parallelism.
index_checkpoints([blob, other], targets=None, parallel=0)
```

Both encodings are read: `checkpoint.latest.resources`, which is what a
backend stores, and `deployment.resources`, which is what an export writes.
`shape` is `"empty"` when `checkpoint.latest` is absent or null — a stack that
has never deployed — as against `"resources"` with no entries, a deployed
stack that manages nothing.

The contract is to fail loudly. Anything that cannot be read with certainty
raises a `ValueError` prefixed `Not a Pulumi checkpoint:` — an unrecognised
version, a missing deployment, both encodings at once, invalid JSON. The
caller is an ownership gate, where an empty index and an unread document are
the same value and the second one authorises a delete, so there is no input
for which this answers with a confident empty result.

Both entry points release the GIL for the parse, so a caller reading a bucket
from many threads never queues its parses behind one another.

It is a pure function of the bytes it is given: nothing reads a file, the
network, or a plugin.

## Graph export (BigQuery Graph)

The language host can export a stack's resource dependency graph and, on top
of it, BigQuery table/column-level SQL lineage — both keyed for cross-stack
joins in a shared graph store:

```bash
pulumi-language-yaml graph --stack prod --organization my-org --lineage \
  --format ndjson --out ./graph-export
```

See [GRAPH.md](GRAPH.md) for the ID contracts, BigQuery DDL, loading and
refresh flow, property-graph definitions, and sample GQL queries.

## Security

See [SECURITY.md](SECURITY.md) for vulnerability reporting and security details.

## License

Copyright (c) 2024-2026 Lituus-io. Dual-licensed under AGPL-3.0-or-later and a commercial license. See [LICENSE](LICENSE) for details.

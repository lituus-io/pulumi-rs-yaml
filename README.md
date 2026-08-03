# pulumi-rs-yaml

[![CI](https://github.com/lituus-io/pulumi-rs-yaml/actions/workflows/ci.yml/badge.svg)](https://github.com/lituus-io/pulumi-rs-yaml/actions/workflows/ci.yml)
[![Security](https://github.com/lituus-io/pulumi-rs-yaml/actions/workflows/security.yml/badge.svg)](https://github.com/lituus-io/pulumi-rs-yaml/actions/workflows/security.yml)
[![Fuzz](https://github.com/lituus-io/pulumi-rs-yaml/actions/workflows/fuzz.yml/badge.svg)](https://github.com/lituus-io/pulumi-rs-yaml/actions/workflows/fuzz.yml)
[![Benchmark](https://github.com/lituus-io/pulumi-rs-yaml/actions/workflows/benchmark.yml/badge.svg)](https://github.com/lituus-io/pulumi-rs-yaml/actions/workflows/benchmark.yml)
[![fuzz targets](https://img.shields.io/badge/fuzz%20targets-11-blue)](fuzz/fuzz_targets)
[![security tests](https://img.shields.io/badge/security%20tests-81-blue)](crates/pulumi-rs-yaml-core/tests/security_tests.rs)
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

Targets: `fuzz_yaml_parser`, `fuzz_interpolation`, `fuzz_jinja`, `fuzz_builtins`, `fuzz_converter`, `fuzz_yaml_bomb`, `fuzz_extra_context`, `fuzz_starlark`, `fuzz_parallel_eval`, `fuzz_resource_graph`, `fuzz_sql_lineage`.

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

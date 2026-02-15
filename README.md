# pulumi-rs-yaml

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

Targets: `fuzz_yaml_parser`, `fuzz_interpolation`, `fuzz_jinja`, `fuzz_builtins`, `fuzz_converter`, `fuzz_yaml_bomb`.

## Security

See [SECURITY.md](SECURITY.md) for vulnerability reporting and security details.

## License

Copyright (c) 2024-2026 Lituus-io. Dual-licensed under AGPL-3.0-or-later and a commercial license. See [LICENSE](LICENSE) for details.

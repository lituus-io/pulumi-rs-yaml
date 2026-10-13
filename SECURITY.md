# Security Policy

## Reporting Vulnerabilities

Report security vulnerabilities privately to **spicyzhug@gmail.com**. Include a description, reproduction steps, and impact assessment. You will receive an acknowledgement within 48 hours.

Do **not** open public issues for security vulnerabilities.

## Security Measures

### Dependency auditing

- **cargo-audit** runs weekly and on every push/PR to detect known CVEs in dependencies.
- **cargo-deny** checks licenses, bans unsafe crates, and detects duplicate dependencies.

### Static analysis

- **Clippy** runs with `-D warnings` on every push/PR.
- Security-focused Clippy lints (`unwrap_used`, `expect_used`, `panic`) run in the security workflow as advisory warnings (`-W`); they surface findings but do not fail the build.

### Fuzz testing

The core fuzz targets are listed below; `fuzz/Cargo.toml` registers all twenty-two:

| Target | Coverage |
|--------|----------|
| `fuzz_yaml_parser` | YAML parsing and template extraction |
| `fuzz_interpolation` | `${}` interpolation and expression evaluation |
| `fuzz_jinja` | Jinja `{% %}` / `{{ }}` block processing |
| `fuzz_builtins` | Built-in function evaluation (fn::select, fn::join, etc.) |
| `fuzz_converter` | YAML-to-PCL converter |
| `fuzz_yaml_bomb` | Exponential expansion / billion laughs detection |
| `fuzz_extra_context` | Jinja extra context vars and builtin-override protection |
| `fuzz_starlark` | Starlark compilation and `fn::starlark` evaluation |
| `fuzz_parallel_eval` | Parallel vs. sequential evaluator differential testing |
| `fuzz_resource_graph` | Dependency-graph export: determinism, ordering, URN construction |
| `fuzz_sql_lineage` | SQL lineage export, the SQL parser boundary, and the statement layer |
| `fuzz_sql_statements` | Statement splitting and classification on author text: every range in bounds and on a character boundary |

Fuzz tests run weekly in CI and can be triggered manually.

### Build hardening

- `panic = "abort"` eliminates unwinding attack surface in release builds.
- `strip = "symbols"` removes debug symbols from release binaries.
- `lto = true` and `codegen-units = 1` enable cross-crate optimization.
- Linux binaries use musl for fully static linking (no glibc dependency).
- Windows binaries use static CRT linking.

### Input validation

- YAML parsing uses `serde_yaml` (safe by default, no arbitrary deserialization).
- Jinja block syntax is validated before template rendering.
- Interpolation expressions are parsed with a bounded recursive descent parser.
- `readFile()` is restricted to the project directory (no path traversal), including static SQL reads by the lineage exporter.
- SQL handed to the parser is bounded by size and bracket-nesting depth, and parsed on a worker thread with a large explicit stack — deep recursion cannot abort the process under `panic = "abort"`.
- Statement splitting slices author text only at ASCII delimiters, which are always character boundaries, so no SQL text can make a slice panic.
- The lineage source store reads `fn::readFile` files up front under the same containment as the exporter, refuses absolute paths, symlink escapes and files over 16 MiB, and holds nothing it could not decode as UTF-8.

### Exported SQL text

The lineage statement layer (`--sql-statements`, `include_statements=True`) exports the full text of every SQL statement a program carries. SQL can hold credentials, connection strings and literals an author never meant to publish, so the layer is **off by default**, and nothing in the default output contains statement text. Enable it only for destinations that may hold the program's source.

## Supported Versions

| Version | Supported |
|---------|-----------|
| 0.5.x   | Yes       |
| < 0.5   | No        |

## License

Copyright (c) 2024-2026 Lituus-io. All rights reserved. See [LICENSE](LICENSE).

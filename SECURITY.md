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
- Security-focused Clippy lints (`unwrap_used`, `expect_used`, `panic`) run in the security workflow.

### Fuzz testing

Six fuzz targets cover the attack surface:

| Target | Coverage |
|--------|----------|
| `fuzz_yaml_parser` | YAML parsing and template extraction |
| `fuzz_interpolation` | `${}` interpolation and expression evaluation |
| `fuzz_jinja` | Jinja `{% %}` / `{{ }}` block processing |
| `fuzz_builtins` | Built-in function evaluation (fn::select, fn::join, etc.) |
| `fuzz_converter` | YAML-to-PCL converter |
| `fuzz_yaml_bomb` | Exponential expansion / billion laughs detection |

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
- `readFile()` is restricted to the project directory (no path traversal).

## Supported Versions

| Version | Supported |
|---------|-----------|
| 0.2.x | Yes |

## License

Copyright (c) 2024-2026 Lituus-io. All rights reserved. See [LICENSE](LICENSE).

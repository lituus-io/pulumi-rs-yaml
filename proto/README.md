# Proto definitions

Protobuf/gRPC definitions for the Pulumi RPC interfaces consumed and implemented
by this workspace. Vendored from [pulumi/pulumi](https://github.com/pulumi/pulumi)
`proto/` at **v3.228.0** (Apache-2.0, upstream license headers retained).

## Relationship to the generated code

The Rust stubs in `crates/pulumi-rs-yaml-proto/src/generated/` are
**pre-generated and checked in** — there is no `build.rs`. The generated RPC
surface matches the files here exactly:

| Proto | Service | Generated module |
|-------|---------|------------------|
| `pulumi/language.proto` | `LanguageRuntime` (17 RPCs) | `pulumirpc` |
| `pulumi/provider.proto` | `ResourceProvider` (20 RPCs) | `pulumirpc` |
| `pulumi/resource.proto` | `ResourceMonitor` (12 RPCs) | `pulumirpc` |
| `pulumi/engine.proto` | `Engine` | `pulumirpc` |
| `pulumi/converter.proto` | `Converter` | `pulumirpc` |
| `pulumi/analyzer.proto` | `Analyzer` | `pulumirpc` |
| `pulumi/callback.proto` | `Callbacks` | `pulumirpc` |
| `pulumi/codegen/hcl.proto` | (messages only) | `pulumirpc::codegen` |
| `pulumi/codegen/loader.proto` | `Loader` | `codegen` |

## Regenerating

Regeneration is a deliberate, manual step (do not add a `build.rs`). Use
`tonic-build`/`prost-build` matching the versions in the workspace
`Cargo.toml` (`tonic-build = "0.12"`, `prost = "0.13"`), e.g. from a small
throwaway build script or `protoc` invocation:

```rust
tonic_build::configure()
    .build_server(true)
    .build_client(true)
    .out_dir("crates/pulumi-rs-yaml-proto/src/generated")
    .compile_protos(
        &[
            "proto/pulumi/language.proto",
            "proto/pulumi/provider.proto",
            "proto/pulumi/resource.proto",
            "proto/pulumi/engine.proto",
            "proto/pulumi/converter.proto",
            "proto/pulumi/analyzer.proto",
            "proto/pulumi/callback.proto",
            "proto/pulumi/codegen/hcl.proto",
            "proto/pulumi/codegen/loader.proto",
        ],
        &["proto"],
    )?;
```

When bumping the vendored protos to a newer upstream tag, verify the RPC
method sets still match what the crates implement (`server.rs`,
`component_provider.rs`, `clients.rs`) before regenerating — upstream adds
RPCs regularly (e.g. `RunPlugin2`, `ResourceProvider.List`,
`ResourceMonitor.GetDeploymentInfo` appeared after v3.228.0).

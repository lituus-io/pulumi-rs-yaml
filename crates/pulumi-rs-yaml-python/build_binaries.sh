#!/usr/bin/env bash
set -euo pipefail

# Build language and converter binaries, then copy into the Python package
# bin/ directory so maturin includes them in the wheel.
#
# Set CARGO_BUILD_TARGET to cross-compile (e.g. x86_64-apple-darwin).

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
BIN_DIR="$SCRIPT_DIR/python/pulumi_yaml_rs/bin"

mkdir -p "$BIN_DIR"

TARGET_FLAG=""
RELEASE_DIR="$WORKSPACE_ROOT/target/release"
if [[ -n "${CARGO_BUILD_TARGET:-}" ]]; then
    TARGET_FLAG="--target $CARGO_BUILD_TARGET"
    RELEASE_DIR="$WORKSPACE_ROOT/target/$CARGO_BUILD_TARGET/release"
fi

echo "Building pulumi-language-yaml and pulumi-converter-yaml..."
cargo build --release --manifest-path "$WORKSPACE_ROOT/Cargo.toml" \
    -p pulumi-rs-yaml-language \
    -p pulumi-rs-yaml-converter \
    $TARGET_FLAG

# Determine binary suffix
EXE_SUFFIX=""
if [[ "${CARGO_BUILD_TARGET:-}" == *windows* ]] || [[ "$(uname -s)" == *MINGW* ]] || [[ "$(uname -s)" == *MSYS* ]] || [[ "${OS:-}" == "Windows_NT" ]]; then
    EXE_SUFFIX=".exe"
fi

echo "Copying binaries to $BIN_DIR..."
cp "$RELEASE_DIR/pulumi-language-yaml${EXE_SUFFIX}" "$BIN_DIR/"
cp "$RELEASE_DIR/pulumi-converter-yaml${EXE_SUFFIX}" "$BIN_DIR/"

echo "Done. Binaries:"
ls -lh "$BIN_DIR"/pulumi-*

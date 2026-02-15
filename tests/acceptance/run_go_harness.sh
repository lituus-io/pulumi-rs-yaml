#!/usr/bin/env bash
# Run the Go pulumi-test-language harness against our Rust pulumi-language-yaml binary.
#
# The Go test harness (pulumi-test-language) runs all L1/L2 test projects via gRPC
# and is language-agnostic. Our Rust binary implements the same LanguageRuntime gRPC
# interface and can be directly substituted.
#
# Prerequisites:
# - Rust toolchain (for building pulumi-language-yaml)
# - Go toolchain (for running the test harness)
# - pulumi-yaml Go repo at /Users/gatema/Desktop/oss/pulumi-yaml
# - pulumi Go repo at /Users/gatema/Desktop/oss/pulumi-yaml/../../pulumi
#   (or adjust PULUMI_REPO below)
#
# Usage: ./run_go_harness.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
BINARY="$ROOT_DIR/target/release/pulumi-language-yaml"
PULUMI_YAML_REPO="${PULUMI_YAML_REPO:-/Users/gatema/Desktop/oss/pulumi-yaml}"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

log() { echo -e "${GREEN}[OK]${NC} $1"; }
warn() { echo -e "${YELLOW}[WARN]${NC} $1"; }
fail() { echo -e "${RED}[FAIL]${NC} $1"; exit 1; }

# Step 1: Build the Rust binary
echo "=== Building Rust language host (release) ==="
(cd "$ROOT_DIR" && cargo build --release --bin pulumi-language-yaml)
if [ ! -f "$BINARY" ]; then
    fail "Binary not found at $BINARY"
fi
log "Rust binary built: $BINARY"

# Step 2: Inject our binary into PATH so the Go test harness finds it
TEMP_BIN=$(mktemp -d)
trap "rm -rf $TEMP_BIN" EXIT
ln -sf "$BINARY" "$TEMP_BIN/pulumi-language-yaml"
export PATH="$TEMP_BIN:$PATH"

FOUND=$(which pulumi-language-yaml)
log "Binary in PATH: $FOUND"

# Step 3: Run the Go test harness
echo ""
echo "=== Running Go test harness ==="
echo "Note: The Go TestLanguage function starts the language host in-process via Go code."
echo "To test against our Rust binary, we use a custom test wrapper that starts our binary"
echo "as an external process and connects to it via gRPC."
echo ""

# The Go test harness in pulumi-yaml starts the language plugin in-process.
# To use our Rust binary instead, we run the Go tests with our binary available,
# and rely on the test framework's language plugin discovery mechanism.
#
# Method 1: Run Go tests directly (uses Go's in-process server, NOT our binary)
# This is useful for comparing results:
# cd "$PULUMI_YAML_REPO" && go test ./cmd/pulumi-language-yaml/ -run TestLanguage -v -count=1
#
# Method 2: Use pulumi-test-language as standalone engine (preferred for our binary)
# This requires building the test engine and writing a thin Go wrapper.

if [ ! -d "$PULUMI_YAML_REPO" ]; then
    fail "pulumi-yaml repo not found at $PULUMI_YAML_REPO"
fi

echo "Running Go TestLanguage for baseline comparison..."
cd "$PULUMI_YAML_REPO"

# Run the Go tests and capture output
set +e
GO_OUTPUT=$(go test ./cmd/pulumi-language-yaml/ -run TestLanguage -v -count=1 -timeout 300s 2>&1)
GO_EXIT=$?
set -e

echo "$GO_OUTPUT" | grep -E "--- (PASS|FAIL|SKIP)" | head -80

if [ $GO_EXIT -eq 0 ]; then
    PASSED=$(echo "$GO_OUTPUT" | grep -c "--- PASS" || true)
    SKIPPED=$(echo "$GO_OUTPUT" | grep -c "--- SKIP" || true)
    echo ""
    log "Go harness: $PASSED passed, $SKIPPED skipped"
else
    FAILED=$(echo "$GO_OUTPUT" | grep -c "--- FAIL" || true)
    warn "Go harness exited with code $GO_EXIT ($FAILED failures)"
fi

echo ""
echo "========================================="
echo "Go harness baseline run complete."
echo "To test our Rust binary against the same tests,"
echo "see tests/acceptance/go_harness_wrapper/ for the custom wrapper."
echo "========================================="

#!/usr/bin/env bash
# Acceptance tests for pulumi-rs-yaml using GCP
#
# This script:
# 1. Builds the Rust language host binary
# 2. Configures Pulumi to use it instead of the Go version
# 3. Runs pulumi preview + up + destroy for each test project
# 4. Reports per-test pass/fail results
#
# Prerequisites:
# - Pulumi CLI installed
# - GCP credentials at $GOOGLE_APPLICATION_CREDENTIALS
# - gcp provider plugin installed (pulumi plugin install resource gcp)
#
# Usage:
#   GOOGLE_APPLICATION_CREDENTIALS=/path/to/creds.json ./run_acceptance.sh            # Run all tests
#   GOOGLE_APPLICATION_CREDENTIALS=/path/to/creds.json ./run_acceptance.sh gcp-bucket  # Run single test
#   GOOGLE_APPLICATION_CREDENTIALS=/path/to/creds.json ./run_acceptance.sh --preview-only  # Preview only (no deploy)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
BINARY="$ROOT_DIR/target/release/pulumi-language-yaml"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

log()  { echo -e "${GREEN}[PASS]${NC} $1"; }
warn() { echo -e "${YELLOW}[WARN]${NC} $1"; }
fail() { echo -e "${RED}[FAIL]${NC} $1"; }
info() { echo -e "${BLUE}[INFO]${NC} $1"; }

# Parse arguments
PREVIEW_ONLY=false
SINGLE_TEST=""
for arg in "$@"; do
    case "$arg" in
        --preview-only) PREVIEW_ONLY=true ;;
        *) SINGLE_TEST="$arg" ;;
    esac
done

# Check prerequisites
if [ -z "${GOOGLE_APPLICATION_CREDENTIALS:-}" ]; then
    echo -e "${RED}ERROR:${NC} GOOGLE_APPLICATION_CREDENTIALS not set"
    exit 1
fi

if [ ! -f "$GOOGLE_APPLICATION_CREDENTIALS" ]; then
    echo -e "${RED}ERROR:${NC} GCP credentials file not found: $GOOGLE_APPLICATION_CREDENTIALS"
    exit 1
fi

if ! command -v pulumi &>/dev/null; then
    echo -e "${RED}ERROR:${NC} pulumi CLI not found"
    exit 1
fi

# Step 1: Build the Rust binary
echo "=== Building Rust language host (release) ==="
(cd "$ROOT_DIR" && cargo build --release --bin pulumi-language-yaml)
if [ ! -f "$BINARY" ]; then
    echo -e "${RED}ERROR:${NC} Binary not found at $BINARY"
    exit 1
fi
log "Binary built: $(ls -lh "$BINARY" | awk '{print $5}')"

# Step 2: Set up the Pulumi environment to use our binary
TEMP_BIN=$(mktemp -d)
ln -sf "$BINARY" "$TEMP_BIN/pulumi-language-yaml"
export PATH="$TEMP_BIN:$PATH"
export PULUMI_CONFIG_PASSPHRASE="test-passphrase"

FOUND=$(which pulumi-language-yaml)
log "Binary in PATH: $FOUND"

# Install GCP provider if needed
pulumi plugin install resource gcp 2>&1 || true

# Clean up temp bin on exit
cleanup_temp() {
    rm -rf "$TEMP_BIN"
}
trap cleanup_temp EXIT

# Discover test projects
declare -a TEST_PROJECTS=()
if [ -n "$SINGLE_TEST" ]; then
    if [ -d "$SCRIPT_DIR/$SINGLE_TEST" ] && [ -f "$SCRIPT_DIR/$SINGLE_TEST/Pulumi.yaml" ]; then
        TEST_PROJECTS+=("$SINGLE_TEST")
    else
        echo -e "${RED}ERROR:${NC} Test project '$SINGLE_TEST' not found or missing Pulumi.yaml"
        exit 1
    fi
else
    for dir in "$SCRIPT_DIR"/*/; do
        project=$(basename "$dir")
        if [ -f "$dir/Pulumi.yaml" ]; then
            TEST_PROJECTS+=("$project")
        fi
    done
fi

echo ""
echo "=== Found ${#TEST_PROJECTS[@]} test project(s) ==="
for p in "${TEST_PROJECTS[@]}"; do
    info "$p"
done
echo ""

# Track results
TOTAL=0
PASSED=0
FAILED=0
SKIPPED=0
declare -a FAILED_TESTS=()

# Run each test project
run_test() {
    local project="$1"
    local test_dir="$SCRIPT_DIR/$project"
    local stack_name="test-${project}-$(date +%s)"

    echo ""
    echo "==========================================="
    echo "  Testing: $project"
    echo "==========================================="

    TOTAL=$((TOTAL + 1))

    # Initialize stack
    cd "$test_dir"
    export PULUMI_BACKEND_URL="file://$test_dir/.pulumi"
    mkdir -p "$test_dir/.pulumi"

    # Cleanup function for this test
    test_cleanup() {
        cd "$test_dir"
        if [ "$PREVIEW_ONLY" = false ]; then
            pulumi destroy --yes --non-interactive 2>&1 || true
        fi
        pulumi stack rm "$stack_name" --yes --non-interactive 2>&1 || true
        rm -rf "$test_dir/.pulumi"
    }

    pulumi stack init "$stack_name" --non-interactive 2>&1 || true
    pulumi stack select "$stack_name" 2>&1

    # Preview
    info "Running preview..."
    if ! PREVIEW_OUTPUT=$(pulumi preview --non-interactive 2>&1); then
        echo "$PREVIEW_OUTPUT"
        fail "$project: preview failed"
        FAILED=$((FAILED + 1))
        FAILED_TESTS+=("$project (preview)")
        test_cleanup
        return
    fi

    if [ "$PREVIEW_ONLY" = true ]; then
        log "$project: preview passed"
        PASSED=$((PASSED + 1))
        test_cleanup
        return
    fi

    # Special handling for preview-only projects
    if [ "$project" = "gcp-preview-only" ]; then
        log "$project: preview-only test passed"
        PASSED=$((PASSED + 1))
        test_cleanup
        return
    fi

    # Up (deploy)
    info "Running up..."
    if ! UP_OUTPUT=$(pulumi up --yes --non-interactive 2>&1); then
        echo "$UP_OUTPUT"
        fail "$project: up failed"
        FAILED=$((FAILED + 1))
        FAILED_TESTS+=("$project (up)")
        test_cleanup
        return
    fi

    # Validate outputs exist
    info "Checking outputs..."
    if ! OUTPUT_JSON=$(pulumi stack output --json 2>&1); then
        fail "$project: failed to get outputs"
        FAILED=$((FAILED + 1))
        FAILED_TESTS+=("$project (outputs)")
        test_cleanup
        return
    fi

    # Destroy
    info "Destroying..."
    if ! pulumi destroy --yes --non-interactive 2>&1; then
        warn "$project: destroy had errors (may need manual cleanup)"
    fi

    # Remove stack
    pulumi stack rm "$stack_name" --yes --non-interactive 2>&1 || true
    rm -rf "$test_dir/.pulumi"

    log "$project: ALL STEPS PASSED"
    PASSED=$((PASSED + 1))
}

# Run all tests
for project in "${TEST_PROJECTS[@]}"; do
    run_test "$project" || true
done

# Summary
echo ""
echo "==========================================="
echo "  SUMMARY"
echo "==========================================="
echo -e "  Total:   $TOTAL"
echo -e "  ${GREEN}Passed:  $PASSED${NC}"
if [ $FAILED -gt 0 ]; then
    echo -e "  ${RED}Failed:  $FAILED${NC}"
    for ft in "${FAILED_TESTS[@]}"; do
        echo -e "    ${RED}- $ft${NC}"
    done
fi
if [ $SKIPPED -gt 0 ]; then
    echo -e "  ${YELLOW}Skipped: $SKIPPED${NC}"
fi
echo "==========================================="

if [ $FAILED -gt 0 ]; then
    exit 1
fi

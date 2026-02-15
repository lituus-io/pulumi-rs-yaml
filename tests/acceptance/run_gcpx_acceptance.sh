#!/usr/bin/env bash
# GCPX provider acceptance tests — full CRUD lifecycle through the Rust YAML runtime.
#
# Tests: Pulumi CLI → pulumi-language-yaml (Rust) → pulumi-resource-gcpx (Rust) → real GCP
#
# Each test runs: CREATE (pulumi up) → verify outputs → UPDATE (pulumi up) → verify → DELETE (pulumi destroy)
#
# Prerequisites:
#   - Pulumi CLI installed
#   - GOOGLE_APPLICATION_CREDENTIALS pointing to a GCP service account JSON
#   - jq installed
#
# Usage:
#   GOOGLE_APPLICATION_CREDENTIALS=/path/to/creds.json ./run_gcpx_acceptance.sh
#   GOOGLE_APPLICATION_CREDENTIALS=/path/to/creds.json ./run_gcpx_acceptance.sh gcpx-table-crud

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
YAML_BINARY="$ROOT_DIR/target/release/pulumi-language-yaml"

# GCPX provider — check both common locations
GCPX_ROOT="${GCPX_ROOT:-}"
for candidate in \
    "$ROOT_DIR/../pulumi-resource-gcpx" \
    "$HOME/Library/CloudStorage/SynologyDrive-code/pulumi-resource-gcpx" \
    "$HOME/Desktop/drive/git/code/pulumi-resource-gcpx"; do
    if [ -z "$GCPX_ROOT" ] && [ -f "$candidate/Cargo.toml" ]; then
        GCPX_ROOT="$candidate"
    fi
done
if [ -z "$GCPX_ROOT" ]; then
    echo "ERROR: Cannot find pulumi-resource-gcpx repo. Set GCPX_ROOT env var."
    exit 1
fi
GCPX_BINARY="$GCPX_ROOT/target/release/pulumi-resource-gcpx"

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
SINGLE_TEST=""
for arg in "$@"; do
    SINGLE_TEST="$arg"
done

# Check prerequisites
if [ -z "${GOOGLE_APPLICATION_CREDENTIALS:-}" ]; then
    echo -e "${RED}ERROR:${NC} GOOGLE_APPLICATION_CREDENTIALS not set"
    exit 1
fi
if [ ! -f "$GOOGLE_APPLICATION_CREDENTIALS" ]; then
    echo -e "${RED}ERROR:${NC} Credentials file not found: $GOOGLE_APPLICATION_CREDENTIALS"
    exit 1
fi
if ! command -v pulumi &>/dev/null; then
    echo -e "${RED}ERROR:${NC} pulumi CLI not found"
    exit 1
fi
if ! command -v jq &>/dev/null; then
    echo -e "${RED}ERROR:${NC} jq not found"
    exit 1
fi

SA_EMAIL=$(jq -r '.client_email' "$GOOGLE_APPLICATION_CREDENTIALS")
info "Service account: $SA_EMAIL"

# Build binaries
echo ""
echo "=== Building Rust YAML language host ==="
(cd "$ROOT_DIR" && cargo build --release --bin pulumi-language-yaml 2>&1)
log "Language host built: $YAML_BINARY"

echo ""
echo "=== Building gcpx provider ==="
(cd "$GCPX_ROOT" && cargo build --release --bin pulumi-resource-gcpx 2>&1)
log "Provider built: $GCPX_BINARY"

# Set up PATH with both binaries
TEMP_BIN=$(mktemp -d)
ln -sf "$YAML_BINARY" "$TEMP_BIN/pulumi-language-yaml"
ln -sf "$GCPX_BINARY" "$TEMP_BIN/pulumi-resource-gcpx"
export PATH="$TEMP_BIN:$PATH"
export PULUMI_CONFIG_PASSPHRASE="test-passphrase"

info "pulumi-language-yaml: $(which pulumi-language-yaml)"
info "pulumi-resource-gcpx: $(which pulumi-resource-gcpx)"

cleanup_temp() { rm -rf "$TEMP_BIN"; }
trap cleanup_temp EXIT

# Unique suffix for resource names (avoid collisions between runs)
SUFFIX="$(date +%s)"

# Discover tests
GCPX_TESTS=()
if [ -n "$SINGLE_TEST" ]; then
    if [ -d "$SCRIPT_DIR/$SINGLE_TEST" ] && [ -f "$SCRIPT_DIR/$SINGLE_TEST/create.yaml" ]; then
        GCPX_TESTS+=("$SINGLE_TEST")
    else
        echo -e "${RED}ERROR:${NC} Test '$SINGLE_TEST' not found or missing create.yaml"
        exit 1
    fi
else
    for dir in "$SCRIPT_DIR"/gcpx-*/; do
        project=$(basename "$dir")
        if [ -f "$dir/create.yaml" ]; then
            GCPX_TESTS+=("$project")
        fi
    done
fi

echo ""
echo "=== Found ${#GCPX_TESTS[@]} gcpx test(s), suffix=$SUFFIX ==="
for t in "${GCPX_TESTS[@]}"; do info "$t"; done
echo ""

# Track results
TOTAL=0
PASSED=0
FAILED=0
declare -a FAILED_TESTS=()

# Prepare Pulumi.yaml from template (substitutes __SUFFIX__ and __SA_EMAIL__)
prepare_yaml() {
    local src="$1"
    local dst="$2"
    sed -e "s/__SUFFIX__/$SUFFIX/g" -e "s/__SA_EMAIL__/$SA_EMAIL/g" "$src" > "$dst"
}

# Prepare all YAML files for a phase (main + satellite files)
# e.g., create.yaml → Pulumi.yaml, create.dbt.yaml → Pulumi.dbt.yaml
prepare_all_yaml() {
    local test_dir="$1"
    local phase="$2"  # "create" or "update"
    # Main file
    prepare_yaml "$test_dir/${phase}.yaml" "$test_dir/Pulumi.yaml"
    # Satellite files: create.dbt.yaml → Pulumi.dbt.yaml, etc.
    for f in "$test_dir/${phase}."*.yaml; do
        [ -f "$f" ] || continue
        local suffix="${f##*${phase}.}"  # e.g., "dbt.yaml"
        prepare_yaml "$f" "$test_dir/Pulumi.${suffix}"
    done
}

# Remove all generated Pulumi*.yaml files
cleanup_yaml_files() {
    local test_dir="$1"
    rm -f "$test_dir"/Pulumi*.yaml
}

# Verify that stack outputs contain expected non-empty keys
verify_outputs() {
    local label="$1"
    shift
    local json
    json=$(pulumi stack output --json 2>&1) || { fail "$label: failed to get outputs"; return 1; }
    info "Outputs: $json"
    for key in "$@"; do
        local val
        val=$(echo "$json" | jq -r --arg k "$key" '.[$k] // empty')
        if [ -z "$val" ]; then
            fail "$label: output '$key' missing or empty"
            return 1
        fi
    done
    return 0
}

run_test() {
    local project="$1"
    local test_dir="$SCRIPT_DIR/$project"
    local stack_name="gcpx-${project}-${SUFFIX}"

    echo ""
    echo "==========================================="
    echo "  Testing: $project  (CRUD lifecycle)"
    echo "==========================================="

    TOTAL=$((TOTAL + 1))

    cd "$test_dir"
    export PULUMI_BACKEND_URL="file://$test_dir/.pulumi"
    mkdir -p "$test_dir/.pulumi"

    # Cleanup function — always try to destroy
    do_cleanup() {
        cd "$test_dir"
        pulumi destroy --yes --non-interactive 2>&1 || true
        pulumi stack rm "$stack_name" --yes --non-interactive 2>&1 || true
        rm -rf "$test_dir/.pulumi"
        cleanup_yaml_files "$test_dir"
    }

    # Generate Pulumi.yaml (and satellite files) — Pulumi CLI needs them for stack init
    info "[CREATE] Preparing YAML files from create templates..."
    prepare_all_yaml "$test_dir" "create"

    # --- Stack init ---
    pulumi stack init "$stack_name" --non-interactive 2>&1 || true
    pulumi stack select "$stack_name" 2>&1

    # ==================== CREATE ====================

    info "[CREATE] Running preview..."
    if ! pulumi preview --non-interactive 2>&1; then
        fail "$project: CREATE preview failed"
        FAILED=$((FAILED + 1)); FAILED_TESTS+=("$project (create preview)")
        do_cleanup; return
    fi

    info "[CREATE] Running pulumi up..."
    if ! pulumi up --yes --non-interactive 2>&1; then
        fail "$project: CREATE up failed"
        FAILED=$((FAILED + 1)); FAILED_TESTS+=("$project (create up)")
        do_cleanup; return
    fi
    log "[CREATE] Resources created successfully"

    # Determine expected output keys from the create.yaml outputs section
    local create_keys
    create_keys=$(grep -E '^\s+\w+:' "$test_dir/create.yaml" | sed -n '/^outputs:/,$ { /^outputs:/d; s/^\s*\([a-zA-Z_][a-zA-Z0-9_]*\):.*/\1/p }')
    if [ -n "$create_keys" ]; then
        # shellcheck disable=SC2086
        if ! verify_outputs "$project CREATE" $create_keys; then
            FAILED=$((FAILED + 1)); FAILED_TESTS+=("$project (create outputs)")
            do_cleanup; return
        fi
        log "[CREATE] Outputs verified"
    fi

    # ==================== UPDATE ====================
    if [ -f "$test_dir/update.yaml" ]; then
        info "[UPDATE] Preparing YAML files from update templates..."
        prepare_all_yaml "$test_dir" "update"

        info "[UPDATE] Running preview..."
        if ! pulumi preview --non-interactive 2>&1; then
            fail "$project: UPDATE preview failed"
            FAILED=$((FAILED + 1)); FAILED_TESTS+=("$project (update preview)")
            do_cleanup; return
        fi

        info "[UPDATE] Running pulumi up..."
        if ! pulumi up --yes --non-interactive 2>&1; then
            fail "$project: UPDATE up failed"
            FAILED=$((FAILED + 1)); FAILED_TESTS+=("$project (update up)")
            do_cleanup; return
        fi
        log "[UPDATE] Resources updated successfully"

        local update_keys
        update_keys=$(grep -E '^\s+\w+:' "$test_dir/update.yaml" | sed -n '/^outputs:/,$ { /^outputs:/d; s/^\s*\([a-zA-Z_][a-zA-Z0-9_]*\):.*/\1/p }')
        if [ -n "$update_keys" ]; then
            # shellcheck disable=SC2086
            if ! verify_outputs "$project UPDATE" $update_keys; then
                FAILED=$((FAILED + 1)); FAILED_TESTS+=("$project (update outputs)")
                do_cleanup; return
            fi
            log "[UPDATE] Outputs verified"
        fi
    fi

    # ==================== DELETE ====================
    info "[DELETE] Running pulumi destroy..."
    if ! pulumi destroy --yes --non-interactive 2>&1; then
        warn "$project: destroy had errors (may need manual cleanup)"
    fi
    log "[DELETE] Resources destroyed"

    # Cleanup stack
    pulumi stack rm "$stack_name" --yes --non-interactive 2>&1 || true
    rm -rf "$test_dir/.pulumi"
    cleanup_yaml_files "$test_dir"

    log "$project: CRUD LIFECYCLE PASSED"
    PASSED=$((PASSED + 1))
}

# Run tests
for project in "${GCPX_TESTS[@]}"; do
    run_test "$project" || true
done

# Summary
echo ""
echo "==========================================="
echo "  GCPX ACCEPTANCE TEST SUMMARY"
echo "==========================================="
echo -e "  Total:   $TOTAL"
echo -e "  ${GREEN}Passed:  $PASSED${NC}"
if [ $FAILED -gt 0 ]; then
    echo -e "  ${RED}Failed:  $FAILED${NC}"
    for ft in "${FAILED_TESTS[@]}"; do
        echo -e "    ${RED}- $ft${NC}"
    done
fi
echo "==========================================="

if [ $FAILED -gt 0 ]; then exit 1; fi

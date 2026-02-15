#!/usr/bin/env bash
# Two-phase acceptance test for get: resource option
#
# Phase 1: Create a bucket with a known name
# Phase 2: Add a get: resource that reads the bucket back
#
# This is necessary because get: tries to read the resource during preview,
# so the resource must exist in GCP before the get: block is added.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/../../.." && pwd)"
BINARY="$ROOT_DIR/target/release/pulumi-language-yaml"

RED='\033[0;31m'
GREEN='\033[0;32m'
BLUE='\033[0;34m'
NC='\033[0m'

log()  { echo -e "${GREEN}[PASS]${NC} $1"; }
fail() { echo -e "${RED}[FAIL]${NC} $1"; }
info() { echo -e "${BLUE}[INFO]${NC} $1"; }

# Check prerequisites
if [ -z "${GOOGLE_APPLICATION_CREDENTIALS:-}" ]; then
    echo -e "${RED}ERROR:${NC} GOOGLE_APPLICATION_CREDENTIALS not set"
    exit 1
fi

# Build if needed
if [ ! -f "$BINARY" ]; then
    info "Building Rust language host..."
    (cd "$ROOT_DIR" && cargo build --release --bin pulumi-language-yaml)
fi

# Set up PATH
TEMP_BIN=$(mktemp -d)
ln -sf "$BINARY" "$TEMP_BIN/pulumi-language-yaml"
export PATH="$TEMP_BIN:$PATH"
export PULUMI_CONFIG_PASSPHRASE="test-passphrase"

STACK_NAME="test-get-resource-$(date +%s)"

cd "$SCRIPT_DIR"
export PULUMI_BACKEND_URL="file://$SCRIPT_DIR/.pulumi"
mkdir -p "$SCRIPT_DIR/.pulumi"

cleanup() {
    cd "$SCRIPT_DIR"
    # Restore phase 1 yaml and phase2 file
    cp Pulumi.yaml.bak Pulumi.yaml 2>/dev/null || true
    rm -f Pulumi.yaml.bak
    mv phase2.yaml.hidden Pulumi.phase2.yaml 2>/dev/null || true
    pulumi destroy --yes --non-interactive 2>&1 || true
    pulumi stack rm "$STACK_NAME" --yes --non-interactive 2>&1 || true
    rm -rf "$SCRIPT_DIR/.pulumi"
    rm -rf "$TEMP_BIN"
}
trap cleanup EXIT

# Hide phase2 file so multi-file system doesn't pick it up
mv Pulumi.phase2.yaml phase2.yaml.hidden

# Init stack
pulumi stack init "$STACK_NAME" --non-interactive 2>&1 || true
pulumi stack select "$STACK_NAME" 2>&1

# --- Phase 1: Create the bucket ---
info "Phase 1: Creating bucket..."
cp Pulumi.yaml Pulumi.yaml.bak

if ! pulumi up --yes --non-interactive 2>&1; then
    fail "gcp-get-resource: Phase 1 (create bucket) failed"
    exit 1
fi
log "Phase 1: Bucket created"

# --- Phase 2: Add get: resource and update ---
info "Phase 2: Adding get: resource..."
cp phase2.yaml.hidden Pulumi.yaml

info "Phase 2: Running preview..."
if ! pulumi preview --non-interactive 2>&1; then
    fail "gcp-get-resource: Phase 2 preview failed"
    exit 1
fi
log "Phase 2: Preview passed"

info "Phase 2: Running up..."
if ! UP_OUTPUT=$(pulumi up --yes --non-interactive 2>&1); then
    echo "$UP_OUTPUT"
    fail "gcp-get-resource: Phase 2 up failed"
    exit 1
fi
log "Phase 2: Up succeeded"

# Validate outputs
info "Checking outputs..."
OUTPUT_JSON=$(pulumi stack output --json 2>&1)
echo "$OUTPUT_JSON"

# Verify the get: resource returned valid data
if echo "$OUTPUT_JSON" | python3 -c "import sys,json; d=json.load(sys.stdin); assert d.get('existingBucketName') == 'get-resource-test-bucket-rsyaml', f'Expected bucket name, got: {d.get(\"existingBucketName\")}'"; then
    log "get: resource returned correct bucket name"
else
    fail "get: resource did not return expected bucket name"
    exit 1
fi

if echo "$OUTPUT_JSON" | python3 -c "import sys,json; d=json.load(sys.stdin); assert d.get('existingBucketLocation','').upper() == 'US', f'Expected US location, got: {d.get(\"existingBucketLocation\")}'"; then
    log "get: resource returned correct bucket location"
else
    fail "get: resource did not return expected location"
    exit 1
fi

if echo "$OUTPUT_JSON" | python3 -c "import sys,json; d=json.load(sys.stdin); assert d.get('projectId'), f'Missing projectId'"; then
    log "fn::invoke returned project info"
else
    fail "fn::invoke did not return project info"
    exit 1
fi

# Restore original yaml before cleanup (cleanup trap handles the rest)
cp Pulumi.yaml.bak Pulumi.yaml

echo ""
log "gcp-get-resource: ALL STEPS PASSED (two-phase get: test)"

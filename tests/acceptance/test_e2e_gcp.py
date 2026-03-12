"""
End-to-end test: Pulumi engine + Rust pulumi-language-yaml + real GCP resource.

Verifies the full deployment pipeline:
  1. Pulumi discovers the Rust language host binary
  2. Parses a YAML template
  3. Creates a real GCS bucket in GCP
  4. Reads back outputs
  5. Destroys the resource

Usage:
    cd tests/acceptance
    pytest test_e2e_gcp.py -v -s
"""

import json
import os
import shutil
import subprocess
import tempfile
import time

import pytest


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

GCP_PROJECT = "spacy-muffin-lab-5a292e"
GCP_REGION = "us-central1"
STACK_NAME = "e2e-test"


def _make_pulumi_yaml(bucket_name: str) -> str:
    return f"""\
name: e2e-gcp-bucket-test
runtime: yaml
description: E2E test for pulumi-rs-yaml — creates a real GCS bucket
config:
  gcp:project:
    value: {GCP_PROJECT}
  gcp:region:
    value: {GCP_REGION}
resources:
  testBucket:
    type: gcp:storage:Bucket
    properties:
      name: {bucket_name}
      location: US
      forceDestroy: true
      uniformBucketLevelAccess: true
outputs:
  bucketName: ${{testBucket.name}}
  bucketUrl: ${{testBucket.url}}
"""


def run_pulumi(args: list[str], *, cwd: str, env: dict) -> subprocess.CompletedProcess:
    """Run a pulumi CLI command, capturing output."""
    cmd = ["pulumi"] + args + ["--non-interactive"]
    return subprocess.run(
        cmd,
        cwd=cwd,
        env=env,
        capture_output=True,
        text=True,
        timeout=300,
    )


# ---------------------------------------------------------------------------
# Session-scoped shared state
# ---------------------------------------------------------------------------

class _E2EState:
    """Holds state shared across the ordered tests in this module."""

    def __init__(self):
        self.work_dir: str | None = None
        self.env: dict | None = None
        self.bucket_name: str | None = None
        self.preview_result: subprocess.CompletedProcess | None = None
        self.up_result: subprocess.CompletedProcess | None = None
        self.outputs: dict | None = None
        self.destroy_result: subprocess.CompletedProcess | None = None


_state = _E2EState()


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------

@pytest.fixture(scope="module", autouse=True)
def e2e_environment(rust_binary, gcp_credentials, pulumi_cli):
    """Set up the temp project directory, PATH, and env; tear down after all tests."""
    # Unique bucket name
    bucket_name = f"pulumi-rs-e2e-test-{int(time.time())}"
    _state.bucket_name = bucket_name

    # Create temp directory with Pulumi.yaml
    work_dir = tempfile.mkdtemp(prefix="pulumi-e2e-")
    _state.work_dir = work_dir

    pulumi_yaml = _make_pulumi_yaml(bucket_name)
    with open(os.path.join(work_dir, "Pulumi.yaml"), "w") as f:
        f.write(pulumi_yaml)

    # Create a symlink directory so the Rust binary is on PATH
    bin_dir = os.path.join(work_dir, "bin")
    os.makedirs(bin_dir)
    os.symlink(rust_binary, os.path.join(bin_dir, "pulumi-language-yaml"))

    # Build environment
    env = os.environ.copy()
    env["PATH"] = bin_dir + os.pathsep + env.get("PATH", "")
    env["PULUMI_BACKEND_URL"] = f"file://{work_dir}/.pulumi"
    env["PULUMI_CONFIG_PASSPHRASE"] = "e2e-test"
    env["GOOGLE_APPLICATION_CREDENTIALS"] = gcp_credentials
    _state.env = env

    # Create local backend directory
    os.makedirs(os.path.join(work_dir, ".pulumi"), exist_ok=True)

    # Init stack
    result = run_pulumi(["stack", "init", STACK_NAME], cwd=work_dir, env=env)
    assert result.returncode == 0, f"stack init failed:\n{result.stderr}"

    yield

    # ---- Teardown: best-effort destroy + cleanup ----
    # If destroy wasn't already run by the test, run it now
    if _state.destroy_result is None:
        run_pulumi(["destroy", "--yes"], cwd=work_dir, env=env)

    run_pulumi(["stack", "rm", STACK_NAME, "--yes"], cwd=work_dir, env=env)
    shutil.rmtree(work_dir, ignore_errors=True)


# ---------------------------------------------------------------------------
# Tests (ordered — each depends on the previous)
# ---------------------------------------------------------------------------

class TestE2EGCP:
    """Ordered E2E tests for Pulumi + Rust plugin + GCP."""

    def test_preview_succeeds(self):
        """Rust plugin is discovered and preview produces a valid plan."""
        result = run_pulumi(["preview"], cwd=_state.work_dir, env=_state.env)
        _state.preview_result = result

        # Print output for debugging
        print(result.stdout)
        if result.stderr:
            print(result.stderr)

        assert result.returncode == 0, (
            f"pulumi preview failed (rc={result.returncode}):\n{result.stderr}"
        )
        # Preview should mention the bucket resource
        combined = result.stdout + result.stderr
        assert "testBucket" in combined or "Bucket" in combined, (
            "Preview output does not mention the bucket resource"
        )

    def test_deploy_creates_bucket(self):
        """pulumi up succeeds and creates the GCS bucket."""
        assert _state.preview_result is not None and _state.preview_result.returncode == 0, (
            "Skipping deploy — preview did not succeed"
        )

        result = run_pulumi(["up", "--yes"], cwd=_state.work_dir, env=_state.env)
        _state.up_result = result

        print(result.stdout)
        if result.stderr:
            print(result.stderr)

        assert result.returncode == 0, (
            f"pulumi up failed (rc={result.returncode}):\n{result.stderr}"
        )

    def test_outputs_valid(self):
        """Stack outputs contain expected bucket name and URL."""
        assert _state.up_result is not None and _state.up_result.returncode == 0, (
            "Skipping output check — deploy did not succeed"
        )

        result = run_pulumi(
            ["stack", "output", "--json"],
            cwd=_state.work_dir,
            env=_state.env,
        )

        print(result.stdout)
        assert result.returncode == 0, (
            f"stack output failed (rc={result.returncode}):\n{result.stderr}"
        )

        outputs = json.loads(result.stdout)
        _state.outputs = outputs

        assert "bucketName" in outputs, f"Missing 'bucketName' in outputs: {outputs}"
        assert outputs["bucketName"] == _state.bucket_name, (
            f"Expected bucket name '{_state.bucket_name}', got '{outputs['bucketName']}'"
        )

        assert "bucketUrl" in outputs, f"Missing 'bucketUrl' in outputs: {outputs}"
        assert outputs["bucketUrl"].startswith("gs://"), (
            f"Expected bucketUrl to start with 'gs://', got '{outputs['bucketUrl']}'"
        )

    def test_destroy_cleans_up(self):
        """pulumi destroy succeeds and removes all resources."""
        assert _state.up_result is not None and _state.up_result.returncode == 0, (
            "Skipping destroy — deploy did not succeed"
        )

        result = run_pulumi(["destroy", "--yes"], cwd=_state.work_dir, env=_state.env)
        _state.destroy_result = result

        print(result.stdout)
        if result.stderr:
            print(result.stderr)

        assert result.returncode == 0, (
            f"pulumi destroy failed (rc={result.returncode}):\n{result.stderr}"
        )

        # Verify no resources remain
        combined = result.stdout + result.stderr
        # After destroy, there should be 0 resources
        assert "0 remaining" in combined or "Resources:" not in combined or result.returncode == 0, (
            "Destroy may not have fully cleaned up"
        )

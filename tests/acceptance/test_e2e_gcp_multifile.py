"""
End-to-end test: Pulumi engine + Rust plugin + multi-file YAML + real GCP resources.

Verifies that the Rust language host correctly merges multiple Pulumi YAML files
and handles cross-file resource references. Deploys 3 real GCS buckets defined
across 3 files with cross-file dependsOn.

File layout:
  Pulumi.yaml          — project config + outputs (references resources from other files)
  Pulumi.storage.yaml  — storageBucket resource
  Pulumi.logging.yaml  — logBucket (dependsOn storageBucket) + archiveBucket

Usage:
    cd tests/acceptance
    GOOGLE_APPLICATION_CREDENTIALS=/path/to/creds.json pytest test_e2e_gcp_multifile.py -v -s
"""

import json
import os
import shutil
import subprocess
import tempfile
import time

import pytest

from test_e2e_gcp import run_pulumi, GCP_PROJECT, GCP_REGION


# ---------------------------------------------------------------------------
# Multi-file template generators
# ---------------------------------------------------------------------------

STACK_NAME = "e2e-multifile"


def _make_main_yaml(suffix: str) -> str:
    """Pulumi.yaml — config + outputs referencing resources from satellite files."""
    return f"""\
name: e2e-gcp-multifile-test
runtime: yaml
description: E2E multi-file test — cross-file resource references
config:
  gcp:project:
    value: {GCP_PROJECT}
  gcp:region:
    value: {GCP_REGION}
outputs:
  storageBucketName: ${{storageBucket.name}}
  storageBucketUrl: ${{storageBucket.url}}
  logBucketName: ${{logBucket.name}}
  logBucketUrl: ${{logBucket.url}}
  archiveBucketName: ${{archiveBucket.name}}
  archiveBucketUrl: ${{archiveBucket.url}}
"""


def _make_storage_yaml(suffix: str) -> str:
    """Pulumi.storage.yaml — defines storageBucket."""
    return f"""\
resources:
  storageBucket:
    type: gcp:storage:Bucket
    properties:
      name: pulumi-rs-mf-storage-{suffix}
      location: US
      forceDestroy: true
      uniformBucketLevelAccess: true
"""


def _make_logging_yaml(suffix: str) -> str:
    """Pulumi.logging.yaml — defines logBucket (depends on storageBucket) + archiveBucket."""
    return f"""\
resources:
  logBucket:
    type: gcp:storage:Bucket
    properties:
      name: pulumi-rs-mf-logs-{suffix}
      location: US
      forceDestroy: true
      uniformBucketLevelAccess: true
    options:
      dependsOn:
        - ${{storageBucket}}
  archiveBucket:
    type: gcp:storage:Bucket
    properties:
      name: pulumi-rs-mf-archive-{suffix}
      location: US
      forceDestroy: true
      uniformBucketLevelAccess: true
"""


# ---------------------------------------------------------------------------
# Shared state
# ---------------------------------------------------------------------------

class _MFState:
    """Holds state shared across the ordered multi-file tests."""

    def __init__(self):
        self.work_dir: str | None = None
        self.env: dict | None = None
        self.suffix: str | None = None
        self.preview_result: subprocess.CompletedProcess | None = None
        self.up_result: subprocess.CompletedProcess | None = None
        self.outputs: dict | None = None
        self.destroy_result: subprocess.CompletedProcess | None = None


_state = _MFState()


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------

@pytest.fixture(scope="module", autouse=True)
def multifile_environment(rust_binary, gcp_credentials, pulumi_cli):
    """Set up temp project with 3 YAML files; tear down after all tests."""
    suffix = str(int(time.time()))
    _state.suffix = suffix

    work_dir = tempfile.mkdtemp(prefix="pulumi-e2e-mf-")
    _state.work_dir = work_dir

    # Write the 3 YAML files
    files = {
        "Pulumi.yaml": _make_main_yaml(suffix),
        "Pulumi.storage.yaml": _make_storage_yaml(suffix),
        "Pulumi.logging.yaml": _make_logging_yaml(suffix),
    }
    for name, content in files.items():
        with open(os.path.join(work_dir, name), "w") as f:
            f.write(content)

    # Symlink binary onto PATH
    bin_dir = os.path.join(work_dir, "bin")
    os.makedirs(bin_dir)
    os.symlink(rust_binary, os.path.join(bin_dir, "pulumi-language-yaml"))

    env = os.environ.copy()
    env["PATH"] = bin_dir + os.pathsep + env.get("PATH", "")
    env["PULUMI_BACKEND_URL"] = f"file://{work_dir}/.pulumi"
    env["PULUMI_CONFIG_PASSPHRASE"] = "e2e-test"
    env["GOOGLE_APPLICATION_CREDENTIALS"] = gcp_credentials
    _state.env = env

    os.makedirs(os.path.join(work_dir, ".pulumi"), exist_ok=True)

    result = run_pulumi(["stack", "init", STACK_NAME], cwd=work_dir, env=env)
    assert result.returncode == 0, f"stack init failed:\n{result.stderr}"

    yield

    # Teardown
    if _state.destroy_result is None:
        run_pulumi(["destroy", "--yes"], cwd=work_dir, env=env)
    run_pulumi(["stack", "rm", STACK_NAME, "--yes"], cwd=work_dir, env=env)
    shutil.rmtree(work_dir, ignore_errors=True)


# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------

class TestE2EGCPMultiFile:
    """Ordered E2E tests for multi-file Pulumi YAML + GCP."""

    def test_preview_succeeds(self):
        """Multi-file merge works and preview shows all 3 buckets."""
        result = run_pulumi(["preview"], cwd=_state.work_dir, env=_state.env)
        _state.preview_result = result

        print(result.stdout)
        if result.stderr:
            print(result.stderr)

        assert result.returncode == 0, (
            f"pulumi preview failed (rc={result.returncode}):\n{result.stderr}"
        )

        combined = result.stdout + result.stderr
        # All three resources should appear in the preview
        for resource in ("storageBucket", "logBucket", "archiveBucket"):
            assert resource in combined, (
                f"Preview output missing resource '{resource}'"
            )

    def test_deploy_creates_buckets(self):
        """pulumi up creates all 3 GCS buckets across files."""
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

        # Should have created 4 resources: stack + 3 buckets
        combined = result.stdout + result.stderr
        assert "4 created" in combined, (
            f"Expected '4 created' in output, got:\n{combined}"
        )

    def test_outputs_valid(self):
        """Stack outputs contain all 3 bucket names and URLs from cross-file references."""
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

        suffix = _state.suffix

        # Verify all 6 outputs
        expected = {
            "storageBucketName": f"pulumi-rs-mf-storage-{suffix}",
            "logBucketName": f"pulumi-rs-mf-logs-{suffix}",
            "archiveBucketName": f"pulumi-rs-mf-archive-{suffix}",
        }
        for key, expected_value in expected.items():
            assert key in outputs, f"Missing '{key}' in outputs: {outputs}"
            assert outputs[key] == expected_value, (
                f"Expected {key}='{expected_value}', got '{outputs[key]}'"
            )

        url_keys = ("storageBucketUrl", "logBucketUrl", "archiveBucketUrl")
        for key in url_keys:
            assert key in outputs, f"Missing '{key}' in outputs: {outputs}"
            assert outputs[key].startswith("gs://"), (
                f"Expected {key} to start with 'gs://', got '{outputs[key]}'"
            )

    def test_destroy_cleans_up(self):
        """pulumi destroy removes all 3 buckets."""
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

        # Should have deleted 4 resources: stack + 3 buckets
        combined = result.stdout + result.stderr
        assert "4 deleted" in combined, (
            f"Expected '4 deleted' in output, got:\n{combined}"
        )

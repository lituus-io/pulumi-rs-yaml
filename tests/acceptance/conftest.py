"""Shared fixtures for acceptance tests."""

import os
import json
import shutil
import subprocess
import tempfile
import time

import pytest


def _find_project_root():
    """Walk up from this file to find the workspace root (contains Cargo.toml)."""
    d = os.path.dirname(os.path.abspath(__file__))
    for _ in range(10):
        if os.path.isfile(os.path.join(d, "Cargo.toml")):
            return d
        d = os.path.dirname(d)
    return None


PROJECT_ROOT = _find_project_root()


@pytest.fixture(scope="session")
def project_root():
    return PROJECT_ROOT


@pytest.fixture(scope="session")
def rust_binary(project_root):
    """Path to the built pulumi-language-yaml binary. Skips if not found."""
    binary = os.path.join(project_root, "target", "release", "pulumi-language-yaml")
    if not os.path.isfile(binary):
        pytest.skip(f"Rust binary not found at {binary} — run `cargo build --release` first")
    return binary


@pytest.fixture(scope="session")
def gcp_credentials():
    """Resolve GCP credentials. Returns the path to a credentials JSON file.

    Priority:
    1. GOOGLE_APPLICATION_CREDENTIALS env var (if set and file exists)
    2. gcloud ADC at ~/.config/gcloud/application_default_credentials.json
    3. Skip the test
    """
    # 1. Explicit env var
    explicit = os.environ.get("GOOGLE_APPLICATION_CREDENTIALS")
    if explicit and os.path.isfile(explicit):
        return explicit

    # 2. gcloud ADC
    adc = os.path.expanduser("~/.config/gcloud/application_default_credentials.json")
    if os.path.isfile(adc):
        return adc

    pytest.skip("No GCP credentials available (set GOOGLE_APPLICATION_CREDENTIALS or run `gcloud auth application-default login`)")


@pytest.fixture(scope="session")
def pulumi_cli():
    """Ensure pulumi CLI is available."""
    path = shutil.which("pulumi")
    if path is None:
        pytest.skip("pulumi CLI not found on PATH")
    return path

"""Integration test: pip install -> console_script -> binary exec flow.

Validates:
1. pip install pulumi-rs-yaml installs pulumi_yaml_rs/ with _native .so + bin/ binaries
2. console_scripts pulumi-language-yaml and pulumi-converter-yaml are created on PATH
3. Python wrapper cli:language_main() locates bundled binary in package bin/ dir
4. os.execvp() dispatches to the Rust binary (zero Python overhead after exec)
"""
import os
import sys
import stat
import subprocess

import pytest


class TestBinaryDiscovery:
    """Test that _find_binary.py correctly locates bundled binaries."""

    def test_find_language_binary_returns_path(self):
        from pulumi_yaml_rs._find_binary import find_language_binary
        path = find_language_binary()
        assert os.path.isfile(path)
        assert "pulumi-language-yaml" in os.path.basename(path)

    def test_find_converter_binary_returns_path(self):
        from pulumi_yaml_rs._find_binary import find_converter_binary
        path = find_converter_binary()
        assert os.path.isfile(path)
        assert "pulumi-converter-yaml" in os.path.basename(path)

    def test_binaries_are_executable(self):
        from pulumi_yaml_rs._find_binary import find_language_binary, find_converter_binary
        for path in [find_language_binary(), find_converter_binary()]:
            st = os.stat(path)
            assert st.st_mode & stat.S_IXUSR, f"{path} is not executable"

    def test_binary_in_package_bin_dir(self):
        """Binary must live inside the package's bin/ directory."""
        import pulumi_yaml_rs
        pkg_dir = os.path.dirname(os.path.abspath(pulumi_yaml_rs.__file__))
        bin_dir = os.path.join(pkg_dir, "bin")
        from pulumi_yaml_rs._find_binary import find_language_binary
        path = find_language_binary()
        assert path.startswith(bin_dir), f"Binary {path} not in {bin_dir}"

    def test_missing_binary_raises(self, tmp_path, monkeypatch):
        """If binary is missing, FileNotFoundError is raised."""
        monkeypatch.setattr("pulumi_yaml_rs._find_binary._BIN_DIR", str(tmp_path))
        from pulumi_yaml_rs._find_binary import find_language_binary
        with pytest.raises(FileNotFoundError):
            find_language_binary()


class TestConsoleScriptEntryPoints:
    """Test that pip-installed console_scripts dispatch correctly."""

    def test_language_entry_point_on_path(self):
        """pulumi-language-yaml should be findable on PATH after pip install."""
        import shutil
        path = shutil.which("pulumi-language-yaml")
        assert path is not None, "pulumi-language-yaml not found on PATH"

    def test_converter_entry_point_on_path(self):
        """pulumi-converter-yaml should be findable on PATH after pip install."""
        import shutil
        path = shutil.which("pulumi-converter-yaml")
        assert path is not None, "pulumi-converter-yaml not found on PATH"

    def test_language_binary_runs(self):
        """Invoking pulumi-language-yaml with no args should exit (not crash)."""
        result = subprocess.run(
            ["pulumi-language-yaml"],
            capture_output=True, timeout=5,
        )
        # The binary will fail (no engine address) but should not segfault
        assert result.returncode != 139, "Binary segfaulted"

    def test_converter_binary_runs(self):
        """Invoking pulumi-converter-yaml with no args should exit (not crash)."""
        result = subprocess.run(
            ["pulumi-converter-yaml"],
            capture_output=True, timeout=5,
        )
        assert result.returncode != 139, "Binary segfaulted"


class TestPythonBindingsImport:
    """Test that the PyO3 native module imports correctly alongside CLI."""

    def test_native_module_imports(self):
        from pulumi_yaml_rs._native import parse_template
        assert callable(parse_template)

    def test_parse_template_works(self):
        from pulumi_yaml_rs import parse_template
        result = parse_template("name: test\nruntime: yaml\nresources: {}")
        assert isinstance(result, dict)
        assert "has_errors" in result

    def test_all_exports_available(self):
        import pulumi_yaml_rs
        expected = [
            "parse_template", "load_project", "discover_project_files",
            "has_jinja_blocks", "strip_jinja_blocks", "validate_jinja",
            "preprocess_jinja", "evaluate_builtin", "create_execution_plan",
            "find_language_binary", "find_converter_binary",
        ]
        for name in expected:
            assert hasattr(pulumi_yaml_rs, name), f"Missing export: {name}"

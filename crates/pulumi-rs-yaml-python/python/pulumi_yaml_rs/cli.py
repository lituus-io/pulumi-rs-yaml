import os
import sys


def _exec_binary(binary_path):
    if sys.platform == "win32":
        import subprocess
        sys.exit(subprocess.run([binary_path, *sys.argv[1:]]).returncode)
    else:
        os.execvp(binary_path, [binary_path, *sys.argv[1:]])


def language_main():
    from pulumi_yaml_rs._find_binary import find_language_binary
    _exec_binary(find_language_binary())


def converter_main():
    from pulumi_yaml_rs._find_binary import find_converter_binary
    _exec_binary(find_converter_binary())


def main():
    if len(sys.argv) > 1 and sys.argv[1] == "converter":
        sys.argv = [sys.argv[0]] + sys.argv[2:]
        converter_main()
    else:
        language_main()

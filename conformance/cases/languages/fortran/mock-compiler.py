#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Build fixture objects and check that the required module exists."""

from pathlib import Path
import re
import subprocess
import sys


def option(args, name, default=None):
    for index, argument in enumerate(args):
        if argument == name and index + 1 < len(args):
            return args[index + 1]
        if name in ("-I", "-J") and argument.startswith(name) and len(argument) > 2:
            return argument[2:]
    return default


def main(args):
    if "--version" in args or "-v" in args and "-o" not in args:
        print("GNU Fortran (fixture) 14.2.0")
        return 0
    output_arg = option(args, "-o")
    if output_arg is None:
        return 0
    output = Path(output_arg)
    output.parent.mkdir(parents=True, exist_ok=True)
    source = next((Path(arg) for arg in args if arg.lower().endswith(".f90")), None)
    if "-E" in args and source is not None:
        output.write_bytes(source.read_bytes())
        return 0
    if "-c" not in args:
        objects = [arg for arg in args if arg.endswith((".o", ".obj", ".a"))]
        return subprocess.run(["cc", *objects, "-o", output]).returncode
    if source is None:
        return 1
    contents = source.read_text(encoding="utf-8")
    module_dir = Path(option(args, "-J", "."))
    module_dir.mkdir(exist_ok=True, parents=True)
    for required in re.findall(r"^\s*use\s+(\w+)", contents, re.MULTILINE):
        if not (module_dir / f"{required}.mod").is_file():
            print(f"Missing module {required}.", file=sys.stderr)
            return 1
    for provided in re.findall(r"^\s*module\s+(\w+)", contents, re.MULTILINE):
        (module_dir / f"{provided}.mod").write_text(
            f"GNU fixture module\n{provided}\n", encoding="utf-8"
        )
    code = (b"int main(void) { return 0; }\n"
            if re.search(r"^\s*program\s+", contents, re.MULTILINE)
            else b"int provider(void) { return 42; }\n")
    return subprocess.run(["cc", "-x", "c", "-c", "-", "-o", output], input=code).returncode


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))

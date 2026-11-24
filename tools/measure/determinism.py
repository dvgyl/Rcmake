#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Compare generated build files. Exclude compiler probes and configure diagnostics."""

from __future__ import annotations

import argparse
import difflib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
from collections.abc import Iterator


ROOT = Path(__file__).resolve().parents[2]
GENERATORS = ("Ninja", "Ninja Multi-Config", "Unix Makefiles", "Xcode")
TIMEOUT = 120
Snapshot = dict[str, tuple[str, bytes]]

PROJECT = {
    "CMakeLists.txt": """\
cmake_minimum_required(VERSION 4.5)
project(Determinism LANGUAGES C CXX)
add_library(core STATIC core.c)
set_property(TARGET core PROPERTY POSITION_INDEPENDENT_CODE ON)
add_library(shared SHARED shared.c)
target_link_libraries(shared PRIVATE core)
add_executable(app main.cpp)
target_link_libraries(app PRIVATE shared)
add_custom_command(
  OUTPUT "${CMAKE_CURRENT_BINARY_DIR}/note.txt"
  COMMAND "${CMAKE_COMMAND}" -E copy "${CMAKE_CURRENT_SOURCE_DIR}/note.txt"
          "${CMAKE_CURRENT_BINARY_DIR}/note.txt"
  DEPENDS "${CMAKE_CURRENT_SOURCE_DIR}/note.txt"
  VERBATIM
)
add_custom_target(note ALL DEPENDS "${CMAKE_CURRENT_BINARY_DIR}/note.txt")
add_dependencies(app note)
install(TARGETS app shared core)
enable_testing()
add_test(NAME run-app COMMAND app)
""",
    "core.c": "int core(void) { return 42; }\n",
    "shared.c": "int core(void);\nint shared(void) { return core(); }\n",
    "main.cpp": 'extern "C" int shared(void);\nint main() { return shared() != 42; }\n',
    "note.txt": "Custom command input.\n",
}


def run(binary: Path, *args: str | Path) -> bytes:
    result = subprocess.run(
        [binary, *args], capture_output=True, check=False, timeout=TIMEOUT
    )
    if result.returncode:
        output = (result.stdout + result.stderr).decode(errors="replace")
        raise RuntimeError(f"{' '.join(map(str, args))} exited with {result.returncode}\n{output}")
    return result.stdout


def available_generators(binary: Path) -> Iterator[str]:
    capabilities = json.loads(run(binary, "-E", "capabilities"))
    advertised = {generator["name"] for generator in capabilities["generators"]}
    for name in GENERATORS:
        if name not in advertised:
            print(f"{name} skipped. The binary does not advertise this generator.")
        elif name.startswith("Ninja") and not shutil.which("ninja"):
            print(f"{name} skipped. Ninja is missing from PATH.")
        elif name == "Unix Makefiles" and not shutil.which("make"):
            print(f"{name} skipped. Make is missing from PATH.")
        elif name == "Xcode" and sys.platform != "darwin":
            print(f"{name} skipped. Xcode requires macOS.")
        else:
            yield name


def is_probe_output(path: Path) -> bool:
    parts = path.parts
    return (
        path.as_posix() == "CMakeFiles/CMakeConfigureLog.yaml"
        or parts[:2] in (("CMakeFiles", "CMakeScratch"), ("CMakeFiles", "CMakeTmp"))
        or (
            len(parts) > 2
            and parts[0] == "CMakeFiles"
            and parts[1][:1].isdigit()
            and parts[2] in ("CompilerIdC", "CompilerIdCXX")
        )
    )


def snapshot(build: Path) -> Snapshot:
    files = {}
    prefix = os.fsencode(build)
    for path in sorted(build.rglob("*")):
        relative = path.relative_to(build)
        if is_probe_output(relative):
            continue
        name = relative.as_posix()
        if path.is_symlink():
            kind, data = "symlink", os.fsencode(path.readlink())
        elif path.is_file():
            kind, data = "file", path.read_bytes()
        elif path.is_dir():
            continue
        else:
            raise RuntimeError(f"Unsupported output type at {name}")
        files[name] = (kind, data.replace(prefix, b"<B>"))
    return files


def compare(left: Snapshot, right: Snapshot) -> bool:
    equal = 0
    names = left.keys() | right.keys()
    for name in sorted(names):
        a = left.get(name)
        b = right.get(name)
        if a == b:
            equal += 1
            continue
        if a is not None and b is not None and a[0] != b[0]:
            print(f"{name} changed from {a[0]} to {b[0]}.", file=sys.stderr)
            continue
        before = a[1] if a else b""
        after = b[1] if b else b""
        if b"\0" in before or b"\0" in after or before == after:
            print(f"{name} differs ({len(before)} and {len(after)} bytes).", file=sys.stderr)
            continue
        diff = difflib.diff_bytes(
            difflib.unified_diff,
            before.splitlines(keepends=True),
            after.splitlines(keepends=True),
            os.fsencode(f"bld-a/{name}") if a else b"/dev/null",
            os.fsencode(f"bld-b/{name}") if b else b"/dev/null",
        )
        sys.stderr.buffer.writelines(
            line if line.endswith(b"\n") else line + b"\n\\ No newline at end of file\n"
            for line in diff
        )
        sys.stderr.buffer.flush()
    percent = 100 * equal / len(names) if names else 0
    print(f"{equal}/{len(names)} files match ({percent:.1f}%).", flush=True)
    return bool(names) and equal == len(names)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--rcmake-bin",
        type=Path,
        default=Path(os.environ.get("RCMAKE_BIN", ROOT / "target/stage/bin/rcmake")),
        help="Path to the staged Rcmake binary.",
    )
    binary = parser.parse_args().rcmake_bin.absolute()
    passed = True
    checked = 0
    try:
        with tempfile.TemporaryDirectory(prefix="rcmake-determinism-") as directory:
            root = Path(directory).resolve()
            source = root / "src"
            source.mkdir()
            for name, content in PROJECT.items():
                (source / name).write_text(content, encoding="utf-8")
            for name in available_generators(binary):
                print(f"Checking {name}.", flush=True)
                checked += 1
                with tempfile.TemporaryDirectory(dir=root) as case:
                    a, b = Path(case) / "bld-a", Path(case) / "bld-b"
                    try:
                        for build in (a, b):
                            run(binary, "-S", source, "-B", build, "-G", name)
                        passed = compare(snapshot(a), snapshot(b)) and passed
                    except (OSError, RuntimeError, subprocess.TimeoutExpired) as error:
                        print(error, file=sys.stderr)
                        passed = False
    except (OSError, RuntimeError, ValueError, subprocess.TimeoutExpired) as error:
        print(error, file=sys.stderr)
        return 1
    if not checked:
        print("No generators were available.", file=sys.stderr)
    return 0 if checked and passed else 1


if __name__ == "__main__":
    raise SystemExit(main())

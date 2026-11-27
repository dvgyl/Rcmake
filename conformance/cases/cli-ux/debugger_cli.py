#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Compare the `--debugger*` command-line behavior of rcmake with CMake 4.5.1.

These cases need no debugger client: missing option values (`DebuggerArgMissingPipe`,
`DebuggerArgMissingDapLog`), `--debugger` without a pipe, an adapter that can not bind,
debugger options after `-P <script>`, and `-E capabilities`.

Usage: debugger_cli.py [--rcmake PATH] [--oracle PATH]
"""

import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[3]


def run(binary, args, cwd):
    result = subprocess.run([binary, *args], cwd=cwd, capture_output=True,
                            stdin=subprocess.DEVNULL, timeout=30)
    # The front end names itself in the usage hint.
    stderr = result.stderr.replace(b"Run 'rcmake --help'", b"Run 'cmake --help'")
    return result.returncode, result.stdout, stderr


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--rcmake", default=str(ROOT / "target/debug/rcmake"))
    parser.add_argument("--oracle", default=os.environ.get("RCMAKE_ORACLE_CMAKE", ""))
    args = parser.parse_args()
    if not args.oracle:
        print("debugger_cli: set RCMAKE_ORACLE_CMAKE or pass --oracle", file=sys.stderr)
        return 2
    args.rcmake = str(Path(args.rcmake).resolve())
    args.oracle = str(Path(args.oracle).resolve())
    failures = 0
    with tempfile.TemporaryDirectory(prefix="dbgcli-") as tmp:
        work = Path(tmp)
        (work / "s.cmake").write_text('message(STATUS "ran")\n', encoding="utf-8")
        (work / "proj").mkdir()
        (work / "proj/CMakeLists.txt").write_text("project(P NONE)\n", encoding="utf-8")
        missing = "/nonexistent-rcmake-dir/sock"
        cases = {
            "DebuggerArgMissingPipe": ["--debugger-pipe"],
            "DebuggerArgMissingDapLog": ["--debugger-dap-log"],
            "script-without-pipe": ["--debugger", "-P", "s.cmake"],
            "option-after-script": ["-P", "s.cmake", "--debugger"],
            "script-bind-failure": ["--debugger", "--debugger-pipe", missing, "-P", "s.cmake"],
            "pipe-without-debugger": ["--debugger-pipe", missing, "-P", "s.cmake"],
            "configure-without-pipe": ["--debugger", "-S", "proj", "-B", "b1"],
            "configure-bind-failure": ["--debugger", "--debugger-pipe", missing,
                                       "-S", "proj", "-B", "b2"],
        }
        for name, case in cases.items():
            expected = run(args.oracle, case, work)
            actual = run(args.rcmake, case, work)
            if expected == actual:
                print(f"PASS {name}")
            else:
                failures += 1
                print(f"FAIL {name}\n  oracle: {expected}\n  rcmake: {actual}")
        capabilities = {}
        for label, binary in (("oracle", args.oracle), ("rcmake", args.rcmake)):
            output = subprocess.run([binary, "-E", "capabilities"], capture_output=True,
                                    timeout=30, check=True).stdout
            capabilities[label] = json.loads(output)["debugger"]
        if capabilities["oracle"] == capabilities["rcmake"] is True:
            print("PASS capabilities-debugger")
        else:
            failures += 1
            print(f"FAIL capabilities-debugger {capabilities}")
    print(f"debugger_cli: {failures} failing case(s)")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())

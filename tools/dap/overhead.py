#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Measure the cost of the debugger hook when no debugger is attached.

The script runs a `-P` loop of about 200,000 commands with a baseline binary and a
candidate binary, alternating the runs, and reports the median wall-clock time of each.
The gate passes if the candidate median is at most 1 % slower than the baseline median.

Usage: overhead.py --baseline OLD --candidate NEW [--runs 9] [--commands 200000]
Exit status: 0 if the gate passes, 1 otherwise.
"""

import argparse
import pathlib
import statistics
import subprocess
import sys
import tempfile
import time


def script(commands: int) -> str:
    # Each iteration runs three command frames (`set`, `math`, and `if`). A normal
    # `endif()` is not a frame. About `commands` command frames run in total.
    loops = commands // 3
    return (
        f"set(N 0)\n"
        f"foreach(i RANGE 1 {loops})\n"
        f"  set(X ${{i}})\n"
        f"  math(EXPR N \"${{N}} + 1\")\n"
        f"  if(N EQUAL 0)\n"
        f"  endif()\n"
        f"endforeach()\n"
        f"message(STATUS \"${{N}}\")\n"
    )


def measure(exe: str, path: pathlib.Path) -> float:
    start = time.perf_counter()
    subprocess.run([exe, "-P", str(path)], check=True, capture_output=True, timeout=600)
    return time.perf_counter() - start


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--baseline", required=True)
    parser.add_argument("--candidate", required=True)
    parser.add_argument("--runs", type=int, default=9)
    parser.add_argument("--commands", type=int, default=200_000)
    parser.add_argument("--limit", type=float, default=0.01)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="dapcost-") as tmp:
        path = pathlib.Path(tmp) / "loop.cmake"
        path.write_text(script(args.commands), encoding="utf-8")
        measure(args.baseline, path)
        measure(args.candidate, path)
        times = {"baseline": [], "candidate": []}
        for _ in range(args.runs):
            times["baseline"].append(measure(args.baseline, path))
            times["candidate"].append(measure(args.candidate, path))
    base = statistics.median(times["baseline"])
    cand = statistics.median(times["candidate"])
    change = cand / base - 1
    print(f"baseline  median {base * 1000:.1f} ms  min {min(times['baseline']) * 1000:.1f} ms")
    print(f"candidate median {cand * 1000:.1f} ms  min {min(times['candidate']) * 1000:.1f} ms")
    print(f"change {change * 100:+.2f} % (limit {args.limit * 100:.1f} %)")
    return 0 if change <= args.limit else 1


if __name__ == "__main__":
    sys.exit(main())

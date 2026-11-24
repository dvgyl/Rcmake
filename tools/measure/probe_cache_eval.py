#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Check real compiler probe invalidation and shared-store crash recovery.

Source env/env.sh before use. Smoke mode checks the fixture and process
handling on a staged binary. It does not require probe-cache support.
"""

from __future__ import annotations

import argparse
from contextlib import ExitStack
from dataclasses import dataclass
import hashlib
import json
import os
from pathlib import Path
import random
import re
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import time

from mutation_eval import diff_bytes, normalize
from oracle_prop_test import clean_environment, executable, run_command, write_json


WORKERS = 8
TRIGGERS = {
    "cpath": ('#include <rcmake_m6_cpath.h>\n', False, True),
    "lang": ("", True, True),
    "compiler": ("#ifndef RCMAKE_M6_SWAP\n#error Missing wrapper definition\n#endif\n", False, True),
    "header-root": ('#include <rcmake_m6_root.h>\n', False, True),
    "header-subdir": ('#include <existing/rcmake_m6_subdir.h>\n', False, True),
    "library": ("", False, True),
    "toolchain": ("", True, True),
    "developer-dir": ("", True, True),
    "transitive": ('#include <rcmake_m6_outer.h>\n', False, True),
}
PROBE_STATUS = b"-- M6 probe results "
PANIC = re.compile(rb"(?im)thread [^\n]+ panicked at|(?:^|\n)fatal runtime error:|RUST_BACKTRACE")


@dataclass(frozen=True)
class Fixture:
    """Keep the paths and compiler configuration for one test fixture."""

    root: Path
    compiler: str
    delay: float

    @property
    def source(self) -> Path:
        return self.root / "source"

    @property
    def log(self) -> Path:
        return self.root / "compiler.log"

    def wrapper(self, swapped: bool = False) -> None:
        """Log attempted fixture compilations and invoke the system compiler."""
        actions = []
        if self.delay:
            actions.append(f"    sleep {self.delay:g}\n")
        if swapped:
            actions.append('    set -- -DRCMAKE_M6_SWAP=1 "$@"\n')
        script = f"""#!/bin/sh
compile=no
source=no
for arg do
    case "$arg" in
        -c) compile=yes ;;
        */m6_probe_*.c|m6_probe_*.c) source=yes ;;
    esac
done
if [ "$compile:$source" = yes:yes ]; then
    printf '%s\\n' "$PWD" >> {shlex.quote(str(self.log))}
{''.join(actions)}fi
exec {shlex.quote(self.compiler)} "$@"
"""
        replacement = self.root / "compiler.new"
        replacement.write_text(script, encoding="utf-8")
        replacement.chmod(0o755)
        replacement.replace(self.root / "compiler")

    def invocations(self) -> list[str]:
        """Read one record per attempted fixture compilation."""
        return self.log.read_text(encoding="utf-8").splitlines() if self.log.exists() else []


def fixture(root: Path, compiler: str, prefix: str = "", probes: int = 1,
            delay: float = 0, library: bool = False) -> Fixture:
    """Create a C project with a leaf directory that runs real try_compile probes."""
    case = Fixture(root, compiler, delay)
    for name in ("source/leaf", "include/existing", "library", "empty", "alternate"):
        (root / name).mkdir(parents=True)
    case.wrapper()
    (root / "toolchain.cmake").write_text(
        'set(CMAKE_C_COMPILER "${CMAKE_CURRENT_LIST_DIR}/compiler")\n'
        'set(M6_TOOLCHAIN_REVISION 1)\n', encoding="utf-8",
    )
    (case.source / "CMakeLists.txt").write_text(
        "cmake_minimum_required(VERSION 4.5)\nproject(M6ProbeCache C)\n"
        "add_subdirectory(leaf)\n", encoding="utf-8",
    )
    link = ('LINK_OPTIONS "-L${CMAKE_CURRENT_SOURCE_DIR}/../../library" '
            'LINK_LIBRARIES "-lrcmake_m6_probe"') if library else ""
    # Keep each probe source different so a cold storm performs many writes.
    leaf = f"""\
set(results "")
set(prefix [==[{prefix}]==])
foreach(index RANGE 1 {probes})
  try_compile(result
    SOURCE_FROM_CONTENT "m6_probe_${{index}}.c"
      "${{prefix}}int main(void) {{ return ${{index}} - ${{index}}; }}"
    COMPILE_DEFINITIONS "-I${{CMAKE_CURRENT_SOURCE_DIR}}/../../include"
    {link}
    NO_CACHE)
  string(APPEND results "${{result}}\\n")
endforeach()
file(WRITE "${{CMAKE_BINARY_DIR}}/probe-results.txt" "${{results}}")
string(REPLACE "\\n" " " display "${{results}}")
message(STATUS "M6 probe results ${{display}}")
"""
    (case.source / "leaf/CMakeLists.txt").write_text(leaf, encoding="utf-8")
    return case


def environment(case: Fixture, store: Path) -> dict[str, str]:
    """Use a private store and a stable baseline for the changed inputs."""
    env = clean_environment()
    for key in ("RCMAKE_PROBE_CACHE", "RCMAKE_NO_PROBE_CACHE", "RCMAKE_NO_INCREMENTAL",
                "RCMAKE_PROBE_ENV_EXTRA", "DEVELOPER_DIR", "LANGUAGE", "LC_ALL",
                "LC_MESSAGES", "CFLAGS", "CPPFLAGS", "LDFLAGS", "CC"):
        env.pop(key, None)
    env.update(LANG="C", CPATH=str(case.root / "empty"), RCMAKE_PROBE_JOBS="1",
               RCMAKE_CACHE_DIR=str(store), RCMAKE_PROBE_CACHE_DIR=str(store))
    return env


def command(binary: Path, case: Fixture, build: Path, no_cache: bool = False) -> list:
    """Make a configure command for an empty Ninja build directory."""
    flags = ["--no-probe-cache"] if no_cache else []
    return [binary, *flags, "-S", case.source, "-B", build, "-G", "Ninja",
            f"-DCMAKE_TOOLCHAIN_FILE={case.root / 'toolchain.cmake'}"]


def output(result: dict, case: Fixture, build: Path) -> dict[str, bytes]:
    """Read exact probe results and normalized configure streams."""
    stdout = normalize(result["stdout"].encode(), case.source, build)
    stderr = normalize(result["stderr"].encode(), case.source, build)
    if result["error"] or result["returncode"] != 0 or PANIC.search(stdout + b"\n" + stderr):
        raise ValueError(f"Configure failed in {build}. {result}")
    result_file = build / "probe-results.txt"
    probe_stdout = b"\n".join(line for line in stdout.splitlines() if line.startswith(PROBE_STATUS))
    if not probe_stdout or not result_file.is_file():
        raise ValueError(f"Configure did not report probe results in {build}.")
    return {"result": result_file.read_bytes(), "probe stdout": probe_stdout + b"\n",
            "stdout": stdout, "stderr": stderr}


def compare(actual: dict, expected: dict, keys, label: str) -> None:
    """Report the first output difference."""
    for key in keys:
        if actual[key] != expected[key]:
            diff = diff_bytes(expected[key], actual[key], f"expected/{key}", f"actual/{key}")
            raise ValueError(f"{label} differs.\n{diff.decode(errors='replace')}")


def check_result(data: dict, expected: bool, count: int = 1) -> None:
    """Check the expected number and value of the CMake booleans."""
    result = (b"TRUE\n" if expected else b"FALSE\n") * count
    compare(data, {"result": result}, ("result",), "Probe result")


def configure(args, case: Fixture, name: str, env: dict, *, oracle: bool = False,
              no_cache: bool = False) -> tuple[dict, int]:
    """Configure and count actual compiler calls attributable to the probe."""
    build = case.root / name
    binary = args.oracle if oracle else args.rcmake
    before = len(case.invocations())
    result = run_command(command(binary, case, build, no_cache), case.root, args.timeout, env)
    return output(result, case, build), len(case.invocations()) - before


def change_input(name: str, case: Fixture, env: dict, args) -> None:
    """Change only the input selected for this invalidation check."""
    headers = {
        "header-root": "include/rcmake_m6_root.h",
        "header-subdir": "include/existing/rcmake_m6_subdir.h",
        "transitive": "include/existing/rcmake_m6_inner.h",
    }
    if name in headers:
        (case.root / headers[name]).write_text("#define RCMAKE_M6_HEADER 1\n", encoding="utf-8")
    elif name == "cpath":
        env["CPATH"] = str(case.root / "alternate")
    elif name == "lang":
        env["LANG"] = "POSIX"
    elif name == "compiler":
        case.wrapper(swapped=True)
    elif name == "toolchain":
        with (case.root / "toolchain.cmake").open("a", encoding="utf-8") as stream:
            stream.write("set(M6_TOOLCHAIN_REVISION 2)\n")
    elif name == "developer-dir":
        env["DEVELOPER_DIR"] = args.developer_dir
    elif name == "library":
        source = case.root / "archive.c"
        source.write_text("int m6_archive(void) { return 1; }\n", encoding="utf-8")
        obj = case.root / "archive.o"
        archive = case.root / "library/librcmake_m6_probe.a"
        for cmd in ([case.compiler, "-c", source, "-o", obj], ["ar", "rcs", archive, obj]):
            result = run_command(cmd, case.root, args.timeout, env)
            if result["error"] or result["returncode"]:
                raise ValueError(f"Could not create the test library. {result}")


def invalidation(args, root: Path, name: str, row: dict) -> None:
    """Require a cold compile, a warm hit, and a miss after each input change."""
    prefix, before, after = TRIGGERS[name]
    case = fixture(root, args.compiler, prefix, library=name == "library")
    if name == "cpath":
        (root / "alternate/rcmake_m6_cpath.h").write_text(
            "#define RCMAKE_M6_HEADER 1\n", encoding="utf-8")
    if name == "transitive":
        (root / "include/rcmake_m6_outer.h").write_text(
            "#include <existing/rcmake_m6_inner.h>\n", encoding="utf-8")
    store = root / "store"
    env = environment(case, store)
    calls = row["compiler_calls"] = {}
    cold, calls["cold"] = configure(args, case, "cold", env)
    check_result(cold, before)
    warm, calls["warm"] = configure(args, case, "warm", env)
    compare(warm, cold, cold.keys(), "Warm configure")
    if not calls["cold"]:
        raise ValueError("The cold configure did not invoke the probe compiler.")
    change_input(name, case, env, args)
    changed, calls["changed"] = configure(args, case, "changed", env)
    oracle, calls["oracle"] = configure(args, case, "oracle", env, oracle=True)
    check_result(oracle, after)
    if not args.smoke:
        disabled, calls["cache_off"] = configure(args, case, "cache-off", env, no_cache=True)
        check_result(disabled, after)
        compare(disabled, oracle, oracle.keys(), "Cache-off CMake parity")
        if not calls["cache_off"]:
            raise ValueError("The cache-off configure did not invoke the probe compiler.")
    expected_stale = name == "transitive" and not calls["changed"] and changed["result"] == cold["result"]
    if expected_stale:
        row["transitive_behavior"] = "Documented stale failure"
        compare(changed, cold, cold.keys(), "Transitive cached result")
    else:
        baseline = oracle if args.smoke else disabled
        compare(changed, baseline, baseline.keys(), "Changed configure")
        if name == "transitive":
            row["transitive_behavior"] = "Recompiled and found the new header"
    if not args.smoke and calls["warm"]:
        raise ValueError("The warm configure invoked the compiler. Expected a cache hit.")
    if not calls["changed"] and not expected_stale:
        raise ValueError("The changed configure did not invoke the compiler. Expected a cache miss.")


def audit_store(store: Path) -> int:
    """Check published entry and blob frames. Ignore private staging files."""
    frames = [
        *(("binary entry", path) for path in (store / "entries").rglob("*.entry")),
        *(("binary blob", path) for path in (store / "blobs").rglob("*.blob")),
        *(("text entry", path) for path in (store / "probe/v1").rglob("*.json")),
    ]
    entries = 0
    for kind, path in frames:
        if not path.is_file() or path.is_symlink():
            raise ValueError(f"Invalid published cache path {path}.")
        data = path.read_bytes()
        if kind.startswith("binary"):
            magic = b"RCMENTRY" if kind == "binary entry" else b"RCM_BLOB"
            body, checksum = data[:-32], data[-32:]
            valid = (len(body) > 17 and body.startswith(magic + b"\x01")
                     and body[17:21] == b"\x28\xb5\x2f\xfd"
                     and hashlib.sha256(body).digest() == checksum)
        else:
            header, _, body = data.partition(b"\n")
            expected = f"rcmake-entry 1 {len(body)} {hashlib.sha256(body).hexdigest()}".encode()
            valid = header == expected
            if valid:
                json.loads(body)
        if not valid:
            raise ValueError(f"Corrupt published cache file {path}.")
        if kind == "text entry" or path.is_relative_to(store / "entries/probe-v1"):
            entries += 1
    return entries


def kill_group(process: subprocess.Popen) -> None:
    """Stop the configure and all children in its process group."""
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass


def crash_victims(workers: list, case: Fixture, store: Path, deadline: float,
                  args) -> dict[int, Path | None]:
    """Kill seeded workers during a probe or while a staging file exists."""
    rng = random.Random(args.seed)
    victims = rng.sample(range(WORKERS), WORKERS // 2)
    selected = set(victims)
    killed = {}
    staging = store / "tmp"
    while selected and time.monotonic() < deadline:
        active = case.invocations()
        staged = {name.split("-", 1)[0]: staging / name
                  for name in os.listdir(staging)} if staging.is_dir() else {}
        for index in sorted(selected):
            process, build, _, _ = workers[index]
            if process.poll() is not None:
                raise ValueError(f"Crash victim {index} completed before SIGKILL.")
            candidate = staged.get(str(process.pid))
            writing = candidate is not None and candidate.exists()
            probing = any(path.startswith(str(build) + os.sep) for path in active)
            if writing or (probing and (args.smoke or index != victims[0])):
                kill_group(process)
                killed[index] = candidate if writing else None
        selected.difference_update(killed)
        if selected:
            time.sleep(0.001)
    if selected:
        raise ValueError(f"Timed out before killing crash victims {sorted(selected)}.")
    return killed


def parallel(args, case: Fixture, store: Path, env: dict, crash: bool, row: dict) -> list[dict]:
    """Start all workers before waiting. Check each survivor and killed worker."""
    workers = []
    deadline = time.monotonic() + args.timeout
    with ExitStack() as stack:
        try:
            for index in range(WORKERS):
                build = case.root / f"worker-{index}"
                stdout = stack.enter_context(tempfile.TemporaryFile())
                stderr = stack.enter_context(tempfile.TemporaryFile())
                process = subprocess.Popen(
                    command(args.rcmake, case, build), cwd=case.root, env=env,
                    stdin=subprocess.DEVNULL, stdout=stdout, stderr=stderr, start_new_session=True,
                )
                workers.append((process, build, stdout, stderr))
            killed = crash_victims(workers, case, store, deadline, args) if crash else {}
            row["killed"] = sorted(killed)
            outputs = []
            for index, (process, build, out, err) in enumerate(workers):
                status = process.wait(timeout=max(0, deadline - time.monotonic()))
                out.seek(0)
                err.seek(0)
                stdout, stderr = out.read(), err.read()
                if index in killed:
                    if status != -signal.SIGKILL or PANIC.search(stdout + b"\n" + stderr):
                        raise ValueError(f"Worker {index} did not stop cleanly on SIGKILL. {status}.")
                else:
                    outputs.append(output({"stdout": stdout.decode(errors="replace"),
                                           "stderr": stderr.decode(errors="replace"),
                                           "returncode": status, "error": None}, case, build))
            row["staging_interruptions"] = sum(path is not None and path.exists() for path in killed.values())
            row["completed"] = len(outputs)
            return outputs
        finally:
            for process, _, _, _ in workers:
                if process.poll() is None:
                    kill_group(process)
                    process.wait(timeout=5)


def concurrency(args, root: Path, crash: bool, row: dict) -> None:
    """Compare the eight-worker run and a post-crash configure with CMake."""
    probes = 4 if args.smoke else 24
    case = fixture(root, args.compiler, probes=probes, delay=0.15 if crash else 0)
    store = root / "store"
    env = environment(case, store)
    oracle, _ = configure(args, case, "oracle", env, oracle=True)
    check_result(oracle, True, probes)
    survivors = parallel(args, case, store, env, crash, row)
    for actual in survivors:
        compare(actual, oracle, oracle.keys(), "Parallel CMake parity")
    row["published_before_recovery"] = audit_store(store)
    recovered, row["recovery_compiler_calls"] = configure(args, case, "recovery", env)
    compare(recovered, oracle, oracle.keys(), "Recovery CMake parity")
    row["published_after_recovery"] = audit_store(store)
    if not args.smoke:
        if not row["published_before_recovery"] or not row["published_after_recovery"]:
            raise ValueError("The shared store has no published probe entries.")
        if row["recovery_compiler_calls"] >= probes:
            raise ValueError("Recovery did not reuse any published probe results.")
        if crash and not row["staging_interruptions"]:
            raise ValueError("No SIGKILL left a staging file. A write interruption is unverified.")


def evaluate(args, root: Path, report: dict) -> None:
    """Run each gate case and record individual failures."""
    version = run_command([args.oracle, "--version"], root, args.timeout, clean_environment())
    if version["returncode"] or version["error"] or version["stdout"].splitlines()[:1] != ["cmake version 4.5.1"]:
        raise ValueError(f"Expected the CMake 4.5.1 oracle. {version}")
    if sys.platform == "darwin":
        dev = run_command(["xcode-select", "-p"], root, args.timeout, clean_environment())
        if dev["returncode"] or dev["error"] or not Path(dev["stdout"].strip()).is_dir():
            raise ValueError(f"Could not find the active macOS developer directory. {dev}")
        args.developer_dir = dev["stdout"].strip()
    else:
        args.developer_dir = str(root)
    for name in [*TRIGGERS, "parallel", "crash"]:
        row = {"case": name, "gate": "G3" if name in TRIGGERS else "G4"}
        report["cases"].append(row)
        try:
            if name in TRIGGERS:
                invalidation(args, root / name, name, row)
            else:
                concurrency(args, root / name, name == "crash", row)
        except (OSError, ValueError, subprocess.SubprocessError) as error:
            row["error"] = str(error)
            print(f"{name}. {error}", file=sys.stderr)
        row["passed"] = "error" not in row


def main() -> int:
    """Write JSON and Markdown results for the requested mode."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rcmake", required=True)
    parser.add_argument("--oracle", default=os.environ.get("RCMAKE_ORACLE_CMAKE"))
    parser.add_argument("--cc", default=shutil.which("cc"))
    parser.add_argument("--smoke", action="store_true")
    parser.add_argument("--seed", type=int, default=0)
    parser.add_argument("--timeout", type=int, default=120)
    parser.add_argument("--out-json", type=Path, required=True)
    parser.add_argument("--out-md", type=Path, required=True)
    args = parser.parse_args()
    if args.out_json.resolve() == args.out_md.resolve():
        parser.error("Use different paths for the JSON and Markdown reports.")
    if args.timeout < 1 or not args.oracle or not args.cc:
        parser.error("Set a positive timeout and specify or install the CMake oracle and C compiler.")
    report = {"mode": "smoke" if args.smoke else "gate", "seed": args.seed,
              "rcmake": args.rcmake, "oracle": args.oracle, "cases": []}
    try:
        args.rcmake = executable(args.rcmake)
        args.oracle = executable(args.oracle)
        args.compiler = str(executable(args.cc))
        with tempfile.TemporaryDirectory(prefix="rcmake-m6-probes-") as directory:
            evaluate(args, Path(directory).resolve(), report)
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        report["error"] = str(error)
        print(error, file=sys.stderr)
    report["passed"] = "error" not in report and all(row["passed"] for row in report["cases"])
    write_json(args.out_json, report)
    lines = ["# M6 compiler probe cache", "", f"Mode {report['mode']}. Seed {args.seed}."]
    if args.smoke:
        lines.extend(("", "Smoke mode checks fixture results and recovery after SIGKILL.",
                      "Cache hits, cache-off parity, and interrupted store writes require gate mode."))
    lines.extend(("", "| Gate | Case | Result |", "| --- | --- | --- |"))
    for row in report["cases"]:
        lines.append(f"| {row['gate']} | {row['case']} | {'Pass' if row['passed'] else 'Fail'} |")
    for row in report["cases"]:
        if "error" in row:
            lines.extend(("", f"## {row['case']}", "", row["error"]))
    if "error" in report:
        lines.extend(("", report["error"]))
    args.out_md.parent.mkdir(parents=True, exist_ok=True)
    args.out_md.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())

#!/usr/bin/env python3
"""Compare the staged Rcmake binary with CMake 4.5.1 using upstream RunCMake.

Source env/env.sh before use. The upstream driver runs all assertions on each
binary in a separate temporary copy of the source tree. Use --json to keep
the per-case results and upstream failure messages.

The score includes cases that pass with CMake 4.5.1. A missing Rcmake result
counts as a failure. Exit status 1 means the results differ. Exit status 2
means a setup or driver error prevented the full comparison.
"""

from __future__ import annotations

import argparse
from collections import Counter
import json
import math
import os
from pathlib import Path
import platform
import re
import shutil
import signal
import subprocess
import sys
import tempfile

SUITES = (
    "Syntax", "if", "foreach", "while", "function", "block", "return", "set",
    "option", "list", "string", "math", "file", "cmake_path",
    "separate_arguments", "configure_file", "cmake_parse_arguments",
    "cmake_language", "CommandLine", "CommandLineTar", "CMakePresets",
    "Diagnostics", "DiagnosticScope", "PolicyScope", "message", "include",
    "include_guard", "get_filename_component", "execute_process", "find_file",
    "find_path", "find_program", "find_library", "cmake_minimum_required",
    "Policy", "get_property", "set_property", "define_property",
    "cmake_host_system_information", "variable_watch", "CMP0219",
)
ALIASES = {
    "macro": ("Syntax", "function", "CMP0219"),
    "unset": ("set", "CommandLine"),
    "cmake_policy": ("Policy", "PolicyScope"),
    "cmake_diagnostic": ("Diagnostics", "DiagnosticScope"),
}
HELPERS = {
    "EXIT_CODE_EXE": "exit_code",
    "EXIT_CRASH_EXE": "exit_crash",
    "PRINT_STDIN_EXE": "print_stdin",
    "LLVM_RC": "pseudo_llvm-rc",
}
TEST_ENVIRONMENT = (
    "PATH", "HOME", "USER", "LOGNAME", "LANG", "LC_ALL", "LC_CTYPE", "LC_MESSAGES",
    "SDKROOT", "DEVELOPER_DIR", "RCMAKE_VENDORED_DATA_ROOT",
)
COMPLETE = "-- runcmake_eval: suite complete"
CASE_RESULT = re.compile(
    r"^-- (?P<passed>.+) - PASSED$"
    r"|^CMake Error at .*[/\\]RunCMake\.cmake:\d+ \(message\):\n"
    r"  (?P<failed>.+) - FAILED:\n(?P<detail>(?:^[ \t].*\n|^\n)*)",
    re.MULTILINE,
)


def run(command, cwd, env, timeout):
    """Capture combined output and terminate the process group on timeout."""
    with tempfile.TemporaryFile(dir=cwd) as output:
        with subprocess.Popen(
            command, cwd=cwd, env=env, stdin=subprocess.DEVNULL,
            stdout=output, stderr=subprocess.STDOUT, start_new_session=True,
        ) as process:
            try:
                process.wait(timeout=timeout)
                timed_out = False
            except (subprocess.TimeoutExpired, KeyboardInterrupt) as exc:
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                process.wait()
                if isinstance(exc, KeyboardInterrupt):
                    raise
                timed_out = True
        output.seek(0)
        log = output.read().decode("utf-8", errors="replace").replace("\r\n", "\n")
    return process.returncode, timed_out, log


def prepare_helpers(source, directory, env, timeout):
    """Build the small C fixtures used by CommandLine and execute_process."""
    directory.mkdir()
    compiler = shutil.which("cc")
    if compiler is None:
        raise ValueError("Install a C compiler named cc for the upstream test fixtures.")
    for name in HELPERS.values():
        code, timed_out, log = run(
            [compiler, str(source / f"{name}.c"), "-o", str(directory / name)],
            directory, env, timeout,
        )
        if code or timed_out:
            raise ValueError(f"Cannot build upstream fixture {name}.\n{log}")
    return {key: directory / name for key, name in HELPERS.items()}


def run_suite(driver, target, source, build, wrapper, settings, env, timeout):
    """Run the upstream suite and collect each reported case in execution order."""
    build.mkdir()
    variables = {
        **settings,
        "CMAKE_MODULE_PATH": source.parent,
        "RunCMake_SOURCE_DIR": source,
        "RunCMake_BINARY_DIR": build,
        "RunCMake_TEST_TIMEOUT": min(timeout, 20),
        "EVAL_TEST_BINARY": target,
    }
    command = [driver, *(f"-D{key}={value}" for key, value in variables.items()), "-P", wrapper]
    code, timed_out, log = run(command, build, env, timeout)
    cases = {}
    occurrences = Counter()
    for match in CASE_RESULT.finditer(log):
        name = match["passed"] or match["failed"]
        occurrences[name] += 1
        cases[name, occurrences[name]] = {
            "passed": match["passed"] is not None,
            "detail": "\n".join(
                line.removeprefix("  ") for line in (match["detail"] or "").rstrip().splitlines()
            ),
        }
    complete = COMPLETE in log.splitlines() and code in (0, 1) and not timed_out
    other_errors = [
        line for line in CASE_RESULT.sub("", log).splitlines()
        if line.startswith(("CMake Error", "CMake Internal Error"))
    ]
    result = {"complete": complete, "timed_out": timed_out, "driver_errors": other_errors}
    if not complete or other_errors:
        result["log"] = log
    return cases, result


def compare(reference, candidate):
    """Score each case that passed with the reference binary."""
    results = []
    for name, occurrence in dict.fromkeys((*reference, *candidate)):
        key = name, occurrence
        expected = reference.get(key)
        actual = candidate.get(key)
        if expected is None:
            status = "no_reference"
        elif not expected["passed"]:
            status = "reference_failed"
        elif actual is None:
            status = "missing"
        else:
            status = "passed" if actual["passed"] else "failed"
        results.append({
            "name": name, "occurrence": occurrence, "status": status,
            "oracle": expected, "rcmake": actual,
        })
    return results


def evaluate(suites, driver, candidate, source, timeout):
    """Run the selected suites in separate reference and candidate workspaces."""
    # A CLI case can print the test environment into the report.
    env = {key: os.environ[key] for key in TEST_ENVIRONMENT if key in os.environ}

    ninja = shutil.which("ninja")
    generator = "Ninja" if ninja else "Unix Makefiles"
    settings = {
        "RunCMake_GENERATOR": generator,
        "RunCMake_GENERATOR_IS_MULTI_CONFIG": 0,
        "CMAKE_SYSTEM_NAME": platform.system(),
        "Python_EXECUTABLE": sys.executable,
    }
    if ninja:
        settings["RunCMake_MAKE_PROGRAM"] = ninja
    print(f"{'Suite':<32} {'Passed':>6} {'Failed':>6} {'Total':>6} {'Ref. fail':>9}", flush=True)
    results = {}
    # Use the physical path. CMake and some checks canonicalize /tmp on macOS.
    with tempfile.TemporaryDirectory(prefix="runcmake-eval-", dir="/tmp") as temporary:
        work = Path(temporary).resolve()
        temp = work / "tmp"
        temp.mkdir()
        env["TMPDIR"] = str(temp)
        wrapper = work / "driver.cmake"
        wrapper.write_text(
            'set(CMAKE_COMMAND "${EVAL_TEST_BINARY}")\n'
            'include("${RunCMake_SOURCE_DIR}/RunCMakeTest.cmake" NO_POLICY_SCOPE)\n'
            'message(STATUS "runcmake_eval: suite complete")\n', encoding="utf-8",
        )
        if {"CommandLine", "execute_process"}.intersection(suites):
            settings.update(prepare_helpers(source, work / "helpers", env, timeout))
        relative_source = source.relative_to(source.parents[1])
        for lane in ("oracle", "rcmake"):
            shutil.copytree(
                source.parents[1], work / lane / "source", symlinks=True,
                ignore=shutil.ignore_patterns("._*", ".git"),
            )
            (work / lane / "build" / relative_source).mkdir(parents=True)
        for suite in suites:
            by_lane = {}
            for lane, target in (("oracle", driver), ("rcmake", candidate)):
                root = work / lane
                by_lane[lane] = run_suite(
                    driver, target, root / "source" / relative_source / suite,
                    root / "build" / relative_source / suite, wrapper, settings, env, timeout,
                )
            reference, oracle_driver = by_lane["oracle"]
            actual, rcmake_driver = by_lane["rcmake"]
            cases = compare(reference, actual)
            count = Counter(case["status"] for case in cases)
            failed = count["failed"] + count["missing"]
            total = count["passed"] + failed
            results[suite] = {
                "passed": count["passed"], "failed": failed, "total": total,
                "missing": count["missing"], "reference_failed": count["reference_failed"],
                "no_reference": count["no_reference"], "cases": cases,
                "oracle_driver": oracle_driver, "rcmake_driver": rcmake_driver,
            }
            print(
                f"{suite:<32} {count['passed']:>6} {failed:>6} {total:>6}"
                f" {count['reference_failed']:>9}", flush=True,
            )
            for lane, metadata in (("oracle", oracle_driver), ("Rcmake", rcmake_driver)):
                if metadata["timed_out"]:
                    print(f"  {lane} exceeded the suite timeout.", flush=True)
                elif not metadata["complete"] or metadata["driver_errors"]:
                    print(f"  {lane} reported a driver error.", flush=True)
        # Keep failure messages readable after the temporary directories are removed.
        for suite in results.values():
            for case in suite["cases"]:
                for lane in ("oracle", "rcmake"):
                    if case[lane] is not None:
                        case[lane]["detail"] = case[lane]["detail"].replace(str(work), "<sandbox>")
            for lane in ("oracle_driver", "rcmake_driver"):
                metadata = suite[lane]
                metadata["driver_errors"] = [
                    line.replace(str(work), "<sandbox>") for line in metadata["driver_errors"]
                ]
                if "log" in metadata:
                    metadata["log"] = metadata["log"].replace(str(work), "<sandbox>")
    return generator, results


def main():
    """Parse options, run the comparison, and write the report."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--suite", action="append", metavar="NAME",
        help="Select a suite by name. Repeat to select more. Use all for every Wave 3 suite.",
    )
    parser.add_argument("--json", type=Path, metavar="PATH", help="Write the case report to PATH.")
    parser.add_argument(
        "--timeout", type=float, default=180, metavar="SECONDS",
        help="Limit each suite run to this many seconds. The default is 180 seconds.",
    )
    parser.add_argument(
        "--rcmake-bin", type=Path, metavar="PATH",
        help="Use this Rcmake executable. The default is $RCMAKE_ROOT/target/stage/bin/cmake.",
    )
    args = parser.parse_args()
    if not math.isfinite(args.timeout) or args.timeout <= 0:
        parser.error("--timeout must be a finite number greater than zero.")
    if os.name != "posix":
        parser.error("This evaluator requires a POSIX host.")
    try:
        source = Path(os.environ["RCMAKE_ORACLE_RUNCMAKE_DIR"]).resolve()
        driver = Path(os.environ["RCMAKE_ORACLE_CMAKE"]).absolute()
        candidate = args.rcmake_bin or Path(os.environ["RCMAKE_ROOT"]) / "target/stage/bin/cmake"
    except KeyError as exc:
        parser.error(f"Source env/env.sh to set {exc.args[0]}.")
    if source.parts[-2:] != ("Tests", "RunCMake") or not (source / "RunCMake.cmake").is_file():
        parser.error("RCMAKE_ORACLE_RUNCMAKE_DIR must select Tests/RunCMake in the CMake source tree.")
    candidate = candidate.absolute()  # Keep the cmake symlink for multi-call dispatch.
    suites = list(dict.fromkeys(
        suite for name in (args.suite or ["all"])
        for suite in (SUITES if name == "all" else ALIASES.get(name, (name,)))
    ))
    for suite in suites:
        if suite.startswith(".") or "/" in suite or not (source / suite / "RunCMakeTest.cmake").is_file():
            parser.error(f"Unknown upstream RunCMake suite {suite!r}.")
    try:
        version = subprocess.check_output(
            [driver, "--version"], timeout=args.timeout, text=True,
        ).splitlines()
        if version[:1] != ["cmake version 4.5.1"]:
            parser.error("RCMAKE_ORACLE_CMAKE must select CMake 4.5.1.")
        if not candidate.is_file() or not os.access(candidate, os.X_OK):
            parser.error(f"Rcmake binary is missing or cannot run: {candidate}")
        generator, results = evaluate(suites, driver, candidate, source, args.timeout)
        totals = {
            key: sum(suite[key] for suite in results.values())
            for key in ("passed", "failed", "total", "missing", "reference_failed", "no_reference")
        }
        rate = 100 * totals["passed"] / totals["total"] if totals["total"] else None
        print(
            f"{'Overall':<32} {totals['passed']:>6} {totals['failed']:>6} "
            f"{totals['total']:>6} {totals['reference_failed']:>9}"
        )
        print(f"Pass rate: {rate:.2f}%" if rate is not None else "Pass rate unavailable.")
        if args.json:
            report = {
                "oracle": str(driver), "rcmake": str(candidate), "source": str(source),
                "generator": generator, "timeout_seconds": args.timeout, "totals": totals,
                "pass_rate_percent": rate, "suites": results,
            }
            args.json.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    except (OSError, ValueError, subprocess.SubprocessError) as exc:
        parser.exit(2, f"{parser.prog}: {exc}\n")
    if not totals["total"] or any(
        not suite[f"{lane}_driver"]["complete"] or suite[f"{lane}_driver"]["driver_errors"]
        for suite in results.values() for lane in ("oracle", "rcmake")
    ):
        return 2
    return int(bool(totals["failed"] or totals["reference_failed"] or totals["no_reference"]))


if __name__ == "__main__":
    raise SystemExit(main())

#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Compare staged Rcmake with CMake 4.5.1 for Milestone 6.

Source env/env.sh, then run this script with --rcmake and --oracle.
Use --projects synthetic zlib to include a local conformance/corpus/zlib
checkout. Each run uses a private source copy and build directory.
"""

from __future__ import annotations

import argparse
from dataclasses import dataclass
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import re
import shutil
import signal
import statistics
import subprocess
import sys
import tarfile
import tempfile
import threading
import time
import tomllib


ROOT = Path(__file__).resolve().parents[2]
CORPUS = ROOT / "conformance/corpus"
ORACLE_VERSION = "4.5.1"
TOOLS = ("rcmake", "oracle")
PHASES = ("cold configure", "warm probe-cache configure",
          "no-op reconfigure", "single-file touch reconfigure")
UTILITIES = ("-P script", "-E tar", "-E copy", "-E sha256sum", "CTest parallel")
RSS = {
    "Darwin": ("-l", r"^\s*(\d+)\s+maximum resident set size$", 1),
    "Linux": ("-v", r"Maximum resident set size \(kbytes\):\s*(\d+)", 1024),
}


@dataclass
class Sample:
    """Record one command result or a failed prerequisite."""

    ms: float | None = None
    peak_rss_bytes: int | None = None
    error: str | None = None
    probe_cache_files_before: int | None = None


def measure(command, cwd, env, timeout):
    """Measure wall time and read peak RSS from the system time command."""
    flag, pattern, bytes_per_unit = RSS[platform.system()]
    timed_out = threading.Event()
    with tempfile.TemporaryFile() as log:
        start = time.perf_counter()
        with subprocess.Popen(
            ["/usr/bin/time", flag, *command], cwd=cwd, env=env,
            stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT,
            start_new_session=True,
        ) as process:
            def kill_group():
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass

            def deadline():
                timed_out.set()
                kill_group()

            timer = threading.Timer(timeout, deadline)
            timer.start()
            try:
                process.wait()
                elapsed = (time.perf_counter() - start) * 1000
            except KeyboardInterrupt:
                kill_group()
                process.wait()
                raise
            finally:
                timer.cancel()
                timer.join()
            if timed_out.is_set():
                return Sample(error=f"Timed out after {timeout:g} s."), ""
        log.seek(max(0, log.seek(0, os.SEEK_END) - 32768))
        output = log.read().decode("utf-8", errors="replace")
    matches = re.findall(pattern, output, re.MULTILINE)
    if process.returncode:
        diagnostic = "\n".join(
            line for line in output.splitlines()
            if line and not line.startswith((" ", "\t"))
        )[-2000:]
        return Sample(error=f"Exit status {process.returncode}. {diagnostic}"), output
    if not matches or int(matches[-1]) == 0:
        return Sample(error="The time command did not report peak RSS."), output
    return Sample(elapsed, int(matches[-1]) * bytes_per_unit), output


def synthetic_project(source, directories):
    """Create a C compiler probe and one library per subdirectory."""
    source.mkdir()
    lines = [
        "cmake_minimum_required(VERSION 4.5)",
        "project(M6Bench LANGUAGES C)",
        "include(CheckCSourceCompiles)",
        'check_c_source_compiles("int main(void) { return 0; }" M6_PROBE)',
        "if(NOT M6_PROBE)",
        '  message(FATAL_ERROR "The C compiler probe failed.")',
        "endif()",
    ]
    for index in range(directories):
        name = f"lib{index}"
        child = source / name
        child.mkdir()
        (child / "CMakeLists.txt").write_text(f"add_library({name} STATIC main.c)\n")
        (child / "main.c").write_text(f"int {name}(void) {{ return {index}; }}\n")
        lines.append(f"add_subdirectory({name})")
    (source / "CMakeLists.txt").write_text("\n".join(lines) + "\n")


def select_projects(names, scratch, directories):
    """Resolve project names beneath the corpus directory."""
    profiles = {}
    with (CORPUS / "manifest.toml").open("rb") as manifest:
        for project in tomllib.load(manifest)["project"]:
            profiles[project["name"]] = project.get("configure_args", [])
    projects = {}
    for name in names:
        if name == "synthetic":
            source = scratch / "synthetic"
            if not source.exists():
                synthetic_project(source, directories)
            projects[name] = (source, [])
            continue
        source = (CORPUS / name).resolve()
        if not source.is_relative_to(CORPUS.resolve()) or not (source / "CMakeLists.txt").is_file():
            raise ValueError(f"Project {name!r} needs a local checkout under {CORPUS}.")
        projects[name] = (source, profiles.get(name, []))
    return projects


def configure_runs(binary, project, work, generator, env, timeout, synthetic):
    """Run the four configure phases in one isolated trial."""
    original, configure_args = project
    source = work / "source"
    shutil.copytree(original, source, symlinks=True)
    lists = source / "CMakeLists.txt"
    if lists.is_symlink():
        lists.unlink()
        shutil.copyfile(original / lists.name, lists)
    build = work / "build"
    probe = work / "probes"
    probe.mkdir()
    env = {**env, "RCMAKE_PROBE_CACHE_DIR": str(probe)}
    command = [binary, "-S", source, "-B", build, "-G", generator, *configure_args]
    build_file = "build.ninja" if generator == "Ninja" else "Makefile"
    results = {}
    for phase in PHASES:
        if results and next(reversed(results.values())).error:
            results[phase] = Sample(error="The previous configure phase failed.")
            continue
        if phase == "warm probe-cache configure":
            shutil.rmtree(build)
        if phase == "single-file touch reconfigure":
            stat = lists.stat()
            os.utime(lists, ns=(stat.st_atime_ns, max(time.time_ns(), stat.st_mtime_ns + 1)))
        files_before = sum(path.is_file() for path in probe.rglob("*"))
        sample, _ = measure(command, work, env, timeout)
        sample.probe_cache_files_before = files_before
        if not sample.error:
            cache = build / "CMakeCache.txt"
            if not cache.is_file() or not (build / build_file).is_file():
                sample.error = "Configure did not produce the cache and build file."
            elif synthetic and "M6_PROBE:INTERNAL=1" not in cache.read_text(errors="replace"):
                sample.error = "The synthetic compiler probe result is missing."
        results[phase] = sample
    return results


def utility_inputs(scratch, quick):
    """Create the script, file, and CTest tests before starting the timer."""
    iterations = 100 if quick else 10000
    script = scratch / "bench.cmake"
    script.write_text(
        "cmake_minimum_required(VERSION 4.5)\n"
        "set(total 0)\n"
        f"foreach(i RANGE 1 {iterations})\n"
        '  math(EXPR total "${total}+1")\n'
        "endforeach()\n"
        'message(STATUS "M6 result=${total}")\n'
    )
    payload = scratch / "payload.bin"
    payload.write_bytes(bytes(range(256)) * (256 if quick else 16384))
    test_count = 12 if quick else 128
    test_file = "".join(f'add_test(test_{index} "/usr/bin/true")\n'
                        for index in range(test_count))
    return script, iterations, payload, test_file, test_count


def utility_runs(binary, ctest, inputs, work, env, timeout):
    """Run the script, file utilities, and parallel CTest."""
    script, iterations, payload, test_file, test_count = inputs
    (work / "CTestTestfile.cmake").write_text(test_file)
    target = work / payload.name
    shutil.copyfile(payload, target)
    expected = payload.read_bytes()
    digest = hashlib.sha256(expected).hexdigest()
    commands = (
        [binary, "-P", script],
        [binary, "-E", "tar", "cf", "payload.tar", target.name],
        [binary, "-E", "copy", target.name, "copy.bin"],
        [binary, "-E", "sha256sum", target.name],
        [ctest, "--test-dir", work, "--parallel", "4", "--output-on-failure"],
    )
    results = {}
    for name, command in zip(UTILITIES, commands):
        sample, output = measure(command, work, env, timeout)
        if not sample.error:
            try:
                if name == "-P script":
                    valid = f"-- M6 result={iterations}" in output.splitlines()
                elif name == "-E tar":
                    with tarfile.open(work / "payload.tar") as archive:
                        member = archive.extractfile(target.name)
                        valid = member is not None and member.read() == expected
                elif name == "-E copy":
                    valid = (work / "copy.bin").read_bytes() == expected
                elif name == "-E sha256sum":
                    valid = any(line.startswith(f"{digest} ") for line in output.splitlines())
                else:
                    valid = re.search(
                        rf"100% tests passed(?:, 0 tests failed)? out of {test_count}\b", output,
                    ) is not None
            except (OSError, KeyError, tarfile.TarError):
                valid = False
            if not valid:
                sample.error = "The command produced an unexpected result."
        results[name] = sample
    return results


def summarize(samples):
    """Calculate metrics from successful samples and retain every result."""
    times = [sample.ms for sample in samples if not sample.error]
    memory = [sample.peak_rss_bytes for sample in samples if not sample.error]
    quartiles = statistics.quantiles(times, n=4, method="inclusive") if len(times) > 1 else None
    return {
        "median_ms": statistics.median(times) if times else None,
        "iqr_ms": quartiles[2] - quartiles[0] if quartiles else (0.0 if times else None),
        "min_ms": min(times, default=None),
        "max_ms": max(times, default=None),
        "peak_rss_bytes": max(memory, default=None),
        "failures": len(samples) - len(times),
        "samples": [vars(sample) for sample in samples],
    }


def result_row(project, case, by_tool):
    """Compare medians when every sample passed on both tools."""
    row = {"project": project, "case": case, **{
        tool: summarize(by_tool[tool][case]) for tool in TOOLS
    }}
    row["speedup"] = (row["oracle"]["median_ms"] / row["rcmake"]["median_ms"]
                      if not any(row[tool]["failures"] for tool in TOOLS) else None)
    return row


def markdown(report):
    """Render timings, memory, and failures as Markdown."""
    meta = report["meta"]
    lines = [
        "# Milestone 6 benchmark", "",
        f"{meta['runs']} runs per tool on {meta['system']} {meta['machine']}. "
        f"Generator: {meta['generator']}. Timeout: {meta['timeout_seconds']:g} s per command.",
        f"Rcmake profile: {meta['rcmake_profile']}. Load average: {meta['load_average'][0]:.2f} "
        f"over one minute on {meta['cpus']} logical CPUs.",
        "",
        "Time columns show median / interquartile range (IQR) / min / max, in ms. "
        "RSS is the largest peak resident set size among successful runs, in MiB. "
        "Speedup divides the CMake median by the Rcmake median.",
        "",
        "Cold configure starts with an empty build directory and probe cache. "
        "The warm run clears the build directory and keeps the probe cache. "
        "Both reconfigure runs use the warm build directory. "
        "The operating system keeps its file cache across runs.",
        "",
        "| Project | Case | Rcmake time | CMake 4.5.1 time | Rcmake RSS | CMake RSS | Speedup |",
        "| --- | --- | ---: | ---: | ---: | ---: | ---: |",
    ]
    details = []
    for row in report["results"]:
        timings, memory = [], []
        title = row["project"].replace("|", r"\|")
        for tool in TOOLS:
            item = row[tool]
            timings.append(" / ".join(f"{item[key]:.2f}" for key in
                           ("median_ms", "iqr_ms", "min_ms", "max_ms"))
                           if item["median_ms"] is not None else "—")
            rss = item["peak_rss_bytes"]
            memory.append(f"{rss / 1048576:.2f}" if rss is not None else "—")
            if item["failures"]:
                error = next(sample["error"] for sample in item["samples"] if sample["error"])
                details.append(
                    f"- {title}, {row['case']}, {tool}: {item['failures']} failed. "
                    f"{' '.join(error.split())}"
                )
            if row["case"] == PHASES[1] and tool == "rcmake":
                count = sum(sample["probe_cache_files_before"] == 0 for sample in item["samples"])
                if count:
                    details.append(
                        f"- {title}: Rcmake had an empty probe cache before {count} warm runs. "
                        "Those runs measure clean configure with the cache path set."
                    )
        ratio = f"{row['speedup']:.2f}x" if row["speedup"] is not None else "—"
        lines.append(f"| {title} | {row['case']} | {' | '.join(timings + memory)} | {ratio} |")
    if details:
        lines += ["", *details]
    return "\n".join(lines) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rcmake", type=Path, default=ROOT / "target/stage/bin/cmake")
    parser.add_argument("--oracle", type=Path, default=os.environ.get("RCMAKE_ORACLE_CMAKE"))
    parser.add_argument("--projects", nargs="+", default=["synthetic"],
                        help="Project names under conformance/corpus, or synthetic.")
    parser.add_argument("--runs", type=int, default=5, help="Runs per tool and case.")
    parser.add_argument("--quick", action="store_true", help="Use smaller fixtures and shorter timeouts.")
    parser.add_argument("--timeout", type=float, help="Seconds per command. Default: 180, or 30 in quick mode.")
    parser.add_argument("--json", type=Path, metavar="OUT")
    parser.add_argument("--markdown", type=Path, metavar="OUT")
    args = parser.parse_args()
    timeout = args.timeout if args.timeout is not None else (30 if args.quick else 180)
    if args.runs < 1 or not math.isfinite(timeout) or timeout <= 0:
        parser.error("Use at least one run and a finite, positive timeout.")
    if platform.system() not in RSS:
        parser.error("Peak RSS measurement requires macOS or Linux.")
    if args.oracle is None:
        parser.error("Set --oracle or source env/env.sh.")
    binaries = {"rcmake": args.rcmake.absolute(), "oracle": args.oracle.absolute()}
    ctests = {tool: path.with_name("ctest") for tool, path in binaries.items()}
    for path in (*binaries.values(), *ctests.values()):
        if not path.is_file() or not os.access(path, os.X_OK):
            parser.error(f"Cannot execute {path}.")

    env = {
        key: value for key, value in os.environ.items()
        if not key.startswith(("CMAKE_", "CTEST_", "CPACK_"))
        and key not in {"RCMAKE_PROBE_CACHE_DIR", "MAKEFLAGS", "PWD"}
    }
    env.update(LC_ALL="C", COPYFILE_DISABLE="1")
    generator = "Ninja" if shutil.which("ninja") else "Unix Makefiles"
    load = os.getloadavg()
    directories = 4 if args.quick else 64
    rows = []
    with tempfile.TemporaryDirectory(prefix="rcmake-bench-m6-", dir="/tmp") as temp:
        scratch = Path(temp)
        sample, version = measure([binaries["oracle"], "--version"], scratch, env, timeout)
        if sample.error:
            raise ValueError(f"Cannot read the oracle version. {sample.error}")
        if f"cmake version {ORACLE_VERSION}" not in version.splitlines():
            raise ValueError(f"The oracle must report CMake version {ORACLE_VERSION}.")
        sample, version = measure([binaries["rcmake"], "--version"], scratch, env, timeout)
        if sample.error:
            raise ValueError(f"Cannot read the Rcmake version. {sample.error}")
        rcmake_version = version.splitlines()[:2]
        projects = select_projects(args.projects, scratch, directories)
        inputs = utility_inputs(scratch, args.quick)
        for index, (name, project) in enumerate([*projects.items(), ("Utilities", None)]):
            print(f"Measuring {name} ({args.runs} runs per tool).", file=sys.stderr, flush=True)
            cases = PHASES if project else UTILITIES
            by_tool = {tool: {case: [] for case in cases} for tool in TOOLS}
            for trial in range(args.runs):
                order = TOOLS if (trial + index) % 2 == 0 else TOOLS[::-1]
                for tool in order:
                    work = scratch / f"{index}-{trial}-{tool}"
                    work.mkdir()
                    if project:
                        samples = configure_runs(binaries[tool], project, work, generator, env,
                                                 timeout, synthetic=name == "synthetic")
                    else:
                        samples = utility_runs(binaries[tool], ctests[tool], inputs, work, env, timeout)
                    for case, sample in samples.items():
                        by_tool[tool][case].append(sample)
                    shutil.rmtree(work)
            rows.extend(result_row(name, case, by_tool) for case in cases)

    report = {
        "meta": {
            "runs": args.runs, "quick": args.quick, "timeout_seconds": timeout,
            "system": platform.system(), "os_release": platform.release(),
            "machine": platform.machine(), "cpus": os.cpu_count(), "load_average": load,
            "generator": generator, "synthetic_directories": directories,
            "ctest_jobs": 4, "ctest_tests": inputs[-1], "oracle_version": ORACLE_VERSION,
            "rcmake_version": rcmake_version,
            "rcmake_profile": next((part for part in reversed(binaries["rcmake"].resolve().parts)
                                    if part in {"release", "debug"}), "unknown"),
            "binaries": {tool: str(path) for tool, path in binaries.items()},
            "projects": {name: {"source": "synthetic" if name == "synthetic" else str(source),
                                "configure_args": configure_args}
                         for name, (source, configure_args) in projects.items()},
        },
        "results": rows,
    }
    output = markdown(report)
    if args.json:
        args.json.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    if args.markdown:
        args.markdown.write_text(output, encoding="utf-8")
    print(output, end="")
    return int(any(row[tool]["failures"] for row in rows for tool in TOOLS))


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, ValueError) as exc:
        sys.exit(str(exc))

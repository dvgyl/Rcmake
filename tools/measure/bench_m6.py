#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Compare staged Rcmake with CMake 4.5.1 for Milestone 6.

Source env/env.sh, then run this script with --rcmake and --oracle.
Use --projects synthetic zlib to include a local conformance/corpus/zlib
checkout. Each run uses a private source copy and build directory.
Use --scenario all to run the Milestone 6 configure scenarios.
Use --require-gates to enforce the acceptance thresholds.
"""

from __future__ import annotations

import argparse
from contextlib import ExitStack
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

from mutation_eval import API, discover_leaves
from oracle_prop_test import run_command


ROOT = Path(__file__).resolve().parents[2]
CORPUS = ROOT / "conformance/corpus"
ORACLE_VERSION = "4.5.1"
TOOLS = ("rcmake", "oracle")
PHASES = ("cold configure", "warm probe-cache configure",
          "no-op reconfigure", "single-file touch reconfigure")
COLD, WARM_PROBE, NO_OP, ROOT_TOUCH = PHASES
INCREMENTAL_LEAF = "incremental-leaf"
COLD_FETCH, WARM_FETCH = "cold FetchContent configure", "warm-fetch"
SCENARIOS = {"warm-probe": WARM_PROBE, INCREMENTAL_LEAF: INCREMENTAL_LEAF, WARM_FETCH: WARM_FETCH}
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
    replay_record_before: bool | None = None
    fetch_origins_hidden: bool | None = None


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


def synthetic_project(source, directories, probes):
    """Create independent C compiler probes and one library per subdirectory."""
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
    for index in range(1, probes):
        lines.extend((
            f'check_c_source_compiles("int main(void) {{ return {index} - {index}; }}" M6_PROBE_{index})',
            f"if(NOT M6_PROBE_{index})",
            f'  message(FATAL_ERROR "C compiler probe {index} failed.")',
            "endif()",
        ))
    for index in range(directories):
        name = f"lib{index}"
        child = source / name
        child.mkdir()
        (child / "CMakeLists.txt").write_text(f"add_library({name} STATIC main.c)\n")
        (child / "main.c").write_text(f"int {name}(void) {{ return {index}; }}\n")
        lines.append(f"add_subdirectory({name})")
    (source / "CMakeLists.txt").write_text("\n".join(lines) + "\n")


def select_projects(names, scratch, directories, probes):
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
                synthetic_project(source, directories, probes)
            projects[name] = (source, [])
            continue
        source = (CORPUS / name).resolve()
        if not source.is_relative_to(CORPUS.resolve()) or not (source / "CMakeLists.txt").is_file():
            raise ValueError(f"Project {name!r} needs a local checkout under {CORPUS}.")
        projects[name] = (source, profiles.get(name, []))
    return projects


def with_store(env, store):
    """Use only the trial's local store for Rcmake."""
    return {**env, "RCMAKE_CACHE_DIR": str(store), "RCMAKE_PROBE_CACHE_DIR": str(store)}


def configure_sample(binary, source, build, configure_args, work, generator, env, timeout):
    """Time a configure and require the cache and the generator output."""
    command = [binary, "-S", source, "-B", build, "-G", generator, *configure_args]
    sample, _ = measure(command, work, env, timeout)
    build_file = "build.ninja" if generator == "Ninja" else "Makefile"
    if not sample.error and not all((build / name).is_file() for name in ("CMakeCache.txt", build_file)):
        sample.error = "Configure did not produce the cache and build file."
    return sample


def entry_count(store, domain):
    """Count published entries in one cache domain."""
    return sum(path.is_file() for path in (store / "entries" / f"{domain}-v1").rglob("*.entry"))


def configure_runs(binary, project, work, generator, env, timeout, synthetic, cases):
    """Run the selected configure phases in one isolated trial."""
    original, configure_args = project
    source = work / "source"
    shutil.copytree(original, source, symlinks=True)
    lists = source / "CMakeLists.txt"
    if lists.is_symlink():
        lists.unlink()
        shutil.copyfile(original / lists.name, lists)
    cold, warm = work / "cold", work / "warm"
    store = work / "store"
    env = with_store(env, store)
    if INCREMENTAL_LEAF in cases:
        query = cold / API / "query"
        query.mkdir(parents=True)
        (query / "codemodel-v2").touch()
    results = {}
    for phase in cases:
        prior = WARM_PROBE if phase == NO_OP else NO_OP if phase == ROOT_TOUCH else COLD
        if phase != COLD and results[prior].error:
            results[phase] = Sample(error=f"The {prior} phase failed.")
            continue
        build = cold if phase in (COLD, INCREMENTAL_LEAF) else warm
        replay = None
        if phase == ROOT_TOUCH:
            stat = lists.stat()
            os.utime(lists, ns=(stat.st_atime_ns, max(time.time_ns(), stat.st_mtime_ns + 1)))
        if phase == INCREMENTAL_LEAF:
            try:
                leaves = discover_leaves(source, build)
                if not leaves:
                    raise ValueError("The codemodel has no writable leaf CMakeLists.txt.")
                leaf = next((leaf for leaf in leaves if leaf.targets), leaves[0])
                with (source / leaf.lists).open("ab") as stream:
                    stream.write(b"\n# Milestone 6 benchmark edit.\n")
                replay = (build / "CMakeFiles/rcmake/configure.v1.bin").is_file()
            except (OSError, ValueError, KeyError, StopIteration) as error:
                results[phase] = Sample(error=f"Could not edit a configured leaf. {error}")
                continue
        files_before = entry_count(store, "probe") + sum(
            path.is_file() for path in (store / "probe/v1").rglob("*.json")
        )
        sample = configure_sample(binary, source, build, configure_args, work, generator, env, timeout)
        sample.probe_cache_files_before = files_before
        sample.replay_record_before = replay
        if not sample.error and synthetic:
            cache = build / "CMakeCache.txt"
            if "M6_PROBE:INTERNAL=1" not in cache.read_text(errors="replace"):
                sample.error = "The synthetic compiler probe result is missing."
        results[phase] = sample
    return results


def fetch_fixture(root, env, timeout):
    """Create a local archive and a Git repository with a pinned commit."""
    root.mkdir()
    archive_source = root / "archive-source"
    archive_source.mkdir()
    (archive_source / "CMakeLists.txt").write_text("add_library(archive_dep INTERFACE)\n")
    archive = root / "fixture.tar.gz"
    with tarfile.open(archive, "w:gz") as stream:
        stream.add(archive_source, arcname=archive_source.name)
    git = root / "git-source"
    git.mkdir()
    (git / "CMakeLists.txt").write_text("add_library(git_dep INTERFACE)\n")
    for args in (["init", "--quiet", "--template=", "--object-format=sha1"],
                 ["add", "CMakeLists.txt"],
                 ["-c", "user.name=M6 Benchmark", "-c", "user.email=m6@example.invalid",
                  "commit", "--quiet", "--no-gpg-sign", "-m", "Add fixture"],
                 ["rev-parse", "HEAD"]):
        result = run_command(["git", *args], git, timeout, env)
        if result["error"] or result["returncode"]:
            raise ValueError(f"Could not create the local Git fixture. {result['error'] or result['stderr']}")
    commit = result["stdout"].strip()
    source = root / "source"
    source.mkdir()
    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    (source / "CMakeLists.txt").write_text(f"""\
cmake_minimum_required(VERSION 4.5)
project(M6Fetch NONE)
include(FetchContent)
FetchContent_Declare(archive_dep URL "{archive.as_uri()}" URL_HASH SHA256={digest})
FetchContent_Declare(git_dep GIT_REPOSITORY "{git.as_uri()}" GIT_TAG {commit})
FetchContent_MakeAvailable(archive_dep git_dep)
if(NOT TARGET archive_dep OR NOT TARGET git_dep)
  message(FATAL_ERROR "Expected both FetchContent targets.")
endif()
""", encoding="utf-8")
    return source, (archive, git)


def fetch_runs(binary, fixture, work, generator, env, timeout):
    """Time cold and warm FetchContent in fresh build directories."""
    source, origins = fixture
    store = work / "store"
    env = with_store({**env, "RCMAKE_FETCH_PREFETCH": "1"}, store)
    args = (binary, source)
    cold = configure_sample(*args, work / "cold", [], work, generator, env, timeout)
    if cold.error:
        return {COLD_FETCH: cold, WARM_FETCH: Sample(error="The cold FetchContent configure failed.")}
    cached = bool(entry_count(store, "fetch") and any((store / "git").rglob("HEAD")))
    with ExitStack() as restore:
        if cached:
            # Require a fresh build to succeed when only the store has the inputs.
            for origin in origins:
                hidden = origin.with_suffix(origin.suffix + ".hidden")
                origin.rename(hidden)
                restore.callback(hidden.rename, origin)
            env = {**env, "RCMAKE_FETCH_OFFLINE": "1"}
        warm = configure_sample(*args, work / "warm", [], work, generator, env, timeout)
    warm.fetch_origins_hidden = cached
    return {COLD_FETCH: cold, WARM_FETCH: warm}


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
        "p95_ms": sorted(times)[math.ceil(0.95 * len(times)) - 1] if times else None,
        "iqr_ms": quartiles[2] - quartiles[0] if quartiles else (0.0 if times else None),
        "min_ms": min(times, default=None),
        "max_ms": max(times, default=None),
        "peak_rss_bytes": max(memory, default=None),
        "failures": len(samples) - len(times),
        "samples": [vars(sample) for sample in samples],
    }


def result_row(project, case, by_tool, cold):
    """Compare medians when every sample passed on both tools."""
    row = {"project": project, "case": case, **{
        tool: summarize(by_tool[tool][case]) for tool in TOOLS
    }}
    rcmake, oracle = (row[tool] for tool in TOOLS)
    row["speedup"] = (oracle["median_ms"] / rcmake["median_ms"]
                      if not (rcmake["failures"] or oracle["failures"]) else None)
    baseline = summarize(by_tool["rcmake"][cold]) if cold else None
    row["cold_speedup"] = (baseline["median_ms"] / rcmake["median_ms"]
                           if baseline and not (baseline["failures"] or rcmake["failures"]) else None)
    return row


def scenario_gate(row):
    """Assess the measured speed and the cache or replay evidence."""
    case = row["case"]
    samples = row["rcmake"]["samples"]
    if row["speedup"] is None:
        return {"passed": False, "detail": "A configure failed."}
    if case == WARM_PROBE:
        if not all(sample["probe_cache_files_before"] for sample in samples):
            return {"passed": False, "detail": "The warm run started without published probe entries."}
        return {"passed": row["speedup"] >= 5, "detail": "Target at least 5× versus fresh CMake. Stretch target 10×."}
    if case == INCREMENTAL_LEAF:
        if not all(sample["replay_record_before"] for sample in samples):
            return {"passed": False, "detail": "The replay record was missing before the leaf edit."}
        return {"passed": row["rcmake"]["p95_ms"] < 100, "detail": "Target Rcmake p95 below 100 ms."}
    return {"passed": all(sample["fetch_origins_hidden"] for sample in samples),
            "detail": "Require an offline configure from the archive and Git caches with both origins hidden."}


def markdown(report):
    """Render timings, memory, and failures as Markdown."""
    meta = report["meta"]
    lines = [
        "# Milestone 6 benchmark", "",
        f"Runs per tool {meta['runs']} on {meta['system']} {meta['machine']}. "
        f"Generator {meta['generator']}. Timeout {meta['timeout_seconds']:g} s per command.",
        f"Rcmake profile {meta['rcmake_profile']}. Load average {meta['load_average'][0]:.2f} "
        f"over one minute on {meta['cpus']} logical CPUs.",
        "",
        "Time columns show median and p95 in ms. "
        "RSS is the largest peak resident set size among successful runs, in MiB. "
        "Speedups divide the reference median by the Rcmake median.",
        "",
        "Cold configure starts with an empty build directory and probe cache. "
        "Warm probe and FetchContent runs use a fresh build directory and the same store. "
        "The leaf run appends a comment to one configured leaf and reuses its build directory. "
        "The operating system keeps its file cache across runs.",
        "",
        "| Project | Case | Rcmake median / p95 | CMake 4.5.1 median / p95 | Rcmake RSS | CMake RSS | vs CMake | vs Rcmake cold | Gate |",
        "| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | --- |",
    ]
    details = []
    for row in report["results"]:
        timings, memory = [], []
        title = row["project"].replace("|", r"\|")
        for tool in TOOLS:
            item = row[tool]
            timings.append(" / ".join(f"{item[key]:.2f}" for key in ("median_ms", "p95_ms"))
                           if item["median_ms"] is not None else "—")
            rss = item["peak_rss_bytes"]
            memory.append(f"{rss / 1048576:.2f}" if rss is not None else "—")
            if item["failures"]:
                error = next(sample["error"] for sample in item["samples"] if sample["error"])
                details.append(
                    f"- {title}, {row['case']}, {tool}. {item['failures']} failed. "
                    f"{' '.join(error.split())}"
                )
            if row["case"] == WARM_PROBE and tool == "rcmake" and "gate" not in row:
                count = sum(sample["probe_cache_files_before"] == 0 for sample in item["samples"])
                if count:
                    details.append(
                        f"- {title}. Rcmake had an empty probe cache before {count} warm runs. "
                        "Those runs measure clean configure with the cache path set."
                    )
        ratio = f"{row['speedup']:.2f}x" if row["speedup"] is not None else "—"
        cold = f"{row['cold_speedup']:.2f}x" if row["cold_speedup"] is not None else "—"
        gate = "—"
        if "gate" in row:
            gate = "Pass" if row["gate"]["passed"] else "Fail"
            details.append(f"- {title}, {row['case']}. {row['gate']['detail']}")
        lines.append(f"| {title} | {row['case']} | {' | '.join(timings + memory)} | {ratio} | {cold} | {gate} |")
    if details:
        lines += ["", *details]
    return "\n".join(lines) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rcmake", type=Path, default=ROOT / "target/stage/bin/cmake")
    parser.add_argument("--oracle", type=Path, default=os.environ.get("RCMAKE_ORACLE_CMAKE"))
    parser.add_argument("--projects", nargs="+", default=["synthetic"],
                        help="Project names under conformance/corpus, or synthetic. Warm fetch uses its own fixture.")
    parser.add_argument("--runs", type=int, default=5, help="Runs per tool and case.")
    parser.add_argument("--quick", action="store_true", help="Use smaller fixtures and shorter timeouts.")
    parser.add_argument("--scenario", choices=(*SCENARIOS, "all"), action="append",
                        help="Run a configure scenario and its cold baseline. Repeat to select more than one.")
    parser.add_argument("--require-gates", action="store_true",
                        help="Return an error if a selected scenario misses its acceptance target.")
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
    if args.require_gates and not args.scenario:
        parser.error("Select a scenario when using --require-gates.")
    if args.json and args.markdown and args.json.resolve() == args.markdown.resolve():
        parser.error("Use different report paths.")
    requested = args.scenario or []
    selected = set(SCENARIOS if "all" in requested else requested)
    scenarios = [phase for name, phase in SCENARIOS.items() if name in selected]
    phases = (COLD, *(phase for phase in scenarios if phase != WARM_FETCH)) if selected else PHASES
    binaries = {"rcmake": args.rcmake.absolute(), "oracle": args.oracle.absolute()}
    ctests = {tool: path.with_name("ctest") for tool, path in binaries.items()}
    for path in (*binaries.values(), *(ctests.values() if not selected else ())):
        if not path.is_file() or not os.access(path, os.X_OK):
            parser.error(f"Cannot execute {path}.")

    env = {
        key: value for key, value in os.environ.items()
        if (not key.startswith(("CMAKE_", "CTEST_", "CPACK_", "RCMAKE_", "GIT_"))
            or key == "RCMAKE_VENDORED_DATA_ROOT")
        and key not in {"MAKEFLAGS", "PWD"}
    }
    env.update(LC_ALL="C", COPYFILE_DISABLE="1")
    generator = "Ninja" if shutil.which("ninja") else "Unix Makefiles"
    load = os.getloadavg()
    directories = 4 if args.quick else 64
    probes = 1 if args.quick or WARM_PROBE not in scenarios else 24
    fetch_env = {**env, "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": os.devnull}
    rows = []
    with tempfile.TemporaryDirectory(prefix="rcmake-bench-m6-", dir="/tmp") as temp:
        scratch = Path(temp).resolve()
        sample, version = measure([binaries["oracle"], "--version"], scratch, env, timeout)
        if sample.error:
            raise ValueError(f"Cannot read the oracle version. {sample.error}")
        if f"cmake version {ORACLE_VERSION}" not in version.splitlines():
            raise ValueError(f"The oracle must report CMake version {ORACLE_VERSION}.")
        sample, version = measure([binaries["rcmake"], "--version"], scratch, env, timeout)
        if sample.error:
            raise ValueError(f"Cannot read the Rcmake version. {sample.error}")
        rcmake_version = version.splitlines()[:2]
        projects = select_projects(args.projects, scratch, directories, probes) if len(phases) > 1 else {}
        inputs = utility_inputs(scratch, args.quick) if not selected else None
        workloads = [(name, project, phases) for name, project in projects.items()]
        if WARM_FETCH in scenarios:
            fixture = fetch_fixture(scratch / "fetch-fixture", fetch_env, timeout)
            workloads.append(("Local FetchContent", fixture, (COLD_FETCH, WARM_FETCH)))
        if inputs:
            workloads.append(("Utilities", inputs, UTILITIES))
        for index, (name, project, cases) in enumerate(workloads):
            print(f"Measuring {name} ({args.runs} runs per tool).", file=sys.stderr, flush=True)
            by_tool = {tool: {case: [] for case in cases} for tool in TOOLS}
            for trial in range(args.runs):
                order = TOOLS if (trial + index) % 2 == 0 else TOOLS[::-1]
                for tool in order:
                    work = scratch / f"{index}-{trial}-{tool}"
                    work.mkdir()
                    if cases == UTILITIES:
                        samples = utility_runs(binaries[tool], ctests[tool], inputs, work, env, timeout)
                    elif WARM_FETCH in cases:
                        samples = fetch_runs(binaries[tool], project, work, generator, fetch_env, timeout)
                    else:
                        samples = configure_runs(binaries[tool], project, work, generator, env,
                                                 timeout, synthetic=name == "synthetic", cases=cases)
                    for case, sample in samples.items():
                        by_tool[tool][case].append(sample)
                    shutil.rmtree(work)
            for case in cases:
                cold = cases[0] if case != cases[0] and cases != UTILITIES else None
                row = result_row(name, case, by_tool, cold)
                if case in scenarios:
                    row["gate"] = scenario_gate(row)
                rows.append(row)

    report = {
        "meta": {
            "runs": args.runs, "quick": args.quick, "timeout_seconds": timeout,
            "system": platform.system(), "os_release": platform.release(),
            "machine": platform.machine(), "cpus": os.cpu_count(), "load_average": load,
            "generator": generator, "synthetic_directories": directories,
            "synthetic_probes": probes,
            "scenarios": [name for name in SCENARIOS if name in selected],
            "ctest_jobs": 4 if inputs else None, "ctest_tests": inputs[-1] if inputs else None,
            "oracle_version": ORACLE_VERSION,
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
    failures = any(row[tool]["failures"] for row in rows for tool in TOOLS)
    missed_gate = any(not row["gate"]["passed"] for row in rows if "gate" in row)
    return int(failures or (args.require_gates and missed_gate))


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, ValueError) as exc:
        sys.exit(str(exc))

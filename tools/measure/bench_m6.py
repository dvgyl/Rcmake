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
from collections import Counter
from contextlib import ExitStack
from dataclasses import dataclass
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import re
import shlex
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

from determinism import observe_compilers
from mutation_eval import API, REPLAY_RESULT, discover_leaves, replay_leaves
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
REGISTRY_FLAGS = (
    "-DCMAKE_FIND_USE_PACKAGE_REGISTRY=FALSE",
    "-DCMAKE_FIND_USE_SYSTEM_PACKAGE_REGISTRY=FALSE",
    "-DCMAKE_EXPORT_NO_PACKAGE_REGISTRY=ON",
)
LEAF_MARKER = "RCMAKE_M6_BENCH_LEAF_EDIT"
CACHE_SETTINGS = re.compile(
    r"^((?:CMAKE_\w+_(?:COMPILER(?:_ARG1|_LAUNCHER)?|FLAGS)|CMAKE_(?:MAKE_PROGRAM"
    r"|TOOLCHAIN_FILE|OSX_SYSROOT|SYSROOT|FIND_USE_(?:SYSTEM_)?PACKAGE_REGISTRY"
    r"|EXPORT_NO_PACKAGE_REGISTRY))):[^=]*=(.*)$", re.MULTILINE,
)
COMPILER_CACHES = {"ccache", "sccache"}


@dataclass
class Sample:
    """Record one command result or a failed prerequisite."""

    ms: float | None = None
    peak_rss_bytes: int | None = None
    error: str | None = None
    unavailable: str | None = None
    probe_hits: int = 0
    replayed_regions: int = 0
    edited_leaf: str | None = None
    edited_leaf_executed: bool = False
    control_executed: bool = False
    fetch_origins_hidden: bool | None = None
    toolchain: dict | None = None


def benchmark_env(inherited):
    """Remove inherited caches and disable ccache in each child process."""
    ignored = ("CMAKE_", "CTEST_", "CPACK_", "RCMAKE_", "GIT_", "CCACHE_", "SCCACHE_")
    env = {key: value for key, value in inherited.items()
           if (not key.startswith(ignored) or key == "RCMAKE_VENDORED_DATA_ROOT")
           and key not in {"MAKEFLAGS", "PWD"}}
    env.update(LC_ALL="C", COPYFILE_DISABLE="1", CCACHE_DISABLE="1")
    if "PATH" in env:
        env["PATH"] = os.pathsep.join(
            part for part in env["PATH"].split(os.pathsep)
            if Path(part).name not in COMPILER_CACHES
        )
    for key in ("CC", "CXX", "FC", "OBJC", "OBJCXX", "CUDACXX", "HIPCXX"):
        compiler = shlex.split(env.get(key, ""))
        if compiler and Path(compiler[0]).name in COMPILER_CACHES:
            if len(compiler) < 2 or compiler[1].startswith("-"):
                raise ValueError(f"Set {key} to a compiler without a cache wrapper.")
            compiler = compiler[1:]
            env[key] = shlex.join(compiler)
        name = compiler[0] if compiler else {"CC": "cc", "CXX": "c++"}.get(key)
        path = shutil.which(name, path=env.get("PATH")) if name else None
        if path and Path(path).resolve().name in COMPILER_CACHES:
            raise ValueError(f"Set {key} to a compiler without a cache wrapper.")
    return env


def trial_env(env, work):
    """Use a private home and cache directory for this tool and trial."""
    home, cache = work / "home", work / "xdg-cache"
    home.mkdir()
    cache.mkdir()
    return {**env, "HOME": str(home), "XDG_CACHE_HOME": str(cache)}


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


def configure_sample(binary, source, build, configure_args, work, generator, env, timeout,
                     *, timed=True):
    """Time a configure and require the cache and the generator output."""
    command = [binary, "-S", source, "-B", build, "-G", generator, *configure_args, *REGISTRY_FLAGS]
    if timed:
        sample, _ = measure(command, work, env, timeout)
    else:
        result = run_command(command, work, timeout, env)
        error = result["error"]
        if result["returncode"] and not error:
            error = (result["stderr"] or result["stdout"])[-2000:] or f"Exit status {result['returncode']}."
        sample = Sample(error=error)
    build_file = "build.ninja" if generator == "Ninja" else "Makefile"
    cache = build / "CMakeCache.txt"
    if sample.error:
        return sample
    if not cache.is_file() or not (build / build_file).is_file():
        sample.error = "Configure did not produce the cache and build file."
        return sample
    settings = dict(CACHE_SETTINGS.findall(cache.read_text(errors="replace")))
    sample.toolchain = settings
    for key, value in settings.items():
        if key.endswith("_COMPILER_LAUNCHER") and value and not value.endswith("-NOTFOUND"):
            sample.error = f"The configure selected a compiler launcher in {key}."
        if key.endswith("_COMPILER") and value and (
            Path(value).resolve().name in COMPILER_CACHES or Path(value).name in COMPILER_CACHES
        ):
            sample.error = f"The configure selected a compiler cache in {key}."
    return sample


def entry_count(store, domain):
    """Count published entries in one cache domain."""
    return sum(path.is_file() for path in (store / "entries" / f"{domain}-v1").rglob("*.entry"))


def copy_source(original, work):
    """Copy benchmark sources and make the root list file writable."""
    source = work / "source"
    shutil.copytree(original, source, symlinks=True)
    lists = source / "CMakeLists.txt"
    if lists.is_symlink():
        lists.unlink()
        shutil.copyfile(original / lists.name, lists)
    return source


def query_codemodel(build):
    query = build / API / "query"
    query.mkdir(parents=True, exist_ok=True)
    (query / "codemodel-v2").touch()


def shared_leaves(source, build):
    """Read configured leaves that can be edited inside the source copy."""
    return {leaf.lists: leaf for leaf in discover_leaves(source, build)
            if leaf.lists != Path("CMakeLists.txt")}


def select_common_leaf(binaries, project, work, generator, env, timeout):
    """Choose one non-root leaf and one unchanged control from both codemodels."""
    original, configure_args = project
    leaves = {}
    for tool, binary in binaries.items():
        lane = work / tool
        lane.mkdir()
        source = copy_source(original, lane)
        build = lane / "build"
        query_codemodel(build)
        clean = with_store(trial_env(env, lane), lane / "store")
        sample = configure_sample(binary, source, build, configure_args, lane, generator, clean,
                                  timeout, timed=False)
        if sample.error:
            raise ValueError(f"Cannot discover leaves for {tool}. {sample.error}")
        leaves[tool] = shared_leaves(source, build)
    common = sorted(leaves["rcmake"].keys() & leaves["oracle"].keys())
    if not common:
        raise ValueError("The codemodels have no common non-root leaf.")
    leaf = next((path for path in common if all(lane[path].targets for lane in leaves.values())),
                common[0])
    alternatives = [path for path in common if path != leaf]
    control = next((path for path in alternatives if all(lane[path].targets for lane in leaves.values())),
                   alternatives[0] if alternatives else Path("CMakeLists.txt"))
    return leaf, control


def compiler_inputs(log):
    """Count source names passed to the compiler in probe trees."""
    return Counter(name for name in log.read_bytes().split(b"\0") if name) if log.is_file() else Counter()


def cache_hits(cold, warm):
    """Count cold probe compilations skipped in a fresh build with the same inputs."""
    return sum((cold - warm).values()) if not warm - cold else 0


def leaf_trace(trace, source, leaf, control):
    """Read command dispatch evidence for two selected CMake files."""
    rows = [json.loads(line) for line in trace.read_text().splitlines()]
    if not rows or rows[0].get("version", {}).get("major") != 1:
        raise ValueError("The configure did not emit a JSON trace.")
    edited = [row for row in rows[1:] if row.get("file") == str(source / leaf)]
    controls = [row for row in rows[1:] if row.get("file") == str(source / control)]
    marker = any(row.get("cmd") == "set" and row.get("args", [])[:2] == [LEAF_MARKER, "1"]
                 for row in edited)
    return bool(edited), bool(controls), marker


def configure_runs(binary, project, work, generator, env, timeout, *, synthetic, cases,
                   leaf_pair=None, unavailable=None, timed=True, replay_evidence=False):
    """Run the selected configure phases in one isolated trial."""
    original, configure_args = project
    source = copy_source(original, work)
    lists = source / "CMakeLists.txt"
    cold, warm = work / "cold", work / "warm"
    env = with_store(trial_env(env, work), work / "store")
    unavailable = unavailable or {}
    log = None
    if WARM_PROBE in cases and WARM_PROBE not in unavailable:
        env = observe_compilers(work, {**env, "RCMAKE_PROBE_JOBS": "1"}, intervals=False)
        log = Path(env["RCMAKE_M6_PROBE_LOG"])
    trace = work / "leaf-trace.jsonl"
    trace_args = []
    if leaf_pair:
        leaf, control = leaf_pair
        query_codemodel(cold)
        if replay_evidence:
            env["RCMAKE_MEASURE_REPLAY"] = "1"
        elif not timed:
            trace_args = ["--trace-format=json-v1", f"--trace-redirect={trace}",
                          f"--trace-source={source / leaf}", f"--trace-source={source / control}"]
    steady = cold if WARM_PROBE in unavailable else warm
    cold_inputs = Counter()
    control_targets = None
    results = {}
    for phase in cases:
        if phase in unavailable:
            results[phase] = Sample(unavailable=unavailable[phase])
            continue
        if phase == INCREMENTAL_LEAF and leaf_pair is None:
            results[phase] = Sample(unavailable="No common configured leaf was selected.")
            continue
        prior = NO_OP if phase == ROOT_TOUCH else COLD
        if phase == NO_OP and steady == warm:
            prior = WARM_PROBE
        if phase != COLD and results[prior].error:
            results[phase] = Sample(error=f"The {prior} phase failed.")
            continue
        build = cold if phase in (COLD, INCREMENTAL_LEAF) else warm if phase == WARM_PROBE else steady
        if phase == ROOT_TOUCH:
            stat = lists.stat()
            os.utime(lists, ns=(stat.st_atime_ns, max(time.time_ns(), stat.st_mtime_ns + 1)))
        if phase == INCREMENTAL_LEAF:
            try:
                cold_leaves = shared_leaves(source, build)
                if leaf not in cold_leaves:
                    raise ValueError(f"{leaf} is absent from the cold codemodel.")
                if control != Path("CMakeLists.txt"):
                    control_targets = cold_leaves[control].targets
                with (source / leaf).open("ab") as stream:
                    stream.write(f"\nset({LEAF_MARKER} 1)\n".encode())
            except (OSError, ValueError, KeyError, TypeError, StopIteration) as error:
                results[phase] = Sample(error=f"Could not edit a configured leaf. {error}")
                continue
        if log:
            log.unlink(missing_ok=True)
        if replay_evidence:
            (build / REPLAY_RESULT).unlink(missing_ok=True)
        args = configure_args
        if trace_args and phase in (COLD, INCREMENTAL_LEAF):
            trace.unlink(missing_ok=True)
            args = [*configure_args, *trace_args]
        sample = configure_sample(binary, source, build, args, work, generator, env, timeout,
                                  timed=timed)
        if not sample.error and synthetic:
            cache = build / "CMakeCache.txt"
            if "M6_PROBE:INTERNAL=1" not in cache.read_text(errors="replace").splitlines():
                sample.error = "The synthetic compiler probe result is missing."
        if not sample.error and log and phase == COLD:
            cold_inputs = compiler_inputs(log)
        if not sample.error and log and phase == WARM_PROBE:
            warm_inputs = compiler_inputs(log)
            if sample.toolchain == results[COLD].toolchain:
                sample.probe_hits = cache_hits(cold_inputs, warm_inputs)
            if sample.probe_hits and not timed:
                log.unlink(missing_ok=True)
                shutil.rmtree(warm)
                control_env = with_store(env, work / "empty-store")
                control_sample = configure_sample(
                    binary, source, warm, configure_args, work, generator,
                    control_env, timeout, timed=False,
                )
                control_inputs = compiler_inputs(log)
                if (control_sample.error or control_inputs != cold_inputs
                        or control_sample.toolchain != sample.toolchain):
                    sample.probe_hits = 0
        if not sample.error and (trace_args or replay_evidence) and phase == COLD:
            try:
                if replay_evidence:
                    evaluated, replayed = replay_leaves(source, build)
                    if replayed or leaf not in evaluated or control not in evaluated:
                        raise ValueError("The cold run must evaluate the leaf and its control.")
                else:
                    executed, controls, _ = leaf_trace(trace, source, leaf, control)
                    if not (executed and controls):
                        raise ValueError("The cold trace must show the leaf and its control.")
            except (OSError, ValueError) as error:
                sample.error = f"Could not verify the cold configure. {error}"
        if not sample.error and phase == INCREMENTAL_LEAF:
            try:
                sample.edited_leaf = str(leaf)
                active_leaves = shared_leaves(source, build)
                if leaf not in active_leaves:
                    raise ValueError(f"{leaf} is absent from the updated codemodel.")
                if replay_evidence:
                    evaluated, replayed = replay_leaves(source, build)
                    sample.edited_leaf_executed = leaf in evaluated and leaf not in replayed
                    sample.control_executed = control in evaluated
                    control_present = control == Path("CMakeLists.txt") or (
                        control in active_leaves and active_leaves[control].targets == control_targets
                    )
                    sample.replayed_regions = int(control in replayed and control_present)
                elif trace_args:
                    _, controls, marker = leaf_trace(trace, source, leaf, control)
                    sample.edited_leaf_executed = marker
                    sample.control_executed = controls
            except (OSError, ValueError, KeyError, TypeError, StopIteration) as error:
                sample.error = f"Could not verify the leaf configure. {error}"
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


def fetch_runs(binary, fixture, work, generator, env, timeout, *, offline, timed=True):
    """Run a fresh fetch followed by an offline reuse or another fresh fetch."""
    source, origins = fixture
    store = work / "store"
    env = with_store({**trial_env(env, work), "RCMAKE_FETCH_PREFETCH": "1"}, store)
    args = (binary, source)
    cold = configure_sample(*args, work / "cold", [], work, generator, env, timeout, timed=timed)
    if cold.error:
        return {COLD_FETCH: cold, WARM_FETCH: Sample(error="The cold FetchContent configure failed.")}
    if offline and not (entry_count(store, "fetch") and any((store / "git").rglob("HEAD"))):
        return {COLD_FETCH: cold, WARM_FETCH: Sample(unavailable="The archive or Git cache is missing.")}
    with ExitStack() as restore:
        if offline:
            # Require a fresh build to succeed when only the store has the inputs.
            for origin in origins:
                hidden = origin.with_suffix(origin.suffix + ".hidden")
                origin.rename(hidden)
                restore.callback(hidden.rename, origin)
            env = {**env, "RCMAKE_FETCH_OFFLINE": "1"}
        warm = configure_sample(*args, work / "warm", [], work, generator, env, timeout, timed=timed)
    warm.fetch_origins_hidden = offline and not warm.error
    return {COLD_FETCH: cold, WARM_FETCH: warm}


def preflight(binaries, project, cases, work, generator, env, timeout, *, synthetic):
    """Use untimed configure results to establish each scenario's prerequisites."""
    work.mkdir()
    unavailable = {}
    leaf_pair = None
    if WARM_FETCH in cases:
        sample = fetch_runs(binaries["rcmake"], project, work, generator, env, timeout,
                            offline=True, timed=False)[WARM_FETCH]
        if not sample.fetch_origins_hidden:
            unavailable[WARM_FETCH] = sample.unavailable or sample.error or "Offline fetch was not verified."
    if WARM_PROBE in cases:
        lane = work / "probe"
        lane.mkdir()
        sample = configure_runs(binaries["rcmake"], project, lane, generator, env, timeout,
                                synthetic=synthetic, cases=(COLD, WARM_PROBE),
                                timed=False)[WARM_PROBE]
        if not sample.probe_hits:
            unavailable[WARM_PROBE] = sample.error or "No probe cache hits in the untimed configure."
    if INCREMENTAL_LEAF in cases:
        try:
            leaf_pair = select_common_leaf(binaries, project, work, generator, env, timeout)
            for tool, binary in binaries.items():
                lane = work / f"replay-{tool}"
                lane.mkdir()
                sample = configure_runs(binary, project, lane, generator, env, timeout,
                                        synthetic=synthetic, cases=(COLD, INCREMENTAL_LEAF),
                                        leaf_pair=leaf_pair, timed=False,
                                        replay_evidence=tool == "rcmake")[INCREMENTAL_LEAF]
                if sample.error or not sample.edited_leaf_executed:
                    raise ValueError(sample.error or f"{tool} did not execute the leaf edit.")
                if tool == "rcmake" and not sample.replayed_regions:
                    raise ValueError("Rcmake did not replay the unchanged region in the untimed configure.")
                if tool == "oracle" and not sample.control_executed:
                    raise ValueError("CMake did not execute the unchanged control.")
        except (OSError, ValueError, KeyError, TypeError, StopIteration) as error:
            unavailable[INCREMENTAL_LEAF] = str(error)
            leaf_pair = None
    return leaf_pair, unavailable


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
    successes = [sample for sample in samples if not (sample.error or sample.unavailable)]
    times = [sample.ms for sample in successes]
    memory = [sample.peak_rss_bytes for sample in successes]
    quartiles = statistics.quantiles(times, n=4, method="inclusive") if len(times) > 1 else None
    return {
        "median_ms": statistics.median(times) if times else None,
        "p95_ms": sorted(times)[math.ceil(0.95 * len(times)) - 1] if times else None,
        "iqr_ms": quartiles[2] - quartiles[0] if quartiles else (0.0 if times else None),
        "min_ms": min(times, default=None),
        "max_ms": max(times, default=None),
        "peak_rss_bytes": max(memory, default=None),
        "failures": sum(bool(sample.error) for sample in samples),
        "unavailable": sum(bool(sample.unavailable) for sample in samples),
        "samples": [vars(sample) for sample in samples],
    }


def result_row(project, case, by_tool, cold):
    """Compare medians when every sample passed on both tools."""
    row = {"project": project, "case": case, **{
        tool: summarize(by_tool[tool][case]) for tool in TOOLS
    }}
    rcmake, oracle = (row[tool] for tool in TOOLS)
    def ratio(baseline):
        if baseline and not any(item["failures"] or item["unavailable"]
                                for item in (baseline, rcmake)):
            return baseline["median_ms"] / rcmake["median_ms"]
        return None

    row["speedup"] = ratio(oracle)
    baseline = summarize(by_tool["rcmake"][cold]) if cold else None
    row["cold_speedup"] = ratio(baseline)
    if case == WARM_FETCH:
        row["oracle_case"] = "Fresh fetch with visible origins"
        row["rcmake_case"] = "Offline fetch from cache"
        row["cache_speedup"] = row["cold_speedup"]
    return row


def scenario_gate(row):
    """Assess the measured speed and the cache or replay evidence."""
    def status(passed, detail):
        return {"passed": passed, "status": "pass" if passed else "fail", "detail": detail}

    case = row["case"]
    samples = row["rcmake"]["samples"]
    for tool in TOOLS:
        reason = next((sample["unavailable"] for sample in row[tool]["samples"]
                       if sample.get("unavailable")), None)
        if reason:
            return {"passed": False, "status": "unavailable", "detail": reason}
    if row["speedup"] is None:
        return status(False, "A configure failed.")
    if case == WARM_PROBE:
        if not all(sample.get("probe_hits", 0) > 0 for sample in samples):
            return status(False, "A warm run did not skip any cold probe compilations.")
        return status(row["speedup"] >= 5, "Target at least 5× versus fresh CMake. Stretch target 10×.")
    if case == INCREMENTAL_LEAF:
        oracle = row["oracle"]["samples"]
        leaves = {sample.get("edited_leaf") for sample in [*samples, *oracle]}
        if len(leaves) != 1 or leaves & {None, "CMakeLists.txt"}:
            return status(False, "Both tools must edit the same non-root leaf.")
        if not all(sample.get("edited_leaf_executed") for sample in [*samples, *oracle]):
            return status(False, "A configure did not execute the edited leaf.")
        if not all(sample.get("replayed_regions", 0) > 0 for sample in samples):
            return status(False, "A run did not replay the unchanged region.")
        if not all(sample.get("control_executed") for sample in oracle):
            return status(False, "CMake did not execute the unchanged control.")
        return status(row["rcmake"]["p95_ms"] < 100, "Target Rcmake p95 below 100 ms.")
    return status(all(sample.get("fetch_origins_hidden") for sample in samples),
                  "Require an offline configure from the archive and Git caches with both origins hidden.")


def markdown(report):
    """Render timings, memory, and failures as Markdown."""
    meta = report["meta"]
    toolchain_paths = ", ".join(f"{name}={path}" for name, path in meta["toolchain_defaults"].items() if path)
    toolchain_env = ", ".join(f"{key}={value}" for key, value in meta["toolchain_env"].items())
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
        "Untimed runs check Rcmake replay results and trace the CMake leaf edit. "
        "Both tools record compiler calls during probe timings. "
        "The operating system keeps its file cache across runs.",
        "Each tool and trial uses a private home and XDG cache. "
        "The environment sets CCACHE_DISABLE=1 and removes SCCACHE_* and CMAKE_*_COMPILER_LAUNCHER. "
        "Configure disables package registries.",
        "The warm fetch CMake timing is a fresh fetch with visible origins. "
        "The within-Rcmake ratio is the cache speedup.",
        f"Default tool paths. {toolchain_paths or 'None found.'}",
        f"Toolchain environment overrides. {toolchain_env or 'None.'}",
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
            if item["unavailable"] and "gate" not in row:
                reason = next(sample["unavailable"] for sample in item["samples"] if sample["unavailable"])
                details.append(f"- {title}, {row['case']}, {tool}. Unavailable. {' '.join(reason.split())}")
        ratio = f"{row['speedup']:.2f}x" if row["speedup"] is not None else "—"
        cold = f"{row['cold_speedup']:.2f}x" if row["cold_speedup"] is not None else "—"
        gate = "—"
        if "gate" in row:
            gate = row["gate"]["status"].capitalize()
            details.append(f"- {title}, {row['case']}. {' '.join(row['gate']['detail'].split())}")
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

    env = benchmark_env(os.environ)
    generator = "Ninja" if shutil.which("ninja", path=env.get("PATH")) else "Unix Makefiles"
    load = os.getloadavg()
    directories = 4 if args.quick else 64
    probes = 24 if not args.quick and WARM_PROBE in phases else 1
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
        workloads = [(name, project, phases, {}) for name, project in projects.items()]
        if WARM_FETCH in scenarios:
            try:
                if not shutil.which("git"):
                    raise ValueError("The local FetchContent fixture requires Git.")
                fixture = fetch_fixture(scratch / "fetch-fixture", fetch_env, timeout)
                workloads.append(("Local FetchContent", fixture, (COLD_FETCH, WARM_FETCH), {}))
            except (OSError, ValueError) as error:
                workloads.append(("Local FetchContent", None, (COLD_FETCH, WARM_FETCH),
                                  {case: str(error) for case in (COLD_FETCH, WARM_FETCH)}))
        if inputs:
            workloads.append(("Utilities", inputs, UTILITIES, {}))
        for index, (name, project, cases, unavailable) in enumerate(workloads):
            leaf_pair = None
            workload_env = fetch_env if WARM_FETCH in cases else env
            if cases != UTILITIES and not unavailable:
                print(f"Checking prerequisites for {name}.", file=sys.stderr, flush=True)
                leaf_pair, unavailable = preflight(
                    binaries, project, cases, scratch / f"preflight-{index}", generator, workload_env,
                    timeout, synthetic=name == "synthetic",
                )
                if WARM_FETCH in unavailable:
                    unavailable[COLD_FETCH] = unavailable[WARM_FETCH]
            print(f"Measuring {name} ({args.runs} runs per tool).", file=sys.stderr, flush=True)
            by_tool = {tool: {case: [] for case in cases} for tool in TOOLS}
            for trial in range(args.runs):
                order = TOOLS if (trial + index) % 2 == 0 else TOOLS[::-1]
                for tool in order:
                    work = scratch / f"{index}-{trial}-{tool}"
                    work.mkdir()
                    if cases == UTILITIES:
                        samples = utility_runs(binaries[tool], ctests[tool], inputs, work,
                                               trial_env(env, work), timeout)
                    elif WARM_FETCH in cases:
                        if unavailable:
                            samples = {case: Sample(unavailable=unavailable[case]) for case in cases}
                        else:
                            samples = fetch_runs(binaries[tool], project, work, generator, fetch_env,
                                                 timeout, offline=tool == "rcmake")
                    else:
                        samples = configure_runs(binaries[tool], project, work, generator, env,
                                                 timeout, synthetic=name == "synthetic", cases=cases,
                                                 leaf_pair=leaf_pair, unavailable=unavailable)
                        if leaf_pair and not samples[INCREMENTAL_LEAF].error:
                            verify = work / "verify-leaf"
                            verify.mkdir()
                            evidence = configure_runs(
                                binaries[tool], project, verify, generator, env, timeout,
                                synthetic=name == "synthetic", cases=(COLD, INCREMENTAL_LEAF),
                                leaf_pair=leaf_pair, timed=False, replay_evidence=tool == "rcmake",
                            )[INCREMENTAL_LEAF]
                            leaf_sample = samples[INCREMENTAL_LEAF]
                            leaf_sample.error = evidence.error
                            leaf_sample.edited_leaf_executed = evidence.edited_leaf_executed
                            leaf_sample.control_executed = evidence.control_executed
                            leaf_sample.replayed_regions = evidence.replayed_regions
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
            "toolchain_env": {key: env[key] for key in (
                "CC", "CXX", "FC", "OBJC", "OBJCXX", "CUDACXX", "HIPCXX", "CFLAGS", "CXXFLAGS",
                "CPPFLAGS", "LDFLAGS", "SDKROOT", "MACOSX_DEPLOYMENT_TARGET", "DEVELOPER_DIR",
            ) if key in env},
            "toolchain_defaults": {name: shutil.which(name, path=env.get("PATH"))
                                   for name in ("cc", "c++", "ninja", "make")},
            "cache_env": {
                "CCACHE_DISABLE": env["CCACHE_DISABLE"],
                "SCCACHE_*": "unset", "CMAKE_*_COMPILER_LAUNCHER": "unset",
                "HOME": "private per trial", "XDG_CACHE_HOME": "private per trial",
                "RCMAKE_CACHE_DIR": "private per trial", "RCMAKE_PROBE_CACHE_DIR": "private per trial",
                "package_registry_flags": REGISTRY_FLAGS,
            },
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

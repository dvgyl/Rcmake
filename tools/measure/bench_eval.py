#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Compare the latency and peak memory of Rcmake and the pinned CMake 4.5.1 oracle.

The harness runs each benchmark with both tools and reports, for each tool:

* the median and the 95th-percentile (p95) wall-clock time in milliseconds,
* the highest peak resident set size (RSS, the physical memory in use) in MiB.

The speedup is the oracle median divided by the Rcmake median. A value above 1.0
means that Rcmake is faster.

Benchmark categories:

* ``script``: ``cmake -P`` microbenchmarks (variables, lists, loops, regex,
  ``string(JSON)``, ``math``, function calls).
* ``tool``: ``cmake -E`` utilities (``tar`` create and extract, ``sha256sum``,
  ``copy_directory``).
* ``project``: synthetic C projects with 100 and 500 targets, with transitive
  ``target_link_libraries``, ``install``, ``export``, and ``compile_commands.json``.
  Each project runs a cold configure (new build tree) and a warm re-configure.
* ``corpus``: optional (``--include-corpus``) cold configure of the pinned
  zlib, fmt, and nlohmann/json releases from ``conformance/corpus/manifest.toml``.

Example:

    source env/env.sh
    python3 tools/measure/bench_eval.py --runs 5 --json /tmp/bench.json
"""

from __future__ import annotations

import argparse
from dataclasses import dataclass, field
import datetime
import json
import math
import os
from pathlib import Path
import platform
import re
import resource
import shutil
import signal
import statistics
import subprocess
import sys
import tempfile
import threading
import time
from typing import Callable


ROOT = Path(__file__).resolve().parents[2]
ORACLE_VERSION = "4.5.1"
ORACLE_RELATIVE = "env/oracle/cmake-4.5.1-macos-universal/CMake.app/Contents/bin/cmake"
CORPUS_PROJECTS = ("zlib", "fmt", "nlohmann/json")
MIB = 1024 * 1024


def checkout_roots() -> list[Path]:
    """Return this checkout and, for a Git worktree, the main checkout."""
    roots = [ROOT]
    try:
        common = subprocess.run(
            ["git", "rev-parse", "--path-format=absolute", "--git-common-dir"],
            cwd=ROOT, capture_output=True, text=True, timeout=10, check=False,
        ).stdout.strip()
    except (OSError, subprocess.TimeoutExpired):
        common = ""
    if common:
        main = Path(common).parent
        if main != ROOT:
            roots.append(main)
    return roots


def find_rcmake(explicit: str | None) -> Path:
    """Find the staged Rcmake binary."""
    if explicit:
        return Path(explicit).resolve()
    for root in checkout_roots():
        candidate = root / "target/stage/bin/rcmake"
        if candidate.is_file():
            return candidate
    raise SystemExit("Cannot find target/stage/bin/rcmake. Run scripts/stage.sh or use --rcmake.")


def find_oracle(explicit: str | None) -> Path:
    """Find the pinned CMake 4.5.1 binary."""
    candidates = [explicit, os.environ.get("RCMAKE_ORACLE_CMAKE")]
    candidates += [str(root / ORACLE_RELATIVE) for root in checkout_roots()]
    for candidate in candidates:
        if candidate and Path(candidate).is_file():
            return Path(candidate)
    raise SystemExit("Cannot find CMake 4.5.1. Source env/env.sh or use --oracle.")


def tool_version(binary: Path) -> str:
    """Return the first line of `<binary> --version`."""
    try:
        output = subprocess.run(
            [str(binary), "--version"], capture_output=True, text=True, timeout=30, check=False
        ).stdout
    except (OSError, subprocess.TimeoutExpired) as error:
        return f"unknown ({error})"
    return output.splitlines()[0] if output else "unknown"


def build_profile(binary: Path, declared: str) -> str:
    """Return the Cargo profile of the Rcmake binary (`debug`, `release`, or `unknown`)."""
    if declared != "auto":
        return declared
    parts = binary.resolve().parts
    for profile in ("release", "debug"):
        if profile in parts:
            return profile
    # A staged binary is a copy. Compare it with the Cargo outputs next to the stage.
    target = binary.resolve().parent.parent.parent
    size = binary.stat().st_size
    for profile in ("release", "debug"):
        built = target / profile / "rcmake"
        if built.is_file() and built.stat().st_size == size:
            return profile
    return "unknown"


def find_generator(requested: str) -> str:
    """Use Ninja when it is on PATH, otherwise Unix Makefiles."""
    if requested != "auto":
        return requested
    return "Ninja" if shutil.which("ninja") else "Unix Makefiles"


@dataclass
class Sample:
    """One timed run of one command."""

    seconds: float
    rss_bytes: int | None
    returncode: int | None
    error: str | None = None


TIME_RSS_RE = {
    "Darwin": (re.compile(r"^\s*(\d+)\s+maximum resident set size", re.MULTILINE), 1),
    "Linux": (re.compile(r"Maximum resident set size \(kbytes\):\s*(\d+)"), 1024),
}


def rss_method(requested: str) -> str:
    """Choose `time` (`/usr/bin/time -l` or `-v`) or `wait4` (`getrusage` data)."""
    if requested != "auto":
        return requested
    if platform.system() in TIME_RSS_RE and os.access("/usr/bin/time", os.X_OK):
        return "time"
    return "wait4"


def measure(command: list[str], cwd: Path, env: dict[str, str], timeout: float,
            method: str, log: Path) -> Sample:
    """Run `command` once and return its wall-clock time and peak RSS.

    The `time` method wraps the command in `/usr/bin/time`. The `wait4` method reads
    the child's resource usage (`getrusage` data) when the process ends. The
    process group is killed when the timeout expires.
    """
    system = platform.system()
    wrapped = command
    if method == "time":
        wrapped = ["/usr/bin/time", "-l" if system == "Darwin" else "-v", *command]
    timed_out = threading.Event()
    with log.open("wb") as output:
        start = time.perf_counter()
        try:
            process = subprocess.Popen(
                wrapped, cwd=cwd, env=env, stdin=subprocess.DEVNULL,
                stdout=output, stderr=output, start_new_session=True,
            )
        except OSError as error:
            return Sample(0.0, None, None, str(error))

        def kill() -> None:
            timed_out.set()
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass

        timer = threading.Timer(timeout, kill)
        timer.start()
        try:
            _, status, usage = os.wait4(process.pid, 0)
        finally:
            timer.cancel()
        seconds = time.perf_counter() - start
        process.returncode = os.waitstatus_to_exitcode(status)
    text = log.read_bytes().decode("utf-8", "replace")
    rss: int | None = None
    if method == "time":
        pattern, unit = TIME_RSS_RE[system]
        matches = pattern.findall(text)
        if matches:
            rss = int(matches[-1]) * unit
    else:
        # ru_maxrss is in bytes on macOS and in KiB on Linux.
        rss = usage.ru_maxrss * (1 if system == "Darwin" else 1024)
    error = None
    if timed_out.is_set():
        error = f"timed out after {timeout:.0f} s"
    elif process.returncode != 0:
        tail = "\n".join(text.strip().splitlines()[-8:])
        error = f"exit code {process.returncode}: {tail}"
    return Sample(seconds, rss, process.returncode, error)


def percentile(values: list[float], fraction: float) -> float:
    """Nearest-rank percentile. With 3 runs, p95 is the slowest run."""
    ordered = sorted(values)
    rank = max(1, math.ceil(fraction * len(ordered)))
    return ordered[rank - 1]


@dataclass
class Context:
    """Shared state for one tool: binary, scratch root, generator, environment."""

    name: str
    binary: Path
    scratch: Path
    generator: str
    env: dict[str, str]


@dataclass
class Bench:
    """One benchmark.

    `command` returns the argument list and working directory for a tool.
    `setup` runs once per tool before the timed runs (not timed).
    `prepare` runs before each timed run (not timed).
    """

    name: str
    category: str
    command: Callable[[Context], tuple[list[str], Path]]
    setup: Callable[[Context], None] | None = None
    prepare: Callable[[Context], None] | None = None
    notes: str = ""


SCRIPTS: dict[str, str] = {
    "var-expansion": """
set(a alpha)
set(b beta)
set(c gamma)
set(name b)
foreach(i RANGE 20000)
  set(x "${a}-${${name}}-${c}-${i}")
  set(y "${x}${x}")
endforeach()
""",
    "lists": """
set(l "")
foreach(i RANGE 4000)
  math(EXPR j "(${i} * 7919) % 4001")
  list(APPEND l "item${j}")
endforeach()
list(SORT l)
list(REMOVE_DUPLICATES l)
list(REVERSE l)
foreach(i RANGE 0 3000 7)
  list(FIND l "item${i}" pos)
  list(GET l 0 1 2 first)
endforeach()
list(TRANSFORM l TOUPPER)
list(JOIN l "," joined)
string(LENGTH "${joined}" n)
""",
    "foreach-while": """
set(n 0)
foreach(i RANGE 40000)
  set(last ${i})
endforeach()
foreach(item IN ITEMS a b c d e f g h i j)
  foreach(i RANGE 1000)
    set(x "${item}${i}")
  endforeach()
endforeach()
set(i 0)
while(i LESS 10000)
  math(EXPR i "${i} + 1")
endwhile()
""",
    "regex": """
set(text "")
foreach(i RANGE 200)
  string(APPEND text "key${i} = value_${i}; path/to/file${i}.c;")
endforeach()
foreach(i RANGE 150)
  string(REGEX MATCHALL "key[0-9]+" keys "${text}")
  string(REGEX REPLACE "value_([0-9]+)" "V\\\\1" replaced "${text}")
  if(text MATCHES "file(1[0-9][0-9])\\\\.c")
    set(hit "${CMAKE_MATCH_1}")
  endif()
endforeach()
""",
    "string-json": """
set(doc "{}")
foreach(i RANGE 300)
  string(JSON doc SET "${doc}" "key${i}" "{\\"index\\": ${i}, \\"tags\\": [\\"a\\", \\"b\\"]}")
endforeach()
foreach(i RANGE 300)
  string(JSON value GET "${doc}" "key${i}" "index")
  string(JSON tag GET "${doc}" "key${i}" "tags" 1)
endforeach()
string(JSON len LENGTH "${doc}")
""",
    "math": """
set(acc 1)
foreach(i RANGE 30000)
  math(EXPR acc "((${acc} * 31 + ${i}) ^ (${i} << 3)) % 1000003")
endforeach()
math(EXPR hex "${acc}" OUTPUT_FORMAT HEXADECIMAL)
""",
    "function-calls": """
function(add_one out value)
  math(EXPR result "${value} + 1")
  set(${out} "${result}" PARENT_SCOPE)
endfunction()
macro(twice out value)
  add_one(${out} "${value}")
  add_one(${out} "${${out}}")
endmacro()
set(v 0)
foreach(i RANGE 6000)
  twice(v "${v}")
endforeach()
""",
}


def script_benches() -> list[Bench]:
    """Create one `-P` benchmark per entry in SCRIPTS."""

    def make(name: str, body: str) -> Bench:
        def setup(cx: Context) -> None:
            (cx.scratch / "scripts").mkdir(exist_ok=True)
            (cx.scratch / "scripts" / f"{name}.cmake").write_text(body.lstrip(), encoding="utf-8")

        def command(cx: Context) -> tuple[list[str], Path]:
            return [str(cx.binary), "-P", f"{name}.cmake"], cx.scratch / "scripts"

        return Bench(f"-P {name}", "script", command, setup=setup)

    return [make(name, body) for name, body in SCRIPTS.items()]


DATASET_FILES = 2000
DATASET_FILE_BYTES = 4096
HASH_FILE_BYTES = 64 * MIB


def make_dataset(base: Path) -> None:
    """Create the shared `-E` input data once (deterministic content)."""
    data = base / "dataset"
    if data.is_dir():
        return
    for index in range(DATASET_FILES):
        folder = data / f"d{index % 40:02d}" / f"s{index % 7}"
        folder.mkdir(parents=True, exist_ok=True)
        seed = index.to_bytes(4, "little")
        (folder / f"f{index:05d}.txt").write_bytes((seed * (DATASET_FILE_BYTES // 4))[:DATASET_FILE_BYTES])
    block = bytes(range(256)) * 4096
    with (base / "hash.bin").open("wb") as output:
        for _ in range(HASH_FILE_BYTES // len(block)):
            output.write(block)


def tool_benches(shared: Path) -> list[Bench]:
    """Create the `-E` benchmarks. All tools read the same dataset."""
    archive_names = {"tar": "data.tar", "tgz": "data.tar.gz"}

    def remove(path: Path) -> None:
        shutil.rmtree(path, ignore_errors=True)
        if path.is_file():
            path.unlink()

    def create(kind: str, flags: str) -> Bench:
        def prepare(cx: Context) -> None:
            (cx.scratch / archive_names[kind]).unlink(missing_ok=True)

        def command(cx: Context) -> tuple[list[str], Path]:
            archive = cx.scratch / archive_names[kind]
            return [str(cx.binary), "-E", "tar", flags, str(archive), "dataset"], shared

        return Bench(f"-E tar {flags} ({kind})", "tool", command, prepare=prepare,
                     notes=f"{DATASET_FILES} files x {DATASET_FILE_BYTES} B")

    def extract(kind: str) -> Bench:
        def setup(cx: Context) -> None:
            flags = "cf" if kind == "tar" else "czf"
            archive = cx.scratch / f"extract-{archive_names[kind]}"
            subprocess.run([str(cx.binary), "-E", "tar", flags, str(archive), "dataset"],
                           cwd=shared, env=cx.env, check=True, capture_output=True, timeout=600)

        def prepare(cx: Context) -> None:
            dest = cx.scratch / f"extract-{kind}"
            remove(dest)
            dest.mkdir()

        def command(cx: Context) -> tuple[list[str], Path]:
            archive = cx.scratch / f"extract-{archive_names[kind]}"
            return [str(cx.binary), "-E", "tar", "xf", str(archive)], cx.scratch / f"extract-{kind}"

        return Bench(f"-E tar xf ({kind})", "tool", command, setup=setup, prepare=prepare)

    def sha256(cx: Context) -> tuple[list[str], Path]:
        return [str(cx.binary), "-E", "sha256sum", "hash.bin"], shared

    def copy_prepare(cx: Context) -> None:
        remove(cx.scratch / "copy")

    def copy(cx: Context) -> tuple[list[str], Path]:
        return [str(cx.binary), "-E", "copy_directory", "dataset", str(cx.scratch / "copy")], shared

    return [
        create("tar", "cf"),
        create("tgz", "czf"),
        extract("tar"),
        extract("tgz"),
        Bench("-E sha256sum (64 MiB)", "tool", sha256),
        Bench("-E copy_directory", "tool", copy, prepare=copy_prepare,
              notes=f"{DATASET_FILES} files"),
    ]


def write_synthetic_project(source: Path, targets: int) -> None:
    """Write a C project with `targets` libraries and transitive link edges.

    Library `i` links `i - 1` (PUBLIC) and `i // 2` (PRIVATE), so the usage
    requirements are transitive and the link graph is deep. Every tenth library
    also has an executable. All libraries are installed and exported.
    """
    if (source / "CMakeLists.txt").is_file():
        return
    (source / "src").mkdir(parents=True, exist_ok=True)
    (source / "include").mkdir(exist_ok=True)
    lines = [
        "cmake_minimum_required(VERSION 3.20)",
        f"project(synth{targets} VERSION 1.0 LANGUAGES C)",
        "set(CMAKE_EXPORT_COMPILE_COMMANDS ON)",
        "include(GNUInstallDirs)",
    ]
    for index in range(targets):
        (source / "include" / f"lib{index}.h").write_text(
            f"int lib{index}(int);\n", encoding="utf-8")
        calls = f"lib{index - 1}(v) + " if index else ""
        (source / "src" / f"lib{index}.c").write_text(
            f'#include "lib{index}.h"\n'
            + (f'#include "lib{index - 1}.h"\n' if index else "")
            + f"int lib{index}(int v) {{ return {calls}v + {index}; }}\n",
            encoding="utf-8",
        )
        lines += [
            f"add_library(lib{index} STATIC src/lib{index}.c)",
            f"target_include_directories(lib{index} PUBLIC "
            f"$<BUILD_INTERFACE:${{CMAKE_CURRENT_SOURCE_DIR}}/include> "
            f"$<INSTALL_INTERFACE:${{CMAKE_INSTALL_INCLUDEDIR}}>)",
            f"target_compile_definitions(lib{index} PUBLIC LIB{index}=1 "
            f"PRIVATE $<$<CONFIG:Debug>:LIB{index}_DEBUG>)",
        ]
        if index:
            lines.append(f"target_link_libraries(lib{index} PUBLIC lib{index - 1} "
                         f"PRIVATE lib{index // 2})")
        if index % 10 == 9:
            (source / "src" / f"app{index}.c").write_text(
                f'#include "lib{index}.h"\nint main(void) {{ return lib{index}(0) == 0; }}\n',
                encoding="utf-8",
            )
            lines += [
                f"add_executable(app{index} src/app{index}.c)",
                f"target_link_libraries(app{index} PRIVATE lib{index})",
            ]
    libraries = " ".join(f"lib{index}" for index in range(targets))
    lines += [
        f"install(TARGETS {libraries} EXPORT synthTargets",
        "  ARCHIVE DESTINATION ${CMAKE_INSTALL_LIBDIR}",
        "  INCLUDES DESTINATION ${CMAKE_INSTALL_INCLUDEDIR})",
        "install(DIRECTORY include/ DESTINATION ${CMAKE_INSTALL_INCLUDEDIR})",
        "install(EXPORT synthTargets NAMESPACE synth:: "
        "DESTINATION ${CMAKE_INSTALL_LIBDIR}/cmake/synth)",
        "export(EXPORT synthTargets NAMESPACE synth:: FILE synthTargets.cmake)",
    ]
    (source / "CMakeLists.txt").write_text("\n".join(lines) + "\n", encoding="utf-8")


def configure_command(cx: Context, source: Path, build: Path, extra: list[str]) -> list[str]:
    return [str(cx.binary), "-S", str(source), "-B", str(build), "-G", cx.generator,
            "-DCMAKE_BUILD_TYPE=Debug", *extra]


def project_benches(shared: Path, sizes: list[int]) -> list[Bench]:
    """Create cold-configure and warm re-configure benchmarks for each project size."""
    benches = []
    for targets in sizes:
        source = shared / f"synth{targets}"
        write_synthetic_project(source, targets)

        def cold_prepare(cx: Context, targets: int = targets) -> None:
            shutil.rmtree(cx.scratch / f"cold{targets}", ignore_errors=True)

        def cold(cx: Context, targets: int = targets, source: Path = source) -> tuple[list[str], Path]:
            return configure_command(cx, source, cx.scratch / f"cold{targets}", []), cx.scratch

        def warm_setup(cx: Context, targets: int = targets, source: Path = source) -> None:
            build = cx.scratch / f"warm{targets}"
            shutil.rmtree(build, ignore_errors=True)
            subprocess.run(configure_command(cx, source, build, []), cwd=cx.scratch,
                           env=cx.env, check=True, capture_output=True, timeout=1800)

        def warm(cx: Context, targets: int = targets, source: Path = source) -> tuple[list[str], Path]:
            return configure_command(cx, source, cx.scratch / f"warm{targets}", []), cx.scratch

        benches += [
            Bench(f"configure {targets} targets (cold)", "project", cold, prepare=cold_prepare,
                  notes="new build tree, includes compiler checks"),
            Bench(f"configure {targets} targets (warm)", "project", warm, setup=warm_setup,
                  notes="re-configure an existing build tree"),
        ]
    return benches


def corpus_benches(shared: Path) -> list[Bench]:
    """Create cold-configure benchmarks for the pinned Tier-1 corpus releases.

    The archives come from the corpus cache (`env/corpus/cache`, also the main
    checkout's cache for a worktree). A missing archive is downloaded and its
    SHA-256 digest is checked by `corpus_eval.cached_archive`.
    """
    sys.path.insert(0, str(Path(__file__).resolve().parent))
    import corpus_eval  # noqa: PLC0415  (local tool module)
    import tomllib  # noqa: PLC0415

    for root in checkout_roots():
        if (root / "env/corpus/cache").is_dir():
            corpus_eval.CACHE = root / "env/corpus/cache"
            break
    manifest = tomllib.loads(corpus_eval.MANIFEST.read_text(encoding="utf-8"))
    projects = {project["name"]: project for project in manifest["project"]}
    benches = []
    for name in CORPUS_PROJECTS:
        project = projects.get(name)
        if project is None:
            continue
        slug = name.replace("/", "-")
        try:
            source = corpus_eval.extract(corpus_eval.cached_archive(project), shared / f"corpus-{slug}")
        except (OSError, ValueError) as error:
            print(f"Skip corpus project {name}: {error}", file=sys.stderr)
            continue
        args = [arg for arg in project.get("configure_args", []) if not arg.startswith("-DCMAKE_BUILD_TYPE")]
        dependency = project.get("test_dependency")
        if dependency and dependency.get("configure_variable"):
            try:
                data = corpus_eval.extract(corpus_eval.cached_archive(dependency),
                                           shared / f"corpus-{slug}-dependency")
                args.append(f"-D{dependency['configure_variable']}={data}")
            except (OSError, ValueError) as error:
                print(f"Corpus project {name}: no test data ({error})", file=sys.stderr)

        def prepare(cx: Context, slug: str = slug) -> None:
            shutil.rmtree(cx.scratch / f"corpus-{slug}", ignore_errors=True)

        def command(cx: Context, slug: str = slug, source: Path = source,
                    args: list[str] = args) -> tuple[list[str], Path]:
            return configure_command(cx, source, cx.scratch / f"corpus-{slug}", args), cx.scratch

        benches.append(Bench(f"corpus {name} {project.get('version', '')} (cold)", "corpus",
                             command, prepare=prepare))
    return benches


@dataclass
class Result:
    """The timed runs of one benchmark for one tool."""

    samples: list[Sample] = field(default_factory=list)
    error: str | None = None

    def summary(self) -> dict:
        times = [s.seconds * 1000 for s in self.samples]
        rss = [s.rss_bytes for s in self.samples if s.rss_bytes is not None]
        return {
            "ok": self.error is None and bool(times),
            "error": self.error,
            "runs": len(times),
            "median_ms": round(statistics.median(times), 2) if times else None,
            "p95_ms": round(percentile(times, 0.95), 2) if times else None,
            "min_ms": round(min(times), 2) if times else None,
            "peak_rss_mib": round(max(rss) / MIB, 1) if rss else None,
            "samples_ms": [round(t, 2) for t in times],
        }


def run_bench(bench: Bench, cx: Context, runs: int, warmup: int, timeout: float,
              method: str) -> Result:
    """Run the setup, the warm-up runs, and the timed runs of one benchmark."""
    result = Result()
    log = cx.scratch / "last-run.log"
    try:
        if bench.setup:
            bench.setup(cx)
    except (OSError, subprocess.SubprocessError) as error:
        detail = getattr(error, "stderr", b"") or b""
        result.error = f"setup failed: {error} {detail.decode('utf-8', 'replace')[-400:]}".strip()
        return result
    for index in range(warmup + runs):
        if bench.prepare:
            bench.prepare(cx)
        command, cwd = bench.command(cx)
        sample = measure(command, cwd, cx.env, timeout, method, log)
        if sample.error:
            result.error = sample.error
            return result
        if index >= warmup:
            result.samples.append(sample)
    return result


def fmt_ms(summary: dict) -> str:
    if not summary["ok"]:
        return "FAIL"
    return f"{summary['median_ms']:.1f} ({summary['p95_ms']:.1f})"


def fmt_rss(summary: dict) -> str:
    value = summary.get("peak_rss_mib")
    return "-" if value is None else f"{value:.1f}"


def speedup(oracle: dict, rcmake: dict) -> float | None:
    if oracle["ok"] and rcmake["ok"] and rcmake["median_ms"]:
        return oracle["median_ms"] / rcmake["median_ms"]
    return None


def markdown_table(rows: list[dict], meta: dict) -> str:
    """Render the result rows and the per-category geometric means as Markdown."""
    lines = [
        f"## Rcmake vs CMake {ORACLE_VERSION} benchmark",
        "",
        f"- Rcmake: `{meta['rcmake']['version']}`, profile **{meta['rcmake']['profile']}**",
        f"- Oracle: `{meta['oracle']['version']}`",
        f"- Host: {meta['host']['system']} {meta['host']['machine']}, "
        f"{meta['host']['cpus']} CPUs; generator {meta['generator']}; "
        f"{meta['runs']} runs + {meta['warmup']} warm-up; RSS via {meta['rss_method']}",
        "- Times are median (p95) in ms. Speedup = cmake median / rcmake median "
        "(above 1.0 means Rcmake is faster).",
        "",
        "| Benchmark | cmake 4.5.1 ms | rcmake ms | Speedup | cmake RSS MiB | rcmake RSS MiB |",
        "|---|---:|---:|---:|---:|---:|",
    ]
    for row in rows:
        ratio = row["speedup"]
        lines.append(
            f"| {row['name']} | {fmt_ms(row['oracle'])} | {fmt_ms(row['rcmake'])} | "
            f"{'-' if ratio is None else f'{ratio:.2f}x'} | {fmt_rss(row['oracle'])} | "
            f"{fmt_rss(row['rcmake'])} |"
        )
    lines += ["", "| Category | Benchmarks | Geometric mean speedup |", "|---|---:|---:|"]
    for category, value in meta["geomean_speedup"].items():
        count = sum(1 for row in rows if row["category"] == category and row["speedup"])
        lines.append(f"| {category} | {count} | {'-' if value is None else f'{value:.2f}x'} |")
    failures = [row for row in rows if not (row["oracle"]["ok"] and row["rcmake"]["ok"])]
    if failures:
        lines += ["", "### Failures", ""]
        for row in failures:
            for tool in ("oracle", "rcmake"):
                if row[tool]["error"]:
                    detail = row[tool]["error"].replace("\n", " ")[:300]
                    lines.append(f"- {row['name']} ({tool}): {detail}")
    return "\n".join(lines) + "\n"


def geomean(values: list[float]) -> float | None:
    return math.exp(sum(math.log(v) for v in values) / len(values)) if values else None


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--rcmake", help="Rcmake binary (default: target/stage/bin/rcmake)")
    parser.add_argument("--oracle", help="CMake 4.5.1 binary (default: $RCMAKE_ORACLE_CMAKE)")
    parser.add_argument("--profile", default="auto",
                        help="Cargo profile of the Rcmake binary (default: detect)")
    parser.add_argument("--runs", type=int, default=5, help="timed runs per benchmark")
    parser.add_argument("--warmup", type=int, default=1, help="untimed runs per benchmark")
    parser.add_argument("--timeout", type=float, default=900, help="seconds per run")
    parser.add_argument("--generator", default="auto", help="CMake generator (default: Ninja)")
    parser.add_argument("--sizes", default="100,500", help="synthetic project target counts")
    parser.add_argument("--category", action="append",
                        choices=["script", "tool", "project", "corpus"],
                        help="run only these categories (repeatable)")
    parser.add_argument("--filter", help="run only benchmarks whose name matches this regex")
    parser.add_argument("--include-corpus", action="store_true",
                        help="also configure the pinned zlib, fmt, and nlohmann/json releases")
    parser.add_argument("--rss-method", default="auto", choices=["auto", "time", "wait4"],
                        help="peak RSS source: /usr/bin/time or wait4 resource usage")
    parser.add_argument("--json", type=Path, help="write the results as JSON to this file")
    parser.add_argument("--markdown", type=Path, help="write the Markdown table to this file")
    parser.add_argument("--keep", action="store_true", help="keep the scratch directory")
    args = parser.parse_args()
    if args.runs < 1 or args.warmup < 0:
        parser.error("--runs must be at least 1 and --warmup at least 0")

    rcmake = find_rcmake(args.rcmake)
    oracle = find_oracle(args.oracle)
    generator = find_generator(args.generator)
    method = rss_method(args.rss_method)
    sizes = [int(size) for size in args.sizes.split(",") if size.strip()]
    categories = set(args.category or ["script", "tool", "project"])
    if args.include_corpus:
        categories.add("corpus")
    pattern = re.compile(args.filter) if args.filter else None

    env = {key: value for key, value in os.environ.items()
           if not key.startswith(("CMAKE_", "CTEST_", "CPACK_")) and key not in {"MAKEFLAGS", "PWD"}}
    env["COPYFILE_DISABLE"] = "1"
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))

    scratch = Path(tempfile.mkdtemp(prefix="rcmake-bench-", dir="/tmp"))
    shared = scratch / "shared"
    shared.mkdir()
    started = datetime.datetime.now(datetime.timezone.utc)
    try:
        benches: list[Bench] = []
        if "script" in categories:
            benches += script_benches()
        if "tool" in categories:
            make_dataset(shared)
            benches += tool_benches(shared)
        if "project" in categories:
            benches += project_benches(shared, sizes)
        if "corpus" in categories:
            benches += corpus_benches(shared)
        if pattern:
            benches = [bench for bench in benches if pattern.search(bench.name)]

        contexts = {}
        for name, binary in (("oracle", oracle), ("rcmake", rcmake)):
            (scratch / name).mkdir()
            contexts[name] = Context(name, binary, scratch / name, generator, env)

        rows = []
        for bench in benches:
            row = {"name": bench.name, "category": bench.category, "notes": bench.notes}
            # Alternate the tool order to spread any cache or thermal effects.
            order = ("oracle", "rcmake") if len(rows) % 2 == 0 else ("rcmake", "oracle")
            for tool in order:
                result = run_bench(bench, contexts[tool], args.runs, args.warmup,
                                   args.timeout, method)
                row[tool] = result.summary()
            row["speedup"] = speedup(row["oracle"], row["rcmake"])
            rows.append(row)
            ratio = row["speedup"]
            print(f"{bench.name:<38} cmake {fmt_ms(row['oracle']):>20}  "
                  f"rcmake {fmt_ms(row['rcmake']):>20}  "
                  f"{'-' if ratio is None else f'{ratio:.2f}x':>7}", flush=True)

        meta = {
            "schema": "rcmake-bench-v1",
            "started": started.isoformat(timespec="seconds"),
            "finished": datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds"),
            "runs": args.runs,
            "warmup": args.warmup,
            "generator": generator,
            "rss_method": method,
            "host": {"system": platform.system(), "release": platform.release(),
                     "machine": platform.machine(), "cpus": os.cpu_count()},
            "rcmake": {"path": str(rcmake), "version": tool_version(rcmake),
                       "profile": build_profile(rcmake, args.profile)},
            "oracle": {"path": str(oracle), "version": tool_version(oracle)},
            "geomean_speedup": {},
        }
        for category in ("script", "tool", "project", "corpus"):
            values = [row["speedup"] for row in rows
                      if row["category"] == category and row["speedup"]]
            if any(row["category"] == category for row in rows):
                meta["geomean_speedup"][category] = geomean(values)
        report = markdown_table(rows, meta)
        print()
        print(report)
        if args.markdown:
            args.markdown.write_text(report, encoding="utf-8")
        if args.json:
            args.json.write_text(json.dumps({"meta": meta, "results": rows}, indent=2) + "\n",
                                 encoding="utf-8")
            print(f"Wrote {args.json}")
        failed = any(not (row["oracle"]["ok"] and row["rcmake"]["ok"]) for row in rows)
        return 1 if failed else 0
    finally:
        if args.keep:
            print(f"Scratch directory: {scratch}")
        else:
            shutil.rmtree(scratch, ignore_errors=True)


if __name__ == "__main__":
    raise SystemExit(main())

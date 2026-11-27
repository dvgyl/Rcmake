#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Check configure output across probe worker counts and CLI modes.

Source env/env.sh before use. Use --rcmake for the Ninja determinism gate.
Its default fixture has two C/C++ targets and emits a warning. Pass project
directories to check other inputs. Use --rcmake-bin for the generator suite.
"""

from __future__ import annotations

import argparse
from collections import ChainMap, Counter, defaultdict
from decimal import Decimal
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile

try:
    from .mutation_eval import API, QUERIES, diff_bytes
    from .ninja_canon import ASSIGNMENT, MAX_INCLUDE_DEPTH, Normalizer, expand, statements, tokens
    from .oracle_prop_test import executable
except ImportError:
    from mutation_eval import API, QUERIES, diff_bytes
    from ninja_canon import ASSIGNMENT, MAX_INCLUDE_DEPTH, Normalizer, expand, statements, tokens
    from oracle_prop_test import executable


ROOT = Path(__file__).resolve().parents[2]
GENERATORS = ("Ninja", "Ninja Multi-Config", "Unix Makefiles", "Xcode")
ARTIFACTS = ("CMakeCache.txt", "compile_commands.json")
NATIVE_FLAGS = ("--diagnostics=rich", "--profile")
SCRATCH_PATH = re.compile(
    r"(?P<prefix>(?:<build>/|(?<![\w./-]))CMakeFiles/CMakeScratch/TryCompile-)"
    r"(?:[0-9a-f]+-(?P<index>[0-9a-f]+)|[a-zA-Z0-9]{6})(?=$|[/\\\s'\":;,|)\]])"
)
PHASE_TIME = re.compile(rb"(?m)^(-- (?:Configuring|Generating) done) \(\d+(?:\.\d+)?s\)(?=\r?$)")
SPECULATIVE = re.compile(r"Spec-[0-9a-f]{16}-[0-9a-f]+")
FIXTURE = {
    "CMakeLists.txt": """\
cmake_minimum_required(VERSION 4.5)
project(M6Determinism LANGUAGES C CXX)
try_compile(M6_C_PROBE SOURCE_FROM_CONTENT probe.c "int main(void) { return 0; }")
try_compile(M6_CXX_PROBE SOURCE_FROM_CONTENT probe.cpp "int main() { return 0; }")
try_compile(M6_OTHER_PROBE SOURCE_FROM_CONTENT other.c "int main(void) { return 1; }")
try_compile(M6_LAST_PROBE SOURCE_FROM_CONTENT last.cpp "int main() { return 1; }")
if(NOT M6_C_PROBE OR NOT M6_CXX_PROBE OR NOT M6_OTHER_PROBE OR NOT M6_LAST_PROBE)
  message(FATAL_ERROR "A compiler probe for the determinism test failed.")
endif()
add_library(c_part STATIC part.c)
add_executable(cxx_app main.cpp)
target_link_libraries(cxx_app PRIVATE c_part)
message(WARNING "Determinism diagnostic sample.")
""",
    "part.c": "int part(void) { return 0; }\n",
    "main.cpp": 'extern "C" int part(void);\nint main() { return part(); }\n',
}


GENERATOR_FIXTURE = {
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


def run(command: list, cwd: Path, env: dict[str, str], timeout: int) -> tuple[bytes, bytes]:
    """Capture bytes from a command. Stop its process group on timeout."""
    with subprocess.Popen(
        command, cwd=cwd, env=env, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
        stderr=subprocess.PIPE, start_new_session=True,
    ) as process:
        try:
            stdout, stderr = process.communicate(timeout=timeout)
        except (subprocess.TimeoutExpired, KeyboardInterrupt):
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.communicate(timeout=5)
            raise
    if process.returncode:
        message = (stderr or stdout)[-4000:].decode(errors="replace").strip()
        raise ValueError(f"{command[0]} exited with status {process.returncode}.\n{message}")
    return stdout, stderr


def ninja_paths(paths: Normalizer) -> Normalizer:
    """Match known paths in Ninja syntax, including escaped path characters."""
    def escape(path: str) -> str:
        return path.replace("$", "$$").replace(" ", "$ ").replace(":", "$:")

    for path, label in list(paths.labels.items()):
        paths.labels[escape(path)] = label
        paths.labels[json.dumps(path, ensure_ascii=False)[1:-1]] = label
    alternatives = "|".join(map(re.escape, sorted(paths.labels, key=len, reverse=True)))
    paths.pattern = re.compile(
        rf"(?:(?<![\w./-])|(?<=-[BSILF]))(?:{alternatives})"
        r"(?=$|[/\s'\"\x60:;,|)&\]]|\$[ :]|\$\r?\n)"
    )
    return paths


def normalized(data: bytes, paths: Normalizer) -> bytes:
    """Replace known paths and volatile IDs in generated scratch paths."""
    value = paths.text(data.decode(errors="surrogateescape"))
    value = SCRATCH_PATH.sub(lambda match: match["prefix"] + "<scratch>"
                             + (match["index"] or ""), value)
    return value.encode(errors="surrogateescape")


def ninja_files(build: Path, paths: Normalizer) -> dict[str, bytes]:
    """Read the manifest and all resolved include and subninja files."""
    files = {}

    def read(path: Path, scope: ChainMap, active: tuple[Path, ...]) -> None:
        path = path.resolve()
        if path in active or len(active) >= MAX_INCLUDE_DEPTH:
            raise ValueError(f"Invalid Ninja include chain at {path}.")
        name = str(path.relative_to(build)) if path.is_relative_to(build) else paths.text(str(path))
        files[name] = normalized(path.read_bytes(), paths)

        def lookup(key):
            return scope.get(key, "")

        for statement, _ in statements(path):
            keyword, _, argument = statement.partition(" ")
            if keyword in {"include", "subninja"}:
                arguments = tokens(argument.lstrip(" "))
                if len(arguments) != 1:
                    raise ValueError(f"Expected one Ninja include path in {path}.")
                included = expand(arguments[0], lookup)
                if not included:
                    raise ValueError(f"Empty Ninja include path in {path}.")
                read(build / included, scope if keyword == "include" else scope.new_child(),
                     (*active, path))
            elif match := ASSIGNMENT.fullmatch(statement):
                scope[match[1]] = expand(match[2], lookup)

    read(build / "build.ninja", ChainMap(), ())
    return files


def file_api(build: Path, paths: Normalizer) -> dict[str, bytes]:
    """Check reply references and replace generated names in the raw JSON."""
    reply = build / API / "reply"
    raw = {path.name: normalized(path.read_bytes(), paths) for path in reply.glob("*.json")}
    parsed = {name: json.loads(data) for name, data in raw.items()}
    indices = [name for name in raw if name.startswith("index-")]
    if len(indices) != 1:
        raise ValueError(f"Expected one File API index in {reply}.")
    queries = parsed[indices[0]]["reply"]
    if any(queries.get(query, {}).get("jsonFile") not in raw for query in QUERIES):
        raise ValueError(f"Missing File API query replies in {reply}.")

    names = {
        name: "index.json" if name in indices else re.sub(r"-[0-9a-f]{20}(?=\.json$)", "", name)
        for name in raw
    }

    def replace_refs(data: bytes, labels: dict[str, str]) -> bytes:
        def replace(match: re.Match[bytes]) -> bytes:
            name = json.loads(match[2])
            if name not in labels:
                raise ValueError(f"Missing File API reply {name} in {reply}.")
            return match[1] + json.dumps(labels[name]).encode()

        return re.sub(rb'("jsonFile"\s*:\s*)("(?:[^"\\]|\\.)*")', replace, data)

    counts = Counter(names.values())
    labels = names.copy()
    for name, stem in names.items():
        if counts[stem] > 1:
            digest = hashlib.sha256(replace_refs(raw[name], names)).hexdigest()
            labels[name] = f"{stem.removesuffix('.json')}-{digest}.json"
    if len(set(labels.values())) != len(labels):
        raise ValueError(f"Ambiguous File API reply names in {reply}.")
    return {
        f"{API}/reply/{labels[name]}": replace_refs(data, labels)
        for name, data in raw.items()
    }


def snapshot(build: Path, tool_paths: dict[Path, str]) -> dict[str, bytes]:
    """Read generated build files, including the full Ninja manifest."""
    paths = ninja_paths(Normalizer(build, replacements=tool_paths))
    files = {name: normalized((build / name).read_bytes(), paths) for name in ARTIFACTS}
    manifests = ninja_files(build.resolve(), paths)
    if not manifests["build.ninja"].strip():
        raise ValueError(f"Empty Ninja manifest in {build}.")
    if any(not data.strip() for data in files.values()):
        raise ValueError(f"Empty generated artifact in {build}.")
    if not json.loads(files["compile_commands.json"]):
        raise ValueError(f"No compile commands in {build}.")
    return {**manifests, **files, **file_api(build, paths)}


def compare(expected: dict[str, bytes], actual: dict[str, bytes], label: str) -> None:
    """Compare each file and report the first difference."""
    for name in sorted(expected.keys() | actual.keys()):
        diff = diff_bytes(expected.get(name), actual.get(name), f"baseline/{name}", f"{label}/{name}")
        if diff:
            raise ValueError(f"{label} differs in {name}.\n{diff[:4000].decode(errors='replace')}")


def reject_constant(value: str) -> None:
    """Reject nonstandard JSON numeric constants."""
    raise ValueError(f"Invalid JSON constant {value}.")


def check_trace(path: Path) -> None:
    """Require recorded spans with no crossing intervals on a trace track."""
    trace = json.loads(path.read_bytes(), parse_float=Decimal, parse_constant=reject_constant)
    events = trace["traceEvents"] if isinstance(trace, dict) else trace
    if not isinstance(events, list):
        raise ValueError(f"Expected a list of trace events in {path}.")
    tracks = defaultdict(list)
    for event in events:
        if not isinstance(event, dict) or not isinstance(event.get("name"), str):
            raise ValueError(f"Invalid trace event in {path}.")
        if event.get("name") == "Rcmake profile samples omitted":
            raise ValueError(f"The trace omitted samples in {path}.")
        if event.get("ph") != "X":
            continue
        start, duration = event["ts"], event["dur"]
        if any(type(value) not in (int, Decimal) or value < 0 for value in (start, duration)):
            raise ValueError(f"Invalid time in {path}.")
        track = (event["pid"], event["tid"])
        if any(type(value) is not int or value < 0 for value in track):
            raise ValueError(f"Invalid trace track in {path}.")
        tracks[track].append((start, start + duration, event["name"]))
    if not tracks:
        raise ValueError(f"No complete events in {path}.")
    for track, spans in tracks.items():
        ends = []
        for start, end, name in sorted(spans, key=lambda span: (span[0], -span[1])):
            while ends and start >= ends[-1]:
                ends.pop()
            if ends and end > ends[-1]:
                raise ValueError(f"Trace overlap spill on track {track} at {name}.")
            ends.append(end)


def check_profile(build: Path) -> None:
    """Check the trace, folded stack rows, and HTML flamegraph data."""
    directory = build / "rcmake-profile"
    current = (directory / "current").read_text(encoding="utf-8")
    if not re.fullmatch(r"run-[0-9-]+\n", current):
        raise ValueError(f"Invalid profile pointer in {directory}.")
    directory /= current.rstrip("\n")
    check_trace(directory / "trace.json")
    folded = (directory / "stacks.folded").read_text(encoding="utf-8").splitlines()
    if not folded or any(not re.fullmatch(r".+ [1-9][0-9]*", row) for row in folded):
        raise ValueError(f"Invalid or truncated folded stacks in {directory}.")

    html = (directory / "flamegraph.html").read_text(encoding="utf-8")
    data = re.findall(r'<script type="application/json" id="profile-data">(.*?)</script>',
                      html, flags=re.DOTALL)
    if not html.lower().startswith("<!doctype html>") or not html.rstrip().endswith("</html>") or len(data) != 1:
        raise ValueError(f"Invalid flamegraph HTML in {directory}.")
    graph = json.loads(data[0], parse_constant=reject_constant)
    frames = graph["frames"]
    if graph["omitted"] != 0 or graph["dropped"] != 0 or not isinstance(frames, list) or not frames:
        raise ValueError(f"Missing or truncated flamegraph frames in {directory}.")
    for index, frame in enumerate(frames):
        if (not isinstance(frame, list) or len(frame) != 8
                or type(frame[0]) is not int or not 0 <= frame[0] <= index
                or any(not isinstance(value, str) for value in frame[1:4])
                or any(type(value) is not int or value < 0 for value in frame[4:])):
            raise ValueError(f"Invalid flamegraph frame {index} in {directory}.")


def configure(args, binary: Path, source: Path, build: Path, env: dict[str, str],
              flag: str | None = None) -> tuple[bytes, bytes]:
    """Configure in a fresh Ninja tree with four File API requests."""
    query = build / API / "query"
    query.mkdir(parents=True)
    for name in QUERIES:
        (query / name).touch()
    store = str(build / "rcmake-store")
    env = {**env, "RCMAKE_CACHE_DIR": store, "RCMAKE_PROBE_CACHE_DIR": store}
    command = [binary, *args.cmake_arg, "-S", source, "-B", build, "-G", "Ninja",
               "-DCMAKE_EXPORT_COMPILE_COMMANDS=ON"]
    if flag:
        command.append(flag)
    return run(command, build.parent, env, args.timeout)


def configure_streams(streams: tuple[bytes, bytes], build: Path, tools: dict[Path, str],
                      *, compat: bool = True) -> dict[str, bytes]:
    """Normalize paths and configure phase durations in the captured streams."""
    if compat and any(code in stream for stream in streams for code in (b"\x1b", b"\xc2\x9b")):
        raise ValueError(f"ANSI escape in Compat output from {build}.")
    paths = Normalizer(build, replacements=tools)
    stdout, stderr = (normalized(stream, paths) for stream in streams)
    return {"stdout": PHASE_TIME.sub(rb"\g<1> (<elapsed>)", stdout), "stderr": stderr}


PROBE_INPUT_OBSERVER = """\
case "$PWD" in
    */TryCompile-*|*/CMakeTmp|*/CMakeTmp/*|*/Spec-*)
        compile=no
        for arg do
            if [ "$arg" = -c ]; then compile=yes; break; fi
        done
        if [ "$compile" = yes ]; then
            for arg do
                case "$arg" in
                    *.c|*.cc|*.cpp|*.cxx|*.C)
                        if [ -f "$arg" ]; then
                            printf '%s\\0' "${arg##*/}" >> "$RCMAKE_M6_PROBE_LOG"
                        fi
                        ;;
                esac
            done
        fi
        ;;
esac
"""


def observe_compilers(root: Path, env: dict[str, str], *, intervals: bool = False) -> dict[str, str]:
    """Record probe input names for the benchmark."""
    if intervals:
        raise ValueError("Use the native compiler interval recorder.")
    prefix = "#!/bin/sh\n" + PROBE_INPUT_OBSERVER
    observed = {**env, "RCMAKE_M6_PROBE_LOG": str(root / "probe-inputs.bin")}
    for key, default in (("CC", "cc"), ("CXX", "c++")):
        command = shlex.split(env.get(key) or default)
        if not command or not (compiler := shutil.which(command[0], path=env.get("PATH"))):
            continue
        wrapper = root / f"observe-{key}"
        wrapper.write_text(
            f'{prefix}exec {shlex.join([compiler, *command[1:]])} "$@"\n',
            encoding="utf-8",
        )
        wrapper.chmod(0o755)
        observed[key] = shlex.quote(str(wrapper))
    return observed


def probe_compiles(build: Path) -> list[tuple[str, int, int, int]]:
    """Read the verified compiler intervals from this build."""
    path = build / "CMakeFiles/rcmake-probe-intervals.txt"
    if not path.is_file():
        return []
    records = []
    for line in path.read_text().splitlines():
        scratch, start, end, status = line.split()
        records.append((scratch, int(start), int(end), int(status)))
    return records


def overlap_on_workers(records: list) -> bool:
    """Require two successful compiler calls in distinct, overlapping worker trees."""
    active = {}
    for scratch, start, end, status in sorted(records, key=lambda row: row[1]):
        if status or end <= start or not SPECULATIVE.fullmatch(scratch):
            continue
        active = {path: finish for path, finish in active.items() if finish > start}
        if active.keys() - {scratch}:
            return True
        active[scratch] = max(end, active.get(scratch, 0))
    return False


def check_project(args, source: Path, work: Path, compat: Path, native: Path,
                  reference: Path | None, enabled_flags: list[str], env: dict[str, str]) -> bool:
    """Compare stable output and report whether parallel workers ran."""
    work.mkdir()
    env = {**env, "RCMAKE_MEASURE_PROBE_INTERVALS": "1"}
    paths = {compat: "<cmake-command>", native: "<cmake-command>"}
    build = work / "compat-jobs-1"
    streams = configure(args, compat, source, build, {**env, "RCMAKE_PROBE_JOBS": "1"})
    baseline = snapshot(build, paths)
    baseline_streams = configure_streams(streams, build, paths)
    if probe_compiles(build):
        raise ValueError("Parallel probe workers ran with RCMAKE_PROBE_JOBS=1.")
    if reference:
        build = work / "stderr-reference"
        # Upstream CMake honors CLICOLOR_FORCE=1 even when CMAKE_COLOR_DIAGNOSTICS=OFF.
        ref_env = {key: value for key, value in env.items() if key != "CLICOLOR_FORCE"}
        streams = configure(args, reference, source, build, ref_env)
        reference_stderr = configure_streams(streams, build, {reference: "<cmake-command>"})["stderr"]
        compare({"stderr": reference_stderr}, {"stderr": baseline_streams["stderr"]}, "Compat jobs 1")

    env = {**env, "RCMAKE_PROBE_JOBS": "4"}
    for label, binary, overrides in (
        ("compat-jobs-4", compat, {}),
        ("oracle-mode", native, {"RCMAKE_ORACLE_MODE": "1", "RCMAKE_DIAGNOSTICS": "rich"}),
    ):
        build = work / label
        streams = configure(args, binary, source, build, {**env, **overrides})
        compare({**baseline, **baseline_streams},
                {**snapshot(build, paths), **configure_streams(streams, build, paths)}, label)
        if probe_compiles(build):
            raise ValueError(f"Parallel probe workers ran in {label}.")

    parallel = False
    native_baseline = None
    for jobs in ("1", "4"):
        label = f"native-jobs-{jobs}"
        build = work / label
        native_env = {**env, "RCMAKE_PROBE_JOBS": jobs, "RCMAKE_NO_PROBE_CACHE": "1"}
        streams = configure_streams(configure(args, native, source, build, native_env),
                                    build, paths, compat=False)
        artifacts = {**snapshot(build, paths), "stdout": streams["stdout"]}
        compare({**baseline, "stdout": baseline_streams["stdout"]}, artifacts, label)
        records = probe_compiles(build)
        parallel = overlap_on_workers(records)
        if jobs == "1":
            native_baseline = {**artifacts, "stderr": streams["stderr"]}
            if records:
                raise ValueError("Parallel probe workers ran with RCMAKE_PROBE_JOBS=1.")
        else:
            compare(native_baseline, {**artifacts, "stderr": streams["stderr"]}, label)

    for flag in enabled_flags:
        label = flag[2:].split("=")[0]
        build = work / label
        streams = configure_streams(configure(args, native, source, build, env, flag),
                                    build, paths, compat=False)
        compare({**baseline, "stdout": baseline_streams["stdout"]},
                {**snapshot(build, paths), "stdout": streams["stdout"]}, label)
        if flag == "--profile":
            check_profile(build)
        elif not args.projects:
            rich = streams["stderr"]
            warning = b"Determinism diagnostic sample."
            if warning not in rich or warning not in baseline_streams["stderr"] or rich == baseline_streams["stderr"]:
                raise ValueError("The rich diagnostics flag did not change the fixture warning.")

    print(f"PASS {source.name} output. {len(baseline)} files and both Compat streams match.")
    if parallel:
        print(f"PASS {source.name} parallel probes. Compiler calls overlapped in distinct worker trees.")
    else:
        print(f"UNAVAILABLE {source.name} parallel probes. No overlapping compiler calls in probe workers.")
    for flag in enabled_flags:
        print(f"PASS {source.name} {flag}.")
    return parallel


OPAQUE_COMPILER = """\
import os
from pathlib import Path
import subprocess
import sys
import time

compiler = sys.argv[1:]
scratch = Path.cwd()
root = Path(os.environ["RCMAKE_TEST_OPAQUE_LOG"])
sentinel = root.with_suffix(".sentinel")
names = {Path(arg).name for arg in compiler}
probe = next((name for name in ("first.c", "second.c", "third.c") if name in names), None)
if "-c" not in compiler or probe is None:
    os.execv(compiler[0], compiler)
with root.open("a", encoding="utf-8") as output:
    print(scratch.name, probe, file=output)
if probe == "second.c" and not sentinel.is_file():
    sys.exit(1)
if probe == "first.c":
    time.sleep(0.10)
status = subprocess.run(compiler, check=False, timeout=15).returncode
if probe == "first.c" and status == 0:
    sentinel.touch()
sys.exit(status)
"""


def check_opaque_compiler(args, root: Path, native: Path, env: dict[str, str]) -> None:
    """Ensure an opaque compiler with dependencies between calls runs in order."""
    command = shlex.split(env.get("CC") or "cc")
    compiler = shutil.which(command[0], path=env.get("PATH")) if command else None
    if not compiler:
        if args.require_workers or args.require_native:
            raise ValueError("The opaque compiler fixture requires a C compiler.")
        return
    root.mkdir()
    observer = root / "opaque.py"
    observer.write_text(OPAQUE_COMPILER, encoding="utf-8")
    wrapper = root / "opaque-cc"
    wrapper.write_text(
        f'#!/bin/sh\nexec {shlex.join([sys.executable, str(observer), compiler, *command[1:]])} "$@"\n',
        encoding="utf-8",
    )
    wrapper.chmod(0o755)
    source = root / "src"
    source.mkdir()
    (source / "CMakeLists.txt").write_text("""\
cmake_minimum_required(VERSION 4.5)
project(Opaque C)
try_compile(FIRST SOURCE_FROM_CONTENT first.c "int main(void) { return 0; }")
try_compile(SECOND SOURCE_FROM_CONTENT second.c "int main(void) { return 0; }")
try_compile(THIRD SOURCE_FROM_CONTENT third.c "int main(void) { return 0; }")
if(NOT FIRST OR NOT SECOND OR NOT THIRD)
  message(FATAL_ERROR "The compiler calls ran out of order.")
endif()
""", encoding="utf-8")
    expected = None
    for jobs in ("1", "4"):
        log = root / f"jobs-{jobs}.log"
        build = root / f"build-{jobs}"
        trial_env = {**env, "CC": str(wrapper), "RCMAKE_NO_PROBE_CACHE": "1",
                     "RCMAKE_PROBE_JOBS": jobs, "RCMAKE_TEST_OPAQUE_LOG": str(log)}
        streams = configure_streams(configure(args, native, source, build, trial_env),
                                    build, {native: "<cmake-command>"}, compat=False)
        records = [line.split() for line in log.read_text().splitlines()]
        if any(SPECULATIVE.fullmatch(scratch) for scratch, _ in records):
            raise ValueError("A worker started the opaque compiler.")
        if [name for _, name in records] != ["first.c", "second.c", "third.c"]:
            raise ValueError("The opaque compiler calls ran out of order.")
        if expected is None:
            expected = streams
        else:
            compare(expected, streams, "opaque compiler jobs 4")
    print("PASS opaque compiler. The probe calls ran in order without workers.")


def check_replay_order(args, root: Path, native: Path, env: dict[str, str]) -> None:
    """Check target and test order after the child directory replays."""
    source = root / "src"
    child = source / "child"
    child.mkdir(parents=True)
    (source / "CMakeLists.txt").write_text("""\
cmake_minimum_required(VERSION 4.5)
project(ReplayOrder NONE)
enable_testing()
add_subdirectory(child)
get_property(targets DIRECTORY child PROPERTY BUILDSYSTEM_TARGETS)
get_property(tests DIRECTORY child PROPERTY TESTS)
if(NOT targets STREQUAL "second;first" OR NOT tests STREQUAL "test_second;test_first")
  message(FATAL_ERROR "Wrong order. Targets=[${targets}], tests=[${tests}]")
endif()
""", encoding="utf-8")
    entry = child / "CMakeLists.txt"
    entry.write_text("""\
foreach(kind IN ITEMS second first)
  if(kind STREQUAL first)
    add_library(first INTERFACE)
    add_test(NAME test_first COMMAND "${CMAKE_COMMAND}" -E true)
  else()
    add_library(second INTERFACE)
    add_test(NAME test_second COMMAND "${CMAKE_COMMAND}" -E true)
  endif()
endforeach()
""", encoding="utf-8")
    build = root / "build"
    command = [native, "-S", source, "-B", build, "-G", "Ninja"]
    measure = {**env, "RCMAKE_MEASURE_REPLAY": "1", "RCMAKE_CACHE_DIR": str(root / "store")}
    run(command, root, measure, args.timeout)
    journal = build / "CMakeFiles/rcmake/configure.v1.bin"
    journal.unlink()
    run(command, root, measure, args.timeout)
    ctest = build / "child/CTestTestfile.cmake"
    cold = ctest.read_bytes()
    first = cold.index(b"test_second")
    if first >= cold.index(b"test_first"):
        raise ValueError("CTest registration differs from test creation order.")
    run(command, root, measure, args.timeout)
    report = json.loads((journal.parent / "replay-result.v1.json").read_bytes())
    if entry.as_posix() not in report["replayed"]:
        raise ValueError("The child directory did not replay.")
    if ctest.read_bytes() != cold:
        raise ValueError("CTest registration changed after replay.")
    run(command, root, {**measure, "RCMAKE_NO_INCREMENTAL": "1"}, args.timeout)
    if ctest.read_bytes() != cold:
        raise ValueError("CTest registration changed with replay disabled.")
    print("PASS replay order. The child replayed with the same targets and tests.")


def persona_path(binary: Path, name: str, aliases: Path) -> Path:
    """Use the staged tool name or create a temporary link to the same binary."""
    if binary.suffix.lower() == ".exe":
        name += ".exe"
    sibling = binary.with_name(name)
    if sibling.is_file() and sibling.samefile(binary):
        return executable(sibling)
    link = aliases / name
    link.symlink_to(binary)
    return link


def is_probe_output(path: Path) -> bool:
    """Identify compiler probe files and the configure log."""
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


def generator_snapshot(build: Path) -> dict[str, bytes]:
    """Read all generated files and symlink targets outside compiler probes."""
    files = {}
    prefix = os.fsencode(build)
    for path in sorted(build.rglob("*")):
        relative = path.relative_to(build)
        if is_probe_output(relative):
            continue
        if path.is_symlink():
            name, data = f"{relative} (symlink)", os.fsencode(path.readlink())
        elif path.is_file():
            name, data = str(relative), path.read_bytes()
        elif path.is_dir():
            continue
        else:
            raise ValueError(f"Unsupported output type at {relative}.")
        files[name] = data.replace(prefix, b"<B>")
    return files


def check_generators(args, binary: Path, root: Path, env: dict[str, str]) -> int:
    """Check two clean builds for each generator on the original fixture."""
    source = root / "src"
    source.mkdir()
    for name, content in GENERATOR_FIXTURE.items():
        (source / name).write_text(content, encoding="utf-8")
    stdout, _ = run([binary, "-E", "capabilities"], root, env, args.timeout)
    advertised = {generator["name"] for generator in json.loads(stdout)["generators"]}
    requirements = {"Ninja": "ninja", "Ninja Multi-Config": "ninja", "Unix Makefiles": "make"}
    checked = failures = 0
    for index, name in enumerate(GENERATORS):
        if name not in advertised:
            print(f"SKIP {name}. The binary does not advertise this generator.")
            continue
        if name in requirements and not shutil.which(requirements[name]):
            print(f"SKIP {name}. The required build tool is missing.")
            continue
        if name == "Xcode" and sys.platform != "darwin":
            print("SKIP Xcode. This generator requires macOS.")
            continue
        checked += 1
        try:
            snapshots = []
            for suffix in ("a", "b"):
                build = root / f"generator-{index}" / f"bld-{suffix}"
                streams = run([binary, "-S", source, "-B", build, "-G", name], root, env, args.timeout)
                generated = generator_snapshot(build)
                if not generated:
                    raise ValueError("No generated files.")
                snapshots.append({**generated, **configure_streams(streams, build, {}, compat=False)})
            compare(*snapshots, name)
            print(f"PASS {name}. {len(snapshots[0]) - 2} files and both streams match.")
        except (OSError, ValueError, subprocess.SubprocessError) as error:
            failures += 1
            print(f"FAIL {name}. {error}", file=sys.stderr)
    if not checked:
        raise ValueError("No generators were available.")
    return int(bool(failures))


def main() -> int:
    """Run the determinism gate or print its execution plan."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("projects", type=Path, nargs="*", help="Directories with a CMakeLists.txt.")
    binaries = parser.add_mutually_exclusive_group()
    binaries.add_argument("--rcmake", type=Path, help="Staged executable for the determinism gate.")
    binaries.add_argument("--rcmake-bin", type=Path, help="Staged executable for the generator suite.")
    parser.add_argument("--baseline", "--oracle", default=os.environ.get("RCMAKE_ORACLE_CMAKE"),
                        help="Reference CMake executable for exact Compat stderr comparison.")
    parser.add_argument("--cmake-arg", action="append", default=[],
                        help="Add a configure argument. Use --cmake-arg=-DNAME=VALUE.")
    parser.add_argument("--timeout", type=int, default=120, help="Seconds per command.")
    parser.add_argument("--require-native", action="store_true",
                        help="Require the Native flags and overlapping probe workers.")
    parser.add_argument("--require-workers", action="store_true",
                        help="Fail if the Native jobs-4 configure has no overlapping probe jobs.")
    parser.add_argument("--dry-run", action="store_true", help="Print the plan without running commands.")
    args = parser.parse_args()
    if args.timeout < 1:
        parser.error("Use a positive timeout.")
    if not args.rcmake and (args.projects or args.cmake_arg or args.require_native or args.require_workers):
        parser.error("Use --rcmake to select the determinism gate.")
    if not args.rcmake:
        binary = args.rcmake_bin or Path(os.environ.get("RCMAKE_BIN", ROOT / "target/stage/bin/rcmake"))
        if args.dry_run:
            print(f"Executable {binary}. Compare two clean builds per available generator.")
            return 0
        with tempfile.TemporaryDirectory(prefix="rcmake-determinism-", dir="/tmp") as directory:
            return check_generators(args, executable(binary), Path(directory).resolve(),
                                    dict(os.environ, SOURCE_DATE_EPOCH="946684800", TZ="UTC"))
    if args.dry_run:
        print(f"Executable {args.rcmake}. Projects {', '.join(map(str, args.projects)) or 'built-in C/C++ fixture'}.")
        print("Compare Compat and Native jobs 1 and 4, oracle mode, and the available Native flags.")
        print("Check Ninja, the CMake cache, compile commands, File API replies, both streams, and profiles.")
        if args.baseline:
            print(f"Compare Compat stderr with {args.baseline}.")
        return 0

    env = {key: value for key, value in os.environ.items()
           if not key.startswith(("RCMAKE_", "CMAKE_")) or key == "RCMAKE_VENDORED_DATA_ROOT"}
    env.update(LC_ALL="C", CLICOLOR_FORCE="1", CMAKE_COLOR_DIAGNOSTICS="OFF", COPYFILE_DISABLE="1",
               SOURCE_DATE_EPOCH="946684800", TZ="UTC")
    binary = executable(args.rcmake)
    reference = executable(args.baseline) if args.baseline else None
    with tempfile.TemporaryDirectory(prefix="rcmake-determinism-", dir="/tmp") as directory:
        root = Path(directory).resolve()
        aliases = root / "bin"
        aliases.mkdir()
        compat = persona_path(binary, "cmake", aliases)
        native = persona_path(binary, "rcmake", aliases)
        help_text, _ = run([native, "--help"], root, env, args.timeout)
        enabled = []
        for flag in NATIVE_FLAGS:
            if re.search(rb"(?m)^\s*" + re.escape(flag.split("=")[0].encode()) + rb"(?=[=\s\[])", help_text):
                enabled.append(flag)
            elif args.require_native:
                raise ValueError(f"{native} does not advertise {flag}.")
            else:
                print(f"SKIP {flag}. The staged binary does not advertise this flag.")

        sources = [path.resolve() for path in args.projects]
        if not sources:
            source = root / "fixture"
            source.mkdir()
            for name, contents in FIXTURE.items():
                (source / name).write_text(contents, encoding="utf-8")
            sources.append(source)
        complete = True
        if args.require_native:
            check_replay_order(args, root / "replay-order", native, env)
        if os.name != "nt":
            check_opaque_compiler(args, root / "opaque", native, env)
        for index, source in enumerate(sources):
            if not (source / "CMakeLists.txt").is_file():
                raise ValueError(f"No CMakeLists.txt in {source}.")
            complete &= check_project(args, source, root / f"project-{index}", compat, native,
                                      reference, enabled, env)
    return 2 if (args.require_native or args.require_workers) and not complete else 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (KeyError, OSError, TypeError, ValueError, subprocess.SubprocessError) as exc:
        sys.exit(str(exc))

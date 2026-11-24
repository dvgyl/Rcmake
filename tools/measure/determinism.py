#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Check configure output across probe worker counts and CLI modes.

Source env/env.sh before use. Use --rcmake for the Milestone 6 Ninja gate.
Its default fixture has two C/C++ targets and emits a warning. Pass project
directories to check other inputs. Use --rcmake-bin for the generator suite.
"""

from __future__ import annotations

import argparse
from collections import Counter, defaultdict
from decimal import Decimal
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import sys
import tempfile

from mutation_eval import API, QUERIES, diff_bytes
from ninja_canon import Normalizer
from oracle_prop_test import executable


ROOT = Path(__file__).resolve().parents[2]
GENERATORS = ("Ninja", "Ninja Multi-Config", "Unix Makefiles", "Xcode")
ARTIFACTS = ("build.ninja", "CMakeCache.txt", "compile_commands.json")
NATIVE_FLAGS = ("--diagnostics=rich", "--profile")
FIXTURE = {
    "CMakeLists.txt": """\
cmake_minimum_required(VERSION 4.5)
project(M6Determinism LANGUAGES C CXX)
include(CheckCSourceCompiles)
include(CheckCXXSourceCompiles)
check_c_source_compiles("int main(void) { return 0; }" M6_C_PROBE)
check_cxx_source_compiles("int main() { return 0; }" M6_CXX_PROBE)
if(NOT M6_C_PROBE OR NOT M6_CXX_PROBE)
  message(FATAL_ERROR "A Milestone 6 compiler probe failed.")
endif()
add_library(c_part STATIC part.c)
add_executable(cxx_app main.cpp)
target_link_libraries(cxx_app PRIVATE c_part)
message(WARNING "Milestone 6 diagnostic sample.")
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


def normalized(data: bytes, paths: Normalizer) -> bytes:
    """Replace known paths and preserve all other bytes."""
    return paths.text(data.decode(errors="surrogateescape")).encode(errors="surrogateescape")


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
    """Read the generated files with only build and executable paths replaced."""
    paths = Normalizer(build, replacements=tool_paths)
    files = {name: normalized((build / name).read_bytes(), paths) for name in ARTIFACTS}
    if any(not data.strip() for data in files.values()):
        raise ValueError(f"Empty generated artifact in {build}.")
    if not json.loads(files["compile_commands.json"]):
        raise ValueError(f"No compile commands in {build}.")
    return {**files, **file_api(build, paths)}


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


def check_compat(streams: tuple[bytes, bytes], build: Path) -> bytes:
    """Check both streams for ANSI escapes and return normalized stderr."""
    if any(code in stream for stream in streams for code in (b"\x1b", b"\xc2\x9b")):
        raise ValueError(f"ANSI escape in Compat output from {build}.")
    return normalized(streams[1], Normalizer(build))


def check_project(args, source: Path, work: Path, compat: Path, native: Path,
                  reference: Path | None, enabled_flags: list[str], env: dict[str, str]) -> None:
    """Check worker count, Compat stderr, and each available native flag."""
    work.mkdir()
    paths = {compat: "<cmake-command>", native: "<cmake-command>"}
    build = work / "compat-jobs-1"
    streams = configure(args, compat, source, build, {**env, "RCMAKE_PROBE_JOBS": "1"})
    baseline = snapshot(build, paths)
    baseline_err = check_compat(streams, build)
    if reference:
        build = work / "stderr-reference"
        stderr = check_compat(configure(args, reference, source, build, env), build)
        compare({"stderr": stderr}, {"stderr": baseline_err}, "Compat jobs 1")

    env = {**env, "RCMAKE_PROBE_JOBS": "4"}
    for label, binary, overrides in (
        ("compat-jobs-4", compat, {}),
        ("oracle-mode", native, {"RCMAKE_ORACLE_MODE": "1", "RCMAKE_DIAGNOSTICS": "rich"}),
    ):
        build = work / label
        stderr = check_compat(configure(args, binary, source, build, {**env, **overrides}), build)
        compare({**baseline, "stderr": baseline_err},
                {**snapshot(build, paths), "stderr": stderr}, label)

    for flag in enabled_flags:
        label = flag[2:].split("=")[0]
        build = work / label
        _, stderr = configure(args, native, source, build, env, flag)
        compare(baseline, snapshot(build, paths), label)
        if flag == "--profile":
            check_profile(build)
        elif not args.projects and stderr == baseline_err:
            raise ValueError("The rich diagnostics flag did not change the fixture warning.")

    print(f"PASS {source.name}. {len(baseline)} files match. Compat stderr matches and has no ANSI escapes.")
    for flag in enabled_flags:
        print(f"PASS {source.name} {flag}.")


def persona_path(binary: Path, name: str, aliases: Path) -> Path:
    """Use the staged tool name or create a temporary link to the same binary."""
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
                run([binary, "-S", source, "-B", build, "-G", name], root, env, args.timeout)
                snapshots.append(generator_snapshot(build))
            if not snapshots[0]:
                raise ValueError("No generated files.")
            compare(*snapshots, name)
            print(f"PASS {name}. {len(snapshots[0])} files match.")
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
    binaries.add_argument("--rcmake", type=Path, help="Staged executable for the Milestone 6 gate.")
    binaries.add_argument("--rcmake-bin", type=Path, help="Staged executable for the generator suite.")
    parser.add_argument("--baseline", "--oracle", default=os.environ.get("RCMAKE_ORACLE_CMAKE"),
                        help="Reference CMake executable for exact Compat stderr comparison.")
    parser.add_argument("--cmake-arg", action="append", default=[],
                        help="Add a configure argument. Use --cmake-arg=-DNAME=VALUE.")
    parser.add_argument("--timeout", type=int, default=120, help="Seconds per command.")
    parser.add_argument("--require-native", action="store_true", help="Fail if a native flag is absent.")
    parser.add_argument("--dry-run", action="store_true", help="Print the plan without running commands.")
    args = parser.parse_args()
    if args.timeout < 1:
        parser.error("Use a positive timeout.")
    if not args.rcmake and (args.projects or args.cmake_arg or args.require_native):
        parser.error("Use --rcmake to select the Milestone 6 gate.")
    if not args.rcmake:
        binary = args.rcmake_bin or Path(os.environ.get("RCMAKE_BIN", ROOT / "target/stage/bin/rcmake"))
        if args.dry_run:
            print(f"Executable {binary}. Compare two clean builds per available generator.")
            return 0
        with tempfile.TemporaryDirectory(prefix="rcmake-determinism-", dir="/tmp") as directory:
            return check_generators(args, executable(binary), Path(directory).resolve(), dict(os.environ))
    if args.dry_run:
        print(f"Executable {args.rcmake}. Projects {', '.join(map(str, args.projects)) or 'built-in C/C++ fixture'}.")
        print("Compare Compat jobs 1 and 4, oracle mode, and the available native flags.")
        print("Check Ninja, the CMake cache, compile commands, File API replies, stderr, and profile files.")
        if args.baseline:
            print(f"Compare Compat stderr with {args.baseline}.")
        return 0

    env = {key: value for key, value in os.environ.items()
           if not key.startswith(("RCMAKE_", "CMAKE_")) or key == "RCMAKE_VENDORED_DATA_ROOT"}
    env.update(LC_ALL="C", CLICOLOR_FORCE="1", CMAKE_COLOR_DIAGNOSTICS="OFF", COPYFILE_DISABLE="1")
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
        for index, source in enumerate(sources):
            if not (source / "CMakeLists.txt").is_file():
                raise ValueError(f"No CMakeLists.txt in {source}.")
            check_project(args, source, root / f"project-{index}", compat, native, reference, enabled, env)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (KeyError, OSError, TypeError, ValueError, subprocess.SubprocessError) as exc:
        sys.exit(str(exc))

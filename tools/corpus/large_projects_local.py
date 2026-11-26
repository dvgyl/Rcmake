#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Compare results for five large projects on macOS arm64.

Source env/env.sh in Bash before running this script. Use a release stage
with bin/rcmake, the three CMake tool links, and share/cmake-4.5. Use a fresh
run directory on the internal volume. Reports stay after build tree cleanup.
"""

from __future__ import annotations

import argparse
from collections import Counter
from collections.abc import Iterator
from contextlib import contextmanager, suppress
from dataclasses import dataclass
from filecmp import cmp
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import signal
import subprocess
import sys
import time
import tomllib
from typing import BinaryIO
from xml.etree.ElementTree import ParseError

from normalize import normalize_codemodel_abstract_target_ties
from pathswap import check_exec_log, executable, write_json
from ninja_canon import Normalizer, normalized_commands
from path_swap_eval import (
    BLOCKED_TOOLS, DATA_DIR, TOOLS, TOOL_CACHE_KEYS, cache_tools, lane_environment,
    support_path, test_results, version_line,
)

ROOT = Path(__file__).resolve().parents[2]
MANIFEST = ROOT / "conformance/corpus/large_projects.toml"
GIB = 1024**3
PROJECT_SPACE = {"duckdb": 27, "opencv": 27, "vtk": 34, "qtbase": 32, "llvm-project": 48}
MIN_FREE = 15 * GIB
MAX_RSS = 9 * GIB
CONFIGURE_LIMIT = 600
DRY_RUN_LIMIT = 120
L1_FILES = ("cache.json", "codemodel.json", "edges.stdout", "diagnostics.txt")
RUN_ERRORS = (OSError, RuntimeError, ValueError, KeyError, ParseError, subprocess.SubprocessError)
# File API members that hold backtrace indexes or reply file names. The
# comparison drops them because they change when an unrelated node changes.
MODEL_IGNORED_KEYS = frozenset({"backtrace", "backtraces", "backtraceGraph", "jsonFile"})
# Cache entries whose value is the configure time. Only the value is hidden.
NONDETERMINISTIC_CACHE_KEYS = frozenset({"OPENCV_TIMESTAMP"})

GRAPH_SCRIPT = """
import json
from pathlib import Path
import sys
from ninja_canon import Normalizer, canonicalize

build, source, replacements = sys.argv[1:]
paths = {Path(path): label for path, label in json.loads(replacements)}
json.dump(canonicalize(Normalizer(Path(build), Path(source), paths)), sys.stdout, sort_keys=True)
"""

VTK_SMOKE = (
    'find_package(VTK 9.5 CONFIG REQUIRED COMPONENTS CommonCore FiltersCore'
    ' PATHS "${CMAKE_PREFIX_PATH}" NO_DEFAULT_PATH)',
    "VTK::CommonCore VTK::FiltersCore",
    "#include <vtkNew.h>\n#include <vtkTriangleFilter.h>\n"
    "int main() { vtkNew<vtkTriangleFilter> f; return f->GetOutput() == nullptr; }\n",
)
QT_SMOKE = (
    'find_package(Qt6 6.8.3 EXACT CONFIG REQUIRED COMPONENTS Core Network'
    ' PATHS "${CMAKE_PREFIX_PATH}" NO_DEFAULT_PATH)',
    "Qt6::Core Qt6::Network",
    "#include <QtCore/QCoreApplication>\n#include <QtNetwork/QHostAddress>\n"
    "int main(int n, char **v) { QCoreApplication app(n, v); "
    "return QHostAddress(QHostAddress::LocalHost).isNull(); }\n",
)


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    """Read the project, tools, output directory, and run mode."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--project", required=True, choices=(*PROJECT_SPACE, "all"))
    parser.add_argument("--rcmake-stage", required=True, type=Path,
                        help="Rcmake installation root with bin and share directories.")
    parser.add_argument("--oracle", required=True, type=Path, help="Path to Oracle cmake.")
    parser.add_argument("--mode", choices=("parity", "full"), default="parity",
                        help="Full also runs the representative build, tests, and manifest checks.")
    parser.add_argument("--run-dir", required=True, type=Path, help="Directory for a fresh run.")
    parser.add_argument("--keep-builds", action="store_true", help="Keep source and build trees.")
    return parser.parse_args(argv)


def load_projects(selection: str, manifest: Path = MANIFEST) -> list[dict]:
    """Load the selected pins in the local run order."""
    with manifest.open("rb") as stream:
        data = tomllib.load(stream)
    if data.get("schema_version") != 1:
        raise ValueError("The large-project manifest must use schema version 1.")
    pins = {pin["name"]: pin for pin in data["project"]}
    return [pins[name] for name in (PROJECT_SPACE if selection == "all" else (selection,))]


def group_rss(pid: int) -> int:
    """Return the sampled resident memory of a process group in bytes."""
    processes = subprocess.check_output(["/bin/ps", "-axo", "pgid=,rss="], text=True, timeout=5)
    return sum(int(kib) for group, kib in map(str.split, processes.splitlines())
               if int(group) == pid) * 1024


@contextmanager
def process_group(argv: list[str], cwd: Path, env: dict[str, str],
                  out: BinaryIO, err: BinaryIO) -> Iterator[subprocess.Popen[bytes]]:
    """Stop remaining group members and wait for the leader on exit."""
    with subprocess.Popen(argv, cwd=cwd, env=env, stdin=subprocess.DEVNULL,
                          stdout=out, stderr=err, start_new_session=True) as proc:
        try:
            yield proc
        finally:
            # The group can still contain children after the leader exits.
            with suppress(ProcessLookupError):
                os.killpg(proc.pid, signal.SIGKILL)
            proc.wait()


def run_command(log: Path, label: str, argv: list, timeout: int,
                env: dict[str, str], cwd: Path) -> Path:
    """Run one process group and save its streams and status."""
    argv = list(map(str, argv))
    print(f"[{log.name}] {label}", flush=True)
    start = time.monotonic()
    peak_rss = 0
    min_free = shutil.disk_usage(log).free
    proc = None
    stop = "disk limit" if min_free < MIN_FREE else None
    stdout, stderr = (log / f"{label}.{suffix}" for suffix in ("stdout", "stderr"))
    with stdout.open("wb") as out, stderr.open("wb") as err:
        try:
            if stop is None:
                with process_group(argv, cwd, env, out, err) as proc:
                    while proc.poll() is None:
                        free = shutil.disk_usage(log).free
                        rss = group_rss(proc.pid)
                        min_free = min(min_free, free)
                        peak_rss = max(peak_rss, rss)
                        remaining = timeout - (time.monotonic() - start)
                        if remaining <= 0:
                            stop = "timeout"
                        elif free < MIN_FREE:
                            stop = "disk limit"
                        elif rss > MAX_RSS:
                            stop = "memory limit"
                        if stop:
                            break
                        with suppress(subprocess.TimeoutExpired):
                            proc.wait(timeout=min(1, remaining))
        except BaseException as error:
            stop = str(error) or type(error).__name__
            raise
        finally:
            code = proc.returncode if proc is not None else None
            write_json(log / f"{label}.json", {
                "argv": argv, "cwd": str(cwd), "returncode": code, "stop": stop,
                "limit_s": timeout, "wall_s": round(time.monotonic() - start, 2),
                "min_free_bytes": min_free, "peak_group_rss_bytes_sampled": peak_rss,
                "status": "pass" if code == 0 and stop is None else "fail",
            })
    if code or stop:
        raise RuntimeError(f"{label} failed ({stop or code}). See {log}.")
    return stdout


@dataclass(frozen=True)
class Toolchain:
    """Store the paths that must stay the same for both engines."""

    developer: Path
    sdk: Path
    cc: Path
    cxx: Path
    ninja: Path
    python: Path

    def cmake_args(self) -> list[str]:
        """Return the shared macOS configure options."""
        return [
            "-DCMAKE_BUILD_TYPE=Release", "-DCMAKE_POLICY_VERSION_MINIMUM=3.5",
            "-DCMAKE_OSX_ARCHITECTURES=arm64", f"-DCMAKE_OSX_SYSROOT={self.sdk}",
            f"-DCMAKE_C_COMPILER={self.cc}", f"-DCMAKE_CXX_COMPILER={self.cxx}",
            f"-DCMAKE_MAKE_PROGRAM={self.ninja}", "-DCMAKE_FIND_USE_PACKAGE_REGISTRY=OFF",
            "-DCMAKE_FIND_USE_SYSTEM_PACKAGE_REGISTRY=OFF",
        ]


def installations(stage: Path, oracle: Path) -> dict[str, Path]:
    """Check the layout of the release stage and the oracle installation."""
    candidate_bin = stage.resolve() / "bin"
    oracle_bin = oracle.absolute().parent.resolve()
    if oracle.name != "cmake" or oracle_bin == candidate_bin:
        raise ValueError("Use a separate oracle bin/cmake.")
    binary = executable(candidate_bin / "rcmake")
    if binary.resolve() != binary:
        raise ValueError("Copy the release executable to the stage at bin/rcmake.")
    result = {"oracle": oracle_bin, "rcmake": candidate_bin}
    for engine, directory in result.items():
        if not (directory.parent / DATA_DIR / "Modules/CMakeDetermineSystem.cmake").is_file():
            raise ValueError(f"CMake modules are missing from {directory.parent / DATA_DIR}.")
        for tool in TOOLS:
            path = executable(directory / tool).resolve()
            if (path == binary.resolve()) != (engine == "rcmake"):
                raise ValueError(f"Incorrect executable for {directory / tool}.")
    return result


def preflight(log: Path, engines: dict[str, Path]) -> Toolchain:
    """Pin the selected Xcode tools and record the host and binary identities."""
    if platform.system() != "Darwin" or platform.machine() != "arm64":
        raise ValueError("Use native Python on macOS arm64 for the large-project suite.")
    env = dict(os.environ)

    def probe(label, *argv):
        return run_command(log, label, list(argv), 30, env, log).read_text().strip()

    developer = Path(env.get("DEVELOPER_DIR") or probe("developer-dir", "/usr/bin/xcode-select", "-p"))
    developer = developer.resolve(strict=True)
    env["DEVELOPER_DIR"] = str(developer)
    xcrun = ["/usr/bin/xcrun", "--sdk", "macosx"]
    sdk = Path(probe("sdk", *xcrun, "--show-sdk-path")).resolve(strict=True)
    cc = executable(probe("cc", *xcrun, "--find", "clang"))
    cxx = executable(probe("cxx", *xcrun, "--find", "clang++"))
    ninja = executable(env.get("NINJA") or shutil.which("ninja") or "ninja")
    python = executable(sys.executable)
    probe("arm64-ninja", "/usr/bin/lipo", "-verify_arch", "arm64", ninja)
    candidate, oracle = engines["rcmake"] / "rcmake", engines["oracle"] / "cmake"
    hashes = {}
    for path in (candidate, *(engines["oracle"] / tool for tool in TOOLS), MANIFEST):
        if path != MANIFEST:
            probe(f"arm64-{path.name}", "/usr/bin/lipo", "-verify_arch", "arm64", path)
        with path.open("rb") as stream:
            hashes[str(path)] = hashlib.file_digest(stream, "sha256").hexdigest()
    for label, argv in {
        "xcode": ["/usr/bin/xcodebuild", "-version"],
        "sdk-version": [*xcrun, "--show-sdk-version"],
        "clang": [cc, "--version"], "ninja": [ninja, "--version"], "python": [python, "--version"],
        "macos": ["/usr/bin/sw_vers"], "memory": ["/usr/sbin/sysctl", "hw.memsize", "vm.swapusage"],
    }.items():
        probe(label, *argv)
    toolchain = Toolchain(developer, sdk, cc, cxx, ninja, python)
    write_json(log / "host.json", {
        "sha256": hashes, "oracle": str(oracle), "rcmake": str(candidate),
        "developer_dir": str(developer), "configure_args": toolchain.cmake_args(),
        "python": str(python), "free_bytes": shutil.disk_usage(log).free,
    })
    return toolchain


def checkout(log: Path, work: Path, source: Path, repository: str, revision: str,
             env: dict[str, str], *, sparse: bool = False, label: str = "") -> str:
    """Fetch one revision and return the checked out commit."""
    git = ["git", "-C", source]
    filters = ["--filter=blob:none"] if sparse else []
    steps = [
        ("init", ["git", "init", source], 20),
        ("remote", [*git, "remote", "add", "origin", f"https://github.com/{repository}.git"], 20),
        ("fetch", [*git, "fetch", "--depth=1", *filters, "origin", revision], 900),
    ]
    if sparse:
        steps.append(("sparse", [*git, "sparse-checkout", "set", "llvm", "clang",
                                 "cmake", "third-party"], 600))
    steps.append(("checkout", [*git, "checkout", "--detach", "FETCH_HEAD"], 900))
    for step, argv, timeout in steps:
        run_command(log, f"{label}{step}", argv, timeout, env, work)
    identity = run_command(log, f"{label}source-sha", [*git, "rev-parse", "HEAD"], 20, env, work)
    return identity.read_text().strip()


def configure_args(name: str, pin: dict, lane: Path, tc: Toolchain,
                   env: dict[str, str], test_data: Path | None) -> list[str]:
    """Apply the project options for the pinned macOS profile."""
    args = list(pin["configure_args"])
    if name == "opencv":
        downloads = lane / "downloads"
        downloads.mkdir()
        args += [f"-DOPENCV_DOWNLOAD_PATH={downloads}", "-DWITH_ADE=OFF",
                 "-DBUILD_JPEG=ON", "-DBUILD_PNG=ON", "-DBUILD_ZLIB=ON",
                 f"-DPYTHON3_EXECUTABLE={tc.python}"]
        if test_data:
            env["OPENCV_TEST_DATA_PATH"] = str(test_data)
            args.append(f"-DOPENCV_TEST_DATA_PATH={test_data}")
    elif name == "llvm-project":
        args.append(f"-DPython3_EXECUTABLE={tc.python}")
    elif name == "qtbase":
        ssl, icu = (Path("/opt/homebrew/opt") / name for name in ("openssl@3", "icu4c"))
        ssl, icu = ssl.resolve(strict=True), icu.resolve(strict=True)
        ssl_lib, crypto_lib = (ssl / "lib" / f"lib{name}.3.dylib" for name in ("ssl", "crypto"))
        if not all(path.is_file() for path in (ssl_lib, crypto_lib, icu / "lib/pkgconfig/icu-uc.pc")):
            raise ValueError("Qt requires the Homebrew OpenSSL 3 and ICU libraries.")
        env["PKG_CONFIG_PATH"] = f"{ssl}/lib/pkgconfig:{icu}/lib/pkgconfig"
        args += [f"-DCMAKE_OBJCXX_COMPILER={tc.cxx}", f"-DOPENSSL_ROOT_DIR={ssl}",
                 f"-DOPENSSL_INCLUDE_DIR={ssl}/include", f"-DOPENSSL_SSL_LIBRARY={ssl_lib}",
                 f"-DOPENSSL_CRYPTO_LIBRARY={crypto_lib}", f"-DICU_ROOT={icu}"]
    return args


def verify_tools(log: Path, bin_dir: Path, cwd: Path, env: dict[str, str], candidate: bool) -> None:
    """Check all PATH matches and record each version banner."""
    found = {}
    for tool in TOOLS:
        matches = [Path(directory) / tool for directory in env["PATH"].split(os.pathsep)
                   if shutil.which(tool, path=directory)]
        expected = bin_dir / tool
        if matches != [expected]:
            raise ValueError(f"Expected only {expected} on PATH. Found {matches}.")
        output = run_command(log, tool, [expected, "--version"], 20, env, cwd)
        banner = output.read_text(errors="replace")
        found[tool] = {"path": str(expected), "version": version_line(tool, banner, candidate)}
    write_json(log / "tools.json", found)


def save_configure(log: Path, build: Path) -> None:
    """Save the cache, configure log, File API reply, and Ninja files."""
    reply = build / ".cmake/api/v1/reply"
    if reply.is_dir():
        shutil.copytree(reply, log / "file-api")
    for relative in ("CMakeCache.txt", "config.summary", "CMakeFiles/CMakeConfigureLog.yaml"):
        path = build / relative
        if path.is_file():
            shutil.copy2(path, log / path.name)
    for path in build.rglob("*.ninja"):
        if path.is_file():
            target = log / "ninja" / path.relative_to(build)
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(path, target)


def configure(log: Path, label: str, build: Path, source: Path, args: list[str],
              env: dict[str, str], *, query_api: bool = False) -> None:
    """Configure a new build tree and save any outputs even on failure."""
    if query_api:
        query = build / ".cmake/api/v1/query"
        query.mkdir(parents=True)
        (query / "codemodel-v2").touch()
    evidence = log if label == "configure" else log / label
    evidence.mkdir(exist_ok=True)
    try:
        run_command(log, label, ["cmake", "-S", source, "-B", build, "-G", "Ninja", *args],
                    CONFIGURE_LIMIT, env, source)
    finally:
        save_configure(evidence, build)


def model_data(reply: Path, normalizer: Normalizer) -> dict:
    """Read the codemodel and expand its references to other reply files."""
    indexes = list(reply.glob("index-*.json"))
    if len(indexes) != 1:
        raise ValueError(f"Expected one File API reply index in {reply}.")

    def read(filename: str) -> dict:
        path = reply / filename
        if path.parent != reply or path.is_symlink():
            raise ValueError(f"Invalid File API filename {filename!r}.")
        return json.loads(path.read_text())

    def expand(value):
        if isinstance(value, list):
            return [expand(item) for item in value]
        if isinstance(value, dict):
            row = {key: expand(item) for key, item in value.items()
                   if key not in MODEL_IGNORED_KEYS}
            if "jsonFile" in value:
                row["data"] = expand(read(value["jsonFile"]))
            for key in ("dependencies", "orderDependencies"):
                if key in row:
                    row[key].sort(key=lambda item: item["id"])
            return row
        return normalizer.text(value) if isinstance(value, str) else value

    index = read(indexes[0].name)
    return normalize_codemodel_abstract_target_ties(
        expand(read(index["reply"]["codemodel-v2"]["jsonFile"]))
    )


def verify_configure(log: Path, build: Path, bin_dir: Path, peer: Path,
                     exec_log: Path | None = None, offset: int = 0) -> None:
    """Check cached paths, generated commands, and Rcmake invocation evidence."""
    tools = cache_tools(build, bin_dir)
    for tool, key in TOOL_CACHE_KEYS.items():
        path = Path(tools[key])
        if path != bin_dir / tool and not (
            exec_log is not None and tool == "cmake" and path == bin_dir / "rcmake"
        ):
            raise ValueError(f"{key} points to {path}. Expected {bin_dir / tool}.")
    write_json(log / "cache-tools.json", tools)
    for path in (log / "ninja").rglob("*.ninja"):
        if os.fsencode(peer) + b"/" in path.read_bytes():
            raise ValueError(f"The other engine appears in {path}.")
    if exec_log is not None:
        proof = check_exec_log(exec_log, offset, ("cmake",))
        write_json(log / "exec-proof.json", proof)
        if proof["status"] != "pass":
            raise ValueError("Rcmake configure invocation proof is missing or invalid.")


def save_l1(log: Path, normalizer: Normalizer, tc: Toolchain, env: dict[str, str]) -> None:
    """Save normalized configure outputs and record any failed comparison inputs."""
    errors = {}
    cache_path = log / "CMakeCache.txt"
    if cache_path.is_file():
        tool_labels = {key: f"<{tool}>" for tool, key in TOOL_CACHE_KEYS.items()}
        cache = {}
        for line in cache_path.read_text(errors="replace").splitlines():
            key, sep, value = line.partition("=")
            if sep and not key.startswith(("#", "//")):
                name = key.split(":", 1)[0]
                if name in NONDETERMINISTIC_CACHE_KEYS:
                    cache[key] = "<nondeterministic>"
                else:
                    cache[key] = tool_labels.get(name, normalizer.text(value))
        write_json(log / "cache.json", cache)
    (log / "diagnostics.txt").write_text(normalizer.text(
        (log / "configure.stderr").read_text(errors="replace")))
    try:
        write_json(log / "codemodel.json", model_data(log / "file-api", normalizer))
    except RUN_ERRORS as error:
        errors["codemodel.json"] = str(error)
    try:
        replacements = json.dumps([(str(path), label)
                                   for path, label in normalizer.replacements.items()])
        run_command(log, "edges", [tc.python, "-c", GRAPH_SCRIPT, normalizer.build,
                                   normalizer.source, replacements], 180,
                    env | {"PYTHONPATH": str(ROOT / "tools/measure")}, normalizer.build)
    except RUN_ERRORS as error:
        errors["edges.stdout"] = str(error)
    write_json(log / "l1-errors.json", errors)


def representative_test(name: str, log: Path, build: Path, prefix: Path,
                        tc: Toolchain, env: dict[str, str], jobs: str, l4: dict) -> None:
    """Run the project's representative test with the selected engine."""
    lane = build.parent
    if name == "duckdb":
        for target in l4["executables"]:
            run_command(log, "unit-test", [build / target], 1800, env, build)
    elif name == "opencv":
        junit = log / "core-test.xml"
        output = run_command(log, "core-test", ["ctest", "--test-dir", build, *l4["ctest"],
                                               "--no-tests=error", "--verbose", "-j", "1",
                                               "--output-junit", junit], 1800, env, lane)
        skips = re.findall(r"(?m)^\s*(?:\d+:\s*)?\[\s+SKIPPED\s+\]\s+(\d+) tests?\b",
                           output.read_text(errors="replace"))
        write_json(log / "core-test-results.json", {
            "ctest": test_results(junit), "gtest_skipped": sum(map(int, skips)),
        })
    elif name == "llvm-project":
        smoke = lane / "smoke"
        smoke.mkdir()
        program = smoke / "main.cpp"
        program.write_text("int main() { constexpr auto x = [] { return 42; }; return x() != 42; }\n")
        clang = build / "bin/clang"
        run_command(log, "clang-version", [clang, "--version"], 60, env, lane)
        run_command(log, "clang-compile", [clang, "--driver-mode=g++", "-arch", "arm64",
                                           "-isysroot", tc.sdk, "-std=c++17", program,
                                           "-o", smoke / "probe"], 300, env, lane)
        run_command(log, "clang-run", [smoke / "probe"], 60, env, lane)
    else:
        if l4.get("ctest"):
            run_command(log, "upstream-test", ["ctest", "--test-dir", build, *l4["ctest"],
                                               "--no-tests=error", "--output-on-failure"],
                        1800, env, lane)
        run_command(log, "install", [tc.ninja, "-C", build, "-j", jobs, "install"], 7200, env, lane)
        smoke = lane / "smoke"
        smoke.mkdir()
        package, libraries, program = VTK_SMOKE if name == "vtk" else QT_SMOKE
        (smoke / "main.cpp").write_text(program)
        (smoke / "CMakeLists.txt").write_text(
            "cmake_minimum_required(VERSION 3.25)\nproject(probe LANGUAGES CXX)\n"
            f"{package}\nadd_executable(probe main.cpp)\n"
            f"target_link_libraries(probe PRIVATE {libraries})\n"
            "enable_testing()\nadd_test(NAME probe COMMAND probe)\n")
        configure(log, "smoke-configure", smoke / "build", smoke,
                  [*tc.cmake_args(), f"-DCMAKE_PREFIX_PATH={prefix}"], env)
        run_command(log, "smoke-build", ["cmake", "--build", smoke / "build",
                                        "--parallel", jobs], 600, env, lane)
        run_command(log, "smoke-test", ["ctest", "--test-dir", smoke / "build",
                                       "--no-tests=error", "--output-on-failure",
                                       "-j", "1"], 600, env, lane)


def run_lane(engine: str, engines: dict[str, Path], pin: dict, lane: Path, log: Path,
             source: Path, support: list[Path], tc: Toolchain, mode: str,
             test_data: Path | None, result: dict) -> None:
    """Run one engine and record each completed level before continuing."""
    name = pin["name"]
    candidate = engine == "rcmake"
    bin_dir, peer = engines[engine], engines["oracle" if candidate else "rcmake"]
    build, prefix = lane / "build", lane / "install"
    jobs = "2" if name in {"duckdb", "opencv"} else "1"
    env = lane_environment(lane, [bin_dir, *support])
    env.update(DEVELOPER_DIR=str(tc.developer), CC=str(tc.cc), CXX=str(tc.cxx), SDKROOT=str(tc.sdk),
               NINJA_STATUS="[%f/%t] ", PYTHONDONTWRITEBYTECODE="1", COPYFILE_DISABLE="1",
               CMAKE_BUILD_PARALLEL_LEVEL=jobs, CTEST_PARALLEL_LEVEL="1")
    exec_log = log / "exec.jsonl" if candidate else None
    if exec_log is not None:
        env["RCMAKE_EXEC_LOG"] = str(exec_log)
    args = configure_args(name, pin, lane, tc, env, test_data)
    write_json(log / "environment.json", env)
    if name == "qtbase":
        run_command(log, "qt-dependencies", ["pkg-config", "--modversion", "openssl", "icu-uc",
                                              "icu-i18n"], 20, env, source)
    result["path"] = "fail"
    verify_tools(log, bin_dir, source, env, candidate)
    result["path"] = "pass"
    base = [*tc.cmake_args(), "-DCMAKE_PREFIX_PATH=", f"-DCMAKE_INSTALL_PREFIX={prefix}"]
    cmake_source = source / pin.get("source_subdir", ".")
    result["configure"] = "fail"
    configure(log, "configure", build, cmake_source, [*base, *args], env, query_api=True)
    result["configure"] = "pass"

    replacements = {bin_dir.parent / DATA_DIR: "<cmake-data>", prefix: "<install>", lane: "<lane>"}
    replacements.update({bin_dir / tool: f"<{tool}>" for tool in TOOLS})
    if candidate:
        replacements[bin_dir / "rcmake"] = "<cmake>"
    normalizer = Normalizer(build, source, replacements)
    save_l1(log, normalizer, tc, env)
    result["identity"] = "fail"
    verify_configure(log, build, bin_dir, peer, exec_log)
    if name == "llvm-project" and not re.search(
        r"(?m)^-- Targeting AArch64\s*$", (log / "configure.stdout").read_text(errors="replace")
    ):
        raise ValueError("LLVM configure did not report the AArch64 target.")
    result["identity"] = "pass"

    result["dry_run"] = "fail"
    output = run_command(log, "dry-run", [tc.ninja, "-C", build, "-n", "-v", "--", *pin["l2_targets"]],
                         DRY_RUN_LIMIT, env, lane)
    commands = normalized_commands(output.read_text(errors="replace"), normalizer)
    if not commands:
        raise ValueError("A fresh Ninja dry run produced no commands.")
    write_json(log / "commands.json", commands)
    result["dry_run"] = "pass"
    if mode == "parity":
        return

    result["build"] = "fail"
    manifest_build = build
    if name == "opencv":
        build = lane / "test-build"
        test_args = ["-DBUILD_LIST=core,imgproc,imgcodecs,ts" if arg.startswith("-DBUILD_LIST=") else arg
                     for arg in args]
        offset = exec_log.stat().st_size if exec_log is not None else 0
        configure(log, "test-configure", build, cmake_source, [*base, *test_args], env)
        verify_configure(log / "test-configure", build, bin_dir, peer, exec_log, offset)
        run_command(log, "test-target", [tc.ninja, "-C", build, "-t", "query", "opencv_test_core"],
                    DRY_RUN_LIMIT, env, lane)
    limit = 21600 if name == "llvm-project" else 7200
    run_command(log, "build", [tc.ninja, "-C", build, "-j", jobs, *pin["l3_targets"]], limit, env, lane)
    result["build"] = "pass"
    l4 = pin["l4"]
    result["test"] = "fail"
    representative_test(name, log, build, prefix, tc, env, jobs, l4)
    result["test"] = "pass"
    result["manifest_l4"] = "fail"
    if l4.get("ninja"):
        run_command(log, "manifest-test", [tc.ninja, "-C", manifest_build, "-j", "1", *l4["ninja"]],
                    limit, env, lane)
    if l4.get("install") and name in {"duckdb", "opencv", "llvm-project"}:
        run_command(log, "manifest-install", [tc.ninja, "-C", manifest_build, "-j", "1", "install"],
                    limit, env, lane)
    result["manifest_l4"] = "pass"


def compare_lanes(report: Path) -> tuple[dict, str]:
    """Compare available L1 files and count the excess commands in each lane."""
    oracle, rcmake = report / "oracle", report / "rcmake"
    errors = set()
    for log in (oracle, rcmake):
        path = log / "l1-errors.json"
        if path.is_file():
            errors.update(json.loads(path.read_text()))
    l1 = {}
    for artifact in L1_FILES:
        left, right = oracle / artifact, rcmake / artifact
        if artifact in errors or not all(path.is_file() for path in (left, right)):
            l1[artifact] = "incomplete"
        else:
            l1[artifact] = "equal" if cmp(left, right, shallow=False) else "different"
    left, right = oracle / "commands.json", rcmake / "commands.json"
    if not all(path.is_file() for path in (left, right)):
        return l1, "incomplete"
    first, second = (Counter(json.loads(path.read_text())) for path in (left, right))
    write_json(report / "command-count-diff.json", {
        "oracle_only": dict(first - second), "rcmake_only": dict(second - first),
    })
    return l1, "equal" if first == second else "different"


def run_project(pin: dict, root: Path, engines: dict[str, Path], tc: Toolchain,
                args: argparse.Namespace) -> dict:
    """Run both engines serially using one source path and separate build state."""
    name = pin["name"]
    report = root / "reports" / name
    report.mkdir()
    work = root / name
    work.mkdir()
    source = work / "source"
    result = {"project": name, "mode": args.mode, "pin": pin, "status": "incomplete", "lanes": {}}
    try:
        budget = PROJECT_SPACE[name]
        if shutil.disk_usage(root).free < budget * GIB:
            raise ValueError(f"{name} requires {budget} GiB of free space at start.")
        directories = [tc.ninja.parent, tc.python.parent, "/opt/homebrew/bin",
                       "/usr/bin", "/bin", "/usr/sbin", "/sbin"]
        support = support_path(work, os.pathsep.join(map(str, directories)), BLOCKED_TOOLS)
        fetch_env = lane_environment(work / "fetch-state", support)
        fetch_env.update(GIT_TERMINAL_PROMPT="0", GIT_LFS_SKIP_SMUDGE="1", COPYFILE_DISABLE="1")
        revision = checkout(report, work, source, pin["repository"], pin["commit"], fetch_env,
                            sparse=(name == "llvm-project"))
        if revision != pin["commit"]:
            raise ValueError(f"Source commit {revision} differs from {pin['commit']}.")

        test_data = None
        if name == "opencv" and args.mode == "full":
            if override := os.environ.get("OPENCV_TEST_DATA_PATH"):
                test_data = Path(override).resolve(strict=True)
            else:
                extra = work / "opencv-extra"
                checkout(report, work, extra, "opencv/opencv_extra", "refs/tags/4.12.0",
                         fetch_env, label="test-data-")
                test_data = extra / "testdata"
            if not test_data.is_dir():
                raise ValueError(f"OpenCV test data directory is missing at {test_data}.")

        git = ["git", "-C", source]
        for engine in engines:
            log, lane = report / engine, work / engine
            log.mkdir()
            levels = ("path", "configure", "identity", "dry_run", "build", "test", "manifest_l4")
            outcome = dict.fromkeys(levels, "incomplete")
            if args.mode == "parity":
                outcome.update(build="not_requested", test="not_requested", manifest_l4="not_requested")
            result["lanes"][engine] = outcome
            try:
                run_lane(engine, engines, pin, lane, log, source, support, tc, args.mode,
                         test_data, outcome)
            except RUN_ERRORS as error:
                outcome["error"] = str(error)
                print(f"[{name} {engine}] {error}", file=sys.stderr, flush=True)
            finally:
                write_json(report / "result.json", result)
                if lane.is_dir():
                    try:
                        run_command(log, "source-status", [*git, "status", "--porcelain", "--ignored"],
                                    120, fetch_env, work)
                        run_command(log, "usage", ["/usr/bin/du", "-sk", lane, source],
                                    180, fetch_env, work)
                    except RUN_ERRORS as error:
                        outcome["usage_error"] = str(error)
                    if not args.keep_builds:
                        shutil.rmtree(lane)
            if engine == "oracle":
                run_command(report, "source-reset", [*git, "reset", "--hard", pin["commit"]],
                            300, fetch_env, work)
                run_command(report, "source-clean", [*git, "clean", "-ffdx"], 300, fetch_env, work)
        result["l1"], result["l2"] = compare_lanes(report)
        first, second = (result["lanes"][engine] for engine in ("oracle", "rcmake"))
        if "error" in first:
            result["status"] = "oracle_error"
        elif ("error" in second or result["l2"] != "equal"
              or any(status != "equal" for status in result["l1"].values())):
            result["status"] = "fail"
        else:
            result["status"] = "pass"
    except RUN_ERRORS as error:
        result["error"] = str(error)
        print(f"[{name}] {error}", file=sys.stderr, flush=True)
    finally:
        write_json(report / "result.json", result)
        if not args.keep_builds:
            shutil.rmtree(work)
    return result


def main(argv: list[str] | None = None) -> int:
    """Run the comparison and return a nonzero status for an unproven project."""
    args = parse_args(argv)
    run = args.run_dir.resolve()
    try:
        projects = load_projects(args.project)
        engines = installations(args.rcmake_stage, args.oracle)
        run.mkdir(parents=True, exist_ok=True)
        for pin in projects:
            for path in (run / pin["name"], run / "reports" / pin["name"]):
                if path.exists() or path.is_symlink():
                    raise ValueError(f"Use a fresh run directory. {path} already exists.")
        host = run / "reports/host"
        host.mkdir(parents=True)
        tc = preflight(host, engines)
        results = []
        for pin in projects:
            result = run_project(pin, run, engines, tc, args)
            results.append(result)
            write_json(run / "reports/result.json", {"projects": results})
            print(f"[{pin['name']}] {result['status']}", flush=True)
        return 0 if all(result["status"] == "pass" for result in results) else 1
    except RUN_ERRORS as error:
        print(f"The large-project suite could not complete. {error}", file=sys.stderr)
        return 2
    except KeyboardInterrupt:
        print(f"The large-project suite stopped. Reports are in {run / 'reports'}.", file=sys.stderr)
        return 130


if __name__ == "__main__":
    raise SystemExit(main())

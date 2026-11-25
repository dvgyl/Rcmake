#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Independent black-box differential verification suite.

The suite builds small C, C++, Objective-C++, Fortran, CMake, and Presets
projects in a sandbox under /tmp. It runs each scenario with the CMake 4.5.1
oracle and with a staged Rcmake tree. A scenario passes when every check
passes for each tool set and the normalized observations are equal.

Use --oracle-only to run the checks with the oracle only. This mode proves
that the suite itself is correct.

Scenarios s15 to s20 are in independent_scenarios_extra.py. This module
loads them into the same registry.

Examples:
  source env/env.sh
  python3 tools/measure/independent_verify.py --oracle "$RCMAKE_ORACLE_CMAKE" --oracle-only
  python3 tools/measure/independent_verify.py --stage target/stage --only s05,s10
"""

from __future__ import annotations

import argparse
import atexit
from dataclasses import dataclass
import gzip
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
import tarfile
import tempfile
import textwrap
import time
from typing import Any, Callable, Iterable, Mapping, Sequence
import xml.etree.ElementTree as ET
import zipfile


DARWIN = sys.platform == "darwin"
ROOT = Path(__file__).resolve().parents[2]
TOOLSET_DIRS = ("oracle", "rcmake")  # Equal length keeps path budgets equal.
DEFAULT_TIMEOUT = 600.0
# Environment variables that change CMake, CTest, CPack, or build tool behavior.
SCRUBBED_ENV = re.compile(
    r"^(CMAKE_[A-Z_]+|CTEST_[A-Z_]+|CPACK_[A-Z_]+|MAKEFLAGS|MFLAGS|MAKELEVEL|"
    r"NINJA_STATUS|DESTDIR|CC|CXX|OBJC|OBJCXX|FC|CFLAGS|CXXFLAGS|OBJCFLAGS|"
    r"OBJCXXFLAGS|FFLAGS|LDFLAGS|CPATH|LIBRARY_PATH|VERBOSE)$"
)
RSS_WRAPPER = (
    "import resource, subprocess, sys\n"
    "code = subprocess.run(sys.argv[1:]).returncode\n"
    "peak = resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss\n"
    "scale = 1 if sys.platform == 'darwin' else 1024\n"
    "print('INDEPENDENT_VERIFY_PEAK_RSS', peak * scale, file=sys.stderr)\n"
    "sys.exit(code)\n"
)
MIB = 1024 * 1024


class ScenarioSkip(Exception):
    """Raised when a scenario cannot run on this host."""


class StepFailed(Exception):
    """Raised when a scenario cannot continue after a failed check."""


# --------------------------------------------------------------------------
# Tool sets and processes


@dataclass(frozen=True)
class Toolset:
    """One CMake implementation: the cmake, ctest, and cpack programs."""

    label: str
    cmake: Path
    ctest: Path
    cpack: Path

    @classmethod
    def from_cmake(cls, label: str, cmake: Path) -> "Toolset":
        """Use the ctest and cpack programs next to a cmake program."""
        cmake = Path(cmake).absolute()
        return cls(label, cmake, cmake.with_name("ctest"), cmake.with_name("cpack"))

    @classmethod
    def from_stage(cls, stage: Path) -> "Toolset":
        """Use the bin/ programs of a staged Rcmake tree."""
        bindir = Path(stage).absolute() / "bin"
        return cls("rcmake", bindir / "cmake", bindir / "ctest", bindir / "cpack")

    def missing(self) -> list[str]:
        """Return the programs that do not exist or are not executable."""
        return [
            str(path) for path in (self.cmake, self.ctest, self.cpack)
            if not (path.is_file() and os.access(path, os.X_OK))
        ]


@dataclass
class Proc:
    """The result of one child process."""

    argv: list[str]
    cwd: str
    returncode: int
    stdout: str
    stderr: str
    seconds: float
    timed_out: bool = False

    @property
    def output(self) -> str:
        """Return stdout followed by stderr."""
        return self.stdout + self.stderr

    @property
    def ok(self) -> bool:
        """Return True when the process exited with status 0."""
        return self.returncode == 0 and not self.timed_out

    def tail(self, lines: int = 25) -> str:
        """Return the last lines of the output for a failure report."""
        text = self.output.rstrip().splitlines()[-lines:]
        cmd = " ".join(self.argv)
        return f"$ {cmd}\n[exit {self.returncode}{' timeout' if self.timed_out else ''}]\n" + "\n".join(text)


def child_env(extra: Mapping[str, str] | None = None) -> dict[str, str]:
    """Return a clean environment for a child process."""
    env = {key: value for key, value in os.environ.items() if not SCRUBBED_ENV.match(key)}
    env["COPYFILE_DISABLE"] = "1"
    env.setdefault("LC_ALL", "C")
    if extra:
        for key, value in extra.items():
            if value is None:
                env.pop(key, None)
            else:
                env[key] = value
    return env


def run_process(
    argv: Sequence[str | os.PathLike[str]],
    cwd: Path,
    env: Mapping[str, str],
    timeout: float,
    stdin: str | bytes | None = None,
) -> Proc:
    """Run a process with a timeout. Kill its process group on timeout."""
    args = [os.fspath(arg) for arg in argv]
    start = time.monotonic()
    data = stdin.encode() if isinstance(stdin, str) else stdin
    try:
        child = subprocess.Popen(
            args, cwd=cwd, env=dict(env), start_new_session=True,
            stdin=subprocess.PIPE if data is not None else subprocess.DEVNULL,
            stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        )
    except OSError as exc:
        return Proc(args, str(cwd), 127, "", f"cannot start: {exc}", 0.0)
    try:
        out, err = child.communicate(data, timeout=timeout)
        timed_out = False
    except subprocess.TimeoutExpired:
        kill_group(child)
        out, err = child.communicate()
        timed_out = True
    seconds = time.monotonic() - start
    return Proc(
        args, str(cwd), child.returncode if not timed_out else -9,
        out.decode("utf-8", "replace"), err.decode("utf-8", "replace"), seconds, timed_out,
    )


def kill_group(child: subprocess.Popen[bytes]) -> None:
    """Kill a child and every process in its session."""
    try:
        os.killpg(child.pid, signal.SIGKILL)
    except (ProcessLookupError, PermissionError):
        child.kill()


def peak_rss(proc: Proc) -> int | None:
    """Read the peak resident set size that RSS_WRAPPER prints."""
    found = re.findall(r"INDEPENDENT_VERIFY_PEAK_RSS (\d+)", proc.stderr)
    return int(found[-1]) if found else None


# --------------------------------------------------------------------------
# Sandbox


class Sandbox:
    """A temporary directory tree under /tmp. It is removed on exit."""

    def __init__(self, base: str = "/tmp", keep: bool = False) -> None:
        base_path = Path(base)
        base_path.mkdir(parents=True, exist_ok=True)
        # Use the real path. On macOS /tmp is a symbolic link to /private/tmp.
        self.path = Path(os.path.realpath(tempfile.mkdtemp(prefix="rcmake-indep-", dir=base_path)))
        self.keep = keep
        self._removed = False
        atexit.register(self.cleanup)

    def cleanup(self) -> None:
        """Remove the sandbox unless the user asked to keep it."""
        if self._removed or self.keep:
            return
        self._removed = True
        remove_tree(self.path)

    def __enter__(self) -> "Sandbox":
        return self

    def __exit__(self, *exc: object) -> None:
        self.cleanup()


def remove_tree(path: Path) -> None:
    """Remove a directory tree. Make read-only directories writable first.

    Symbolic links are not followed, so no file outside the tree changes.
    """
    if path.is_symlink() or path.is_file():
        path.unlink(missing_ok=True)
        return
    if not path.exists():
        return
    try:
        os.chmod(path, 0o700)
    except OSError:
        pass
    for dirpath, dirnames, _files in os.walk(path):
        for name in dirnames:
            child = os.path.join(dirpath, name)
            if not os.path.islink(child):
                try:
                    os.chmod(child, 0o700)
                except OSError:
                    pass
    shutil.rmtree(path, ignore_errors=True)


def install_signal_handlers() -> None:
    """Turn SIGTERM and SIGHUP into SystemExit so that atexit cleanup runs."""
    def handler(signum: int, _frame: Any) -> None:
        raise SystemExit(128 + signum)
    for sig in (signal.SIGTERM, signal.SIGHUP):
        signal.signal(sig, handler)


# --------------------------------------------------------------------------
# Scenario context


@dataclass
class Check:
    """One assertion result."""

    name: str
    ok: bool
    detail: str = ""


@dataclass
class Options:
    """Settings that every scenario can read."""

    jobs: int = 4
    timeout: float = DEFAULT_TIMEOUT
    xcode: bool = True
    archive_mib: int = 520
    keep: bool = False


class Ctx:
    """The state of one scenario run with one tool set."""

    def __init__(self, tools: Toolset, root: Path, options: Options) -> None:
        self.tools = tools
        self.root = root
        self.options = options
        self.checks: list[Check] = []
        self.skips: list[str] = []
        self.observations: dict[str, Any] = {}
        self.transcript: list[str] = []
        self.aliases: list[tuple[str, str]] = []
        root.mkdir(parents=True, exist_ok=True)

    # Files ---------------------------------------------------------------

    def write(self, rel: str | Path, text: str, base: Path | None = None) -> Path:
        """Write dedented text to a file below the root and return its path."""
        path = (base or self.root) / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        content = textwrap.dedent(text)
        if content.startswith("\n"):
            content = content[1:]
        path.write_text(content, encoding="utf-8")
        return path

    def tree(self, base: Path, files: Mapping[str, str]) -> Path:
        """Write a mapping of relative paths to dedented text below base."""
        for rel, text in files.items():
            self.write(rel, text, base)
        return base

    # Processes -----------------------------------------------------------

    def run(
        self, argv: Sequence[str | os.PathLike[str]], cwd: Path | None = None,
        env: Mapping[str, str | None] | None = None, timeout: float | None = None,
        stdin: str | bytes | None = None,
    ) -> Proc:
        """Run a process with the clean environment."""
        proc = run_process(
            argv, cwd or self.root, child_env(env), timeout or self.options.timeout, stdin,
        )
        self.transcript.append(f"[{proc.returncode}] {' '.join(proc.argv)} ({proc.seconds:.2f}s)")
        return proc

    def cmake(self, *args: str | os.PathLike[str], **kw: Any) -> Proc:
        """Run the cmake program of this tool set."""
        return self.run([self.tools.cmake, *args], **kw)

    def ctest(self, *args: str | os.PathLike[str], **kw: Any) -> Proc:
        """Run the ctest program of this tool set."""
        return self.run([self.tools.ctest, *args], **kw)

    def cpack(self, *args: str | os.PathLike[str], **kw: Any) -> Proc:
        """Run the cpack program of this tool set."""
        return self.run([self.tools.cpack, *args], **kw)

    def configure(self, src: Path, build: Path, generator: str, *args: str, **kw: Any) -> Proc:
        """Configure a project with a generator."""
        extra = list(args)
        if generator in SINGLE_CONFIG and not any(a.startswith("-DCMAKE_BUILD_TYPE") for a in extra):
            extra.append("-DCMAKE_BUILD_TYPE=Release")
        return self.cmake("-S", src, "-B", build, "-G", generator, *extra, **kw)

    def build(self, build: Path, generator: str, *args: str, config: str = "Release", **kw: Any) -> Proc:
        """Build a tree with the configured parallel job count."""
        extra = ["--config", config] if generator in MULTI_CONFIG else []
        return self.cmake("--build", build, "-j", str(self.options.jobs), *extra, *args, **kw)

    # Results -------------------------------------------------------------

    def check(self, name: str, ok: bool, detail: Any = "") -> bool:
        """Record one assertion."""
        self.checks.append(Check(name, bool(ok), "" if ok else str(detail)[:4000]))
        return bool(ok)

    def require(self, name: str, ok: bool, detail: Any = "") -> None:
        """Record one assertion. Stop the scenario when it fails."""
        if not self.check(name, ok, detail):
            raise StepFailed(name)

    def ok(self, name: str, proc: Proc, required: bool = True) -> bool:
        """Assert that a process exited with status 0."""
        if required:
            self.require(name, proc.ok, proc.tail())
            return True
        return self.check(name, proc.ok, proc.tail())

    def fails(self, name: str, proc: Proc) -> bool:
        """Assert that a process exited with a nonzero status."""
        return self.check(name, proc.returncode != 0 and not proc.timed_out, proc.tail())

    def skip(self, reason: str) -> None:
        """Record a sub-case that cannot run on this host."""
        self.skips.append(reason)

    def observe(self, key: str, value: Any) -> None:
        """Record a normalized value for the differential comparison."""
        self.observations[key] = self.normalize(value)

    def learn_tool_paths(self, build: Path) -> None:
        """Read the tool paths that a configured tree records and alias them.

        A tool can resolve symbolic links in its own path. The cache then
        holds a path that is not the path of the Toolset. The aliases give
        both paths the same placeholder.
        """
        entries = cache_entries(build, TOOL_PATH_ENTRIES)
        for name, placeholder in TOOL_PATH_ENTRIES.items():
            value = entries.get(name, "").partition("=")[2]
            if value and (value, placeholder) not in self.aliases:
                self.aliases.append((value, placeholder))

    def normalize(self, value: Any) -> Any:
        """Replace host-specific paths in nested values."""
        if isinstance(value, str):
            return normalize_text(value, self.root, self.tools, getattr(self, "aliases", ()))
        if isinstance(value, Mapping):
            return {self.normalize(k): self.normalize(v) for k, v in value.items()}
        if isinstance(value, (list, tuple)):
            return [self.normalize(item) for item in value]
        return value


SINGLE_CONFIG = ("Ninja", "Unix Makefiles")
MULTI_CONFIG = ("Ninja Multi-Config", "Xcode")
# Cache entries that hold tool paths, and the placeholder of each path.
TOOL_PATH_ENTRIES = {
    "CMAKE_COMMAND": "<CMAKE>",
    "CMAKE_CTEST_COMMAND": "<CTEST>",
    "CMAKE_CPACK_COMMAND": "<CPACK>",
    "CMAKE_ROOT": "<CMAKE_ROOT>",
}


def normalize_text(
    text: str, root: Path, tools: Toolset | None = None, aliases: Iterable[tuple[str, str]] = (),
) -> str:
    """Replace the run root, tool paths, and aliases with stable placeholders."""
    pairs: list[tuple[str, str]] = list(aliases)
    for candidate in {str(root), os.path.realpath(root)}:
        pairs.append((candidate, "<ROOT>"))
    if tools is not None:
        for path in (tools.cmake, tools.ctest, tools.cpack):
            pairs.append((str(path), f"<{path.name.upper()}>"))
    for old, new in sorted(pairs, key=lambda pair: -len(pair[0])):
        text = text.replace(old, new)
    text = re.sub(r"\(\d+(?:\.\d+)?s\)", "(<T>s)", text)
    text = re.sub(r"\d+\.\d+ sec", "<T> sec", text)
    return text.replace("\r\n", "\n")


def generators_available(options: Options) -> list[str]:
    """Return the CMake generators that this host can run."""
    found = []
    if shutil.which("ninja"):
        found += ["Ninja", "Ninja Multi-Config"]
    if shutil.which("make"):
        found.append("Unix Makefiles")
    if DARWIN and options.xcode and shutil.which("xcodebuild"):
        probe = subprocess.run(["xcodebuild", "-version"], capture_output=True, timeout=60)
        if probe.returncode == 0:
            found.append("Xcode")
    return found


def slug(text: str) -> str:
    """Make a short file-name-safe name."""
    return re.sub(r"[^A-Za-z0-9]+", "-", text).strip("-").lower()


def exe_path(build: Path, name: str, generator: str, config: str = "Release", subdir: str = "") -> Path:
    """Return the path of a built executable for a generator layout."""
    base = build / subdir if subdir else build
    if generator in MULTI_CONFIG:
        return base / config / name
    return base / name


def file_listing(top: Path) -> list[str]:
    """List the files and links below a directory as sorted relative paths."""
    items = []
    for path in sorted(top.rglob("*")):
        if path.name.startswith("._") or path.name == ".DS_Store":
            continue
        rel = path.relative_to(top).as_posix()
        if path.is_symlink():
            items.append(f"{rel} -> {os.readlink(path)}")
        elif path.is_file():
            items.append(rel)
    return items


def cache_entries(build: Path, names: Iterable[str]) -> dict[str, str]:
    """Read selected entries from CMakeCache.txt."""
    wanted = set(names)
    found: dict[str, str] = {}
    cache = build / "CMakeCache.txt"
    if not cache.is_file():
        return found
    for line in cache.read_text(encoding="utf-8", errors="replace").splitlines():
        match = re.match(r"^([^#/][^:=]*):([A-Z]+)=(.*)$", line)
        if match and match.group(1) in wanted:
            found[match.group(1)] = f"{match.group(2)}={match.group(3)}"
    return found


def touch_later(path: Path, text: str | None = None) -> None:
    """Rewrite a file so that its time stamp is newer than every build output.

    GNU Make 3.81 compares time stamps with one second resolution. The wait
    makes sure that each edit is seen as newer than the previous outputs.
    """
    time.sleep(1.1)
    if text is not None:
        path.write_text(text, encoding="utf-8")
    else:
        path.touch()


def no_op(proc: Proc) -> bool:
    """Return True when a build did not compile or link anything."""
    return proc.ok and not re.search(r"Building \w+ object|Linking |Generating |Building \w+ precompiled", proc.output)


def compiled(proc: Proc, pattern: str) -> bool:
    """Return True when a build output shows that it compiled a matching object."""
    return bool(re.search(r"Building \w+ object \S*" + pattern, proc.output))


# --------------------------------------------------------------------------
# Scenario registry


@dataclass
class Scenario:
    """A named scenario function."""

    sid: str
    title: str
    func: Callable[[Ctx], None]
    requires: Callable[[Options], str | None] | None = None


SCENARIOS: list[Scenario] = []


def scenario(sid: str, title: str, requires: Callable[[Options], str | None] | None = None):
    """Register a scenario function."""
    def wrap(func: Callable[[Ctx], None]) -> Callable[[Ctx], None]:
        SCENARIOS.append(Scenario(sid, title, func, requires))
        SCENARIOS.sort(key=lambda sc: sc.sid)
        return func
    return wrap


def need_darwin(_options: Options) -> str | None:
    """Skip a scenario when the host is not macOS."""
    return None if DARWIN else "needs macOS"


# --------------------------------------------------------------------------
# s05: deep diamond usage requirements


DIAMOND_LEVELS = 20
DIAMOND_WIDTH = ("a", "b", "c")


@scenario("s05", "Deep diamond $<TARGET_PROPERTY:...,INCLUDE_DIRECTORIES> scaling (3x20 targets, 3^20 paths)")
def s05_diamond(ctx: Ctx) -> None:
    src = ctx.root / "src"
    ctx.write("src/CMakeLists.txt", f"""
        cmake_minimum_required(VERSION 4.0)
        project(diamond C)
        set(prev "")
        foreach(level RANGE 1 {DIAMOND_LEVELS})
          set(cur "")
          foreach(k {' '.join(DIAMOND_WIDTH)})
            add_library(L${{level}}${{k}} INTERFACE)
            target_include_directories(L${{level}}${{k}} INTERFACE "${{CMAKE_SOURCE_DIR}}/inc/L${{level}}${{k}}")
            target_compile_definitions(L${{level}}${{k}} INTERFACE "D_L${{level}}${{k}}=${{level}}")
            if(prev)
              target_link_libraries(L${{level}}${{k}} INTERFACE ${{prev}})
            endif()
            list(APPEND cur L${{level}}${{k}})
          endforeach()
          set(prev ${{cur}})
        endforeach()
        add_library(top INTERFACE)
        target_link_libraries(top INTERFACE ${{prev}})
        add_executable(app main.c)
        target_link_libraries(app PRIVATE top)
        file(GENERATE OUTPUT incs.txt CONTENT "$<TARGET_PROPERTY:app,INCLUDE_DIRECTORIES>\\n")
        file(GENERATE OUTPUT defs.txt CONTENT "$<TARGET_PROPERTY:app,COMPILE_DEFINITIONS>\\n")
        file(GENERATE OUTPUT iface.txt CONTENT "$<TARGET_PROPERTY:top,INTERFACE_INCLUDE_DIRECTORIES>\\n")
        """)
    for level in range(1, DIAMOND_LEVELS + 1):
        for k in DIAMOND_WIDTH:
            ctx.write(f"src/inc/L{level}{k}/L{level}{k}.h", f"#define VALUE_L{level}{k} {level}\n")
    ctx.write("src/main.c", """
        #include <stdio.h>
        #include "L1a.h"
        #include "L20c.h"
        int main(void) {
          printf("diamond %d %d %d\\n", VALUE_L1a + VALUE_L20c, D_L1b, D_L20a);
          return 0;
        }
        """)
    build = ctx.root / "build"
    cold = ctx.configure(src, build, "Ninja")
    ctx.ok("cold configure", cold)
    ctx.check("cold configure < 3.0s", cold.seconds < 3.0, f"{cold.seconds:.2f}s")
    warm = ctx.cmake(build)
    ctx.ok("warm reconfigure", warm)
    ctx.check("warm reconfigure < 3.0s", warm.seconds < 3.0, f"{warm.seconds:.2f}s")
    expected = {f"{src}/inc/L{level}{k}" for level in range(1, DIAMOND_LEVELS + 1) for k in DIAMOND_WIDTH}
    for name in ("incs.txt", "iface.txt"):
        items = (build / name).read_text().strip().split(";")
        ctx.check(f"{name} lists each of the 60 directories once",
                  len(items) == 60 and set(items) == expected, f"{len(items)} items")
        ctx.observe(name, (build / name).read_text())
    defs = (build / "defs.txt").read_text().strip().split(";")
    ctx.check("defs.txt lists 60 definitions", len(set(defs)) == 60 == len(defs), len(defs))
    ctx.observe("defs.txt", (build / "defs.txt").read_text())
    ctx.ok("build", ctx.build(build, "Ninja"))
    run = ctx.run([build / "app"])
    ctx.check("app output", run.ok and run.stdout == "diamond 21 1 20\n", run.tail())
    ctx.observe("app", run.stdout)


# --------------------------------------------------------------------------
# s06: find_library, find_package, FeatureSummary


@scenario("s06", "find_library lib64 + /* wildcard HINTS/PATHS + FeatureSummary + find_package literal ENV")
def s06_find(ctx: Ctx) -> None:
    r = ctx.root
    for rel in ("pfx/p64/lib64/libsixty.a", "pfx/plib/lib/libplain.a",
                "opt/vendorB-2.0/lib/libwild.a", "opt/vendorA-1.0/lib/libwildhint.a"):
        ctx.write(rel, "!<arch>\n")
    ctx.write("opt/vendorB-2.0/lib/cmake/Wid/WidConfig.cmake", "set(Wid_MARK wildcard)\n")
    ctx.write("envroot/lib/cmake/Gadget/GadgetConfig.cmake", "set(Gadget_MARK expanded-env)\n")
    ctx.write("src/ENV/GadgetConfig.cmake", "set(Gadget_MARK literal-env)\n")
    ctx.write("env dir/share/Thing/ThingConfig.cmake", "set(Thing_MARK spaced)\n")
    variables = ("SIXTY_LIB", "SIXTY_PFX", "SIXTY_OFF", "PLAIN_LIB", "WILD_LIB", "WILDHINT_LIB",
                 "WILDTOP_LIB", "Gadget_DIR", "Thing_DIR", "Thing2_DIR", "Missing_DIR", "Wid_DIR",
                 "Gadget_MARK", "Thing_MARK", "Wid_MARK", "Gadget_FOUND", "Thing_FOUND")
    ctx.write("src/CMakeLists.txt", f"""
        cmake_minimum_required(VERSION 4.0)
        project(finds C)
        set_property(GLOBAL PROPERTY FIND_LIBRARY_USE_LIB64_PATHS TRUE)
        find_library(SIXTY_LIB sixty PATHS "${{ROOTX}}/pfx/p64/lib" NO_DEFAULT_PATH)
        find_library(SIXTY_PFX sixty)
        find_library(PLAIN_LIB plain PATHS "${{ROOTX}}/pfx/plib/lib" NO_DEFAULT_PATH)
        set_property(GLOBAL PROPERTY FIND_LIBRARY_USE_LIB64_PATHS FALSE)
        find_library(SIXTY_OFF sixty PATHS "${{ROOTX}}/pfx/p64/lib" NO_DEFAULT_PATH)
        find_library(WILD_LIB wild PATHS "${{ROOTX}}/opt/*/lib" NO_DEFAULT_PATH)
        find_library(WILDHINT_LIB wildhint HINTS "${{ROOTX}}/opt/*/lib" NO_DEFAULT_PATH)
        find_library(WILDTOP_LIB wild HINTS "${{ROOTX}}/opt/*" PATH_SUFFIXES lib NO_DEFAULT_PATH)
        # In find_package, ENV is not a keyword. It is the relative path "ENV".
        find_package(Gadget CONFIG PATHS ENV GADGET_PREFIX NO_DEFAULT_PATH)
        find_package(Thing CONFIG HINTS ENV THING_PREFIX NO_DEFAULT_PATH)
        find_package(Thing2 NAMES Thing CONFIG HINTS "$ENV{{THING_PREFIX}}" NO_DEFAULT_PATH)
        find_package(Missing CONFIG QUIET PATHS ENV NOT_SET_VAR NO_DEFAULT_PATH)
        find_package(Wid CONFIG QUIET PATHS "${{ROOTX}}/opt/*" NO_DEFAULT_PATH)
        include(FeatureSummary)
        set_package_properties(Gadget PROPERTIES DESCRIPTION "Gadget library"
          URL "https://example.invalid/gadget" TYPE REQUIRED PURPOSE "Gadget support")
        set_package_properties(Thing PROPERTIES DESCRIPTION "Thing library" TYPE RECOMMENDED)
        set_package_properties(Missing PROPERTIES DESCRIPTION "Missing thing" TYPE OPTIONAL)
        option(WITH_FAST "fast" ON)
        add_feature_info(Fast WITH_FAST "Fast mode")
        add_feature_info(Slow "NOT WITH_FAST" "Slow mode")
        feature_summary(WHAT ALL)
        feature_summary(WHAT ENABLED_FEATURES DISABLED_FEATURES PACKAGES_FOUND PACKAGES_NOT_FOUND
          FILENAME "${{CMAKE_BINARY_DIR}}/features.txt")
        foreach(v {' '.join(variables)})
          message(STATUS "RESULT ${{v}}=${{${{v}}}}")
        endforeach()
        """)
    env = {"GADGET_PREFIX": str(r / "envroot"), "THING_PREFIX": str(r / "env dir")}
    build = r / "build"
    proc = ctx.configure(r / "src", build, "Ninja", f"-DROOTX={r}", f"-DCMAKE_PREFIX_PATH={r}/pfx/p64", env=env)
    ctx.ok("configure", proc)
    results = dict(re.findall(r"(?m)^-- RESULT (\w+)=(.*)$", proc.stdout))
    expect = {
        "SIXTY_LIB": f"{r}/pfx/p64/lib64/libsixty.a",
        "SIXTY_PFX": f"{r}/pfx/p64/lib64/libsixty.a",
        "SIXTY_OFF": "SIXTY_OFF-NOTFOUND",
        "PLAIN_LIB": f"{r}/pfx/plib/lib/libplain.a",
        "WILD_LIB": f"{r}/opt/vendorB-2.0/lib/libwild.a",
        "WILDHINT_LIB": f"{r}/opt/vendorA-1.0/lib/libwildhint.a",
        "WILDTOP_LIB": f"{r}/opt/vendorB-2.0/lib/libwild.a",
        "Gadget_DIR": f"{r}/src/ENV",
        "Gadget_MARK": "literal-env",
        "Thing_DIR": "Thing_DIR-NOTFOUND",
        "Thing2_DIR": f"{r}/env dir/share/Thing",
        "Thing_MARK": "spaced",
        "Missing_DIR": "Missing_DIR-NOTFOUND",
        "Wid_DIR": f"{r}/opt/vendorB-2.0/lib/cmake/Wid",
        "Wid_MARK": "wildcard",
    }
    for key, value in expect.items():
        ctx.check(f"{key} result", results.get(key) == value, f"got {results.get(key)!r}, want {value!r}")
    summary = proc.stdout.split("-- RESULT", 1)[0]
    summary = "\n".join(line for line in summary.splitlines()
                        if not re.match(r"-- (The C compiler|Detecting|Check for working|Found)", line))
    ctx.check("summary lists the REQUIRED package as found",
              "REQUIRED packages have been found:\n\n * Gadget, Gadget library" in summary, summary)
    ctx.check("summary lists the RECOMMENDED package as not found",
              "RECOMMENDED packages have not been found:\n\n * Thing, Thing library" in summary, summary)
    ctx.check("summary lists the disabled feature", " * Slow, Slow mode" in summary, summary)
    ctx.observe("results", results)
    ctx.observe("summary", summary)
    ctx.observe("features.txt", (build / "features.txt").read_text())
    ctx.observe("cache", cache_entries(build, variables + ("WITH_FAST",)))
    cache = cache_entries(build, ("SIXTY_LIB", "Gadget_DIR"))
    ctx.check("cache SIXTY_LIB is FILEPATH", cache.get("SIXTY_LIB") == f"FILEPATH={expect['SIXTY_LIB']}", cache)
    ctx.check("cache Gadget_DIR is PATH", cache.get("Gadget_DIR") == f"PATH={expect['Gadget_DIR']}", cache)


# --------------------------------------------------------------------------
# s09: file(INSTALL), file(DOWNLOAD), file(RELATIVE_PATH), CPS


@scenario("s09", "file(INSTALL) lib64->lib symlink + file:// URL with spaces + RELATIVE_PATH case + CPS cxx_std_20")
def s09_files(ctx: Ctx) -> None:
    r = ctx.root
    ctx.write("tree/lib/libx.txt", "lib payload\n")
    (r / "tree/lib64").symlink_to("lib")
    ctx.write("dl dir/data file.txt", "payload\n")
    sha = hashlib.sha256(b"payload\n").hexdigest()
    ctx.write("t.cmake", f"""
        file(INSTALL "${{R}}/tree/lib" "${{R}}/tree/lib64" DESTINATION "${{R}}/out1")
        file(INSTALL "${{R}}/tree" DESTINATION "${{R}}/out2")
        file(INSTALL FILES "${{R}}/tree/lib64" DESTINATION "${{R}}/out3" TYPE DIRECTORY)
        foreach(d out1/lib64 out2/tree/lib64 out3/lib64 out2/tree/lib/libx.txt)
          if(IS_SYMLINK "${{R}}/${{d}}")
            file(READ_SYMLINK "${{R}}/${{d}}" t)
            message("KIND ${{d}} symlink ${{t}}")
          elseif(IS_DIRECTORY "${{R}}/${{d}}")
            message("KIND ${{d}} directory")
          elseif(EXISTS "${{R}}/${{d}}")
            message("KIND ${{d}} file")
          else()
            message("KIND ${{d}} missing")
          endif()
        endforeach()
        file(DOWNLOAD "file://${{R}}/dl%20dir/data%20file.txt" "${{R}}/got1.txt" STATUS s1
             EXPECTED_HASH SHA256={sha})
        list(GET s1 0 c1)
        message("DL encoded ${{c1}}")
        file(DOWNLOAD "file://${{R}}/dl dir/data file.txt" "${{R}}/got2.txt" STATUS s2)
        list(GET s2 0 c2)
        message("DL raw ${{c2}}")
        file(DOWNLOAD "file://${{R}}/dl%20dir/absent.txt" "${{R}}/got3.txt" STATUS s3)
        list(GET s3 0 c3)
        message("DL absent-nonzero ${{c3}}")
        file(RELATIVE_PATH r1 "/Data/A/B" "/data/a/b/c/d.txt")
        file(RELATIVE_PATH r2 "/Data/A/B" "/Data/A/B/c/d.txt")
        file(RELATIVE_PATH r3 "/x/Y" "/x/y")
        file(RELATIVE_PATH r4 "/x/y/" "/x/y/z/")
        message("REL r1=[${{r1}}] r2=[${{r2}}] r3=[${{r3}}] r4=[${{r4}}]")
        """)
    proc = ctx.cmake(f"-DR={r}", "-P", r / "t.cmake")
    ctx.ok("cmake -P", proc)
    out = proc.stderr
    for d in ("out1/lib64", "out2/tree/lib64", "out3/lib64"):
        ctx.check(f"{d} stays a symlink to lib", f"KIND {d} symlink lib" in out, out)
    ctx.check("installed file content", (r / "out2/tree/lib/libx.txt").read_text() == "lib payload\n")
    ctx.check("file:// %20 download", "DL encoded 0" in out and (r / "got1.txt").read_text() == "payload\n", out)
    ctx.check("file:// raw space download", "DL raw 0" in out and (r / "got2.txt").read_text() == "payload\n", out)
    ctx.check("file:// missing file fails", "DL absent-nonzero 0" not in out, out)
    # macOS (like Windows) compares paths without case in RELATIVE_PATH.
    if DARWIN:
        rel = "REL r1=[c/d.txt] r2=[c/d.txt] r3=[] r4=[z]"
    else:
        rel = "REL r1=[../../../data/a/b/c/d.txt] r2=[c/d.txt] r3=[../y] r4=[z]"
    ctx.check("RELATIVE_PATH case rule", rel in out, out)
    installs = sorted(line for line in proc.stdout.splitlines() if line.startswith("-- Installing"))
    ctx.observe("installing", installs)
    ctx.observe("messages", [line for line in out.splitlines() if line.split(" ")[0] in ("KIND", "DL", "REL")])
    ctx.observe("out2", file_listing(r / "out2"))

    # CPS package with compile_features c++20 that a C++ consumer uses.
    ctx.write("pfx/include/widget.h", """
        #if __cplusplus < 202002L
        #error "widget needs C++20"
        #endif
        consteval int widget_value() { return 20; }
        """)
    ctx.write("pfx/lib/cps/widget/widget.cps", """
        {
          "cps_version": "0.13.0",
          "name": "widget",
          "version": "2.1.0",
          "compat_version": "2.0.0",
          "cps_path": "@prefix@/lib/cps/widget",
          "default_components": ["core"],
          "components": {
            "core": {
              "type": "interface",
              "compile_features": ["c++20"],
              "definitions": {"*": {"WIDGET_CORE": "1"}},
              "includes": ["@prefix@/include"]
            },
            "extra": {"type": "interface", "requires": [":core"], "compile_features": ["threads"]}
          }
        }
        """)
    ctx.write("cps/CMakeLists.txt", """
        cmake_minimum_required(VERSION 4.0)
        project(cpsuse CXX)
        find_package(widget 2.0 REQUIRED)
        foreach(t widget::core widget::extra)
          foreach(p INTERFACE_COMPILE_FEATURES INTERFACE_COMPILE_DEFINITIONS INTERFACE_LINK_LIBRARIES)
            get_target_property(v ${t} ${p})
            message(STATUS "PROP ${t} ${p}=${v}")
          endforeach()
        endforeach()
        message(STATUS "PROP widget_VERSION=${widget_VERSION}")
        add_executable(use use.cpp)
        target_link_libraries(use PRIVATE widget::core)
        """)
    ctx.write("cps/use.cpp", """
        #include <cstdio>
        #include "widget.h"
        int main() { std::printf("widget %d %d\\n", widget_value(), WIDGET_CORE); return 0; }
        """)
    build = r / "cps-build"
    proc = ctx.configure(r / "cps", build, "Ninja", f"-DCMAKE_PREFIX_PATH={r}/pfx")
    ctx.ok("CPS configure", proc)
    props = [line for line in proc.stdout.splitlines() if line.startswith("-- PROP")]
    ctx.check("CPS c++20 maps to cxx_std_20",
              "-- PROP widget::core INTERFACE_COMPILE_FEATURES=cxx_std_20" in props, props)
    ctx.check("CPS threads maps to Threads::Threads",
              "-- PROP widget::extra INTERFACE_LINK_LIBRARIES=Threads::Threads;widget::core" in props, props)
    ctx.observe("cps properties", props)
    ctx.ok("CPS consumer build", ctx.build(build, "Ninja"))
    run = ctx.run([build / "use"])
    ctx.check("CPS consumer output", run.stdout == "widget 20 1\n", run.tail())


# --------------------------------------------------------------------------
# s10: Presets v13 and command line matrix


PRESETS_V13 = """
{
  "version": 13,
  "cmakeMinimumRequired": {"major": 4, "minor": 5, "patch": 0},
  "configurePresets": [
    {"name": "base", "hidden": true, "generator": "Ninja", "binaryDir": "${sourceDir}/out/${presetName}",
     "cacheVariables": {"CMAKE_BUILD_TYPE": "Release", "SEEN_PRESET": "${configurePresetName}"}},
    {"name": "dev", "inherits": "base"},
    {"name": "off", "inherits": "base", "condition": false}
  ],
  "buildPresets": [
    {"name": "dev", "configurePreset": "dev"},
    {"name": "dry", "configurePreset": "dev", "nativeToolOptions": ["-n"]}
  ],
  "testPresets": [
    {"name": "dev", "configurePreset": "dev", "environment": {"CFG_NAME": "${configurePresetName}"},
     "output": {"outputOnFailure": true}}
  ],
  "packagePresets": [
    {"name": "dev", "configurePreset": "dev", "generators": ["TGZ"]}
  ],
  "workflowPresets": [
    {"name": "dev", "steps": [
      {"type": "configure", "name": "dev"}, {"type": "build", "name": "dev"},
      {"type": "test", "name": "dev"}, {"type": "package", "name": "dev"}]}
  ]
}
"""


def sarif_digest(path: Path) -> list[dict[str, Any]]:
    """Reduce a SARIF log to levels, messages, locations, and stack frames."""
    data = json.loads(path.read_text(encoding="utf-8"))
    digest = []
    for run in data.get("runs", []):
        for result in run.get("results", []):
            def where(location: Mapping[str, Any]) -> str:
                phys = location.get("physicalLocation", {})
                names = [item.get("name") for item in location.get("logicalLocations", [])]
                return (f"{phys.get('artifactLocation', {}).get('uriBaseId', '')}:"
                        f"{phys.get('artifactLocation', {}).get('uri', '')}:"
                        f"{phys.get('region', {}).get('startLine', '')}:{','.join(map(str, names))}")
            digest.append({
                "ruleId": result.get("ruleId"),
                "level": result.get("level"),
                "message": result.get("message", {}).get("text"),
                "locations": [where(loc) for loc in result.get("locations", [])],
                "stacks": [[where(frame.get("location", {})) for frame in stack.get("frames", [])]
                           for stack in result.get("stacks", [])],
            })
    return digest


@scenario("s10", "Presets v13 workflow + CLI matrix (disabled preset, nativeToolOptions, -C/-D/-U, --fresh, SARIF, -E cat)")
def s10_presets_cli(ctx: Ctx) -> None:
    r = ctx.root
    src = r / "src"
    ctx.tree(src, {
        "CMakeLists.txt": """
            cmake_minimum_required(VERSION 4.0)
            project(cli C)
            enable_testing()
            include(cmake/warn.cmake)
            add_executable(a a.c)
            add_executable(b b.c)
            add_executable(c c.c)
            add_subdirectory(sub)
            install(TARGETS a COMPONENT A DESTINATION bin)
            install(TARGETS b COMPONENT B DESTINATION bin)
            install(TARGETS c COMPONENT C DESTINATION bin)
            add_test(NAME ta COMMAND a)
            add_test(NAME envcheck COMMAND ${CMAKE_COMMAND} -P ${CMAKE_SOURCE_DIR}/check_env.cmake)
            message(STATUS "VALUES PRE=${PRE} OVR=${OVR} VAR_A=${VAR_A} VAR_B=${VAR_B} VAR_C=${VAR_C}")
            set(CPACK_GENERATOR TGZ)
            set(CPACK_PACKAGE_FILE_NAME cli-pkg)
            include(CPack)
            """,
        "cmake/warn.cmake": """
            function(emit_warning)
              message(WARNING "deliberate warning")
            endfunction()
            macro(outer)
              emit_warning()
            endmacro()
            outer()
            """,
        "check_env.cmake": """
            if(NOT "$ENV{CFG_NAME}" STREQUAL "dev")
              message(FATAL_ERROR "CFG_NAME=[$ENV{CFG_NAME}]")
            endif()
            """,
        "init.cmake": """
            set(PRE "from-preload" CACHE STRING "")
            set(OVR "from-preload" CACHE STRING "")
            """,
        "sub/CMakeLists.txt": "add_executable(s s.c)\n",
        "sub/s.c": "int main(void) { return 0; }\n",
        "a.c": "int main(void) { return 0; }\n",
        "b.c": "int main(void) { return 0; }\n",
        "c.c": "int main(void) { return 0; }\n",
        "CMakePresets.json": PRESETS_V13,
    })
    out = src / "out"

    # Workflow: configure, build, test, package.
    wf = ctx.cmake("--workflow", "--preset", "dev", cwd=src)
    ctx.ok("workflow preset", wf)
    steps = re.findall(r"(?m)^Executing workflow step .*$", wf.output)
    ctx.check("workflow ran 4 steps", len(steps) == 4, steps)
    ctx.check("workflow tests passed", "100% tests passed out of 2" in wf.output, wf.tail())
    ctx.check("workflow package", (out / "dev/cli-pkg.tar.gz").is_file())
    ctx.check("${configurePresetName} in cache",
              cache_entries(out / "dev", ["SEEN_PRESET"]).get("SEEN_PRESET", "").endswith("=dev"))
    ctx.observe("workflow steps", steps)

    # A disabled configure preset fails also when its build tree exists.
    ctx.ok("plain configure of out/off", ctx.cmake("-S", src, "-B", out / "off", "-G", "Ninja", cwd=src))
    off = ctx.cmake("--preset", "off", cwd=src)
    ctx.fails("disabled preset is rejected", off)
    ctx.check("disabled preset message", "disabled" in off.stderr, off.tail())
    ctx.observe("disabled preset", [off.returncode != 0, off.stderr.strip()])

    # Command line native tool options replace the preset nativeToolOptions.
    (out / "dev/a").unlink(missing_ok=True)
    dry = ctx.cmake("--build", "--preset", "dry", cwd=src)
    ctx.ok("build preset with nativeToolOptions -n", dry)
    ctx.check("dry run does not link", not (out / "dev/a").exists())
    real = ctx.cmake("--build", "--preset", "dry", "--", "-v", cwd=src)
    ctx.ok("build preset with -- -v", real)
    ctx.check("-- options replace nativeToolOptions", (out / "dev/a").is_file(), real.tail())

    # --target with a semicolon list.
    ctx.ok("clean", ctx.cmake("--build", out / "dev", "--target", "clean"))
    tgt = ctx.cmake("--build", out / "dev", "--target", "a;b")
    ctx.ok("--build --target 'a;b'", tgt)
    built = [name for name in ("a", "b", "c") if (out / "dev" / name).is_file()]
    ctx.check("only a and b are built", built == ["a", "b"], built)
    ctx.observe("target a;b built", built)

    # --install --component with a semicolon list and with repeated options.
    for label, args in (("A;B", ["--component", "A;B"]), ("A B", ["--component", "A", "--component", "B"])):
        prefix = r / f"inst-{slug(label)}"
        proc = ctx.cmake("--install", out / "dev", "--prefix", prefix, *args)
        ctx.ok(f"--install --component {label}", proc)
        listing = file_listing(prefix) if prefix.exists() else []
        ctx.check(f"component {label} installs a and b", listing == ["bin/a", "bin/b"], listing)
        ctx.observe(f"install {label}", listing)

    # --build of a subdirectory of the build tree.
    sub_ninja = ctx.cmake("--build", "out/dev/sub", cwd=src)
    ctx.observe("build ninja subdir", [sub_ninja.returncode != 0, "build.ninja" in sub_ninja.output])
    ctx.fails("--build out/dev/sub fails for Ninja", sub_ninja)
    if shutil.which("make"):
        ctx.ok("configure Makefiles tree", ctx.configure(src, out / "mk", "Unix Makefiles"))
        sub_make = ctx.cmake("--build", "out/mk/sub", cwd=src)
        ctx.ok("--build out/mk/sub", sub_make)
        ctx.check("subdirectory target built", (out / "mk/sub/s").is_file())
        ctx.check("other targets not built", not (out / "mk/a").exists())

    # -C preload, -D override, -U glob, SARIF.
    cli = r / "cli"
    sarif = r / "diag.sarif"
    first = ctx.cmake("-S", src, "-B", cli, "-G", "Ninja", "-C", src / "init.cmake", "-DOVR=cli",
                      "-DVAR_A=1", "-DVAR_B=2", "-DVAR_C=3", f"--sarif-output={sarif}")
    ctx.ok("configure with -C and -D", first)
    ctx.check("-D overrides -C", "VALUES PRE=from-preload OVR=cli VAR_A=1 VAR_B=2 VAR_C=3" in first.stdout, first.tail())
    ctx.check("warning backtrace", "CMake Warning at cmake/warn.cmake:2 (message):" in first.stderr, first.stderr)
    order = ctx.cmake("-S", src, "-B", r / "cli2", "-G", "Ninja", "-DOVR=cli-first", "-C", src / "init.cmake")
    ctx.ok("configure with -D before -C", order)
    ctx.check("-D before -C still wins", "OVR=cli-first" in order.stdout, order.tail())
    unset = ctx.cmake("-U", "VAR_[AB]", cli)
    ctx.ok("-U 'VAR_[AB]'", unset)
    left = cache_entries(cli, ["VAR_A", "VAR_B", "VAR_C"])
    ctx.check("-U removes VAR_A and VAR_B only", sorted(left) == ["VAR_C"], left)
    ctx.observe("values", re.findall(r"(?m)^-- VALUES .*$", first.stdout + unset.stdout))
    ctx.observe("warning", first.stderr.strip())
    if sarif.is_file():
        digest = sarif_digest(sarif)
        ctx.observe("sarif", digest)
        warn = [item for item in digest if item["message"] == "deliberate warning"]
        stack = warn[0]["stacks"][0] if warn and warn[0]["stacks"] else []
        ctx.check("SARIF warning location",
                  bool(warn) and warn[0]["locations"] == ["CMAKE_SOURCE_DIR:cmake/warn.cmake:2:message"], digest)
        ctx.check("SARIF stack frames", stack == [
            "CMAKE_SOURCE_DIR:cmake/warn.cmake:2:message",
            "CMAKE_SOURCE_DIR:cmake/warn.cmake:5:emit_warning",
            "CMAKE_SOURCE_DIR:cmake/warn.cmake:7:outer",
            "CMAKE_SOURCE_DIR:CMakeLists.txt:4:include",
        ], stack)
    else:
        ctx.check("SARIF file written", False, str(sarif))

    # --fresh in a directory without CMakeCache.txt.
    fresh = ctx.cmake("-S", src, "-B", r / "fresh", "-G", "Ninja", "--fresh")
    ctx.ok("--fresh without CMakeCache.txt", fresh)
    ctx.check("--fresh writes a cache", (r / "fresh/CMakeCache.txt").is_file())

    # -E cat with stdin and a file name that starts with a dash.
    ctx.write("-dashfile", "dash file\n")
    cats = {}
    for label, args in (("- -dashfile", ["-", "-dashfile"]), ("- ./-dashfile", ["-", "./-dashfile"]),
                        ("-- -dashfile", ["--", "-dashfile"]), ("- -- -dashfile", ["-", "--", "-dashfile"])):
        proc = ctx.cmake("-E", "cat", *args, stdin="from stdin\n")
        cats[label] = [proc.returncode, proc.stdout, proc.stderr.strip()]
    ctx.check("-E cat - -dashfile", cats["- -dashfile"][:2] == [0, "from stdin\ndash file\n"], cats)
    ctx.check("-E cat -- -dashfile", cats["-- -dashfile"][:2] == [0, "dash file\n"], cats)
    ctx.observe("cat", cats)


# --------------------------------------------------------------------------
# s01: end-to-end build, install, export, import


DEMO_HEADER = """
    #ifndef DEMO_DEMO_H
    #define DEMO_DEMO_H
    #ifdef __cplusplus
    extern "C" {
    #endif
    int demo_obj_value(void);
    int demo_static_value(void);
    int demo_shared_value(void);
    int demo_objcxx_value(void);
    #ifdef __cplusplus
    }
    #endif
    #endif
    """

DEMO_MAIN = """
    #include <cstdio>
    #include "demo/demo.h"
    #ifndef DEMO_HAVE_OBJCXX
    extern "C" int demo_objcxx_value(void) { return 6; }
    #endif
    consteval int square(int x) { return x * x; }
    int main() {
      constexpr int sq = square(7);
      int a = demo_static_value(), b = demo_shared_value(), c = demo_objcxx_value();
      std::printf("%s %d %d %d %d\\n", NAME, sq, a, b, c);
      return (sq == 49 && a == 13 && b == 83 && c == 6) ? 0 : 1;
    }
    """

DEMO_PROJECT = {
    "CMakeLists.txt": """
        cmake_minimum_required(VERSION 3.28)
        project(Demo VERSION 1.2.3 LANGUAGES C CXX)
        if(APPLE)
          enable_language(OBJCXX)
        endif()
        include(GNUInstallDirs)
        include(CMakePackageConfigHelpers)
        enable_testing()
        set(CMAKE_CXX_STANDARD 20)
        set(CMAKE_CXX_STANDARD_REQUIRED ON)
        set(CMAKE_CXX_EXTENSIONS OFF)
        if(APPLE)
          set(rel_rpath "@loader_path/../${CMAKE_INSTALL_LIBDIR}")
        else()
          set(rel_rpath "$ORIGIN/../${CMAKE_INSTALL_LIBDIR}")
        endif()

        add_library(demo_obj OBJECT obj.c)
        set_target_properties(demo_obj PROPERTIES POSITION_INDEPENDENT_CODE ON)
        target_include_directories(demo_obj PUBLIC $<BUILD_INTERFACE:${CMAKE_CURRENT_SOURCE_DIR}/include>)

        add_library(demo_static STATIC static.c $<TARGET_OBJECTS:demo_obj>)
        set_target_properties(demo_static PROPERTIES POSITION_INDEPENDENT_CODE ON)
        target_include_directories(demo_static PUBLIC
          $<BUILD_INTERFACE:${CMAKE_CURRENT_SOURCE_DIR}/include>
          $<INSTALL_INTERFACE:${CMAKE_INSTALL_INCLUDEDIR}>)

        add_library(demo_shared SHARED shared.cpp)
        target_link_libraries(demo_shared PRIVATE demo_static)
        target_include_directories(demo_shared PUBLIC
          $<BUILD_INTERFACE:${CMAKE_CURRENT_SOURCE_DIR}/include>
          $<INSTALL_INTERFACE:${CMAKE_INSTALL_INCLUDEDIR}>)
        set_target_properties(demo_shared PROPERTIES VERSION ${PROJECT_VERSION} SOVERSION 1
          INSTALL_NAME_DIR "@rpath")
        target_compile_features(demo_shared PUBLIC cxx_std_20)

        set(demo_targets demo_obj demo_static demo_shared)
        add_executable(demo_app app.cpp)
        target_compile_definitions(demo_app PRIVATE NAME="app")
        target_link_libraries(demo_app PRIVATE demo_shared demo_static)
        if(APPLE)
          add_library(demo_objcxx STATIC objcxx.mm)
          target_compile_options(demo_objcxx PRIVATE -fobjc-arc)
          target_link_libraries(demo_objcxx PUBLIC "-framework Foundation")
          target_include_directories(demo_objcxx PUBLIC
            $<BUILD_INTERFACE:${CMAKE_CURRENT_SOURCE_DIR}/include>
            $<INSTALL_INTERFACE:${CMAKE_INSTALL_INCLUDEDIR}>)
          list(APPEND demo_targets demo_objcxx)
          target_link_libraries(demo_app PRIVATE demo_objcxx)
          target_compile_definitions(demo_app PRIVATE DEMO_HAVE_OBJCXX)
        endif()
        set_target_properties(demo_app PROPERTIES
          BUILD_RPATH "${CMAKE_BINARY_DIR}/extra-rpath"
          INSTALL_RPATH "${rel_rpath}")
        add_test(NAME demo_app COMMAND demo_app)
        set_tests_properties(demo_app PROPERTIES PASS_REGULAR_EXPRESSION "^app 49 13 83 6")

        install(TARGETS ${demo_targets} EXPORT DemoTargets
          ARCHIVE DESTINATION ${CMAKE_INSTALL_LIBDIR}
          LIBRARY DESTINATION ${CMAKE_INSTALL_LIBDIR}
          RUNTIME DESTINATION ${CMAKE_INSTALL_BINDIR}
          OBJECTS DESTINATION ${CMAKE_INSTALL_LIBDIR}/objects
          INCLUDES DESTINATION ${CMAKE_INSTALL_INCLUDEDIR})
        install(TARGETS demo_app RUNTIME DESTINATION ${CMAKE_INSTALL_BINDIR})
        install(DIRECTORY include/ DESTINATION ${CMAKE_INSTALL_INCLUDEDIR})
        install(EXPORT DemoTargets NAMESPACE Demo:: DESTINATION ${CMAKE_INSTALL_LIBDIR}/cmake/Demo)
        configure_package_config_file(DemoConfig.cmake.in ${CMAKE_CURRENT_BINARY_DIR}/DemoConfig.cmake
          INSTALL_DESTINATION ${CMAKE_INSTALL_LIBDIR}/cmake/Demo
          PATH_VARS CMAKE_INSTALL_INCLUDEDIR)
        write_basic_package_version_file(${CMAKE_CURRENT_BINARY_DIR}/DemoConfigVersion.cmake
          VERSION ${PROJECT_VERSION} COMPATIBILITY SameMajorVersion)
        install(FILES ${CMAKE_CURRENT_BINARY_DIR}/DemoConfig.cmake
          ${CMAKE_CURRENT_BINARY_DIR}/DemoConfigVersion.cmake
          DESTINATION ${CMAKE_INSTALL_LIBDIR}/cmake/Demo)
        """,
    "DemoConfig.cmake.in": """
        @PACKAGE_INIT@
        include("${CMAKE_CURRENT_LIST_DIR}/DemoTargets.cmake")
        set_and_check(DEMO_INCLUDE_DIR "@PACKAGE_CMAKE_INSTALL_INCLUDEDIR@")
        check_required_components(Demo)
        """,
    "include/demo/demo.h": DEMO_HEADER,
    "obj.c": """
        #include "demo/demo.h"
        int demo_obj_value(void) { return 3; }
        """,
    "static.c": """
        #include "demo/demo.h"
        int demo_static_value(void) { return 10 + demo_obj_value(); }
        """,
    "shared.cpp": """
        #include <algorithm>
        #include "demo/demo.h"
        extern "C" int demo_shared_value(void) {
          return std::ranges::max({1, 7, 4}) * 10 + demo_static_value();
        }
        """,
    "objcxx.mm": """
        #import <Foundation/Foundation.h>
        #include "demo/demo.h"
        #if !__has_feature(objc_arc)
        #error "ARC is required"
        #endif
        extern "C" int demo_objcxx_value(void) {
          @autoreleasepool {
            NSString *text = [NSString stringWithFormat:@"%d-%s", 42, "arc"];
            return (int)[text length];
          }
        }
        """,
    "app.cpp": DEMO_MAIN,
}

DEMO_CONSUMER = {
    "CMakeLists.txt": """
        cmake_minimum_required(VERSION 3.28)
        project(Consumer CXX)
        find_package(Demo 1.2 CONFIG REQUIRED)
        message(STATUS "FOUND Demo ${Demo_VERSION} include=${DEMO_INCLUDE_DIR}")
        enable_testing()
        add_executable(consumer consumer.cpp)
        target_compile_definitions(consumer PRIVATE NAME="consumer"
          $<$<TARGET_EXISTS:Demo::demo_objcxx>:DEMO_HAVE_OBJCXX>)
        target_link_libraries(consumer PRIVATE Demo::demo_shared Demo::demo_static
          $<TARGET_NAME_IF_EXISTS:Demo::demo_objcxx>)
        add_test(NAME consumer COMMAND consumer)
        """,
    "consumer.cpp": DEMO_MAIN,
}


def macho_rpaths(binary: Path) -> list[str]:
    """Return the LC_RPATH entries of a Mach-O file."""
    out = subprocess.run(["otool", "-l", str(binary)], capture_output=True, text=True, timeout=60).stdout
    return re.findall(r"cmd LC_RPATH\n\s+cmdsize \d+\n\s+path (.*?) \(offset \d+\)", out)


def macho_id(binary: Path) -> str:
    """Return the install name of a Mach-O shared library."""
    out = subprocess.run(["otool", "-D", str(binary)], capture_output=True, text=True, timeout=60).stdout
    lines = out.strip().splitlines()
    return lines[1].strip() if len(lines) > 1 else ""


@scenario("s01", "End-to-end C + C++20 + ObjC++ build/test/install/export/import consumer on every generator")
def s01_end_to_end(ctx: Ctx) -> None:
    src = ctx.tree(ctx.root / "src", DEMO_PROJECT)
    consumer_src = ctx.tree(ctx.root / "consumer", DEMO_CONSUMER)
    generators = generators_available(ctx.options)
    if not generators:
        raise ScenarioSkip("no generator available")
    if "Xcode" not in generators and DARWIN:
        ctx.skip("Xcode generator not available or disabled")
    for gen in generators:
        try:
            e2e_generator(ctx, gen, src, consumer_src)
        except StepFailed:
            continue  # Report the failure and continue with the next generator.


def e2e_generator(ctx: Ctx, gen: str, src: Path, consumer_src: Path) -> None:
    """Run the end-to-end flow with one generator."""
    tag = slug(gen)
    build = ctx.root / f"b-{tag}"
    prefix = ctx.root / f"inst-{tag}"
    # Xcode signs binaries after the link. The install step then edits the
    # rpaths with install_name_tool, which makes that signature invalid, and
    # macOS kills the installed program. With signing off, the linker ad-hoc
    # signature stays valid because install_name_tool updates it.
    signing = ["-DCMAKE_XCODE_ATTRIBUTE_CODE_SIGNING_ALLOWED=NO"] if gen == "Xcode" else []
    ctx.ok(f"{gen}: configure", ctx.configure(src, build, gen, f"-DCMAKE_INSTALL_PREFIX={prefix}", *signing))
    ctx.ok(f"{gen}: build", ctx.build(build, gen))
    app = exe_path(build, "demo_app", gen)
    run = ctx.run([app])
    ctx.check(f"{gen}: build-tree app runs", run.stdout == "app 49 13 83 6\n", run.tail())
    test = ctx.ctest("--test-dir", build, "-C", "Release", "--output-on-failure")
    ctx.check(f"{gen}: ctest", test.ok and "100% tests passed" in test.stdout, test.tail())
    ctx.ok(f"{gen}: install", ctx.cmake("--install", build, "--config", "Release"))
    listing = file_listing(prefix)
    for required in ("bin/demo_app", "include/demo/demo.h", "lib/cmake/Demo/DemoConfig.cmake",
                     "lib/cmake/Demo/DemoConfigVersion.cmake", "lib/cmake/Demo/DemoTargets.cmake",
                     "lib/cmake/Demo/DemoTargets-release.cmake"):
        ctx.check(f"{gen}: installs {required}", required in listing, listing)
    ctx.check(f"{gen}: installs the object file",
              any(item.startswith("lib/objects/") for item in listing), listing)
    ctx.observe(f"{gen}: install tree", listing)
    installed = ctx.run([prefix / "bin/demo_app"])
    ctx.check(f"{gen}: installed app runs", installed.stdout == "app 49 13 83 6\n", installed.tail())
    if DARWIN:
        build_rpaths = macho_rpaths(app)
        ctx.check(f"{gen}: BUILD_RPATH in build tree", f"{build}/extra-rpath" in build_rpaths, build_rpaths)
        inst_rpaths = macho_rpaths(prefix / "bin/demo_app")
        ctx.check(f"{gen}: INSTALL_RPATH replaces BUILD_RPATH", inst_rpaths == ["@loader_path/../lib"], inst_rpaths)
        lib = prefix / "lib/libdemo_shared.1.dylib"
        ctx.check(f"{gen}: INSTALL_NAME_DIR", macho_id(lib) == "@rpath/libdemo_shared.1.dylib", macho_id(lib))
        ctx.observe(f"{gen}: installed rpaths", inst_rpaths)
        ctx.observe(f"{gen}: installed id", macho_id(lib))
    targets = (prefix / "lib/cmake/Demo/DemoTargets.cmake").read_text()
    ctx.check(f"{gen}: export uses the Demo:: namespace",
              "add_library(Demo::demo_shared SHARED IMPORTED)" in targets
              and "add_library(Demo::demo_obj OBJECT IMPORTED)" in targets, targets[:2000])

    cbuild = ctx.root / f"c-{tag}"
    conf = ctx.configure(consumer_src, cbuild, gen, f"-DCMAKE_PREFIX_PATH={prefix}")
    ctx.ok(f"{gen}: consumer configure", conf)
    found = re.findall(r"(?m)^-- FOUND Demo .*$", conf.stdout)
    ctx.check(f"{gen}: consumer find_package", found == [f"-- FOUND Demo 1.2.3 include={prefix}/include"], found)
    ctx.observe(f"{gen}: consumer found", found)
    ctx.ok(f"{gen}: consumer build", ctx.build(cbuild, gen))
    crun = ctx.run([exe_path(cbuild, "consumer", gen)])
    ctx.check(f"{gen}: consumer runs", crun.stdout == "consumer 49 13 83 6\n", crun.tail())
    ctest = ctx.ctest("--test-dir", cbuild, "-C", "Release")
    ctx.check(f"{gen}: consumer ctest", ctest.ok and "100% tests passed" in ctest.stdout, ctest.tail())
    ctx.observe(f"{gen}: outputs", [run.stdout, installed.stdout, crun.stdout])


# --------------------------------------------------------------------------
# s02: long link command


LONG_DIRS = [f"component_{i}_with_a_deliberately_long_directory_name_x" for i in range(6)]
LONG_FILES = 440


def long_link_sources() -> dict[str, str]:
    """Return the sources of the long link project."""
    files: dict[str, str] = {}
    deep = "/".join(LONG_DIRS)
    for i in range(LONG_FILES):
        files[f"{deep}/translation_unit_with_a_long_descriptive_file_name_{i:04d}.c"] = (
            f"int long_fn_{i}(void) {{ return {i % 7}; }}\n"
        )
    decls = "".join(f"int long_fn_{i}(void);\n" for i in range(LONG_FILES))
    calls = "".join(f"  sum += long_fn_{i}();\n" for i in range(LONG_FILES))
    files["main.c"] = (
        f"#include <stdio.h>\n{decls}int main(void) {{\n  int sum = 0;\n{calls}"
        "  printf(\"sum %d\\n\", sum);\n  return 0;\n}\n"
    )
    files["CMakeLists.txt"] = (
        "cmake_minimum_required(VERSION 4.0)\nproject(longlink C)\n"
        "file(GLOB_RECURSE parts CONFIGURE_DEPENDS \"${CMAKE_CURRENT_SOURCE_DIR}/component_*/*.c\")\n"
        "list(SORT parts)\nadd_executable(longlink main.c ${parts})\n"
    )
    return files


@scenario("s02", "Long link command (> 140 KiB of object paths, spaces and quotes in paths), no-op rebuild")
def s02_long_link(ctx: Ctx) -> None:
    files = long_link_sources()
    src = ctx.root / "src dir with 'quote'"
    for rel, text in files.items():
        path = src / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")
    objects = [f"CMakeFiles/longlink.dir/{rel}.o" for rel in files if rel.endswith(".c")]
    total = sum(len(obj) + 1 for obj in objects)
    ctx.check("object paths exceed 140 KiB", total > 140 * 1024, total)
    expected = f"sum {sum(i % 7 for i in range(LONG_FILES))}\n"
    gens = [g for g in ("Ninja", "Unix Makefiles") if g in generators_available(ctx.options)]
    for gen in gens:
        build = ctx.root / f"build dir 'q' {slug(gen)}"
        try:
            ctx.ok(f"{gen}: configure", ctx.configure(src, build, gen))
            ctx.ok(f"{gen}: build", ctx.build(build, gen))
        except StepFailed:
            continue
        run = ctx.run([build / "longlink"])
        ctx.check(f"{gen}: executable runs", run.stdout == expected, run.tail())
        again = ctx.build(build, gen)
        ctx.check(f"{gen}: second build is a no-op", no_op(again), again.tail())
        rsp = sorted(p.relative_to(build).as_posix() for p in build.rglob("*.rsp"))
        ctx.observe(f"{gen}: response files", rsp)
        ctx.observe(f"{gen}: output", run.stdout)


# --------------------------------------------------------------------------
# s03: universal binary with precompiled headers


def universal_supported(_options: Options) -> str | None:
    """Skip when the host cannot build arm64 and x86_64 code."""
    if not DARWIN:
        return "needs macOS"
    with tempfile.TemporaryDirectory() as tmp:
        probe = subprocess.run(
            ["clang", "-arch", "arm64", "-arch", "x86_64", "-x", "c", "-c", "-", "-o", f"{tmp}/p.o"],
            input=b"int x;\n", capture_output=True, timeout=120,
        )
    return None if probe.returncode == 0 else "the toolchain cannot build arm64;x86_64"


@scenario("s03", "Universal arm64;x86_64 PCH (target_precompile_headers, SKIP_PRECOMPILE_HEADERS, REUSE_FROM)",
          requires=universal_supported)
def s03_universal_pch(ctx: Ctx) -> None:
    src = ctx.tree(ctx.root / "src", {
        "CMakeLists.txt": """
            cmake_minimum_required(VERSION 4.0)
            project(pch CXX)
            set(CMAKE_CXX_STANDARD 17)
            add_library(pchlib STATIC a.cpp b.cpp skip.cpp)
            target_precompile_headers(pchlib PRIVATE pch.h)
            set_source_files_properties(skip.cpp PROPERTIES SKIP_PRECOMPILE_HEADERS ON)
            add_executable(pchapp main.cpp)
            target_link_libraries(pchapp PRIVATE pchlib)
            set_target_properties(pchapp PROPERTIES PRECOMPILE_HEADERS_REUSE_FROM pchlib)
            """,
        "pch.h": """
            #include <vector>
            #define PCH_MARKER 1
            inline int pch_version() { return 1; }
            """,
        "a.cpp": """
            #ifndef PCH_MARKER
            #error "pch.h is not force-included"
            #endif
            int a_value() { return pch_version() * 10; }
            """,
        "b.cpp": """
            int b_value() { std::vector<int> v{1, 2, 3}; return static_cast<int>(v.size()); }
            """,
        "skip.cpp": """
            #ifdef PCH_MARKER
            #error "SKIP_PRECOMPILE_HEADERS source sees the PCH"
            #endif
            int skip_value() { return 5; }
            """,
        "main.cpp": """
            #include <cstdio>
            int a_value(); int b_value(); int skip_value();
            int main() {
              std::printf("pch %d %d %d %d\\n", pch_version(), a_value(), b_value(), skip_value());
              return 0;
            }
            """,
    })
    pch = src / "pch.h"
    original = pch.read_text()
    for gen in [g for g in ("Ninja", "Unix Makefiles") if g in generators_available(ctx.options)]:
        pch.write_text(original)
        build = ctx.root / f"b-{slug(gen)}"
        try:
            ctx.ok(f"{gen}: configure", ctx.configure(src, build, gen, "-DCMAKE_OSX_ARCHITECTURES=arm64;x86_64"))
            ctx.ok(f"{gen}: build", ctx.build(build, gen))
        except StepFailed:
            continue
        app = build / "pchapp"
        archs = subprocess.run(["lipo", "-archs", str(app)], capture_output=True, text=True, timeout=60).stdout.split()
        ctx.check(f"{gen}: lipo -archs has arm64 and x86_64", sorted(archs) == ["arm64", "x86_64"], archs)
        lib_archs = subprocess.run(["lipo", "-archs", str(build / "libpchlib.a")],
                                   capture_output=True, text=True, timeout=60).stdout.split()
        ctx.check(f"{gen}: static library is universal", sorted(lib_archs) == ["arm64", "x86_64"], lib_archs)
        run = ctx.run([app])
        ctx.check(f"{gen}: first output", run.stdout == "pch 1 10 3 5\n", run.tail())
        ctx.observe(f"{gen}: pch files", sorted(p.name for p in build.rglob("cmake_pch*") if p.is_file()))
        touch_later(pch, original.replace("return 1;", "return 2;"))
        rebuild = ctx.build(build, gen)
        ctx.ok(f"{gen}: rebuild after PCH header edit", rebuild, required=False)
        ctx.check(f"{gen}: PCH edit recompiles a.cpp", compiled(rebuild, r"a\.cpp\.o"), rebuild.tail())
        ctx.check(f"{gen}: PCH edit does not recompile skip.cpp", not compiled(rebuild, r"skip\.cpp\.o"),
                  rebuild.tail())
        run2 = ctx.run([app])
        ctx.check(f"{gen}: output after PCH edit", run2.stdout == "pch 2 20 3 5\n", run2.tail())
        again = ctx.build(build, gen)
        ctx.check(f"{gen}: third build is a no-op", no_op(again), again.tail())
        ctx.observe(f"{gen}: outputs", [run.stdout, run2.stdout])


# --------------------------------------------------------------------------
# s04: OBJECT_DEPENDS, MAIN_DEPENDENCY, generated headers, configure_file


@scenario("s04", "OBJECT_DEPENDS + MAIN_DEPENDENCY + generated headers + configure_file reconfigure rebuild (-j4)")
def s04_generated(ctx: Ctx) -> None:
    files = {
        "CMakeLists.txt": """
            cmake_minimum_required(VERSION 4.0)
            project(gen C)
            configure_file(config.h.in ${CMAKE_BINARY_DIR}/gen/config.h)
            add_custom_command(OUTPUT ${CMAKE_BINARY_DIR}/gen/table.h
              COMMAND ${CMAKE_COMMAND} -DIN=${CMAKE_SOURCE_DIR}/table.def
                      -DOUT=${CMAKE_BINARY_DIR}/gen/table.h -P ${CMAKE_SOURCE_DIR}/gen_table.cmake
              MAIN_DEPENDENCY table.def
              DEPENDS gen_table.cmake
              COMMENT "Generating table.h")
            add_custom_target(gen_headers DEPENDS ${CMAKE_BINARY_DIR}/gen/table.h)
            file(GLOB parts ${CMAKE_SOURCE_DIR}/part*.c)
            list(SORT parts)
            add_library(parts STATIC ${parts})
            target_include_directories(parts PUBLIC ${CMAKE_BINARY_DIR}/gen)
            add_dependencies(parts gen_headers)
            add_executable(genapp main.c ${CMAKE_BINARY_DIR}/gen/table.h)
            target_link_libraries(genapp PRIVATE parts)
            set_source_files_properties(main.c PROPERTIES OBJECT_DEPENDS ${CMAKE_SOURCE_DIR}/extra.dep)
            """,
        "config.h.in": "#define CONFIG_VALUE @CONFIG_NUMBER@\n#define CONFIG_TAG \"one\"\n",
        "table.def": "1;2;3\n",
        "extra.dep": "first\n",
        "gen_table.cmake": """
            file(READ "${IN}" content)
            string(STRIP "${content}" content)
            set(sum 0)
            foreach(n IN LISTS content)
              math(EXPR sum "${sum} + ${n}")
            endforeach()
            file(WRITE "${OUT}.tmp" "#define TABLE_SUM ${sum}\\n")
            file(COPY_FILE "${OUT}.tmp" "${OUT}" ONLY_IF_DIFFERENT)
            """,
        "main.c": """
            #include <stdio.h>
            #include "config.h"
            #include "table.h"
            int parts_total(void);
            int main(void) {
              printf("config=%d tag=%s table=%d parts=%d\\n", CONFIG_VALUE, CONFIG_TAG, TABLE_SUM, parts_total());
              return 0;
            }
            """,
    }
    for i in range(8):
        files[f"part{i}.c"] = (
            f'#include "table.h"\nint part_{i}(void) {{ return TABLE_SUM + {i}; }}\n'
            + ("" if i else "".join(f"int part_{j}(void);\n" for j in range(1, 8))
               + "int parts_total(void) { return part_0()"
               + "".join(f" + part_{j}()" for j in range(1, 8)) + "; }\n")
        )
    for gen in [g for g in ("Ninja", "Unix Makefiles") if g in generators_available(ctx.options)]:
        src = ctx.tree(ctx.root / f"src-{slug(gen)}", files)
        build = ctx.root / f"b-{slug(gen)}"
        try:
            generated_flow(ctx, gen, src, build)
        except StepFailed:
            continue


def generated_flow(ctx: Ctx, gen: str, src: Path, build: Path) -> None:
    """Run the generated header flow with one generator."""
    ctx.ok(f"{gen}: configure", ctx.configure(src, build, gen, "-DCONFIG_NUMBER=1"))
    ctx.ok(f"{gen}: parallel -j4 clean build", ctx.cmake("--build", build, "-j", "4"))
    app = build / "genapp"
    outputs = []

    def expect(label: str, text: str) -> None:
        run = ctx.run([app])
        outputs.append(run.stdout)
        ctx.check(f"{gen}: {label}", run.stdout == text, run.tail())

    expect("first output", "config=1 tag=one table=6 parts=76\n")
    touch_later(src / "config.h.in", "#define CONFIG_VALUE @CONFIG_NUMBER@\n#define CONFIG_TAG \"two\"\n")
    step = ctx.cmake("--build", build, "-j", "4")
    ctx.ok(f"{gen}: rebuild after config.h.in edit", step)
    ctx.check(f"{gen}: config.h regenerated", '"two"' in (build / "gen/config.h").read_text())
    ctx.check(f"{gen}: config.h.in edit reruns CMake", "Configuring done" in step.output, step.tail())
    expect("output after config.h.in edit", "config=1 tag=two table=6 parts=76\n")
    touch_later(src / "table.def", "1;2;3;4\n")
    step = ctx.cmake("--build", build, "-j", "4")
    ctx.ok(f"{gen}: rebuild after table.def edit", step)
    ctx.check(f"{gen}: MAIN_DEPENDENCY edit regenerates table.h", "Generating table.h" in step.output, step.tail())
    expect("output after table.def edit", "config=1 tag=two table=10 parts=108\n")
    touch_later(src / "extra.dep", "second\n")
    step = ctx.cmake("--build", build, "-j", "4")
    ctx.ok(f"{gen}: rebuild after OBJECT_DEPENDS edit", step)
    ctx.check(f"{gen}: OBJECT_DEPENDS edit recompiles main.c", compiled(step, r"main\.c\.o"), step.tail())
    ctx.check(f"{gen}: OBJECT_DEPENDS edit does not recompile parts", not compiled(step, r"part\d\.c\.o"), step.tail())
    step = ctx.cmake("--build", build, "-j", "4")
    ctx.check(f"{gen}: final build is a no-op", no_op(step), step.tail())
    ctx.observe(f"{gen}: outputs", outputs)


# --------------------------------------------------------------------------
# s07: large streaming archives


BIG_SENTINEL = b"BIG-END-SENTINEL"
RSS_LIMIT = 256 * MIB


class ZeroStream:
    """A file-like reader of zero bytes that ends with BIG_SENTINEL."""

    def __init__(self, size: int) -> None:
        self.size = size
        self.pos = 0

    def read(self, count: int = -1) -> bytes:
        """Return the next bytes of the stream."""
        left = self.size - self.pos
        if count < 0 or count > left:
            count = left
        start, end = self.pos, self.pos + count
        self.pos = end
        tail_start = self.size - len(BIG_SENTINEL)
        if end <= tail_start:
            return bytes(count)
        zeros = max(0, tail_start - start)
        return bytes(zeros) + BIG_SENTINEL[max(0, start - tail_start):end - tail_start]


def write_big_tar_gz(path: Path, size: int) -> None:
    """Write a gzip tar file with two small members and one large member."""
    def add(tar: tarfile.TarFile, name: str, data: bytes | ZeroStream, length: int) -> None:
        info = tarfile.TarInfo(name)
        info.size = length
        info.mode = 0o644
        info.mtime = 1_700_000_000
        tar.addfile(info, data if isinstance(data, ZeroStream) else _BytesReader(data))
    with gzip.GzipFile(path, "wb", compresslevel=1, mtime=0) as gz:
        with tarfile.open(fileobj=gz, mode="w|", format=tarfile.PAX_FORMAT) as tar:
            add(tar, "payload/sentinel-begin.txt", b"BEGIN\n", 6)
            add(tar, "payload/big.bin", ZeroStream(size), size)
            add(tar, "payload/sentinel-end.txt", b"END\n", 4)


def write_stored_zip(path: Path, size: int) -> None:
    """Write a stored (not compressed) zip file with one large member."""
    with zipfile.ZipFile(path, "w", compression=zipfile.ZIP_STORED) as zf:
        zf.writestr("zpayload/z-begin.txt", "ZBEGIN\n")
        with zf.open(zipfile.ZipInfo("zpayload/zbig.bin", (2024, 1, 1, 0, 0, 0)), "w") as out:
            stream = ZeroStream(size)
            while chunk := stream.read(4 * MIB):
                out.write(chunk)
        zf.writestr("zpayload/z-end.txt", "ZEND\n")


class _BytesReader:
    """A minimal reader over bytes for tarfile.addfile."""

    def __init__(self, data: bytes) -> None:
        self.data = data
        self.pos = 0

    def read(self, count: int = -1) -> bytes:
        """Return the next bytes."""
        end = len(self.data) if count < 0 else self.pos + count
        chunk = self.data[self.pos:end]
        self.pos += len(chunk)
        return chunk


def big_file_ok(path: Path, size: int) -> bool:
    """Check the size and the end sentinel of a large extracted member."""
    if not path.is_file() or path.stat().st_size != size:
        return False
    with path.open("rb") as file:
        file.seek(size - len(BIG_SENTINEL))
        return file.read() == BIG_SENTINEL


@scenario("s07", "Large streaming archives (> 513 MiB decoded .tar.gz + stored .zip) with bounded peak RSS")
def s07_archives(ctx: Ctx) -> None:
    size = ctx.options.archive_mib * MIB
    tgz = ctx.root / "big.tar.gz"
    zipf = ctx.root / "stored.zip"
    write_big_tar_gz(tgz, size)
    write_stored_zip(zipf, size)
    ctx.check("tar.gz decodes to more than 513 MiB", size > 513 * MIB, size)

    def measured(label: str, *args: str | os.PathLike[str], cwd: Path | None = None) -> Proc:
        proc = ctx.run([sys.executable, "-c", RSS_WRAPPER, ctx.tools.cmake, *args], cwd=cwd)
        rss = peak_rss(proc)
        ctx.check(f"{label}: exit 0", proc.ok, proc.tail())
        ctx.check(f"{label}: peak RSS < 256 MiB", rss is not None and rss < RSS_LIMIT,
                  f"{(rss or 0) / MIB:.1f} MiB")
        return proc

    listing = measured("tar tf big.tar.gz", "-E", "tar", "tf", tgz)
    names = [line for line in listing.stdout.splitlines() if line]
    ctx.check("tar tf lists the members", names == [
        "payload/sentinel-begin.txt", "payload/big.bin", "payload/sentinel-end.txt"], names)
    ctx.observe("tgz members", names)

    full = ctx.root / "x-full"
    full.mkdir()
    measured("tar xf big.tar.gz", "-E", "tar", "xf", tgz, cwd=full)
    ctx.check("extracted big.bin size and sentinel", big_file_ok(full / "payload/big.bin", size))
    ctx.check("extracted sentinels", (full / "payload/sentinel-begin.txt").read_bytes() == b"BEGIN\n"
              and (full / "payload/sentinel-end.txt").read_bytes() == b"END\n")
    remove_tree(full)

    member = ctx.root / "x-member"
    member.mkdir()
    measured("tar xf big.tar.gz <member>", "-E", "tar", "xf", tgz, "payload/sentinel-end.txt", cwd=member)
    ctx.check("member filter extracts one file", file_listing(member) == ["payload/sentinel-end.txt"],
              file_listing(member))

    ctx.write("extract.cmake", """
        file(ARCHIVE_EXTRACT INPUT "${ARCHIVE}" DESTINATION "${DEST}" ${EXTRA})
        """)
    dest = ctx.root / "x-patterns"
    measured("file(ARCHIVE_EXTRACT PATTERNS) tar.gz", f"-DARCHIVE={tgz}", f"-DDEST={dest}",
             "-DEXTRA=PATTERNS;payload/sentinel-*", "-P", ctx.root / "extract.cmake")
    ctx.check("ARCHIVE_EXTRACT PATTERNS skips big.bin", file_listing(dest) == [
        "payload/sentinel-begin.txt", "payload/sentinel-end.txt"], file_listing(dest))
    ctx.observe("pattern extract", file_listing(dest))

    zlist = measured("tar tf stored.zip", "-E", "tar", "tf", zipf)
    znames = [line for line in zlist.stdout.splitlines() if line]
    ctx.check("zip members", znames == ["zpayload/z-begin.txt", "zpayload/zbig.bin", "zpayload/z-end.txt"], znames)
    ctx.observe("zip members", znames)
    zmember = ctx.root / "z-member"
    zmember.mkdir()
    measured("tar xf stored.zip <member>", "-E", "tar", "xf", zipf, "zpayload/z-end.txt", cwd=zmember)
    ctx.check("zip member filter", file_listing(zmember) == ["zpayload/z-end.txt"], file_listing(zmember))
    zdest = ctx.root / "z-full"
    measured("file(ARCHIVE_EXTRACT) stored.zip", f"-DARCHIVE={zipf}", f"-DDEST={zdest}", "-DEXTRA=",
             "-P", ctx.root / "extract.cmake")
    ctx.check("zip big member size and sentinel", big_file_ok(zdest / "zpayload/zbig.bin", size))
    ctx.check("zip small members", (zdest / "zpayload/z-end.txt").read_text() == "ZEND\n")
    remove_tree(zdest)


# --------------------------------------------------------------------------
# s08: try_compile with generator expressions and cross-compiled try_run


@scenario("s08", "try_compile genex imported deps ($<$<BOOL:1>:Vendor::Dep>) + cross try_run preseeded cache")
def s08_try(ctx: Ctx) -> None:
    src = ctx.tree(ctx.root / "src", {
        "CMakeLists.txt": """
            cmake_minimum_required(VERSION 4.0)
            project(tc C)
            add_library(Vendor::Dep INTERFACE IMPORTED)
            set_target_properties(Vendor::Dep PROPERTIES
              INTERFACE_COMPILE_DEFINITIONS "VENDOR_DEP=7"
              INTERFACE_INCLUDE_DIRECTORIES "${CMAKE_SOURCE_DIR}/vendor")
            add_library(Vendor::Top INTERFACE IMPORTED)
            set_target_properties(Vendor::Top PROPERTIES INTERFACE_LINK_LIBRARIES "$<$<BOOL:1>:Vendor::Dep>")
            add_library(Vendor::TopOff INTERFACE IMPORTED)
            set_target_properties(Vendor::TopOff PROPERTIES INTERFACE_LINK_LIBRARIES "$<$<BOOL:0>:Vendor::Dep>")
            try_compile(TC_GENEX SOURCES ${CMAKE_SOURCE_DIR}/probe.c LINK_LIBRARIES Vendor::Top)
            try_compile(TC_PLAIN SOURCES ${CMAKE_SOURCE_DIR}/probe.c LINK_LIBRARIES Vendor::Dep)
            try_compile(TC_OFF SOURCES ${CMAKE_SOURCE_DIR}/probe.c LINK_LIBRARIES Vendor::TopOff)
            message(STATUS "TRY TC_GENEX=${TC_GENEX} TC_PLAIN=${TC_PLAIN} TC_OFF=${TC_OFF}")
            if(TOP_LEVEL_GENEX)
              # try_compile copies only plain imported target names into the
              # test project, so this target name inside a genex is not found.
              try_compile(TC_TOP SOURCES ${CMAKE_SOURCE_DIR}/probe.c LINK_LIBRARIES "$<$<BOOL:1>:Vendor::Dep>")
            endif()
            """,
        "vendor/vendor.h": "#define VENDOR_HEADER 1\n",
        "probe.c": """
            #include "vendor.h"
            #if VENDOR_DEP != 7
            #error "Vendor::Dep usage requirements are missing"
            #endif
            int main(void) { return VENDOR_HEADER - 1; }
            """,
    })
    proc = ctx.configure(src, ctx.root / "b-tc", "Ninja")
    ctx.ok("try_compile configure", proc)
    line = re.findall(r"(?m)^-- TRY .*$", proc.stdout)
    ctx.check("try_compile results", line == ["-- TRY TC_GENEX=TRUE TC_PLAIN=TRUE TC_OFF=FALSE"], line)
    ctx.observe("try_compile", line)
    top = ctx.configure(src, ctx.root / "b-top", "Ninja", "-DTOP_LEVEL_GENEX=ON")
    ctx.fails("top-level genex in LINK_LIBRARIES is not copied", top)
    missing = "links to:\n\n    Vendor::Dep\n\n  but the target was not found." in top.stderr
    ctx.check("top-level genex error names Vendor::Dep", missing, top.tail())
    ctx.observe("top-level genex", [top.returncode != 0, missing])

    cross =ctx.tree(ctx.root / "cross", {
        "CMakeLists.txt": """
            cmake_minimum_required(VERSION 4.0)
            project(cross C)
            try_run(RUN_RES COMPILE_RES SOURCES ${CMAKE_SOURCE_DIR}/runme.c
              RUN_OUTPUT_VARIABLE run_out ARGS "${CMAKE_BINARY_DIR}/executed.flag")
            message(STATUS "TRYRUN cross=${CMAKE_CROSSCOMPILING} compile=${COMPILE_RES} run=${RUN_RES} out=[${run_out}]")
            """,
        "runme.c": """
            #include <stdio.h>
            int main(int argc, char **argv) {
              if (argc > 1) { FILE *f = fopen(argv[1], "w"); if (f) { fputs("ran\\n", f); fclose(f); } }
              puts("real-output");
              return 3;
            }
            """,
        "toolchain.cmake": f"""
            set(CMAKE_SYSTEM_NAME {platform.system()})
            set(CMAKE_SYSTEM_PROCESSOR {platform.machine()})
            """,
    })
    build = ctx.root / "b-cross"
    seeded = ctx.configure(cross, build, "Ninja", f"-DCMAKE_TOOLCHAIN_FILE={cross}/toolchain.cmake",
                           "-DRUN_RES=42", "-DRUN_RES__TRYRUN_OUTPUT=preseeded-output")
    ctx.ok("cross configure with preseeded cache", seeded)
    line = re.findall(r"(?m)^-- TRYRUN .*$", seeded.stdout)
    ctx.check("preseeded try_run results",
              line == ["-- TRYRUN cross=TRUE compile=TRUE run=42 out=[preseeded-output]"], line)
    ctx.check("preseeded binary is not executed", not (build / "executed.flag").exists())
    ctx.observe("preseeded", line)

    unseeded_build = ctx.root / "b-cross-unseeded"
    unseeded = ctx.configure(cross, unseeded_build, "Ninja", f"-DCMAKE_TOOLCHAIN_FILE={cross}/toolchain.cmake")
    ctx.fails("cross try_run without preseeded cache fails", unseeded)
    ctx.check("TryRunResults.cmake written", (unseeded_build / "TryRunResults.cmake").is_file())
    ctx.check("unseeded binary is not executed", not (unseeded_build / "executed.flag").exists())
    if (unseeded_build / "TryRunResults.cmake").is_file():
        sets = re.findall(r"(?m)^set\(\s*(\S+)", (unseeded_build / "TryRunResults.cmake").read_text())
        ctx.check("TryRunResults.cmake lists RUN_RES", "RUN_RES" in sets and "RUN_RES__TRYRUN_OUTPUT" in sets, sets)
        ctx.observe("TryRunResults variables", sets)
    ctx.observe("unseeded cache", cache_entries(unseeded_build, ["RUN_RES", "RUN_RES__TRYRUN_OUTPUT"]))


# --------------------------------------------------------------------------
# s11: CTest matrix


CTEST_PROJECT = {
    "CMakeLists.txt": """
        cmake_minimum_required(VERSION 4.0)
        project(ct C)
        enable_testing()
        set(S ${CMAKE_SOURCE_DIR}/scripts)
        set(W ${CMAKE_BINARY_DIR}/work)
        file(MAKE_DIRECTORY ${W})

        # gtest-style discovery: the build lists the cases and writes add_test() calls.
        add_executable(gt gt.c)
        set(disc ${CMAKE_CURRENT_BINARY_DIR}/gt_tests.cmake)
        add_custom_command(TARGET gt POST_BUILD
          COMMAND ${CMAKE_COMMAND} -DEXE=$<TARGET_FILE:gt> -DOUT=${disc} -P ${S}/discover.cmake)
        set_property(DIRECTORY APPEND PROPERTY TEST_INCLUDE_FILES ${disc})

        # ENVIRONMENT: APPEND adds entries, a second set_tests_properties replaces them.
        add_test(NAME env_append COMMAND ${CMAKE_COMMAND} -DHAVE=A=1|B=2 -DABSENT=X -P ${S}/checkenv.cmake)
        set_tests_properties(env_append PROPERTIES ENVIRONMENT "A=1")
        set_property(TEST env_append APPEND PROPERTY ENVIRONMENT "B=2")
        add_test(NAME env_replace COMMAND ${CMAKE_COMMAND} -DHAVE=Y=2 -DABSENT=X -P ${S}/checkenv.cmake)
        set_tests_properties(env_replace PROPERTIES ENVIRONMENT "X=1")
        set_tests_properties(env_replace PROPERTIES ENVIRONMENT "Y=2")
        # A bare name without "=" in ENVIRONMENT removes the variable.
        add_test(NAME env_unset COMMAND ${CMAKE_COMMAND} -DHAVE=KEEP=outer -DABSENT=FOO -P ${S}/checkenv.cmake)
        set_tests_properties(env_unset PROPERTIES ENVIRONMENT "FOO")

        # DEPENDS appended twice: dep_c must start after dep_a and dep_b.
        foreach(t dep_a dep_b)
          add_test(NAME ${t} COMMAND ${CMAKE_COMMAND} -DNAME=${t} -DLOG=${W}/deps.log -P ${S}/slow_append.cmake)
        endforeach()
        add_test(NAME dep_c COMMAND ${CMAKE_COMMAND} -DLOG=${W}/deps.log -P ${S}/check_deps.cmake)
        set_property(TEST dep_c APPEND PROPERTY DEPENDS dep_a)
        set_property(TEST dep_c APPEND PROPERTY DEPENDS dep_b)

        # Fixtures with FIXTURES_REQUIRED appended twice.
        add_test(NAME fx_setup COMMAND ${CMAKE_COMMAND} -E touch ${W}/fx.ready)
        add_test(NAME fx_setup2 COMMAND ${CMAKE_COMMAND} -E touch ${W}/fx2.ready)
        add_test(NAME fx_use COMMAND ${CMAKE_COMMAND} -DFILES=${W}/fx.ready|${W}/fx2.ready -P ${S}/check_files.cmake)
        add_test(NAME fx_cleanup COMMAND ${CMAKE_COMMAND} -E rm -f ${W}/fx.ready)
        set_tests_properties(fx_setup PROPERTIES FIXTURES_SETUP fx)
        set_tests_properties(fx_setup2 PROPERTIES FIXTURES_SETUP fx2)
        set_tests_properties(fx_cleanup PROPERTIES FIXTURES_CLEANUP fx)
        set_property(TEST fx_use APPEND PROPERTY FIXTURES_REQUIRED fx)
        set_property(TEST fx_use APPEND PROPERTY FIXTURES_REQUIRED fx2)

        # RESOURCE_LOCK appended: the two tests must not overlap.
        foreach(t lock_a lock_b)
          add_test(NAME ${t} COMMAND ${CMAKE_COMMAND} -DLOCK=${W}/lock.held -P ${S}/hold_lock.cmake)
          set_property(TEST ${t} APPEND PROPERTY RESOURCE_LOCK other)
          set_property(TEST ${t} APPEND PROPERTY RESOURCE_LOCK shared_res)
        endforeach()

        # TIMEOUT_AFTER_MATCH: the timeout becomes 1 second after "READY".
        add_test(NAME tam COMMAND sh -c "echo READY; exec sleep 30")
        set_tests_properties(tam PROPERTIES TIMEOUT 120 TIMEOUT_AFTER_MATCH "1;READY")

        # --stop-on-failure with a fixture.
        add_test(NAME sf_setup COMMAND ${CMAKE_COMMAND} -E touch ${W}/sf.setup)
        add_test(NAME sf_fail COMMAND ${CMAKE_COMMAND} -E false)
        add_test(NAME sf_later COMMAND ${CMAKE_COMMAND} -E touch ${W}/sf.later)
        add_test(NAME sf_cleanup COMMAND ${CMAKE_COMMAND} -E touch ${W}/sf.cleanup)
        set_tests_properties(sf_setup PROPERTIES FIXTURES_SETUP sf)
        set_tests_properties(sf_fail sf_later PROPERTIES FIXTURES_REQUIRED sf)
        set_tests_properties(sf_later PROPERTIES DEPENDS sf_fail)
        set_tests_properties(sf_cleanup PROPERTIES FIXTURES_CLEANUP sf)
        """,
    "gt.c": """
        #include <stdio.h>
        #include <string.h>
        static const char *cases[] = {"alpha", "beta", "gamma"};
        int main(int argc, char **argv) {
          if (argc > 1 && strcmp(argv[1], "--list") == 0) {
            for (int i = 0; i < 3; ++i) puts(cases[i]);
            return 0;
          }
          for (int i = 0; i < 3; ++i)
            if (argc > 1 && strcmp(argv[1], cases[i]) == 0) { printf("case %s ok\\n", cases[i]); return 0; }
          return 1;
        }
        """,
    "scripts/discover.cmake": """
        execute_process(COMMAND "${EXE}" --list OUTPUT_VARIABLE names RESULT_VARIABLE rv)
        if(NOT rv EQUAL 0)
          message(FATAL_ERROR "listing failed")
        endif()
        string(REPLACE "\\n" ";" names "${names}")
        set(content "")
        foreach(n IN LISTS names)
          if(NOT n STREQUAL "")
            string(APPEND content "add_test([=[gt.${n}]=] [==[${EXE}]==] ${n})\\n")
            string(APPEND content "set_tests_properties([=[gt.${n}]=] PROPERTIES LABELS discovered PASS_REGULAR_EXPRESSION \\"case ${n} ok\\")\\n")
          endif()
        endforeach()
        file(WRITE "${OUT}" "${content}")
        """,
    "scripts/checkenv.cmake": """
        string(REPLACE "|" ";" HAVE "${HAVE}")
        foreach(pair IN LISTS HAVE)
          string(REGEX MATCH "^([^=]+)=(.*)$" _ "${pair}")
          if(NOT "$ENV{${CMAKE_MATCH_1}}" STREQUAL "${CMAKE_MATCH_2}")
            message(FATAL_ERROR "${CMAKE_MATCH_1}=[$ENV{${CMAKE_MATCH_1}}], want [${CMAKE_MATCH_2}]")
          endif()
        endforeach()
        foreach(name IN LISTS ABSENT)
          if(DEFINED ENV{${name}})
            message(FATAL_ERROR "${name} is set to [$ENV{${name}}]")
          endif()
        endforeach()
        """,
    "scripts/slow_append.cmake": """
        execute_process(COMMAND "${CMAKE_COMMAND}" -E sleep 1)
        file(APPEND "${LOG}" "${NAME}\\n")
        """,
    "scripts/check_deps.cmake": """
        file(STRINGS "${LOG}" lines)
        list(SORT lines)
        if(NOT lines STREQUAL "dep_a;dep_b")
          message(FATAL_ERROR "dependencies did not finish first: [${lines}]")
        endif()
        """,
    "scripts/check_files.cmake": """
        string(REPLACE "|" ";" FILES "${FILES}")
        foreach(f IN LISTS FILES)
          if(NOT EXISTS "${f}")
            message(FATAL_ERROR "missing ${f}")
          endif()
        endforeach()
        """,
    "scripts/hold_lock.cmake": """
        if(EXISTS "${LOCK}")
          message(FATAL_ERROR "the resource lock was not respected")
        endif()
        file(TOUCH "${LOCK}")
        execute_process(COMMAND "${CMAKE_COMMAND}" -E sleep 1)
        file(REMOVE "${LOCK}")
        """,
}

DASHBOARD_PROJECT = {
    "CMakeLists.txt": """
        cmake_minimum_required(VERSION 4.0)
        project(dash C)
        enable_testing()
        file(WRITE ${CMAKE_BINARY_DIR}/build_type.txt "${CMAKE_BUILD_TYPE}")
        add_executable(dapp dapp.c)
        add_test(NAME always COMMAND dapp)
        add_test(NAME debug_only COMMAND ${CMAKE_COMMAND} -E touch ${CMAKE_BINARY_DIR}/debug.flag CONFIGURATIONS Debug)
        add_test(NAME release_only COMMAND ${CMAKE_COMMAND} -E touch ${CMAKE_BINARY_DIR}/release.flag CONFIGURATIONS Release)
        """,
    "dapp.c": "int main(void) { return 0; }\n",
    "dash.cmake": """
        set(CTEST_SOURCE_DIRECTORY "${SRC}")
        set(CTEST_BINARY_DIRECTORY "${BIN}")
        set(CTEST_CMAKE_GENERATOR "Ninja")
        set(CTEST_CONFIGURATION_TYPE Debug)
        set(CTEST_SITE indep)
        set(CTEST_BUILD_NAME dash)
        ctest_start(Experimental)
        ctest_configure(RETURN_VALUE conf_rv)
        ctest_build(RETURN_VALUE build_rv)
        ctest_test(RETURN_VALUE test_rv)
        message("DASH configure=${conf_rv} build=${build_rv} test=${test_rv}")
        """,
}


def junit_cases(path: Path) -> list[str]:
    """Return 'name:status' for each test case of a JUnit file.

    CTest writes status="run" for a test that passed. A failed test also has
    a <failure> element.
    """
    cases = []
    for case in ET.parse(path).getroot().iter("testcase"):
        status = case.get("status", "")
        if case.find("failure") is not None:
            status += "+failure"
        cases.append(f"{case.get('name')}:{status}")
    return sorted(cases)


@scenario("s11", "CTest matrix (dynamic discovery, appended properties, TIMEOUT_AFTER_MATCH, --stop-on-failure, ctest -S, JUnit)")
def s11_ctest(ctx: Ctx) -> None:
    src = ctx.tree(ctx.root / "src", CTEST_PROJECT)
    build = ctx.root / "build"
    ctx.ok("configure", ctx.configure(src, build, "Ninja"))
    ctx.ok("build", ctx.build(build, "Ninja"))
    env = {"FOO": "outer", "KEEP": "outer", "X": None}

    listing = ctx.ctest("--test-dir", build, "-N", env=env)
    names = re.findall(r"(?m)^\s*Test\s+#\d+: (\S+)$", listing.stdout)
    ctx.check("discovered tests are listed", {"gt.alpha", "gt.beta", "gt.gamma"} <= set(names), names)
    ctx.observe("test list", names)

    junit = ctx.root / "junit.xml"
    main = ctx.ctest("--test-dir", build, "-j4", "-E", "^(tam|sf_.*)$", "--output-on-failure",
                     "--output-junit", junit, env=env)
    ctx.check("main test set passes", main.ok and "100% tests passed" in main.stdout, main.tail(60))
    if junit.is_file():
        cases = junit_cases(junit)
        ctx.check("JUnit lists every test as passed", len(cases) == 15 and all(c.endswith(":run") for c in cases),
                  cases)
        ctx.observe("junit", cases)
    else:
        ctx.check("JUnit file written", False, str(junit))
    labels = ctx.ctest("--test-dir", build, "-L", "discovered", env=env)
    ctx.check("label of discovered tests", labels.ok and "100% tests passed out of 3" in labels.stdout, labels.tail())

    tam = ctx.ctest("--test-dir", build, "-R", "^tam$", env=env)
    ctx.fails("TIMEOUT_AFTER_MATCH test fails", tam)
    ctx.check("TIMEOUT_AFTER_MATCH reports a timeout", "***Timeout" in tam.stdout, tam.tail())
    ctx.check("TIMEOUT_AFTER_MATCH ends early", tam.seconds < 15, f"{tam.seconds:.1f}s")
    ctx.observe("tam", [line.split("  ")[0] for line in tam.stdout.splitlines() if "tam" in line and "Test" in line])

    work = build / "work"
    stop = ctx.ctest("--test-dir", build, "-R", "^sf_", "--stop-on-failure", env=env)
    ran = [name for name in ("setup", "later", "cleanup") if (work / f"sf.{name}").exists()]
    ctx.fails("--stop-on-failure run fails", stop)
    ctx.check("--stop-on-failure ran the setup only", ran == ["setup"], ran)
    ctx.observe("stop-on-failure", ran)
    for name in ("setup", "later", "cleanup"):
        (work / f"sf.{name}").unlink(missing_ok=True)
    full = ctx.ctest("--test-dir", build, "-R", "^sf_", env=env)
    ran = [name for name in ("setup", "later", "cleanup") if (work / f"sf.{name}").exists()]
    ctx.fails("run without --stop-on-failure fails", full)
    ctx.check("without --stop-on-failure the cleanup runs", "setup" in ran and "cleanup" in ran, ran)
    ctx.observe("no stop-on-failure", ran)

    dsrc = ctx.tree(ctx.root / "dash", DASHBOARD_PROJECT)
    dbin = ctx.root / "dash-build"
    dash = ctx.ctest("-S", dsrc / "dash.cmake", f"-DSRC={dsrc}", f"-DBIN={dbin}", "-VV")
    ctx.ok("ctest -S dashboard", dash, required=False)
    ctx.check("dashboard steps succeed", "DASH configure=0 build=0 test=0" in dash.output, dash.tail())
    build_type = (dbin / "build_type.txt").read_text() if (dbin / "build_type.txt").is_file() else None
    ctx.check("ctest_configure passes CMAKE_BUILD_TYPE", build_type == "Debug", build_type)
    ctx.check("ctest_test uses CTEST_CONFIGURATION_TYPE",
              (dbin / "debug.flag").exists() and not (dbin / "release.flag").exists())
    ctx.observe("dashboard", [build_type, (dbin / "debug.flag").exists(), (dbin / "release.flag").exists()])


# --------------------------------------------------------------------------
# s12: CPack matrix


CPACK_PROJECT = {
    "CMakeLists.txt": """
        cmake_minimum_required(VERSION 4.0)
        project(pk VERSION 1.0 LANGUAGES C)
        add_executable(tool tool.c)
        target_compile_options(tool PRIVATE -g)
        install(TARGETS tool RUNTIME DESTINATION bin COMPONENT runtime)
        install(FILES doc.txt DESTINATION share/doc COMPONENT docs)
        install(FILES debug_only.txt DESTINATION share/doc CONFIGURATIONS Debug COMPONENT docs)
        set(CPACK_PACKAGE_NAME pk)
        set(CPACK_SYSTEM_NAME sys)
        set(CPACK_GENERATOR "TGZ;ZIP")
        set(CPACK_ARCHIVE_COMPONENT_INSTALL ON)
        set(CPACK_STRIP_FILES ON)
        set(CPACK_PRE_BUILD_SCRIPTS ${CMAKE_SOURCE_DIR}/pre.cmake)
        set(CPACK_PRE_LOG ${CMAKE_BINARY_DIR}/pre.log)
        include(CPack)
        """,
    "tool.c": """
        #include <stdio.h>
        static int __attribute__((noinline)) local_marker_fn(int x) { return x * 3; }
        int main(int argc, char **argv) { (void)argv; printf("tool %d\\n", local_marker_fn(argc + 1)); return 0; }
        """,
    "doc.txt": "documentation\n",
    "debug_only.txt": "debug notes\n",
    "pre.cmake": """
        if(DEFINED PRE_SCRIPT_RAN)
          set(leak "leaked")
        else()
          set(leak "clean")
        endif()
        set(PRE_SCRIPT_RAN 1)
        file(APPEND "${CPACK_PRE_LOG}" "pre ${CPACK_GENERATOR} config=${CPACK_BUILD_CONFIG} ${leak}\\n")
        """,
}


def archive_names(path: Path) -> list[str]:
    """List the file members of a .tar.gz or .zip archive.

    On macOS, libarchive adds an AppleDouble "._<name>" member for each file
    with extended attributes. macOS adds com.apple.provenance to new files, so
    these members depend on the host and are not part of the comparison.
    """
    if path.suffix == ".zip":
        with zipfile.ZipFile(path) as zf:
            names = [name for name in zf.namelist() if not name.endswith("/")]
    else:
        with tarfile.open(path) as tar:
            names = [m.name for m in tar.getmembers() if not m.isdir()]
    return sorted(name for name in names if not Path(name).name.startswith("._"))


@scenario("s12", "CPack matrix (component TGZ/ZIP, INSTALLED_DIRECTORIES + PACKAGING_INSTALL_PREFIX, PRE_BUILD_SCRIPTS, STRIP, -C)")
def s12_cpack(ctx: Ctx) -> None:
    if "Ninja Multi-Config" not in generators_available(ctx.options):
        raise ScenarioSkip("needs Ninja")
    src = ctx.tree(ctx.root / "src", CPACK_PROJECT)
    build = ctx.root / "build"
    ctx.ok("configure", ctx.configure(src, build, "Ninja Multi-Config"))
    for config in ("Debug", "Release"):
        ctx.ok(f"build {config}", ctx.build(build, "Ninja Multi-Config", config=config))
    contents: dict[str, Any] = {}
    for config in ("Debug", "Release"):
        out = ctx.root / f"pkg-{config}"
        (build / "pre.log").unlink(missing_ok=True)
        proc = ctx.cpack("-C", config, "-B", out, cwd=build)
        ctx.ok(f"cpack -C {config}", proc, required=False)
        packages = sorted(p.name for p in out.glob("*") if p.is_file())
        want = sorted(f"pk-1.0-sys-{comp}.{ext}" for comp in ("docs", "runtime") for ext in ("tar.gz", "zip"))
        ctx.check(f"{config}: one archive per component and generator", packages == want, packages)
        for name in packages:
            contents[f"{config}/{name}"] = archive_names(out / name)
        docs = contents.get(f"{config}/pk-1.0-sys-docs.tar.gz", [])
        has_debug = any(name.endswith("share/doc/debug_only.txt") for name in docs)
        ctx.check(f"{config}: CONFIGURATIONS Debug file follows -C", has_debug == (config == "Debug"), docs)
        log = (build / "pre.log").read_text().splitlines() if (build / "pre.log").is_file() else []
        ctx.check(f"{config}: pre-build script runs once per generator with a clean scope",
                  sorted(log) == sorted([f"pre TGZ config={config} clean", f"pre ZIP config={config} clean"]), log)
        ctx.observe(f"{config}: pre-build log", sorted(log))
        runtime = out / "pk-1.0-sys-runtime.tar.gz"
        if runtime.is_file():
            unpack = ctx.root / f"unpack-{config}"
            with tarfile.open(runtime) as tar:
                tar.extractall(unpack, filter="tar")
            tools = list(unpack.rglob("bin/tool"))
            if tools:
                run = ctx.run([tools[0]])
                ctx.check(f"{config}: packaged tool runs", run.stdout == "tool 6\n", run.tail())
                nm = subprocess.run(["nm", str(tools[0])], capture_output=True, text=True, timeout=60).stdout
                ctx.check(f"{config}: CPACK_STRIP_FILES strips local symbols", "local_marker_fn" not in nm, nm[:500])
            else:
                ctx.check(f"{config}: runtime archive has bin/tool", False, archive_names(runtime))
    built_nm = subprocess.run(["nm", str(build / "Debug/tool")], capture_output=True, text=True, timeout=60).stdout
    ctx.check("the build-tree tool keeps local symbols", "local_marker_fn" in built_nm, built_nm[:500])
    ctx.observe("archives", contents)

    ctx.write("payload/data/info.txt", "installed directory payload\n")
    ctx.write("payload/data/nested/deep.txt", "deep\n")
    config = ctx.write("installed.cmake", f"""
        set(CPACK_GENERATOR TGZ)
        set(CPACK_PACKAGE_NAME inst)
        set(CPACK_PACKAGE_VERSION 2.0)
        set(CPACK_PACKAGE_DESCRIPTION "Installed directory package")
        set(CPACK_SYSTEM_NAME sys)
        set(CPACK_PACKAGE_FILE_NAME inst-pkg)
        set(CPACK_INSTALL_CMAKE_PROJECTS "")
        set(CPACK_INSTALLED_DIRECTORIES "{ctx.root}/payload;share/payload")
        set(CPACK_PACKAGING_INSTALL_PREFIX "/opt/inst")
        set(CPACK_INCLUDE_TOPLEVEL_DIRECTORY OFF)
        """)
    out = ctx.root / "pkg-installed"
    proc = ctx.cpack("--config", config, "-B", out, cwd=ctx.root)
    ctx.ok("cpack CPACK_INSTALLED_DIRECTORIES", proc, required=False)
    archive = out / "inst-pkg.tar.gz"
    names = archive_names(archive) if archive.is_file() else []
    ctx.check("installed directory under the packaging prefix", names == [
        "opt/inst/share/payload/data/info.txt", "opt/inst/share/payload/data/nested/deep.txt"], names)
    ctx.observe("installed directories", names)


# --------------------------------------------------------------------------
# s13: Fortran module graph and unity builds


UNITY_FILES = {
    "CMakeLists.txt": """
        cmake_minimum_required(VERSION 4.0)
        project(un C)
        add_subdirectory(lib)
        add_executable(unapp main.c)
        target_link_libraries(unapp PRIVATE unilib grouped)
        """,
    "lib/CMakeLists.txt": """
        add_library(unilib STATIC a.c b.c c.c d.c e.c)
        set_target_properties(unilib PROPERTIES UNITY_BUILD ON UNITY_BUILD_BATCH_SIZE 2
          UNITY_BUILD_RELOCATABLE TRUE UNITY_BUILD_UNIQUE_ID UNITY_ID)
        set_source_files_properties(e.c PROPERTIES SKIP_UNITY_BUILD_INCLUSION ON)
        add_library(grouped STATIC g1.c g2.c g3.c)
        set_target_properties(grouped PROPERTIES UNITY_BUILD ON UNITY_BUILD_MODE GROUP
          UNITY_BUILD_RELOCATABLE TRUE)
        set_source_files_properties(g1.c g2.c PROPERTIES UNITY_GROUP first)
        set_source_files_properties(g3.c PROPERTIES UNITY_GROUP second)
        """,
    # a.c and e.c both define helper(). e.c is not in a unity file, so they do not collide.
    "lib/a.c": "static int helper(void) { return 1; }\nint fa(void) { return helper(); }\n",
    "lib/b.c": "int fb(void) { return 2; }\n",
    "lib/c.c": "static int only_c(void) { return 3; }\nint fc(void) { return only_c(); }\n",
    "lib/d.c": "int fd(void) { return 4; }\n",
    "lib/e.c": "static int helper(void) { return 5; }\nint fe(void) { return helper(); }\n",
    "lib/g1.c": "int g1(void) { return 10; }\n",
    "lib/g2.c": "int g2(void) { return 20; }\n",
    "lib/g3.c": "int g3(void) { return 30; }\n",
    "main.c": """
        #include <stdio.h>
        int fa(void); int fb(void); int fc(void); int fd(void); int fe(void);
        int g1(void); int g2(void); int g3(void);
        int main(void) {
          printf("unity %d %d\\n", fa() + fb() + fc() + fd() + fe(), g1() + g2() + g3());
          return 0;
        }
        """,
}

FORTRAN_FILES = {
    "CMakeLists.txt": """
        cmake_minimum_required(VERSION 4.0)
        project(fmod Fortran)
        add_library(fmods STATIC mod_c.f90 mod_b.f90 mod_a.f90)
        add_executable(fapp main.f90)
        target_link_libraries(fapp PRIVATE fmods)
        """,
    "mod_a.f90": "module mod_a\n  implicit none\n  integer, parameter :: base = 40\nend module mod_a\n",
    "mod_b.f90": ("module mod_b\n  use mod_a\n  implicit none\ncontains\n  integer function plus_one()\n"
                  "    plus_one = base + 1\n  end function plus_one\nend module mod_b\n"),
    "mod_c.f90": ("module mod_c\n  use mod_b\n  implicit none\ncontains\n  integer function answer()\n"
                  "    answer = plus_one() + 1\n  end function answer\nend module mod_c\n"),
    "main.f90": "program fapp\n  use mod_c\n  implicit none\n  print '(A,I0)', 'fortran ', answer()\nend program fapp\n",
}


def unity_sources(build: Path) -> dict[str, str]:
    """Return the generated unity source files of a build tree."""
    return {
        p.relative_to(build).as_posix(): p.read_text()
        for p in sorted(build.rglob("Unity/unity_*"))
        if p.is_file() and p.suffix in (".c", ".cxx")
    }


@scenario("s13", "Fortran module graph (if gfortran) + UNITY_BUILD batch/group + relocatable unity subdirectory")
def s13_unity_fortran(ctx: Ctx) -> None:
    gens = [g for g in ("Ninja", "Unix Makefiles") if g in generators_available(ctx.options)]
    for gen in gens:
        tag = slug(gen)
        first: dict[str, str] | None = None
        for place in ("src-one", "relocated source dir"):
            src = ctx.tree(ctx.root / tag / place, UNITY_FILES)
            build = ctx.root / tag / f"build {place}"
            try:
                ctx.ok(f"{gen} [{place}]: configure", ctx.configure(src, build, gen))
                ctx.ok(f"{gen} [{place}]: build", ctx.build(build, gen))
            except StepFailed:
                break
            run = ctx.run([build / "unapp"])
            ctx.check(f"{gen} [{place}]: output", run.stdout == "unity 15 60\n", run.tail())
            unity = unity_sources(build)
            names = sorted(Path(name).name for name in unity)
            ctx.check(f"{gen} [{place}]: unity files", names == [
                "unity_0_c.c", "unity_1_c.c", "unity_first_c.c", "unity_second_c.c"], names)
            text = "".join(unity.values())
            ctx.check(f"{gen} [{place}]: relocatable unity files have no absolute paths",
                      str(ctx.root) not in text, text)
            ctx.check(f"{gen} [{place}]: SKIP_UNITY_BUILD_INCLUSION source is not included",
                      "e.c" not in text and "a.c" in text, text)
            ctx.check(f"{gen} [{place}]: UNITY_BUILD_UNIQUE_ID", "#define UNITY_ID" in text, text)
            if first is None:
                first = unity
                ctx.observe(f"{gen}: unity sources", unity)
            else:
                ctx.check(f"{gen}: unity sources are identical after relocation", unity == first,
                          json.dumps(unity)[:2000])
    if not shutil.which("gfortran"):
        ctx.skip("gfortran is not installed: Fortran module graph not tested")
        return
    for gen in gens:
        src = ctx.tree(ctx.root / f"fortran-{slug(gen)}", FORTRAN_FILES)
        build = ctx.root / f"fortran-build-{slug(gen)}"
        try:
            ctx.ok(f"Fortran {gen}: configure", ctx.configure(src, build, gen))
            ctx.ok(f"Fortran {gen}: build -j4", ctx.cmake("--build", build, "-j", "4"))
        except StepFailed:
            continue
        run = ctx.run([build / "fapp"])
        ctx.check(f"Fortran {gen}: output", run.stdout.strip() == "fortran 42", run.tail())
        touch_later(src / "mod_a.f90", FORTRAN_FILES["mod_a.f90"].replace("40", "50"))
        ctx.ok(f"Fortran {gen}: rebuild after module edit", ctx.cmake("--build", build, "-j", "4"), required=False)
        run = ctx.run([build / "fapp"])
        ctx.check(f"Fortran {gen}: module edit propagates", run.stdout.strip() == "fortran 52", run.tail())
        ctx.observe(f"Fortran {gen}: modules", sorted(p.name for p in build.rglob("*.mod")))


# --------------------------------------------------------------------------
# s14: cold, warm, and oracle determinism


DETERMINISM_FILES = {
    "CMakeLists.txt": """
        cmake_minimum_required(VERSION 4.0)
        project(det VERSION 3.1 LANGUAGES C CXX)
        set(CMAKE_EXPORT_COMPILE_COMMANDS ON)
        enable_testing()
        option(DET_FEATURE "feature" ON)
        configure_file(version.h.in ${CMAKE_BINARY_DIR}/version.h @ONLY)
        add_library(detcore STATIC core.c)
        target_include_directories(detcore PUBLIC ${CMAKE_BINARY_DIR} include)
        target_compile_definitions(detcore PUBLIC $<$<BOOL:${DET_FEATURE}>:DET_FEATURE=1> DET_NAME="det")
        add_library(detshared SHARED shared.cpp)
        target_link_libraries(detshared PRIVATE detcore)
        target_compile_features(detshared PRIVATE cxx_std_17)
        add_executable(detapp main.cpp)
        target_link_libraries(detapp PRIVATE detshared detcore)
        add_test(NAME detapp COMMAND detapp)
        set_tests_properties(detapp PROPERTIES PASS_REGULAR_EXPRESSION "^det 3.1 7 1")
        file(GENERATE OUTPUT targets.txt CONTENT
          "core=$<TARGET_FILE_NAME:detcore>\\nshared=$<TARGET_FILE_NAME:detshared>\\nsoname=$<TARGET_SONAME_FILE_NAME:detshared>\\napp=$<TARGET_FILE_NAME:detapp>\\ndefs=$<TARGET_PROPERTY:detapp,COMPILE_DEFINITIONS>\\nincs=$<TARGET_PROPERTY:detapp,INCLUDE_DIRECTORIES>\\nfeatures=$<TARGET_PROPERTY:detshared,COMPILE_FEATURES>\\n")
        get_property(vars DIRECTORY PROPERTY VARIABLES)
        list(FILTER vars INCLUDE REGEX "^(PROJECT_|det_|CMAKE_PROJECT_)")
        list(SORT vars)
        set(dump "")
        foreach(v IN LISTS vars)
          string(APPEND dump "${v}=${${v}}\\n")
        endforeach()
        file(WRITE ${CMAKE_BINARY_DIR}/vars.txt "${dump}")
        """,
    "version.h.in": "#define DET_VERSION \"@PROJECT_VERSION@\"\n#define DET_MAJOR @PROJECT_VERSION_MAJOR@\n",
    "include/det.h": """
        #ifdef __cplusplus
        extern "C" {
        #endif
        int det_core(void);
        int det_shared(void);
        #ifdef __cplusplus
        }
        #endif
        """,
    "core.c": '#include "det.h"\n#include "version.h"\nint det_core(void) { return DET_MAJOR + 4; }\n',
    "shared.cpp": '#include "det.h"\nextern "C" int det_shared(void) { return det_core(); }\n',
    "main.cpp": """
        #include <cstdio>
        #include "det.h"
        #include "version.h"
        int main() { std::printf("det %s %d %d\\n", DET_VERSION, det_shared(), DET_FEATURE); return 0; }
        """,
}


def compile_db(build: Path) -> list[dict[str, str]]:
    """Read compile_commands.json sorted by file."""
    path = build / "compile_commands.json"
    if not path.is_file():
        return []
    entries = json.loads(path.read_text())
    return sorted(
        ({key: entry.get(key, "") for key in ("directory", "file", "output", "command")} for entry in entries),
        key=lambda entry: entry["file"],
    )


def determinism_snapshot(build: Path) -> dict[str, Any]:
    """Collect the configure outputs that must not depend on history."""
    def text(name: str) -> str:
        path = build / name
        return path.read_text() if path.is_file() else "<missing>"
    return {
        "targets.txt": text("targets.txt"),
        "vars.txt": text("vars.txt"),
        "version.h": text("version.h"),
        "compile_commands": compile_db(build),
        "cache": cache_entries(build, ["DET_FEATURE", "CMAKE_BUILD_TYPE", "CMAKE_PROJECT_NAME",
                                       "det_BINARY_DIR", "det_SOURCE_DIR", "det_IS_TOP_LEVEL"]),
    }


@scenario("s14", "Cold vs warm vs oracle determinism (configure outputs, compile database, binaries)")
def s14_determinism(ctx: Ctx) -> None:
    src = ctx.tree(ctx.root / "src", DETERMINISM_FILES)
    gens = [g for g in ("Ninja", "Unix Makefiles") if g in generators_available(ctx.options)]
    for gen in gens:
        tag = slug(gen)
        cold_dir = ctx.root / f"cold-{tag}"
        try:
            ctx.ok(f"{gen}: cold configure", ctx.configure(src, cold_dir, gen))
        except StepFailed:
            continue
        cold = determinism_snapshot(cold_dir)
        ctx.check(f"{gen}: compile database has 3 entries", len(cold["compile_commands"]) == 3, cold["compile_commands"])
        warm_proc = ctx.cmake(cold_dir)
        ctx.ok(f"{gen}: warm reconfigure", warm_proc, required=False)
        warm = determinism_snapshot(cold_dir)
        ctx.check(f"{gen}: warm reconfigure equals cold configure", warm == cold,
                  json.dumps(diff_observations(cold, warm))[:2000])
        again_dir = ctx.root / f"again-{tag}"
        ctx.ok(f"{gen}: second cold configure", ctx.configure(src, again_dir, gen), required=False)
        again = json.loads(json.dumps(determinism_snapshot(again_dir)).replace(f"again-{tag}", f"cold-{tag}"))
        ctx.check(f"{gen}: second cold configure equals the first", again == cold,
                  json.dumps(diff_observations(cold, again))[:2000])
        ctx.ok(f"{gen}: build", ctx.build(cold_dir, gen), required=False)
        run = ctx.run([cold_dir / "detapp"])
        ctx.check(f"{gen}: binary output", run.stdout == "det 3.1 7 1\n", run.tail())
        test = ctx.ctest("--test-dir", cold_dir)
        ctx.check(f"{gen}: ctest", test.ok and "100% tests passed" in test.stdout, test.tail())
        flip = ctx.cmake("-DDET_FEATURE=OFF", cold_dir)
        ctx.ok(f"{gen}: reconfigure with an option change", flip, required=False)
        ctx.check(f"{gen}: option change reaches file(GENERATE)",
                  "DET_FEATURE=1" not in (cold_dir / "targets.txt").read_text())
        back = ctx.cmake("-DDET_FEATURE=ON", cold_dir)
        ctx.ok(f"{gen}: reconfigure back", back, required=False)
        ctx.check(f"{gen}: reconfigure back restores the snapshot", determinism_snapshot(cold_dir) == cold)
        ctx.observe(f"{gen}: snapshot", cold)
        ctx.observe(f"{gen}: output", run.stdout)


# @@SCENARIOS@@


# --------------------------------------------------------------------------
# Runner


def run_one(sc: Scenario, tools: Toolset, root: Path, options: Options) -> dict[str, Any]:
    """Run a scenario with one tool set and return its record."""
    ctx = Ctx(tools, root, options)
    start = time.monotonic()
    error = ""
    try:
        sc.func(ctx)
    except StepFailed:
        pass
    except ScenarioSkip:
        raise
    except Exception as exc:  # A scenario bug must not stop the other scenarios.
        error = f"{type(exc).__name__}: {exc}"
        ctx.check("scenario raised no exception", False, error)
    return {
        "seconds": round(time.monotonic() - start, 2),
        "passed": all(check.ok for check in ctx.checks) and bool(ctx.checks),
        "checks": [check.__dict__ for check in ctx.checks],
        "skips": ctx.skips,
        "observations": ctx.observations,
        "transcript": ctx.transcript,
    }


def diff_observations(left: Mapping[str, Any], right: Mapping[str, Any]) -> list[dict[str, Any]]:
    """Compare two observation maps and return the differences."""
    out = []
    for key in sorted(set(left) | set(right)):
        if left.get(key, "<missing>") != right.get(key, "<missing>"):
            out.append({"key": key, "oracle": left.get(key, "<missing>"), "rcmake": right.get(key, "<missing>")})
    return out


def run_scenario(sc: Scenario, toolsets: Sequence[Toolset], sandbox: Path, options: Options) -> dict[str, Any]:
    """Run a scenario with each tool set and compare the results."""
    record: dict[str, Any] = {"id": sc.sid, "title": sc.title, "runs": {}, "differences": []}
    start = time.monotonic()
    reason = sc.requires(options) if sc.requires else None
    if reason:
        record.update(status="skip", reason=reason, seconds=0.0)
        return record
    try:
        for tools in toolsets:
            root = sandbox / sc.sid / (tools.label if tools.label in TOOLSET_DIRS else slug(tools.label))
            try:
                record["runs"][tools.label] = run_one(sc, tools, root, options)
            finally:
                if not options.keep:
                    remove_tree(root)
    except ScenarioSkip as exc:
        record.update(status="skip", reason=str(exc), seconds=round(time.monotonic() - start, 2))
        return record
    passed = all(run["passed"] for run in record["runs"].values())
    if len(toolsets) == 2:
        first, second = (record["runs"][t.label]["observations"] for t in toolsets)
        record["differences"] = diff_observations(first, second)
        passed = passed and not record["differences"]
    record["status"] = "pass" if passed else "fail"
    record["seconds"] = round(time.monotonic() - start, 2)
    return record


def print_record(record: Mapping[str, Any], verbose: bool) -> None:
    """Print one scenario result."""
    status = record["status"].upper()
    print(f"{status:4} {record['id']} {record['title']} ({record.get('seconds', 0):.1f}s)", flush=True)
    if record["status"] == "skip":
        print(f"     skipped: {record.get('reason', '')}")
        return
    for label, run in record["runs"].items():
        passed = sum(1 for check in run["checks"] if check["ok"])
        total = len(run["checks"])
        print(f"     {label}: {passed}/{total} checks")
        for note in run["skips"]:
            print(f"       skipped sub-case: {note}")
        for check in run["checks"]:
            if not check["ok"] or verbose:
                mark = "ok  " if check["ok"] else "FAIL"
                print(f"       {mark} {check['name']}")
                if check["detail"]:
                    print(textwrap.indent(check["detail"], "            "))
    for diff in record["differences"]:
        print(f"     DIFF {diff['key']}")
        print(f"       oracle: {json.dumps(diff['oracle'])[:1500]}")
        print(f"       rcmake: {json.dumps(diff['rcmake'])[:1500]}")


def select(ids: str | None) -> list[Scenario]:
    """Select scenarios by a comma-separated list of ids."""
    if not ids:
        return list(SCENARIOS)
    wanted = [part.strip() for part in ids.split(",") if part.strip()]
    known = {sc.sid: sc for sc in SCENARIOS}
    unknown = [sid for sid in wanted if sid not in known]
    if unknown:
        raise SystemExit(f"Unknown scenario id: {', '.join(unknown)}")
    return [known[sid] for sid in wanted]


def parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    """Parse the command line."""
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--oracle", type=Path, default=os.environ.get("RCMAKE_ORACLE_CMAKE"),
                        help="CMake 4.5.1 cmake program (default: $RCMAKE_ORACLE_CMAKE)")
    parser.add_argument("--stage", type=Path, default=ROOT / "target/stage",
                        help="Staged Rcmake tree with bin/cmake, bin/ctest, bin/cpack")
    parser.add_argument("--oracle-only", action="store_true", help="Run the checks with the oracle only")
    parser.add_argument("--only", help="Comma-separated scenario ids, for example s01,s05")
    parser.add_argument("--list", action="store_true", help="List the scenarios and exit")
    parser.add_argument("--json", type=Path, help="Write the scorecard as JSON to this file")
    parser.add_argument("--work-root", default="/tmp", help="Parent directory of the sandbox (default: /tmp)")
    parser.add_argument("--keep", action="store_true", help="Keep the sandbox for inspection")
    parser.add_argument("--jobs", type=int, default=4, help="Parallel build jobs (default: 4)")
    parser.add_argument("--timeout", type=float, default=DEFAULT_TIMEOUT, help="Timeout of one command in seconds")
    parser.add_argument("--no-xcode", action="store_true", help="Do not use the Xcode generator")
    parser.add_argument("--archive-mib", type=int, default=520,
                        help="Decoded size of the large archive member in MiB (default: 520)")
    parser.add_argument("--verbose", action="store_true", help="Print every check")
    return parser.parse_args(argv)


def main(argv: Sequence[str] | None = None) -> int:
    """Run the suite and return the process exit status."""
    args = parse_args(argv)
    if args.list:
        for sc in SCENARIOS:
            print(f"{sc.sid}  {sc.title}")
        return 0
    if not args.oracle:
        print("Set --oracle or RCMAKE_ORACLE_CMAKE.", file=sys.stderr)
        return 2
    toolsets = [Toolset.from_cmake("oracle", args.oracle)]
    if not args.oracle_only:
        toolsets.append(Toolset.from_stage(args.stage))
    for tools in toolsets:
        missing = tools.missing()
        if missing:
            print(f"{tools.label}: missing program(s): {', '.join(missing)}", file=sys.stderr)
            return 2
    options = Options(jobs=max(1, args.jobs), timeout=args.timeout, xcode=not args.no_xcode,
                      archive_mib=args.archive_mib, keep=args.keep)
    scenarios = select(args.only)
    install_signal_handlers()
    records = []
    started = time.monotonic()
    with Sandbox(args.work_root, keep=args.keep) as sandbox:
        if args.keep:
            print(f"Sandbox: {sandbox.path}")
        for sc in scenarios:
            record = run_scenario(sc, toolsets, sandbox.path, options)
            print_record(record, args.verbose)
            records.append(record)
    summary = {status: sum(1 for r in records if r["status"] == status) for status in ("pass", "fail", "skip")}
    card = {
        "mode": "oracle-only" if args.oracle_only else "differential",
        "platform": f"{platform.system()} {platform.machine()}",
        "toolsets": {t.label: str(t.cmake) for t in toolsets},
        "seconds": round(time.monotonic() - started, 1),
        "summary": summary,
        "scenarios": records,
    }
    print(f"\nScorecard ({card['mode']}): {summary['pass']} pass, {summary['fail']} fail, "
          f"{summary['skip']} skip in {card['seconds']}s")
    if args.json:
        args.json.parent.mkdir(parents=True, exist_ok=True)
        args.json.write_text(json.dumps(card, indent=2) + "\n", encoding="utf-8")
    return 1 if summary["fail"] else 0


def load_extra_scenarios() -> None:
    """Register the scenarios of independent_scenarios_extra.py.

    The extra module imports this module by name. When this file runs as a
    script, its module name is __main__, so the alias makes the import give
    this module and not a second copy with a second registry.
    """
    sys.modules.setdefault("independent_verify", sys.modules[__name__])
    here = str(Path(__file__).resolve().parent)
    if here not in sys.path:
        sys.path.insert(0, here)
    import independent_scenarios_extra  # noqa: F401  # pylint: disable=import-outside-toplevel


load_extra_scenarios()


if __name__ == "__main__":
    sys.exit(main())

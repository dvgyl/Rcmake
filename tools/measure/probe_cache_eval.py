#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Check compiler probe invalidation and shared-store recovery on macOS or Linux.

Source env/env.sh before use. Use --smoke to check baseline compiler behavior
on a staged binary. Gate mode requires probe caching, libzstd and a binary
that accepts a native dynamic-loader hook.
"""

from __future__ import annotations

import argparse
from collections.abc import Callable
from contextlib import ExitStack
import ctypes
from ctypes.util import find_library
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
import stat
import subprocess
import sys
import tempfile
import time

try:
    from ._proc import kill_group
    from .mutation_eval import diff_bytes, normalize
    from .oracle_prop_test import clean_environment, executable, run_command, write_json
except ImportError:
    from _proc import kill_group
    from mutation_eval import diff_bytes, normalize
    from oracle_prop_test import clean_environment, executable, run_command, write_json


WORKERS = 8
TRIGGERS = {
    "cpath": '#include <rcmake_probe_cpath.h>\n',
    "cpath-add": '#include <rcmake_probe_cpath_add.h>\n',
    "has-include": ('#if !__has_include(<rcmake_probe_optional.h>)\n'
                    'int rcmake_probe_header_required = ;\n#endif\n'),
    "header-shadow": ('#include <limits.h>\n#ifndef RCMAKE_PROBE_SHADOW\n'
                      'int rcmake_probe_shadow_required = ;\n#endif\n'),
    "lang": "#ifndef RCMAKE_PROBE_POSIX\nint rcmake_probe_posix_required = ;\n#endif\n",
    "compiler": "#ifndef RCMAKE_PROBE_SWAP\nint rcmake_probe_wrapper_change_required = ;\n#endif\n",
    "header-root": '#include <rcmake_probe_root.h>\n',
    "header-subdir": '#include <existing/rcmake_probe_subdir.h>\n',
    "library": "int probe_archive(void);\n",
    "toolchain": "#ifndef RCMAKE_PROBE_TOOLCHAIN\nint rcmake_probe_toolchain_change_required = ;\n#endif\n",
    "developer-dir": "#ifndef RCMAKE_PROBE_DEVELOPER\nint rcmake_probe_developer_change_required = ;\n#endif\n",
    "transitive": '#include <rcmake_probe_outer.h>\n',
}
PROBE_STATUS = b"-- Probe results "
COMPILER_MARKER = b"Compiler reached the probe"
PRIVATE_EVIDENCE = (b".rcmake-compile-dependencies", b".rcmake-link-dependencies",
                    b".rcmake-source-dependencies", b".rcmake-failed-dependencies")
SCRATCH_ID = re.compile(rb"\b(?:TryCompile-[A-Za-z0-9]+|cmTC_[A-Za-z0-9]+)\b")
PANIC = re.compile(rb"(?im)thread [^\n]+ panicked at|(?:^|\n)fatal runtime error:|RUST_BACKTRACE")
PARITY_KEYS = ("result", "probe stdout", "stdout", "stderr")
DIGEST = re.compile(r"[0-9a-f]{64}")
ENTRY_LIMIT = 16 * 1024 * 1024
BLOB_LIMIT = 512 * 1024 * 1024
ARTIFACT_LIMIT = 4 * 1024 * 1024


@dataclass(frozen=True)
class Fixture:
    """Keep the paths and compiler configuration for one test fixture."""

    root: Path
    compiler: str
    probes: int
    alternate_developer_dir: str = ""

    @property
    def source(self) -> Path:
        return self.root / "source"

    @property
    def log(self) -> Path:
        return self.root / "compiler.log"

    def wrapper(self, swapped: bool = False) -> None:
        """Log attempted fixture compilations and invoke the system compiler."""
        swap = '    set -- -DRCMAKE_PROBE_SWAP=1 "$@"\n' if swapped else ""
        script = f"""#!/bin/sh
compile=no
source=
for arg do
    case "$arg" in
        -c) compile=yes ;;
        */probe_*.c|probe_*.c) source=${{arg##*/}} ;;
    esac
done
if [ "$compile" = yes ] && [ -n "$source" ]; then
    printf '%s\\t%s\\n' "$PWD" "$source" >> {shlex.quote(str(self.log))}
    printf '%s\\n' '{COMPILER_MARKER.decode()}' >&2
    if [ "${{LANG-}}" = POSIX ]; then
        set -- -DRCMAKE_PROBE_POSIX=1 "$@"
    fi
    if [ -n "${{DEVELOPER_DIR-}}" ] && [ "$DEVELOPER_DIR" = {shlex.quote(self.alternate_developer_dir)} ]; then
        set -- -DRCMAKE_PROBE_DEVELOPER=1 "$@"
    fi
{swap}fi
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
            library: bool = False, alternate_developer_dir: str = "") -> Fixture:
    """Create a C project with a leaf directory that runs real try_compile probes."""
    case = Fixture(root, compiler, probes, alternate_developer_dir)
    for name in ("source/leaf", "include/existing", "library", "empty", "alternate"):
        (root / name).mkdir(parents=True)
    case.wrapper()
    (root / "toolchain.cmake").write_text(
        'set(CMAKE_C_COMPILER "${CMAKE_CURRENT_LIST_DIR}/compiler")\n'
        'set(CMAKE_C_COMPILER_FORCED TRUE)\n'
        'set(PROBE_TOOLCHAIN_DEFINITION "")\n', encoding="utf-8",
    )
    (case.source / "CMakeLists.txt").write_text(
        "cmake_minimum_required(VERSION 4.5)\nproject(ProbeCache C)\n"
        "add_subdirectory(leaf)\n", encoding="utf-8",
    )
    link = ('LINK_OPTIONS "-L${CMAKE_CURRENT_SOURCE_DIR}/../../library" '
            'LINK_LIBRARIES "-lrcmake_probe_archive"') if library else ""
    result_code = "probe_archive() + ${index} - 1" if library else "${index}"
    # Keep each probe source different so a cold storm performs many writes.
    leaf = f"""\
set(results "")
set(prefix [==[{prefix}]==])
foreach(index RANGE 1 {probes})
  set(log "${{CMAKE_BINARY_DIR}}/probe-${{index}}.log")
  try_compile(result
    SOURCE_FROM_CONTENT "probe_${{index}}.c"
      "${{prefix}}int main(void) {{ return {result_code}; }}"
    COMPILE_DEFINITIONS "-I${{CMAKE_CURRENT_SOURCE_DIR}}/../../include" ${{PROBE_TOOLCHAIN_DEFINITION}}
    {link}
    OUTPUT_VARIABLE compile_output
    COPY_FILE "${{CMAKE_BINARY_DIR}}/probe-${{index}}"
    NO_CACHE)
  file(WRITE "${{log}}" "${{compile_output}}")
  string(APPEND results "${{result}}\\n")
endforeach()
file(WRITE "${{CMAKE_BINARY_DIR}}/probe-results.txt" "${{results}}")
string(REPLACE "\\n" " " display "${{results}}")
message(STATUS "Probe results ${{display}}")
"""
    (case.source / "leaf/CMakeLists.txt").write_text(leaf, encoding="utf-8")
    backdate(*root.rglob("*"), root)
    return case


def backdate(*paths: Path) -> None:
    """Model inputs that existed before the build.

    rcmake publishes a probe only if the files and directories it read
    are older than 2 s.
    """
    old = time.time() - 10
    for path in paths:
        os.utime(path, (old, old), follow_symlinks=False)


def environment(case: Fixture, store: Path) -> dict[str, str]:
    """Use a private store and a stable baseline for the changed inputs."""
    env = clean_environment()
    for key in tuple(env):
        if key.startswith(("RCMAKE_PROBE_", "RCMAKE_CACHE_", "RCMAKE_NO_", "RCMAKE_MEASURE_")) or key in {
            "RCMAKE_STORE_DIR", "DEVELOPER_DIR", "LANGUAGE", "LC_ALL", "LC_MESSAGES", "CFLAGS",
            "CPPFLAGS", "LDFLAGS", "CC", "SDKROOT", "C_INCLUDE_PATH", "LIBRARY_PATH",
            "LD_PRELOAD", "DYLD_INSERT_LIBRARIES",
        }:
            del env[key]
    # StoreConfig reads RCMAKE_CACHE_DIR. The storm uses eight configure processes.
    # Keep each process serial when the probe pool is active.
    env.update(LANG="C", CPATH=str(case.root / "empty"), RCMAKE_CACHE_DIR=str(store),
               RCMAKE_PROBE_JOBS="1")
    return env


def command(binary: Path, case: Fixture, build: Path, no_cache: bool = False,
            generator: str = "Ninja") -> list:
    """Make a configure command for an empty build directory."""
    flags = ["--no-probe-cache"] if no_cache else []
    return [binary, *flags, "-S", case.source, "-B", build, "-G", generator,
            f"-DCMAKE_TOOLCHAIN_FILE={case.root / 'toolchain.cmake'}"]


def output(result: dict, case: Fixture, build: Path, args, env: dict) -> dict:
    """Read probe output and run each copied executable."""
    stdout = normalize(result["stdout"].encode(), case.source, build)
    stderr = normalize(result["stderr"].encode(), case.source, build)
    if result["error"] or result["returncode"] != 0 or PANIC.search(stdout + b"\n" + stderr):
        raise ValueError(f"Configure failed in {build}. {result}")
    result_file = build / "probe-results.txt"
    probe_stdout = b"\n".join(line for line in stdout.splitlines() if line.startswith(PROBE_STATUS))
    if not probe_stdout or not result_file.is_file():
        raise ValueError(f"Configure did not report probe results in {build}.")
    results = result_file.read_bytes()
    values = results.splitlines()
    if len(values) != case.probes or any(value not in (b"TRUE", b"FALSE") for value in values):
        raise ValueError(f"Invalid probe results in {result_file}.")
    logs = []
    copies = []
    configure_log = build / "CMakeFiles/CMakeConfigureLog.yaml"
    if configure_log.is_file():
        contents = configure_log.read_bytes()
        if any(name in contents for name in PRIVATE_EVIDENCE):
            raise ValueError("The configure log contains private probe evidence flags.")
    for index, value in enumerate(values, 1):
        log = (build / f"probe-{index}.log").read_bytes()
        if any(name in log for name in PRIVATE_EVIDENCE):
            raise ValueError(f"OUTPUT_VARIABLE contains private probe evidence flags for probe {index}.")
        if COMPILER_MARKER not in log:
            raise ValueError(f"OUTPUT_VARIABLE did not contain the compiler output for probe {index}.")
        logs.append(SCRATCH_ID.sub(b"<PROBE>", normalize(log, case.source, build)))
        path = build / f"probe-{index}"
        if value == b"FALSE":
            if path.exists():
                raise ValueError(f"COPY_FILE created an executable for failed probe {index}.")
            copies.append(None)
            continue
        if path.is_symlink() or not path.is_file():
            raise ValueError(f"COPY_FILE did not produce a regular executable for probe {index}.")
        status = run_command([path], build, args.timeout, env)
        if status["error"] or status["returncode"] != index or status["stdout"] or status["stderr"]:
            raise ValueError(f"COPY_FILE produced the wrong executable for probe {index}. {status}")
        copies.append((hashlib.sha256(path.read_bytes()).hexdigest(), stat.S_IMODE(path.stat().st_mode)))
    return {"result": results, "probe stdout": probe_stdout + b"\n", "stdout": stdout,
            "stderr": stderr, "logs": logs, "copies": copies}


def compare(actual: dict, expected: dict, keys, label: str) -> None:
    """Report the first output difference."""
    for key in keys:
        if actual[key] != expected[key]:
            if not isinstance(expected[key], bytes):
                raise ValueError(f"{label} differs for {key}.")
            diff = diff_bytes(expected[key], actual[key], f"expected/{key}", f"actual/{key}")
            raise ValueError(f"{label} differs.\n{diff.decode(errors='replace')}")


def check_result(data: dict, expected: bool, count: int = 1) -> None:
    """Check the expected number and value of the CMake booleans."""
    result = (b"TRUE\n" if expected else b"FALSE\n") * count
    compare(data, {"result": result}, ("result",), "Probe result")


def configure(args, case: Fixture, name: str, env: dict, *, oracle: bool = False,
              no_cache: bool = False, generator: str | None = None,
              cwd: Path | None = None) -> tuple[dict, int]:
    """Configure and count actual compiler calls attributable to the probe."""
    build = case.root / name
    binary = args.oracle if oracle else args.rcmake
    selected_generator = generator or getattr(args, "generator", "Ninja")
    before = len(case.invocations())
    result = run_command(command(binary, case, build, no_cache, generator=selected_generator),
                         cwd or case.root, args.timeout, env)
    return output(result, case, build, args, env), len(case.invocations()) - before


PROBE_HEADERS = {
    "header-root": ("include/rcmake_probe_root.h", "#define RCMAKE_PROBE_HEADER 1\n"),
    "header-subdir": ("include/existing/rcmake_probe_subdir.h", "#define RCMAKE_PROBE_HEADER 1\n"),
    "transitive": ("include/existing/rcmake_probe_inner.h", "#define RCMAKE_PROBE_HEADER 1\n"),
    "cpath-add": ("empty/rcmake_probe_cpath_add.h", "#define RCMAKE_PROBE_HEADER 1\n"),
    "has-include": ("include/rcmake_probe_optional.h", "#define RCMAKE_PROBE_HEADER 1\n"),
    "header-shadow": ("include/limits.h", "#define RCMAKE_PROBE_SHADOW 1\n"),
}


def _build_probe_library(case: Fixture, env: dict, args) -> None:
    source = case.root / "archive.c"
    source.write_text("int probe_archive(void) { return 1; }\n", encoding="utf-8")
    obj = case.root / "archive.o"
    archive = case.root / "library/librcmake_probe_archive.a"
    for cmd in ([case.compiler, "-c", source, "-o", obj], ["ar", "rcs", archive, obj]):
        result = run_command(cmd, case.root, args.timeout, env)
        if result["error"] or result["returncode"]:
            raise ValueError(f"Could not create the test library. {result}")


def change_input(name: str, case: Fixture, env: dict, args) -> None:
    """Change only the input selected for this invalidation check."""
    if name in PROBE_HEADERS:
        rel, content = PROBE_HEADERS[name]
        (case.root / rel).write_text(content, encoding="utf-8")
        return
    if name == "cpath":
        env["CPATH"] = str(case.root / "alternate")
        return
    if name == "lang":
        env["LANG"] = "POSIX"
        return
    if name == "compiler":
        case.wrapper(swapped=True)
        return
    if name == "toolchain":
        with (case.root / "toolchain.cmake").open("a", encoding="utf-8") as stream:
            stream.write('set(PROBE_TOOLCHAIN_DEFINITION "-DRCMAKE_PROBE_TOOLCHAIN=1")\n')
        return
    if name == "developer-dir":
        env["DEVELOPER_DIR"] = args.developer_dirs[1]
        return
    if name == "library":
        _build_probe_library(case, env, args)


def invalidation(args, root: Path, name: str, row: dict) -> None:
    """Require a warm hit and a fresh compile after each input change."""
    alternate = args.developer_dirs[1] if name == "developer-dir" else ""
    case = fixture(root, args.compiler, TRIGGERS[name], library=name == "library",
                   alternate_developer_dir=alternate)
    if name == "cpath":
        (root / "alternate/rcmake_probe_cpath.h").write_text(
            "#define RCMAKE_PROBE_HEADER 1\n", encoding="utf-8")
    if name == "transitive":
        outer = root / "include/rcmake_probe_outer.h"
        outer.write_text("#include <existing/rcmake_probe_inner.h>\n", encoding="utf-8")
        backdate(outer, outer.parent)
    store = root / "store"
    env = environment(case, store)
    if name == "developer-dir":
        env["DEVELOPER_DIR"] = args.developer_dirs[0]
    calls = row["compiler_calls"] = {}
    cold, calls["cold"] = configure(args, case, "cold", env)
    check_result(cold, False)
    require_calls(calls, "cold", 1)
    before_oracle, calls["oracle_before"] = configure(args, case, "oracle-before", env, oracle=True)
    require_calls(calls, "oracle_before", 1)
    compare(cold, before_oracle, PARITY_KEYS, "Cold CMake parity")
    if not args.smoke:
        if len(audit_store(store)) != 1:
            raise ValueError("Expected one published entry for the cold probe.")
        warm, calls["warm"] = configure(args, case, "warm", env)
        compare(warm, cold, cold.keys(), "Warm configure")
        require_calls(calls, "warm", 0)
    change_input(name, case, env, args)
    changed, calls["changed"] = configure(args, case, "changed", env)
    oracle, calls["oracle"] = configure(args, case, "oracle", env, oracle=True)
    require_calls(calls, "oracle", 1)
    check_result(oracle, True)
    compare(changed, oracle, PARITY_KEYS, "Changed CMake parity")
    require_calls(calls, "changed", 1)


def require_calls(calls: dict, lane: str, expected: int) -> None:
    """Check the exact number of fixture compiler calls in a lane."""
    if calls[lane] != expected:
        raise ValueError(f"Wrong compiler call count in the {lane} lane. Found {calls[lane]}. Expected {expected}.")


def cache_control(args, root: Path, row: dict) -> None:
    """Prove that the candidate reads and bypasses the same seeded store."""
    case = fixture(root, args.compiler)
    store = root / "store"
    env = environment(case, store)
    calls = row["compiler_calls"] = {}
    cold, calls["cold"] = configure(args, case, "cold", env)
    check_result(cold, True)
    require_calls(calls, "cold", 1)
    oracle, calls["oracle"] = configure(args, case, "oracle", env, oracle=True)
    require_calls(calls, "oracle", 1)
    compare(cold, oracle, PARITY_KEYS, "Cold CMake parity")
    if args.smoke:
        return

    errors = []
    entries = {}
    try:
        entries = audit_store(store)
        row["published"] = len(entries)
        require_artifacts(entries, set(cold["copies"]), exact=True)
    except (OSError, ValueError) as error:
        errors.append(str(error))
    try:
        warm, calls["warm"] = configure(args, case, "warm", env)
        require_calls(calls, "warm", 0)
        compare(warm, cold, cold.keys(), "Warm configure")
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        errors.append(str(error))
    try:
        disabled, calls["cache_off"] = configure(args, case, "cache-off", env, no_cache=True)
        compare(disabled, oracle, PARITY_KEYS, "Cache-off CMake parity")
        require_calls(calls, "cache_off", 1)
        if audit_store(store) != entries:
            raise ValueError("The cache-off lane changed the seeded probe entries.")
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        errors.append(str(error))
    if errors:
        raise ValueError("Probe cache control failed. " + "\n".join(errors))


class Zstd:
    """Decode one complete Zstandard frame with the Rust store limits."""

    def __init__(self) -> None:
        name = find_library("zstd")
        if not name:
            raise ValueError("Install libzstd to validate probe store files.")
        self.lib = ctypes.CDLL(name)
        self.lib.ZSTD_findFrameCompressedSize.argtypes = (ctypes.c_char_p, ctypes.c_size_t)
        self.lib.ZSTD_findFrameCompressedSize.restype = ctypes.c_size_t
        self.lib.ZSTD_compressBound.argtypes = (ctypes.c_size_t,)
        self.lib.ZSTD_compressBound.restype = ctypes.c_size_t
        self.lib.ZSTD_decompress.argtypes = (ctypes.c_void_p, ctypes.c_size_t, ctypes.c_char_p,
                                            ctypes.c_size_t)
        self.lib.ZSTD_decompress.restype = ctypes.c_size_t

    def read(self, path: Path, magic: bytes, limit: int) -> bytes:
        """Check the frame header, size, checksum and decoded bytes."""
        header = 17
        footer = 32
        info = path.lstat()
        if not stat.S_ISREG(info.st_mode):
            raise ValueError(f"Expected a regular cache file at {path}.")
        with path.open("rb") as source:
            prefix = source.read(header)
            length = int.from_bytes(prefix[9:], "little")
            cap = self.lib.ZSTD_compressBound(length) if length <= limit else 0
            if (len(prefix) != header or prefix[:9] != magic + b"\x01" or length > limit
                    or not header + footer < info.st_size <= header + cap + footer):
                raise ValueError(f"Invalid cache frame header in {path}.")
            data = source.read(cap + footer + 1)
        compressed, checksum = data[:-footer], data[-footer:]
        if (hashlib.sha256(prefix + compressed).digest() != checksum
                or compressed[:4] != b"\x28\xb5\x2f\xfd"
                or self.lib.ZSTD_findFrameCompressedSize(compressed, len(compressed)) != len(compressed)):
            raise ValueError(f"Invalid cache frame in {path}.")
        decoded = ctypes.create_string_buffer(length)
        if self.lib.ZSTD_decompress(decoded, length, compressed, len(compressed)) != length:
            raise ValueError(f"Invalid cache payload length in {path}.")
        return decoded.raw


def cache_files(root: Path, suffix: str):
    """List regular cache files on their exact digest shards."""
    if root.is_symlink() or root.parent.is_symlink():
        raise ValueError(f"Invalid cache directory {root}.")
    if not root.exists():
        return
    for shard in sorted(root.iterdir()):
        if shard.is_symlink() or not shard.is_dir() or not re.fullmatch(r"[0-9a-f]{2}", shard.name):
            raise ValueError(f"Invalid cache shard {shard}.")
        for path in sorted(shard.iterdir()):
            if path.suffix != suffix or not DIGEST.fullmatch(path.stem) or path.stem[:2] != shard.name:
                raise ValueError(f"Invalid cache path {path}.")
            yield path


def template(value) -> bool:
    """Check that a probe template has typed segments."""
    def byte_list(items):
        return isinstance(items, list) and all(type(byte) is int and 0 <= byte <= 255 for byte in items)

    if not isinstance(value, list) or len(value) > 32_768:
        return False
    for segment in value:
        if not isinstance(segment, dict) or len(segment) != 1:
            return False
        if "Literal" in segment and byte_list(segment["Literal"]):
            continue
        bindings = segment["Binding"]
        if not isinstance(bindings, list) or not bindings or not all(
                isinstance(item["token"], str) and isinstance(item["encodings"], list)
                and 1 <= len(item["encodings"]) <= 3
                and all(encoding in ("Raw", "Forward", "Backslash", "LowerDrive", "UpperDrive",
                                     "Escaped", "Quoted") for encoding in item["encodings"])
                and byte_list(item["raw_special"]) for item in bindings):
            return False
    return True


def probe_entry(decoded: bytes, path: Path, blobs: dict) -> tuple[str, int] | None:
    """Validate entry identity, payload shape and artifact references."""
    try:
        record = json.loads(decoded)
        if record["domain"] != "ProbeV1" or record["key"] != path.stem:
            raise ValueError("The domain or key does not match the path.")
        entry = record["entry"]
        trace, references, payload = entry["trace"], entry["blobs"], entry["payload"]
        if not isinstance(trace, list) or not all(
                isinstance(stamp, dict) and len(stamp) == 1 and isinstance(next(iter(stamp.values())), dict)
                and next(iter(stamp)) in ("FileStat", "FileHash", "DirEntries", "TreeListing", "Missing", "EnvVar",
                                     "NegativeLookup")
                for stamp in trace):
            raise ValueError("Invalid trace.")
        checks = payload["checks"]
        if not isinstance(checks, list) or not all(
                isinstance(check, list) and len(check) == 2 and DIGEST.fullmatch(check[1])
                for check in checks):
            raise ValueError("Invalid journal checks.")
        outcome = payload["outcome"]
        if (type(outcome["compiled"]) is not bool or type(outcome["exit_code"]) is not int
                or not -(2**31) <= outcome["exit_code"] < 2**31 or not template(outcome["output"])):
            raise ValueError("Invalid probe outcome.")
        diagnostics = outcome["diagnostics"]
        if (not isinstance(diagnostics, list) or len(diagnostics) > 16_384 or not all(
                diag["stream"] in ("Stdout", "Stderr") and template(diag["message"])
                for diag in diagnostics)):
            raise ValueError("Invalid probe diagnostics.")
        artifact = outcome.get("artifact")
        if artifact is None and references == [] and not outcome["compiled"]:
            return None
        digest, mode = artifact["blob"], artifact["mode"]
        if (not outcome["compiled"] or references != [digest] or digest not in blobs
                or not 0 < blobs[digest] <= ARTIFACT_LIMIT or type(mode) is not int
                or not 0 <= mode < 2**32 or not template(artifact["relative_path"])):
            raise ValueError("Invalid or missing artifact reference.")
        return digest, stat.S_IMODE(mode)
    except (KeyError, TypeError, ValueError) as error:
        raise ValueError(f"Invalid probe entry {path}. {error}") from error


@dataclass(frozen=True)
class PublishedProbe:
    """Keep the observed entry content and its artifact identity."""

    digest: str
    artifact: tuple[str, int] | None


def audit_store(store: Path) -> dict[Path, PublishedProbe]:
    """Check published bytes. The runtime checks input stamps during recovery."""
    blob_paths = list(cache_files(store / "blobs", ".blob"))
    entry_paths = list(cache_files(store / "entries/probe-v1", ".entry"))
    if not entry_paths and not blob_paths:
        return {}
    decoder = Zstd()
    blobs = {}
    for path in blob_paths:
        decoded = decoder.read(path, b"RCM_BLOB", BLOB_LIMIT)
        if hashlib.sha256(decoded).hexdigest() != path.stem:
            raise ValueError(f"The blob content does not match its digest in {path}.")
        blobs[path.stem] = len(decoded)
    entries = {}
    for path in entry_paths:
        decoded = decoder.read(path, b"RCMENTRY", ENTRY_LIMIT)
        entries[path] = PublishedProbe(hashlib.sha256(decoded).hexdigest(), probe_entry(decoded, path, blobs))
    return entries


def require_artifacts(entries: dict[Path, PublishedProbe], copies: set, *, exact: bool) -> None:
    """Check that the published artifacts match the fixture executables."""
    artifacts = {entry.artifact for entry in entries.values()}
    if len(artifacts) != len(entries) or not artifacts <= copies or (exact and artifacts != copies):
        raise ValueError(f"Expected {'all' if exact else 'distinct'} fixture artifacts in the store. "
                         f"Entry count {len(entries)}. Copied executable count {len(copies)}.")


def parallel(args, case: Fixture, env: dict) -> list[dict]:
    """Start all workers before waiting and check each result."""
    workers = []
    deadline = time.monotonic() + args.timeout
    with ExitStack() as stack:
        try:
            for index in range(WORKERS):
                build = case.root / f"worker-{index}"
                stdout = stack.enter_context(tempfile.TemporaryFile())
                stderr = stack.enter_context(tempfile.TemporaryFile())
                process = subprocess.Popen(
                    command(args.rcmake, case, build, generator=getattr(args, "generator", "Ninja")),
                    cwd=case.root, env=env, stdin=subprocess.DEVNULL, stdout=stdout, stderr=stderr,
                    start_new_session=True,
                )
                workers.append((process, build, stdout, stderr))
            outputs = []
            for process, build, out, err in workers:
                status = process.wait(timeout=max(0, deadline - time.monotonic()))
                out.seek(0)
                err.seek(0)
                stdout, stderr = out.read(), err.read()
                outputs.append(output({"stdout": stdout.decode(errors="replace"),
                                       "stderr": stderr.decode(errors="replace"),
                                       "returncode": status, "error": None}, case, build, args, env))
            return outputs
        finally:
            for process, _, _, _ in workers:
                if process.poll() is None:
                    kill_group(process)
                    process.wait(timeout=5)


def concurrency(args, root: Path, row: dict) -> None:
    """Compare eight workers with CMake and reuse every published result."""
    probes = 24
    case = fixture(root, args.compiler, probes=probes)
    store = root / "store"
    env = environment(case, store)
    oracle, _ = configure(args, case, "oracle", env, oracle=True)
    check_result(oracle, True, probes)
    results = parallel(args, case, env)
    row["completed"] = len(results)
    for actual in results:
        compare(actual, oracle, PARITY_KEYS, "Parallel CMake parity")
    entries = audit_store(store)
    row["published_before_recovery"] = len(entries)
    recovered, row["recovery_compiler_calls"] = configure(args, case, "recovery", env)
    compare(recovered, oracle, PARITY_KEYS, "Parallel recovery parity")
    compare(recovered, results[0], ("logs",), "Parallel output replay")
    require_calls(row, "recovery_compiler_calls", 0)
    require_artifacts(entries, set(recovered["copies"]), exact=True)
    if audit_store(store) != entries:
        raise ValueError("The warm worker changed the published probe entries.")


# Stop at the close of an entry's staging file. Store publication calls
# rename after this close. The checkpoint is an atomic symlink to that file.
def publication_hook(case: Fixture, env: dict, target: int) -> Path:
    """Arm a checkpoint after staging a probe entry and before publishing it."""
    arm = case.root / "armed"
    checkpoint = case.root / "checkpoint"
    arm.touch()
    env.update(RCMAKE_MEASURE_ARM=str(arm), RCMAKE_MEASURE_CHECKPOINT=str(checkpoint),
               RCMAKE_MEASURE_ENTRY=str(target))
    return checkpoint


def interrupt(args, case: Fixture, env: dict, checkpoint: Path) -> int:
    """Kill one configure after its checkpoint and wait for SIGKILL."""
    with tempfile.TemporaryFile() as streams:
        process = subprocess.Popen(
            command(args.rcmake, case, case.root / "interrupted",
                    generator=getattr(args, "generator", "Ninja")),
            cwd=case.root, env=env, stdin=subprocess.DEVNULL, stdout=streams, stderr=streams,
            start_new_session=True,
        )
        try:
            deadline = time.monotonic() + args.timeout
            while not checkpoint.is_symlink():
                if process.poll() is not None:
                    streams.seek(0)
                    raise ValueError("The configure exited before its checkpoint.\n"
                                     + streams.read().decode(errors="replace"))
                if time.monotonic() >= deadline:
                    raise ValueError("The configure did not reach its checkpoint before the timeout.")
                time.sleep(0.01)
            kill_group(process)
            status = process.wait(timeout=5)
            streams.seek(0)
            if status != -signal.SIGKILL or PANIC.search(streams.read()):
                raise ValueError(f"Expected SIGKILL at the checkpoint. Exit status {status}.")
            return process.pid
        finally:
            if process.poll() is None:
                kill_group(process)
                process.wait(timeout=5)


def crash_recovery(args, root: Path, row: dict) -> None:
    """Check reuse of each intact entry after a stopped publication."""
    probes = 24
    intact = random.Random(args.seed).randrange(8, 17)
    case = fixture(root, args.compiler, probes=probes)
    store = root / "store"
    env = environment(case, store)
    oracle, _ = configure(args, case, "oracle", env, oracle=True)
    check_result(oracle, True, probes)
    checkpoint = publication_hook(case, env, intact + 1)
    victim = interrupt(args, case, env, checkpoint)
    Path(env["RCMAKE_MEASURE_ARM"]).unlink()
    stage = checkpoint.readlink()
    if stage.parent != store / "tmp" or not stage.name.startswith(f"{victim}-"):
        raise ValueError(f"The checkpoint did not identify the victim's staged entry. {stage}")
    decoded = Zstd().read(stage, b"RCMENTRY", ENTRY_LIMIT)
    record = json.loads(decoded)
    key = record.get("key") if isinstance(record, dict) else None
    if not isinstance(key, str) or not DIGEST.fullmatch(key):
        raise ValueError("The staged probe entry has an invalid key.")
    destination = store / "entries/probe-v1" / key[:2] / f"{key}.entry"
    if destination.exists():
        raise ValueError("The staged probe entry was already published.")
    row["staging_interruptions"] = 1
    entries = audit_store(store)
    row["published_before_recovery"] = len(entries)
    if len(entries) != intact:
        raise ValueError(f"Expected {intact} intact probe entries after the interruption.")
    recovered, row["recovery_compiler_calls"] = configure(args, case, "recovery", env)
    compare(recovered, oracle, PARITY_KEYS, "Crash recovery parity")
    require_calls(row, "recovery_compiler_calls", probes - intact)
    expected = [f"probe_{index}.c" for index in range(intact + 1, probes + 1)]
    actual = [line.split("\t")[-1] for line in case.invocations()[-len(expected):]]
    if actual != expected:
        raise ValueError(f"Recovery compiled {actual}. Expected {expected}.")
    copies = set(recovered["copies"])
    require_artifacts(entries, copies, exact=False)
    final = audit_store(store)
    row["published_after_recovery"] = len(final)
    require_artifacts(final, copies, exact=True)
    if any(final.get(path) != data for path, data in entries.items()):
        raise ValueError("Recovery changed an intact probe entry.")


def corrupt_entry(args, root: Path, row: dict) -> None:
    """Recompile only the probe whose published entry was corrupted."""
    case = fixture(root, args.compiler, probes=2)
    store = root / "store"
    env = environment(case, store)
    cold, count = configure(args, case, "cold", env)
    check_result(cold, True, 2)
    if count != 2:
        raise ValueError(f"Expected a cold compile for both probes. Found {count} compiler calls.")
    entries = audit_store(store)
    require_artifacts(entries, set(cold["copies"]), exact=True)
    victim, entry = next(iter(entries.items()))
    index = cold["copies"].index(entry.artifact) + 1
    victim.write_bytes(victim.read_bytes()[:-1])
    recovered, row["recovery_compiler_calls"] = configure(args, case, "recovery", env)
    compare(recovered, cold, (*PARITY_KEYS, "logs"), "Corrupt entry recovery parity")
    require_calls(row, "recovery_compiler_calls", 1)
    actual_source = case.invocations()[-1].split("\t")[-1]
    if actual_source != f"probe_{index}.c":
        raise ValueError(f"Recovery compiled {actual_source}. Expected probe {index}.")
    final = audit_store(store)
    require_artifacts(final, set(recovered["copies"]), exact=True)
    if any(final.get(path) != data for path, data in entries.items() if path != victim):
        raise ValueError("Recovery changed the intact probe entry.")


def developer_dirs(root: Path, timeout: int) -> tuple[str, str] | None:
    """Find two distinct Apple developer directories with usable SDKs."""
    if sys.platform != "darwin":
        return None
    env = clean_environment()
    env.pop("DEVELOPER_DIR", None)
    env.pop("SDKROOT", None)
    active = run_command(["xcode-select", "-p"], root, timeout, env)
    if active["error"] or active["returncode"]:
        return None
    candidates = [Path(active["stdout"].strip()), Path("/Library/Developer/CommandLineTools")]
    candidates.extend(app / "Contents/Developer" for app in sorted(Path("/Applications").glob("Xcode*.app")))
    usable = []
    for path in candidates:
        path = path.resolve()
        if path in usable or not path.is_dir():
            continue
        result = run_command(["xcrun", "--sdk", "macosx", "--show-sdk-path"], root,
                             timeout, {**env, "DEVELOPER_DIR": str(path)})
        sdk = result["stdout"].strip()
        if not result["error"] and not result["returncode"] and sdk and Path(sdk).is_dir():
            usable.append(path)
        if len(usable) == 2:
            return str(usable[0]), str(usable[1])
    return None


def resolve_ninja(args) -> Path:
    """Locate a real ninja executable for Ninja-generator soundness lanes."""
    configured = getattr(args, "ninja", None)
    if configured:
        return executable(configured)
    found = shutil.which("ninja")
    if found:
        return executable(found)
    oracle_dir = os.environ.get("RCMAKE_ORACLE_DIR")
    if oracle_dir:
        candidate = Path(oracle_dir) / "bin" / "ninja"
        if candidate.is_file() and os.access(candidate, os.X_OK):
            return executable(candidate)
    raise ValueError("The ninja case requires a real ninja executable on PATH or via --ninja.")


def ninja_version(ninja: Path, cwd: Path, timeout: int, env: dict[str, str]) -> str:
    """Read and validate the reported version string of a ninja executable."""
    result = run_command([ninja, "--version"], cwd, timeout, env)
    lines = result["stdout"].splitlines() if result["stdout"] else []
    if result["error"] or result["returncode"] != 0 or not lines or not lines[0].strip():
        raise ValueError(f"Could not query ninja version from {ninja}. {result}")
    return lines[0].strip()


def stage_ninja(real_ninja: Path, directory: Path, timeout: int, env: dict[str, str]) -> Path:
    """Place a real ninja executable inside a dedicated bin directory."""
    directory.mkdir(parents=True, exist_ok=True)
    staged = directory / "ninja"
    staged.unlink(missing_ok=True)
    shutil.copy2(real_ninja, staged)
    staged.chmod(0o755)
    check = run_command([staged, "--version"], directory, timeout, env)
    if check["error"] or check["returncode"] != 0 or not check["stdout"].strip():
        staged.unlink(missing_ok=True)
        staged.symlink_to(real_ninja)
        ninja_version(staged, directory, timeout, env)
    backdate(staged, directory)
    return staged


def prepend_path(env: dict[str, str], directory: Path) -> dict[str, str]:
    """Return a copy of env with directory placed first on PATH."""
    updated = dict(env)
    existing = updated.get("PATH", "")
    updated["PATH"] = f"{directory}{os.pathsep}{existing}" if existing else str(directory)
    return updated


def cache_entry_value(build: Path, name: str) -> str | None:
    """Read one typed entry from CMakeCache.txt."""
    cache_file = build / "CMakeCache.txt"
    if not cache_file.is_file():
        return None
    prefix = f"{name}:"
    for line in cache_file.read_text(encoding="utf-8", errors="replace").splitlines():
        if line.startswith(prefix) and "=" in line:
            return line.split("=", 1)[1]
    return None


def require_ninja_make_program(build: Path, staged_ninja: Path) -> str:
    """Verify CMake resolved CMAKE_MAKE_PROGRAM to the staged ninja binary."""
    value = cache_entry_value(build, "CMAKE_MAKE_PROGRAM")
    if not value or value.endswith("-NOTFOUND"):
        raise ValueError(f"CMAKE_MAKE_PROGRAM did not resolve to ninja in {build}: {value!r}.")
    resolved_value = Path(value).resolve()
    if Path(value) != staged_ninja and resolved_value != staged_ninja.resolve():
        raise ValueError(
            f"CMAKE_MAKE_PROGRAM resolved to {value!r} instead of staged ninja {staged_ninja}."
        )
    return value


def ninja_present(args, root: Path, row: dict) -> None:
    """Require probe publication and warm reuse with real ninja first on PATH."""
    real_ninja = resolve_ninja(args)
    case = fixture(root, args.compiler)
    store = root / "store"
    base_env = environment(case, store)
    staged = stage_ninja(real_ninja, root / "ninja-bin", args.timeout, base_env)
    env = prepend_path(base_env, staged.parent)
    row["ninja"] = str(real_ninja)
    row["ninja_version"] = ninja_version(staged, root, args.timeout, env)
    calls = row["compiler_calls"] = {}
    cold, calls["cold"] = configure(args, case, "cold", env, generator="Ninja")
    row["make_program"] = require_ninja_make_program(case.root / "cold", staged)
    check_result(cold, True)
    require_calls(calls, "cold", 1)
    oracle, calls["oracle"] = configure(args, case, "oracle", env, oracle=True, generator="Ninja")
    require_calls(calls, "oracle", 1)
    compare(cold, oracle, PARITY_KEYS, "Ninja-present cold CMake parity")
    if args.smoke:
        return

    errors = []
    entries = {}
    try:
        entries = audit_store(store)
        row["published"] = len(entries)
        require_artifacts(entries, set(cold["copies"]), exact=True)
    except (OSError, ValueError) as error:
        errors.append(str(error))
    try:
        warm, calls["warm"] = configure(args, case, "warm", env, generator="Ninja")
        require_calls(calls, "warm", 0)
        compare(warm, cold, cold.keys(), "Ninja-present warm configure")
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        errors.append(str(error))
    try:
        disabled, calls["cache_off"] = configure(args, case, "cache-off", env, no_cache=True,
                                                 generator="Ninja")
        compare(disabled, oracle, PARITY_KEYS, "Ninja-present cache-off CMake parity")
        require_calls(calls, "cache_off", 1)
        if audit_store(store) != entries:
            raise ValueError("The cache-off lane changed the seeded probe entries.")
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        errors.append(str(error))
    if errors:
        raise ValueError("Ninja-present probe cache failed. " + "\n".join(errors))


@dataclass(frozen=True)
class CaseSpec:
    """Describe one selectable probe-cache evaluation case."""

    name: str
    gate: str
    group: str
    description: str
    runner: Callable | None = None


BASELINE_CASES: tuple[CaseSpec, ...] = (
    CaseSpec("cache-control", "G3/G4", "baseline",
             "Seed, reuse, and bypass the persistent probe cache.", cache_control),
    *(CaseSpec(name, "G3", "baseline", f"Invalidate probe cache when {name} input changes.", None)
      for name in TRIGGERS),
    CaseSpec("parallel", "G4", "baseline",
             "Run eight concurrent configures sharing one probe store.", concurrency),
    CaseSpec("crash", "G4", "baseline",
             "Recover intact probe entries after a SIGKILL during publication.", crash_recovery),
    CaseSpec("corrupt-entry", "G4", "baseline",
             "Recompile only the probe whose published entry was corrupted.", corrupt_entry),
)

EXTENDED_CASES: tuple[CaseSpec, ...] = (
    CaseSpec("ninja-present", "G3/G4", "extended",
             "Publish and reuse a probe with real ninja first on PATH and -G Ninja.",
             ninja_present),
)

ALL_CASES: tuple[CaseSpec, ...] = BASELINE_CASES + EXTENDED_CASES
CASE_INDEX: dict[str, CaseSpec] = {spec.name: spec for spec in ALL_CASES}


def select_cases(
    *,
    smoke: bool = False,
    cases: list[str] | None = None,
    extended: bool = False,
    all_cases: bool = False,
) -> list[CaseSpec]:
    """Resolve the requested evaluation cases without duplicating entries."""
    if extended and all_cases:
        raise ValueError("Use either --extended or --all-cases, not both.")
    if cases:
        if extended or all_cases:
            raise ValueError("Do not combine --case with --extended or --all-cases.")
        requested: list[str] = []
        for item in cases:
            for part in item.split(","):
                name = part.strip()
                if name:
                    requested.append(name)
        if not requested:
            raise ValueError("Specify at least one case name with --case.")
        unknown = [name for name in requested if name not in CASE_INDEX]
        if unknown:
            valid = ", ".join(spec.name for spec in ALL_CASES)
            raise ValueError(f"Unknown case(s): {', '.join(unknown)}. Valid cases: {valid}.")
        seen: set[str] = set()
        selected: list[CaseSpec] = []
        for name in requested:
            if name not in seen:
                seen.add(name)
                selected.append(CASE_INDEX[name])
        return selected
    if smoke:
        if extended or all_cases:
            raise ValueError("--smoke cannot be combined with --extended or --all-cases.")
        return [CASE_INDEX["cache-control"], CASE_INDEX["transitive"]]
    if extended:
        return list(EXTENDED_CASES)
    if all_cases:
        return list(ALL_CASES)
    return list(BASELINE_CASES)


def format_case_list() -> str:
    """Format registered baseline and extended cases for CLI discovery."""
    lines = ["Baseline cases:"]
    for spec in BASELINE_CASES:
        lines.append(f"  {spec.name:<22s} [{spec.gate}] {spec.description}")
    lines.append("")
    lines.append("Extended cases:")
    for spec in EXTENDED_CASES:
        lines.append(f"  {spec.name:<22s} [{spec.gate}] {spec.description}")
    return "\n".join(lines)


def evaluate(args, root: Path, report: dict) -> None:
    """Check the fixture and run the selected cache cases."""
    if sys.platform not in ("darwin", "linux"):
        raise ValueError("This probe fixture requires macOS or Linux and a POSIX C compiler.")
    version = run_command([args.oracle, "--version"], root, args.timeout, clean_environment())
    if version["returncode"] or version["error"] or version["stdout"].splitlines()[:1] != ["cmake version 4.5.1"]:
        raise ValueError(f"Expected the CMake 4.5.1 oracle. {version}")
    candidate = run_command([args.rcmake, "--version"], root, args.timeout, clean_environment())
    if candidate["returncode"] or candidate["error"] or "Rcmake" not in candidate["stdout"]:
        raise ValueError(f"Expected a staged Rcmake executable. {candidate}")
    selected = select_cases(
        smoke=args.smoke,
        cases=getattr(args, "case", None),
        extended=getattr(args, "extended", False),
        all_cases=getattr(args, "all_cases", False),
    )
    control_ready = True
    for spec in selected:
        row = {"case": spec.name, "gate": "Smoke" if args.smoke else spec.gate, "status": "passed"}
        report["cases"].append(row)
        if not control_ready:
            row.update(status="blocked", reason="The probe cache control failed.", passed=False)
            continue
        try:
            if spec.name == "developer-dir":
                args.developer_dirs = developer_dirs(root, args.timeout)
                if not args.developer_dirs:
                    reason = "The case requires two macOS developer directories with usable SDKs."
                    if sys.platform == "darwin" and not args.smoke:
                        raise ValueError(reason)
                    row.update(status="skipped", reason=reason, passed=None)
                    continue
                row["developer_dirs"] = list(args.developer_dirs)
            if spec.runner is None:
                invalidation(args, root / spec.name, spec.name, row)
            else:
                spec.runner(args, root / spec.name, row)
        except (OSError, ValueError, subprocess.SubprocessError) as error:
            row["status"] = "failed"
            row["error"] = str(error)
            print(f"{spec.name}. {error}", file=sys.stderr)
            if spec.name == "cache-control" and not args.smoke:
                control_ready = False
        row["passed"] = row["status"] == "passed"


def render_markdown(report: dict) -> str:
    """Render the Markdown summary table and per-case diagnostics."""
    lines = ["# Compiler probe cache", "", f"Mode {report['mode']}. Seed {report['seed']}."]
    if report["mode"] == "smoke":
        lines.extend(("", "Smoke mode checks compiler success, compiler failure, output and copied executables.",
                      "G3 and G4 require gate mode."))
    lines.extend(("", "| Gate | Case | Result |", "| --- | --- | --- |"))
    for row in report["cases"]:
        lines.append(f"| {row['gate']} | {row['case']} | {row['status'].capitalize()} |")
    for row in report["cases"]:
        if "error" in row or "reason" in row:
            lines.extend(("", f"## {row['case']}", "", row.get("error", row.get("reason"))))
    available = report.get("available_extended_cases")
    if available:
        lines.extend((
            "",
            f"Extended cases available via `--extended`, `--all-cases`, or `--case`: {', '.join(available)}.",
        ))
    if "error" in report:
        lines.extend(("", report["error"]))
    return "\n".join(lines) + "\n"


def build_parser() -> argparse.ArgumentParser:
    """Build the command-line parser for probe cache evaluation."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rcmake")
    parser.add_argument("--oracle", default=os.environ.get("RCMAKE_ORACLE_CMAKE"))
    parser.add_argument("--cc", default=shutil.which("cc"))
    parser.add_argument("--ninja", default=None,
                        help="Path to the real ninja executable for Ninja-present lanes.")
    parser.add_argument("--generator", default="Ninja",
                        help="CMake generator for baseline cases (default: Ninja). Ninja lanes always use Ninja.")
    parser.add_argument("--smoke", action="store_true")
    parser.add_argument("--case", action="append", default=None,
                        help="Run only the named case(s). Repeat or comma-separate names.")
    parser.add_argument("--extended", action="store_true",
                        help="Run only the extended soundness and runahead cases.")
    parser.add_argument("--all-cases", "--include-extended", dest="all_cases", action="store_true",
                        help="Run all baseline and extended cases.")
    parser.add_argument("--list-cases", action="store_true",
                        help="Print all registered baseline and extended cases and exit.")
    parser.add_argument("--seed", type=int, default=0)
    parser.add_argument("--timeout", type=int, default=120)
    parser.add_argument("--out-json", type=Path)
    parser.add_argument("--out-md", type=Path)
    return parser


def main(argv: list[str] | None = None) -> int:
    """Write JSON and Markdown results for the requested mode."""
    parser = build_parser()
    args = parser.parse_args(argv)
    if args.list_cases:
        print(format_case_list())
        return 0
    if not args.rcmake or not args.out_json or not args.out_md:
        parser.error("--rcmake, --out-json, and --out-md are required unless --list-cases is used.")
    if args.out_json.resolve() == args.out_md.resolve():
        parser.error("Use different paths for the JSON and Markdown reports.")
    if args.timeout < 1 or not args.oracle or not args.cc:
        parser.error("Set a positive timeout and specify or install the CMake oracle and C compiler.")
    try:
        select_cases(
            smoke=args.smoke,
            cases=args.case,
            extended=args.extended,
            all_cases=args.all_cases,
        )
    except ValueError as error:
        parser.error(str(error))
    report = {"mode": "smoke" if args.smoke else "gate", "seed": args.seed,
              "rcmake": args.rcmake, "oracle": args.oracle, "cases": []}
    if not args.smoke and not args.case and not args.extended and not args.all_cases:
        report["available_extended_cases"] = [spec.name for spec in EXTENDED_CASES]
    try:
        args.rcmake = executable(args.rcmake)
        args.oracle = executable(args.oracle)
        args.compiler = str(executable(args.cc))
        with tempfile.TemporaryDirectory(prefix="rcmake-probes-") as directory:
            evaluate(args, Path(directory).resolve(), report)
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        report["error"] = str(error)
        print(error, file=sys.stderr)
    report["passed"] = ("error" not in report and any(row["passed"] for row in report["cases"])
                        and all(row["status"] in ("passed", "skipped") for row in report["cases"]))
    write_json(args.out_json, report)
    args.out_md.parent.mkdir(parents=True, exist_ok=True)
    args.out_md.write_text(render_markdown(report), encoding="utf-8")
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())

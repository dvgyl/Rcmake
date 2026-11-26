#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Benchmark CMake 4.5.1 against Rcmake on real-world C++ projects.

Measures cold configure, warm persistent probe-cache configure, incremental
leaf reconfigure, peak RSS, and optional generated Ninja build parity across
DuckDB, OpenCV, Protobuf, Qt Base, and LLVM, plus optional vcpkg and Conan
package-manager profiles.

The default incremental leaf edit appends a comment line. Use
--leaf-edit compile-definition to also append add_compile_definitions(), which
changes the compile commands of every target in the edited directory.

Example:
    python3 tools/measure/bench_giants.py --projects duckdb,opencv,protobuf,qtbase,llvm-project --runs 3
"""

from __future__ import annotations

import argparse
import hashlib
from http.client import HTTPException
from contextlib import contextmanager
from dataclasses import asdict, dataclass
from datetime import datetime, timezone
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
from typing import Any, Callable, Sequence

if __package__:
    from .bench_eval import describe_hardware, host_hardware
    from .corpus_eval import download, extract
    from .oracle_prop_test import resolve_identity
else:
    from bench_eval import describe_hardware, host_hardware
    from corpus_eval import download, extract
    from oracle_prop_test import resolve_identity

ROOT = Path(__file__).resolve().parents[2]
LARGE_PROJECTS_MANIFEST = ROOT / "conformance/corpus/large_projects.toml"
CORPUS_MANIFEST = ROOT / "conformance/corpus/manifest.toml"
ECOSYSTEM_DIR = ROOT / "conformance/ecosystem"

ORACLE_VERSION = "4.5.1"
ORACLE_RELATIVE = "env/oracle/cmake-4.5.1-macos-universal/CMake.app/Contents/bin/cmake"
DEFAULT_PROJECTS = ("duckdb", "opencv", "protobuf", "qtbase", "llvm-project")
QUICK_PROJECTS = ("duckdb", "protobuf")
PKG_MANAGER_PROFILES = ("vcpkg", "conan")
LLVM_SPARSE_PATHS = ("llvm", "clang", "cmake", "third-party")
MIB = 1024 * 1024

RSS_CONFIG = {
    "Darwin": ("-l", r"^\s*(\d+)\s+maximum resident set size$", 1),
    "Linux": ("-v", r"Maximum resident set size \(kbytes\):\s*(\d+)", 1024),
}
BASE_CMAKE_ARGS = (
    "-DCMAKE_BUILD_TYPE=Release",
    "-DCMAKE_POLICY_VERSION_MINIMUM=3.5",
)
REGISTRY_FLAGS = (
    "-DCMAKE_FIND_USE_PACKAGE_REGISTRY=FALSE",
    "-DCMAKE_FIND_USE_SYSTEM_PACKAGE_REGISTRY=FALSE",
    "-DCMAKE_EXPORT_NO_PACKAGE_REGISTRY=ON",
)
COMPILER_CACHES = frozenset({"ccache", "sccache"})
TOUCH_PREFIX = "# rcmake-bench-giants-touch"
LEAF_EDIT_DEFINITION = "RCMAKE_BENCH_GIANTS_LEAF_EDIT"
LEAF_EDITS = ("comment", "compile-definition")

PROBE_STATS_RE = re.compile(
    r"probe[\s_-]*cache[^0-9\n]*(\d+)\s*hits?[^0-9\n]*(\d+)\s*miss(?:es)?",
    re.IGNORECASE,
)
PROBE_KV_HITS_RE = re.compile(r"\bprobe_hits\s*[:=]\s*(\d+)\b", re.IGNORECASE)
PROBE_KV_MISSES_RE = re.compile(r"\bprobe_misses\s*[:=]\s*(\d+)\b", re.IGNORECASE)
PROFILE_HEADER_RE = re.compile(
    r"^Rcmake profile:\s+([0-9.<]+\s*(?:ms|s))\s+invocation wall time,\s+(\d+)\s+commands$",
    re.MULTILINE,
)
PROFILE_ROW_RE = re.compile(
    r"^\s*([0-9.<]+\s*(?:ms|s)|—|-)\s+([0-9.<]+\s*(?:ms|s)|—|-)\s+([0-9.]+%|—|-)\s+(\d+|—|-)\s{2,}(.+?)\s*$"
)
NINJA_STEP_RE = re.compile(r"^\[(\d+)/(\d+)\]\s+(.*)$")
NINJA_OUTPUT_OBJ_RE = re.compile(
    r"""(?:^|\s)(?:-o\s+|/Fo)["']?([^\s"']+\.(?:o|obj))["']?(?=\s|$)"""
)


@dataclass(frozen=True)
class ProjectSpec:
    """Pinned source, configure profile, and verification targets for one benchmark."""

    name: str
    kind: str
    repository: str
    version: str
    commit: str
    source_subdir: str = "."
    configure_args: tuple[str, ...] = ()
    l2_targets: tuple[str, ...] = ()
    sparse_paths: tuple[str, ...] = ()
    submodules: tuple[str, ...] = ()
    required_tools: tuple[str, ...] = ()
    local_fixture: Path | None = None
    archive: tuple[str, str] | None = None
    abseil: ProjectSpec | None = None


@dataclass(frozen=True)
class CommandMeasurement:
    """Result of one measured subprocess invocation."""

    wall_ms: float | None
    peak_rss_bytes: int | None
    exit_code: int | None
    stdout: str = ""
    stderr: str = ""
    error: str | None = None
    timed_out: bool = False

    @property
    def ok(self) -> bool:
        return self.error is None and not self.timed_out and self.exit_code == 0

    @property
    def peak_rss_mib(self) -> float | None:
        if self.peak_rss_bytes is None:
            return None
        return round(self.peak_rss_bytes / MIB, 2)


@dataclass(frozen=True)
class ProbeCacheStats:
    """Observed persistent probe-cache activity for a warm configure run."""

    entries_before: int
    entries_after: int
    entries_added: int
    hits: int | None = None
    misses: int | None = None
    source: str = "store"


CommandRunner = Callable[[Sequence[str], Path, dict[str, str], float], CommandMeasurement]


def checkout_roots() -> list[Path]:
    """Return this checkout and, for a Git worktree, the main repository root."""
    roots = [ROOT]
    try:
        common = subprocess.run(
            ["git", "rev-parse", "--path-format=absolute", "--git-common-dir"],
            cwd=ROOT,
            capture_output=True,
            text=True,
            timeout=10,
            check=False,
        ).stdout.strip()
    except (OSError, subprocess.TimeoutExpired):
        common = ""
    if common:
        main = Path(common).parent
        if main != ROOT:
            roots.append(main)
    return roots


def default_cache_dir() -> Path:
    """Return a cross-worktree cache directory for shallow-cloned sources."""
    override = os.environ.get("RCMAKE_GIANTS_CACHE_DIR")
    if override:
        return Path(override).expanduser().resolve()
    xdg = os.environ.get("XDG_CACHE_HOME")
    base = Path(xdg).expanduser() if xdg else Path.home() / ".cache"
    return (base / "rcmake-bench-giants").resolve()


def find_rcmake(explicit: Path | None) -> Path:
    """Locate the native rcmake executable."""
    if explicit is not None:
        return explicit.expanduser().resolve()
    for root in checkout_roots():
        for relative in ("target/stage/bin/rcmake", "target/release/rcmake"):
            candidate = root / relative
            if candidate.is_file() and os.access(candidate, os.X_OK):
                return candidate.resolve()
    raise ValueError("Cannot find rcmake. Pass --rcmake or stage target/stage/bin/rcmake.")


def find_oracle(explicit: Path | None) -> Path:
    """Locate the CMake 4.5.1 reference executable."""
    candidates: list[Path] = []
    if explicit is not None:
        candidates.append(explicit.expanduser())
    env_oracle = os.environ.get("RCMAKE_ORACLE_CMAKE")
    if env_oracle:
        candidates.append(Path(env_oracle).expanduser())
    for root in checkout_roots():
        candidates.append(root / ORACLE_RELATIVE)
    for candidate in candidates:
        if candidate.is_file() and os.access(candidate, os.X_OK):
            return candidate.resolve()
    on_path = shutil.which("cmake")
    if on_path:
        return Path(on_path).resolve()
    raise ValueError("Cannot find CMake 4.5.1. Pass --oracle or source env/env.sh.")


def load_project_catalog(
    large_manifest: Path = LARGE_PROJECTS_MANIFEST,
    corpus_manifest: Path = CORPUS_MANIFEST,
    ecosystem_dir: Path = ECOSYSTEM_DIR,
) -> dict[str, ProjectSpec]:
    """Build the project roster from the repository's pinned manifests."""
    catalog: dict[str, ProjectSpec] = {}
    if large_manifest.is_file():
        with large_manifest.open("rb") as stream:
            large_data = tomllib.load(stream)
        for entry in large_data.get("project", []):
            name = entry["name"]
            catalog[name] = ProjectSpec(
                name=name,
                kind="giant",
                repository=entry["repository"],
                version=entry["version"],
                commit=entry["commit"],
                source_subdir=entry.get("source_subdir", "."),
                configure_args=tuple(entry.get("configure_args", ())),
                l2_targets=tuple(entry.get("l2_targets", ())),
                sparse_paths=LLVM_SPARSE_PATHS if name == "llvm-project" else (),
                submodules=(".",) if entry.get("submodules") else (),
                required_tools=("pkg-config",) if name == "qtbase" else (),
            )

    if corpus_manifest.is_file():
        with corpus_manifest.open("rb") as stream:
            corpus_data = tomllib.load(stream)
        pins = {entry["name"]: entry for entry in corpus_data.get("project", [])}
        if "protobuf" in pins:
            protobuf = pins["protobuf"]
            abseil = pins["abseil-cpp"]
            version = f"v{protobuf['version'].lstrip('v')}"
            catalog["protobuf"] = ProjectSpec(
                name="protobuf", kind="giant", repository="protocolbuffers/protobuf",
                version=version, commit=version,
                configure_args=tuple(protobuf["configure_args"]),
                l2_targets=("libprotobuf", "protoc"),
                archive=(protobuf["url"], protobuf["sha256"]),
                abseil=ProjectSpec(
                    name="abseil-cpp", kind="dependency", repository="abseil/abseil-cpp",
                    version=abseil["version"], commit=abseil["version"],
                    archive=(abseil["url"], abseil["sha256"]),
                ),
            )

    catalog["vcpkg"] = ProjectSpec(
        name="vcpkg",
        kind="package_manager",
        repository="local/conformance/ecosystem/vcpkg-manifest",
        version="manifest",
        commit="workspace",
        source_subdir=".",
        configure_args=(),
        l2_targets=("vcpkg-probe",),
        required_tools=("vcpkg",),
        local_fixture=ecosystem_dir / "vcpkg-manifest",
    )
    catalog["conan"] = ProjectSpec(
        name="conan",
        kind="package_manager",
        repository="local/conformance/ecosystem/conan-consumer",
        version="consumer",
        commit="workspace",
        source_subdir=".",
        configure_args=(),
        l2_targets=("conan-probe",),
        required_tools=("conan",),
        local_fixture=ecosystem_dir / "conan-consumer",
    )
    return catalog


def parse_project_selection(
    raw_projects: Sequence[str] | None,
    *,
    quick: bool,
    include_pkg_managers: bool,
    catalog: dict[str, ProjectSpec],
) -> list[str]:
    """Normalize comma- or space-separated project names and validate them."""
    if raw_projects:
        names: list[str] = []
        for item in raw_projects:
            for token in item.split(","):
                cleaned = token.strip()
                if cleaned and cleaned not in names:
                    names.append(cleaned)
    else:
        names = list(QUICK_PROJECTS if quick else DEFAULT_PROJECTS)
    if include_pkg_managers:
        for profile in PKG_MANAGER_PROFILES:
            if profile not in names:
                names.append(profile)
    unknown = [name for name in names if name not in catalog]
    if unknown:
        valid = ", ".join(sorted(catalog))
        raise ValueError(f"Unknown project(s): {', '.join(unknown)}. Valid choices: {valid}.")
    if not names:
        raise ValueError("Select at least one project to benchmark.")
    return names


def parse_source_overrides(raw_overrides: Sequence[str]) -> dict[str, Path]:
    """Parse NAME=PATH overrides for pre-cloned local source trees."""
    overrides: dict[str, Path] = {}
    for item in raw_overrides:
        name, sep, raw_path = item.partition("=")
        if not sep or not name.strip() or not raw_path.strip():
            raise ValueError(f"Invalid --source-dir value {item!r}. Expected NAME=PATH.")
        overrides[name.strip()] = Path(raw_path.strip()).expanduser().resolve()
    return overrides


def benchmark_env(inherited: dict[str, str]) -> dict[str, str]:
    """Strip inherited build/cache variables and disable compiler cache wrappers."""
    ignored_prefixes = ("CMAKE_", "CTEST_", "CPACK_", "RCMAKE_", "GIT_", "CCACHE_", "SCCACHE_")
    env = {
        key: value
        for key, value in inherited.items()
        if (not key.startswith(ignored_prefixes) or key == "RCMAKE_VENDORED_DATA_ROOT")
        and key not in {"MAKEFLAGS", "PWD"}
    }
    env.update(LC_ALL="C", COPYFILE_DISABLE="1", CCACHE_DISABLE="1")
    if "PATH" in env:
        env["PATH"] = os.pathsep.join(
            part for part in env["PATH"].split(os.pathsep) if Path(part).name not in COMPILER_CACHES
        )
    for key in ("CC", "CXX", "FC", "OBJC", "OBJCXX"):
        compiler = shlex.split(env.get(key, ""))
        if compiler and Path(compiler[0]).name in COMPILER_CACHES:
            if len(compiler) < 2 or compiler[1].startswith("-"):
                raise ValueError(f"Set {key} to a compiler without a cache wrapper.")
            env[key] = shlex.join(compiler[1:])
    return env


def trial_env(
    base_env: dict[str, str],
    work: Path,
    *,
    probe_cache_dir: Path | None = None,
    probe_cache_mode: str = "readwrite",
    extra: dict[str, str] | None = None,
) -> dict[str, str]:
    """Set a private home and XDG cache, and select the probe cache for the trial."""
    home = work / "home"
    xdg_cache = work / "xdg-cache"
    home.mkdir(parents=True, exist_ok=True)
    xdg_cache.mkdir(parents=True, exist_ok=True)
    env = {**base_env, "HOME": str(home), "XDG_CACHE_HOME": str(xdg_cache)}
    if probe_cache_dir is not None:
        probe_cache_dir.mkdir(parents=True, exist_ok=True)
        env["RCMAKE_CACHE_DIR"] = str(probe_cache_dir)
        env["RCMAKE_PROBE_CACHE_DIR"] = str(probe_cache_dir)
        env["RCMAKE_PROBE_CACHE"] = probe_cache_mode
    if extra:
        env.update(extra)
    return env


def parse_rss_output(system: str, stderr_text: str) -> int | None:
    """Extract peak RSS in bytes from /usr/bin/time output."""
    config = RSS_CONFIG.get(system)
    if config is None:
        return None
    _, pattern, bytes_per_unit = config
    matches = re.findall(pattern, stderr_text, re.MULTILINE)
    if not matches:
        return None
    value = int(matches[-1])
    return value * bytes_per_unit if value > 0 else None


def measure_command(
    command: Sequence[str],
    cwd: Path,
    env: dict[str, str],
    timeout: float,
) -> CommandMeasurement:
    """Run one command with wall-clock timing, timeout enforcement, and peak RSS capture."""
    system = platform.system()
    time_bin = Path("/usr/bin/time")
    rss_config = RSS_CONFIG.get(system)
    argv = list(map(str, command))
    wrapped = [str(time_bin), rss_config[0], *argv] if rss_config and time_bin.is_file() else argv
    timed_out = threading.Event()
    with tempfile.TemporaryFile() as out_file, tempfile.TemporaryFile() as err_file:
        start = time.perf_counter()
        try:
            proc = subprocess.Popen(
                wrapped,
                cwd=cwd,
                env=env,
                stdin=subprocess.DEVNULL,
                stdout=out_file,
                stderr=err_file,
                start_new_session=True,
            )
        except OSError as exc:
            return CommandMeasurement(None, None, None, error=f"Failed to start command: {exc}")

        def kill_group() -> None:
            try:
                os.killpg(proc.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass

        def on_timeout() -> None:
            timed_out.set()
            kill_group()

        timer = threading.Timer(timeout, on_timeout)
        timer.start()
        try:
            exit_code = proc.wait()
            wall_ms = round((time.perf_counter() - start) * 1000.0, 2)
        except KeyboardInterrupt:
            kill_group()
            proc.wait()
            raise
        finally:
            timer.cancel()
            timer.join()

        out_file.seek(max(0, out_file.seek(0, os.SEEK_END) - 131072))
        err_file.seek(max(0, err_file.seek(0, os.SEEK_END) - 131072))
        stdout_text = out_file.read().decode("utf-8", errors="replace")
        stderr_text = err_file.read().decode("utf-8", errors="replace")

    if timed_out.is_set():
        return CommandMeasurement(
            wall_ms=None,
            peak_rss_bytes=None,
            exit_code=None,
            stdout=stdout_text[-4000:],
            stderr=stderr_text[-4000:],
            error=f"Timed out after {timeout:g} s.",
            timed_out=True,
        )

    peak_rss = parse_rss_output(system, stderr_text) if rss_config else None
    error: str | None = None
    if exit_code != 0:
        tail = "\n".join(
            line
            for line in (stderr_text or stdout_text).splitlines()
            if line.strip() and "maximum resident set size" not in line.lower()
        )[-1500:]
        error = f"Exit status {exit_code}. {tail}".strip()
    return CommandMeasurement(
        wall_ms=wall_ms,
        peak_rss_bytes=peak_rss,
        exit_code=exit_code,
        stdout=stdout_text,
        stderr=stderr_text,
        error=error,
    )


def count_probe_entries(store_dir: Path) -> int:
    """Count published probe-v1 entries in an Rcmake cache directory."""
    domain_dir = store_dir / "entries" / "probe-v1"
    if not domain_dir.is_dir():
        return 0
    return sum(1 for path in domain_dir.rglob("*.entry") if path.is_file())


def parse_probe_cache_stats(
    output_text: str,
    entries_before: int,
    entries_after: int,
) -> ProbeCacheStats:
    """Extract probe cache hit/miss counters from output or fall back to store entry counts."""
    entries_added = max(0, entries_after - entries_before)
    match = PROBE_STATS_RE.search(output_text)
    if match:
        return ProbeCacheStats(
            entries_before=entries_before,
            entries_after=entries_after,
            entries_added=entries_added,
            hits=int(match.group(1)),
            misses=int(match.group(2)),
            source="output",
        )
    hits_match = PROBE_KV_HITS_RE.search(output_text)
    misses_match = PROBE_KV_MISSES_RE.search(output_text)
    if hits_match or misses_match:
        return ProbeCacheStats(
            entries_before=entries_before,
            entries_after=entries_after,
            entries_added=entries_added,
            hits=int(hits_match.group(1)) if hits_match else None,
            misses=int(misses_match.group(1)) if misses_match else None,
            source="output",
        )
    return ProbeCacheStats(
        entries_before=entries_before,
        entries_after=entries_after,
        entries_added=entries_added,
        hits=None,
        misses=entries_added if entries_before > 0 or entries_after > 0 else None,
        source="store",
    )


def parse_profile_summary(stderr_text: str) -> dict[str, Any] | None:
    """Parse phase and command-category timings from `rcmake --profile` stderr output."""
    header = PROFILE_HEADER_RE.search(stderr_text)
    if not header:
        return None
    section: str | None = None
    phases: dict[str, dict[str, Any]] = {}
    categories: dict[str, dict[str, Any]] = {}
    for line in stderr_text.splitlines():
        stripped = line.strip()
        if stripped.startswith("Phases can overlap"):
            section = "phase"
            continue
        if stripped.startswith("Command categories can overlap"):
            section = "category"
            continue
        if stripped.startswith(("Top list files", "Top command call sites", "Profile files:")):
            section = None
            continue
        if section is None or stripped.startswith("Inclusive"):
            continue
        row = PROFILE_ROW_RE.match(line)
        if not row:
            continue
        inclusive, self_time, percent, calls_raw, name = row.groups()
        entry = {
            "inclusive": inclusive,
            "self": self_time,
            "percent": percent,
            "calls": int(calls_raw) if calls_raw.isdigit() else None,
        }
        if section == "phase":
            phases[name] = entry
        else:
            categories[name] = entry
    stats = parse_probe_cache_stats(stderr_text, 0, 0)
    result: dict[str, Any] = {
        "invocation_wall": header.group(1),
        "commands": int(header.group(2)),
        "phases": phases,
        "categories": categories,
    }
    if stats.source == "output":
        result["probe_hits"] = stats.hits
        result["probe_misses"] = stats.misses
    return result


def unescape_ninja_token(token: str) -> str:
    """Decode Ninja escape sequences (`$ `, `$:`, `$$`) in one path token."""
    out: list[str] = []
    index = 0
    length = len(token)
    while index < length:
        char = token[index]
        if char == "$" and index + 1 < length:
            nxt = token[index + 1]
            if nxt in (" ", ":", "$"):
                out.append(nxt)
                index += 2
                continue
        out.append(char)
        index += 1
    return "".join(out)


def split_ninja_tokens(text: str) -> list[str]:
    """Split a Ninja dependency list on unescaped whitespace."""
    tokens: list[str] = []
    current: list[str] = []
    index = 0
    length = len(text)
    while index < length:
        char = text[index]
        if char == "$" and index + 1 < length:
            current.append(char)
            current.append(text[index + 1])
            index += 2
            continue
        if char in (" ", "\t"):
            if current:
                tokens.append(unescape_ninja_token("".join(current)))
                current.clear()
            index += 1
            continue
        current.append(char)
        index += 1
    if current:
        tokens.append(unescape_ninja_token("".join(current)))
    return tokens


def parse_ninja_rerun_cmakelists(ninja_text: str, source_root: Path) -> list[Path]:
    """Read configured CMakeLists.txt paths relative to source_root from build.ninja."""
    unfolded = re.sub(r"\$\r?\n[ \t]*", "", ninja_text)
    resolved_root = source_root.resolve()
    found: set[Path] = set()
    for line in unfolded.splitlines():
        if not line.startswith("build ") or ": RERUN_CMAKE" not in line:
            continue
        _, _, deps = line.partition(": RERUN_CMAKE")
        for token in split_ninja_tokens(deps):
            if token in {"|", "||"}:
                continue
            path = Path(token)
            if path.name != "CMakeLists.txt":
                continue
            candidate = path if path.is_absolute() else (resolved_root / path)
            try:
                resolved = candidate.resolve()
            except OSError:
                continue
            if resolved.is_relative_to(resolved_root):
                found.add(resolved.relative_to(resolved_root))
    return sorted(found)


def choose_leaf_cmakelists(
    cmake_lists: Sequence[Path],
    rcmake_lists: Sequence[Path],
    source_root: Path,
) -> tuple[Path | None, str]:
    """Select one deterministic non-root leaf CMakeLists.txt shared by both builds."""
    cmake_set = set(cmake_lists)
    rcmake_set = set(rcmake_lists)
    if cmake_set and rcmake_set:
        candidates = sorted(cmake_set & rcmake_set) or sorted(cmake_set | rcmake_set)
    elif cmake_set or rcmake_set:
        candidates = sorted(cmake_set | rcmake_set)
    else:
        candidates = sorted(
            path.relative_to(source_root)
            for path in source_root.rglob("CMakeLists.txt")
            if path.is_file() and not path.is_symlink()
        )

    directories = {path.parent for path in candidates}
    non_root_leaves: list[Path] = []
    for rel in candidates:
        if rel == Path("CMakeLists.txt"):
            continue
        parent = rel.parent
        has_child = any(other != parent and other.is_relative_to(parent) for other in directories)
        if has_child:
            continue
        full = source_root / rel
        if full.is_file() and not full.is_symlink():
            non_root_leaves.append(rel)
    if non_root_leaves:
        return non_root_leaves[0], "leaf"
    root_file = source_root / "CMakeLists.txt"
    if root_file.is_file() and not root_file.is_symlink():
        return Path("CMakeLists.txt"), "root_fallback"
    return None, "none"


def parse_ninja_dry_run(stdout_text: str) -> dict[str, Any]:
    """Parse step counts and compile object outputs from `ninja -n -v` output."""
    steps = 0
    declared_total = 0
    compile_objects: list[str] = []
    seen_objects: set[str] = set()
    no_work = False
    for raw_line in stdout_text.splitlines():
        line = raw_line.strip()
        if line == "ninja: no work to do.":
            no_work = True
            continue
        match = NINJA_STEP_RE.match(line)
        if not match:
            continue
        steps += 1
        declared_total = max(declared_total, int(match.group(2)))
        command_text = match.group(3)
        obj_match = NINJA_OUTPUT_OBJ_RE.search(command_text)
        if obj_match:
            target = obj_match.group(1)
            if target not in seen_objects:
                seen_objects.add(target)
                compile_objects.append(target)
    return {
        "step_count": steps,
        "declared_total": declared_total,
        "no_work": no_work,
        "compile_objects": compile_objects,
    }


def choose_fast_compile_target(
    cmake_plan: dict[str, Any],
    rcmake_plan: dict[str, Any],
    explicit_target: str | None = None,
) -> str | None:
    """Pick one fast leaf object target present in both Ninja dry-run plans."""
    if explicit_target:
        return explicit_target
    rcmake_objects = set(rcmake_plan.get("compile_objects", ()))
    for obj in cmake_plan.get("compile_objects", ()):
        if obj in rcmake_objects:
            return obj
    return None


def summarize_measurements(samples: Sequence[CommandMeasurement]) -> dict[str, Any]:
    """Compute median, p95, range, peak RSS MiB, and exit codes across runs."""
    successes = [sample for sample in samples if sample.ok and sample.wall_ms is not None]
    times = sorted(sample.wall_ms for sample in successes if sample.wall_ms is not None)
    rss_values = [sample.peak_rss_bytes for sample in successes if sample.peak_rss_bytes is not None]
    peak_rss_bytes = max(rss_values) if rss_values else None
    p95_ms = times[math.ceil(0.95 * len(times)) - 1] if times else None
    return {
        "runs": len(samples),
        "succeeded": len(successes),
        "failed": len(samples) - len(successes),
        "median_ms": round(statistics.median(times), 2) if times else None,
        "p95_ms": round(p95_ms, 2) if p95_ms is not None else None,
        "min_ms": round(times[0], 2) if times else None,
        "max_ms": round(times[-1], 2) if times else None,
        "peak_rss_bytes": peak_rss_bytes,
        "peak_rss_mib": round(peak_rss_bytes / MIB, 2) if peak_rss_bytes is not None else None,
        "exit_codes": [sample.exit_code for sample in samples],
        "errors": [sample.error for sample in samples if sample.error],
        "samples": [
            {
                "wall_ms": sample.wall_ms,
                "peak_rss_bytes": sample.peak_rss_bytes,
                "peak_rss_mib": sample.peak_rss_mib,
                "exit_code": sample.exit_code,
                "error": sample.error,
                "timed_out": sample.timed_out,
            }
            for sample in samples
        ],
    }


def compute_speedup(cmake_ms: float | None, rcmake_ms: float | None) -> float | None:
    """Return cmake_ms / rcmake_ms rounded to 2 decimals when both are positive."""
    if cmake_ms is None or rcmake_ms is None or cmake_ms <= 0 or rcmake_ms <= 0:
        return None
    return round(cmake_ms / rcmake_ms, 2)


def compute_rss_ratio(cmake_rss: int | None, rcmake_rss: int | None) -> float | None:
    """Return cmake_rss / rcmake_rss (>1 means Rcmake uses less memory)."""
    if cmake_rss is None or rcmake_rss is None or cmake_rss <= 0 or rcmake_rss <= 0:
        return None
    return round(cmake_rss / rcmake_rss, 2)


def build_reproducer_command(
    projects: Sequence[str],
    runs: int,
    *,
    generator: str = "Ninja",
    package_manager: str = "none",
    verify_build: bool = False,
    verify_build_mode: str = "both",
    profile_diagnostics: bool = False,
    leaf_edit: str = "comment",
) -> str:
    """Build the clean one-command CLI reproducer without machine-local paths."""
    parts = [
        "python3",
        "tools/measure/bench_giants.py",
        "--projects",
        ",".join(projects),
        "--runs",
        str(runs),
    ]
    if generator != "Ninja":
        parts.extend(["--generator", generator])
    if package_manager != "none":
        parts.extend(["--package-manager", package_manager])
    if verify_build:
        parts.append("--verify-build")
        if verify_build_mode != "both":
            parts.extend(["--verify-build-mode", verify_build_mode])
    if profile_diagnostics:
        parts.append("--profile-diagnostics")
    if leaf_edit != "comment":
        parts.extend(["--leaf-edit", leaf_edit])
    return " ".join(parts)


def find_executable(tool: str, env: dict[str, str]) -> Path | None:
    """Locate a host tool, checking VCPKG_ROOT for vcpkg."""
    if tool == "vcpkg":
        vcpkg_root = env.get("VCPKG_ROOT") or os.environ.get("VCPKG_ROOT")
        if vcpkg_root:
            candidate = Path(vcpkg_root).expanduser().resolve() / "vcpkg"
            if candidate.is_file() and os.access(candidate, os.X_OK):
                return candidate
    found = shutil.which(tool, path=env.get("PATH"))
    return Path(found).resolve() if found else None


def platform_extra_configure_args(
    spec: ProjectSpec,
    work: Path,
) -> tuple[list[str], dict[str, str]]:
    """Return platform-specific configure arguments and environment additions."""
    args: list[str] = [*BASE_CMAKE_ARGS, *spec.configure_args, *REGISTRY_FLAGS]
    extra_env: dict[str, str] = {}
    if spec.name == "opencv":
        downloads = work / "opencv-downloads"
        downloads.mkdir(parents=True, exist_ok=True)
        args.extend([
            f"-DOPENCV_DOWNLOAD_PATH={downloads}",
            "-DWITH_ADE=OFF",
            "-DBUILD_JPEG=ON",
            "-DBUILD_PNG=ON",
            "-DBUILD_ZLIB=ON",
            f"-DPYTHON3_EXECUTABLE={sys.executable}",
        ])
    elif spec.name == "llvm-project":
        args.append(f"-DPython3_EXECUTABLE={sys.executable}")
    elif spec.name == "qtbase" and platform.system() == "Darwin":
        homebrew = Path("/opt/homebrew/opt")
        ssl = homebrew / "openssl@3"
        icu = homebrew / "icu4c"
        if ssl.is_dir() and icu.is_dir():
            ssl_resolved, icu_resolved = ssl.resolve(), icu.resolve()
            ssl_lib = ssl_resolved / "lib/libssl.3.dylib"
            crypto_lib = ssl_resolved / "lib/libcrypto.3.dylib"
            if ssl_lib.is_file() and crypto_lib.is_file():
                extra_env["PKG_CONFIG_PATH"] = (
                    f"{ssl_resolved / 'lib/pkgconfig'}:{icu_resolved / 'lib/pkgconfig'}"
                )
                args.extend([
                    f"-DOPENSSL_ROOT_DIR={ssl_resolved}",
                    f"-DOPENSSL_INCLUDE_DIR={ssl_resolved / 'include'}",
                    f"-DOPENSSL_SSL_LIBRARY={ssl_lib}",
                    f"-DOPENSSL_CRYPTO_LIBRARY={crypto_lib}",
                    f"-DICU_ROOT={icu_resolved}",
                ])
    return args, extra_env


def ensure_source(
    spec: ProjectSpec,
    cache_dir: Path,
    source_overrides: dict[str, Path],
    env: dict[str, str],
    *,
    no_fetch: bool,
    clone_timeout: float,
    runner: CommandRunner = measure_command,
) -> tuple[Path | None, str | None]:
    """Return a ready source root (with source_subdir applied) or an unavailable reason."""
    if spec.name in source_overrides:
        root = source_overrides[spec.name]
        cmake_source = root / spec.source_subdir
        if not (cmake_source / "CMakeLists.txt").is_file():
            return None, f"Override source directory lacks {spec.source_subdir}/CMakeLists.txt: {root}"
        return cmake_source, None

    if spec.local_fixture is not None:
        fixture = spec.local_fixture
        if not (fixture / "CMakeLists.txt").is_file():
            return None, f"Local fixture missing at {fixture}."
        dest = cache_dir / "sources" / spec.name
        if dest.exists():
            shutil.rmtree(dest)
        dest.parent.mkdir(parents=True, exist_ok=True)
        shutil.copytree(fixture, dest)
        return dest, None

    dest = cache_dir / "sources" / spec.name
    if spec.archive:
        url, digest = spec.archive
        dest = dest.with_name(f"{spec.name}-{digest}")
    cmake_source = dest / spec.source_subdir
    if (cmake_source / "CMakeLists.txt").is_file():
        return cmake_source, None
    if no_fetch:
        return None, f"Cached checkout missing at {dest} and --no-fetch was set."

    if spec.archive:
        dest.parent.mkdir(parents=True, exist_ok=True)
        try:
            with tempfile.TemporaryDirectory(prefix=f".{spec.name}-", dir=dest.parent) as directory:
                staging = Path(directory)
                archive = staging / "source.tar.gz"
                download(url, digest, archive)
                source = extract(archive, staging / "extracted")
                if not (source / spec.source_subdir / "CMakeLists.txt").is_file():
                    return None, f"The {spec.name} archive does not contain {spec.source_subdir}/CMakeLists.txt."
                try:
                    source.rename(dest)
                except OSError:
                    if not (cmake_source / "CMakeLists.txt").is_file():
                        raise
        except (OSError, ValueError, HTTPException, tarfile.TarError) as error:
            return None, f"Cannot prepare the {spec.name} release archive: {error}"
        return cmake_source, None

    git_bin = find_executable("git", env)
    if git_bin is None:
        return None, "git executable is not available on PATH."

    (cache_dir / "sources").mkdir(parents=True, exist_ok=True)
    staging = Path(tempfile.mkdtemp(prefix=f".clone-{spec.name}-", dir=cache_dir / "sources"))
    git_env = {**env, "GIT_TERMINAL_PROMPT": "0", "GIT_LFS_SKIP_SMUDGE": "1"}
    try:
        steps: list[list[str]] = [
            [str(git_bin), "init", str(staging)],
            [
                str(git_bin),
                "-C",
                str(staging),
                "remote",
                "add",
                "origin",
                f"https://github.com/{spec.repository}.git",
            ],
        ]
        fetch_cmd = [str(git_bin), "-C", str(staging), "fetch", "--depth=1"]
        if spec.sparse_paths:
            fetch_cmd.append("--filter=blob:none")
        fetch_cmd.extend(["origin", spec.commit])
        steps.append(fetch_cmd)
        if spec.sparse_paths:
            steps.append([str(git_bin), "-C", str(staging), "sparse-checkout", "set", *spec.sparse_paths])
        steps.append([str(git_bin), "-C", str(staging), "checkout", "--detach", "FETCH_HEAD"])

        for step in steps:
            result = runner(step, staging.parent, git_env, clone_timeout)
            if not result.ok:
                return None, f"Failed to clone {spec.repository} ({spec.commit}): {result.error}"

        if spec.submodules and (staging / ".gitmodules").is_file():
            sub_cmd = [str(git_bin), "-C", str(staging), "submodule", "update", "--init", "--depth=1"]
            if spec.submodules != (".",):
                sub_cmd.extend(["--", *spec.submodules])
            result = runner(sub_cmd, staging, git_env, clone_timeout)
            if not result.ok:
                return None, f"Failed to initialize submodules for {spec.name}: {result.error}"

        if dest.exists():
            shutil.rmtree(dest)
        staging.rename(dest)
        if not (cmake_source / "CMakeLists.txt").is_file():
            return None, f"Cloned {spec.name} does not contain {spec.source_subdir}/CMakeLists.txt."
        return cmake_source, None
    finally:
        if staging.exists():
            shutil.rmtree(staging, ignore_errors=True)


def prepare_abseil(spec, cache_dir, cmake, generator, env, timeout, no_fetch, runner):
    """Install a pinned Abseil package for reuse by both Protobuf configurations."""
    compiler_env = (
        "PATH", "CC", "CXX", "CPPFLAGS", "CFLAGS", "CXXFLAGS", "LDFLAGS",
        "SDKROOT", "DEVELOPER_DIR", "MACOSX_DEPLOYMENT_TARGET", "INCLUDE", "LIB", "LIBPATH",
    )
    args = [*BASE_CMAKE_ARGS, "-DCMAKE_CXX_STANDARD=17", "-DCMAKE_POSITION_INDEPENDENT_CODE=ON",
            "-DBUILD_TESTING=OFF", "-DABSL_BUILD_TESTING=OFF", "-DABSL_ENABLE_INSTALL=ON",
            "-DCMAKE_INSTALL_LIBDIR=lib"]
    key = (spec.archive, str(cmake), generator, platform.machine(), args,
           [env.get(name) for name in compiler_env])
    digest = hashlib.sha256(json.dumps(key).encode()).hexdigest()
    prefix = cache_dir / "pkg" / f"abseil-{spec.version}-{digest}"
    config = Path("lib/cmake/absl/abslConfig.cmake")
    if (prefix / config).is_file():
        return prefix, None

    source, error = ensure_source(spec, cache_dir, {}, env, no_fetch=no_fetch,
                                  clone_timeout=timeout, runner=runner)
    if error:
        return None, error
    prefix.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".abseil-", dir=prefix.parent) as directory:
        work = Path(directory)
        build = work / "build"
        install = work / "install"
        steps = (
            [str(cmake), "-S", str(source), "-B", str(build), "-G", generator,
             f"-DCMAKE_INSTALL_PREFIX={install}", *args],
            [str(cmake), "--build", str(build), "--config", "Release", "--parallel", "2"],
            [str(cmake), "--install", str(build), "--config", "Release"],
        )
        for argv in steps:
            result = runner(argv, work, env, max(timeout, 600))
            if not result.ok:
                return None, f"Abseil {argv[1]} failed: {result.error}\n{result.stderr[-2000:]}"
        if not (install / config).is_file():
            return None, f"The Abseil install did not produce {config}."
        try:
            install.rename(prefix)
        except OSError:
            if not (prefix / config).is_file():
                raise
    return prefix, None


def prepare_package_manager_profile(
    profile: str,
    source_root: Path,
    cache_dir: Path,
    work: Path,
    env: dict[str, str],
    timeout: float,
    *,
    is_dedicated_fixture: bool,
    runner: CommandRunner = measure_command,
) -> tuple[list[str], dict[str, str], str | None]:
    """Prepare vcpkg or Conan toolchain arguments; return an unavailable reason if missing."""
    if profile == "none":
        return [], {}, None

    if profile == "vcpkg":
        vcpkg_bin = find_executable("vcpkg", env)
        if vcpkg_bin is None:
            return [], {}, "vcpkg executable not found on PATH or in VCPKG_ROOT."
        vcpkg_root = Path(env.get("VCPKG_ROOT") or os.environ.get("VCPKG_ROOT") or vcpkg_bin.parent).resolve()
        toolchain = vcpkg_root / "scripts/buildsystems/vcpkg.cmake"
        if not toolchain.is_file():
            return [], {}, f"vcpkg toolchain file missing at {toolchain}."
        binary_cache = cache_dir / "pkg" / "vcpkg-binary-cache"
        binary_cache.mkdir(parents=True, exist_ok=True)
        extra_env = {
            "VCPKG_ROOT": str(vcpkg_root),
            "VCPKG_BINARY_SOURCES": f"clear;files,{binary_cache},readwrite",
            "VCPKG_FORCE_SYSTEM_BINARIES": "1",
        }
        extra_args = [f"-DCMAKE_TOOLCHAIN_FILE={toolchain}"]
        if is_dedicated_fixture and (source_root / "vcpkg.json").is_file():
            install_root = cache_dir / "pkg" / "vcpkg-installed"
            buildtrees = work / "vcpkg-buildtrees"
            packages = work / "vcpkg-packages"
            install_cmd = [
                str(vcpkg_bin),
                "install",
                f"--x-install-root={install_root}",
                f"--x-buildtrees-root={buildtrees}",
                f"--x-packages-root={packages}",
            ]
            res = runner(install_cmd, source_root, {**env, **extra_env}, max(timeout, 600.0))
            if not res.ok:
                return [], {}, f"vcpkg install failed: {res.error}"
            extra_args.extend([
                f"-DVCPKG_INSTALLED_DIR={install_root}",
                "-DVCPKG_MANIFEST_INSTALL=OFF",
            ])
        return extra_args, extra_env, None

    if profile == "conan":
        conan_bin = find_executable("conan", env)
        if conan_bin is None:
            return [], {}, "conan executable not found on PATH."
        conan_home = cache_dir / "pkg" / "conan-home"
        conan_home.mkdir(parents=True, exist_ok=True)
        extra_env = {"CONAN_HOME": str(conan_home)}
        conan_env = {**env, **extra_env}
        if not (conan_home / "profiles/default").is_file():
            detect = runner([str(conan_bin), "profile", "detect", "--force"], work, conan_env, 120.0)
            if not detect.ok:
                return [], {}, f"conan profile detect failed: {detect.error}"
        if is_dedicated_fixture and (source_root / "conanfile.txt").is_file():
            conan_out = work / "conan-generators"
            conan_out.mkdir(parents=True, exist_ok=True)
            install = runner(
                [
                    str(conan_bin),
                    "install",
                    str(source_root),
                    f"--output-folder={conan_out}",
                    "--build=missing",
                    "-s",
                    "build_type=Release",
                    "-c",
                    "tools.cmake.cmaketoolchain:generator=Ninja",
                ],
                source_root,
                conan_env,
                max(timeout, 600.0),
            )
            if not install.ok:
                return [], {}, f"conan install failed: {install.error}"
            toolchain_matches = sorted(conan_out.rglob("conan_toolchain.cmake"))
            if not toolchain_matches:
                return [], {}, f"conan install did not generate conan_toolchain.cmake under {conan_out}."
            return [f"-DCMAKE_TOOLCHAIN_FILE={toolchain_matches[0]}"], extra_env, None
        toolchain_override = env.get("CONAN_TOOLCHAIN_FILE") or os.environ.get("CONAN_TOOLCHAIN_FILE")
        if not toolchain_override or not Path(toolchain_override).is_file():
            return [], {}, "Set CONAN_TOOLCHAIN_FILE to use --package-manager conan on giant projects."
        return [f"-DCMAKE_TOOLCHAIN_FILE={Path(toolchain_override).resolve()}"], extra_env, None

    return [], {}, f"Unsupported package manager profile: {profile}."


@contextmanager
def temporary_leaf_touch(leaf_path: Path, build_dir: Path, marker: str, edit: str = "comment"):
    """Append a reversible edit to leaf_path and advance mtime past build.ninja.

    The `comment` edit appends one comment line. The `compile-definition` edit also
    appends add_compile_definitions(), which changes the directory's targets.
    """
    original_bytes = leaf_path.read_bytes()
    original_stat = leaf_path.stat()
    build_ninja = build_dir / "build.ninja"
    floor_ns = original_stat.st_mtime_ns
    if build_ninja.is_file():
        floor_ns = max(floor_ns, build_ninja.stat().st_mtime_ns)
    new_mtime_ns = max(time.time_ns(), floor_ns + 1_000_000_000)
    try:
        text = f"\n{TOUCH_PREFIX} {marker}\n"
        if edit == "compile-definition":
            text += f"add_compile_definitions({LEAF_EDIT_DEFINITION}=1)\n"
        suffix = text.encode("utf-8")
        leaf_path.write_bytes(original_bytes + suffix)
        os.utime(leaf_path, ns=(original_stat.st_atime_ns, new_mtime_ns))
        yield
    finally:
        leaf_path.write_bytes(original_bytes)
        os.utime(leaf_path, ns=(original_stat.st_atime_ns, original_stat.st_mtime_ns))


def run_configure_once(
    binary: Path,
    source_root: Path,
    build_dir: Path,
    generator: str,
    configure_args: Sequence[str],
    cwd: Path,
    env: dict[str, str],
    timeout: float,
    *,
    is_reconfigure: bool = False,
    extra_flags: Sequence[str] = (),
    runner: CommandRunner = measure_command,
) -> CommandMeasurement:
    """Execute one configure or reconfigure invocation and validate generated artifacts."""
    cmd = [str(binary), "-S", str(source_root), "-B", str(build_dir)]
    if not is_reconfigure:
        cmd.extend(["-G", generator, *configure_args])
    cmd.extend(extra_flags)
    cwd.mkdir(parents=True, exist_ok=True)
    measurement = runner(cmd, cwd, env, timeout)
    if not measurement.ok:
        return measurement
    build_file = "build.ninja" if generator == "Ninja" else "Makefile"
    if not (build_dir / "CMakeCache.txt").is_file() or not (build_dir / build_file).is_file():
        return CommandMeasurement(
            wall_ms=measurement.wall_ms,
            peak_rss_bytes=measurement.peak_rss_bytes,
            exit_code=measurement.exit_code,
            stdout=measurement.stdout,
            stderr=measurement.stderr,
            error=f"Configure did not generate CMakeCache.txt and {build_file}.",
        )
    return measurement


def verify_generated_ninja(
    spec: ProjectSpec,
    cmake_build: Path,
    rcmake_build: Path,
    work: Path,
    env: dict[str, str],
    timeout: float,
    *,
    mode: str,
    explicit_target: str | None = None,
    runner: CommandRunner = measure_command,
) -> dict[str, Any]:
    """Verify generated build.ninja dry-run parity and optional single-target compile parity."""
    ninja_bin = find_executable("ninja", env)
    if ninja_bin is None:
        return {
            "mode": mode,
            "status": "unavailable",
            "error": "ninja executable not found on PATH.",
            "dry_run": None,
            "compile": None,
        }

    dry_runs: dict[str, dict[str, Any]] = {}
    plans: dict[str, dict[str, Any]] = {}
    for tool, build_dir in (("cmake", cmake_build), ("rcmake", rcmake_build)):
        cmd = [str(ninja_bin), "-C", str(build_dir), "-n", "-v"]
        if spec.l2_targets:
            cmd.extend(["--", *spec.l2_targets])
        res = runner(cmd, work, env, timeout)
        plan = parse_ninja_dry_run(res.stdout) if res.ok else {}
        plans[tool] = plan
        dry_runs[tool] = {
            "ok": res.ok,
            "wall_ms": res.wall_ms,
            "exit_code": res.exit_code,
            "step_count": plan.get("step_count") if res.ok else None,
            "declared_total": plan.get("declared_total") if res.ok else None,
            "error": res.error,
        }

    both_dry_ok = dry_runs["cmake"]["ok"] and dry_runs["rcmake"]["ok"]
    step_count_match = (
        both_dry_ok and dry_runs["cmake"]["step_count"] == dry_runs["rcmake"]["step_count"]
    )
    dry_run_summary = {
        "cmake": dry_runs["cmake"],
        "rcmake": dry_runs["rcmake"],
        "targets": list(spec.l2_targets),
        "step_count_match": step_count_match,
    }

    compile_summary: dict[str, Any] | None = None
    compile_ok = True
    if mode in ("compile", "both"):
        target = choose_fast_compile_target(plans["cmake"], plans["rcmake"], explicit_target)
        if target is None:
            compile_summary = {
                "target": None,
                "status": "unavailable",
                "error": "No shared object compile target found in Ninja dry-run plans.",
                "cmake": None,
                "rcmake": None,
                "build_speedup": None,
            }
            compile_ok = mode != "compile"
        else:
            compile_results: dict[str, dict[str, Any]] = {}
            for tool, build_dir in (("cmake", cmake_build), ("rcmake", rcmake_build)):
                res = runner([str(ninja_bin), "-C", str(build_dir), "-j", "1", target], work, env, timeout)
                compile_results[tool] = {
                    "ok": res.ok,
                    "wall_ms": res.wall_ms,
                    "peak_rss_mib": res.peak_rss_mib,
                    "exit_code": res.exit_code,
                    "error": res.error,
                }
            compile_ok = compile_results["cmake"]["ok"] and compile_results["rcmake"]["ok"]
            compile_summary = {
                "target": target,
                "status": "pass" if compile_ok else "fail",
                "cmake": compile_results["cmake"],
                "rcmake": compile_results["rcmake"],
                "build_speedup": compute_speedup(
                    compile_results["cmake"]["wall_ms"],
                    compile_results["rcmake"]["wall_ms"],
                )
                if compile_ok
                else None,
            }

    if mode == "dry-run":
        overall = "pass" if (both_dry_ok and step_count_match) else "fail"
    elif mode == "compile":
        overall = "pass" if compile_ok else "fail"
    else:
        overall = "pass" if (both_dry_ok and step_count_match and compile_ok) else "fail"

    return {
        "mode": mode,
        "status": overall,
        "dry_run": dry_run_summary,
        "compile": compile_summary,
    }


def benchmark_project(
    spec: ProjectSpec,
    *,
    cmake_bin: Path,
    rcmake_bin: Path,
    cache_dir: Path,
    shared_probe_cache: Path,
    work_root: Path,
    base_env: dict[str, str],
    runs: int,
    generator: str,
    timeout: float,
    clone_timeout: float,
    package_manager: str,
    source_overrides: dict[str, Path],
    no_fetch: bool,
    verify_build: bool,
    verify_build_mode: str,
    verify_target: str | None,
    profile_diagnostics: bool,
    keep_builds: bool,
    runner: CommandRunner = measure_command,
    leaf_edit: str = "comment",
) -> dict[str, Any]:
    """Run cold, warm probe-cache, incremental leaf, and optional build verification for one project."""
    project_work = work_root / spec.name
    project_work.mkdir(parents=True, exist_ok=True)
    binaries = {"cmake": cmake_bin, "rcmake": rcmake_bin}

    def unavailable_result(reason: str) -> dict[str, Any]:
        return {
            "name": spec.name,
            "kind": spec.kind,
            "repository": spec.repository,
            "version": spec.version,
            "commit": spec.commit,
            "status": "unavailable",
            "unavailable_reason": reason,
            "cold": None,
            "warm_probe": None,
            "incremental_leaf": None,
            "build_verify": None,
            "profile": None,
        }

    missing_tools = [tool for tool in spec.required_tools if find_executable(tool, base_env) is None]
    if missing_tools:
        return unavailable_result(f"Missing required host tool(s): {', '.join(missing_tools)}.")

    source_root, source_error = ensure_source(
        spec,
        cache_dir,
        source_overrides,
        base_env,
        no_fetch=no_fetch,
        clone_timeout=clone_timeout,
        runner=runner,
    )
    if source_error or source_root is None:
        return unavailable_result(source_error or "Source checkout unavailable.")

    active_pm = spec.name if spec.kind == "package_manager" else package_manager
    pm_args, pm_env, pm_error = prepare_package_manager_profile(
        active_pm,
        source_root,
        cache_dir,
        project_work,
        base_env,
        timeout,
        is_dedicated_fixture=(spec.kind == "package_manager"),
        runner=runner,
    )
    if pm_error:
        return unavailable_result(pm_error)

    configure_args, platform_env = platform_extra_configure_args(spec, project_work)
    if spec.abseil:
        abseil_prefix, error = prepare_abseil(spec.abseil, cache_dir, cmake_bin, generator, base_env,
                                             timeout, no_fetch, runner)
        if error:
            return unavailable_result(error)
        configure_args.append(f"-DCMAKE_PREFIX_PATH={abseil_prefix}")
    configure_args = [*configure_args, *pm_args]
    combined_extra_env = {**platform_env, **pm_env}

    try:
        cold_samples: dict[str, list[CommandMeasurement]] = {"cmake": [], "rcmake": []}
        cold_entries_written: list[int] = []
        # Probe keys include HOME and XDG_CACHE_HOME. Keep both paths stable across trials.
        for trial in range(runs):
            order = ("rcmake", "cmake") if trial % 2 == 0 else ("cmake", "rcmake")
            for tool in order:
                lane = project_work / f"cold-{tool}-{trial}"
                build_dir = lane / "build"
                cold_store = lane / "cold-store" if tool == "rcmake" else None
                env = trial_env(
                    base_env,
                    project_work,
                    probe_cache_dir=cold_store,
                    probe_cache_mode="readwrite",
                    extra=combined_extra_env,
                )
                sample = run_configure_once(
                    binaries[tool],
                    source_root,
                    build_dir,
                    generator,
                    configure_args,
                    lane,
                    env,
                    timeout,
                    runner=runner,
                )
                cold_samples[tool].append(sample)
                if cold_store is not None:
                    cold_entries_written.append(count_probe_entries(cold_store))
                if not keep_builds:
                    shutil.rmtree(lane, ignore_errors=True)

        # Prime the shared persistent probe cache once so every timed warm trial measures cache hits.
        warmup_lane = project_work / "warm-prime"
        warmup_env = trial_env(
            base_env,
            project_work,
            probe_cache_dir=shared_probe_cache,
            probe_cache_mode="readwrite",
            extra=combined_extra_env,
        )
        run_configure_once(
            rcmake_bin,
            source_root,
            warmup_lane / "build",
            generator,
            configure_args,
            warmup_lane,
            warmup_env,
            timeout,
            runner=runner,
        )
        if not keep_builds:
            shutil.rmtree(warmup_lane, ignore_errors=True)

        warm_samples: dict[str, list[CommandMeasurement]] = {"cmake": [], "rcmake": []}
        warm_probe_stats: list[ProbeCacheStats] = []
        kept_builds: dict[str, Path | None] = {"cmake": None, "rcmake": None}

        for trial in range(runs):
            order = ("cmake", "rcmake") if trial % 2 == 0 else ("rcmake", "cmake")
            for tool in order:
                lane = project_work / f"warm-{tool}-{trial}"
                build_dir = lane / "build"
                entries_before = count_probe_entries(shared_probe_cache) if tool == "rcmake" else 0
                env = trial_env(
                    base_env,
                    project_work,
                    probe_cache_dir=shared_probe_cache if tool == "rcmake" else None,
                    probe_cache_mode="readwrite",
                    extra=combined_extra_env,
                )
                sample = run_configure_once(
                    binaries[tool],
                    source_root,
                    build_dir,
                    generator,
                    configure_args,
                    lane,
                    env,
                    timeout,
                    runner=runner,
                )
                warm_samples[tool].append(sample)
                if tool == "rcmake":
                    entries_after = count_probe_entries(shared_probe_cache)
                    warm_probe_stats.append(
                        parse_probe_cache_stats(
                            f"{sample.stdout}\n{sample.stderr}",
                            entries_before,
                            entries_after,
                        )
                    )
                if sample.ok:
                    previous = kept_builds[tool]
                    kept_builds[tool] = build_dir
                    if previous is not None and not keep_builds:
                        shutil.rmtree(previous.parent, ignore_errors=True)
                elif not keep_builds:
                    shutil.rmtree(lane, ignore_errors=True)

        # Incremental leaf reconfigure on the kept configured build directories.
        inc_samples: dict[str, list[CommandMeasurement]] = {"cmake": [], "rcmake": []}
        leaf_rel: Path | None = None
        leaf_mode = "none"
        if kept_builds["cmake"] is not None or kept_builds["rcmake"] is not None:
            cmake_lists = (
                parse_ninja_rerun_cmakelists(
                    (kept_builds["cmake"] / "build.ninja").read_text(encoding="utf-8", errors="replace"),
                    source_root,
                )
                if kept_builds["cmake"] is not None and (kept_builds["cmake"] / "build.ninja").is_file()
                else []
            )
            rcmake_lists = (
                parse_ninja_rerun_cmakelists(
                    (kept_builds["rcmake"] / "build.ninja").read_text(encoding="utf-8", errors="replace"),
                    source_root,
                )
                if kept_builds["rcmake"] is not None and (kept_builds["rcmake"] / "build.ninja").is_file()
                else []
            )
            leaf_rel, leaf_mode = choose_leaf_cmakelists(cmake_lists, rcmake_lists, source_root)

        if leaf_rel is not None:
            leaf_abs = source_root / leaf_rel
            for trial in range(runs):
                order = ("rcmake", "cmake") if trial % 2 == 0 else ("cmake", "rcmake")
                for tool in order:
                    build_dir = kept_builds[tool]
                    if build_dir is None:
                        inc_samples[tool].append(
                            CommandMeasurement(
                                None,
                                None,
                                None,
                                error=f"Prior warm configure failed for {tool}.",
                            )
                        )
                        continue
                    env = trial_env(
                        base_env,
                        project_work,
                        probe_cache_dir=shared_probe_cache if tool == "rcmake" else None,
                        probe_cache_mode="readwrite",
                        extra=combined_extra_env,
                    )
                    with temporary_leaf_touch(leaf_abs, build_dir, f"{spec.name}-{tool}-{trial}",
                                              leaf_edit):
                        sample = run_configure_once(
                            binaries[tool],
                            source_root,
                            build_dir,
                            generator,
                            configure_args,
                            build_dir.parent,
                            env,
                            timeout,
                            is_reconfigure=True,
                            runner=runner,
                        )
                    inc_samples[tool].append(sample)
        else:
            for tool in ("cmake", "rcmake"):
                inc_samples[tool].append(
                    CommandMeasurement(None, None, None, error="No configured CMakeLists.txt available.")
                )

        build_verify: dict[str, Any] | None = None
        if verify_build:
            if generator != "Ninja":
                build_verify = {
                    "mode": verify_build_mode,
                    "status": "unavailable",
                    "error": f"--verify-build requires the Ninja generator (got {generator}).",
                    "dry_run": None,
                    "compile": None,
                }
            elif kept_builds["cmake"] is not None and kept_builds["rcmake"] is not None:
                build_verify = verify_generated_ninja(
                    spec,
                    kept_builds["cmake"],
                    kept_builds["rcmake"],
                    project_work,
                    {**base_env, **combined_extra_env},
                    timeout,
                    mode=verify_build_mode,
                    explicit_target=verify_target,
                    runner=runner,
                )
            else:
                build_verify = {
                    "mode": verify_build_mode,
                    "status": "fail",
                    "error": "Skipped build verification because configure did not succeed for both tools.",
                    "dry_run": None,
                    "compile": None,
                }

        profile_info: dict[str, Any] | None = None
        if profile_diagnostics:
            # Run --profile untimed in an isolated directory because profiling disables probe caching.
            prof_lane = project_work / "profile-diag"
            prof_out = prof_lane / "rcmake-profile"
            prof_env = trial_env(base_env, prof_lane, extra=combined_extra_env)
            prof_res = run_configure_once(
                rcmake_bin,
                source_root,
                prof_lane / "build",
                generator,
                configure_args,
                prof_lane,
                prof_env,
                timeout,
                extra_flags=(f"--profile={prof_out}",),
                runner=runner,
            )
            profile_info = parse_profile_summary(prof_res.stderr)
            if not keep_builds:
                shutil.rmtree(prof_lane, ignore_errors=True)

        cold_cmake = summarize_measurements(cold_samples["cmake"])
        cold_rcmake = summarize_measurements(cold_samples["rcmake"])
        warm_cmake = summarize_measurements(warm_samples["cmake"])
        warm_rcmake = summarize_measurements(warm_samples["rcmake"])
        inc_cmake = summarize_measurements(inc_samples["cmake"])
        inc_rcmake = summarize_measurements(inc_samples["rcmake"])

        last_probe = warm_probe_stats[-1] if warm_probe_stats else ProbeCacheStats(0, 0, 0)
        probe_payload = {
            **asdict(last_probe),
            "cold_entries_written": max(cold_entries_written, default=0),
        }
        if profile_info and probe_payload["hits"] is None and "probe_hits" in profile_info:
            probe_payload["hits"] = profile_info["probe_hits"]
            probe_payload["misses"] = profile_info.get("probe_misses")
            probe_payload["source"] = "profile"

        phases_ok = all(
            summary["failed"] == 0 and summary["succeeded"] > 0
            for summary in (cold_cmake, cold_rcmake, warm_cmake, warm_rcmake, inc_cmake, inc_rcmake)
        )
        verify_ok = build_verify is None or build_verify["status"] == "pass"
        status = "pass" if (phases_ok and verify_ok) else "fail"

        return {
            "name": spec.name,
            "kind": spec.kind,
            "repository": spec.repository,
            "version": spec.version,
            "commit": spec.commit,
            "status": status,
            "unavailable_reason": None,
            "cold": {
                "cmake": cold_cmake,
                "rcmake": cold_rcmake,
                "speedup": compute_speedup(cold_cmake["median_ms"], cold_rcmake["median_ms"]),
                "rss_ratio": compute_rss_ratio(
                    cold_cmake["peak_rss_bytes"], cold_rcmake["peak_rss_bytes"]
                ),
            },
            "warm_probe": {
                "cmake": warm_cmake,
                "rcmake": warm_rcmake,
                "speedup": compute_speedup(warm_cmake["median_ms"], warm_rcmake["median_ms"]),
                "rss_ratio": compute_rss_ratio(
                    warm_cmake["peak_rss_bytes"], warm_rcmake["peak_rss_bytes"]
                ),
                "probe_cache": probe_payload,
            },
            "incremental_leaf": {
                "cmake": inc_cmake,
                "rcmake": inc_rcmake,
                "speedup": compute_speedup(inc_cmake["median_ms"], inc_rcmake["median_ms"]),
                "rss_ratio": compute_rss_ratio(
                    inc_cmake["peak_rss_bytes"], inc_rcmake["peak_rss_bytes"]
                ),
                "leaf_file": leaf_rel.as_posix() if leaf_rel is not None else None,
                "leaf_mode": leaf_mode,
            },
            "build_verify": build_verify,
            "profile": profile_info,
        }
    finally:
        if not keep_builds and project_work.exists():
            shutil.rmtree(project_work, ignore_errors=True)


def summarize_report(projects: Sequence[dict[str, Any]]) -> dict[str, Any]:
    """Compute geometric-mean speedups and peak memory envelopes across projects."""
    def geomean(values: list[float]) -> float | None:
        positive = [value for value in values if value > 0]
        return round(statistics.geometric_mean(positive), 2) if positive else None

    cold_ratios: list[float] = []
    warm_ratios: list[float] = []
    inc_ratios: list[float] = []
    inc_p95s: list[float] = []
    rcmake_rss: list[float] = []
    cmake_rss: list[float] = []

    for project in projects:
        if project.get("status") == "unavailable":
            continue
        for key, bucket in (
            ("cold", cold_ratios),
            ("warm_probe", warm_ratios),
            ("incremental_leaf", inc_ratios),
        ):
            phase = project.get(key) or {}
            if phase.get("speedup") is not None:
                bucket.append(phase["speedup"])
            for tool, rss_bucket in (("rcmake", rcmake_rss), ("cmake", cmake_rss)):
                rss = (phase.get(tool) or {}).get("peak_rss_mib")
                if rss is not None:
                    rss_bucket.append(rss)
        inc_p95 = ((project.get("incremental_leaf") or {}).get("rcmake") or {}).get("p95_ms")
        if inc_p95 is not None:
            inc_p95s.append(inc_p95)

    return {
        "projects_total": len(projects),
        "projects_passed": sum(project["status"] == "pass" for project in projects),
        "projects_failed": sum(project["status"] == "fail" for project in projects),
        "projects_unavailable": sum(project["status"] == "unavailable" for project in projects),
        "cold_speedup_geomean": geomean(cold_ratios),
        "warm_probe_speedup_geomean": geomean(warm_ratios),
        "incremental_leaf_speedup_geomean": geomean(inc_ratios),
        "incremental_leaf_max_p95_ms": round(max(inc_p95s), 2) if inc_p95s else None,
        "rcmake_max_peak_rss_mib": round(max(rcmake_rss), 2) if rcmake_rss else None,
        "cmake_max_peak_rss_mib": round(max(cmake_rss), 2) if cmake_rss else None,
    }


def _fmt_ms(value: float | None) -> str:
    return f"{value:.1f}" if value is not None else "n/a"


def _fmt_ratio(value: float | None) -> str:
    return f"{value:.2f}x" if value is not None else "n/a"


def _fmt_rss(value: float | None) -> str:
    return f"{value:.1f}" if value is not None else "n/a"


def _fmt_probe_stats(probe: dict[str, Any] | None) -> str:
    if not probe:
        return "n/a"
    if probe.get("hits") is not None:
        misses = probe.get("misses")
        return f"{probe['hits']} hits / {misses if misses is not None else '?'} misses"
    return f"{probe.get('entries_after', 0)} entries (+{probe.get('entries_added', 0)})"


def render_markdown(report: dict[str, Any]) -> str:
    """Render the giant-project benchmark report as Markdown."""
    meta = report["meta"]
    summary = report["summary"]
    lines = [
        "# Giants configure benchmark",
        "",
        f"Runs per phase: {meta['runs']}. Host: {meta['system']} {meta['machine']} "
        f"({meta['cpus']} CPUs). Generator: {meta['generator']}.",
        f"Hardware: {describe_hardware(meta.get('hardware') or {})}. "
        f"Leaf edit: {meta.get('leaf_edit', 'comment')}.",
        f"Reference CMake: `{meta['oracle_version']}`. Rcmake: `{meta['rcmake_version']}` "
        f"({meta['rcmake_profile']}).",
        "",
        "## Reproducer",
        "",
        "```bash",
        report["reproducer"],
        "```",
        "",
        "## Configure speed and peak RSS",
        "",
        "Times are median wall-clock milliseconds (incremental leaf shows Rcmake median / p95; "
        "with fewer than 20 runs, p95 is the slowest run). "
        "Warm probe-cache compares a fresh build directory using a shared `RCMAKE_PROBE_CACHE` "
        "against a fresh CMake 4.5.1 build directory. Peak RSS is the maximum across cold and warm runs in MiB.",
        "",
        "| Project | Cold CMake / Rcmake (ms) | Cold speedup | Warm CMake / Rcmake (ms) | Warm speedup | Probe cache | Inc leaf CMake / Rcmake med (p95) ms | Inc speedup | Peak RSS CMake / Rcmake (MiB) | Status |",
        "| --- | ---: | ---: | ---: | ---: | --- | ---: | ---: | ---: | --- |",
    ]

    details: list[str] = []
    build_rows: list[str] = []

    for project in report["projects"]:
        name = f"{project['name']} ({project['version']})"
        if project["status"] == "unavailable":
            lines.append(
                f"| {name} | n/a | n/a | n/a | n/a | n/a | n/a | n/a | n/a | unavailable |"
            )
            details.append(f"- `{project['name']}` unavailable: {project['unavailable_reason']}")
            continue

        cold = project["cold"]
        warm = project["warm_probe"]
        inc = project["incremental_leaf"]

        cold_col = f"{_fmt_ms(cold['cmake']['median_ms'])} / {_fmt_ms(cold['rcmake']['median_ms'])}"
        warm_col = f"{_fmt_ms(warm['cmake']['median_ms'])} / {_fmt_ms(warm['rcmake']['median_ms'])}"
        inc_col = (
            f"{_fmt_ms(inc['cmake']['median_ms'])} / "
            f"{_fmt_ms(inc['rcmake']['median_ms'])} ({_fmt_ms(inc['rcmake']['p95_ms'])})"
        )
        cmake_rss = max(
            (v for v in (cold["cmake"]["peak_rss_mib"], warm["cmake"]["peak_rss_mib"]) if v is not None),
            default=None,
        )
        rcmake_rss = max(
            (v for v in (cold["rcmake"]["peak_rss_mib"], warm["rcmake"]["peak_rss_mib"]) if v is not None),
            default=None,
        )
        rss_col = f"{_fmt_rss(cmake_rss)} / {_fmt_rss(rcmake_rss)}"

        lines.append(
            f"| {name} | {cold_col} | {_fmt_ratio(cold['speedup'])} | "
            f"{warm_col} | {_fmt_ratio(warm['speedup'])} | {_fmt_probe_stats(warm.get('probe_cache'))} | "
            f"{inc_col} | {_fmt_ratio(inc['speedup'])} | {rss_col} | {project['status']} |"
        )

        for phase_name, phase_data in (
            ("cold", cold),
            ("warm_probe", warm),
            ("incremental_leaf", inc),
        ):
            for tool in ("cmake", "rcmake"):
                for err in phase_data[tool]["errors"]:
                    details.append(f"- `{project['name']}` {phase_name} ({tool}): {' '.join(err.split())}")

        bv = project.get("build_verify")
        if bv:
            dry = bv.get("dry_run") or {}
            comp = bv.get("compile") or {}
            dry_steps = (
                f"{(dry.get('cmake') or {}).get('step_count', 'n/a')} / "
                f"{(dry.get('rcmake') or {}).get('step_count', 'n/a')}"
                if dry
                else "n/a"
            )
            parity = "match" if dry.get("step_count_match") else "mismatch"
            target = comp.get("target") or "n/a"
            build_ms = (
                f"{_fmt_ms((comp.get('cmake') or {}).get('wall_ms'))} / "
                f"{_fmt_ms((comp.get('rcmake') or {}).get('wall_ms'))}"
                if comp and comp.get("cmake") and comp.get("rcmake")
                else "n/a"
            )
            build_rows.append(
                f"| {project['name']} | {dry_steps} ({parity}) | `{target}` | {build_ms} | "
                f"{_fmt_ratio(comp.get('build_speedup'))} | {bv['status']} |"
            )

    lines.extend([
        "",
        "## Summary",
        "",
        f"- Projects passed: {summary['projects_passed']} / {summary['projects_total']} "
        f"(failed: {summary['projects_failed']}, unavailable: {summary['projects_unavailable']}).",
        f"- Geometric mean speedup: cold {_fmt_ratio(summary['cold_speedup_geomean'])}, "
        f"warm probe-cache {_fmt_ratio(summary['warm_probe_speedup_geomean'])}, "
        f"incremental leaf {_fmt_ratio(summary['incremental_leaf_speedup_geomean'])}.",
        f"- Worst incremental leaf Rcmake p95: {_fmt_ms(summary['incremental_leaf_max_p95_ms'])} ms.",
        f"- Max peak RSS: CMake {_fmt_rss(summary['cmake_max_peak_rss_mib'])} MiB, "
        f"Rcmake {_fmt_rss(summary['rcmake_max_peak_rss_mib'])} MiB.",
    ])

    if build_rows:
        lines.extend([
            "",
            "## Generated Ninja build verification",
            "",
            "| Project | Dry-run steps CMake / Rcmake | Leaf target | Build CMake / Rcmake (ms) | Build ratio | Status |",
            "| --- | ---: | --- | ---: | ---: | --- |",
            *build_rows,
        ])

    if details:
        lines.extend(["", "## Notes and failures", "", *details])

    return "\n".join(lines) + "\n"


def read_tool_version(
    binary: Path,
    env: dict[str, str],
    timeout: float,
    *,
    runner: CommandRunner = measure_command,
) -> str:
    """Return the first line of `<binary> --version`."""
    res = runner([str(binary), "--version"], binary.parent, env, min(timeout, 30.0))
    if not res.ok:
        raise ValueError(f"Cannot read version from {binary}: {res.error}")
    lines = [line.strip() for line in res.stdout.splitlines() if line.strip()]
    return lines[0] if lines else "unknown"


def parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    """Parse CLI arguments for the giants configure benchmark."""
    parser = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument(
        "--rcmake",
        type=Path,
        default=None,
        help="Path to the native rcmake executable (default: target/stage/bin/rcmake).",
    )
    parser.add_argument(
        "--oracle",
        "--cmake",
        dest="oracle",
        type=Path,
        default=None,
        help="Path to the CMake 4.5.1 executable (default: $RCMAKE_ORACLE_CMAKE or env/oracle).",
    )
    parser.add_argument(
        "--allow-any-cmake",
        action="store_true",
        help="Allow a reference CMake binary whose version is not 4.5.1.",
    )
    parser.add_argument(
        "--projects",
        nargs="+",
        default=None,
        help=(
            "Comma- or space-separated projects to benchmark. "
            f"Default: {','.join(DEFAULT_PROJECTS)}. "
            "Optional package-manager profiles: vcpkg, conan."
        ),
    )
    parser.add_argument(
        "--include-pkg-managers",
        action="store_true",
        help="Append the vcpkg and conan package-manager configure profiles to the roster.",
    )
    parser.add_argument(
        "--package-manager",
        choices=("none", "vcpkg", "conan"),
        default="none",
        help="Apply a vcpkg or Conan toolchain profile when configuring giant projects.",
    )
    parser.add_argument(
        "--quick",
        action="store_true",
        help=(
            f"Fast sanity subset ({','.join(QUICK_PROJECTS)}) with 1 run by default "
            "and shorter command timeouts."
        ),
    )
    parser.add_argument(
        "--runs",
        type=int,
        default=None,
        help="Measured trials per phase and tool (default: 3, or 1 with --quick).",
    )
    parser.add_argument(
        "--cache-dir",
        type=Path,
        default=None,
        help="Cache directory for shallow-cloned project sources and package-manager state.",
    )
    parser.add_argument(
        "--probe-cache-dir",
        type=Path,
        default=None,
        help="Explicit directory for the shared RCMAKE_PROBE_CACHE store (default: temporary per run).",
    )
    parser.add_argument(
        "--source-dir",
        action="append",
        default=[],
        metavar="NAME=PATH",
        help="Use a local pre-cloned checkout for project NAME instead of cloning into --cache-dir.",
    )
    parser.add_argument(
        "--no-fetch",
        action="store_true",
        help="Do not clone missing sources from the network; mark uncached projects unavailable.",
    )
    parser.add_argument(
        "--generator",
        default="Ninja",
        help="CMake generator to benchmark (default: Ninja).",
    )
    parser.add_argument(
        "--timeout",
        type=float,
        default=None,
        help="Per-command timeout in seconds (default: 600, or 180 with --quick).",
    )
    parser.add_argument(
        "--clone-timeout",
        type=float,
        default=900.0,
        help="Timeout in seconds for each git fetch/checkout step (default: 900).",
    )
    parser.add_argument(
        "--verify-build",
        action="store_true",
        help="Verify generated Ninja build parity (dry-run step parity and leaf object compile timing).",
    )
    parser.add_argument(
        "--verify-build-mode",
        choices=("dry-run", "compile", "both"),
        default="both",
        help="Ninja verification mode when --verify-build is set (default: both).",
    )
    parser.add_argument(
        "--verify-target",
        default=None,
        help="Explicit Ninja target to compile during --verify-build.",
    )
    parser.add_argument(
        "--profile-diagnostics",
        action="store_true",
        help="Run an extra untimed `rcmake --profile` pass per project to record command/category timings.",
    )
    parser.add_argument(
        "--keep-builds",
        action="store_true",
        help="Retain temporary build directories after the benchmark completes.",
    )
    parser.add_argument(
        "--json",
        type=Path,
        metavar="OUT",
        help="Write the structured JSON report to OUT.",
    )
    parser.add_argument(
        "--leaf-edit",
        choices=LEAF_EDITS,
        default="comment",
        help="Incremental leaf edit: comment (default) or compile-definition, which changes targets.",
    )
    parser.add_argument("--rcmake-sha", help="Commit SHA of the Rcmake binary.")
    parser.add_argument("--binary-sha256", help="Require this SHA-256 digest for the Rcmake binary.")
    parser.add_argument(
        "--markdown",
        type=Path,
        metavar="OUT",
        help="Write the Markdown summary table to OUT.",
    )
    return parser.parse_args(argv)


def main(
    argv: Sequence[str] | None = None,
    *,
    runner: CommandRunner = measure_command,
) -> int:
    """Run the giants configure benchmark and emit JSON and Markdown summaries."""
    args = parse_args(argv)
    runs = args.runs if args.runs is not None else (1 if args.quick else 3)
    timeout = args.timeout if args.timeout is not None else (180.0 if args.quick else 600.0)
    if runs < 1 or not math.isfinite(timeout) or timeout <= 0:
        raise SystemExit("Use at least one run and a finite, positive timeout.")
    if not math.isfinite(args.clone_timeout) or args.clone_timeout <= 0:
        raise SystemExit("Use a finite, positive --clone-timeout.")
    if args.json and args.markdown and args.json.resolve() == args.markdown.resolve():
        raise SystemExit("Use distinct paths for --json and --markdown.")

    try:
        catalog = load_project_catalog()
        selected_names = parse_project_selection(
            args.projects,
            quick=args.quick,
            include_pkg_managers=args.include_pkg_managers,
            catalog=catalog,
        )
        source_overrides = parse_source_overrides(args.source_dir)
        rcmake_bin = find_rcmake(args.rcmake)
        oracle_bin = find_oracle(args.oracle)
        if not rcmake_bin.name.startswith("rcmake"):
            raise ValueError(
                f"Expected a native rcmake binary (named rcmake*), got {rcmake_bin.name}."
            )
        identity = resolve_identity(rcmake_bin, args.rcmake_sha, args.binary_sha256)
        base_env = benchmark_env(dict(os.environ))
        oracle_version = read_tool_version(oracle_bin, base_env, timeout, runner=runner)
        if not args.allow_any_cmake and ORACLE_VERSION not in oracle_version:
            raise ValueError(
                f"Expected CMake {ORACLE_VERSION}, got {oracle_version!r}. "
                "Pass --allow-any-cmake to override."
            )
        rcmake_version = read_tool_version(rcmake_bin, base_env, timeout, runner=runner)
    except (OSError, ValueError) as exc:
        print(f"bench_giants.py: {exc}", file=sys.stderr)
        return 2

    cache_dir = args.cache_dir.expanduser().resolve() if args.cache_dir else default_cache_dir()
    reproducer = build_reproducer_command(
        selected_names,
        runs,
        generator=args.generator,
        package_manager=args.package_manager,
        verify_build=args.verify_build,
        verify_build_mode=args.verify_build_mode,
        profile_diagnostics=args.profile_diagnostics,
        leaf_edit=args.leaf_edit,
    )

    rcmake_profile = next(
        (part for part in reversed(rcmake_bin.resolve().parts) if part in {"release", "debug"}),
        "unknown",
    )

    load_avg = list(os.getloadavg()) if hasattr(os, "getloadavg") else None
    temp_root = Path(tempfile.mkdtemp(prefix="rcmake-bench-giants-"))
    shared_probe_cache = (
        args.probe_cache_dir.expanduser().resolve()
        if args.probe_cache_dir is not None
        else temp_root / "shared-probe-cache"
    )
    shared_probe_cache.mkdir(parents=True, exist_ok=True)

    project_results: list[dict[str, Any]] = []
    try:
        for name in selected_names:
            spec = catalog[name]
            print(f"[bench_giants] Benchmarking {spec.name} ({spec.version}, {runs} runs)...", file=sys.stderr, flush=True)
            result = benchmark_project(
                spec,
                cmake_bin=oracle_bin,
                rcmake_bin=rcmake_bin,
                cache_dir=cache_dir,
                shared_probe_cache=shared_probe_cache,
                work_root=temp_root / "work",
                base_env=base_env,
                runs=runs,
                generator=args.generator,
                timeout=timeout,
                clone_timeout=args.clone_timeout,
                package_manager=args.package_manager,
                source_overrides=source_overrides,
                no_fetch=args.no_fetch,
                verify_build=args.verify_build,
                verify_build_mode=args.verify_build_mode,
                verify_target=args.verify_target,
                profile_diagnostics=args.profile_diagnostics,
                keep_builds=args.keep_builds,
                runner=runner,
                leaf_edit=args.leaf_edit,
            )
            project_results.append(result)
    finally:
        if not args.keep_builds:
            shutil.rmtree(temp_root, ignore_errors=True)

    summary = summarize_report(project_results)
    report: dict[str, Any] = {
        **identity,
        "schema_version": 1,
        "generated_at": datetime.now(timezone.utc).isoformat(timespec="seconds").replace("+00:00", "Z"),
        "reproducer": reproducer,
        "meta": {
            "runs": runs,
            "quick": args.quick,
            "generator": args.generator,
            "timeout_seconds": timeout,
            "system": platform.system(),
            "os_release": platform.release(),
            "machine": platform.machine(),
            "cpus": os.cpu_count(),
            "load_average": load_avg,
            "hardware": host_hardware(),
            "leaf_edit": args.leaf_edit,
            "oracle_version": oracle_version,
            "rcmake_version": rcmake_version,
            "rcmake_profile": rcmake_profile,
            "projects": selected_names,
            "package_manager": args.package_manager,
            "verify_build": args.verify_build,
            "verify_build_mode": args.verify_build_mode if args.verify_build else None,
            "profile_diagnostics": args.profile_diagnostics,
        },
        "summary": summary,
        "projects": project_results,
    }

    md_text = render_markdown(report)
    if args.json:
        args.json.parent.mkdir(parents=True, exist_ok=True)
        args.json.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    if args.markdown:
        args.markdown.parent.mkdir(parents=True, exist_ok=True)
        args.markdown.write_text(md_text, encoding="utf-8")
    print(md_text, end="")
    return 1 if summary["projects_failed"] > 0 else 0


if __name__ == "__main__":
    raise SystemExit(main())

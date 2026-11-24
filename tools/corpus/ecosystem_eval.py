#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Compare the ecosystem fixtures with CMake 4.5.1 and staged Rcmake."""

from __future__ import annotations

import argparse
from dataclasses import dataclass
import json
import os
from pathlib import Path
from shlex import join
import shutil
import subprocess
import sys

from normalize import BUCKETS, RULES
from pathswap import Stage, artifact_dir, compare_runs, executable, stage_dir, write_json

FIXTURES = Path(__file__).resolve().parents[2] / "conformance" / "ecosystem"


@dataclass
class Scenario:
    """Store one fixture, its host requirements and its command stages."""

    name: str
    fixture: str
    requires: tuple[str, ...]
    stages: list[Stage]
    configured_outputs: tuple[str, ...] = ()


def configure(generator: str, *args: str) -> Stage:
    """Request the File API and compile database for a configure stage."""
    query = "build/.cmake/api/v1/query"
    cmd = ["cmake", "-S", ".", "-B", "build", "-G", generator,
           "-DCMAKE_BUILD_TYPE=Release", "-DCMAKE_EXPORT_COMPILE_COMMANDS=ON", *args]
    return Stage("configure",
                 f"mkdir -p {query} && touch {query}/codemodel-v2 && {join(cmd)}")


BUILD = Stage("build", ["cmake", "--build", "build", "--parallel", "2"])
DRY_RUN = Stage("dry-run", ["ninja", "-C", "build", "-n", "-v"], (), ninja_dry_run=True)
TEST = Stage("test", ["ctest", "--test-dir", "build", "--output-on-failure", "--no-tests=error"],
             ("ctest",))


def scenarios(vcpkg: str | None, vcpkg_options: list[str], rust_driver: Path | None) -> list[Scenario]:
    """Return the local and external ecosystem command lists."""
    result = [
        Scenario("recursive-driver", "recursive-driver", ("ninja",), [
            configure("Ninja"), BUILD, Stage(TEST.name, [*TEST.argv(), "-V"],
                                           ("ctest", "cmake", "cpack")),
            Stage("install", ["cmake", "--install", "build", "--prefix", "install"]),
            Stage("package", ["cpack", "--config", "build/CPackConfig.cmake", "-B", "packages"],
                  ("cpack",)),
        ], ("build/version.h",)),
        Scenario("externalproject-fetchcontent", "local-dependencies", ("make",), [
            configure("Unix Makefiles"), BUILD,
        ]),
        Scenario("pkg-config", "local-dependencies", ("ninja", "pkg-config"), [
            configure("Ninja", "-DUSE_PKG_CONFIG=ON"), DRY_RUN, BUILD,
            Stage("run", ["build/pkg-probe"], ()),
        ]),
        Scenario("vcpkg-manifest", "vcpkg-manifest", ("ninja", "vcpkg"), [
            Stage("ports", f'{join([vcpkg or "vcpkg", "install"])} '
                  '--x-install-root="$PWD/build/vcpkg_installed" '
                  '--x-buildtrees-root="$PWD/build/vcpkg-buildtrees" '
                  '--x-packages-root="$PWD/build/vcpkg-packages"'),
            configure("Ninja", *vcpkg_options), DRY_RUN, BUILD, TEST,
        ]),
        Scenario("scikit-build-core", "scikit-build-demo", ("ninja", "python3"), [
            Stage("wheel", ["python3", "-m", "pip", "wheel", "--no-cache-dir", "--no-deps",
                            "--wheel-dir", "wheels", "."]),
        ]),
        Scenario("meson", "meson-consumer", ("ninja", "meson"), [
            Stage("configure", ["meson", "setup", "build"]),
            Stage("build", ["meson", "compile", "-C", "build", "-j", "2"], ()),
            Stage("test", ["meson", "test", "-C", "build", "--print-errorlogs"], ()),
        ]),
    ]
    for name, option in (("conan-consumer", "--build=missing"), ("conan-source-build", "--build=*")):
        result.append(Scenario(name, "conan-consumer", ("ninja", "conan"), [
            Stage("profile", ["conan", "profile", "detect"], ()),
            Stage("dependencies", ["conan", "install", ".", option, "-s", "build_type=Release",
                                   "-c", "tools.cmake.cmaketoolchain:generator=Ninja"],
                  ("cmake",) if name == "conan-source-build" else ()),
            Stage("configure", ["cmake", "--preset", "conan-release",
                                "-DCMAKE_EXPORT_COMPILE_COMMANDS=ON"]),
            Stage("dry-run", ["cmake", "--build", "--preset", "conan-release", "--", "-n", "-v"],
                  ninja_dry_run=True),
            Stage("build", ["cmake", "--build", "--preset", "conan-release", "--parallel", "2"]),
            Stage("test", ["ctest", "--preset", "conan-release", "--output-on-failure",
                           "--no-tests=error"], ("ctest",)),
        ]))
    result.append(Scenario("rust-cmake-crate", "recursive-driver/sub",
                           (str(rust_driver) if rust_driver else "--rust-cmake-driver",),
                           [Stage("driver", [str(rust_driver)])]))
    return result


def executable_on_path(tool: str) -> str | None:
    """Find a host tool, including vcpkg in VCPKG_ROOT."""
    if tool == "vcpkg" and (root := os.environ.get("VCPKG_ROOT")):
        tool_at_root = Path(root).expanduser().resolve() / tool
        if os.access(tool_at_root, os.X_OK) and tool_at_root.is_file():
            return str(tool_at_root)
    return shutil.which(tool)


def scenario_environment(name: str, artifacts: Path) -> dict[str, str]:
    """Use separate package caches for the two engines."""
    env = {"CMAKE_BUILD_PARALLEL_LEVEL": "2"}
    if name.startswith("conan-"):
        env["CONAN_HOME"] = str(artifacts / "conan")
    if name == "vcpkg-manifest":
        cache = artifacts / "binary-cache"
        cache.mkdir(parents=True)
        env.update(
            VCPKG_BINARY_SOURCES=f"clear;files,{cache},readwrite",
            VCPKG_FORCE_SYSTEM_BINARIES="1",
            VCPKG_MAX_CONCURRENCY="2",
        )
    return env


def run_scenario(scenario: Scenario, binary: Path, oracle: Path,
                 artifacts: Path, timeout: int, vcpkg_root: Path | None) -> dict:
    """Skip missing host tools and record every available scenario."""
    name = scenario.name
    missing = [tool for tool in scenario.requires if not executable_on_path(tool)]
    if missing:
        return {"name": name, "status": "skipped_missing_tool", "missing_tools": missing}
    if name == "vcpkg-manifest" and not (
        vcpkg_root and (vcpkg_root / "scripts/buildsystems/vcpkg.cmake").is_file()
    ):
        return {"name": name, "status": "oracle_error", "error": "Set VCPKG_ROOT to a vcpkg checkout."}

    print(f"Running {name}", flush=True)
    try:
        row = compare_runs(
            FIXTURES / scenario.fixture, scenario.stages, binary, oracle, artifacts, timeout,
            configured_outputs=scenario.configured_outputs,
            oracle_env=scenario_environment(name, artifacts / "oracle"),
            rcmake_env=scenario_environment(name, artifacts / "rcmake"),
        )
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        return {"name": name, "status": "harness_error", "error": str(error)}
    return {"name": name, **row}


def markdown(report: dict) -> str:
    """Summarize scenario outcomes, PATH evidence and file class counts."""
    lines = [
        "# Ecosystem PATH comparison", "", f"Result **{report['status']}**", "",
        "| Scenario | Result | Details |", "| --- | --- | --- |",
    ]
    for row in report["scenarios"]:
        notes = row.get("missing_tools", [])
        if row.get("error"):
            notes = [row["error"]]
        for engine in ("oracle", "rcmake"):
            if proof := row.get(engine):
                if proof.get("path_error"):
                    notes.append(f"{engine} PATH check failed")
                for stage in proof["stages"]:
                    if stage["status"] not in {"pass", "skipped_prior_failure"}:
                        notes.append(f"{engine} {stage['name']} {stage['status']}")
        if any(any(values.values()) for values in row.get("ninja_dry_runs", {}).values()):
            notes.append("Ninja dry-run commands differ")
        lines.append(f"| {row['name']} | {row['status']} | {', '.join(notes)} |")

    for row in report["scenarios"]:
        if "files" not in row:
            continue
        lines.extend(["", f"## {row['name']}", ""])
        if cmake := row["rcmake"].get("tools", {}).get("cmake"):
            lines.append(f"`which -a cmake` returned `{cmake['which']}`.")
            lines.extend([f"The first version line was `{cmake['version']}`.", ""])
        lines.extend([
            "| File class | Byte identical | Identical after normalization | Different |",
            "| --- | ---: | ---: | ---: |",
        ])
        for name, counts in row["files"]["classes"].items():
            lines.append(f"| {name} | " + " | ".join(str(counts[key]) for key in BUCKETS) + " |")
    used_rules = {
        rule for row in report["scenarios"] for file in row.get("files", {}).get("files", [])
        for rule in file["rules"]
    }
    if used_rules:
        lines.extend(["", "## Normalization rules used", ""])
        lines.extend(f"- `{rule}`. {RULES[rule]}" for rule in sorted(used_rules))
    return "\n".join(lines) + "\n"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--stage", type=Path, required=True, help="Stage directory or Rcmake executable.")
    parser.add_argument("--oracle", default=os.environ.get("RCMAKE_ORACLE_CMAKE"),
                        help="Path to the CMake 4.5.1 executable.")
    parser.add_argument("--out-json", type=Path, required=True, help="Path to the JSON report.")
    parser.add_argument("--out-md", type=Path, required=True, help="Path to the Markdown report.")
    parser.add_argument("--timeout", type=int, default=600, help="Time limit in seconds per command.")
    parser.add_argument("--rust-cmake-driver", type=Path,
                        help="Path to a prebuilt executable that uses the Rust cmake crate.")
    args = parser.parse_args()
    if not args.oracle or args.timeout < 1 or args.out_json.resolve() == args.out_md.resolve():
        parser.error("Set an oracle path, a positive timeout and distinct report paths.")
    try:
        binary, oracle = stage_dir(args.stage), executable(args.oracle)
        rust_driver = args.rust_cmake_driver.resolve() if args.rust_cmake_driver else None
        vcpkg = executable_on_path("vcpkg")
        vcpkg_root = None
        vcpkg_options = []
        artifacts = artifact_dir(args.out_json)
        if vcpkg:
            vcpkg_root = Path(os.environ.get("VCPKG_ROOT") or Path(vcpkg).resolve().parent).resolve()
            vcpkg_options = [f"-DCMAKE_TOOLCHAIN_FILE={vcpkg_root / 'scripts/buildsystems/vcpkg.cmake'}"]
        rows = [run_scenario(item, binary, oracle, artifacts / item.name, args.timeout, vcpkg_root)
                for item in scenarios(vcpkg, vcpkg_options, rust_driver)]
        statuses = {row["status"] for row in rows}
        if statuses & {"oracle_error", "harness_error"}:
            status = "error"
        elif "fail" in statuses:
            status = "fail"
        elif "pass" in statuses:
            status = "pass"
        else:
            status = "skipped_missing_tool"
        report = {"status": status, "scenarios": rows}
        write_json(args.out_json, report)
        args.out_md.parent.mkdir(parents=True, exist_ok=True)
        args.out_md.write_text(markdown(report), encoding="utf-8")
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        print(str(error), file=sys.stderr)
        return 2
    print(json.dumps({row["name"]: row["status"] for row in rows}, sort_keys=True))
    return {"pass": 0, "fail": 1, "error": 2, "skipped_missing_tool": 2}[status]


if __name__ == "__main__":
    raise SystemExit(main())

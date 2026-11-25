#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Compare the ecosystem fixtures with CMake 4.5.1 and staged Rcmake."""

from __future__ import annotations

import argparse
from dataclasses import dataclass, field
import json
import os
from pathlib import Path
import shlex
from shlex import join
import shutil
import subprocess
import sys
from typing import Any, Callable

from normalize import BUCKETS, IDE_FILE_API_QUERIES, RULES
from pathswap import Stage, artifact_dir, compare_runs, executable, stage_dir, write_json

FIXTURES = Path(__file__).resolve().parents[2] / "conformance" / "ecosystem"
RECEIPT_SCHEMA_VERSION = 1
IDE_CONFIGURATIONS = ("Debug", "Release")
IDE_TARGETS = ("math_core", "format_util", "ide_app")


@dataclass
class Scenario:
    """Store one fixture, its host requirements, stages, and verification metadata."""

    name: str
    fixture: str
    requires: tuple[str, ...]
    stages: list[Stage]
    configured_outputs: tuple[str, ...] = ()
    category: str = "ecosystem"
    metadata: dict[str, Any] = field(default_factory=dict)
    prepare_source: Callable[[Path], None] | None = None
    required_ide_queries: tuple[str, ...] = ()
    required_configurations: tuple[str, ...] = ()
    require_compile_commands: bool = False


def configure(generator: str, *args: str) -> Stage:
    """Request the File API and compile database for a configure stage."""
    query = "build/.cmake/api/v1/query"
    cmd = ["cmake", "-S", ".", "-B", "build", "-G", generator,
           "-DCMAKE_BUILD_TYPE=Release", "-DCMAKE_EXPORT_COMPILE_COMMANDS=ON", *args]
    return Stage("configure",
                 f"mkdir -p {query} && touch {query}/codemodel-v2 && {join(cmd)}")


def configure_preset(
    stage_name: str,
    preset: str,
    build_dir: str,
    queries: tuple[str, ...] = ("codemodel-v2",),
) -> Stage:
    """Create File API query files and configure a build tree through a CMake preset."""
    query_dir = f"{build_dir}/.cmake/api/v1/query"
    query_files = [f"{query_dir}/{query}" for query in queries]
    cmd = ["cmake", "--preset", preset]
    return Stage(
        stage_name,
        f"mkdir -p {join([query_dir])} && touch {join(query_files)} && {join(cmd)}",
    )


def write_ide_import_fixture(destination: Path) -> None:
    """Write a multi-target C/C++ project with presets and a custom toolchain file."""
    if destination.exists():
        shutil.rmtree(destination)
    for subdir in ("cmake", "include", "src/internal"):
        (destination / subdir).mkdir(parents=True, exist_ok=True)

    (destination / "cmake/ide-toolchain.cmake").write_text(
        "# SPDX-License-Identifier: MIT OR Apache-2.0\n"
        'set(IDE_CUSTOM_TOOLCHAIN "active" CACHE STRING "Custom IDE toolchain marker")\n'
        "add_compile_definitions(IDE_TOOLCHAIN_ACTIVE=1)\n",
        encoding="utf-8",
    )
    (destination / "include/math_ops.h").write_text(
        "// SPDX-License-Identifier: MIT OR Apache-2.0\n"
        "#ifndef IDE_IMPORT_MATH_OPS_H\n"
        "#define IDE_IMPORT_MATH_OPS_H\n"
        "#ifdef __cplusplus\n"
        'extern "C" {\n'
        "#endif\n"
        "int ide_add_values(int left, int right);\n"
        "#ifdef __cplusplus\n"
        "}\n"
        "#endif\n"
        "#endif\n",
        encoding="utf-8",
    )
    (destination / "include/format_util.hpp").write_text(
        "// SPDX-License-Identifier: MIT OR Apache-2.0\n"
        "#pragma once\n"
        "#include <string>\n"
        "std::string ide_format_sum(int left, int right);\n",
        encoding="utf-8",
    )
    (destination / "src/internal/format_detail.hpp").write_text(
        "// SPDX-License-Identifier: MIT OR Apache-2.0\n"
        "#pragma once\n"
        "constexpr const char* IDE_SUM_PREFIX = \"sum=\";\n",
        encoding="utf-8",
    )
    (destination / "src/math_ops.c").write_text(
        "// SPDX-License-Identifier: MIT OR Apache-2.0\n"
        '#include "math_ops.h"\n'
        "int ide_add_values(int left, int right) {\n"
        "    return left + right;\n"
        "}\n",
        encoding="utf-8",
    )
    (destination / "src/format_util.cpp").write_text(
        "// SPDX-License-Identifier: MIT OR Apache-2.0\n"
        '#include "format_util.hpp"\n'
        '#include "format_detail.hpp"\n'
        '#include "math_ops.h"\n'
        "std::string ide_format_sum(int left, int right) {\n"
        "    return std::string(IDE_SUM_PREFIX) + std::to_string(ide_add_values(left, right));\n"
        "}\n",
        encoding="utf-8",
    )
    (destination / "src/main.cpp").write_text(
        "// SPDX-License-Identifier: MIT OR Apache-2.0\n"
        '#include "format_util.hpp"\n'
        "#include <iostream>\n"
        "int main() {\n"
        '    const std::string out = ide_format_sum(20, 22);\n'
        '    std::cout << APP_BUILD_CONFIG << ":" << out << "\\n";\n'
        '    return out == "sum=42" ? 0 : 1;\n'
        "}\n",
        encoding="utf-8",
    )
    (destination / "CMakeLists.txt").write_text(
        "# SPDX-License-Identifier: MIT OR Apache-2.0\n"
        "cmake_minimum_required(VERSION 3.25)\n"
        "project(IdeImportDemo VERSION 1.0.0 LANGUAGES C CXX)\n"
        "\n"
        "add_library(math_core STATIC src/math_ops.c include/math_ops.h)\n"
        'target_include_directories(math_core PUBLIC "${CMAKE_CURRENT_SOURCE_DIR}/include")\n'
        "target_compile_definitions(math_core PUBLIC\n"
        "  MATH_CORE_API=1\n"
        "  $<$<CONFIG:Debug>:IDE_CONFIG_DEBUG=1>\n"
        "  $<$<CONFIG:Release>:IDE_CONFIG_RELEASE=1>\n"
        ")\n"
        "target_compile_features(math_core PUBLIC c_std_11)\n"
        "\n"
        "add_library(format_util STATIC\n"
        "  src/format_util.cpp\n"
        "  include/format_util.hpp\n"
        "  src/internal/format_detail.hpp\n"
        ")\n"
        "target_link_libraries(format_util PUBLIC math_core)\n"
        "target_include_directories(format_util\n"
        '  PUBLIC "${CMAKE_CURRENT_SOURCE_DIR}/include"\n'
        '  PRIVATE "${CMAKE_CURRENT_SOURCE_DIR}/src/internal"\n'
        ")\n"
        "target_compile_definitions(format_util PRIVATE FORMAT_INTERNAL=1)\n"
        "target_compile_features(format_util PUBLIC cxx_std_17)\n"
        "\n"
        "add_executable(ide_app src/main.cpp)\n"
        "target_link_libraries(ide_app PRIVATE format_util)\n"
        'target_compile_definitions(ide_app PRIVATE APP_BUILD_CONFIG="$<CONFIG>")\n'
        "\n"
        "enable_testing()\n"
        "add_test(NAME ide_app_smoke COMMAND ide_app)\n",
        encoding="utf-8",
    )
    presets = {
        "version": 6,
        "configurePresets": [
            {
                "name": "ide-base",
                "hidden": True,
                "generator": "Ninja",
                "toolchainFile": "${sourceDir}/cmake/ide-toolchain.cmake",
                "cacheVariables": {
                    "CMAKE_EXPORT_COMPILE_COMMANDS": "ON",
                },
            },
            {
                "name": "ide-debug",
                "inherits": "ide-base",
                "binaryDir": "${sourceDir}/build/debug",
                "cacheVariables": {
                    "CMAKE_BUILD_TYPE": "Debug",
                },
            },
            {
                "name": "ide-release",
                "inherits": "ide-base",
                "binaryDir": "${sourceDir}/build/release",
                "cacheVariables": {
                    "CMAKE_BUILD_TYPE": "Release",
                },
            },
        ],
        "buildPresets": [
            {"name": "ide-debug", "configurePreset": "ide-debug"},
            {"name": "ide-release", "configurePreset": "ide-release"},
        ],
        "testPresets": [
            {
                "name": "ide-debug",
                "configurePreset": "ide-debug",
                "output": {"outputOnFailure": True},
            },
            {
                "name": "ide-release",
                "configurePreset": "ide-release",
                "output": {"outputOnFailure": True},
            },
        ],
    }
    (destination / "CMakePresets.json").write_text(
        json.dumps(presets, indent=2) + "\n", encoding="utf-8",
    )


BUILD = Stage("build", ["cmake", "--build", "build", "--parallel", "2"])
DRY_RUN = Stage("dry-run", ["ninja", "-C", "build", "-n", "-v"], (), ninja_dry_run=True)
TEST = Stage("test", ["ctest", "--test-dir", "build", "--output-on-failure", "--no-tests=error"],
             ("ctest",))


def scenarios(vcpkg: str | None, vcpkg_options: list[str], rust_driver: Path | None) -> list[Scenario]:
    """Return the local and external ecosystem command lists."""
    result = [
        Scenario(
            "recursive-driver", "recursive-driver", ("ninja",), [
                configure("Ninja"), BUILD, Stage(TEST.name, [*TEST.argv(), "-V"],
                                                 ("ctest", "cmake", "cpack")),
                Stage("install", ["cmake", "--install", "build", "--prefix", "install"]),
                Stage("package", ["cpack", "--config", "build/CPackConfig.cmake", "-B", "packages"],
                      ("cpack",)),
            ],
            ("build/version.h",),
            category="cmake",
            metadata={"generator": "Ninja", "ctest": True, "compile_commands": True},
        ),
        Scenario(
            "externalproject-fetchcontent", "local-dependencies", ("make",), [
                configure("Unix Makefiles"), BUILD,
            ],
            category="cmake",
            metadata={"generator": "Unix Makefiles", "compile_commands": True},
        ),
        Scenario(
            "pkg-config", "local-dependencies", ("ninja", "pkg-config"), [
                configure("Ninja", "-DUSE_PKG_CONFIG=ON"), DRY_RUN, BUILD,
                Stage("run", ["build/pkg-probe"], ()),
            ],
            category="pkg-config",
            metadata={"generator": "Ninja", "compile_commands": True},
        ),
        Scenario(
            "vcpkg-manifest", "vcpkg-manifest", ("ninja", "vcpkg"), [
                Stage("ports", f'{join([vcpkg or "vcpkg", "install"])} '
                      '--x-install-root="$PWD/build/vcpkg_installed" '
                      '--x-buildtrees-root="$PWD/build/vcpkg-buildtrees" '
                      '--x-packages-root="$PWD/build/vcpkg-packages"'),
                configure("Ninja", *vcpkg_options), DRY_RUN, BUILD, TEST,
            ],
            category="vcpkg",
            metadata={"package_manager": "vcpkg", "ctest": True, "compile_commands": True},
        ),
        Scenario(
            "scikit-build-core", "scikit-build-demo", ("ninja", "python3"), [
                Stage("wheel", ["python3", "-m", "pip", "wheel", "--no-cache-dir", "--no-deps",
                                "--wheel-dir", "wheels", "."]),
            ],
            category="python",
            metadata={"build_backend": "scikit-build-core"},
        ),
        Scenario(
            "meson", "meson-consumer", ("ninja", "meson"), [
                Stage("configure", ["meson", "setup", "build"]),
                Stage("build", ["meson", "compile", "-C", "build", "-j", "2"], ()),
                Stage("test", ["meson", "test", "-C", "build", "--print-errorlogs"], ()),
            ],
            category="meson",
            metadata={"build_system": "meson"},
        ),
    ]
    for name, option in (("conan-consumer", "--build=missing"), ("conan-source-build", "--build=*")):
        forced_source = name == "conan-source-build"
        result.append(Scenario(
            name, "conan-consumer", ("ninja", "conan"), [
                Stage("profile", ["conan", "profile", "detect"], ()),
                Stage("dependencies", ["conan", "install", ".", option, "-s", "build_type=Release",
                                       "-c", "tools.cmake.cmaketoolchain:generator=Ninja"],
                      ("cmake",) if forced_source else ()),
                Stage("configure", ["cmake", "--preset", "conan-release",
                                    "-DCMAKE_EXPORT_COMPILE_COMMANDS=ON"]),
                Stage("dry-run", ["cmake", "--build", "--preset", "conan-release", "--", "-n", "-v"],
                      ninja_dry_run=True),
                Stage("build", ["cmake", "--build", "--preset", "conan-release", "--parallel", "2"]),
                Stage("test", ["ctest", "--preset", "conan-release", "--output-on-failure",
                               "--no-tests=error"], ("ctest",)),
            ],
            category="conan",
            metadata={
                "package_manager": "conan",
                "generators": ["CMakeToolchain", "CMakeDeps"],
                "layout": "cmake_layout",
                "build_policy": option,
                "forced_source_build": forced_source,
                "preset": "conan-release",
                "ctest": True,
                "compile_commands": True,
            },
            require_compile_commands=True,
        ))
    result.append(Scenario(
        "ide-import",
        "ide-import",
        ("ninja",),
        [
            configure_preset("configure-debug", "ide-debug", "build/debug", IDE_FILE_API_QUERIES),
            Stage("build-debug", ["cmake", "--build", "--preset", "ide-debug", "--parallel", "2"]),
            Stage("test-debug", ["ctest", "--preset", "ide-debug", "--output-on-failure",
                                 "--no-tests=error"], ("ctest",)),
            configure_preset("configure-release", "ide-release", "build/release", IDE_FILE_API_QUERIES),
            Stage("dry-run-release", ["cmake", "--build", "--preset", "ide-release", "--", "-n", "-v"],
                  ninja_dry_run=True),
            Stage("build-release", ["cmake", "--build", "--preset", "ide-release", "--parallel", "2"]),
            Stage("test-release", ["ctest", "--preset", "ide-release", "--output-on-failure",
                                   "--no-tests=error"], ("ctest",)),
        ],
        category="ide",
        metadata={
            "file_api_queries": list(IDE_FILE_API_QUERIES),
            "compile_commands": True,
            "custom_toolchain": "cmake/ide-toolchain.cmake",
            "presets": ["ide-debug", "ide-release"],
            "configurations": list(IDE_CONFIGURATIONS),
            "targets": list(IDE_TARGETS),
            "ctest": True,
        },
        prepare_source=write_ide_import_fixture,
        required_ide_queries=IDE_FILE_API_QUERIES,
        required_configurations=IDE_CONFIGURATIONS,
        require_compile_commands=True,
    ))
    result.append(Scenario(
        "rust-cmake-crate", "recursive-driver/sub",
        (str(rust_driver) if rust_driver else "--rust-cmake-driver",),
        [Stage("driver", [str(rust_driver)])],
        category="rust-cmake",
        metadata={"driver": "rust-cmake-crate"},
    ))
    return result


def extract_preset(command: str | list[str]) -> str | None:
    """Return the CMake or CTest preset name referenced by a stage command."""
    try:
        tokens = shlex.split(command, posix=True) if isinstance(command, str) else list(command)
    except ValueError:
        return None
    for index, token in enumerate(tokens):
        if token == "--preset" and index + 1 < len(tokens):
            return tokens[index + 1]
        if token.startswith("--preset=") and len(token) > len("--preset="):
            return token.split("=", 1)[1]
    return None


def _is_ctest_stage(stage: Stage, row: dict[str, Any] | None = None) -> bool:
    if "ctest" in stage.expect:
        return True
    command = row.get("command") if isinstance(row, dict) else stage.command
    if isinstance(command, list) and command:
        return Path(command[0]).name == "ctest"
    if isinstance(command, str):
        try:
            tokens = shlex.split(command, posix=True)
        except ValueError:
            return False
        return any(Path(tok).name == "ctest" for tok in tokens)
    return False


def _evaluate_ide_requirements(
    scenario: Scenario,
    ide_report: dict[str, Any] | None,
) -> tuple[list[str], list[str]]:
    """Return (oracle_errors, parity_failures) for a scenario's IDE contracts."""
    oracle_errors: list[str] = []
    failures: list[str] = []
    requires_ide = bool(
        scenario.required_ide_queries
        or scenario.required_configurations
        or scenario.require_compile_commands
    )
    if ide_report is None:
        if requires_ide:
            failures.append("Missing IDE parity inspection data.")
        return oracle_errors, failures

    build_dirs = ide_report.get("build_dirs", {})
    configurations = ide_report.get("configurations", {})
    expected_dirs = [
        configurations[cfg]
        for cfg in scenario.required_configurations
        if cfg in configurations
    ]
    if scenario.require_compile_commands:
        checked_cc_dirs = expected_dirs or [
            key for key, row in build_dirs.items()
            if isinstance(row, dict) and "compile_commands" in row
        ]
        if not checked_cc_dirs:
            failures.append("Missing compile_commands.json in build outputs.")
        for key in checked_cc_dirs:
            cc = build_dirs.get(key, {}).get("compile_commands")
            if not isinstance(cc, dict):
                failures.append(f"{key}: missing compile_commands.json.")
            elif cc.get("status") != "pass":
                if cc.get("oracle_present") is False:
                    oracle_errors.append(f"{key}: oracle did not generate compile_commands.json.")
                else:
                    failures.append(f"{key}: compile_commands.json parity failed.")
            elif cc.get("oracle_count", 0) == 0:
                oracle_errors.append(f"{key}: oracle compile_commands.json is empty.")

    if scenario.required_configurations:
        for cfg in scenario.required_configurations:
            if cfg not in configurations:
                failures.append(f"Missing required build configuration {cfg}.")
        if {"Debug", "Release"} <= set(scenario.required_configurations):
            switches = ide_report.get("configuration_switches", [])
            switch = next(
                (item for item in switches if item.get("from") == "Debug" and item.get("to") == "Release"),
                None,
            )
            if switch is None:
                failures.append("Missing Debug vs Release configuration switch comparison.")
            elif not switch.get("oracle_changed"):
                oracle_errors.append("Oracle Debug and Release configurations did not differ.")
            elif switch.get("status") != "pass":
                failures.append("Debug vs Release configuration switch parity failed.")

    if scenario.required_ide_queries:
        checked_dirs = [
            configurations[cfg]
            for cfg in scenario.required_configurations
            if cfg in configurations
        ] or list(build_dirs)
        if not checked_dirs:
            failures.append("No build directories with File API replies were found.")
        for key in checked_dirs:
            file_api = build_dirs.get(key, {}).get("file_api")
            if not isinstance(file_api, dict):
                failures.append(f"{key}: missing File API reply directory.")
                continue
            oracle_queries = set(file_api.get("oracle_queries", []))
            rcmake_queries = set(file_api.get("rcmake_queries", []))
            for query in scenario.required_ide_queries:
                if query not in oracle_queries:
                    oracle_errors.append(f"{key}: oracle missing File API reply {query}.")
                elif query not in rcmake_queries:
                    failures.append(f"{key}: rcmake missing File API reply {query}.")
                else:
                    qresult = file_api.get("queries", {}).get(query)
                    if not isinstance(qresult, dict) or qresult.get("status") != "pass":
                        failures.append(f"{key}: File API {query} parity failed.")

    if not requires_ide and ide_report.get("status") == "fail":
        failures.append("IDE File API or compile_commands parity failed.")
    return oracle_errors, failures


def build_scenario_receipt(scenario: Scenario, row: dict[str, Any]) -> dict[str, Any]:
    """Score one scenario execution into a structured per-scenario JSON receipt."""
    raw_status = row.get("status", "harness_error")
    oracle_run = row.get("oracle") if isinstance(row.get("oracle"), dict) else None
    rcmake_run = row.get("rcmake") if isinstance(row.get("rcmake"), dict) else None
    files_report = row.get("files") if isinstance(row.get("files"), dict) else None
    dry_runs = row.get("ninja_dry_runs") if isinstance(row.get("ninja_dry_runs"), dict) else {}
    ide_report = files_report.get("ide") if isinstance(files_report, dict) else None

    if raw_status in {"skipped_missing_tool", "harness_error"} or (
        raw_status == "oracle_error" and oracle_run is None
    ):
        return {
            "schema_version": RECEIPT_SCHEMA_VERSION,
            "scenario": scenario.name,
            "category": scenario.category,
            "fixture": scenario.fixture,
            "status": raw_status,
            "metadata": dict(scenario.metadata),
            "missing_tools": list(row.get("missing_tools", [])),
            "error": row.get("error"),
            "oracle_executable": row.get("oracle_executable"),
            "rcmake_executable": row.get("rcmake_executable"),
            "path_check": {"oracle_ok": None, "rcmake_ok": None, "oracle_error": None, "rcmake_error": None},
            "stages": [],
            "parity": {
                "all_stages_passed": False,
                "exit_codes_match": False,
                "exec_proof_passed": False,
                "file_parity": False,
                "file_counts": dict.fromkeys(BUCKETS, 0),
                "file_classes": {},
                "ninja_dry_run_parity": False,
                "ide_parity": None,
            },
            "presets": {"used": [], "stages": {}, "all_matched": False},
            "ctest": {"executed": False, "stages": [], "all_passed": False},
            "ide": None,
            "failures": [row["error"]] if row.get("error") else [],
        }

    oracle_stages = oracle_run.get("stages", []) if oracle_run else []
    rcmake_stages = rcmake_run.get("stages", []) if rcmake_run else []
    oracle_by_name = {item["name"]: item for item in oracle_stages if isinstance(item, dict) and "name" in item}
    rcmake_by_name = {item["name"]: item for item in rcmake_stages if isinstance(item, dict) and "name" in item}

    stage_receipts: list[dict[str, Any]] = []
    preset_stages: dict[str, dict[str, Any]] = {}
    presets_used: list[str] = []
    ctest_stages: list[dict[str, Any]] = []
    oracle_errors: list[str] = []
    failures: list[str] = []

    oracle_path_error = oracle_run.get("path_error") if oracle_run else None
    rcmake_path_error = rcmake_run.get("path_error") if rcmake_run else None
    if oracle_path_error:
        oracle_errors.append(f"Oracle PATH check failed: {oracle_path_error}")
    if rcmake_path_error:
        failures.append(f"Rcmake PATH check failed: {rcmake_path_error}")

    for spec in scenario.stages:
        oracle_stage = oracle_by_name.get(spec.name, {})
        rcmake_stage = rcmake_by_name.get(spec.name, {})
        oracle_status = oracle_stage.get("status", "missing")
        rcmake_status = rcmake_stage.get("status", "missing")
        oracle_code = oracle_stage.get("returncode")
        rcmake_code = rcmake_stage.get("returncode")
        exit_match = oracle_code is not None and rcmake_code is not None and oracle_code == rcmake_code
        exec_proof = rcmake_stage.get("exec_proof") if isinstance(rcmake_stage.get("exec_proof"), dict) else {}
        exec_status = exec_proof.get("status")
        missing_personas = list(exec_proof.get("missing_personas", []))
        preset = extract_preset(spec.command)

        if oracle_status != "pass":
            parity = "oracle_failed"
            oracle_errors.append(f"Oracle stage {spec.name} {oracle_status} (exit {oracle_code}).")
        elif rcmake_status == "pass" and exit_match and exec_status in {None, "pass"}:
            parity = "match"
        else:
            parity = "mismatch"
            failures.append(
                f"Stage {spec.name} diverged: oracle status={oracle_status} exit={oracle_code}, "
                f"rcmake status={rcmake_status} exit={rcmake_code}."
            )

        stage_entry = {
            "name": spec.name,
            "preset": preset,
            "expected_personas": list(spec.expect),
            "oracle_status": oracle_status,
            "rcmake_status": rcmake_status,
            "oracle_exit_code": oracle_code,
            "rcmake_exit_code": rcmake_code,
            "exit_code_match": exit_match,
            "exec_proof_status": exec_status,
            "missing_personas": missing_personas,
            "parity": parity,
        }
        stage_receipts.append(stage_entry)

        if preset:
            if preset not in presets_used:
                presets_used.append(preset)
            preset_stages[spec.name] = {
                "preset": preset,
                "oracle_status": oracle_status,
                "rcmake_status": rcmake_status,
                "oracle_exit_code": oracle_code,
                "rcmake_exit_code": rcmake_code,
                "exit_code_match": exit_match,
                "parity": parity,
            }

        if _is_ctest_stage(spec, oracle_stage):
            ctest_stages.append({
                "name": spec.name,
                "preset": preset,
                "oracle_status": oracle_status,
                "rcmake_status": rcmake_status,
                "oracle_exit_code": oracle_code,
                "rcmake_exit_code": rcmake_code,
                "exit_code_match": exit_match,
                "exec_proof_status": exec_status,
                "parity": parity,
            })

    file_classes = files_report.get("classes", {}) if files_report else {}
    file_counts = {
        bucket: sum(counts.get(bucket, 0) for counts in file_classes.values())
        for bucket in BUCKETS
    }
    file_parity = file_counts["different"] == 0
    if not file_parity:
        failures.append(f"{file_counts['different']} generated file(s) differed between oracle and rcmake.")

    dry_run_parity = not any(any(values.values()) for values in dry_runs.values())
    if not dry_run_parity:
        failures.append("Ninja dry-run commands differed between oracle and rcmake.")

    ide_oracle_errors, ide_failures = _evaluate_ide_requirements(scenario, ide_report)
    oracle_errors.extend(ide_oracle_errors)
    failures.extend(ide_failures)

    all_stages_passed = bool(stage_receipts) and all(
        item["oracle_status"] == "pass" and item["rcmake_status"] == "pass"
        for item in stage_receipts
    )
    exit_codes_match = bool(stage_receipts) and all(item["exit_code_match"] for item in stage_receipts)
    exec_proof_passed = all(item["exec_proof_status"] in {None, "pass"} for item in stage_receipts)
    ide_parity = None if (ide_report is None and not (ide_oracle_errors or ide_failures)) else not (
        ide_oracle_errors or ide_failures or (ide_report is not None and ide_report.get("status") == "fail")
    )

    if oracle_errors or raw_status == "oracle_error":
        status = "oracle_error"
    elif (
        failures
        or raw_status == "fail"
        or not all_stages_passed
        or not exit_codes_match
        or not exec_proof_passed
        or not file_parity
        or not dry_run_parity
        or ide_parity is False
    ):
        status = "fail"
    else:
        status = "pass"

    return {
        "schema_version": RECEIPT_SCHEMA_VERSION,
        "scenario": scenario.name,
        "category": scenario.category,
        "fixture": scenario.fixture,
        "status": status,
        "metadata": dict(scenario.metadata),
        "missing_tools": [],
        "error": oracle_errors[0] if oracle_errors else row.get("error"),
        "oracle_executable": row.get("oracle_executable"),
        "rcmake_executable": row.get("rcmake_executable"),
        "path_check": {
            "oracle_ok": oracle_path_error is None,
            "rcmake_ok": rcmake_path_error is None,
            "oracle_error": oracle_path_error,
            "rcmake_error": rcmake_path_error,
        },
        "stages": stage_receipts,
        "parity": {
            "all_stages_passed": all_stages_passed,
            "exit_codes_match": exit_codes_match,
            "exec_proof_passed": exec_proof_passed,
            "file_parity": file_parity,
            "file_counts": file_counts,
            "file_classes": file_classes,
            "ninja_dry_run_parity": dry_run_parity,
            "ide_parity": ide_parity,
        },
        "presets": {
            "used": presets_used,
            "stages": preset_stages,
            "all_matched": bool(preset_stages) and all(
                item["parity"] == "match" for item in preset_stages.values()
            ),
        },
        "ctest": {
            "executed": bool(ctest_stages),
            "stages": ctest_stages,
            "all_passed": bool(ctest_stages) and all(
                item["parity"] == "match" and item["oracle_exit_code"] == 0 and item["rcmake_exit_code"] == 0
                for item in ctest_stages
            ),
        },
        "ide": ide_report,
        "failures": [*oracle_errors, *failures],
    }


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


def run_scenario(
    scenario: Scenario,
    binary: Path,
    oracle: Path,
    artifacts: Path,
    timeout: int,
    vcpkg_root: Path | None,
    *,
    receipts_dir: Path | None = None,
) -> dict[str, Any]:
    """Skip missing host tools, execute available scenarios, and emit a structured receipt."""
    name = scenario.name
    missing = [tool for tool in scenario.requires if not executable_on_path(tool)]
    if missing:
        raw: dict[str, Any] = {"name": name, "status": "skipped_missing_tool", "missing_tools": missing}
    elif name == "vcpkg-manifest" and not (
        vcpkg_root and (vcpkg_root / "scripts/buildsystems/vcpkg.cmake").is_file()
    ):
        raw = {"name": name, "status": "oracle_error", "error": "Set VCPKG_ROOT to a vcpkg checkout."}
    else:
        print(f"Running {name}", flush=True)
        try:
            if scenario.prepare_source is not None:
                source = artifacts / "fixture"
                scenario.prepare_source(source)
            else:
                source = FIXTURES / scenario.fixture
            row = compare_runs(
                source, scenario.stages, binary, oracle, artifacts, timeout,
                configured_outputs=scenario.configured_outputs,
                oracle_env=scenario_environment(name, artifacts / "oracle"),
                rcmake_env=scenario_environment(name, artifacts / "rcmake"),
            )
            raw = {"name": name, **row}
        except (OSError, ValueError, subprocess.SubprocessError) as error:
            raw = {"name": name, "status": "harness_error", "error": str(error)}

    receipt = build_scenario_receipt(scenario, raw)
    raw["status"] = receipt["status"]
    raw["receipt"] = receipt
    receipt_path = (receipts_dir / f"{name}.json") if receipts_dir is not None else (artifacts / "receipt.json")
    write_json(receipt_path, receipt)
    raw["receipt_path"] = str(receipt_path)
    return raw


def build_report(rows: list[dict[str, Any]]) -> dict[str, Any]:
    """Assemble the top-level JSON report and indexed scenario receipts."""
    statuses = {row["status"] for row in rows}
    if statuses & {"oracle_error", "harness_error"}:
        status = "error"
    elif "fail" in statuses:
        status = "fail"
    elif "pass" in statuses:
        status = "pass"
    else:
        status = "skipped_missing_tool"

    passed = sum(1 for row in rows if row["status"] == "pass")
    failed = sum(1 for row in rows if row["status"] == "fail")
    errors = sum(1 for row in rows if row["status"] in {"oracle_error", "harness_error"})
    skipped = sum(1 for row in rows if row["status"] == "skipped_missing_tool")
    receipts = {
        row["name"]: row["receipt"]
        for row in rows
        if isinstance(row.get("receipt"), dict)
    }
    return {
        "schema_version": RECEIPT_SCHEMA_VERSION,
        "status": status,
        "summary": {
            "total": len(rows),
            "executed": len(rows) - skipped,
            "passed": passed,
            "failed": failed,
            "errors": errors,
            "skipped": skipped,
        },
        "scenarios": rows,
        "receipts": receipts,
    }


def markdown(report: dict[str, Any]) -> str:
    """Summarize scenario outcomes, PATH evidence, receipts, and file class counts."""
    lines = [
        "# Ecosystem PATH comparison", "", f"Result **{report['status']}**", "",
        "| Scenario | Result | Details |", "| --- | --- | --- |",
    ]
    for row in report["scenarios"]:
        notes = list(row.get("missing_tools", []))
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
        if receipt := row.get("receipt"):
            if receipt.get("parity", {}).get("ide_parity") is False:
                notes.append("IDE File API or compile_commands parity failed")
        lines.append(f"| {row['name']} | {row['status']} | {', '.join(notes)} |")

    for row in report["scenarios"]:
        if "files" not in row:
            continue
        lines.extend(["", f"## {row['name']}", ""])
        if cmake := row.get("rcmake", {}).get("tools", {}).get("cmake"):
            lines.append(f"`which -a cmake` returned `{cmake['which']}`.")
            lines.extend([f"The first version line was `{cmake['version']}`.", ""])
        if receipt := row.get("receipt"):
            if receipt.get("stages"):
                lines.extend([
                    "| Stage | Preset | Oracle exit | Rcmake exit | Parity |",
                    "| --- | --- | ---: | ---: | --- |",
                ])
                for stage in receipt["stages"]:
                    lines.append(
                        f"| {stage['name']} | {stage.get('preset') or '-'} | "
                        f"{stage['oracle_exit_code']} | {stage['rcmake_exit_code']} | {stage['parity']} |"
                    )
                lines.append("")
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


def select_scenarios(all_scenarios: list[Scenario], requested: list[str]) -> list[Scenario]:
    """Filter scenarios by requested names, rejecting unknown entries."""
    if not requested:
        return all_scenarios
    wanted = [item.strip() for raw in requested for item in raw.split(",") if item.strip()]
    by_name = {item.name: item for item in all_scenarios}
    unknown = sorted(set(wanted) - set(by_name))
    if unknown:
        raise ValueError(f"Unknown scenario(s): {', '.join(unknown)}")
    selected_names = set(wanted)
    return [item for item in all_scenarios if item.name in selected_names]


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    """Parse CLI options for ecosystem fixture evaluation."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--stage", type=Path, required=True, help="Stage directory or Rcmake executable.")
    parser.add_argument("--oracle", default=os.environ.get("RCMAKE_ORACLE_CMAKE"),
                        help="Path to the CMake 4.5.1 executable.")
    parser.add_argument("--out-json", "--json", dest="out_json", type=Path, required=True,
                        help="Path to the JSON report and per-scenario receipts.")
    parser.add_argument("--out-md", type=Path, help="Optional path to the Markdown report.")
    parser.add_argument("--receipts-dir", type=Path,
                        help="Optional directory for standalone per-scenario receipt JSON files.")
    parser.add_argument("--scenario", action="append", default=[],
                        help="Scenario name to run (repeatable or comma-separated). Defaults to all.")
    parser.add_argument("--timeout", type=int, default=600, help="Time limit in seconds per command.")
    parser.add_argument("--rust-cmake-driver", type=Path,
                        help="Path to a prebuilt executable that uses the Rust cmake crate.")
    args = parser.parse_args(argv)
    if not args.oracle or args.timeout < 1:
        parser.error("Set an oracle path and a positive timeout.")
    if args.out_md is not None and args.out_json.resolve() == args.out_md.resolve():
        parser.error("Use distinct paths for the JSON and Markdown reports.")
    return args


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
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
        chosen = select_scenarios(scenarios(vcpkg, vcpkg_options, rust_driver), args.scenario)
        rows = [
            run_scenario(
                item, binary, oracle, artifacts / item.name, args.timeout, vcpkg_root,
                receipts_dir=args.receipts_dir,
            )
            for item in chosen
        ]
        report = build_report(rows)
        write_json(args.out_json, report)
        if args.out_md is not None:
            args.out_md.parent.mkdir(parents=True, exist_ok=True)
            args.out_md.write_text(markdown(report), encoding="utf-8")
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        print(str(error), file=sys.stderr)
        return 2
    print(json.dumps({row["name"]: row["status"] for row in rows}, sort_keys=True))
    return {"pass": 0, "fail": 1, "error": 2, "skipped_missing_tool": 2}[report["status"]]


if __name__ == "__main__":
    raise SystemExit(main())

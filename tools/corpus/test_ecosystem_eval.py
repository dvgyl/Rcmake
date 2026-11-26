#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Test ecosystem scenario discovery, receipt emission, Conan parity, and IDE diffing."""

from __future__ import annotations

import contextlib
import io
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

import ecosystem_eval
import normalize
import pathswap


def _write_json(path: Path, payload: object) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")


def _build_ide_reply_tree(
    tree_root: Path,
    build_rel: str,
    *,
    config: str,
    hash_suffix: str,
    cmake_bin: str,
    cmake_root: str,
    codemodel_minor: int = 12,
    extra_target: bool = False,
    wrong_source_path: bool = False,
    drop_define: str | None = None,
    drop_cc_flag: str | None = None,
    extra_cc_flag: str | None = None,
    reverse_cc_order: bool = False,
    omit_queries: set[str] | None = None,
    override_cc_config: str | None = None,
) -> None:
    """Populate a realistic build directory with compile_commands.json and File API replies."""
    omit = omit_queries or set()
    effective_cfg = override_cc_config or config
    build_dir = tree_root / build_rel
    reply = build_dir / ".cmake/api/v1/reply"
    reply.mkdir(parents=True, exist_ok=True)

    is_debug = effective_cfg == "Debug"
    cfg_define = "IDE_CONFIG_DEBUG=1" if is_debug else "IDE_CONFIG_RELEASE=1"
    opt_flags = ["-g"] if is_debug else ["-O3", "-DNDEBUG"]

    math_defines = ["IDE_TOOLCHAIN_ACTIVE=1", "MATH_CORE_API=1", cfg_define]
    if drop_define and drop_define in math_defines:
        math_defines.remove(drop_define)

    app_defines = [
        f'APP_BUILD_CONFIG="{effective_cfg}"',
        "IDE_TOOLCHAIN_ACTIVE=1",
        "MATH_CORE_API=1",
        cfg_define,
    ]

    math_source = "src/wrong_math_ops.c" if wrong_source_path else "src/math_ops.c"
    math_target_file = f"target-math_core-{config}-{hash_suffix}.json"
    app_target_file = f"target-ide_app-{config}-{hash_suffix}.json"
    codemodel_file = f"codemodel-v2-{hash_suffix}.json"
    cache_file = f"cache-v2-{hash_suffix}.json"
    toolchains_file = f"toolchains-v1-{hash_suffix}.json"
    cmake_files_file = f"cmakeFiles-v1-{hash_suffix}.json"

    math_target = {
        "name": "math_core",
        "type": "STATIC_LIBRARY",
        "artifacts": [{"path": "libmath_core.a"}],
        "paths": {"source": ".", "build": "."},
        "backtraceGraph": {"files": ["CMakeLists.txt", f"{cmake_root}/Modules/CMakeDetermineSystem.cmake"]},
        "sourceGroups": [{"name": "Source Files"}, {"name": "Header Files"}],
        "sources": [
            {"path": math_source, "compileGroupIndex": 0, "sourceGroupIndex": 0},
            {"path": "include/math_ops.h", "sourceGroupIndex": 1},
        ],
        "compileGroups": [{
            "language": "C",
            "languageStandard": {"standard": "11"},
            "compileCommandFragments": [{"fragment": " ".join(opt_flags)}],
            "defines": [{"define": d} for d in math_defines],
            "includes": [{"path": str(tree_root / "include"), "isSystem": False}],
            "sourceIndexes": [0],
        }],
    }
    app_target = {
        "name": "ide_app",
        "type": "EXECUTABLE",
        "artifacts": [{"path": "ide_app"}],
        "paths": {"source": ".", "build": "."},
        "dependencies": [{"id": "math_core::@6890427a1f51a3e7e1df"}],
        "sourceGroups": [{"name": "Source Files"}],
        "sources": [{"path": "src/main.cpp", "compileGroupIndex": 0, "sourceGroupIndex": 0}],
        "compileGroups": [{
            "language": "CXX",
            "languageStandard": {"standard": "17"},
            "compileCommandFragments": [{"fragment": " ".join(opt_flags)}],
            "defines": [{"define": d} for d in app_defines],
            "includes": [{"path": str(tree_root / "include"), "isSystem": False}],
            "sourceIndexes": [0],
        }],
    }

    target_refs = [
        {"id": "math_core::@6890427a1f51a3e7e1df", "jsonFile": math_target_file, "name": "math_core"},
        {"id": "ide_app::@6890427a1f51a3e7e1df", "jsonFile": app_target_file, "name": "ide_app"},
    ]
    if extra_target:
        stray_file = f"target-stray_tool-{config}-{hash_suffix}.json"
        target_refs.append({
            "id": "stray_tool::@6890427a1f51a3e7e1df", "jsonFile": stray_file, "name": "stray_tool",
        })
        _write_json(reply / stray_file, {
            "name": "stray_tool", "type": "EXECUTABLE", "paths": {"source": ".", "build": "."},
            "sources": [], "compileGroups": [],
        })

    reply_map: dict[str, dict] = {}
    if "codemodel-v2" not in omit:
        _write_json(reply / math_target_file, math_target)
        _write_json(reply / app_target_file, app_target)
        _write_json(reply / codemodel_file, {
            "kind": "codemodel",
            "version": {"major": 2, "minor": codemodel_minor},
            "paths": {"source": str(tree_root), "build": str(build_dir)},
            "configurations": [{
                "name": config,
                "projects": [{"name": "IdeImportDemo"}],
                "targets": target_refs,
            }],
        })
        reply_map["codemodel-v2"] = {
            "jsonFile": codemodel_file, "kind": "codemodel", "version": {"major": 2, "minor": codemodel_minor},
        }

    if "cache-v2" not in omit:
        _write_json(reply / cache_file, {
            "kind": "cache",
            "version": {"major": 2, "minor": 0},
            "entries": [
                {"name": "CMAKE_BUILD_TYPE", "type": "STRING", "value": config},
                {"name": "CMAKE_COMMAND", "type": "INTERNAL", "value": cmake_bin},
                {"name": "CMAKE_ROOT", "type": "INTERNAL", "value": cmake_root},
                {"name": "CMAKE_EXPORT_COMPILE_COMMANDS", "type": "BOOL", "value": "ON"},
                {"name": "CMAKE_TOOLCHAIN_FILE", "type": "FILEPATH",
                 "value": str(tree_root / "cmake/ide-toolchain.cmake")},
                {"name": "IDE_CUSTOM_TOOLCHAIN", "type": "STRING", "value": "active"},
            ],
        })
        reply_map["cache-v2"] = {
            "jsonFile": cache_file, "kind": "cache", "version": {"major": 2, "minor": 0},
        }

    if "toolchains-v1" not in omit:
        _write_json(reply / toolchains_file, {
            "kind": "toolchains",
            "version": {"major": 1, "minor": 1},
            "toolchains": [
                {
                    "language": "C",
                    "compiler": {
                        "id": "Clang", "path": "/usr/bin/cc", "version": "18.1.0",
                        "implicit": {"includeDirectories": ["/usr/include"], "linkDirectories": ["/usr/lib"]},
                    },
                    "sourceFileExtensions": ["c"],
                },
                {
                    "language": "CXX",
                    "compiler": {
                        "id": "Clang", "path": "/usr/bin/c++", "version": "18.1.0",
                        "implicit": {"includeDirectories": ["/usr/include/c++"], "linkDirectories": ["/usr/lib"]},
                    },
                    "sourceFileExtensions": ["cpp", "cxx"],
                },
            ],
        })
        reply_map["toolchains-v1"] = {
            "jsonFile": toolchains_file, "kind": "toolchains", "version": {"major": 1, "minor": 1},
        }

    if "cmakeFiles-v1" not in omit:
        _write_json(reply / cmake_files_file, {
            "kind": "cmakeFiles",
            "version": {"major": 1, "minor": 1},
            "paths": {"source": str(tree_root), "build": str(build_dir)},
            "inputs": [
                {"path": "CMakeLists.txt"},
                {"path": "cmake/ide-toolchain.cmake"},
                {"path": f"{cmake_root}/Modules/CMakeDetermineSystem.cmake", "isCMake": True, "isExternal": True},
            ],
        })
        reply_map["cmakeFiles-v1"] = {
            "jsonFile": cmake_files_file, "kind": "cmakeFiles", "version": {"major": 1, "minor": 1},
        }

    _write_json(reply / f"index-2026-11-25T22-00-00-{hash_suffix[:4]}.json", {
        "cmake": {
            "generator": {"multiConfig": False, "name": "Ninja"},
            "paths": {
                "cmake": cmake_bin,
                "ctest": str(Path(cmake_bin).with_name("ctest")),
                "cpack": str(Path(cmake_bin).with_name("cpack")),
                "root": cmake_root,
            },
            "version": {"major": 4, "minor": 5, "patch": 1, "string": "4.5.1"},
        },
        "objects": list(reply_map.values()),
        "reply": reply_map,
    })

    math_cc_flags = [f"-D{d}" for d in math_defines] + [f"-I{tree_root / 'include'}", *opt_flags]
    if drop_cc_flag and drop_cc_flag in math_cc_flags:
        math_cc_flags.remove(drop_cc_flag)
    if extra_cc_flag:
        math_cc_flags.append(extra_cc_flag)

    app_cc_flags = [
        f'-DAPP_BUILD_CONFIG=\\"{effective_cfg}\\"',
        "-DIDE_TOOLCHAIN_ACTIVE=1",
        "-DMATH_CORE_API=1",
        f"-D{cfg_define}",
        f"-I{tree_root / 'include'}",
        *opt_flags,
    ]
    cc_entries = [
        {
            "directory": str(build_dir),
            "command": " ".join([
                "/usr/bin/cc", *math_cc_flags,
                "-o", "CMakeFiles/math_core.dir/src/math_ops.c.o",
                "-c", str(tree_root / math_source),
            ]),
            "file": str(tree_root / math_source),
            "output": "CMakeFiles/math_core.dir/src/math_ops.c.o",
        },
        {
            "directory": str(build_dir),
            "command": " ".join([
                "/usr/bin/c++", *app_cc_flags,
                "-o", "CMakeFiles/ide_app.dir/src/main.cpp.o",
                "-c", str(tree_root / "src/main.cpp"),
            ]),
            "file": str(tree_root / "src/main.cpp"),
            "output": "CMakeFiles/ide_app.dir/src/main.cpp.o",
        },
    ]
    if reverse_cc_order:
        cc_entries.reverse()
    _write_json(build_dir / "compile_commands.json", cc_entries)


def _passing_compare_row(scenario: ecosystem_eval.Scenario, *, ide: dict | None = None) -> dict:
    """Build a passing compare_runs result matching a scenario's stages."""
    oracle_stages = []
    rcmake_stages = []
    for stage in scenario.stages:
        base = {
            "name": stage.name,
            "command": stage.command,
            "expected_personas": stage.expect,
            "returncode": 0,
            "stdout": "",
            "stderr": "",
            "error": None,
            "status": "pass",
        }
        oracle_stages.append(dict(base))
        rcmake_stages.append({
            **base,
            "exec_proof": {"status": "pass", "count": len(stage.expect), "missing_personas": []},
        })
    files: dict = {
        "classes": {"compile_commands.json": {"byte_identical": 1, "identical_after_normalization": 0, "different": 0}},
        "files": [],
        "rules": normalize.RULES,
    }
    if ide is not None:
        files["ide"] = ide
    return {
        "source": f"/workspace/fixtures/{scenario.fixture}",
        "oracle_executable": "/opt/oracle/bin/cmake",
        "rcmake_executable": "/opt/stage/bin/rcmake",
        "status": "pass",
        "oracle": {"stages": oracle_stages, "oracle_env_unset": True},
        "rcmake": {"stages": rcmake_stages, "oracle_env_unset": True},
        "files": files,
        "ninja_dry_runs": {
            stage.name: {"oracle_only": [], "rcmake_only": []}
            for stage in scenario.stages
            if stage.ninja_dry_run
        },
    }


def _passing_ide_report() -> dict:
    return {
        "status": "pass",
        "build_dirs": {
            "build/debug": {
                "status": "pass",
                "configuration": "Debug",
                "compile_commands": {"status": "pass", "oracle_count": 3, "rcmake_count": 3},
                "file_api": {
                    "status": "pass",
                    "oracle_queries": sorted(normalize.IDE_FILE_API_QUERIES),
                    "rcmake_queries": sorted(normalize.IDE_FILE_API_QUERIES),
                    "queries": {q: {"status": "pass"} for q in normalize.IDE_FILE_API_QUERIES},
                },
            },
            "build/release": {
                "status": "pass",
                "configuration": "Release",
                "compile_commands": {"status": "pass", "oracle_count": 3, "rcmake_count": 3},
                "file_api": {
                    "status": "pass",
                    "oracle_queries": sorted(normalize.IDE_FILE_API_QUERIES),
                    "rcmake_queries": sorted(normalize.IDE_FILE_API_QUERIES),
                    "queries": {q: {"status": "pass"} for q in normalize.IDE_FILE_API_QUERIES},
                },
            },
        },
        "configurations": {"Debug": "build/debug", "Release": "build/release"},
        "configuration_switches": [{
            "from": "Debug",
            "to": "Release",
            "status": "pass",
            "deltas_match": True,
            "oracle_changed": True,
            "rcmake_changed": True,
        }],
    }


class EcosystemEvalTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()

    def _make_executable(self, path: Path) -> Path:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
        path.chmod(0o755)
        return path

    def test_scenario_discovery_covers_conan_and_ide_import(self) -> None:
        items = ecosystem_eval.scenarios(None, [], None)
        by_name = {item.name: item for item in items}

        self.assertIn("conan-consumer", by_name)
        self.assertIn("conan-source-build", by_name)
        self.assertIn("ide-import", by_name)

        consumer = by_name["conan-consumer"]
        self.assertEqual(consumer.category, "conan")
        self.assertEqual(consumer.metadata["generators"], ["CMakeToolchain", "CMakeDeps"])
        self.assertEqual(consumer.metadata["build_policy"], "--build=missing")
        self.assertEqual(consumer.metadata["preset"], "conan-release")
        self.assertTrue(consumer.metadata["ctest"])
        dep_stage = next(s for s in consumer.stages if s.name == "dependencies")
        self.assertIn("--build=missing", dep_stage.command)
        test_stage = next(s for s in consumer.stages if s.name == "test")
        self.assertEqual(ecosystem_eval.extract_preset(test_stage.command), "conan-release")
        self.assertIn("ctest", test_stage.expect)

        source_build = by_name["conan-source-build"]
        src_dep = next(s for s in source_build.stages if s.name == "dependencies")
        self.assertIn("--build=*", src_dep.command)
        self.assertEqual(src_dep.expect, ("cmake",))

        ide = by_name["ide-import"]
        self.assertEqual(ide.category, "ide")
        self.assertEqual(ide.required_ide_queries, normalize.IDE_FILE_API_QUERIES)
        self.assertEqual(ide.required_configurations, ("Debug", "Release"))
        self.assertTrue(ide.require_compile_commands)
        self.assertIsNotNone(ide.prepare_source)
        cfg_debug = next(s for s in ide.stages if s.name == "configure-debug")
        cfg_release = next(s for s in ide.stages if s.name == "configure-release")
        for query in normalize.IDE_FILE_API_QUERIES:
            self.assertIn(query, cfg_debug.command)
            self.assertIn(query, cfg_release.command)
        self.assertEqual(ecosystem_eval.extract_preset(cfg_debug.command), "ide-debug")
        self.assertEqual(ecosystem_eval.extract_preset(cfg_release.command), "ide-release")

    def test_cli_accepts_json_alias_optional_md_and_scenario_filter(self) -> None:
        args = ecosystem_eval.parse_args([
            "--stage", "stage",
            "--oracle", "oracle/bin/cmake",
            "--json", "out/receipts.json",
            "--scenario", "conan-consumer,ide-import",
        ])
        self.assertEqual(args.out_json, Path("out/receipts.json"))
        self.assertIsNone(args.out_md)
        selected = ecosystem_eval.select_scenarios(ecosystem_eval.scenarios(None, [], None), args.scenario)
        self.assertEqual([item.name for item in selected], ["conan-consumer", "ide-import"])

        with self.assertRaisesRegex(ValueError, "Unknown scenario"):
            ecosystem_eval.select_scenarios(ecosystem_eval.scenarios(None, [], None), ["does-not-exist"])

        with contextlib.redirect_stderr(io.StringIO()):
            with self.assertRaises(SystemExit):
                ecosystem_eval.parse_args([
                    "--stage", "stage", "--oracle", "oracle",
                    "--out-json", "same.json", "--out-md", "same.json",
                ])

    def test_write_ide_import_fixture_creates_presets_toolchain_and_targets(self) -> None:
        fixture = self.root / "ide-fixture"
        ecosystem_eval.write_ide_import_fixture(fixture)
        presets = json.loads((fixture / "CMakePresets.json").read_text(encoding="utf-8"))
        configure_by_name = {item["name"]: item for item in presets["configurePresets"]}
        self.assertEqual(configure_by_name["ide-base"]["toolchainFile"], "${sourceDir}/cmake/ide-toolchain.cmake")
        self.assertEqual(configure_by_name["ide-base"]["cacheVariables"]["CMAKE_EXPORT_COMPILE_COMMANDS"], "ON")
        self.assertEqual(configure_by_name["ide-debug"]["cacheVariables"]["CMAKE_BUILD_TYPE"], "Debug")
        self.assertEqual(configure_by_name["ide-release"]["cacheVariables"]["CMAKE_BUILD_TYPE"], "Release")
        self.assertTrue((fixture / "cmake/ide-toolchain.cmake").is_file())
        self.assertTrue((fixture / "src/math_ops.c").is_file())
        self.assertTrue((fixture / "src/format_util.cpp").is_file())
        self.assertTrue((fixture / "src/main.cpp").is_file())
        self.assertTrue((fixture / "include/math_ops.h").is_file())
        self.assertTrue((fixture / "src/internal/format_detail.hpp").is_file())

    def test_compile_commands_diff_normalizes_roots_order_and_quoting(self) -> None:
        oracle_entries = [
            {
                "directory": "/workspace/oracle/build/debug",
                "command": '/usr/bin/c++ -DAPP=\\"Debug\\" -I /workspace/oracle/include -g -o app.o -c /workspace/oracle/src/main.cpp',
                "file": "/workspace/oracle/src/main.cpp",
                "output": "app.o",
            },
            {
                "directory": "/workspace/oracle/build/debug",
                "command": "/usr/bin/cc -DMATH=1 -I/workspace/oracle/include -g -o lib.o -c /workspace/oracle/src/lib.c",
                "file": "/workspace/oracle/src/lib.c",
                "output": "lib.o",
            },
        ]
        rcmake_entries = [
            {
                "directory": "/workspace/rcmake/build/debug",
                "arguments": [
                    "/usr/bin/cc", "-D", "MATH=1", "-I/workspace/rcmake/include",
                    "-g", "-o", "lib.o", "-c", "/workspace/rcmake/src/lib.c",
                ],
                "file": "/workspace/rcmake/src/lib.c",
                "output": "lib.o",
            },
            {
                "directory": "/workspace/rcmake/build/debug",
                "arguments": [
                    "/usr/bin/c++", '-DAPP="Debug"', "-I/workspace/rcmake/include",
                    "-g", "-o", "app.o", "-c", "/workspace/rcmake/src/main.cpp",
                ],
                "file": "/workspace/rcmake/src/main.cpp",
                "output": "app.o",
            },
        ]
        diff = normalize.diff_compile_commands(
            oracle_entries, rcmake_entries,
            oracle_root=Path("/workspace/oracle"), rcmake_root=Path("/workspace/rcmake"),
        )
        self.assertEqual(diff["status"], "pass")
        self.assertEqual(diff["entry_diffs"], [])
        self.assertEqual(diff["missing_entries"], [])
        self.assertEqual(diff["extra_entries"], [])

    def test_compile_commands_diff_catches_missing_flag_wrong_dir_and_extra_entry(self) -> None:
        oracle_entries = [{
            "directory": "/workspace/oracle/build/debug",
            "command": "/usr/bin/cc -DIDE_TOOLCHAIN_ACTIVE=1 -DIDE_CONFIG_DEBUG=1 -I/workspace/oracle/include -g -o a.o -c /workspace/oracle/src/a.c",
            "file": "/workspace/oracle/src/a.c",
            "output": "a.o",
        }]
        rcmake_entries = [
            {
                "directory": "/workspace/rcmake/build/release",
                "command": "/usr/bin/cc -DIDE_TOOLCHAIN_ACTIVE=1 -I/workspace/rcmake/include -g -Wextra -o a.o -c /workspace/rcmake/src/a.c",
                "file": "/workspace/rcmake/src/a.c",
                "output": "a.o",
            },
            {
                "directory": "/workspace/rcmake/build/debug",
                "command": "/usr/bin/cc -o b.o -c /workspace/rcmake/src/b.c",
                "file": "/workspace/rcmake/src/b.c",
                "output": "b.o",
            },
        ]
        diff = normalize.diff_compile_commands(
            oracle_entries, rcmake_entries,
            oracle_root=Path("/workspace/oracle"), rcmake_root=Path("/workspace/rcmake"),
        )
        self.assertEqual(diff["status"], "fail")
        self.assertEqual(len(diff["extra_entries"]), 1)
        self.assertEqual(diff["extra_entries"][0]["file"], "<project>/src/b.c")
        self.assertEqual(len(diff["entry_diffs"]), 1)
        entry_diff = diff["entry_diffs"][0]
        self.assertEqual(entry_diff["missing_flags"], ["-DIDE_CONFIG_DEBUG=1"])
        self.assertEqual(entry_diff["extra_flags"], ["-Wextra"])
        self.assertEqual(entry_diff["directory_mismatch"], {
            "oracle": "<project>/build/debug", "rcmake": "<project>/build/release",
        })

    def test_pathswap_uses_the_original_root_for_ide_paths(self) -> None:
        oracle = self.root / "oracle" / "cmake"
        shim = self.root / "shim"
        stages = [pathswap.Stage("configure", ["cmake", "--preset", "debug"])]

        def evaluate(bin_dir, source, project, stages, env, timeout, exec_log):
            _build_ide_reply_tree(
                project, "build/debug", config="Debug", hash_suffix="aaaaaaaaaaaaaaaaaaaa",
                cmake_bin=str(bin_dir / "cmake"), cmake_root="/opt/cmake/share/cmake-4.5",
            )
            return {"stages": [{"status": "pass"}]}

        with (patch.object(pathswap, "executable"),
              patch.object(pathswap, "prepare_paths", return_value=(shim, {})),
              patch.object(pathswap, "evaluate", side_effect=evaluate)):
            result = pathswap.compare_runs(self.root, stages, shim / "rcmake", oracle,
                                           self.root / "artifacts", 10)

        build = result["files"]["ide"]["build_dirs"]["build/debug"]
        self.assertEqual(build["compile_commands"]["status"], "pass", build["compile_commands"])
        self.assertEqual(build["file_api"]["status"], "pass", build["file_api"])

    def test_compare_normalizes_file_api_replies_tool_paths_and_config_switch(self) -> None:
        oracle_tree = self.root / "oracle-proj"
        rcmake_tree = self.root / "rcmake-proj"
        oracle_bin = "/opt/cmake-4.5.1/bin/cmake"
        rcmake_bin = "/opt/rcmake-stage/bin/cmake"
        oracle_share = "/opt/cmake-4.5.1/share/cmake-4.5"
        rcmake_share = "/opt/rcmake-stage/share/cmake-4.5"

        for cfg, rel in (("Debug", "build/debug"), ("Release", "build/release")):
            _build_ide_reply_tree(
                oracle_tree, rel, config=cfg, hash_suffix="11111111111111111111",
                cmake_bin=oracle_bin, cmake_root=oracle_share,
            )
            _build_ide_reply_tree(
                rcmake_tree, rel, config=cfg, hash_suffix="22222222222222222222",
                cmake_bin=rcmake_bin, cmake_root=rcmake_share, reverse_cc_order=True,
            )

        tools = (
            {
                "cmake": {oracle_bin},
                "ctest": {str(Path(oracle_bin).with_name("ctest"))},
                "cpack": {str(Path(oracle_bin).with_name("cpack"))},
            },
            {
                "cmake": {rcmake_bin},
                "ctest": {str(Path(rcmake_bin).with_name("ctest"))},
                "cpack": {str(Path(rcmake_bin).with_name("cpack"))},
            },
        )
        ide = normalize.compare_ide_build_trees(
            oracle_tree, rcmake_tree,
            oracle_tools=tools[0], rcmake_tools=tools[1],
            required_queries=normalize.IDE_FILE_API_QUERIES,
        )
        self.assertEqual(ide["status"], "pass")
        self.assertEqual(ide["configurations"], {"Debug": "build/debug", "Release": "build/release"})
        self.assertEqual(len(ide["configuration_switches"]), 1)
        switch = ide["configuration_switches"][0]
        self.assertEqual(switch["status"], "pass")
        self.assertTrue(switch["deltas_match"])
        self.assertIn("math_core:IDE_CONFIG_RELEASE=1", switch["oracle"]["defines_added_in_release"])
        self.assertIn("math_core:IDE_CONFIG_DEBUG=1", switch["oracle"]["defines_removed_in_release"])
        self.assertIn("<project>/src/math_ops.c:-O3", switch["oracle"]["compile_command_flags_added_in_release"])
        self.assertIn("<project>/src/math_ops.c:-g", switch["oracle"]["compile_command_flags_removed_in_release"])

        same_oracle = self.root / "same-oracle"
        same_rcmake = self.root / "same-rcmake"
        _build_ide_reply_tree(
            same_oracle, "build/debug", config="Debug", hash_suffix="aaaaaaaaaaaaaaaaaaaa",
            cmake_bin=oracle_bin, cmake_root=oracle_share,
        )
        _build_ide_reply_tree(
            same_rcmake, "build/debug", config="Debug", hash_suffix="bbbbbbbbbbbbbbbbbbbb",
            cmake_bin=rcmake_bin, cmake_root=rcmake_share, reverse_cc_order=True,
        )
        for path in same_rcmake.rglob("*.json"):
            path.write_text(
                path.read_text(encoding="utf-8").replace(str(same_rcmake), str(same_oracle)),
                encoding="utf-8",
            )
        compared = normalize.compare(same_oracle, same_rcmake, self.root / "diffs", tools=tools)
        self.assertEqual(compared["classes"]["File API reply"]["different"], 0)
        self.assertEqual(compared["classes"]["compile_commands.json"]["different"], 0)
        used_rules = {rule for item in compared["files"] for rule in item["rules"]}
        self.assertIn("file_api.tool_path", used_rules)
        self.assertIn("file_api.reply_hash", used_rules)
        self.assertIn("compile_commands.order", used_rules)

    def test_ide_diff_catches_extra_target_wrong_source_missing_flag_and_broken_switch(self) -> None:
        oracle_tree = self.root / "ide-bad-oracle"
        rcmake_tree = self.root / "ide-bad-rcmake"
        oracle_bin = "/opt/cmake-4.5.1/bin/cmake"
        rcmake_bin = "/opt/rcmake-stage/bin/cmake"
        oracle_share = "/opt/cmake-4.5.1/share/cmake-4.5"
        rcmake_share = "/opt/rcmake-stage/share/cmake-4.5"

        _build_ide_reply_tree(
            oracle_tree, "build/debug", config="Debug", hash_suffix="11111111111111111111",
            cmake_bin=oracle_bin, cmake_root=oracle_share,
        )
        _build_ide_reply_tree(
            oracle_tree, "build/release", config="Release", hash_suffix="11111111111111111112",
            cmake_bin=oracle_bin, cmake_root=oracle_share,
        )
        _build_ide_reply_tree(
            rcmake_tree, "build/debug", config="Debug", hash_suffix="22222222222222222221",
            cmake_bin=rcmake_bin, cmake_root=rcmake_share,
            extra_target=True,
            wrong_source_path=True,
            drop_define="IDE_TOOLCHAIN_ACTIVE=1",
            drop_cc_flag="-DIDE_CONFIG_DEBUG=1",
            codemodel_minor=11,
        )
        _build_ide_reply_tree(
            rcmake_tree, "build/release", config="Release", hash_suffix="22222222222222222222",
            cmake_bin=rcmake_bin, cmake_root=rcmake_share,
            override_cc_config="Debug",
        )

        ide = normalize.compare_ide_build_trees(
            oracle_tree, rcmake_tree,
            required_queries=normalize.IDE_FILE_API_QUERIES,
        )
        self.assertEqual(ide["status"], "fail")

        dbg_cm = ide["build_dirs"]["build/debug"]["file_api"]["queries"]["codemodel-v2"]
        self.assertEqual(dbg_cm["status"], "fail")
        self.assertFalse(dbg_cm["version_match"])
        dbg_cfg = dbg_cm["configuration_diffs"]["Debug"]
        self.assertEqual(dbg_cfg["extra_targets"], ["stray_tool"])
        math_diff = dbg_cfg["target_diffs"]["math_core"]
        self.assertEqual(math_diff["sources"]["missing_paths"], ["src/math_ops.c"])
        self.assertEqual(math_diff["sources"]["extra_paths"], ["src/wrong_math_ops.c"])
        self.assertEqual(
            math_diff["compile_groups"]["diffs"][0]["defines"]["missing"],
            ["IDE_TOOLCHAIN_ACTIVE=1"],
        )

        dbg_cc = ide["build_dirs"]["build/debug"]["compile_commands"]
        self.assertEqual(dbg_cc["status"], "fail")
        self.assertEqual(dbg_cc["missing_entries"][0]["file"], "<project>/src/math_ops.c")
        self.assertEqual(dbg_cc["extra_entries"][0]["file"], "<project>/src/wrong_math_ops.c")

        switch = ide["configuration_switches"][0]
        self.assertEqual(switch["status"], "fail")
        self.assertTrue(switch["oracle_changed"])
        self.assertFalse(switch["deltas_match"])

    def test_conan_receipt_scoring_covers_presets_ctest_exit_codes_and_failures(self) -> None:
        by_name = {item.name: item for item in ecosystem_eval.scenarios(None, [], None)}
        consumer = by_name["conan-consumer"]
        source_build = by_name["conan-source-build"]

        conan_ide = {
            "status": "pass",
            "build_dirs": {
                "build/Release": {
                    "status": "pass",
                    "configuration": "Release",
                    "compile_commands": {"status": "pass", "oracle_count": 1, "rcmake_count": 1},
                },
            },
            "configurations": {"Release": "build/Release"},
            "configuration_switches": [],
        }
        pass_row = _passing_compare_row(consumer, ide=conan_ide)
        receipt = ecosystem_eval.build_scenario_receipt(consumer, pass_row)
        self.assertEqual(receipt["status"], "pass")
        self.assertEqual(receipt["presets"]["used"], ["conan-release"])
        self.assertTrue(receipt["presets"]["all_matched"])
        self.assertEqual(sorted(receipt["presets"]["stages"]), ["build", "configure", "dry-run", "test"])
        self.assertTrue(receipt["ctest"]["executed"])
        self.assertTrue(receipt["ctest"]["all_passed"])
        self.assertEqual(receipt["ctest"]["stages"][0]["oracle_exit_code"], 0)
        self.assertEqual(receipt["ctest"]["stages"][0]["rcmake_exit_code"], 0)
        self.assertTrue(receipt["parity"]["exit_codes_match"])
        self.assertTrue(receipt["parity"]["exec_proof_passed"])
        self.assertTrue(receipt["parity"]["file_parity"])

        ctest_fail_row = _passing_compare_row(consumer, ide=conan_ide)
        ctest_fail_row["status"] = "fail"
        ctest_fail_row["rcmake"]["stages"][-1]["returncode"] = 8
        ctest_fail_row["rcmake"]["stages"][-1]["status"] = "failed"
        fail_receipt = ecosystem_eval.build_scenario_receipt(consumer, ctest_fail_row)
        self.assertEqual(fail_receipt["status"], "fail", "ctest exit mismatch must fail receipt")
        self.assertFalse(fail_receipt["ctest"]["all_passed"])
        self.assertFalse(fail_receipt["parity"]["exit_codes_match"])
        self.assertEqual(fail_receipt["ctest"]["stages"][0]["oracle_exit_code"], 0)
        self.assertEqual(fail_receipt["ctest"]["stages"][0]["rcmake_exit_code"], 8)
        self.assertFalse(fail_receipt["presets"]["all_matched"])

        proof_fail_row = _passing_compare_row(source_build, ide=conan_ide)
        proof_fail_row["rcmake"]["stages"][1]["status"] = "missing_exec_log"
        proof_fail_row["rcmake"]["stages"][1]["exec_proof"] = {
            "status": "missing_exec_log", "count": 0, "missing_personas": ["cmake"],
        }
        proof_receipt = ecosystem_eval.build_scenario_receipt(source_build, proof_fail_row)
        self.assertEqual(proof_receipt["status"], "fail", "missing cmake exec proof must fail source-build")
        self.assertFalse(proof_receipt["parity"]["exec_proof_passed"])
        self.assertEqual(proof_receipt["stages"][1]["missing_personas"], ["cmake"])

        oracle_fail_row = _passing_compare_row(consumer, ide=conan_ide)
        oracle_fail_row["status"] = "oracle_error"
        oracle_fail_row["oracle"]["stages"][1]["status"] = "failed"
        oracle_fail_row["oracle"]["stages"][1]["returncode"] = 1
        oracle_receipt = ecosystem_eval.build_scenario_receipt(consumer, oracle_fail_row)
        self.assertEqual(oracle_receipt["status"], "oracle_error", "oracle failure must not score as pass/fail")
        self.assertEqual(oracle_receipt["stages"][1]["parity"], "oracle_failed")

    def test_ide_import_receipt_requires_queries_compile_commands_and_config_switch(self) -> None:
        ide_scenario = next(item for item in ecosystem_eval.scenarios(None, [], None) if item.name == "ide-import")

        ok_receipt = ecosystem_eval.build_scenario_receipt(
            ide_scenario, _passing_compare_row(ide_scenario, ide=_passing_ide_report()),
        )
        self.assertEqual(ok_receipt["status"], "pass")
        self.assertTrue(ok_receipt["parity"]["ide_parity"])
        self.assertEqual(ok_receipt["presets"]["used"], ["ide-debug", "ide-release"])
        self.assertTrue(ok_receipt["ctest"]["all_passed"])

        missing_query_ide = _passing_ide_report()
        missing_query_ide["build_dirs"]["build/debug"]["file_api"]["rcmake_queries"] = [
            "cache-v2", "cmakeFiles-v1", "codemodel-v2",
        ]
        bad_receipt = ecosystem_eval.build_scenario_receipt(
            ide_scenario, _passing_compare_row(ide_scenario, ide=missing_query_ide),
        )
        self.assertEqual(bad_receipt["status"], "fail", "missing toolchains-v1 must fail ide-import receipt")
        self.assertFalse(bad_receipt["parity"]["ide_parity"])
        self.assertTrue(any("toolchains-v1" in msg for msg in bad_receipt["failures"]))

        missing_release_cc = _passing_ide_report()
        del missing_release_cc["build_dirs"]["build/release"]["compile_commands"]
        cc_receipt = ecosystem_eval.build_scenario_receipt(
            ide_scenario, _passing_compare_row(ide_scenario, ide=missing_release_cc),
        )
        self.assertEqual(cc_receipt["status"], "fail", "missing Release compile_commands.json must fail receipt")
        self.assertTrue(any("build/release: missing compile_commands.json" in msg for msg in cc_receipt["failures"]))

        missing_release_cfg = _passing_ide_report()
        del missing_release_cfg["configurations"]["Release"]
        missing_release_cfg["configuration_switches"] = []
        cfg_receipt = ecosystem_eval.build_scenario_receipt(
            ide_scenario, _passing_compare_row(ide_scenario, ide=missing_release_cfg),
        )
        self.assertEqual(cfg_receipt["status"], "fail", "missing Release configuration must fail receipt")
        self.assertTrue(any("Release" in msg for msg in cfg_receipt["failures"]))

    def test_main_emits_structured_json_receipts_and_standalone_files(self) -> None:
        stage_bin = self._make_executable(self.root / "stage/bin/rcmake")
        oracle_bin = self._make_executable(self.root / "oracle/bin/cmake")
        out_json = self.root / "reports/ecosystem.json"
        out_md = self.root / "reports/ecosystem.md"
        receipts_dir = self.root / "reports/receipts"

        all_by_name = {item.name: item for item in ecosystem_eval.scenarios(None, [], None)}

        def fake_compare(source, stages, binary, oracle, artifacts, timeout, **kwargs):
            name = artifacts.name
            scenario = all_by_name[name]
            if name == "ide-import":
                self.assertTrue((source / "CMakePresets.json").is_file())
                self.assertTrue((source / "cmake/ide-toolchain.cmake").is_file())
                return _passing_compare_row(scenario, ide=_passing_ide_report())
            return _passing_compare_row(scenario, ide={
                "status": "pass",
                "build_dirs": {
                    "build/Release": {
                        "status": "pass",
                        "configuration": "Release",
                        "compile_commands": {"status": "pass", "oracle_count": 1, "rcmake_count": 1},
                    },
                },
                "configurations": {"Release": "build/Release"},
                "configuration_switches": [],
            })

        with patch.object(ecosystem_eval, "executable_on_path", return_value="/usr/bin/tool"), \
                patch.object(ecosystem_eval, "compare_runs", side_effect=fake_compare), \
                contextlib.redirect_stdout(io.StringIO()):
            code = ecosystem_eval.main([
                "--stage", str(stage_bin.parent.parent),
                "--oracle", str(oracle_bin),
                "--json", str(out_json),
                "--out-md", str(out_md),
                "--receipts-dir", str(receipts_dir),
                "--scenario", "conan-consumer,conan-source-build,ide-import",
            ])

        self.assertEqual(code, 0)
        report = json.loads(out_json.read_text(encoding="utf-8"))
        self.assertEqual(report["status"], "pass")
        self.assertEqual(report["summary"], {
            "total": 3, "executed": 3, "passed": 3, "failed": 0, "errors": 0, "skipped": 0,
        })
        self.assertEqual(sorted(report["receipts"]), ["conan-consumer", "conan-source-build", "ide-import"])
        for name in ("conan-consumer", "conan-source-build", "ide-import"):
            standalone = json.loads((receipts_dir / f"{name}.json").read_text(encoding="utf-8"))
            self.assertEqual(standalone["scenario"], name)
            self.assertEqual(standalone["status"], "pass")
            self.assertEqual(standalone, report["receipts"][name])
        md_text = out_md.read_text(encoding="utf-8")
        self.assertIn("## ide-import", md_text)
        self.assertIn("conan-release", md_text)

    def test_main_returns_exit_one_when_a_scenario_fails(self) -> None:
        stage_bin = self._make_executable(self.root / "stage-fail/bin/rcmake")
        oracle_bin = self._make_executable(self.root / "oracle-fail/bin/cmake")
        out_json = self.root / "reports-fail/ecosystem.json"
        ide_scenario = next(item for item in ecosystem_eval.scenarios(None, [], None) if item.name == "ide-import")

        def failing_compare(source, stages, binary, oracle, artifacts, timeout, **kwargs):
            row = _passing_compare_row(ide_scenario, ide=_passing_ide_report())
            row["files"]["ide"]["configuration_switches"][0]["status"] = "fail"
            row["files"]["ide"]["configuration_switches"][0]["deltas_match"] = False
            row["files"]["ide"]["status"] = "fail"
            return row

        with patch.object(ecosystem_eval, "executable_on_path", return_value="/usr/bin/ninja"), \
                patch.object(ecosystem_eval, "compare_runs", side_effect=failing_compare), \
                contextlib.redirect_stdout(io.StringIO()):
            code = ecosystem_eval.main([
                "--stage", str(stage_bin.parent.parent),
                "--oracle", str(oracle_bin),
                "--json", str(out_json),
                "--scenario", "ide-import",
            ])

        self.assertEqual(code, 1)
        report = json.loads(out_json.read_text(encoding="utf-8"))
        self.assertEqual(report["status"], "fail")
        self.assertEqual(report["summary"]["failed"], 1)
        self.assertEqual(report["receipts"]["ide-import"]["status"], "fail")


if __name__ == "__main__":
    unittest.main()

#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Tests for unseen_eval L1 artifact capture and normalization."""

from __future__ import annotations

import json
from pathlib import Path
import sys
import tempfile
import unittest

CORPUS_DIR = Path(__file__).resolve().parent
sys.path.insert(0, str(CORPUS_DIR))

import unseen_eval


def _write_json(path: Path, payload: object) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")


class UnseenEvalCaptureL1Test(unittest.TestCase):
    def _seed_build(self, root: Path, *, shared_shim: bool = False) -> tuple[Path, Path, dict[str, Path]]:
        source = root / "src"
        build = root / "build"
        tools = root / "tools"
        source.mkdir(parents=True)
        build.mkdir(parents=True)
        tools.mkdir(parents=True)

        cmake_bin = tools / "cmake"
        cpack_bin = cmake_bin if shared_shim else tools / "cpack"
        ctest_bin = cmake_bin if shared_shim else tools / "ctest"
        ccmake_bin = cmake_bin if shared_shim else tools / "ccmake"
        modules_dir = root / "share" / "cmake"
        modules_dir.mkdir(parents=True)

        (build / "CMakeCache.txt").write_text(
            f"CMAKE_COMMAND:INTERNAL={cmake_bin}\n"
            f"CMAKE_CPACK_COMMAND:INTERNAL={cpack_bin}\n"
            f"CMAKE_CTEST_COMMAND:INTERNAL={ctest_bin}\n"
            f"CMAKE_EDIT_COMMAND:INTERNAL={ccmake_bin}\n"
            f"CMAKE_ROOT:INTERNAL={modules_dir}\n",
            encoding="utf-8",
        )
        (build / "build.ninja").write_text(
            "rule CUSTOM_COMMAND\n  command = $COMMAND\n"
            f"build CMakeFiles/edit_cache.util: CUSTOM_COMMAND\n"
            f"  COMMAND = {ccmake_bin} -S{source} -B{build}\n",
            encoding="utf-8",
        )

        reply = build / ".cmake" / "api" / "v1" / "reply"
        _write_json(
            reply / "index-2026.json",
            {"objects": [{"kind": "codemodel", "jsonFile": "codemodel-v2-abc.json"}]},
        )
        _write_json(
            reply / "target-check-abc.json",
            {
                "id": "check::@root",
                "name": "check",
                "dependencies": [
                    {"id": "test_z::@root"},
                    {"id": "test_a::@root"},
                ],
            },
        )
        _write_json(
            reply / "codemodel-v2-abc.json",
            {
                "configurations": [
                    {
                        "name": "",
                        "targets": [
                            {
                                "id": "check::@root",
                                "name": "check",
                                "jsonFile": "target-check-abc.json",
                            }
                        ],
                    }
                ]
            },
        )
        tool_paths = {
            "cmake": cmake_bin,
            "cpack": cpack_bin,
            "ctest": ctest_bin,
            "ccmake": ccmake_bin,
        }
        return source, build, tool_paths

    def test_maps_cmake_edit_command_to_ccmake_and_sorts_dependencies(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            source, build, tool_paths = self._seed_build(Path(tmp), shared_shim=False)
            artifacts, errors, _ = unseen_eval.capture_l1(
                build, source, tool_paths, {"returncode": 0, "stderr": ""}
            )
            self.assertEqual(errors, {})
            self.assertEqual(artifacts["cache"]["CMAKE_EDIT_COMMAND:INTERNAL"], "<ccmake>")
            self.assertEqual(artifacts["cache"]["CMAKE_COMMAND:INTERNAL"], "<cmake>")
            self.assertEqual(artifacts["cache"]["CMAKE_CTEST_COMMAND:INTERNAL"], "<ctest>")
            self.assertEqual(artifacts["cache"]["CMAKE_CPACK_COMMAND:INTERNAL"], "<cpack>")
            edit_edge = next(
                row
                for row in artifacts["ninja"]
                if row["outputs"] == ["<build>/CMakeFiles/edit_cache.util"]
            )
            self.assertEqual(edit_edge["command"], "<ccmake> -S<source> -B<build>")
            deps = artifacts["codemodel"]["configurations"][0]["targets"][0]["jsonFile"]["dependencies"]
            self.assertEqual([d["id"] for d in deps], ["test_a::@root", "test_z::@root"])

    def test_cmake_label_wins_when_all_shims_share_one_binary(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            source, build, tool_paths = self._seed_build(Path(tmp), shared_shim=True)
            artifacts, errors, _ = unseen_eval.capture_l1(
                build, source, tool_paths, {"returncode": 0, "stderr": ""}
            )
            self.assertEqual(errors, {})
            self.assertEqual(artifacts["cache"]["CMAKE_COMMAND:INTERNAL"], "<cmake>")
            self.assertEqual(artifacts["cache"]["CMAKE_EDIT_COMMAND:INTERNAL"], "<ccmake>")
            edit_edge = next(
                row
                for row in artifacts["ninja"]
                if row["outputs"] == ["<build>/CMakeFiles/edit_cache.util"]
            )
            self.assertEqual(edit_edge["command"], "<cmake> -S<source> -B<build>")


if __name__ == "__main__":
    unittest.main()

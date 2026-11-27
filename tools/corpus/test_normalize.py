#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Tests for codemodel normalization and Ninja phony canonicalization."""

from __future__ import annotations

from pathlib import Path
import sys
import tempfile
import unittest

CORPUS_DIR = Path(__file__).resolve().parent
MEASURE_DIR = CORPUS_DIR.parent / "measure"
sys.path.insert(0, str(CORPUS_DIR))
sys.path.insert(0, str(MEASURE_DIR))

import ninja_canon
import normalize


class NormalizeCodemodelDependenciesTest(unittest.TestCase):
    def test_sorts_inlined_target_dependencies_without_abstract_ties(self) -> None:
        codemodel_a = {
            "configurations": [
                {
                    "name": "",
                    "targets": [
                        {
                            "id": "check::@6890427a",
                            "name": "check",
                            "jsonFile": {
                                "id": "check::@6890427a",
                                "name": "check",
                                "dependencies": [
                                    {"id": "test_object::@6890427a", "backtrace": 2},
                                    {"id": "test_array::@6890427a", "backtrace": 1},
                                    {"id": "test_dump::@6890427a", "backtrace": 3},
                                ],
                            },
                        }
                    ],
                }
            ]
        }
        codemodel_b = {
            "configurations": [
                {
                    "name": "",
                    "targets": [
                        {
                            "id": "check::@6890427a",
                            "name": "check",
                            "jsonFile": {
                                "id": "check::@6890427a",
                                "name": "check",
                                "dependencies": [
                                    {"id": "test_dump::@6890427a", "backtrace": 3},
                                    {"id": "test_array::@6890427a", "backtrace": 1},
                                    {"id": "test_object::@6890427a", "backtrace": 2},
                                ],
                            },
                        }
                    ],
                }
            ]
        }

        norm_a = normalize.normalize_codemodel_abstract_target_ties(
            codemodel_a, dir_data_key="jsonFile"
        )
        norm_b = normalize.normalize_codemodel_abstract_target_ties(
            codemodel_b, dir_data_key="jsonFile"
        )
        self.assertEqual(norm_a, norm_b)
        deps = norm_a["configurations"][0]["targets"][0]["jsonFile"]["dependencies"]
        self.assertEqual(
            [dep["id"] for dep in deps],
            ["test_array::@6890427a", "test_dump::@6890427a", "test_object::@6890427a"],
        )

    def test_sorts_dependencies_alongside_abstract_target_tie_breaking(self) -> None:
        codemodel = {
            "configurations": [
                {
                    "name": "",
                    "abstractTargets": [
                        {
                            "id": "pkg::@b",
                            "name": "pkg",
                            "dependencies": [{"id": "z::@1"}, {"id": "a::@1"}],
                        },
                        {
                            "id": "pkg::@a",
                            "name": "pkg",
                            "dependencies": [{"id": "m::@1"}, {"id": "b::@1"}],
                        },
                    ],
                    "projects": [{"name": "demo", "abstractTargetIndexes": [0, 1]}],
                    "directories": [{"abstractTargetIndexes": [0, 1]}],
                    "targets": [
                        {
                            "id": "app::@1",
                            "name": "app",
                            "dependencies": [{"id": "pkg::@b"}, {"id": "pkg::@a"}],
                        }
                    ],
                }
            ]
        }
        result = normalize.normalize_codemodel_abstract_target_ties(codemodel)
        cfg = result["configurations"][0]
        self.assertEqual([t["id"] for t in cfg["abstractTargets"]], ["pkg::@a", "pkg::@b"])
        self.assertEqual(
            [d["id"] for d in cfg["abstractTargets"][0]["dependencies"]],
            ["b::@1", "m::@1"],
        )
        self.assertEqual(
            [d["id"] for d in cfg["targets"][0]["dependencies"]],
            ["pkg::@a", "pkg::@b"],
        )


class NinjaCanonPhonyOrderingTest(unittest.TestCase):
    def test_phony_inputs_are_sorted_while_rule_inputs_preserve_order(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            b1 = root / "b1"
            b2 = root / "b2"
            b1.mkdir()
            b2.mkdir()
            (b1 / "build.ninja").write_text(
                "rule LINK\n  command = cc $in -o $out\n"
                "build app: LINK b.o a.o\n"
                "build check: phony CMakeFiles/check test_object test_array\n",
                encoding="utf-8",
            )
            (b2 / "build.ninja").write_text(
                "rule LINK\n  command = cc $in -o $out\n"
                "build app: LINK b.o a.o\n"
                "build check: phony test_array CMakeFiles/check test_object\n",
                encoding="utf-8",
            )
            rows1 = ninja_canon.canonicalize(ninja_canon.Normalizer(b1))
            rows2 = ninja_canon.canonicalize(ninja_canon.Normalizer(b2))
            self.assertEqual(rows1, rows2)

            app_row = next(r for r in rows1 if r["outputs"] == ["<build>/app"])
            self.assertEqual(app_row["inputs"], ["<build>/b.o", "<build>/a.o"])
            check_row = next(r for r in rows1 if r["outputs"] == ["<build>/check"])
            self.assertEqual(
                check_row["inputs"],
                ["<build>/CMakeFiles/check", "<build>/test_array", "<build>/test_object"],
            )


if __name__ == "__main__":
    unittest.main()

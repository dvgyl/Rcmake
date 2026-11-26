# SPDX-License-Identifier: MIT OR Apache-2.0

from argparse import Namespace
from pathlib import Path
from tempfile import TemporaryDirectory
import unittest
from unittest.mock import patch

from path_swap_eval import ORACLE_VERSION, OUTPUT_STAGES, evaluate_project, normalize_output


class OutputParityTests(unittest.TestCase):
    def test_stage_output_changes_fail_the_comparison(self):
        for stage in OUTPUT_STAGES:
            for stream in ("stdout", "stderr"):
                with self.subTest(stage=stage, stream=stream), TemporaryDirectory() as directory:
                    root = Path(directory)
                    installations = {name: root / name / "bin" for name in ("oracle", "rcmake")}

                    def evaluate(name, bin_dir, project, source, env, args, *, fixture):
                        stages = {step: {"returncode": 0, "error": None, "stdout": "", "stderr": ""}
                                  for step in OUTPUT_STAGES}
                        stages[stage][stream] = "Expected output.\n" if name == "oracle" else "Changed output.\n"
                        return {"stages": stages}

                    with patch("path_swap_eval.evaluate", side_effect=evaluate):
                        report = evaluate_project(root / "source", root, installations, [], Namespace(),
                                                  fixture=True)
                    key = f"{stage}.{stream}"
                    self.assertEqual(report["status"], "fail")
                    self.assertEqual(report["differences"], [key])
                    diff = report["output_diffs"][key]
                    self.assertIn(f"--- cmake/{key}", diff)
                    self.assertIn(f"+++ rcmake/{key}", diff)
                    self.assertIn("-Expected output.", diff)
                    self.assertIn("+Changed output.", diff)

    def test_lane_paths_timing_and_version_footers_match(self):
        oracle_root, rcmake_root = Path("/tmp/oracle"), Path("/tmp/rcmake")
        for stage, oracle, rcmake in (
            ("configure", "-- Configuring done (3.2s)\n-- Generating done (0.1s)\n",
             "-- Configuring done (0.4s)\n-- Generating done (0.0s)\n"),
            ("test", "1/1 Test #1: payload ...   Passed    0.20 sec\n"
             "100% tests passed out of 1\nTotal Test time (real) =   0.20 sec\n",
             "1/1 Test #1: payload ...   Passed    0.01 sec\n"
             "100% tests passed out of 1\nTotal Test time (real) =   0.01 sec\n"),
            ("build", f"cmake version {ORACLE_VERSION}\n\n"
             "CMake suite maintained and supported by Kitware (kitware.com/cmake).\n",
             f"cmake version {ORACLE_VERSION}\nrcmake 0.1.0\n"),
            ("build-legacy", f"cmake version {ORACLE_VERSION}\n\n"
             "CMake suite maintained and supported by Kitware (kitware.com/cmake).\n",
             f"cmake version {ORACLE_VERSION}\nRcmake 0.1.0\n"),
        ):
            with self.subTest(stage=stage):
                left = normalize_output(f"{oracle_root}/payload\n{oracle}", {"project": oracle_root}, stage)
                right = normalize_output(f"{rcmake_root}/payload\n{rcmake}", {"project": rcmake_root}, stage)
                self.assertEqual(left, right)
                self.assertIn("<project>/payload", left)

    def test_preserve_diagnostics_and_test_summary(self):
        output = ("Rcmake is missing a file.\n"
                  "Warning: waited for 0.20 sec\n"
                  "1/2 Test #1: payload ...   ***Failed    0.20 sec\n"
                  "50% tests passed out of 2\n")
        normalized = normalize_output(output, {}, "test")
        self.assertEqual(normalized, output.replace("***Failed    0.20 sec", "***Failed <time> sec"))


if __name__ == "__main__":
    unittest.main()

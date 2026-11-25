# SPDX-License-Identifier: MIT OR Apache-2.0
"""Unit tests for tools/measure/probe_cache_eval.py."""

from __future__ import annotations

from contextlib import redirect_stderr, redirect_stdout
import io
import json
import os
from pathlib import Path
import subprocess
import sys
from tempfile import TemporaryDirectory
import time
import types
import unittest
from unittest import mock

_MEASURE_DIR = str(Path(__file__).resolve().parent)
if _MEASURE_DIR not in sys.path:
    sys.path.insert(0, _MEASURE_DIR)

import probe_cache_eval as pce


def _fake_executable(path: Path, body: str = "#!/bin/sh\nexit 0\n") -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(body, encoding="utf-8")
    path.chmod(0o755)
    return path


def _make_args(root: Path, **overrides) -> types.SimpleNamespace:
    rcmake = _fake_executable(root / "bin/rcmake", "#!/bin/sh\necho 'cmake version 4.5.1'\necho 'Rcmake 0.1.0'\n")
    oracle = _fake_executable(root / "bin/cmake", "#!/bin/sh\necho 'cmake version 4.5.1'\n")
    compiler = _fake_executable(root / "bin/cc")
    cxx = _fake_executable(root / "bin/c++")
    ninja = _fake_executable(
        root / "bin/ninja",
        '#!/bin/sh\nif [ "$1" = "--version" ]; then echo "1.13.2"; exit 0; fi\necho "REAL:$*"\n',
    )
    defaults = {
        "rcmake": rcmake,
        "oracle": oracle,
        "compiler": str(compiler),
        "cxx": str(cxx),
        "ninja": str(ninja),
        "generator": "Ninja",
        "smoke": False,
        "case": None,
        "extended": False,
        "all_cases": False,
        "seed": 0,
        "timeout": 30,
    }
    defaults.update(overrides)
    return types.SimpleNamespace(**defaults)


def _sample_probe_output(artifact_digest: str = "a" * 64) -> dict:
    return {
        "result": b"TRUE\n",
        "probe stdout": b"-- Probe results TRUE \n",
        "stdout": b"-- Probe results TRUE \n-- Configuring done\n",
        "stderr": b"",
        "logs": [b"Compiler reached the probe\n"],
        "copies": [(artifact_digest, 0o755)],
    }


class CaseRegistrationAndSelectionTests(unittest.TestCase):
    def test_baseline_and_extended_registries_have_unique_expected_cases(self):
        baseline_names = [spec.name for spec in pce.BASELINE_CASES]
        expected_baseline = [
            "cache-control",
            *pce.TRIGGERS.keys(),
            "parallel",
            "crash",
            "corrupt-entry",
        ]
        self.assertEqual(baseline_names, expected_baseline)
        self.assertEqual(len(pce.BASELINE_CASES), 16)

        extended_names = [spec.name for spec in pce.EXTENDED_CASES]
        self.assertEqual(
            extended_names,
            [
                "ninja-present",
                "ninja-swap",
                "flag-plugin",
                "env-inert",
                "run-env",
                "run-file",
                "runahead-effects",
                "runahead-mispredict",
            ],
        )

        all_names = [spec.name for spec in pce.ALL_CASES]
        self.assertEqual(len(all_names), len(set(all_names)))
        self.assertEqual(len(pce.ALL_CASES), 24)
        for spec in pce.ALL_CASES:
            self.assertIn(spec.gate, {"G3", "G4", "G3/G4"})
            self.assertIn(spec.group, {"baseline", "extended"})
            self.assertTrue(spec.description.strip())
            if spec.name not in pce.TRIGGERS:
                self.assertTrue(callable(spec.runner), spec.name)

    def test_select_cases_default_smoke_extended_all_and_explicit(self):
        self.assertEqual([s.name for s in pce.select_cases()], [s.name for s in pce.BASELINE_CASES])
        self.assertEqual([s.name for s in pce.select_cases(smoke=True)], ["cache-control", "transitive"])
        self.assertEqual(
            [s.name for s in pce.select_cases(extended=True)],
            [s.name for s in pce.EXTENDED_CASES],
        )
        self.assertEqual(
            [s.name for s in pce.select_cases(all_cases=True)],
            [s.name for s in pce.ALL_CASES],
        )
        picked = pce.select_cases(cases=["ninja-present,flag-plugin", "ninja-present", "run-file"])
        self.assertEqual([s.name for s in picked], ["ninja-present", "flag-plugin", "run-file"])

    def test_select_cases_rejects_unknown_empty_and_conflicting_flags(self):
        with self.assertRaisesRegex(ValueError, "Unknown case.*no-such-trap"):
            pce.select_cases(cases=["no-such-trap"])
        with self.assertRaisesRegex(ValueError, "at least one case"):
            pce.select_cases(cases=[" , "])
        with self.assertRaisesRegex(ValueError, "not both"):
            pce.select_cases(extended=True, all_cases=True)
        with self.assertRaisesRegex(ValueError, "Do not combine --case"):
            pce.select_cases(cases=["ninja-present"], extended=True)
        with self.assertRaisesRegex(ValueError, "Do not combine --case"):
            pce.select_cases(cases=["ninja-present"], all_cases=True)
        with self.assertRaisesRegex(ValueError, "--smoke cannot be combined"):
            pce.select_cases(smoke=True, extended=True)
        with self.assertRaisesRegex(ValueError, "--smoke cannot be combined"):
            pce.select_cases(smoke=True, all_cases=True)


class CliAndReportFormattingTests(unittest.TestCase):
    def setUp(self):
        tmp = TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name).resolve()

    def run_cli(self, argv: list[str]) -> tuple[int, str, str]:
        out, err = io.StringIO(), io.StringIO()
        with redirect_stdout(out), redirect_stderr(err):
            try:
                code = pce.main(argv)
            except SystemExit as exc:
                code = exc.code if isinstance(exc.code, int) else 1
        return code, out.getvalue(), err.getvalue()

    def test_list_cases_prints_baseline_and_extended_without_binary_args(self):
        code, stdout, stderr = self.run_cli(["--list-cases"])
        self.assertEqual(code, 0, stderr)
        self.assertIn("Baseline cases:", stdout)
        self.assertIn("Extended cases:", stdout)
        for spec in pce.ALL_CASES:
            self.assertIn(spec.name, stdout)

    def test_main_rejects_missing_args_same_output_paths_and_unknown_case(self):
        code, _, stderr = self.run_cli([])
        self.assertEqual(code, 2)
        self.assertIn("--rcmake", stderr)

        args = _make_args(self.root)
        same = self.root / "report.out"
        code, _, stderr = self.run_cli([
            "--rcmake", str(args.rcmake),
            "--oracle", str(args.oracle),
            "--cc", args.compiler,
            "--out-json", str(same),
            "--out-md", str(same),
        ])
        self.assertEqual(code, 2)
        self.assertIn("different paths", stderr)

        code, _, stderr = self.run_cli([
            "--rcmake", str(args.rcmake),
            "--oracle", str(args.oracle),
            "--cc", args.compiler,
            "--case", "does-not-exist",
            "--out-json", str(self.root / "out.json"),
            "--out-md", str(self.root / "out.md"),
        ])
        self.assertEqual(code, 2)
        self.assertIn("does-not-exist", stderr)

    def test_main_writes_expected_json_and_markdown_for_default_and_failing_case(self):
        args = _make_args(self.root)
        out_json = self.root / "baseline.json"
        out_md = self.root / "baseline.md"

        def fake_eval(_args, _dir, report):
            for spec in pce.BASELINE_CASES:
                report["cases"].append({"case": spec.name, "gate": spec.gate, "status": "passed", "passed": True})

        with mock.patch.object(pce, "evaluate", side_effect=fake_eval):
            code, _, stderr = self.run_cli([
                "--rcmake", str(args.rcmake),
                "--oracle", str(args.oracle),
                "--cc", args.compiler,
                "--out-json", str(out_json),
                "--out-md", str(out_md),
            ])
        self.assertEqual(code, 0, stderr)
        payload = json.loads(out_json.read_text(encoding="utf-8"))
        self.assertTrue(payload["passed"])
        self.assertEqual(payload["mode"], "gate")
        self.assertEqual(len(payload["cases"]), 16)
        self.assertTrue(all(row["status"] == "passed" for row in payload["cases"]))
        self.assertEqual(payload["available_extended_cases"], [spec.name for spec in pce.EXTENDED_CASES])

        md = out_md.read_text(encoding="utf-8")
        self.assertIn("| G3/G4 | cache-control | Passed |", md)
        self.assertIn("ninja-present", md)
        self.assertIn("runahead-mispredict", md)

        fail_json = self.root / "ninja-fail.json"
        fail_md = self.root / "ninja-fail.md"

        def fake_fail(_args, _dir, report):
            report["cases"].append({
                "case": "ninja-present",
                "gate": "G3/G4",
                "status": "failed",
                "passed": False,
                "published": 0,
                "error": "Entry count 0.",
            })

        with mock.patch.object(pce, "evaluate", side_effect=fake_fail):
            code, _, _ = self.run_cli([
                "--rcmake", str(args.rcmake),
                "--oracle", str(args.oracle),
                "--cc", args.compiler,
                "--case", "ninja-present",
                "--out-json", str(fail_json),
                "--out-md", str(fail_md),
            ])
        self.assertEqual(code, 1)
        fail_payload = json.loads(fail_json.read_text(encoding="utf-8"))
        self.assertFalse(fail_payload["passed"])
        self.assertNotIn("available_extended_cases", fail_payload)
        self.assertIn("## ninja-present\n\nEntry count 0.", fail_md.read_text(encoding="utf-8"))

    def test_evaluate_blocks_baseline_after_cache_control_failure_but_allows_direct_case(self):
        args = _make_args(self.root)
        report = {"mode": "gate", "seed": 0, "cases": []}

        def fake_version(cmd, *_a, **_kw):
            if Path(cmd[0]) == args.oracle:
                return {"returncode": 0, "stdout": "cmake version 4.5.1\n", "stderr": "", "error": None}
            return {"returncode": 0, "stdout": "cmake version 4.5.1\nRcmake 0.1.0\n", "stderr": "", "error": None}

        broken_control = pce.CaseSpec(
            "cache-control", "G3/G4", "baseline", "broken",
            lambda *_a, **_kw: (_ for _ in ()).throw(ValueError("Entry count 0.")),
        )
        patched_baseline = (broken_control, *pce.BASELINE_CASES[1:])
        with (
            mock.patch.object(pce, "run_command", side_effect=fake_version),
            mock.patch.object(pce, "select_cases", return_value=list(patched_baseline)),
            redirect_stderr(io.StringIO()),
        ):
            pce.evaluate(args, self.root / "work-1", report)

        self.assertEqual(report["cases"][0]["status"], "failed")
        self.assertEqual(len(report["cases"]), 16)
        self.assertTrue(all(row["status"] == "blocked" for row in report["cases"][1:]))

        ran = []
        direct_case = pce.CaseSpec(
            "ninja-present", "G3/G4", "extended", "direct",
            lambda _args, _dir, row: ran.append(row["case"]),
        )
        direct_report = {"mode": "gate", "seed": 0, "cases": []}
        args_direct = _make_args(self.root, case=["ninja-present"])
        with (
            mock.patch.object(pce, "run_command", side_effect=fake_version),
            mock.patch.object(pce, "select_cases", return_value=[direct_case]),
        ):
            pce.evaluate(args_direct, self.root / "work-2", direct_report)
        self.assertEqual(ran, ["ninja-present"])
        self.assertEqual(direct_report["cases"][0]["status"], "passed")


class NinjaHelpersAndSwapLogicTests(unittest.TestCase):
    def setUp(self):
        tmp = TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name).resolve()

    def test_stage_ninja_prepend_path_and_require_make_program(self):
        real_ninja = _fake_executable(
            self.root / "orig/ninja",
            '#!/bin/sh\nif [ "$1" = "--version" ]; then echo "1.13.2"; exit 0; fi\necho "REAL:$*"\n',
        )
        staged = pce.stage_ninja(real_ninja, self.root / "staged-bin", 10, os.environ.copy())
        self.assertTrue(staged.is_file())
        self.assertLess(staged.stat().st_mtime, time.time() - 5)
        self.assertEqual(pce.ninja_version(staged, self.root, 10, os.environ.copy()), "1.13.2")

        updated_env = pce.prepend_path({"PATH": "/usr/bin:/bin"}, staged.parent)
        self.assertTrue(updated_env["PATH"].startswith(f"{staged.parent}{os.pathsep}"))

        build = self.root / "build"
        build.mkdir()
        (build / "CMakeCache.txt").write_text(f"CMAKE_MAKE_PROGRAM:FILEPATH={staged}\n", encoding="utf-8")
        self.assertEqual(pce.require_ninja_make_program(build, staged), str(staged))

        (build / "CMakeCache.txt").write_text("CMAKE_MAKE_PROGRAM:FILEPATH=CMAKE_MAKE_PROGRAM-NOTFOUND\n",
                                              encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "did not resolve"):
            pce.require_ninja_make_program(build, staged)

        (build / "CMakeCache.txt").write_text(f"CMAKE_MAKE_PROGRAM:FILEPATH={real_ninja}\n", encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "instead of staged ninja"):
            pce.require_ninja_make_program(build, staged)

    def test_write_ninja_version_wrapper_changes_version_and_forwards_build_args(self):
        real_ninja = _fake_executable(
            self.root / "orig/ninja",
            '#!/bin/sh\nif [ "$1" = "--version" ]; then echo "1.13.2"; exit 0; fi\necho "REAL:$*"\n',
        )
        staged = pce.stage_ninja(real_ninja, self.root / "bin", 10, os.environ.copy())
        before_bytes = staged.read_bytes()

        pce.write_ninja_version_wrapper(staged, real_ninja, "1.10.0")
        self.assertNotEqual(staged.read_bytes(), before_bytes)
        self.assertEqual(pce.ninja_version(staged, self.root, 10, os.environ.copy()), "1.10.0")

        forwarded = subprocess.run([str(staged), "-v", "cmTC_demo"], capture_output=True, text=True, check=True)
        self.assertEqual(forwarded.stdout.strip(), "REAL:-v cmTC_demo")

    def test_ninja_swap_enforces_platform_scan_rule_expectation(self):
        self.assertEqual(pce.ninja_swap_expected_calls(True), 1)
        self.assertEqual(pce.ninja_swap_expected_calls(False), 0)

        for has_scan_rule, changed_calls, should_pass in (
            (True, 1, True),
            (True, 0, False),
            (False, 0, True),
            (False, 1, False),
        ):
            case_root = self.root / f"swap-{has_scan_rule}-{changed_calls}"
            args = _make_args(case_root)
            sample = _sample_probe_output()
            published = {case_root / "entry": pce.PublishedProbe("d" * 64, sample["copies"][0])}
            row = {}

            def fake_configure(_args, case, name, _env, *, oracle=False, generator=None, **_kw):
                self.assertEqual(generator, "Ninja")
                build = case.root / name
                build.mkdir(parents=True, exist_ok=True)
                staged = case.root / "ninja-bin/ninja"
                (build / "CMakeCache.txt").write_text(f"CMAKE_MAKE_PROGRAM:FILEPATH={staged}\n", encoding="utf-8")
                (build / "compiler-id.txt").write_text("Clang\n" if has_scan_rule else "AppleClang\n",
                                                       encoding="utf-8")
                (build / "has-scan-rule.txt").write_text("YES\n" if has_scan_rule else "NO\n", encoding="utf-8")
                if oracle:
                    return sample, 1
                counts = {"cold": 1, "warm": 0, "changed": changed_calls}
                return sample, counts[name]

            with (
                mock.patch.object(pce, "configure", side_effect=fake_configure),
                mock.patch.object(pce, "audit_store", return_value=published),
            ):
                if should_pass:
                    pce.ninja_swap(args, case_root, row)
                    self.assertEqual(row["has_scan_rule"], has_scan_rule)
                    self.assertEqual(row["expected_changed_calls"], 1 if has_scan_rule else 0)
                    self.assertEqual(row["ninja_version_after"], "1.10.0")
                else:
                    with self.assertRaisesRegex(ValueError, "Wrong compiler call count in the changed lane"):
                        pce.ninja_swap(args, case_root, row)


class ExtendedTrapBehaviorTests(unittest.TestCase):
    def setUp(self):
        tmp = TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name).resolve()

    def test_ninja_present_detects_zero_entries_and_passes_when_published(self):
        sample = _sample_probe_output()
        published = {self.root / "e.entry": pce.PublishedProbe("e" * 64, sample["copies"][0])}

        for entries, warm_calls, should_pass in (({}, 1, False), (published, 0, True)):
            case_root = self.root / f"np-{len(entries)}"
            args = _make_args(case_root)
            row = {}

            def fake_configure(_args, case, name, env, *, oracle=False, no_cache=False, generator=None, **_kw):
                self.assertEqual(generator, "Ninja")
                staged = case.root / "ninja-bin/ninja"
                self.assertTrue(env["PATH"].startswith(f"{staged.parent}{os.pathsep}"))
                build = case.root / name
                build.mkdir(parents=True, exist_ok=True)
                (build / "CMakeCache.txt").write_text(f"CMAKE_MAKE_PROGRAM:FILEPATH={staged}\n", encoding="utf-8")
                if oracle or no_cache or name == "cold":
                    return sample, 1
                return sample, warm_calls

            with (
                mock.patch.object(pce, "configure", side_effect=fake_configure),
                mock.patch.object(pce, "audit_store", return_value=entries),
            ):
                if should_pass:
                    pce.ninja_present(args, case_root, row)
                    self.assertEqual(row["published"], 1)
                    self.assertEqual(row["ninja_version"], "1.13.2")
                else:
                    with self.assertRaisesRegex(ValueError, "Entry count 0"):
                        pce.ninja_present(args, case_root, row)
                    self.assertEqual(row["published"], 0)

    def test_flag_plugin_refuses_store_entries_and_requires_fresh_compile(self):
        for store_entries, changed_calls, error_pattern in (
            ({}, 4, None),
            ({Path("/tmp/x.entry"): pce.PublishedProbe("f" * 64, None)}, 4, "must not publish"),
            ({}, 3, "Wrong compiler call count in the changed lane"),
        ):
            case_root = self.root / f"fp-{len(store_entries)}-{changed_calls}"
            args = _make_args(case_root)
            seen_plugin_bytes = {}
            row = {}

            def fake_raw(_args, case, name, _env, *, oracle=False, **_kw):
                build = case.root / name
                build.mkdir(parents=True, exist_ok=True)
                (build / "flag-results.txt").write_bytes(
                    b"FLAG_PLUGIN=0\nFLAG_USE_LD=0\nFLAG_MARCH_NATIVE=1\nFLAG_PROFILE_USE=0\n"
                )
                seen_plugin_bytes[name] = (case.root / "plugin.so").read_bytes()
                count = 4 if oracle or name == "cold" else changed_calls
                return {"stdout": b"-- ok\n", "stderr": b"", "build": build}, count

            with (
                mock.patch.object(pce, "configure_raw", side_effect=fake_raw),
                mock.patch.object(pce, "audit_store", return_value=store_entries),
            ):
                if error_pattern is None:
                    pce.flag_plugin(args, case_root, row)
                    self.assertEqual(row["published"], 0)
                    self.assertEqual(row["published_after_change"], 0)
                    self.assertEqual(seen_plugin_bytes["cold"], b"plugin-v1\n")
                    self.assertEqual(seen_plugin_bytes["changed"], b"plugin-v2\n")
                else:
                    with self.assertRaisesRegex(ValueError, error_pattern):
                        pce.flag_plugin(args, case_root, row)

    def test_env_inert_mutates_inert_vars_and_fails_on_warm_miss(self):
        sample = _sample_probe_output()
        published = {self.root / "e.entry": pce.PublishedProbe("b" * 64, sample["copies"][0])}

        for warm_calls, should_pass in ((0, True), (1, False)):
            case_root = self.root / f"inert-{warm_calls}"
            args = _make_args(case_root)
            seen_envs = {}
            seen_cwds = {}
            row = {}

            def fake_configure(_args, _case, name, env, *, oracle=False, cwd=None, **_kw):
                seen_envs[name] = dict(env)
                seen_cwds[name] = cwd
                if oracle or name == "cold":
                    return sample, 1
                return sample, warm_calls

            with (
                mock.patch.object(pce, "configure", side_effect=fake_configure),
                mock.patch.object(pce, "audit_store", return_value=published),
            ):
                if should_pass:
                    pce.env_inert(args, case_root, row)
                    self.assertNotEqual(seen_cwds["cold"], seen_cwds["warm"])
                    for var in row["mutated_vars"]:
                        self.assertNotEqual(seen_envs["cold"][var], seen_envs["warm"][var], var)
                else:
                    with self.assertRaisesRegex(ValueError, "Wrong compiler call count in the warm lane"):
                        pce.env_inert(args, case_root, row)

    def test_run_env_requires_cached_build_and_fresh_runtime_env(self):
        published = {self.root / "r.entry": pce.PublishedProbe("c" * 64, ("d" * 64, 0o755))}

        for entries, warm_calls, stale_inert, stale_trap, error_pattern in (
            (published, 0, False, False, None),
            ({}, 0, False, False, "Expected one published entry"),
            (published, 1, False, False, "Wrong compiler call count in the warm lane"),
            (published, 0, True, False, "stale GITHUB_RUN_ID"),
            (published, 0, False, True, "stale RCMAKE_TRAP"),
        ):
            case_root = self.root / f"re-{len(entries)}-{warm_calls}-{stale_inert}-{stale_trap}"
            args = _make_args(case_root)
            row = {}

            def fake_raw(_args, case, name, env, *, oracle=False, **_kw):
                build = case.root / name
                build.mkdir(parents=True, exist_ok=True)
                trap = env["RCMAKE_TRAP"]
                run_id = env["GITHUB_RUN_ID"]
                if stale_inert and name == "changed-inert" and not oracle:
                    run_id = "101"
                if stale_trap and name == "changed-trap" and not oracle:
                    trap = "alpha"
                (build / "compile-result.txt").write_text("TRUE\n", encoding="utf-8")
                (build / "run-result.txt").write_text("0\n", encoding="utf-8")
                (build / "run-output.txt").write_text(f"trap={trap};run={run_id}\n", encoding="utf-8")
                (build / "compile-output.log").write_bytes(pce.COMPILER_MARKER + b"\n")
                count = warm_calls if name == "warm" and not oracle else 1
                return {"stdout": b"-- TryRun compile=TRUE exit=0\n", "stderr": b"", "build": build}, count

            with (
                mock.patch.object(pce, "configure_raw", side_effect=fake_raw),
                mock.patch.object(pce, "audit_store", return_value=entries),
            ):
                if error_pattern is None:
                    pce.run_env(args, case_root, row)
                    self.assertEqual(row["published"], 1)
                else:
                    with self.assertRaisesRegex(ValueError, error_pattern):
                        pce.run_env(args, case_root, row)

    def test_run_file_requires_build_hit_and_observes_modified_file(self):
        published = {self.root / "f.entry": pce.PublishedProbe("1" * 64, ("2" * 64, 0o755))}

        for changed_calls, stale_file, error_pattern in (
            (0, False, None),
            (1, False, "Wrong compiler call count in the changed lane"),
            (0, True, "stale file output"),
        ):
            case_root = self.root / f"rf-{changed_calls}-{stale_file}"
            args = _make_args(case_root)
            row = {}

            def fake_raw(_args, case, name, _env, *, oracle=False, **_kw):
                build = case.root / name
                build.mkdir(parents=True, exist_ok=True)
                content = (case.root / "data.txt").read_bytes()
                if stale_file and name == "changed" and not oracle:
                    content = b"runtime-before\n"
                (build / "compile-result.txt").write_text("TRUE\n", encoding="utf-8")
                (build / "run-result.txt").write_text("0\n", encoding="utf-8")
                (build / "run-output.txt").write_bytes(content)
                (build / "compile-output.log").write_bytes(pce.COMPILER_MARKER + b"\n")
                count = changed_calls if name == "changed" and not oracle else 1
                return {"stdout": b"-- TryRun compile=TRUE exit=0\n", "stderr": b"", "build": build}, count

            with (
                mock.patch.object(pce, "configure_raw", side_effect=fake_raw),
                mock.patch.object(pce, "audit_store", return_value=published),
            ):
                if error_pattern is None:
                    pce.run_file(args, case_root, row)
                    self.assertEqual((case_root / "data.txt").read_bytes(), b"runtime-after\n")
                else:
                    with self.assertRaisesRegex(ValueError, error_pattern):
                        pce.run_file(args, case_root, row)

    def test_runahead_effects_detects_duplicate_file_stdout_or_cache_mutations(self):
        for defect, error_pattern in (
            ("none", None),
            ("dup-file", "Unexpected effects.log"),
            ("dup-stdout", "Runahead-effects CMake parity differs"),
            ("bad-cache", "Runahead-effects CMake parity differs for cache"),
        ):
            case_root = self.root / f"ra-eff-{defect}"
            args = _make_args(case_root)
            seen_jobs = {}
            row = {}

            def fake_raw(_args, case, name, env, *, oracle=False, **_kw):
                seen_jobs[name] = env["RCMAKE_PROBE_JOBS"]
                build = case.root / name
                build.mkdir(parents=True, exist_ok=True)
                effects = b"stage-1:1:1:1\nstage-2:1\n"
                stdout = b"-- Runahead effect 1 = 1\n-- Runahead effect 2 = 1\n"
                cache_val = "val-1-1"
                if name == "jobs8" and not oracle:
                    if defect == "dup-file":
                        effects = b"stage-1:1:1:1\nstage-1:1:1:1\nstage-2:1\n"
                    elif defect == "dup-stdout":
                        stdout = b"-- Runahead effect 1 = 1\n-- Runahead effect 1 = 1\n-- Runahead effect 2 = 1\n"
                    elif defect == "bad-cache":
                        cache_val = "corrupted"
                (build / "effects.log").write_bytes(effects)
                (build / "effect-write.txt").write_bytes(b"written:1:1:1\n")
                cache_lines = [
                    "RUNAHEAD_EFFECT_PROBE_1:INTERNAL=1",
                    "RUNAHEAD_EFFECT_PROBE_2:INTERNAL=1",
                    "RUNAHEAD_EFFECT_PROBE_3:INTERNAL=1",
                    "RUNAHEAD_EFFECT_PROBE_4:INTERNAL=1",
                    f"RUNAHEAD_CACHE_1:STRING={cache_val}",
                    "RUNAHEAD_CACHE_2:STRING=val-2-1",
                    "RUNAHEAD_CACHE_3:STRING=val-3-1",
                    "RUNAHEAD_CACHE_4:STRING=val-4-1",
                ]
                (build / "CMakeCache.txt").write_text("\n".join(cache_lines) + "\n", encoding="utf-8")
                return {"stdout": stdout, "stderr": b"", "build": build}, 4

            with mock.patch.object(pce, "configure_raw", side_effect=fake_raw):
                if error_pattern is None:
                    pce.runahead_effects(args, case_root, row)
                    self.assertEqual(seen_jobs["jobs8"], "8")
                    self.assertEqual(seen_jobs["jobs1"], "1")
                else:
                    with self.assertRaisesRegex(ValueError, error_pattern):
                        pce.runahead_effects(args, case_root, row)

    def test_runahead_mispredict_detects_wrong_branch_and_records_wasted_calls(self):
        for leak_branch, error_pattern in ((False, None), (True, "Unexpected mispredict results")):
            case_root = self.root / f"ra-mis-{leak_branch}"
            args = _make_args(case_root)
            seen_jobs = {}
            row = {}

            def fake_raw(_args, case, name, env, *, oracle=False, **_kw):
                seen_jobs[name] = env["RCMAKE_PROBE_JOBS"]
                build = case.root / name
                build.mkdir(parents=True, exist_ok=True)
                bad = leak_branch and name == "jobs8" and not oracle
                summary = (
                    b"MISPREDICT_FIRST=0\nMISPREDICT_SECOND=0\nMISPREDICT_THIRD=0\n"
                    if bad else b"MISPREDICT_FIRST=0\nMISPREDICT_SECOND=1\nMISPREDICT_THIRD=1\n"
                )
                (build / "mispredict-results.txt").write_bytes(summary)
                (build / "CMakeCache.txt").write_text(
                    "MISPREDICT_FIRST:INTERNAL=\n"
                    f"MISPREDICT_SECOND:INTERNAL={'' if bad else '1'}\n"
                    f"MISPREDICT_THIRD:INTERNAL={'' if bad else '1'}\n",
                    encoding="utf-8",
                )
                count = 5 if name == "jobs8" and not oracle else 3
                return {"stdout": b"-- Mispredict summary |1|1\n", "stderr": b"", "build": build}, count

            with mock.patch.object(pce, "configure_raw", side_effect=fake_raw):
                if error_pattern is None:
                    pce.runahead_mispredict(args, case_root, row)
                    self.assertEqual(seen_jobs["jobs8"], "8")
                    self.assertEqual(row["wasted_compiler_calls"], 2)
                else:
                    with self.assertRaisesRegex(ValueError, error_pattern):
                        pce.runahead_mispredict(args, case_root, row)


if __name__ == "__main__":
    unittest.main()

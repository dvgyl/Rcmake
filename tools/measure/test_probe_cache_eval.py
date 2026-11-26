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
    rcmake = _fake_executable(root / "bin/rcmake", "#!/bin/sh\necho 'cmake version 4.5.1'\necho 'rcmake 0.1.0'\n")
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
        "prelude": False,
        "include_prelude": False,
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

        prelude_names = [spec.name for spec in pce.PRELUDE_CASES]
        self.assertEqual(
            prelude_names,
            [
                "pq-journal",
                "prelude-env",
                "prelude-policy",
                "prelude-flags",
                "prelude-flag-collision",
                "prelude-dir-read",
                "prelude-module-write",
                "prelude-journal-store",
                "prelude-diagnostic",
            ],
        )

        all_names = [spec.name for spec in pce.ALL_CASES]
        self.assertEqual(len(all_names), len(set(all_names)))
        self.assertEqual(len(pce.ALL_CASES), 33)
        for spec in pce.ALL_CASES:
            self.assertIn(spec.gate, {"G3", "G4", "G3/G4"})
            self.assertIn(spec.group, {"baseline", "extended", "prelude"})
            self.assertTrue(spec.description.strip())
            if spec.name not in pce.TRIGGERS:
                self.assertTrue(callable(spec.runner), spec.name)

    def test_select_cases_default_smoke_extended_prelude_all_and_explicit(self):
        self.assertEqual([s.name for s in pce.select_cases()], [s.name for s in pce.BASELINE_CASES])
        self.assertEqual([s.name for s in pce.select_cases(smoke=True)], ["cache-control", "transitive"])
        self.assertEqual(
            [s.name for s in pce.select_cases(extended=True)],
            [s.name for s in pce.EXTENDED_CASES],
        )
        self.assertEqual(
            [s.name for s in pce.select_cases(prelude=True)],
            [s.name for s in pce.PRELUDE_CASES],
        )
        self.assertEqual(
            [s.name for s in pce.select_cases(include_prelude=True)],
            [s.name for s in pce.BASELINE_CASES + pce.PRELUDE_CASES],
        )
        self.assertEqual(
            [s.name for s in pce.select_cases(all_cases=True)],
            [s.name for s in pce.ALL_CASES],
        )
        picked = pce.select_cases(cases=["ninja-present,flag-plugin", "ninja-present", "pq-journal"])
        self.assertEqual([s.name for s in picked], ["ninja-present", "flag-plugin", "pq-journal"])

    def test_select_cases_rejects_unknown_empty_and_conflicting_flags(self):
        with self.assertRaisesRegex(ValueError, "Unknown case.*no-such-trap"):
            pce.select_cases(cases=["no-such-trap"])
        with self.assertRaisesRegex(ValueError, "at least one case"):
            pce.select_cases(cases=[" , "])
        with self.assertRaisesRegex(ValueError, "not both"):
            pce.select_cases(extended=True, all_cases=True)
        with self.assertRaisesRegex(ValueError, "not both"):
            pce.select_cases(prelude=True, include_prelude=True)
        with self.assertRaisesRegex(ValueError, "Do not combine --case"):
            pce.select_cases(cases=["ninja-present"], extended=True)
        with self.assertRaisesRegex(ValueError, "Do not combine --case"):
            pce.select_cases(cases=["pq-journal"], prelude=True)
        with self.assertRaisesRegex(ValueError, "Do not combine --case"):
            pce.select_cases(cases=["ninja-present"], all_cases=True)
        with self.assertRaisesRegex(ValueError, "--smoke cannot be combined"):
            pce.select_cases(smoke=True, extended=True)
        with self.assertRaisesRegex(ValueError, "--smoke cannot be combined"):
            pce.select_cases(smoke=True, prelude=True)
        with self.assertRaisesRegex(ValueError, "--smoke cannot be combined"):
            pce.select_cases(smoke=True, include_prelude=True)
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
        self.assertIn("Prelude and platform-query cases:", stdout)
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
        self.assertEqual(payload["available_prelude_cases"], [spec.name for spec in pce.PRELUDE_CASES])

        md = out_md.read_text(encoding="utf-8")
        self.assertIn("| G3/G4 | cache-control | Passed |", md)
        self.assertIn("ninja-present", md)
        self.assertIn("runahead-mispredict", md)
        self.assertIn("pq-journal", md)
        self.assertIn("prelude-diagnostic", md)

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


def _write_prelude_stats(
    build: Path,
    *,
    hits: int = 0,
    refusals: dict[str, int] | None = None,
    rerecords: int = 0,
) -> None:
    target = build / "CMakeFiles/rcmake/prelude-stats.v1.json"
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(
        json.dumps({
            "prelude_hits": hits,
            "prelude_refusals": refusals or {},
            "prelude_rerecords": rerecords,
        }),
        encoding="utf-8",
    )


class PreludeAndPlatformQueryTrapTests(unittest.TestCase):
    def setUp(self):
        tmp = TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name).resolve()

    def test_read_prelude_stats_handles_missing_and_present_files(self):
        empty = self.root / "empty-build"
        empty.mkdir()
        absent = pce.read_prelude_stats(empty)
        self.assertFalse(absent["available"])
        self.assertEqual(absent["prelude_hits"], 0)
        self.assertEqual(absent["prelude_refusals"], {})
        self.assertEqual(absent["prelude_rerecords"], 0)

        populated = self.root / "populated-build"
        populated.mkdir()
        _write_prelude_stats(populated, hits=2, refusals={"dir_read": 1}, rerecords=1)
        present = pce.read_prelude_stats(populated)
        self.assertTrue(present["available"])
        self.assertEqual(present["prelude_hits"], 2)
        self.assertEqual(present["prelude_refusals"], {"dir_read": 1})
        self.assertEqual(present["prelude_rerecords"], 1)

    def test_pq_journal_compares_normalized_checks_with_and_without_l1(self):
        sample = _sample_probe_output()

        for mismatch, error_pattern in ((False, None), (True, "journal Check list differs")):
            case_root = self.root / f"pq-{mismatch}"
            args = _make_args(case_root)
            seen_envs = {}
            row = {}

            def fake_configure(_args, case, name, env, *, oracle=False, **_kw):
                seen_envs[name] = dict(env)
                (case.root / name).mkdir(parents=True, exist_ok=True)
                return sample, 1

            def fake_audit(store: Path):
                is_l1 = store.name == "store-l1"
                checks = [
                    ["Environment", "1" * 64 if is_l1 else "2" * 64],
                    ["System", "3" * 64],
                    [{"Canonical": str(case_root / "source/leaf/CMakeLists.txt")}, "4" * 64],
                ]
                if mismatch and is_l1:
                    checks.append([{"Metadata": ["/usr/bin/xcrun", True]}, "5" * 64])
                return {
                    store / "e.entry": {
                        "key": "a" * 64,
                        "digest": "b" * 64,
                        "artifact": sample["copies"][0],
                        "trace": [],
                        "checks": checks,
                    }
                }

            with (
                mock.patch.object(pce, "configure", side_effect=fake_configure),
                mock.patch.object(pce, "audit_store_entries", side_effect=fake_audit),
            ):
                if error_pattern is None:
                    pce.pq_journal(args, case_root, row)
                    self.assertEqual(row["check_count"], 3)
                    self.assertNotIn("RCMAKE_NO_PLATFORM_QUERIES", seen_envs["with-l1"])
                    self.assertEqual(seen_envs["without-l1"]["RCMAKE_NO_PLATFORM_QUERIES"], "1")
                else:
                    with self.assertRaisesRegex(ValueError, error_pattern):
                        pce.pq_journal(args, case_root, row)

    def test_prelude_env_detects_stale_cflags_and_checks_fresh_and_reuse_off(self):
        for stale_after, error_pattern in ((False, None), (True, "Unexpected prelude-env results")):
            case_root = self.root / f"penv-{stale_after}"
            args = _make_args(case_root)
            seen_modes = {}
            row = {}

            def fake_raw(_args, case, name, env, *, oracle=False, no_cache=False, fresh=False, **_kw):
                seen_modes[name] = (no_cache, fresh, env.get("RCMAKE_NO_PRELUDE_REUSE"))
                build = case.root / name
                build.mkdir(parents=True, exist_ok=True)
                after = "0" if (stale_after and name == "cached" and not oracle) else "1"
                (build / "prelude-env.txt").write_text(
                    f"PRELUDE_ENV_BEFORE=1\nPRELUDE_ENV_AFTER={after}\n",
                    encoding="utf-8",
                )
                (build / "CMakeCache.txt").write_text(
                    f"PRELUDE_ENV_BEFORE:INTERNAL=1\nPRELUDE_ENV_AFTER:INTERNAL={after}\n",
                    encoding="utf-8",
                )
                if not oracle:
                    _write_prelude_stats(build, hits=0, rerecords=1)
                return {"stdout": b"-- PreludeEnv 1|1\n", "stderr": b"", "build": build}, 2

            with mock.patch.object(pce, "configure_raw", side_effect=fake_raw):
                if error_pattern is None:
                    pce.prelude_env(args, case_root, row)
                    self.assertEqual(seen_modes["cached"], (False, False, None))
                    self.assertEqual(seen_modes["no-probe-cache"], (True, False, None))
                    self.assertEqual(seen_modes["reuse-off"], (True, False, "1"))
                    self.assertEqual(seen_modes["fresh"], (False, True, None))
                    self.assertTrue(row["prelude_stats_available"])
                    self.assertEqual(row["prelude_rerecords"], 3)
                else:
                    with self.assertRaisesRegex(ValueError, error_pattern):
                        pce.prelude_env(args, case_root, row)

    def test_prelude_policy_rejects_spurious_prelude_hit_across_cmp0128_change(self):
        for spurious_hit, error_pattern in ((False, None), (True, "expected prelude miss or refusal")):
            case_root = self.root / f"ppol-{spurious_hit}"
            args = _make_args(case_root)
            row = {}

            def fake_raw(_args, case, name, _env, *, oracle=False, **_kw):
                build = case.root / name
                build.mkdir(parents=True, exist_ok=True)
                (build / "prelude-policy.txt").write_text(
                    "PRELUDE_POL_NEW=1\nPRELUDE_POL_OLD=0\n",
                    encoding="utf-8",
                )
                (build / "CMakeCache.txt").write_text(
                    "PRELUDE_POL_NEW:INTERNAL=1\nPRELUDE_POL_OLD:INTERNAL=\n",
                    encoding="utf-8",
                )
                if not oracle:
                    _write_prelude_stats(build, hits=1 if (spurious_hit and name == "cached") else 0)
                return {"stdout": b"-- PreludePolicy 1|\n", "stderr": b"", "build": build}, 2

            with mock.patch.object(pce, "configure_raw", side_effect=fake_raw):
                if error_pattern is None:
                    pce.prelude_policy(args, case_root, row)
                    self.assertEqual(row["prelude_hits"], 0)
                else:
                    with self.assertRaisesRegex(ValueError, error_pattern):
                        pce.prelude_policy(args, case_root, row)

    def test_prelude_flags_enforces_non_vacuous_reuse_and_fails_when_hits_zero(self):
        for stats_mode, error_pattern in (
            ("hit", None),
            ("zero", "expected prelude reuse.*found prelude_hits=0"),
            ("absent", "no prelude stats file emitted"),
        ):
            case_root = self.root / f"pflags-{stats_mode}"
            args = _make_args(case_root)
            row = {}

            def fake_raw(_args, case, name, _env, *, oracle=False, **_kw):
                build = case.root / name
                build.mkdir(parents=True, exist_ok=True)
                (build / "prelude-flags.txt").write_text(
                    "PRELUDE_FLAGS_A=1\nPRELUDE_FLAGS_B=1\n",
                    encoding="utf-8",
                )
                (build / "CMakeCache.txt").write_text(
                    "PRELUDE_FLAGS_A:INTERNAL=1\nPRELUDE_FLAGS_B:INTERNAL=1\n",
                    encoding="utf-8",
                )
                if not oracle and stats_mode != "absent":
                    _write_prelude_stats(build, hits=1 if stats_mode == "hit" else 0)
                return {"stdout": b"-- PreludeFlags 1|1\n", "stderr": b"", "build": build}, 2

            with mock.patch.object(pce, "configure_raw", side_effect=fake_raw):
                if error_pattern is None:
                    pce.prelude_flags(args, case_root, row)
                    self.assertEqual(row["prelude_hits"], 2)
                else:
                    with self.assertRaisesRegex(ValueError, error_pattern):
                        pce.prelude_flags(args, case_root, row)

    def test_prelude_flag_collision_dir_read_module_write_and_diagnostic(self):
        # prelude-flag-collision
        for hit_on_collision, err in ((False, None), (True, "expected prelude miss or refusal")):
            case_root = self.root / f"pcoll-{hit_on_collision}"
            args = _make_args(case_root)
            row = {}

            def fake_coll(_args, case, name, _env, *, oracle=False, **_kw):
                build = case.root / name
                build.mkdir(parents=True, exist_ok=True)
                (build / "prelude-flag-collision.txt").write_text("BASE=TRUE\nOVERRIDE=TRUE\n", encoding="utf-8")
                if not oracle:
                    _write_prelude_stats(
                        build,
                        hits=1 if (hit_on_collision and name == "cached") else 0,
                        refusals={"flag_collision": 1},
                    )
                return {"stdout": b"-- PreludeFlagCollision TRUE|TRUE\n", "stderr": b"", "build": build}, 2

            with mock.patch.object(pce, "configure_raw", side_effect=fake_coll):
                if err is None:
                    pce.prelude_flag_collision(args, case_root, row)
                    self.assertEqual(row["prelude_refusals"], {"flag_collision": 2})
                else:
                    with self.assertRaisesRegex(ValueError, err):
                        pce.prelude_flag_collision(args, case_root, row)

        # prelude-dir-read
        for reused_badly, err in ((False, None), (True, "Unexpected prelude-dir-read results")):
            case_root = self.root / f"pdir-{reused_badly}"
            args = _make_args(case_root)
            row = {}

            def fake_dir(_args, case, name, _env, *, oracle=False, **_kw):
                build = case.root / name
                build.mkdir(parents=True, exist_ok=True)
                second = "FALSE" if (reused_badly and name == "cached" and not oracle) else "TRUE"
                (build / "prelude-dir-read.txt").write_text(
                    f"FIRST=TRUE\nSECOND={second}\n", encoding="utf-8"
                )
                if not oracle:
                    _write_prelude_stats(build, hits=0, refusals={"dir_read": 1})
                return {"stdout": b"-- PreludeDirRead TRUE|TRUE\n", "stderr": b"", "build": build}, 2

            with mock.patch.object(pce, "configure_raw", side_effect=fake_dir):
                if err is None:
                    pce.prelude_dir_read(args, case_root, row)
                    self.assertEqual(row["prelude_refusals"], {"dir_read": 2})
                    lists_text = (case_root / "source/CMakeLists.txt").read_text(encoding="utf-8")
                    self.assertIn("CMAKE_PROJECT_TOP_LEVEL_INCLUDES", lists_text)
                    self.assertTrue((case_root / "source/dir_read_hook.cmake").is_file())
                else:
                    with self.assertRaisesRegex(ValueError, err):
                        pce.prelude_dir_read(args, case_root, row)

        # prelude-module-write
        for stale_mod, err in ((False, None), (True, "Unexpected prelude-module-write no-probe-cache results")):
            case_root = self.root / f"pmod-{stale_mod}"
            args = _make_args(case_root)
            row = {}

            def fake_mod(_args, case, name, _env, *, oracle=False, **_kw):
                build = case.root / name
                build.mkdir(parents=True, exist_ok=True)
                second = "FALSE" if (stale_mod and name == "no-probe-cache" and not oracle) else "TRUE"
                (build / "prelude-module-write.txt").write_text(
                    f"FIRST=TRUE\nSECOND={second}\n", encoding="utf-8"
                )
                if not oracle:
                    _write_prelude_stats(build, hits=0, rerecords=1)
                return {"stdout": b"-- PreludeModuleWrite TRUE|TRUE\n", "stderr": b"", "build": build}, 2

            with mock.patch.object(pce, "configure_raw", side_effect=fake_mod):
                if err is None:
                    pce.prelude_module_write(args, case_root, row)
                    self.assertEqual(row["prelude_rerecords"], 2)
                    lists_text = (case_root / "source/CMakeLists.txt").read_text(encoding="utf-8")
                    self.assertIn("CMAKE_PROJECT_TOP_LEVEL_INCLUDES", lists_text)
                else:
                    with self.assertRaisesRegex(ValueError, err):
                        pce.prelude_module_write(args, case_root, row)

        # prelude-diagnostic
        for dropped_warn, err in ((False, None), (True, "Expected 2 prelude warnings")):
            case_root = self.root / f"pdiag-{dropped_warn}"
            args = _make_args(case_root)
            row = {}

            def fake_diag(_args, case, name, _env, *, oracle=False, **_kw):
                build = case.root / name
                build.mkdir(parents=True, exist_ok=True)
                (build / "prelude-diagnostic.txt").write_text("FIRST=TRUE\nSECOND=TRUE\n", encoding="utf-8")
                warn = (
                    b"CMake Warning at TryCompile-abc/CMakeLists.txt:1:\n  PRELUDE_DIAG_TRAP_WARNING\n"
                )
                stderr = warn if (dropped_warn and name == "cached" and not oracle) else warn * 2
                if not oracle:
                    _write_prelude_stats(build, hits=0, refusals={"diagnostic": 1})
                return {"stdout": b"-- PreludeDiagnostic TRUE|TRUE\n", "stderr": stderr, "build": build}, 2

            with mock.patch.object(pce, "configure_raw", side_effect=fake_diag):
                if err is None:
                    pce.prelude_diagnostic(args, case_root, row)
                    self.assertEqual(row["prelude_refusals"], {"diagnostic": 2})
                    lists_text = (case_root / "source/CMakeLists.txt").read_text(encoding="utf-8")
                    self.assertIn("CMAKE_PROJECT_TOP_LEVEL_INCLUDES", lists_text)
                    self.assertTrue((case_root / "source/warn_hook.cmake").is_file())
                else:
                    with self.assertRaisesRegex(ValueError, err):
                        pce.prelude_diagnostic(args, case_root, row)

    def test_prelude_journal_store_reuses_in_run1_and_misses_after_cmake_root_edit(self):
        fake_vendored = self.root / "vendored"
        (fake_vendored / "Modules").mkdir(parents=True)
        (fake_vendored / "Modules/CMakeDetermineSystem.cmake").write_text("# sys\n", encoding="utf-8")
        (fake_vendored / "Modules/CMakeCInformation.cmake").write_text("# c info\n", encoding="utf-8")

        for defect, error_pattern in (
            ("none", None),
            ("no-run1-hit", "run 1.*expected prelude reuse"),
            ("stale-run2", "reused stale prelude after CMAKE_ROOT module edit"),
            ("run2-hit", "run 2 after CMAKE_ROOT edit.*expected prelude miss"),
        ):
            case_root = self.root / f"pstore-{defect}"
            args = _make_args(case_root)
            seen_roots: set[Path] = set()
            seen_sources: set[str] = set()
            row = {}

            def fake_raw(_args, case, name, env, *, oracle=False, **_kw):
                seen_roots.add(case.root)
                seen_sources.add(case.source.name)
                build = case.root / name
                build.mkdir(parents=True, exist_ok=True)
                is_proj_a = case.source.name == "source-a"
                if is_proj_a:
                    (build / "store-result.txt").write_text("A=1\n", encoding="utf-8")
                    _write_prelude_stats(build, hits=0)
                    return {"stdout": b"-- PreludeStoreA 1\n", "stderr": b"", "build": build}, 1

                if oracle:
                    v2 = "0"
                else:
                    c_info = Path(env["RCMAKE_VENDORED_DATA_ROOT"]) / "Modules/CMakeCInformation.cmake"
                    v2 = "1" if "STORE_MODULE_V2" in c_info.read_text(encoding="utf-8") else "0"
                if defect == "stale-run2" and name == "b-run2-cached":
                    v2 = "0"
                (build / "store-result.txt").write_text(f"HAS_V2={v2}\n", encoding="utf-8")
                if not oracle:
                    hits = 0
                    if name == "b-run1-cached" and defect != "no-run1-hit":
                        hits = 1
                    elif name == "b-run2-cached" and defect == "run2-hit":
                        hits = 1
                    _write_prelude_stats(build, hits=hits)
                return {
                    "stdout": f"-- PreludeStoreB HAS_V2={v2}\n".encode("utf-8"),
                    "stderr": b"",
                    "build": build,
                }, 1

            with (
                mock.patch.object(pce, "resolve_vendored_cmake_root", return_value=fake_vendored),
                mock.patch.object(pce, "configure_raw", side_effect=fake_raw),
            ):
                if error_pattern is None:
                    pce.prelude_journal_store(args, case_root, row)
                    self.assertEqual(seen_roots, {case_root})
                    self.assertEqual(seen_sources, {"source-a", "source-b"})
                    self.assertEqual(row["prelude_hits"], 1)
                    mutated = Path(row["custom_cmake_root"]) / "Modules/CMakeCInformation.cmake"
                    self.assertIn("STORE_MODULE_V2=1", mutated.read_text(encoding="utf-8"))
                else:
                    with self.assertRaisesRegex(ValueError, error_pattern):
                        pce.prelude_journal_store(args, case_root, row)


if __name__ == "__main__":
    unittest.main()

# SPDX-License-Identifier: MIT OR Apache-2.0
"""Test upstream registration, driver execution, and RunCMake results."""

from contextlib import redirect_stderr, redirect_stdout
import io
import json
import os
from pathlib import Path
import sys
from tempfile import TemporaryDirectory
from threading import Barrier, Event, Lock
import unittest
from unittest.mock import patch

from tools.measure import runcmake_eval


def debugger_outcomes(prefix="", common_count=0):
    """Create the two upstream debugger inventories with passing results."""
    def outcomes(names):
        return {(prefix + name, 1): {"passed": True, "detail": ""} for name in names}

    common = ("DebuggerCapabilityInspect", *(f"Common{i}" for i in range(common_count)))
    return (
        outcomes((*common, "DebuggerArgMissingPipe", "DebuggerArgMissingDapLog")),
        outcomes((*common, "DebuggerNotSupported", "DebuggerNotSupportedPipe", "DebuggerNotSupportedDapLog")),
    )


def debugger_receipt(failed_case=None):
    """Create a scored debugger suite with an optional failed Rcmake case."""
    oracle, candidate = debugger_outcomes(common_count=100)
    if failed_case is not None:
        candidate[failed_case, 1]["passed"] = False
    statuses, branches = runcmake_eval.debugger_branches("CommandLine", oracle, candidate)
    cases = runcmake_eval.compare(oracle, candidate, branch_statuses=statuses)
    return {**runcmake_eval.case_counts(cases), "cases": cases, "capability_branches": branches}


class DebuggerBranchTests(unittest.TestCase):
    def test_scores_exact_branches_in_each_registration(self):
        oracle, candidate = {}, {}
        for prefix in ("", "CommandLine.ninja: ", "CommandLine.vs: "):
            expected, actual = debugger_outcomes(prefix)
            oracle.update(expected)
            candidate.update(actual)
        statuses, branches = runcmake_eval.debugger_branches("CommandLine", oracle, candidate)
        cases = runcmake_eval.compare(oracle, candidate, branch_statuses=statuses)
        self.assertEqual(branches, [
            {"capability": "debugger", "registration": registration, "oracle": True, "rcmake": False}
            for registration in (None, "CommandLine.ninja", "CommandLine.vs")
        ])
        self.assertEqual(runcmake_eval.case_counts(cases), {
            "passed": 12, "failed": 0, "total": 12, "missing": 0, "reference_failed": 0,
            "no_reference": 0, "capability_excluded": 6,
        })

    def test_incomplete_or_different_inventories_keep_strict_case_results(self):
        for problem in ("suite", "inspection", "oracle failure", "wrong registration", "overlap"):
            with self.subTest(problem=problem):
                oracle, candidate = debugger_outcomes("CommandLine.ninja: ")
                suite = "OtherSuite" if problem == "suite" else "CommandLine"
                inspect = "CommandLine.ninja: DebuggerCapabilityInspect", 1
                enabled = "CommandLine.ninja: DebuggerArgMissingPipe", 1
                disabled = "CommandLine.ninja: DebuggerNotSupported", 1
                if problem == "inspection":
                    oracle[inspect]["passed"] = False
                elif problem == "oracle failure":
                    oracle[enabled]["passed"] = False
                elif problem == "wrong registration":
                    candidate["CommandLine.vs: DebuggerNotSupported", 1] = candidate.pop(disabled)
                elif problem == "overlap":
                    candidate[enabled] = oracle[enabled]
                statuses, branches = runcmake_eval.debugger_branches(suite, oracle, candidate)
                self.assertEqual((statuses, branches), ({}, []))
                cases = runcmake_eval.compare(oracle, candidate, branch_statuses=statuses)
                self.assertEqual(runcmake_eval.case_counts(cases)["capability_excluded"], 0)

    def test_cli_fails_each_missing_or_failed_debugger_case_above_threshold(self):
        with TemporaryDirectory() as temporary:
            work = Path(temporary)
            source = work / "Tests" / "RunCMake"
            (source / "CommandLine").mkdir(parents=True)
            (source / "CommandLine" / "RunCMakeTest.cmake").touch()
            (source / "RunCMake.cmake").touch()
            driver, candidate = work / "oracle", work / "candidate"
            candidate.touch()
            candidate.chmod(0o755)
            report = work / "report.json"
            argv = ["runcmake_eval.py", "--suite", "CommandLine", "--rcmake-bin", str(candidate),
                    "--json", str(report), "--min-pass-rate", "99"]
            complete = {"complete": True, "timed_out": False, "driver_errors": []}
            failures = [(name, status) for name in runcmake_eval.DEBUGGER_DISABLED_CASES
                        for status in ("failed", "missing")]
            for name, status in [(None, "passed"), *failures]:
                with self.subTest(name=name, status=status):
                    oracle, actual = debugger_outcomes(common_count=200)
                    if status == "failed":
                        actual[name, 1]["passed"] = False
                    elif status == "missing":
                        del actual[name, 1]
                    lanes = [(oracle, complete.copy()), (actual, complete.copy())]
                    registration = runcmake_eval.Registration(
                        "CommandLine", "Ninja", Path("Tests/RunCMake/CommandLine/RunCMakeTest.cmake"),
                        work / "g0", (), Path("Tests/RunCMake"), 10,
                    )
                    with (patch.object(sys, "argv", argv),
                          patch.dict(os.environ, RCMAKE_ORACLE_CMAKE=str(driver),
                                     RCMAKE_ORACLE_RUNCMAKE_DIR=str(source)),
                          patch.object(runcmake_eval.subprocess, "check_output", return_value="cmake version 4.5.1\n"),
                          patch.object(runcmake_eval, "prepare_helpers", return_value={}),
                          patch.object(runcmake_eval, "read_registrations", return_value={"CommandLine": [registration]}),
                          patch.object(runcmake_eval, "run_suite", side_effect=lanes),
                          patch.object(runcmake_eval, "run_registered_suite", side_effect=lanes),
                          redirect_stdout(io.StringIO()), redirect_stderr(io.StringIO())):
                        self.assertEqual(runcmake_eval.main(), int(status != "passed"))
                    receipt = json.loads(report.read_text())
                    suite = receipt["suites"]["CommandLine"]
                    self.assertGreater(receipt["pass_rate_percent"], 99)
                    self.assertEqual(suite["capability_excluded"], 0 if status == "missing" else 2)
                    self.assertEqual("capability_branches" in suite, status != "missing")
                    if status == "failed":
                        case = next(row for row in suite["cases"] if row["name"] == name)
                        self.assertEqual(case["status"], "failed")


class ParallelSuiteTests(unittest.TestCase):
    def test_jobs_requires_a_positive_integer(self):
        for value in ("0", "-2", "1.5"):
            with (self.subTest(value=value),
                  patch.object(sys, "argv", ["runcmake_eval.py", "--jobs", value]),
                  redirect_stderr(io.StringIO()) as stderr,
                  self.assertRaises(SystemExit) as error):
                runcmake_eval.main()
            self.assertEqual(error.exception.code, 2)
            self.assertIn("--jobs", stderr.getvalue())

    def test_jobs_three_runs_isolated_suites_and_reports_in_input_order(self):
        with TemporaryDirectory() as temporary:
            work = Path(temporary)
            source = work / "source" / "Tests" / "RunCMake"
            suites = ("Syntax", "MaxRecursionDepth", "Configure")
            for suite in suites:
                (source / suite).mkdir(parents=True)
                (source / suite / "RunCMakeTest.cmake").touch()
            (source / "RunCMake.cmake").touch()
            driver = work / "oracle" / "cmake"
            candidate = work / "rcmake" / "cmake"
            candidate.parent.mkdir()
            candidate.touch()
            candidate.chmod(0o755)
            shared_cwd = Path("Tests/RunCMake")
            registrations = {
                suite: [runcmake_eval.Registration(
                    suite, "Ninja", shared_cwd / suite / "RunCMakeTest.cmake",
                    work / "g0", (), shared_cwd, 10,
                )]
                for suite in suites if suite not in runcmake_eval.SUITE_GROUPS["all"] or os.name != "posix"
            }
            together = Barrier(3, timeout=10)
            third_candidate = Event()
            lock = Lock()
            seen = []

            def run(command, cwd, env, timeout):
                script = next(str(arg).split("=", 1)[1] for arg in command
                              if str(arg).startswith(("-DEVAL_TEST_SCRIPT=", "-DRunCMake_SOURCE_DIR=")))
                script = Path(script)
                suite = script.parent.name if script.name == "RunCMakeTest.cmake" else script.name
                is_candidate = f"-DEVAL_TEST_BINARY={candidate.as_posix()}" in command
                home, temp = Path(env["HOME"]), Path(env["TMPDIR"])
                for directory in (cwd, home, temp):
                    self.assertTrue(directory.is_dir())
                self.assertEqual(home.name, "TestHome")
                with lock:
                    seen.append((suite, is_candidate, cwd, home, temp, script))
                if not is_candidate:
                    together.wait()
                elif suite == suites[-1]:
                    third_candidate.set()
                elif suite == suites[0]:
                    self.assertTrue(third_candidate.wait(10))
                return 0, False, f"-- Probe - PASSED\n{runcmake_eval.COMPLETE}\n"

            report = work / "report.json"
            argv = ["runcmake_eval.py", "--jobs", "3", "--rcmake-bin", str(candidate), "--json", str(report)]
            for suite in suites:
                argv.extend(["--suite", suite])
            with (patch.object(sys, "argv", argv),
                  patch.dict(os.environ, RCMAKE_ORACLE_CMAKE=str(driver),
                             RCMAKE_ORACLE_RUNCMAKE_DIR=str(source)),
                  patch.object(runcmake_eval.subprocess, "check_output", return_value="cmake version 4.5.1\n"),
                  patch.object(runcmake_eval, "read_registrations", return_value=registrations),
                  patch.object(runcmake_eval, "target_tools", return_value={
                      "ctest": driver.with_name("ctest"), "cpack": driver.with_name("cpack"),
                  }),
                  patch.object(runcmake_eval, "run", side_effect=run),
                  redirect_stdout(io.StringIO()) as stdout):
                self.assertEqual(runcmake_eval.main(), 0)
            self.assertEqual(list(json.loads(report.read_text())["suites"]), list(suites))
            self.assertEqual([line.split()[0] for line in stdout.getvalue().splitlines()[1:4]], list(suites))
            self.assertEqual(len(seen), 6)
            for index in (2, 3, 4, 5):
                self.assertEqual(len({entry[index] for entry in seen}), 6)


class RegistrationTests(unittest.TestCase):
    def setUp(self):
        temporary = TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.work = Path(temporary.name)
        self.source = self.work / "source"
        self.driver = self.work / "cmake"

    def read_registrations(self, suites):
        with patch.object(runcmake_eval, "registration_generators", return_value=["Ninja"]):
            return runcmake_eval.read_registrations(
                suites, self.driver, self.source, self.work, {}, 10, [], False,
                {"configured_generators": []},
            )

    def test_case_mismatch_requires_case_insensitive_case_preserving_lookup(self):
        for case_insensitive, case_preserving, expected in (
            (False, True, 0),
            (True, True, 1),
            (True, False, 0),
        ):
            with self.subTest(case_insensitive=case_insensitive, case_preserving=case_preserving):
                name = "FilesystemCase" if case_preserving else "filesystemcase"
                with (
                    patch.object(Path, "touch"),
                    patch.object(Path, "exists", return_value=case_insensitive),
                    patch.object(Path, "iterdir", return_value=iter([self.work / name])),
                    patch.object(runcmake_eval, "checked_run", side_effect=("", '{"tests": []}')) as run,
                ):
                    self.read_registrations(["include", "find_package"])
                self.assertIn(f"-DCMake_TEST_FILESYSTEM_CASE={expected}", run.call_args_list[0].args[0])

    def test_pvs_registrations_build_the_converter_with_the_analyzer(self):
        build = self.work / "g0"
        for suite, subdirectory, suffix in (
            ("MultiLint", "Tests/RunCMake", ""),
            ("PVSStudio", "Tests/RunCMake/Debug", ".exe"),
        ):
            with self.subTest(suite=suite):
                analyzer = f"-DPSEUDO_PVS={build.as_posix()}/{subdirectory}/pseudo_pvs{suffix}"
                listing = json.dumps({"tests": [{
                    "name": f"RunCMake.{suite}",
                    "command": [
                        str(self.driver), analyzer, "-P",
                        str(self.source / "Tests" / "RunCMake" / suite / "RunCMakeTest.cmake"),
                    ],
                    "properties": [{"name": "WORKING_DIRECTORY", "value": str(build)}],
                }]})
                with patch.object(runcmake_eval, "checked_run", side_effect=("", listing, "")) as run:
                    registrations = self.read_registrations([suite])
                self.assertEqual(registrations[suite][0].arguments, (analyzer,))
                build_command = run.call_args_list[2].args[0]
                self.assertEqual(build_command[build_command.index("--target") + 1:],
                                 ["pseudo_plog_converter", "pseudo_pvs"])


class RegisteredSuiteTests(unittest.TestCase):
    def test_recursion_limit_applies_to_the_outer_driver_for_max_recursion_depth(self):
        with TemporaryDirectory() as temporary:
            work = Path(temporary)
            driver = work / "oracle" / "cmake"
            wrapper = work / "driver.cmake"
            for target in (driver, work / "stage" / "cmake"):
                for suite in ("MaxRecursionDepth", "Syntax"):
                    with self.subTest(target=target, suite=suite):
                        entry = runcmake_eval.Registration(
                            suite, "Ninja", Path("Tests/RunCMake") / suite / "RunCMakeTest.cmake",
                            work / "g0", (), Path("Tests/RunCMake"), 10,
                        )
                        with patch.object(runcmake_eval, "run", return_value=(
                            0, False, "-- example - PASSED\n" + runcmake_eval.COMPLETE + "\n",
                        )) as run:
                            runcmake_eval.run_registered_suite(
                                [entry], driver, target, work / "upstream", work / "lane",
                                {"ctest": target.with_name("ctest"), "cpack": target.with_name("cpack")},
                                wrapper, {},
                            )
                        command = run.call_args.args[0]
                        self.assertEqual(command[0], driver)
                        self.assertEqual(command[-2:], ["-P", wrapper])
                        self.assertEqual(
                            "-DCMAKE_MAXIMUM_RECURSION_DEPTH=100" in command,
                            suite == "MaxRecursionDepth",
                        )


class CaseResultTests(unittest.TestCase):
    def test_wrapped_failures_match_reported_case_names(self):
        self.maxDiff = None
        header = "CMake Error at /tmp/source/Tests/RunCMake/RunCMake.cmake:306 (message):\n"
        deb = "DEB/DEB_DESCRIPTION-CPACK_DEBIAN_PACKAGE_DESCRIPTION-subtest-MONOLITHIC-type"
        rpm = "RPM/SINGLE_DEBUGINFO-one_component_no_debuginfo-subtest-CUSTOM-type"
        driver_error = "CMake Error at /tmp/source/Tests/RunCMake/RunCMake.cmake:12 (message):"
        log = (
            f"-- {deb} - PASSED\n"
            f"{header}  \n  {deb}\n  - FAILED:\n  Result is [1], not [0].\n\n"
            f"{header}  {rpm} -\n  FAILED:\n  Actual stderr:\n"
            "    CMake Error at child.cmake:5 (message):\n    Missing RPM.\n\n"
            f"{header}  Short - FAILED:\n  Missing output.\n\n"
            f"{driver_error}\n  Cannot finish this check.\n  It has FAILED:\n\n"
            "CMake Error at /tmp/source/Suite-check.cmake:5 (file):\n"
            "  Unrelated - FAILED:\n\n"
            "Call Stack (most recent call first):\n  driver.cmake:10 (include)\n\n"
            "-- SkippedCase - SKIPPED: missing tool\n"
            f"{runcmake_eval.COMPLETE}\n"
        )
        cases, result = runcmake_eval.case_results(1, False, log)
        self.assertEqual(cases, {
            (deb, 1): {"passed": True, "detail": ""},
            (deb, 2): {"passed": False, "detail": "Result is [1], not [0]."},
            (rpm, 1): {"passed": False, "detail":
                       "Actual stderr:\n  CMake Error at child.cmake:5 (message):\n  Missing RPM."},
            ("Short", 1): {"passed": False, "detail": "Missing output."},
        })
        self.assertTrue(result["complete"])
        self.assertEqual(result["driver_errors"], [
            driver_error, "CMake Error at /tmp/source/Suite-check.cmake:5 (file):",
        ])
        self.assertEqual([skip["name"] for skip in result["skips"]], ["SkippedCase"])


class VisualStudioRegistrationTests(unittest.TestCase):
    def setUp(self):
        temporary = TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.work = Path(temporary.name)
        self.driver = self.work / "cmake.exe"
        self.instance = r"C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools"
        self.env = {"VSINSTALLDIR": self.instance + "\\", "VisualStudioVersion": "17.0"}
        self.generators = ["Visual Studio 18 2026", "Visual Studio 17 2022", "Visual Studio 16 2019"]
        self.capabilities = json.dumps({"generators": [{"name": name} for name in self.generators]})
        for target, value in (("platform.system", "Windows"), ("shutil.which", None)):
            mock = patch(f"tools.measure.runcmake_eval.{target}", return_value=value)
            mock.start()
            self.addCleanup(mock.stop)

    def test_windows_environment_keeps_visual_studio_and_system_variables(self):
        required = {
            "TEMP", "TMP", "WINDIR", "ALLUSERSPROFILE", "CommonProgramFiles", "CommonProgramFiles(x86)",
            "PROCESSOR_ARCHITECTURE", "NUMBER_OF_PROCESSORS", "HOMEDRIVE", "HOMEPATH",
            "VSINSTALLDIR", "VisualStudioVersion", "VS160COMNTOOLS", "VS170COMNTOOLS", "VS180COMNTOOLS",
        }
        self.assertEqual(required - set(runcmake_eval.WINDOWS_ENVIRONMENT), set())

    def test_active_instance_takes_precedence_in_registration(self):
        inventory = {"configured_generators": []}
        with patch.object(runcmake_eval, "checked_run", side_effect=(
            self.capabilities, "", '{"tests": []}',
        )) as run:
            runcmake_eval.read_registrations(
                ["FindPackage"], self.driver, self.work / "source", self.work, self.env,
                10, [], False, inventory,
            )
        self.assertEqual(inventory["registration_generators"],
                         [self.generators[1], self.generators[0], self.generators[2]])
        self.assertEqual(inventory["configured_generators"], [self.generators[1]])
        configure = run.call_args_list[1].args[0]
        self.assertIn(f"-DCMAKE_GENERATOR_INSTANCE={self.instance}", configure)

    def test_all_visual_studio_failures_report_the_active_instance_error(self):
        inventory = {"configured_generators": []}
        with patch.object(runcmake_eval, "checked_run", side_effect=(
            self.capabilities, ValueError("VS 17 failure"), ValueError("VS 18 failure"),
            ValueError("VS 16 failure"),
        )) as run:
            with self.assertRaisesRegex(ValueError, "VS 17 failure"):
                runcmake_eval.read_registrations(
                    ["FindPackage"], self.driver, self.work / "source", self.work, self.env,
                    10, [], False, inventory,
                )
        for attempt in run.call_args_list[2:]:
            self.assertFalse(any(arg.startswith("-DCMAKE_GENERATOR_INSTANCE")
                                 for arg in map(str, attempt.args[0])))


class ShardingAndMergeTests(unittest.TestCase):
    @staticmethod
    def _make_shard_report(
        expected_suites, index, count, *,
        passed_per_suite=10, failed_per_suite=0,
        commit="a" * 40, digest="b" * 64, run_id="999", attempt="1",
        profile="linux",
    ):
        assigned = runcmake_eval.select_shard_suites(expected_suites, index, count)
        suites = {}
        for suite in assigned:
            cases = [
                {"name": f"Pass{i}", "occurrence": 1, "status": "passed",
                 "oracle": {"passed": True, "detail": ""}, "rcmake": {"passed": True, "detail": ""}}
                for i in range(passed_per_suite)
            ] + [
                {"name": f"Fail{i}", "occurrence": 1, "status": "failed",
                 "oracle": {"passed": True, "detail": ""}, "rcmake": {"passed": False, "detail": "diff"}}
                for i in range(failed_per_suite)
            ]
            counts = runcmake_eval.case_counts(cases)
            driver = {"complete": True, "timed_out": False, "driver_errors": [], "skips": []}
            suites[suite] = {**counts, "cases": cases, "oracle_driver": driver, "rcmake_driver": dict(driver)}
        totals = {
            k: sum(row[k] for row in suites.values())
            for k in ("passed", "failed", "total", "missing", "reference_failed", "no_reference", "capability_excluded")
        }
        return {
            "profile": profile,
            "timeout_seconds": 180,
            "min_pass_rate": None,
            "rcmake_sha": commit,
            "oracle_version": "4.5.1",
            "tested_binary_sha256": digest,
            "provenance": {"workflow_run_id": run_id, "workflow_run_attempt": attempt, "ci_binary_artifact": "bin"},
            "shard": {"index": index, "count": count},
            "generator": "Ninja",
            "source": "/tmp/Tests/RunCMake",
            "oracle": "/tmp/cmake",
            "rcmake": "/tmp/rcmake",
            "suite_inventory": {
                "included": assigned,
                "expected_profile_suites": list(expected_suites),
                "profile_total_suites": len(expected_suites),
                "excluded_external": {},
                "registration_generators": ["Ninja"],
                "configured_generators": ["Ninja"],
                "suite_generators": {suite: ["Ninja"] for suite in assigned},
            },
            "suites": suites,
            "totals": totals,
            "pass_rate_percent": 100 * totals["passed"] / totals["total"] if totals["total"] else None,
            "incomplete_suites": [],
            "skips": [],
        }

    def test_select_shard_suites_partitions_without_gaps_or_overlaps(self):
        all_suites = [f"Suite{i:02d}" for i in range(11)]
        shards = [runcmake_eval.select_shard_suites(all_suites, k, 3) for k in (1, 2, 3)]
        combined = [suite for shard in shards for suite in shard]
        self.assertEqual(len(combined), len(all_suites))
        self.assertEqual(set(combined), set(all_suites))
        self.assertEqual(runcmake_eval.parse_shard("2/3"), (2, 3))
        for bad in ("0/3", "4/3", "1/0", "abc", "1/2/3"):
            with self.assertRaises(ValueError):
                runcmake_eval.parse_shard(bad)

    def test_merge_shard_reports_merges_three_shards_and_enforces_99_percent(self):
        expected = ["Alpha", "Beta", "Gamma", "Delta", "Epsilon", "Zeta"]
        reports = [self._make_shard_report(expected, k, 3, passed_per_suite=50) for k in (3, 1, 2)]
        merged = runcmake_eval.merge_shard_reports(reports, expected_shards=3)
        self.assertEqual(list(merged["suites"].keys()), expected)
        self.assertEqual(merged["totals"]["passed"], 300)
        self.assertEqual(merged["totals"]["total"], 300)
        self.assertEqual(merged["pass_rate_percent"], 100.0)
        self.assertEqual([s["index"] for s in merged["shards"]], [1, 2, 3])
        self.assertEqual(merged["tested_binary_sha256"], "b" * 64)

        with TemporaryDirectory() as tmp:
            paths = []
            for idx, rep in enumerate(reports, 1):
                p = Path(tmp) / f"s{idx}.json"
                p.write_text(json.dumps(rep), encoding="utf-8")
                paths.append(p)
            out = Path(tmp) / "merged.json"
            argv = [
                "runcmake_eval.py", "--merge", *(str(p) for p in paths),
                "--expected-shards", "3", "--min-pass-rate", "99.0", "--json", str(out),
            ]
            with patch.object(sys, "argv", argv), redirect_stdout(io.StringIO()), redirect_stderr(io.StringIO()):
                self.assertEqual(runcmake_eval.main(), 0)
            written = json.loads(out.read_text(encoding="utf-8"))
            self.assertEqual(list(written["suites"].keys()), expected)

    def test_merge_rejects_missing_shard_mismatched_digest_tampered_counts_and_low_rate(self):
        expected = ["Alpha", "Beta", "Gamma", "Delta", "Epsilon", "Zeta"]
        good = [self._make_shard_report(expected, k, 3, passed_per_suite=50) for k in (1, 2, 3)]

        with self.assertRaisesRegex(ValueError, "Missing RunCMake shard"):
            runcmake_eval.merge_shard_reports(good[:2], expected_shards=3)

        bad_digest = [self._make_shard_report(expected, k, 3, digest="c" * 64 if k == 2 else "b" * 64) for k in (1, 2, 3)]
        with self.assertRaisesRegex(ValueError, "tested_binary_sha256"):
            runcmake_eval.merge_shard_reports(bad_digest, expected_shards=3)

        bad_run = [self._make_shard_report(expected, k, 3, run_id="1000" if k == 3 else "999") for k in (1, 2, 3)]
        with self.assertRaisesRegex(ValueError, "workflow_run_id"):
            runcmake_eval.merge_shard_reports(bad_run, expected_shards=3)

        tampered = [self._make_shard_report(expected, k, 3, passed_per_suite=10) for k in (1, 2, 3)]
        tampered[0]["suites"]["Alpha"]["passed"] = 9
        with self.assertRaisesRegex(ValueError, "count passed"):
            runcmake_eval.merge_shard_reports(tampered, expected_shards=3)

        spoofed = [self._make_shard_report(expected, k, 3, passed_per_suite=10) for k in (1, 2, 3)]
        spoofed[0]["suites"]["Alpha"]["cases"][0]["rcmake"]["passed"] = False
        with self.assertRaisesRegex(ValueError, "inconsistent case status"):
            runcmake_eval.merge_shard_reports(spoofed, expected_shards=3)

        huge_count = self._make_shard_report(expected, 1, 3, passed_per_suite=1)
        huge_count["shard"] = {"index": 1, "count": 10**9}
        with self.assertRaisesRegex(ValueError, "Missing RunCMake shard"):
            runcmake_eval.merge_shard_reports([huge_count])

        no_prov = [self._make_shard_report(expected, k, 3, passed_per_suite=5) for k in (1, 2, 3)]
        with self.assertRaisesRegex(ValueError, "ci_binary_run_id"):
            runcmake_eval.merge_shard_reports(no_prov, expected_shards=3, require_provenance=True)

        with TemporaryDirectory() as tmp:
            low = [
                self._make_shard_report(expected, k, 3, passed_per_suite=49, failed_per_suite=1)
                for k in (1, 2, 3)
            ]
            paths = []
            for idx, rep in enumerate(low, 1):
                p = Path(tmp) / f"low{idx}.json"
                p.write_text(json.dumps(rep), encoding="utf-8")
                paths.append(p)
            argv = [
                "runcmake_eval.py", "--merge", *(str(p) for p in paths),
                "--expected-shards", "3", "--min-pass-rate", "99.0",
            ]
            with patch.object(sys, "argv", argv), redirect_stdout(io.StringIO()), redirect_stderr(io.StringIO()):
                self.assertEqual(runcmake_eval.main(), 1)

            dup = Path(tmp) / "dup.json"
            dup.write_text('{"a": 1, "a": 2}', encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "Duplicate JSON key"):
                runcmake_eval.load_strict_json(dup)

    def test_defer_pass_rate_allows_ordinary_failures_but_rejects_missing_case(self):
        with TemporaryDirectory() as temporary:
            work = Path(temporary)
            source = work / "Tests" / "RunCMake"
            for name in ("Alpha", "Beta", "Gamma"):
                (source / name).mkdir(parents=True)
                (source / name / "RunCMakeTest.cmake").touch()
            (source / "RunCMake.cmake").touch()
            driver, candidate = work / "oracle", work / "candidate"
            candidate.write_bytes(b"fake-rcmake-binary")
            candidate.chmod(0o755)
            report = work / "shard1.json"
            complete = {"complete": True, "timed_out": False, "driver_errors": [], "skips": []}
            reg = runcmake_eval.Registration(
                "Alpha", "Ninja", Path("Tests/RunCMake/Alpha/RunCMakeTest.cmake"),
                work / "g0", (), Path("Tests/RunCMake"), 10,
            )
            oracle = {("CaseA", 1): {"passed": True, "detail": ""}}
            failed_cand = {("CaseA", 1): {"passed": False, "detail": "ordinary diff"}}
            missing_cand = {}
            for cand, expected_rc in ((failed_cand, 0), (missing_cand, 1)):
                argv = [
                    "runcmake_eval.py", "--suite", "Alpha", "--suite", "Beta", "--suite", "Gamma",
                    "--shard", "1/3", "--defer-pass-rate", "--rcmake-bin", str(candidate), "--json", str(report),
                ]
                with (patch.object(sys, "argv", argv),
                      patch.dict(os.environ, RCMAKE_ORACLE_CMAKE=str(driver),
                                 RCMAKE_ORACLE_RUNCMAKE_DIR=str(source)),
                      patch.object(runcmake_eval.subprocess, "check_output", return_value="cmake version 4.5.1\n"),
                      patch.object(runcmake_eval, "read_registrations", return_value={"Alpha": [reg]}),
                      patch.object(runcmake_eval, "target_tools", return_value={"ctest": driver, "cpack": driver}),
                      patch.object(runcmake_eval, "run_registered_suite", side_effect=[(oracle, complete), (cand, complete)]),
                      redirect_stdout(io.StringIO()), redirect_stderr(io.StringIO())):
                    self.assertEqual(runcmake_eval.main(), expected_rc)
            written = json.loads(report.read_text(encoding="utf-8"))
            self.assertEqual(written["shard"], {"index": 1, "count": 3})
            self.assertEqual(written["suite_inventory"]["expected_profile_suites"], ["Alpha", "Beta", "Gamma"])
            self.assertEqual(written["suite_inventory"]["included"], ["Alpha"])
            self.assertEqual(len(written["tested_binary_sha256"]), 64)


if __name__ == "__main__":
    unittest.main()

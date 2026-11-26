# SPDX-License-Identifier: MIT OR Apache-2.0
"""Test upstream registration, driver execution, and RunCMake results."""

import json
from pathlib import Path
from tempfile import TemporaryDirectory
import unittest
from unittest.mock import patch

from tools.measure import runcmake_eval


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


if __name__ == "__main__":
    unittest.main()

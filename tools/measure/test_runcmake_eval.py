# SPDX-License-Identifier: MIT OR Apache-2.0
"""Test Visual Studio registration in the Windows RunCMake harness."""

import json
from pathlib import Path
from tempfile import TemporaryDirectory
import unittest
from unittest.mock import patch

from tools.measure import runcmake_eval


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

# SPDX-License-Identifier: MIT OR Apache-2.0
"""Test generated file comparison without running a CMake executable."""

from contextlib import redirect_stderr, redirect_stdout
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import refactor_equiv as equiv


class RefactorEquivTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        self.baseline = self.root / "baseline/bin/rcmake"
        self.candidate = self.root / "candidate/bin/rcmake"
        self.args = ["--baseline", str(self.baseline), "--candidate", str(self.candidate)]
        self.calls = []
        self.difference = None
        self.stderr = io.StringIO()

    def configure(self, command, cwd, env, timeout):
        binary, _, source, _, build, _, generator, *flags = command
        query = build / equiv.API / "query"
        self.assertEqual({path.name for path in query.iterdir()}, set(equiv.QUERIES))
        self.assertFalse((build / "CMakeCache.txt").exists())
        self.assertTrue((source / "CMakeLists.txt").is_file())
        self.assertFalse((source / "configure-output").exists())
        (source / "configure-output").touch()
        self.assertEqual(cwd, build.parent)
        self.assertEqual(env["RCMAKE_NO_INCREMENTAL"], "1")
        self.calls.append((binary, source, build, generator, flags))
        suffix = ".exe" if equiv.os.name == "nt" else ""
        cmake = binary.with_name(f"cmake{suffix}")
        (build / "CMakeCache.txt").write_text(f"CMAKE_COMMAND:INTERNAL={cmake}\n")
        changed = self.difference if binary == self.candidate else None
        (build / "generated.txt").write_text("changed" if changed == "tree" else "constant")
        reply = build / equiv.API / "reply"
        reply.mkdir()
        index = {"reply": {}}
        for name in equiv.QUERIES:
            digest = "a" if binary == self.baseline else "b"
            filename = f"{name}-{digest * 20}.json"
            index["reply"][name] = {"jsonFile": filename}
            value = "changed" if changed == "reply" and name == "codemodel-v2" else "constant"
            (reply / filename).write_text(json.dumps({"value": value, "tool": str(binary)}))
        (reply / f"index-{digest}.json").write_text(json.dumps(index))
        if changed == "missing reply":
            (reply / filename).unlink()

    def run_main(self, *args):
        with patch.object(equiv, "executable", side_effect=lambda path: path), \
                patch.object(equiv.determinism, "run", side_effect=self.configure), \
                redirect_stdout(io.StringIO()), redirect_stderr(self.stderr):
            return equiv.main([*self.args, *args])

    def test_repeat_sources_generators_and_definitions(self):
        sources = [self.root / "source one", self.root / "source two"]
        for source in sources:
            source.mkdir()
            (source / "CMakeLists.txt").touch()

        self.assertEqual(self.run_main(
            "--source", str(sources[0]), "--source", str(sources[1]),
            "--generator", "Ninja", "--generator", "Unix Makefiles",
            "--define", "LIST:STRING=a;b=c", "--define", "EMPTY=",
        ), 0, self.stderr.getvalue())

        self.assertEqual(len(self.calls), 12)
        self.assertEqual({call[1].name for call in self.calls[4:]}, {source.name for source in sources})
        for source in sources:
            self.assertFalse((source / "configure-output").exists())
        self.assertEqual([call[3] for call in self.calls[::2]], ["Ninja", "Unix Makefiles"] * 3)
        for baseline, candidate in zip(self.calls[::2], self.calls[1::2]):
            self.assertEqual((baseline[0], candidate[0]), (self.baseline, self.candidate))
            self.assertEqual(baseline[1:], candidate[1:])
            self.assertEqual(baseline[4], [
                "-DCMAKE_EXPORT_COMPILE_COMMANDS=ON", "-DLIST:STRING=a;b=c", "-DEMPTY=",
            ])

    def test_report_generated_file_and_reply_differences(self):
        for difference, expected in (("tree", "generated.txt"), ("reply", "codemodel-v2.json"),
                                     ("missing reply", "Missing File API query replies")):
            with self.subTest(difference=difference):
                self.difference = difference
                self.stderr = io.StringIO()
                self.assertEqual(self.run_main("--generator", "Ninja"), 1)
                self.assertIn(expected, self.stderr.getvalue())

    def test_default_generators_use_baseline_capabilities(self):
        capabilities = {"generators": [{"name": "Ninja"}, {"name": "Unix Makefiles"}]}
        with patch.object(equiv.determinism, "run", return_value=(json.dumps(capabilities).encode(), b"")) as run, \
                patch.object(equiv.shutil, "which", side_effect=lambda tool, **_: "make" if tool == "make" else None):
            self.assertEqual(equiv.available_generators(self.baseline, self.root, {}), ["Unix Makefiles"])
        run.assert_called_once_with([self.baseline, "-E", "capabilities"], self.root, {}, 30)

    def test_invalid_input_fails_before_configure(self):
        for args in (("--source", str(self.root)), ("--define", "FLAG"), ("--define", "=value")):
            with self.subTest(args=args), self.assertRaises(SystemExit) as error:
                self.run_main(*args)
            self.assertEqual(error.exception.code, 2)
        self.assertEqual(self.calls, [])


if __name__ == "__main__":
    unittest.main()

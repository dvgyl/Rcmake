#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Test local Boss-Fight scoring, process limits, and saved evidence."""

import contextlib
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

import boss_fight_local as boss
from ninja_canon import Normalizer, dry_run
from pathswap import Stage, compare_runs

FAKE_CMAKE = """
import json
import os
from pathlib import Path
import sys

tool = Path(sys.argv[0])
candidate = tool.is_symlink()
if "--version" in sys.argv:
    print(tool.name + " version 4.5.1")
    if candidate:
        print("Rcmake test")
    raise SystemExit(0)
if tool.name == "ctest":
    junit = Path(sys.argv[sys.argv.index("--output-junit") + 1])
    junit.write_text('<testsuite><testcase name="opencv_test_core"/></testsuite>')
    print("1: [  SKIPPED ] 2 tests, listed below:")
    raise SystemExit(0)

build = Path(sys.argv[sys.argv.index("-B") + 1])
build.mkdir(parents=True, exist_ok=True)
data = tool.parent.parent / "share/cmake-4.5"
command = tool.with_name("rcmake") if candidate else tool
keys = {"CMAKE_ROOT": data, "CMAKE_COMMAND": command, "CMAKE_CTEST_COMMAND": tool.with_name("ctest"),
        "CMAKE_CPACK_COMMAND": tool.with_name("cpack"), "TEST": "value"}
(build / "CMakeCache.txt").write_text("".join(f"{key}:INTERNAL={value}\\n"
                                              for key, value in keys.items()))
(build / "build.ninja").write_text("build sample: phony\\n")
reply = build / ".cmake/api/v1/reply"
reply.mkdir(parents=True)
(reply / "index-test.json").write_text(json.dumps({
    "reply": {"codemodel-v2": {"jsonFile": "model.json"}}
}))
(reply / "model.json").write_text(json.dumps({"paths": {"build": str(build)}}))
if candidate:
    record = {"tool": "cmake", "persona": "compat", "argv": sys.argv[1:]}
    with Path(os.environ["RCMAKE_EXEC_LOG"]).open("a") as log:
        log.write(json.dumps(record) + "\\n")
print("configure complete")
print("configure diagnostic", file=sys.stderr)
"""

FAKE_NINJA = """
import os
from pathlib import Path
import sys

if "-n" in sys.argv:
    print("[1/2] cc -Ione -Itwo")
    if Path(sys.argv[0]).name == "fail-ninja" and os.environ.get("RCMAKE_EXEC_LOG"):
        print("Ninja rejected the graph.", file=sys.stderr)
        raise SystemExit(3)
    print("[2/2] cc -Ione -Itwo")
"""


class BossFightTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()

    def script(self, path, body):
        path.write_text(f"#!{sys.executable}\n{body}")
        path.chmod(0o755)
        return path

    def test_cli_and_manifest(self):
        options = ["--rcmake-stage", "stage", "--oracle", "oracle/bin/cmake", "--run-dir", "run"]
        args = boss.parse_args(["--project", "all", "--mode", "full", "--keep-builds", *options])
        pins = boss.load_projects(args.project)
        self.assertEqual([row["name"] for row in pins],
                         ["duckdb", "opencv", "vtk", "qtbase", "llvm-project"])
        self.assertEqual(args.rcmake_stage, Path("stage"))
        self.assertEqual(args.oracle, Path("oracle/bin/cmake"))
        self.assertEqual(args.run_dir, Path("run"))
        self.assertEqual(args.mode, "full")
        self.assertTrue(args.keep_builds)
        default = boss.parse_args(["--project", "llvm-project", *options])
        self.assertEqual(default.mode, "parity")
        self.assertFalse(default.keep_builds)
        llvm, = boss.load_projects(default.project)
        self.assertEqual(llvm["source_subdir"], "llvm")
        self.assertEqual(llvm["l2_targets"], ["clang"])
        self.assertEqual(llvm["l4"]["ninja"], ["check-clang"])
        self.assertIn("-DBUILD_LIST=core,imgproc,imgcodecs", pins[1]["configure_args"])

    def test_cli_rejects_unknown_project_and_mode(self):
        args = ["--project", "duckdb", "--rcmake-stage", "stage", "--oracle", "oracle",
                "--run-dir", "run"]
        with contextlib.redirect_stderr(io.StringIO()):
            with self.assertRaises(SystemExit):
                boss.parse_args([*args, "--project", "boost"])
            with self.assertRaises(SystemExit):
                boss.parse_args([*args, "--mode", "quick"])

    def test_manifest_rejects_unknown_schema(self):
        path = self.root / "invalid.toml"
        path.write_text("schema_version = 9\n")
        with self.assertRaisesRegex(ValueError, "schema version"):
            boss.load_projects("all", path)

    def test_model_data_drops_backtrace_indexes(self):
        reply = self.root / "reply"
        reply.mkdir()
        (reply / "index-1.json").write_text(json.dumps({"reply": {"codemodel-v2": {"jsonFile": "m.json"}}}))
        target = {"name": "app", "backtrace": 1, "backtraceGraph": {"nodes": []},
                  "sources": [{"path": "a.c", "backtrace": 2, "backtraces": [2, 3]}],
                  "dependencies": [{"id": "b", "backtrace": 4}, {"id": "a", "backtrace": 5}]}
        (reply / "t.json").write_text(json.dumps(target))
        (reply / "m.json").write_text(json.dumps({"targets": [{"name": "app", "jsonFile": "t.json"}]}))
        self.assertEqual(boss.model_data(reply, Normalizer(self.root)), {"targets": [{"name": "app", "data": {
            "name": "app", "sources": [{"path": "a.c"}], "dependencies": [{"id": "a"}, {"id": "b"}]}}]})

    def test_save_l1_hides_only_the_value_of_nondeterministic_cache_entries(self):
        log = self.root / "log"
        log.mkdir()
        (log / "CMakeCache.txt").write_text(
            "// comment\nOPENCV_TIMESTAMP:INTERNAL=2026-11-25T10:00:00Z\nOTHER:STRING=x\n")
        (log / "configure.stderr").write_text("")
        tc = boss.Toolchain(self.root, self.root, Path("/usr/bin/cc"), Path("/usr/bin/c++"),
                            Path("ninja"), Path(sys.executable))
        with patch("boss_fight_local.run_command"):
            boss.save_l1(log, Normalizer(self.root), tc, {})
        self.assertEqual(json.loads((log / "cache.json").read_text()), {
            "OPENCV_TIMESTAMP:INTERNAL": "<nondeterministic>", "OTHER:STRING": "x"})

    def test_ninja_helper_preserves_duplicates_and_argument_order(self):
        output = "[1/3] cc -Ia -Ib\n[2/3] cc -Ib -Ia\n[3/3] cc -Ia -Ib\n"
        result = subprocess.CompletedProcess(["ninja"], 0, stdout=output, stderr="")
        with patch("ninja_canon.subprocess.run", return_value=result):
            self.assertEqual(dry_run(Normalizer(self.root), ["build"]),
                             ["cc -Ia -Ib", "cc -Ia -Ib", "cc -Ib -Ia"])

    def test_pathswap_reports_each_excess_occurrence(self):
        tools = self.root / "bin"
        tools.mkdir()
        for tool in boss.TOOLS:
            self.script(tools / tool, "")
        old = "[1/3] cc -Ia -Ib\n[2/3] cc -Ia -Ib\n[3/3] cc -Ia -Ib\n"
        new = "[1/1] cc -Ia -Ib\n"

        def evaluate(bin_dir, source, project, stages, env, timeout, log):
            project.mkdir()
            stdout = new if log else old
            return {"stages": [{"status": "pass", "returncode": 0, "stdout": stdout}]}

        stage = Stage("dry", ["ninja", "-n", "-v"], ninja_dry_run=True)
        with patch("pathswap.prepare_paths", return_value=(tools, {})), \
                patch("pathswap.evaluate", side_effect=evaluate), \
                patch("pathswap.compare", return_value={"classes": {}}):
            result = compare_runs(self.root, [stage], tools / "cmake", tools / "cmake",
                                  self.root / "reports", 120)
        self.assertEqual(result["status"], "fail")
        self.assertEqual(result["ninja_dry_runs"]["dry"],
                         {"oracle_only": ["cc -Ia -Ib", "cc -Ia -Ib"], "rcmake_only": []})

    def test_local_comparison_reports_counts(self):
        for engine, commands in (("oracle", ["cc", "cc", "cc"]), ("rcmake", ["cc"])):
            path = self.root / engine
            path.mkdir()
            boss.write_json(path / "commands.json", commands)
        l1, l2 = boss.compare_lanes(self.root)
        self.assertEqual(l2, "different")
        self.assertTrue(all(value == "incomplete" for value in l1.values()))
        self.assertEqual(json.loads((self.root / "command-count-diff.json").read_text()),
                         {"oracle_only": {"cc": 2}, "rcmake_only": {}})

    def fake_engines(self):
        engines = {}
        for name in ("oracle", "rcmake"):
            install = self.root / name
            modules = install / "share/cmake-4.5/Modules"
            modules.mkdir(parents=True)
            (modules / "CMakeDetermineSystem.cmake").touch()
            bin_dir = install / "bin"
            bin_dir.mkdir()
            engines[name] = bin_dir
            if name == "rcmake":
                self.script(bin_dir / "rcmake", FAKE_CMAKE)
                for tool in boss.TOOLS:
                    (bin_dir / tool).symlink_to("rcmake")
            else:
                for tool in boss.TOOLS:
                    self.script(bin_dir / tool, FAKE_CMAKE)
        return engines

    def test_configure_evidence_survives_ninja_failure_and_cleanup(self):
        engines = self.fake_engines()
        ninja = self.script(self.root / "fail-ninja", FAKE_NINJA)
        tc = boss.Toolchain(self.root, self.root, Path("/usr/bin/cc"), Path("/usr/bin/c++"),
                            ninja, Path(sys.executable))
        run = self.root / "run"
        (run / "reports").mkdir(parents=True)
        pin, = boss.load_projects("duckdb")
        args = boss.parse_args(["--project", "duckdb", "--rcmake-stage", str(self.root / "rcmake"),
                                "--oracle", str(engines["oracle"] / "cmake"), "--run-dir", str(run)])
        run_command = boss.run_command

        def checkout(log, work, source, repository, revision, env, **kwargs):
            source.mkdir()
            return revision

        def command(log, label, argv, timeout, env, cwd):
            if argv[0] == "git":
                return None
            return run_command(log, label, argv, timeout, env, cwd)

        disk = type("Disk", (), {"free": 100 * boss.GIB})()
        with patch("boss_fight_local.checkout", side_effect=checkout), \
                patch("boss_fight_local.run_command", side_effect=command), \
                patch("boss_fight_local.shutil.disk_usage", return_value=disk), \
                contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            result = boss.run_project(pin, run, engines, tc, args)
        report = run / "reports/duckdb"
        self.assertEqual(result["status"], "fail")
        self.assertEqual(result["l2"], "incomplete")
        self.assertEqual(result["l1"], dict.fromkeys(boss.L1_FILES, "equal"))
        self.assertEqual(result["lanes"]["rcmake"]["configure"], "pass")
        self.assertEqual(result["lanes"]["rcmake"]["dry_run"], "fail")
        self.assertFalse((run / "duckdb").exists())
        self.assertEqual(json.loads((report / "result.json").read_text()), result)
        candidate = report / "rcmake"
        self.assertIn("configure complete", (candidate / "configure.stdout").read_text())
        self.assertIn("configure diagnostic", (candidate / "configure.stderr").read_text())
        for artifact in ("CMakeCache.txt", "file-api/model.json", "ninja/build.ninja",
                         "cache-tools.json", "exec-proof.json", "dry-run.stdout"):
            self.assertTrue((candidate / artifact).is_file(), artifact)
        self.assertEqual(json.loads((candidate / "configure.json").read_text())["returncode"], 0)
        self.assertEqual(json.loads((candidate / "dry-run.json").read_text())["returncode"], 3)
        self.assertIn("Ninja rejected the graph.", (candidate / "dry-run.stderr").read_text())
        self.assertEqual(json.loads((report / "oracle/commands.json").read_text()),
                         ["cc -Ione -Itwo", "cc -Ione -Itwo"])

    def test_opencv_full_uses_the_test_graph_and_installs_the_manifest_graph(self):
        engines = self.fake_engines()
        ninja = self.script(self.root / "ninja", FAKE_NINJA)
        tc = boss.Toolchain(self.root, self.root, Path("/usr/bin/cc"), Path("/usr/bin/c++"),
                            ninja, Path(sys.executable))
        log, lane, source = (self.root / name for name in ("logs", "lane", "source"))
        log.mkdir()
        source.mkdir()
        pin, = boss.load_projects("opencv")
        result = {}
        with patch("boss_fight_local.MIN_FREE", 0), contextlib.redirect_stdout(io.StringIO()):
            boss.run_lane("rcmake", engines, pin, lane, log, source, [], tc, "full", source, result)

        def command(label):
            return json.loads((log / f"{label}.json").read_text())["argv"]

        self.assertIn("-DBUILD_LIST=core,imgproc,imgcodecs", command("configure"))
        self.assertIn("-DBUILD_LIST=core,imgproc,imgcodecs,ts", command("test-configure"))
        self.assertIn(str(lane / "test-build"), command("build"))
        self.assertIn("opencv_test_core", command("build"))
        self.assertIn("--no-tests=error", command("core-test"))
        self.assertEqual(command("manifest-install")[2:], [str(lane / "build"), "-j", "1", "install"])
        self.assertTrue(all(value == "pass" for value in result.values()))
        self.assertEqual(json.loads((log / "core-test-results.json").read_text())["gtest_skipped"], 2)

    def test_timeout_records_failure(self):
        argv = [sys.executable, "-c", "import time; time.sleep(10)"]
        with contextlib.redirect_stdout(io.StringIO()):
            with self.assertRaisesRegex(RuntimeError, "timeout"):
                boss.run_command(self.root, "timeout", argv, 0.1, os.environ.copy(), self.root)
        status = json.loads((self.root / "timeout.json").read_text())
        self.assertEqual(status["stop"], "timeout")
        self.assertLess(status["returncode"], 0)
        self.assertTrue((self.root / "timeout.stdout").is_file())

    def test_disk_limit_stops_before_launch(self):
        argv = [sys.executable, "-c", "print('started')"]
        with patch("boss_fight_local.MIN_FREE", sys.maxsize), contextlib.redirect_stdout(io.StringIO()):
            with self.assertRaisesRegex(RuntimeError, "disk limit"):
                boss.run_command(self.root, "disk", argv, 30, os.environ.copy(), self.root)
        status = json.loads((self.root / "disk.json").read_text())
        self.assertEqual(status["stop"], "disk limit")
        self.assertIsNone(status["returncode"])
        self.assertEqual((self.root / "disk.stdout").read_text(), "")

    def test_memory_limit_stops_process(self):
        argv = [sys.executable, "-c", "import time; time.sleep(10)"]
        with patch("boss_fight_local.MAX_RSS", 0), contextlib.redirect_stdout(io.StringIO()):
            with self.assertRaisesRegex(RuntimeError, "memory limit"):
                boss.run_command(self.root, "memory", argv, 5, os.environ.copy(), self.root)
        status = json.loads((self.root / "memory.json").read_text())
        self.assertEqual(status["stop"], "memory limit")
        self.assertGreater(status["peak_group_rss_bytes_sampled"], 0)


if __name__ == "__main__":
    unittest.main()

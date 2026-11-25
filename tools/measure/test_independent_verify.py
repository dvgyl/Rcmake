# SPDX-License-Identifier: MIT OR Apache-2.0
"""Unit tests for independent_verify.py.

Most tests do not need CMake. The integration test runs one fast scenario
with the CMake 4.5.1 oracle when RCMAKE_ORACLE_CMAKE names a program.
"""

from __future__ import annotations

import io
import json
import os
from pathlib import Path
import shutil
import sys
import tarfile
import tempfile
import textwrap
import time
import unittest
from unittest import mock
import zipfile

sys.path.insert(0, str(Path(__file__).resolve().parent))

import independent_verify as iv  # noqa: E402
import independent_scenarios_extra as ix  # noqa: E402


def fake_tools(root: Path, label: str = "oracle") -> iv.Toolset:
    """Return a tool set with paths that do not need to exist."""
    return iv.Toolset(label, root / "bin/cmake", root / "bin/ctest", root / "bin/cpack")


class TempDirTest(unittest.TestCase):
    """A test case with a temporary directory."""

    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory(prefix="indep-unit-")
        self.tmp = Path(os.path.realpath(self._tmp.name))

    def tearDown(self) -> None:
        self._tmp.cleanup()


class ZeroStreamTest(unittest.TestCase):
    def test_chunks_have_the_size_and_end_with_the_sentinel(self) -> None:
        for size, chunk in ((64, 7), (100, 100), (iv.MIB + 3, 65536)):
            stream = iv.ZeroStream(size)
            data = b"".join(iter(lambda: stream.read(chunk), b""))
            self.assertEqual(len(data), size)
            self.assertTrue(data.endswith(iv.BIG_SENTINEL))
            self.assertEqual(data[: size - len(iv.BIG_SENTINEL)].count(0), size - len(iv.BIG_SENTINEL))

    def test_read_all(self) -> None:
        self.assertEqual(iv.ZeroStream(20).read(), bytes(4) + iv.BIG_SENTINEL)


class ArchiveWriterTest(TempDirTest):
    def test_tar_gz_members_and_large_member_content(self) -> None:
        path = self.tmp / "a.tar.gz"
        iv.write_big_tar_gz(path, iv.MIB)
        with tarfile.open(path) as tar:
            self.assertEqual(tar.getnames(), [
                "payload/sentinel-begin.txt", "payload/big.bin", "payload/sentinel-end.txt"])
            tar.extractall(self.tmp / "x", filter="tar")
        self.assertTrue(iv.big_file_ok(self.tmp / "x/payload/big.bin", iv.MIB))
        self.assertFalse(iv.big_file_ok(self.tmp / "x/payload/big.bin", iv.MIB + 1))
        self.assertEqual((self.tmp / "x/payload/sentinel-end.txt").read_bytes(), b"END\n")

    def test_zip_is_stored(self) -> None:
        path = self.tmp / "a.zip"
        iv.write_stored_zip(path, iv.MIB)
        with zipfile.ZipFile(path) as zf:
            infos = zf.infolist()
            self.assertEqual([i.filename for i in infos],
                             ["zpayload/z-begin.txt", "zpayload/zbig.bin", "zpayload/z-end.txt"])
            self.assertTrue(all(i.compress_type == zipfile.ZIP_STORED for i in infos))
            self.assertEqual(infos[1].file_size, iv.MIB)
            self.assertTrue(zf.read("zpayload/zbig.bin").endswith(iv.BIG_SENTINEL))

    def test_archive_names_ignores_appledouble_members(self) -> None:
        path = self.tmp / "b.tar.gz"
        with tarfile.open(path, "w:gz") as tar:
            for name in ("d/._f.txt", "d/f.txt", "._d"):
                info = tarfile.TarInfo(name)
                info.size = 1
                tar.addfile(info, io.BytesIO(b"x"))
        self.assertEqual(iv.archive_names(path), ["d/f.txt"])


class NormalizeTest(unittest.TestCase):
    def test_root_tools_and_times(self) -> None:
        root = Path("/private/tmp/sandbox/s01/oracle")
        tools = fake_tools(Path("/opt/cm"))
        text = f"{root}/build (0.12s)\n/opt/cm/bin/cmake -E echo\r\nTotal 1.25 sec\n"
        self.assertEqual(iv.normalize_text(text, root, tools),
                         "<ROOT>/build (<T>s)\n<CMAKE> -E echo\nTotal <T> sec\n")

    def test_nested_values(self) -> None:
        root = Path("/r/x")
        ctx = iv.Ctx.__new__(iv.Ctx)
        ctx.root, ctx.tools = root, None
        self.assertEqual(ctx.normalize({"/r/x/k": ["/r/x/a", 3, ("/r/x/b",)]}),
                         {"<ROOT>/k": ["<ROOT>/a", 3, ["<ROOT>/b"]]})


class AliasTest(TempDirTest):
    def test_normalize_text_aliases(self) -> None:
        root = Path("/r/x")
        text = "/opt/real/bin/cmake -P /opt/real/share/cmake-4.5/Modules/A.cmake /r/x/b"
        aliases = [("/opt/real/bin/cmake", "<CMAKE>"), ("/opt/real/share/cmake-4.5", "<CMAKE_ROOT>")]
        self.assertEqual(iv.normalize_text(text, root, None, aliases),
                         "<CMAKE> -P <CMAKE_ROOT>/Modules/A.cmake <ROOT>/b")

    def test_learn_tool_paths_reads_the_cache(self) -> None:
        (self.tmp / "CMakeCache.txt").write_text(textwrap.dedent("""\
            CMAKE_COMMAND:INTERNAL=/real/bin/cmake
            CMAKE_ROOT:INTERNAL=/real/share/cmake-4.5
            CMAKE_CTEST_COMMAND:INTERNAL=/real/bin/ctest
            """))
        ctx = iv.Ctx(fake_tools(Path("/stage")), self.tmp / "run", iv.Options())
        ctx.learn_tool_paths(self.tmp)
        ctx.learn_tool_paths(self.tmp)  # A second read adds nothing.
        self.assertEqual(sorted(ctx.aliases), [
            ("/real/bin/cmake", "<CMAKE>"), ("/real/bin/ctest", "<CTEST>"),
            ("/real/share/cmake-4.5", "<CMAKE_ROOT>")])
        self.assertEqual(ctx.normalize(["/real/bin/cmake", "/stage/bin/cmake", "/real/share/cmake-4.5/x"]),
                         ["<CMAKE>", "<CMAKE>", "<CMAKE_ROOT>/x"])


class DiffTest(unittest.TestCase):
    def test_equal_and_different_maps(self) -> None:
        self.assertEqual(iv.diff_observations({"a": 1}, {"a": 1}), [])
        diffs = iv.diff_observations({"a": 1, "b": 2}, {"a": 3, "c": 4})
        self.assertEqual([d["key"] for d in diffs], ["a", "b", "c"])
        self.assertEqual(diffs[1]["rcmake"], "<missing>")


class SandboxTest(TempDirTest):
    def test_cleanup_removes_read_only_trees(self) -> None:
        box = iv.Sandbox(str(self.tmp))
        inner = box.path / "ro/dir"
        inner.mkdir(parents=True)
        (inner / "f").write_text("x")
        os.chmod(inner, 0o500)
        self.assertEqual(box.path.parent, self.tmp)
        self.assertTrue(box.path.name.startswith("rcmake-indep-"))
        box.cleanup()
        self.assertFalse(box.path.exists())
        box.cleanup()  # A second call does nothing.

    def test_keep(self) -> None:
        with iv.Sandbox(str(self.tmp), keep=True) as box:
            pass
        self.assertTrue(box.path.is_dir())


class ProcessTest(TempDirTest):
    def test_output_status_and_stdin(self) -> None:
        proc = iv.run_process([sys.executable, "-c", "import sys; print(sys.stdin.read().upper()); sys.exit(3)"],
                              self.tmp, iv.child_env(), 60, stdin="abc")
        self.assertEqual((proc.returncode, proc.stdout), (3, "ABC\n"))
        self.assertFalse(proc.ok)
        self.assertIn("[exit 3]", proc.tail())

    def test_missing_program(self) -> None:
        proc = iv.run_process([self.tmp / "absent"], self.tmp, iv.child_env(), 60)
        self.assertEqual(proc.returncode, 127)

    def test_timeout_kills_the_process_group(self) -> None:
        start = time.monotonic()
        proc = iv.run_process(["sh", "-c", "sleep 30 & sleep 30"], self.tmp, iv.child_env(), 0.5)
        self.assertTrue(proc.timed_out)
        self.assertLess(time.monotonic() - start, 10)

    def test_child_env_scrubs_build_variables(self) -> None:
        with mock.patch.dict(os.environ, {"CMAKE_GENERATOR": "Xcode", "MAKEFLAGS": "-j9", "KEEPME": "1"}):
            env = iv.child_env({"ADDED": "2", "KEEPME": None})
        self.assertNotIn("CMAKE_GENERATOR", env)
        self.assertNotIn("MAKEFLAGS", env)
        self.assertNotIn("KEEPME", env)
        self.assertEqual(env["ADDED"], "2")

    def test_rss_wrapper_reports_peak(self) -> None:
        proc = iv.run_process([sys.executable, "-c", iv.RSS_WRAPPER, sys.executable, "-c",
                               "b = bytearray(64 * 1024 * 1024); b[::4096] = b'x' * len(b[::4096])"],
                              self.tmp, iv.child_env(), 120)
        self.assertTrue(proc.ok, proc.tail())
        rss = iv.peak_rss(proc)
        self.assertIsNotNone(rss)
        self.assertGreater(rss, 60 * iv.MIB)
        self.assertIsNone(iv.peak_rss(iv.Proc([], "", 0, "", "nothing", 0.0)))


class HelperTest(TempDirTest):
    def test_no_op_and_compiled(self) -> None:
        def proc(out: str) -> iv.Proc:
            return iv.Proc([], "", 0, out, "", 0.0)
        self.assertTrue(iv.no_op(proc("ninja: no work to do.\n")))
        self.assertTrue(iv.no_op(proc("[100%] Built target app\n")))
        self.assertFalse(iv.no_op(proc("[1/2] Building C object CMakeFiles/a.dir/main.c.o\n")))
        self.assertFalse(iv.no_op(proc("[ 50%] Linking C executable a\n")))
        self.assertFalse(iv.no_op(iv.Proc([], "", 1, "", "", 0.0)))
        self.assertTrue(iv.compiled(proc("Building C object CMakeFiles/a.dir/main.c.o"), r"main\.c\.o"))
        self.assertFalse(iv.compiled(proc("Building C object CMakeFiles/a.dir/part1.c.o"), r"main\.c\.o"))

    def test_cache_entries(self) -> None:
        (self.tmp / "CMakeCache.txt").write_text(textwrap.dedent("""\
            // comment
            #INTERNAL
            A:STRING=1
            B:FILEPATH=/x/y
            C-ADVANCED:INTERNAL=1
            """))
        self.assertEqual(iv.cache_entries(self.tmp, ["A", "B", "Z"]), {"A": "STRING=1", "B": "FILEPATH=/x/y"})
        self.assertEqual(iv.cache_entries(self.tmp / "none", ["A"]), {})

    def test_file_listing(self) -> None:
        (self.tmp / "d").mkdir()
        (self.tmp / "d/f").write_text("x")
        (self.tmp / "d/._f").write_text("x")
        (self.tmp / "l").symlink_to("d")
        self.assertEqual(iv.file_listing(self.tmp), ["d/f", "l -> d"])

    def test_exe_path(self) -> None:
        self.assertEqual(iv.exe_path(Path("/b"), "a", "Ninja"), Path("/b/a"))
        self.assertEqual(iv.exe_path(Path("/b"), "a", "Xcode", "Debug"), Path("/b/Debug/a"))

    def test_junit_cases(self) -> None:
        path = self.tmp / "j.xml"
        path.write_text('<testsuite><testcase name="a" status="run"/>'
                        '<testcase name="b" status="fail"><failure message="x"/></testcase></testsuite>')
        self.assertEqual(iv.junit_cases(path), ["a:run", "b:fail+failure"])

    def test_sarif_digest(self) -> None:
        loc = {"physicalLocation": {"artifactLocation": {"uri": "a.cmake", "uriBaseId": "SRC"},
                                    "region": {"startLine": 4}},
               "logicalLocations": [{"name": "message"}]}
        path = self.tmp / "d.sarif"
        path.write_text(json.dumps({"runs": [{"results": [{
            "ruleId": "CMake.Warning", "level": "warning", "message": {"text": "w"},
            "locations": [loc], "stacks": [{"frames": [{"location": loc}]}]}]}]}))
        self.assertEqual(iv.sarif_digest(path), [{
            "ruleId": "CMake.Warning", "level": "warning", "message": "w",
            "locations": ["SRC:a.cmake:4:message"], "stacks": [["SRC:a.cmake:4:message"]]}])

    def test_long_link_sources_exceed_the_budget(self) -> None:
        files = iv.long_link_sources()
        objects = [f"CMakeFiles/longlink.dir/{rel}.o" for rel in files if rel.endswith(".c") and rel != "main.c"]
        self.assertEqual(len(objects), iv.LONG_FILES)
        self.assertGreater(sum(len(o) + 1 for o in objects), 140 * 1024)
        self.assertIn(f"long_fn_{iv.LONG_FILES - 1}();", files["main.c"])


class RegistryTest(unittest.TestCase):
    def test_twenty_unique_scenarios_in_order(self) -> None:
        ids = [sc.sid for sc in iv.SCENARIOS]
        self.assertEqual(ids, [f"s{i:02d}" for i in range(1, 21)])

    def test_extra_module_uses_the_same_registry(self) -> None:
        self.assertIs(ix.iv, iv)
        iv.load_extra_scenarios()  # A second load must not add the scenarios again.
        self.assertEqual(len(iv.SCENARIOS), 20)

    def test_script_lists_every_scenario_once(self) -> None:
        script = Path(iv.__file__).resolve()
        proc = iv.run_process([sys.executable, script, "--list"], self.tmp_dir(), iv.child_env(), 120)
        ids = [line.split()[0] for line in proc.stdout.splitlines() if line.strip()]
        self.assertEqual(proc.returncode, 0, proc.tail())
        self.assertEqual(ids, [f"s{i:02d}" for i in range(1, 21)])

    def tmp_dir(self) -> Path:
        tmp = tempfile.TemporaryDirectory(prefix="indep-unit-")
        self.addCleanup(tmp.cleanup)
        return Path(tmp.name)

    def test_select(self) -> None:
        self.assertEqual([sc.sid for sc in iv.select("s05, s01")], ["s05", "s01"])
        self.assertEqual(len(iv.select(None)), 20)
        self.assertEqual([sc.sid for sc in iv.select("s20,s15")], ["s20", "s15"])
        with self.assertRaises(SystemExit):
            iv.select("s99")

    def test_cli(self) -> None:
        args = iv.parse_args(["--oracle", "/x/cmake", "--oracle-only", "--only", "s01"])
        self.assertTrue(args.oracle_only)
        self.assertEqual(args.only, "s01")
        with mock.patch("sys.stdout", new=io.StringIO()) as out:
            self.assertEqual(iv.main(["--list"]), 0)
        self.assertIn("s14", out.getvalue())
        self.assertIn("s20", out.getvalue())
        with mock.patch("sys.stderr", new=io.StringIO()):
            self.assertEqual(iv.main(["--oracle", "/nonexistent/cmake", "--oracle-only"]), 2)


class RunnerTest(TempDirTest):
    """Exercise run_scenario with scenario functions that do not run CMake."""

    def run_fake(self, func, labels=("oracle", "rcmake"), requires=None):
        sc = iv.Scenario("t01", "fake", func, requires)
        toolsets = [fake_tools(self.tmp, label) for label in labels]
        return iv.run_scenario(sc, toolsets, self.tmp, iv.Options())

    def test_equal_observations_pass(self) -> None:
        def func(ctx: iv.Ctx) -> None:
            ctx.check("ok", True)
            ctx.observe("path", f"{ctx.root}/out")
        record = self.run_fake(func)
        self.assertEqual(record["status"], "pass")
        self.assertEqual(record["runs"]["oracle"]["observations"], {"path": "<ROOT>/out"})
        self.assertFalse((self.tmp / "t01/oracle").exists())

    def test_different_observations_fail(self) -> None:
        def func(ctx: iv.Ctx) -> None:
            ctx.check("ok", True)
            ctx.observe("who", ctx.tools.label)
        record = self.run_fake(func)
        self.assertEqual(record["status"], "fail")
        self.assertEqual(record["differences"], [{"key": "who", "oracle": "oracle", "rcmake": "rcmake"}])

    def test_failed_check_exception_and_empty_runs_fail(self) -> None:
        def failing(ctx: iv.Ctx) -> None:
            ctx.require("must", False, "detail")
            ctx.check("never reached", True)
        def raising(ctx: iv.Ctx) -> None:
            raise ValueError("boom")
        record = self.run_fake(failing, labels=("oracle",))
        self.assertEqual(record["status"], "fail")
        self.assertEqual(len(record["runs"]["oracle"]["checks"]), 1)
        record = self.run_fake(raising, labels=("oracle",))
        self.assertEqual(record["status"], "fail")
        self.assertIn("ValueError: boom", record["runs"]["oracle"]["checks"][0]["detail"])
        self.assertEqual(self.run_fake(lambda ctx: None, labels=("oracle",))["status"], "fail")

    def test_skips(self) -> None:
        def skip(ctx: iv.Ctx) -> None:
            raise iv.ScenarioSkip("no tool")
        self.assertEqual(self.run_fake(skip)["status"], "skip")
        record = self.run_fake(lambda ctx: None, requires=lambda options: "needs X")
        self.assertEqual((record["status"], record["reason"]), ("skip", "needs X"))


class ExtraHelperTest(TempDirTest):
    def test_is_appledouble(self) -> None:
        self.assertTrue(ix.is_appledouble("d/._a.txt"))
        self.assertTrue(ix.is_appledouble("._d/"))
        self.assertTrue(ix.is_appledouble("x/.DS_Store"))
        self.assertFalse(ix.is_appledouble("d/a._txt"))

    def test_tree_digest(self) -> None:
        (self.tmp / "d/sub").mkdir(parents=True)
        (self.tmp / "d/a.txt").write_text("a")
        os.chmod(self.tmp / "d/a.txt", 0o640)
        (self.tmp / "d/._a.txt").write_text("meta")
        (self.tmp / "d/l").symlink_to("a.txt")
        digest = ix.tree_digest(self.tmp / "d")
        self.assertEqual(digest[0].split(" ")[:2], ["a.txt", "640"])
        self.assertEqual(digest[1:], ["l -> a.txt", "sub/"])
        (self.tmp / "d/a.txt").write_text("b")
        self.assertNotEqual(ix.tree_digest(self.tmp / "d")[0], digest[0])

    def test_archive_kind(self) -> None:
        for name, head in (("g", b"\x1f\x8b\x08"), ("b", b"BZh9"), ("x", b"\xfd7zXZ\x00"),
                           ("z", b"\x28\xb5\x2f\xfd"), ("p", b"PK\x03\x04"), ("s", b"7z\xbc\xaf\x27\x1c")):
            (self.tmp / name).write_bytes(head + bytes(600))
        kinds = [ix.archive_kind(self.tmp / n) for n in "gbxzps"]
        self.assertEqual(kinds, ["gzip", "bzip2", "xz", "zstd", "zip", "7zip"])
        for fmt, kind in ((tarfile.GNU_FORMAT, "tar-gnu"), (tarfile.PAX_FORMAT, "tar-posix"),
                          (tarfile.USTAR_FORMAT, "tar-posix")):
            path = self.tmp / f"t{fmt}.tar"
            with tarfile.open(path, "w", format=fmt) as tar:
                info = tarfile.TarInfo("f")
                info.size = 1
                tar.addfile(info, io.BytesIO(b"x"))
            self.assertEqual(ix.archive_kind(path), kind)
        self.assertEqual(ix.archive_kind(self.tmp / "absent"), "missing")
        (self.tmp / "junk").write_bytes(bytes(600))
        self.assertEqual(ix.archive_kind(self.tmp / "junk"), "unknown")

    def test_listing_lines_and_python_members(self) -> None:
        self.assertEqual(ix.listing_lines("d/\nd/._a\nd/a\n\n"), ["d", "d/a"])
        path = self.tmp / "m.zip"
        with zipfile.ZipFile(path, "w") as zf:
            zf.writestr("d/", "")
            zf.writestr("d/a", "x")
            zf.writestr("d/._a", "x")
        self.assertEqual(ix.python_members(path), [("d", 0), ("d/a", 0)])
        tpath = self.tmp / "m.tar"
        with tarfile.open(tpath, "w") as tar:
            info = tarfile.TarInfo("d/a")
            info.size, info.mtime = 1, ix.FIXED_EPOCH
            tar.addfile(info, io.BytesIO(b"x"))
        self.assertEqual(ix.python_members(tpath), [("d/a", ix.FIXED_EPOCH)])

    def test_cmake_errors_and_message_lines(self) -> None:
        text = textwrap.dedent("""\
            -- Configuring done (0.5s)
            CMake Error in CMakeLists.txt:
              The target named "m" has

                Unix Makefiles

            CMake Warning (dev) at a.cmake:3 (message):
              w
            -- Generating done (0.0s)
            CMake Generate step failed.
            """)
        self.assertEqual(ix.cmake_errors(text), [
            'CMake Error in CMakeLists.txt: The target named "m" has Unix Makefiles',
            "CMake Warning (dev) at a.cmake:3 (message): w"])
        self.assertEqual(ix.cmake_errors("CMake Error: last\n  line"), ["CMake Error: last line"])
        self.assertEqual(ix.message_lines(text, "-- Gen", "CMake Gen"), [
            "-- Generating done (0.0s)", "CMake Generate step failed."])

    def test_resolve_reply_inlines_files_and_stops(self) -> None:
        reply = self.tmp
        (reply / "a.json").write_text(json.dumps({"x": 1, "jsonFile": "b.json"}))
        (reply / "b.json").write_text(json.dumps({"loop": {"jsonFile": "a.json"}}))
        (reply / "bad.json").write_text("{")
        out = ix.resolve_reply(reply, {"objs": [{"jsonFile": "a.json"}, {"jsonFile": "bad.json"},
                                                {"jsonFile": "../escape.json"}, {"jsonFile": "absent.json"}]})
        first = out["objs"][0]["json"]
        self.assertEqual(first["x"], 1)
        # a.json -> b.json -> a.json ... stops after four file references.
        node = first
        for _ in range(12):
            if not isinstance(node, dict):
                break
            node = node.get("json", node.get("loop"))
        self.assertEqual(node, "<not resolved a.json>")
        self.assertEqual(out["objs"][1]["json"], "<unreadable JSONDecodeError>")
        self.assertEqual(out["objs"][2]["json"], "<not resolved ../escape.json>")
        self.assertEqual(out["objs"][3]["json"], "<unreadable FileNotFoundError>")

    def test_file_api_reply_and_find_target(self) -> None:
        build = self.tmp
        self.assertEqual(ix.file_api_reply(build), ([], {}))
        reply = build / ".cmake/api/v1/reply"
        reply.mkdir(parents=True)
        (reply / "t.json").write_text(json.dumps({"name": "app", "type": "EXECUTABLE"}))
        (reply / "index-2.json").write_text(json.dumps({"targets": [{"name": "app", "jsonFile": "t.json"}]}))
        (reply / "index-1.json").write_text(json.dumps({}))
        names, index = ix.file_api_reply(build)
        self.assertEqual(names, ["index-1.json", "index-2.json"])
        self.assertEqual(ix.find_target(index, "app"), {"name": "app", "type": "EXECUTABLE"})
        self.assertEqual(ix.find_target({"abstractTargets": [{"name": "e", "json": {"imported": True}}]}, "e"),
                         {"imported": True})
        self.assertEqual(ix.find_target(index, "absent"), {})

    def test_without_install_layout(self) -> None:
        value = {"json": {"entries": [{"name": "CMAKE_EDIT_COMMAND", "value": "/x/ccmake"},
                                      {"name": "A", "value": "1"}]}}
        self.assertEqual(ix.without_install_layout(value), {"json": {"entries": [{"name": "A", "value": "1"}]}})

    def test_build_ninja_rules(self) -> None:
        (self.tmp / "build.ninja").write_text(textwrap.dedent("""\
            build dep$ sub/new.txt | ${cmake_ninja_workdir}dep$ sub/new.txt: CUSTOM_COMMAND
              COMMAND = cd x && cmake -E cmake_transform_depfile Ninja gccdepfile CMakeFiles/d/%s.d
              DESC = Generating new.txt
              depfile = CMakeFiles/d/%s.d
              deps = gcc
            build dep$ sub/old.txt: CUSTOM_COMMAND
              COMMAND = cd y
              depfile = old.d
            build other: phony dep$ sub/new.txt
            """ % ("a" * 64, "b" * 64)))
        self.assertEqual(ix.build_ninja_rules(self.tmp, "new.txt"), [
            "COMMAND = cd x && cmake -E cmake_transform_depfile Ninja gccdepfile CMakeFiles/d/<HASH>.d",
            "depfile = CMakeFiles/d/<HASH>.d", "deps = gcc"])
        self.assertEqual(ix.build_ninja_rules(self.tmp, "old.txt"), ["COMMAND = cd y", "depfile = old.d"])
        self.assertEqual(ix.build_ninja_rules(self.tmp / "none", "old.txt"), [])

    def test_shared_names(self) -> None:
        with mock.patch.object(ix, "DARWIN", True):
            self.assertEqual(ix.shared_names("v", "1.0", "1"), ("libv.1.0.dylib", "@rpath/libv.1.dylib"))
        with mock.patch.object(ix, "DARWIN", False):
            self.assertEqual(ix.shared_names("v", "1.0", "1"), ("libv.so.1.0", "libv.so.1"))
            self.assertIn("lib/libpkg_shared.so.2", ix.pkg_library_files())

    def test_binary_inspection_parsers(self) -> None:
        readelf = textwrap.dedent("""\
             0x0000000000000001 (NEEDED)             Shared library: [libpkg_shared.so.2]
             0x0000000000000001 (NEEDED)             Shared library: [libc.so.6]
             0x000000000000000e (SONAME)             Library soname: [libpkg_shared.so.2]
             0x000000000000001d (RUNPATH)            Library runpath: [$ORIGIN/../lib]
            """)
        otool_l = ("Load command 12\n          cmd LC_LOAD_DYLIB\n      cmdsize 56\n"
                   "         name @rpath/libpkg_shared.2.dylib (offset 24)\n"
                   "Load command 13\n          cmd LC_LOAD_DYLIB\n      cmdsize 56\n"
                   "         name /usr/lib/libSystem.B.dylib (offset 24)\n")
        otool_big_l = ("app:\n\t@rpath/libpkg_shared.2.dylib (compatibility version 2.0.0, current version 2.3.4)\n"
                       "\t/usr/lib/libSystem.B.dylib (compatibility version 1.0.0, current version 1.0.0)\n")
        outputs = {"readelf": readelf, "otool -l": otool_l, "otool -L": otool_big_l}
        def fake(binary, *args):
            return outputs.get(" ".join(args), "")
        outputs["readelf -d"] = outputs.pop("readelf")
        with mock.patch.object(ix, "tool_output", side_effect=fake):
            self.assertEqual(ix.elf_dynamic(Path("x")), {
                "NEEDED": ["libpkg_shared.so.2", "libc.so.6"], "SONAME": ["libpkg_shared.so.2"],
                "RUNPATH": ["$ORIGIN/../lib"]})
            self.assertEqual(ix.macho_load_dylibs(Path("x")), ["@rpath/libpkg_shared.2.dylib"])
            self.assertEqual(ix.macho_versions(Path("x"), "@rpath/libpkg_shared.2.dylib"),
                             "compatibility version 2.0.0, current version 2.3.4")
            self.assertEqual(ix.macho_versions(Path("x"), "@rpath/absent.dylib"), "")
        self.assertEqual(ix.tool_output(self.tmp, "/nonexistent/tool"), "")

    def test_exported_symbols(self) -> None:
        with mock.patch.object(ix, "DARWIN", True), \
                mock.patch.object(ix, "tool_output", return_value="0000 T _pkg_a\n0000 T _pkg_b\nbad\n"):
            self.assertEqual(ix.exported_symbols(Path("x")), ["pkg_a", "pkg_b"])
        with mock.patch.object(ix, "DARWIN", False), \
                mock.patch.object(ix, "tool_output", return_value="0000 T pkg_b\n0000 T pkg_a\n"):
            self.assertEqual(ix.exported_symbols(Path("x")), ["pkg_a", "pkg_b"])

    def test_write_tar_gz_is_reproducible(self) -> None:
        one, two = self.tmp / "1.tar.gz", self.tmp / "2/2.tar.gz"
        ix.write_tar_gz(one, "top", {"b.txt": "b\n", "a/x.txt": "\n    x\n"})
        ix.write_tar_gz(two, "top", {"a/x.txt": "\n    x\n", "b.txt": "b\n"})
        self.assertEqual(one.read_bytes(), two.read_bytes())
        with tarfile.open(one) as tar:
            self.assertEqual(tar.getnames(), ["top/a/x.txt", "top/b.txt"])
            self.assertEqual(tar.extractfile("top/a/x.txt").read(), b"x\n")
            self.assertTrue(all(m.mtime == ix.FIXED_EPOCH for m in tar.getmembers()))

    def test_file_api_queries(self) -> None:
        ix.write_file_api_queries(self.tmp)
        query = self.tmp / ".cmake/api/v1/query"
        self.assertEqual(sorted(p.name for p in query.iterdir()), [
            "cache-v2", "client-indep", "client-other", "cmakeFiles-v1", "codemodel-v2", "configureLog-v1",
            "toolchains-v1"])
        data = json.loads((query / "client-indep/query.json").read_text())
        self.assertEqual(len(data["requests"]), 7)

    def test_result(self) -> None:
        self.assertEqual(ix.result(iv.Proc([], "", 2, "o", "e", 0.0)), [2, "o", "e"])


@unittest.skipUnless(
    os.environ.get("RCMAKE_ORACLE_CMAKE") and os.access(os.environ.get("RCMAKE_ORACLE_CMAKE", ""), os.X_OK)
    and shutil.which("ninja"),
    "needs RCMAKE_ORACLE_CMAKE and ninja",
)
class OracleIntegrationTest(TempDirTest):
    def test_diamond_scenario_passes_with_the_oracle(self) -> None:
        tools = iv.Toolset.from_cmake("oracle", Path(os.environ["RCMAKE_ORACLE_CMAKE"]))
        sc = next(sc for sc in iv.SCENARIOS if sc.sid == "s05")
        record = iv.run_scenario(sc, [tools], self.tmp, iv.Options())
        failed = [c for c in record["runs"]["oracle"]["checks"] if not c["ok"]]
        self.assertEqual(record["status"], "pass", failed)


if __name__ == "__main__":
    unittest.main()

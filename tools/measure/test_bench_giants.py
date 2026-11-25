# SPDX-License-Identifier: MIT OR Apache-2.0
"""Mock-driven unit tests for tools/measure/bench_giants.py."""

from __future__ import annotations

from contextlib import redirect_stderr, redirect_stdout
import io
import json
import os
from pathlib import Path
import platform
import sys
from tempfile import TemporaryDirectory
import unittest
from unittest import mock

from tools.measure import bench_giants as bg


def _make_fake_binary(path: Path) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
    path.chmod(0o755)
    return path


def _make_project_tree(root: Path, *, subdir: str = ".") -> Path:
    cmake_root = root / subdir
    (cmake_root / "src" / "leaf_a").mkdir(parents=True, exist_ok=True)
    (cmake_root / "src" / "leaf_b").mkdir(parents=True, exist_ok=True)
    (cmake_root / "CMakeLists.txt").write_text(
        "cmake_minimum_required(VERSION 3.20)\nproject(Demo CXX)\nadd_subdirectory(src)\n",
        encoding="utf-8",
    )
    (cmake_root / "src" / "CMakeLists.txt").write_text(
        "add_subdirectory(leaf_a)\nadd_subdirectory(leaf_b)\n",
        encoding="utf-8",
    )
    (cmake_root / "src" / "leaf_a" / "CMakeLists.txt").write_text(
        "add_library(leaf_a STATIC a.cpp)\n",
        encoding="utf-8",
    )
    (cmake_root / "src" / "leaf_b" / "CMakeLists.txt").write_text(
        "add_library(leaf_b STATIC b.cpp)\n",
        encoding="utf-8",
    )
    return cmake_root


class CatalogAndCliTests(unittest.TestCase):
    def test_catalog_loads_five_default_giants_and_pkg_profiles(self):
        catalog = bg.load_project_catalog()
        for name in bg.DEFAULT_PROJECTS:
            self.assertIn(name, catalog)
            spec = catalog[name]
            self.assertEqual(spec.kind, "giant")
            self.assertTrue(spec.repository)
            self.assertTrue(spec.commit)
        self.assertEqual(catalog["llvm-project"].source_subdir, "llvm")
        self.assertEqual(catalog["llvm-project"].sparse_paths, bg.LLVM_SPARSE_PATHS)
        self.assertIn("vcpkg", catalog)
        self.assertIn("conan", catalog)
        self.assertEqual(catalog["vcpkg"].kind, "package_manager")
        self.assertEqual(catalog["conan"].kind, "package_manager")

    def test_project_selection_supports_commas_spaces_quick_and_rejects_unknown(self):
        catalog = bg.load_project_catalog()
        self.assertEqual(
            bg.parse_project_selection(None, quick=False, include_pkg_managers=False, catalog=catalog),
            list(bg.DEFAULT_PROJECTS),
        )
        self.assertEqual(
            bg.parse_project_selection(None, quick=True, include_pkg_managers=False, catalog=catalog),
            list(bg.QUICK_PROJECTS),
        )
        self.assertEqual(
            bg.parse_project_selection(
                ["duckdb,protobuf", "opencv", "duckdb"],
                quick=False,
                include_pkg_managers=True,
                catalog=catalog,
            ),
            ["duckdb", "protobuf", "opencv", "vcpkg", "conan"],
        )
        with self.assertRaises(ValueError):
            bg.parse_project_selection(
                ["duckdb,nonexistent_giant"],
                quick=False,
                include_pkg_managers=False,
                catalog=catalog,
            )

    def test_reproducer_command_matches_canonical_default_and_optional_flags(self):
        self.assertEqual(
            bg.build_reproducer_command(bg.DEFAULT_PROJECTS, 3),
            "python3 tools/measure/bench_giants.py "
            "--projects duckdb,opencv,protobuf,qtbase,llvm-project --runs 3",
        )
        custom = bg.build_reproducer_command(
            ["duckdb", "vcpkg"],
            5,
            package_manager="vcpkg",
            verify_build=True,
            verify_build_mode="dry-run",
            profile_diagnostics=True,
        )
        self.assertEqual(
            custom,
            "python3 tools/measure/bench_giants.py --projects duckdb,vcpkg --runs 5 "
            "--package-manager vcpkg --verify-build --verify-build-mode dry-run --profile-diagnostics",
        )


class ParsingAndHelpersTests(unittest.TestCase):
    def test_parse_rss_output_for_macos_and_linux(self):
        darwin_err = (
            "        0.42 real         0.31 user         0.08 sys\n"
            "           134217728  maximum resident set size\n"
        )
        self.assertEqual(bg.parse_rss_output("Darwin", darwin_err), 134217728)

        linux_err = (
            "\tCommand being timed: \"rcmake\"\n"
            "\tMaximum resident set size (kbytes): 65536\n"
        )
        self.assertEqual(bg.parse_rss_output("Linux", linux_err), 65536 * 1024)
        self.assertIsNone(bg.parse_rss_output("Linux", "no memory line"))
        self.assertIsNone(bg.parse_rss_output("Windows", darwin_err))

    def test_parse_probe_cache_stats_from_output_and_store_fallback(self):
        from_text = bg.parse_probe_cache_stats(
            "Rcmake probe cache: 64 hits, 2 misses\n",
            entries_before=60,
            entries_after=62,
        )
        self.assertEqual((from_text.hits, from_text.misses, from_text.source), (64, 2, "output"))

        from_kv = bg.parse_probe_cache_stats("probe_hits=19 probe_misses=1", 10, 11)
        self.assertEqual((from_kv.hits, from_kv.misses, from_kv.source), (19, 1, "output"))

        from_store = bg.parse_probe_cache_stats("-- Configuring done\n", 45, 45)
        self.assertEqual(
            (from_store.hits, from_store.misses, from_store.entries_before, from_store.entries_added, from_store.source),
            (None, 0, 45, 0, "store"),
        )

    def test_parse_profile_summary_reads_phases_and_categories(self):
        stderr = "\n".join([
            "Rcmake profile: 1.42 s invocation wall time, 18420 commands",
            "% of total uses 1.38 s of recorded wall time. Self time is summed across calls.",
            "",
            "Phases can overlap",
            "  Inclusive        Self % of total    Calls  Phase",
            "     1.10 s      0.95 s      79.7%        1  Eval",
            "   280.4 ms    280.4 ms      20.3%        1  Generate",
            "",
            "Command categories can overlap",
            "  Inclusive        Self % of total    Calls  Category",
            "   420.0 ms     12.5 ms      30.4%       58  check_*",
            "   390.2 ms     95.0 ms      28.3%       58  try_compile",
            "    88.1 ms     15.2 ms       6.4%        9  find_package",
            "",
            "Top list files by self time",
            "   200.0 ms    100.0 ms      14.5%        1  CMakeLists.txt",
        ])
        parsed = bg.parse_profile_summary(stderr)
        self.assertIsNotNone(parsed)
        assert parsed is not None
        self.assertEqual(parsed["invocation_wall"], "1.42 s")
        self.assertEqual(parsed["commands"], 18420)
        self.assertEqual(parsed["phases"]["Eval"]["calls"], 1)
        self.assertEqual(parsed["categories"]["try_compile"]["calls"], 58)
        self.assertEqual(parsed["categories"]["check_*"]["inclusive"], "420.0 ms")
        self.assertIsNone(bg.parse_profile_summary("plain stderr without profile"))

    def test_ninja_rerun_cmakelists_and_leaf_selection_handles_escapes_and_fallback(self):
        with TemporaryDirectory() as tmp:
            root = Path(tmp) / "src dir"
            cmake_root = _make_project_tree(root)
            escaped_root = str(cmake_root.resolve()).replace(" ", "$ ")
            ninja_text = (
                f"build build.ninja CMakeFiles/rules.ninja: RERUN_CMAKE | $\n"
                f"  {escaped_root}/CMakeLists.txt $\n"
                f"  {escaped_root}/src/CMakeLists.txt $\n"
                f"  {escaped_root}/src/leaf_a/CMakeLists.txt $\n"
                f"  {escaped_root}/src/leaf_b/CMakeLists.txt $\n"
                f"  {escaped_root}/cmake/helper.cmake\n"
            )
            found = bg.parse_ninja_rerun_cmakelists(ninja_text, cmake_root)
            self.assertEqual(
                found,
                [
                    Path("CMakeLists.txt"),
                    Path("src/CMakeLists.txt"),
                    Path("src/leaf_a/CMakeLists.txt"),
                    Path("src/leaf_b/CMakeLists.txt"),
                ],
            )
            leaf, mode = bg.choose_leaf_cmakelists(found, found, cmake_root)
            self.assertEqual((leaf, mode), (Path("src/leaf_a/CMakeLists.txt"), "leaf"))

            # When only root CMakeLists.txt is configured, fall back cleanly to root.
            root_only = [Path("CMakeLists.txt")]
            leaf_root, mode_root = bg.choose_leaf_cmakelists(root_only, root_only, cmake_root)
            self.assertEqual((leaf_root, mode_root), (Path("CMakeLists.txt"), "root_fallback"))

    def test_temporary_leaf_touch_advances_mtime_and_restores_on_error(self):
        with TemporaryDirectory() as tmp:
            root = Path(tmp)
            leaf = root / "CMakeLists.txt"
            build = root / "build"
            build.mkdir()
            original = b"add_library(demo STATIC demo.cpp)\n"
            leaf.write_bytes(original)
            (build / "build.ninja").write_text("# ninja\n", encoding="utf-8")
            before_stat = leaf.stat()
            ninja_mtime = (build / "build.ninja").stat().st_mtime_ns

            with self.assertRaises(RuntimeError):
                with bg.temporary_leaf_touch(leaf, build, "trial-0"):
                    self.assertIn(b"trial-0", leaf.read_bytes())
                    self.assertGreater(leaf.stat().st_mtime_ns, ninja_mtime)
                    raise RuntimeError("abort mid-trial")

            self.assertEqual(leaf.read_bytes(), original)
            self.assertEqual(leaf.stat().st_mtime_ns, before_stat.st_mtime_ns)

    def test_parse_ninja_dry_run_and_fast_compile_target_selection(self):
        cmake_out = "\n".join([
            "[1/3] /usr/bin/clang++ -O2 -o src/CMakeFiles/a.dir/a.cpp.o -c a.cpp",
            "[2/3] /usr/bin/clang++ -O2 -o src/CMakeFiles/b.dir/b.cpp.o -c b.cpp",
            "[3/3] /usr/bin/ar qc libdemo.a src/CMakeFiles/a.dir/a.cpp.o",
        ])
        rcmake_out = "\n".join([
            "[1/3] /usr/bin/clang++ -O2 -o src/CMakeFiles/b.dir/b.cpp.o -c b.cpp",
            "[2/3] /usr/bin/clang++ -O2 -o src/CMakeFiles/a.dir/a.cpp.o -c a.cpp",
            "[3/3] /usr/bin/ar qc libdemo.a src/CMakeFiles/a.dir/a.cpp.o",
        ])
        cmake_plan = bg.parse_ninja_dry_run(cmake_out)
        rcmake_plan = bg.parse_ninja_dry_run(rcmake_out)
        self.assertEqual(cmake_plan["step_count"], 3)
        self.assertEqual(cmake_plan["declared_total"], 3)
        self.assertEqual(
            bg.choose_fast_compile_target(cmake_plan, rcmake_plan),
            "src/CMakeFiles/a.dir/a.cpp.o",
        )
        self.assertEqual(
            bg.choose_fast_compile_target(cmake_plan, rcmake_plan, "custom.o"),
            "custom.o",
        )

    def test_measure_command_real_process_rss_and_timeout(self):
        with TemporaryDirectory() as tmp:
            cwd = Path(tmp)
            env = bg.benchmark_env(dict(os.environ))
            ok = bg.measure_command([sys.executable, "-c", "print('hello-giant')"], cwd, env, 15.0)
            self.assertTrue(ok.ok)
            self.assertEqual(ok.exit_code, 0)
            self.assertGreater(ok.wall_ms or 0.0, 0.0)
            self.assertIn("hello-giant", ok.stdout)
            if platform.system() in bg.RSS_CONFIG and Path("/usr/bin/time").is_file():
                self.assertIsNotNone(ok.peak_rss_bytes)
                self.assertGreater(ok.peak_rss_bytes or 0, 0)

            fail = bg.measure_command([sys.executable, "-c", "import sys; sys.exit(7)"], cwd, env, 15.0)
            self.assertFalse(fail.ok)
            self.assertEqual(fail.exit_code, 7)
            self.assertIn("Exit status 7", fail.error or "")

            slow = bg.measure_command(
                [sys.executable, "-c", "import time; time.sleep(5)"],
                cwd,
                env,
                0.2,
            )
            self.assertFalse(slow.ok)
            self.assertTrue(slow.timed_out)
            self.assertIn("Timed out", slow.error or "")


class BenchmarkFlowTests(unittest.TestCase):
    def setUp(self):
        tmp = TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name).resolve()
        self.cmake_bin = _make_fake_binary(self.root / "oracle" / "bin" / "cmake")
        self.rcmake_bin = _make_fake_binary(self.root / "stage" / "release" / "bin" / "rcmake")
        self.cache_dir = self.root / "cache"
        self.probe_dir = self.root / "shared-probes"
        self.source_duckdb = _make_project_tree(self.root / "src-duckdb")
        self.source_proto = _make_project_tree(self.root / "src-protobuf")

    def _mock_runner(self, *, fail_cmake_on: str | None = None):
        calls: list[dict] = []

        def runner(command, cwd, env, timeout):
            argv = [str(part) for part in command]
            exe_name = Path(argv[0]).name
            calls.append({"argv": argv, "cwd": str(cwd), "env": dict(env)})

            if "--version" in argv:
                if exe_name == "cmake":
                    return bg.CommandMeasurement(5.0, 12 * bg.MIB, 0, stdout="cmake version 4.5.1\n")
                return bg.CommandMeasurement(4.0, 8 * bg.MIB, 0, stdout="rcmake 0.1.0 (CMake 4.5.1)\n")

            if exe_name == "ninja":
                if "-n" in argv:
                    stdout = (
                        "[1/2] /usr/bin/clang++ -O2 -o src/leaf_a/a.cpp.o -c a.cpp\n"
                        "[2/2] /usr/bin/clang++ -O2 -o src/leaf_b/b.cpp.o -c b.cpp\n"
                    )
                    return bg.CommandMeasurement(15.0, 16 * bg.MIB, 0, stdout=stdout)
                build_dir = Path(argv[argv.index("-C") + 1])
                wall = 120.0 if "warm-cmake-" in str(build_dir) else 118.0
                return bg.CommandMeasurement(wall, 32 * bg.MIB, 0, stdout="[1/1] done\n")

            src_dir = Path(argv[argv.index("-S") + 1])
            build_dir = Path(argv[argv.index("-B") + 1])
            is_fresh = "-G" in argv
            is_profile = any(arg.startswith("--profile=") for arg in argv)

            if fail_cmake_on and exe_name == "cmake" and fail_cmake_on in str(src_dir):
                return bg.CommandMeasurement(
                    250.0, 50 * bg.MIB, 1, stderr="CMake Error: synthetic failure\n", error="Exit status 1."
                )

            build_dir.mkdir(parents=True, exist_ok=True)
            (build_dir / "CMakeCache.txt").write_text("CMAKE_BUILD_TYPE:STRING=Release\n", encoding="utf-8")
            (build_dir / "build.ninja").write_text(
                f"build build.ninja: RERUN_CMAKE | {src_dir}/CMakeLists.txt "
                f"{src_dir}/src/CMakeLists.txt {src_dir}/src/leaf_a/CMakeLists.txt\n",
                encoding="utf-8",
            )

            if is_profile:
                stderr = (
                    "Rcmake profile: 0.42 s invocation wall time, 3200 commands\n"
                    "Phases can overlap\n"
                    "  Inclusive        Self % of total    Calls  Phase\n"
                    "   350.0 ms    300.0 ms      83.3%        1  Eval\n"
                    "Command categories can overlap\n"
                    "  Inclusive        Self % of total    Calls  Category\n"
                    "   210.0 ms     10.0 ms      50.0%       24  try_compile\n"
                )
                return bg.CommandMeasurement(420.0, 48 * bg.MIB, 0, stderr=stderr)

            if exe_name == "cmake":
                if not is_fresh:
                    return bg.CommandMeasurement(400.0, 180 * bg.MIB, 0)
                return bg.CommandMeasurement(2000.0, 200 * bg.MIB, 0)

            probe_store = Path(env["RCMAKE_PROBE_CACHE_DIR"])
            entries_dir = probe_store / "entries" / "probe-v1"
            existing = len(list(entries_dir.rglob("*.entry"))) if entries_dir.is_dir() else 0
            entries_dir.mkdir(parents=True, exist_ok=True)

            if not is_fresh:
                return bg.CommandMeasurement(40.0, 64 * bg.MIB, 0)

            if existing == 0:
                for i in range(12):
                    (entries_dir / f"probe-{i}.entry").write_bytes(b"ok")
                return bg.CommandMeasurement(1000.0, 96 * bg.MIB, 0)

            return bg.CommandMeasurement(
                250.0,
                80 * bg.MIB,
                0,
                stderr="Rcmake probe cache: 12 hits, 0 misses\n",
            )

        return runner, calls

    def test_end_to_end_main_writes_json_markdown_and_isolates_probe_caches(self):
        runner, calls = self._mock_runner()
        out_json = self.root / "out" / "giants.json"
        out_md = self.root / "out" / "giants.md"
        ninja_fake = _make_fake_binary(self.root / "bin" / "ninja")

        stdout_buf = io.StringIO()
        stderr_buf = io.StringIO()
        with mock.patch.dict(os.environ, {"PATH": f"{ninja_fake.parent}:{os.environ.get('PATH', '')}"}):
            with redirect_stdout(stdout_buf), redirect_stderr(stderr_buf):
                rc = bg.main(
                    [
                        "--rcmake", str(self.rcmake_bin),
                        "--oracle", str(self.cmake_bin),
                        "--projects", "duckdb,protobuf",
                        "--runs", "3",
                        "--cache-dir", str(self.cache_dir),
                        "--probe-cache-dir", str(self.probe_dir),
                        "--source-dir", f"duckdb={self.source_duckdb}",
                        "--source-dir", f"protobuf={self.source_proto}",
                        "--verify-build",
                        "--profile-diagnostics",
                        "--json", str(out_json),
                        "--markdown", str(out_md),
                    ],
                    runner=runner,
                )

        self.assertEqual(rc, 0)
        report = json.loads(out_json.read_text(encoding="utf-8"))
        self.assertEqual(report["schema_version"], 1)
        self.assertEqual(
            report["reproducer"],
            "python3 tools/measure/bench_giants.py --projects duckdb,protobuf --runs 3 "
            "--verify-build --profile-diagnostics",
        )
        self.assertEqual(report["summary"]["projects_passed"], 2)
        self.assertEqual(report["summary"]["cold_speedup_geomean"], 2.0)
        self.assertEqual(report["summary"]["warm_probe_speedup_geomean"], 8.0)
        self.assertEqual(report["summary"]["incremental_leaf_speedup_geomean"], 10.0)
        self.assertEqual(report["summary"]["incremental_leaf_max_p95_ms"], 40.0)
        self.assertEqual(report["summary"]["cmake_max_peak_rss_mib"], 200.0)
        self.assertEqual(report["summary"]["rcmake_max_peak_rss_mib"], 96.0)

        duckdb = report["projects"][0]
        self.assertEqual(duckdb["cold"]["speedup"], 2.0)
        self.assertEqual(duckdb["warm_probe"]["speedup"], 8.0)
        self.assertEqual(duckdb["warm_probe"]["probe_cache"]["hits"], 12)
        self.assertEqual(duckdb["warm_probe"]["probe_cache"]["misses"], 0)
        self.assertEqual(duckdb["warm_probe"]["probe_cache"]["cold_entries_written"], 12)
        self.assertEqual(duckdb["incremental_leaf"]["leaf_file"], "src/leaf_a/CMakeLists.txt")
        self.assertEqual(duckdb["incremental_leaf"]["speedup"], 10.0)
        self.assertEqual(duckdb["incremental_leaf"]["rcmake"]["p95_ms"], 40.0)
        self.assertEqual(duckdb["build_verify"]["status"], "pass")
        self.assertTrue(duckdb["build_verify"]["dry_run"]["step_count_match"])
        self.assertEqual(duckdb["build_verify"]["compile"]["target"], "src/leaf_a/a.cpp.o")
        self.assertAlmostEqual(duckdb["build_verify"]["compile"]["build_speedup"], 1.02)
        self.assertEqual(duckdb["profile"]["commands"], 3200)

        # Verify cold rcmake trials never shared the persistent warm probe cache dir.
        cold_rcmake_stores = {
            call["env"]["RCMAKE_PROBE_CACHE_DIR"]
            for call in calls
            if Path(call["argv"][0]).name == "rcmake"
            and "-B" in call["argv"]
            and "cold-rcmake-" in call["argv"][call["argv"].index("-B") + 1]
        }
        self.assertEqual(len(cold_rcmake_stores), 6)
        self.assertNotIn(str(self.probe_dir), cold_rcmake_stores)

        # Verify no --profile flag was passed during timed warm or incremental runs.
        warm_fresh_rcmake_calls = [
            call["argv"]
            for call in calls
            if Path(call["argv"][0]).name == "rcmake"
            and "-G" in call["argv"]
            and "-B" in call["argv"]
            and "warm-rcmake-" in call["argv"][call["argv"].index("-B") + 1]
        ]
        inc_rcmake_calls = [
            call["argv"]
            for call in calls
            if Path(call["argv"][0]).name == "rcmake"
            and "-G" not in call["argv"]
            and "-B" in call["argv"]
            and "warm-rcmake-" in call["argv"][call["argv"].index("-B") + 1]
        ]
        self.assertEqual(len(warm_fresh_rcmake_calls), 6)
        self.assertEqual(len(inc_rcmake_calls), 6)
        for argv in (*warm_fresh_rcmake_calls, *inc_rcmake_calls):
            self.assertFalse(any(arg.startswith("--profile") for arg in argv))

        md_text = out_md.read_text(encoding="utf-8")
        self.assertIn(report["reproducer"], md_text)
        self.assertIn("| duckdb (", md_text)
        self.assertIn("8.00x", md_text)
        self.assertIn("12 hits / 0 misses", md_text)
        self.assertIn("## Generated Ninja build verification", md_text)
        self.assertEqual(stdout_buf.getvalue(), md_text)

    def test_no_fetch_marks_uncached_project_unavailable_without_git(self):
        runner, calls = self._mock_runner()
        spec = bg.load_project_catalog()["duckdb"]
        result = bg.benchmark_project(
            spec,
            cmake_bin=self.cmake_bin,
            rcmake_bin=self.rcmake_bin,
            cache_dir=self.cache_dir,
            shared_probe_cache=self.probe_dir,
            work_root=self.root / "work",
            base_env=bg.benchmark_env(dict(os.environ)),
            runs=1,
            generator="Ninja",
            timeout=30.0,
            clone_timeout=30.0,
            package_manager="none",
            source_overrides={},
            no_fetch=True,
            verify_build=False,
            verify_build_mode="both",
            verify_target=None,
            profile_diagnostics=False,
            keep_builds=False,
            runner=runner,
        )
        self.assertEqual(result["status"], "unavailable")
        self.assertIn("--no-fetch", result["unavailable_reason"])
        self.assertEqual(calls, [])

    def test_missing_pkg_manager_marks_profile_unavailable_cleanly(self):
        runner, _ = self._mock_runner()
        spec = bg.load_project_catalog()["vcpkg"]
        empty_bin = self.root / "empty-path"
        empty_bin.mkdir()
        env = bg.benchmark_env({"PATH": str(empty_bin)})
        result = bg.benchmark_project(
            spec,
            cmake_bin=self.cmake_bin,
            rcmake_bin=self.rcmake_bin,
            cache_dir=self.cache_dir,
            shared_probe_cache=self.probe_dir,
            work_root=self.root / "work",
            base_env=env,
            runs=1,
            generator="Ninja",
            timeout=30.0,
            clone_timeout=30.0,
            package_manager="none",
            source_overrides={},
            no_fetch=False,
            verify_build=False,
            verify_build_mode="both",
            verify_target=None,
            profile_diagnostics=False,
            keep_builds=False,
            runner=runner,
        )
        self.assertEqual(result["status"], "unavailable")
        self.assertIn("vcpkg", result["unavailable_reason"])

    def test_vcpkg_and_conan_profiles_pass_toolchain_without_mutating_repo_fixture(self):
        vcpkg_root = self.root / "vcpkg-root"
        _make_fake_binary(vcpkg_root / "vcpkg")
        toolchain = vcpkg_root / "scripts" / "buildsystems" / "vcpkg.cmake"
        toolchain.parent.mkdir(parents=True, exist_ok=True)
        toolchain.write_text("# mock vcpkg toolchain\n", encoding="utf-8")
        conan_bin = _make_fake_binary(self.root / "pkg-bin" / "conan")

        seen_commands: list[list[str]] = []

        def pkg_runner(command, cwd, env, timeout):
            argv = [str(part) for part in command]
            seen_commands.append(argv)
            exe = Path(argv[0]).name
            if exe == "vcpkg":
                return bg.CommandMeasurement(30.0, 20 * bg.MIB, 0)
            if exe == "conan":
                if argv[1:3] == ["profile", "detect"]:
                    profile_path = Path(env["CONAN_HOME"]) / "profiles" / "default"
                    profile_path.parent.mkdir(parents=True, exist_ok=True)
                    profile_path.write_text("[settings]\nos=Macos\n", encoding="utf-8")
                    return bg.CommandMeasurement(10.0, 12 * bg.MIB, 0)
                if argv[1] == "install":
                    out_arg = next(a for a in argv if a.startswith("--output-folder="))
                    out_dir = Path(out_arg.split("=", 1)[1]) / "build" / "Release" / "generators"
                    out_dir.mkdir(parents=True, exist_ok=True)
                    (out_dir / "conan_toolchain.cmake").write_text("# mock conan\n", encoding="utf-8")
                    return bg.CommandMeasurement(25.0, 18 * bg.MIB, 0)
            src_dir = Path(argv[argv.index("-S") + 1])
            build_dir = Path(argv[argv.index("-B") + 1])
            build_dir.mkdir(parents=True, exist_ok=True)
            (build_dir / "CMakeCache.txt").write_text("CMAKE_BUILD_TYPE:STRING=Release\n", encoding="utf-8")
            (build_dir / "build.ninja").write_text(
                f"build build.ninja: RERUN_CMAKE | {src_dir}/CMakeLists.txt\n",
                encoding="utf-8",
            )
            return bg.CommandMeasurement(120.0 if exe == "cmake" else 20.0, 40 * bg.MIB, 0)

        env = bg.benchmark_env({
            "PATH": f"{vcpkg_root}:{conan_bin.parent}:{os.environ.get('PATH', '')}",
            "VCPKG_ROOT": str(vcpkg_root),
        })
        catalog = bg.load_project_catalog()
        for profile, expected_toolchain in (
            ("vcpkg", "vcpkg.cmake"),
            ("conan", "conan_toolchain.cmake"),
        ):
            spec = catalog[profile]
            assert spec.local_fixture is not None
            fixture_lists = spec.local_fixture / "CMakeLists.txt"
            before_bytes = fixture_lists.read_bytes()
            res = bg.benchmark_project(
                spec,
                cmake_bin=self.cmake_bin,
                rcmake_bin=self.rcmake_bin,
                cache_dir=self.cache_dir,
                shared_probe_cache=self.probe_dir,
                work_root=self.root / f"work-{profile}",
                base_env=env,
                runs=1,
                generator="Ninja",
                timeout=30.0,
                clone_timeout=30.0,
                package_manager="none",
                source_overrides={},
                no_fetch=False,
                verify_build=False,
                verify_build_mode="both",
                verify_target=None,
                profile_diagnostics=False,
                keep_builds=False,
                runner=pkg_runner,
            )
            self.assertEqual(res["status"], "pass")
            self.assertEqual(res["cold"]["speedup"], 6.0)
            self.assertEqual(res["incremental_leaf"]["leaf_mode"], "root_fallback")
            self.assertEqual(fixture_lists.read_bytes(), before_bytes)
            self.assertTrue(
                any(
                    any(arg.startswith("-DCMAKE_TOOLCHAIN_FILE=") and arg.endswith(expected_toolchain) for arg in cmd)
                    for cmd in seen_commands
                ),
                f"Expected toolchain {expected_toolchain} in configure commands for {profile}",
            )

    def test_sparse_shallow_clone_uses_depth_1_and_reuses_cache_on_second_call(self):
        spec = bg.load_project_catalog()["llvm-project"]
        git_calls: list[list[str]] = []

        def fake_git(command, cwd, env, timeout):
            argv = [str(part) for part in command]
            git_calls.append(argv)
            if "init" in argv:
                Path(argv[-1]).mkdir(parents=True, exist_ok=True)
            elif "checkout" in argv:
                repo_dir = Path(argv[argv.index("-C") + 1])
                _make_project_tree(repo_dir, subdir="llvm")
            return bg.CommandMeasurement(10.0, 8 * bg.MIB, 0)

        fake_git_bin = _make_fake_binary(self.root / "bin" / "git")
        env = {"PATH": str(fake_git_bin.parent)}

        first, err1 = bg.ensure_source(
            spec, self.cache_dir, {}, env, no_fetch=False, clone_timeout=60.0, runner=fake_git
        )
        self.assertIsNone(err1)
        self.assertEqual(first, self.cache_dir / "sources" / "llvm-project" / "llvm")
        self.assertTrue(any("--depth=1" in cmd and "--filter=blob:none" in cmd for cmd in git_calls))
        self.assertTrue(any("sparse-checkout" in cmd and "clang" in cmd for cmd in git_calls))

        count_after_first = len(git_calls)
        second, err2 = bg.ensure_source(
            spec, self.cache_dir, {}, env, no_fetch=False, clone_timeout=60.0, runner=fake_git
        )
        self.assertIsNone(err2)
        self.assertEqual(second, first)
        self.assertEqual(len(git_calls), count_after_first)

    def test_configure_failure_returns_nonzero_and_records_errors(self):
        runner, _ = self._mock_runner(fail_cmake_on="src-duckdb")
        out_json = self.root / "failed.json"
        with redirect_stdout(io.StringIO()), redirect_stderr(io.StringIO()):
            rc = bg.main(
                [
                    "--rcmake", str(self.rcmake_bin),
                    "--oracle", str(self.cmake_bin),
                    "--projects", "duckdb",
                    "--runs", "1",
                    "--cache-dir", str(self.cache_dir),
                    "--source-dir", f"duckdb={self.source_duckdb}",
                    "--json", str(out_json),
                ],
                runner=runner,
            )
        self.assertEqual(rc, 1)
        report = json.loads(out_json.read_text(encoding="utf-8"))
        self.assertEqual(report["summary"]["projects_failed"], 1)
        self.assertEqual(report["projects"][0]["status"], "fail")
        self.assertEqual(report["projects"][0]["cold"]["cmake"]["exit_codes"], [1])


if __name__ == "__main__":
    unittest.main()

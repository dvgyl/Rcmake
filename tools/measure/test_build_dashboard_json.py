# SPDX-License-Identifier: MIT OR Apache-2.0
"""Test dashboard generation with reports that contain synthetic results."""

from contextlib import redirect_stderr
from datetime import datetime, timezone
import io
import json
from pathlib import Path
from tempfile import TemporaryDirectory
import unittest

from tools.measure import build_dashboard_json as builder


class DashboardTests(unittest.TestCase):
    def setUp(self):
        temporary = TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.output = self.root / "latest.json"
        self.base = builder.clean_draft_text(builder.read_json(builder.DATA / "latest.json"))
        self.schema = builder.read_json(builder.DATA / "dashboard.schema.json")

    def report(self, name, payload):
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(payload), encoding="utf-8")
        return str(path)

    def build(self, *args):
        self.assertEqual(builder.main(["--out", str(self.output), *args]), 0)
        result = builder.read_json(self.output)
        builder.validate(result, self.schema, self.schema)
        return result

    def gate(self, snapshot, identifier):
        gates = snapshot["tracks"]["benchmarksAndGates"]["gates"]
        return next(row for row in gates if row["id"] == identifier)

    def test_defaults_refresh_timestamp_without_replacing_results(self):
        result = self.build()
        generated = datetime.fromisoformat(result.pop("generatedAt").replace("Z", "+00:00"))
        self.assertLess(abs((datetime.now(timezone.utc) - generated).total_seconds()), 5)
        expected = dict(self.base)
        expected.pop("generatedAt")
        self.assertEqual(result, expected)

    def test_output_has_no_draft_labels(self):
        self.base["tracks"]["bossFight"]["label"] = "10 Boss-Fight Giants"
        self.base["statusNote"] = "[FINAL_METRIC] Gates are recorded."
        self.base["tracks"]["benchmarksAndGates"]["benchmarks"]["warmProbeSpeedup"]["note"] = (
            "[FINAL_METRIC] Measured across real-world corpus on idle hardware.")
        result = self.build("--base", self.report("draft.json", self.base))
        text = json.dumps(result)
        self.assertNotIn("[FINAL_METRIC]", text)
        self.assertNotIn("Boss-Fight", text)
        self.assertNotIn("real-world corpus", text)
        self.assertEqual(result["tracks"]["bossFight"]["label"], "Large projects")
        self.assertEqual(result["statusNote"], "Gates are recorded.")
        self.assertNotIn("note", result["tracks"]["benchmarksAndGates"]["benchmarks"]["warmProbeSpeedup"])

    def test_metadata_and_explicit_base_can_write_in_place(self):
        self.base["commit"] = "0000000"
        path = self.report("base.json", self.base)
        result = self.build("--base", path, "--commit", "cafe123", "--version", "1.2.3")
        self.assertEqual(result["commit"], "cafe123")
        self.assertEqual(result["rcmakeVersion"], "1.2.3")
        self.assertEqual(result["targetCmakeVersion"], self.base["targetCmakeVersion"])
        self.assertEqual(builder.main(["--base", path, "--out", path, "--version", "2.0.0"]), 0)
        self.assertEqual(builder.read_json(Path(path))["rcmakeVersion"], "2.0.0")
        self.assertEqual(builder.read_json(Path(path))["commit"], "0000000")

    def test_runcmake_os_and_cross_platform_rollups(self):
        def suite(passed, total, **extra):
            return {"passed": passed, "total": total, **extra}

        macos = self.report("macos.json", {"suites": {
            "CommandLine": suite(3, 3), "CMP0066": suite(2, 2), "CMP0001": suite(0, 1),
        }})
        linux = self.report("linux.json", {"suites": {
            "CommandLine": suite(1, 2), "CMP0066": suite(1, 1, reference_failed=1),
        }})
        win = self.report("windows.json", {"suites": {
            "CommandLine": suite(1, 1), "CMP0070": suite(
                1, 1, rcmake_driver={"complete": False, "driver_errors": ["Timed out"]}
            ),
        }})
        tracks = self.build("--runcmake-macos", macos, "--runcmake-linux", linux,
                            "--runcmake-windows", win)["tracks"]
        platforms = tracks["runCmakeOs"]["platforms"]
        self.assertEqual([(row["suitesPassed"], row["passRate"], row["status"]) for row in platforms],
                         [(2, 83.33, "fail"), (0, 66.67, "fail"), (1, 100, "fail")])
        self.assertEqual(tracks["runCmakeOs"]["status"], "fail")
        core = tracks["coreConformance"]
        self.assertEqual((core["suitesAt100"], core["casesPassed"], core["casesTotal"]),
                         (0, 5, 6))
        self.assertEqual(core["highlights"][0]["status"], "fail")
        self.assertEqual(core["highlights"][1]["status"], "pending")
        policies = tracks["policies"]
        self.assertEqual((policies["suitesPassed"], policies["passRate"], policies["status"]),
                         (0, 100, "fail"))

    def test_runcmake_with_missing_platforms_and_suites(self):
        macos = self.report("macos.json", {"suites": {"CommandLine": {"passed": 2, "total": 2}}})
        tracks = self.build("--runcmake-macos", macos)["tracks"]
        platform = tracks["runCmakeOs"]
        self.assertEqual(platform["status"], "partial")
        self.assertEqual([row["status"] for row in platform["platforms"]],
                         ["partial", "pending", "pending"])
        mac = platform["platforms"][0]
        self.assertTrue(mac["partial"])
        self.assertEqual((mac["completedSuites"], mac["expectedSuites"], mac["suitesTotal"]),
                         (1, 371, 371))
        self.assertEqual(tracks["coreConformance"]["status"], "partial")
        self.assertEqual(tracks["policies"], self.base["tracks"]["policies"])

    def test_runcmake_os_coverage_minimums_and_pass_rate_gate(self):
        def make_suites(total_suites, fail_count=0, case_total=10, driver_error=False):
            suites = {}
            for i in range(total_suites):
                name = "CommandLine" if i == 0 else f"Suite{i:03d}"
                if i < fail_count:
                    row = {"passed": 0, "total": case_total}
                else:
                    row = {"passed": case_total, "total": case_total}
                if driver_error and i == 0:
                    row["rcmake_driver"] = {"complete": False, "driver_errors": ["Timed out"]}
                suites[name] = row
            return {"suites": suites}

        # Seven-suite smoke report must never pass or shrink the expected OS denominator.
        smoke_mac = self.report("smoke-mac.json", make_suites(7))
        smoke_lin = self.report("smoke-lin.json", make_suites(7))
        smoke_win = self.report("smoke-win.json", make_suites(7))
        smoke_tracks = self.build(
            "--runcmake-macos", smoke_mac,
            "--runcmake-linux", smoke_lin,
            "--runcmake-windows", smoke_win,
        )["tracks"]["runCmakeOs"]
        self.assertEqual(smoke_tracks["status"], "partial")
        for row, expected in zip(smoke_tracks["platforms"], (371, 362, 367)):
            self.assertTrue(row["partial"])
            self.assertEqual((row["completedSuites"], row["expectedSuites"], row["suitesTotal"]),
                             (7, expected, expected))
            self.assertEqual(row["suitesPassed"], 7)
            self.assertEqual(row["passRate"], 100.0)
            self.assertEqual(row["status"], "partial")

        # Full OS coverage with >= 99.0% case pass rate passes; < 99.0% or driver errors fail.
        full_mac = self.report("full-mac.json", make_suites(371, fail_count=2, case_total=10))
        full_lin = self.report("full-lin.json", make_suites(362, fail_count=0, case_total=10))
        full_win = self.report("full-win.json", make_suites(367, fail_count=4, case_total=10))
        full_tracks = self.build(
            "--runcmake-macos", full_mac,
            "--runcmake-linux", full_lin,
            "--runcmake-windows", full_win,
        )["tracks"]["runCmakeOs"]
        mac_row, lin_row, win_row = full_tracks["platforms"]
        self.assertFalse(mac_row["partial"])
        self.assertGreaterEqual(mac_row["passRate"], 99.0)
        self.assertEqual(mac_row["status"], "pass")
        self.assertEqual(lin_row["status"], "pass")
        self.assertFalse(win_row["partial"])
        self.assertLess(win_row["passRate"], 99.0)
        self.assertEqual(win_row["status"], "fail")
        self.assertEqual(full_tracks["status"], "fail")

        # Full suite count with an incomplete driver still fails even above 99.0%.
        bad_driver = self.report("bad-driver-lin.json", make_suites(362, fail_count=0, driver_error=True))
        bad_lin = self.build("--runcmake-linux", bad_driver)["tracks"]["runCmakeOs"]["platforms"][1]
        self.assertFalse(bad_lin["partial"])
        self.assertEqual(bad_lin["status"], "fail")

        # All three OSes meeting coverage and >= 99.0% pass rate mark the track pass.
        full_win_ok = self.report("full-win-ok.json", make_suites(367, fail_count=1, case_total=10))
        all_ok = self.build(
            "--runcmake-macos", full_mac,
            "--runcmake-linux", full_lin,
            "--runcmake-windows", full_win_ok,
        )["tracks"]["runCmakeOs"]
        self.assertEqual(all_ok["status"], "pass")
        self.assertEqual([row["status"] for row in all_ok["platforms"]], ["pass", "pass", "pass"])

    def test_independent_checks_and_differential_status(self):
        path = self.report("independent.json", {
            "mode": "differential", "summary": {"pass": 1, "fail": 1, "skip": 1}, "scenarios": [
                {"status": status, "runs": {"rcmake": {"checks": [{"ok": True}]}}}
                for status in ("pass", "fail", "skip")
            ],
        })
        row = self.gate(self.build("--independent", path), "blackbox")
        self.assertEqual((row["passed"], row["total"], row["failures"]), (2, 2, 0))
        self.assertEqual((row["status"], row["scenariosPassed"], row["scenariosTotal"]), ("fail", 1, 3))

    def test_soundness_report_flags(self):
        probe = self.report("probe.json", {
            "mode": "gate", "passed": True, "cases": [{"status": "passed", "passed": True}],
        })
        mutation = self.report("mutation.json", {
            "passed": False, "replay_verified": False,
            "steps": [{"step": 0, "passed": True}, {"step": 1, "passed": True}, {"step": 2}],
        })
        determinism = self.report("path-swap.json", {"status": "oracle_error", "differences": []})
        wasm = self.report("wasm.json", {
            "status": "incomplete", "checks": [{"status": "pass"}, {"status": "skipped"}],
        })
        result = self.build("--probe-cache", probe, "--mutation", mutation, "--determinism",
                            determinism, "--wasm", wasm)
        for identifier, expected in {
            "probe-cache": ("pass", 1, 1), "mutation-replay": ("fail", 1, 2),
            "determinism": ("fail", 0, 1), "wasi": ("partial", 1, 2),
        }.items():
            with self.subTest(identifier=identifier):
                row = self.gate(result, identifier)
                self.assertEqual((row["status"], row["passed"], row["total"]), expected)
                self.assertEqual(row["failures"], row["total"] - row["passed"])

    def test_repeat_bench_for_both_report_formats(self):
        configure = self.report("bench-configure.json", {"results": [
            {"case": "warm probe-cache configure", "project": "a", "speedup": 4,
             "gate": {"status": "fail"}},
            {"case": "warm probe-cache configure", "project": "b", "speedup": 9,
             "gate": {"status": "pass"}},
            {"case": "warm probe-cache configure", "project": "synthetic", "speedup": 20},
            {"case": "incremental-leaf", "project": "a", "rcmake": {"p95_ms": 45}},
            {"case": "incremental-leaf", "project": "b", "rcmake": {"p95_ms": 63}},
        ]})
        legacy = self.report("bench.json", {"results": [
            {"name": "configure 100 targets (cold)", "speedup": 7},
            {"name": "configure 500 targets (cold)", "speedup": 1.5,
             "notes": "new build tree, empty probe cache, includes compiler checks",
             "rcmake": {"ok": True, "peak_rss_mib": 88.2}},
        ]})
        section = self.build("--bench", configure, "--bench", legacy)["tracks"]["benchmarksAndGates"]
        self.assertEqual({key: (row["value"], row["status"]) for key, row in section["benchmarks"].items()},
                         {"warmProbeSpeedup": (6, "fail"), "incrementalLeafP95Ms": (63, "pass"),
                          "coldConfigureSpeedup": (1.5, "pass"), "peakRssMiB": (88.2, "pass")})
        self.assertTrue(section["benchmarks"]["warmProbeSpeedup"]["note"].startswith(
            "Geometric mean across a, b."))
        partial = self.build("--bench", configure)["tracks"]["benchmarksAndGates"]["benchmarks"]
        self.assertEqual(partial["peakRssMiB"]["status"], "pending")
        stale = self.report("stale.json", {"results": [
            {"name": "configure 500 targets (cold)", "speedup": 9.0,
             "notes": "new build tree, includes compiler checks",
             "rcmake": {"ok": True, "peak_rss_mib": 88.2}},
        ]})
        with redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
            builder.main(["--out", str(self.output), "--bench", stale])

    def test_large_projects_reads_summary_and_project_reports_without_double_counting(self):
        self.report("large-projects/reports/result.json", {"projects": [
            {"project": "duckdb", "l1": {"cache.json": "equal"}, "l2": "equal"},
        ]})
        self.report("large-projects/reports/duckdb/result.json", {
            "project": "duckdb", "l1": {"cache.json": "equal"}, "l2": "equal",
        })
        self.report("large-projects/reports/opencv/result.json", {
            "project": "opencv", "l1": {"cache.json": "different"}, "l2": "equal",
        })
        snapshot = self.build("--large-projects-dir", str(self.root / "large-projects"))
        track = snapshot["tracks"]["bossFight"]
        self.assertEqual((track["status"], track["projectsTotal"], track["l1Passed"], track["l2Passed"]),
                         ("fail", 10, 1, 2))
        self.assertEqual([(row["l1"], row["l2"]) for row in track["projects"][:3]],
                         [("pass", "pass"), ("fail", "pass"), ("pending", "pending")])

    def test_holdout_uses_public_categories_and_drops_private_content(self):
        name = "private-example-repo"
        url = f"https://example.invalid/{name}"
        label = self.base["tracks"]["unseenHoldout"]["categories"][0]["category"]
        path = self.report("sealed-report.json", {
            "aggregate": {"reposTotal": 4, "reposPassed": 3, "note": url},
            "categories": [
                {"category": label, "reposTotal": 3, "reposPassed": 2, "repositories": [name, url]},
                {"category": url, "reposTotal": 1, "reposPassed": 1},
            ],
            "repos": {"repo": name, "url": url},
        })
        track = self.build("--unseen-holdout", path)["tracks"]["unseenHoldout"]
        self.assertEqual((track["sealed"], track["passRate"], track["status"]), (True, 75, "fail"))
        self.assertEqual(track["categories"][0], {"category": label, "reposTotal": 3,
                                                  "reposPassed": 2, "passRate": 66.67, "status": "fail"})
        self.assertEqual(track["categories"][1]["status"], "pending")
        serialized = self.output.read_text(encoding="utf-8")
        self.assertNotIn(name, serialized)
        self.assertNotIn(url, serialized)

    def test_fuzz_counts_inputs_and_failures(self):
        self.report("fuzz/a.json", {"iterations": 200, "failures": []})
        self.report("fuzz/shard/b.json", {"iterations": 100, "failures": 2})
        self.report("fuzz/._a.json", {})
        gate = self.gate(self.build("--fuzz-dir", str(self.root / "fuzz")), "fuzz")
        self.assertEqual((gate["status"], gate["passed"], gate["total"], gate["failures"]),
                         ("fail", 298, 300, 2))
        self.assertTrue(gate["label"].startswith("Seeded generative fuzzing"), gate["label"])
        self.assertEqual(gate["summary"], "300 inputs, 2 failures")
        self.assertIn("not coverage-guided", gate["detail"])

    def test_vcpkg_aggregates_workflow_artifacts(self):
        def shard(index, ports, outcomes):
            path = self.report(f"vcpkg/artifact-{index}/shard.json", {
                "shard_index": index, "shard_count": 64, "expected_ports": ports,
            })
            rows = [json.dumps({"status": status, "port": f"port-{i}"})
                    for i, status in enumerate(outcomes)]
            Path(path).with_name("results.jsonl").write_text("\n".join(rows), encoding="utf-8")

        shard(2, 2, ["pass", "configure-mismatch"])
        shard(3, 2, ["pass", "pass"])
        shard(4, 2, ["cmake-identity-mismatch"])
        shard(5, 1, ["oracle-blocked-before-configure"])
        track = self.build("--vcpkg-dir", str(self.root / "vcpkg"))["tracks"]["vcpkgMatrix"]
        self.assertEqual((track["shardsCompleted"], track["shardsPassed"], track["passRate"]),
                         (3, 1, 42.86))
        self.assertEqual(track["portsTotal"], 7)
        self.assertEqual([row["status"] for row in track["shards"][:6]],
                         ["pending", "pending", "fail", "pass", "partial", "fail"])
        self.assertEqual(track["shards"][2]["portsFailed"], 1)
        self.assertEqual(track["shards"][4]["portsFailed"], 1)

    def test_ci_status_reports_update_the_snapshot_note(self):
        runs = [
            {"status": "completed", "conclusion": "success"},
            {"status": "completed", "conclusion": "failure"},
            {"status": "in_progress", "conclusion": ""},
        ]
        for payload in (runs, {"workflow_runs": runs}):
            with self.subTest(payload=type(payload)):
                path = self.report("ci.json", payload)
                result = self.build("--ci-status-json", path)
                self.assertEqual(result["statusNote"], "CI reports 1 of 2 completed runs as successful. "
                                 "Open runs total 1.")
                self.assertEqual(result["tracks"], self.base["tracks"])

    def test_schema_checks_constraints_and_does_not_write_invalid_result(self):
        self.output.write_text("keep this file", encoding="utf-8")
        path = self.report("bad-holdout.json", {"aggregate": {"reposTotal": 0, "reposPassed": 0}})
        with redirect_stderr(io.StringIO()) as error, self.assertRaises(SystemExit) as exit_code:
            builder.main(["--out", str(self.output), "--unseen-holdout", path])
        self.assertEqual(exit_code.exception.code, 2)
        self.assertIn("unseenHoldout.reposTotal", error.getvalue())
        self.assertEqual(self.output.read_text(encoding="utf-8"), "keep this file")

    def test_schema_rejects_wrong_types_and_category_extensions(self):
        for track, key, value in (
            ("coreConformance", "status", "unknown"), ("coreConformance", "suitesTotal", True),
            ("coreConformance", "passRate", 100.01), ("vcpkgMatrix", "shards", []),
            ("unseenHoldout", "sealed", False),
        ):
            with self.subTest(key=key):
                snapshot = json.loads(json.dumps(self.base))
                snapshot["tracks"][track][key] = value
                with self.assertRaises(ValueError):
                    builder.validate(snapshot, self.schema, self.schema)
        self.base["tracks"]["unseenHoldout"]["categories"][0]["repo"] = "private"
        with self.assertRaises(ValueError):
            builder.validate(self.base, self.schema, self.schema)

    def test_giants_bench_ingests_per_giant_speedups_rss_and_build_verify(self):
        reproducer = (
            "python3 tools/measure/bench_giants.py "
            "--projects duckdb,opencv,protobuf,qtbase,llvm-project --runs 3"
        )
        path = self.report("giants-bench.json", {
            "schema_version": 1,
            "generated_at": "2026-11-25T20:00:00Z",
            "reproducer": reproducer,
            "meta": {"runs": 3},
            "projects": [
                {
                    "name": "duckdb",
                    "repository": "duckdb/duckdb",
                    "version": "v1.3.2",
                    "status": "pass",
                    "cold": {
                        "cmake": {"median_ms": 4000.0, "peak_rss_mib": 210.5},
                        "rcmake": {"median_ms": 1000.0, "peak_rss_mib": 92.4},
                        "speedup": 4.0,
                    },
                    "warm_probe": {
                        "cmake": {"median_ms": 4000.0, "peak_rss_mib": 208.0},
                        "rcmake": {"median_ms": 500.0, "peak_rss_mib": 84.1},
                        "speedup": 8.0,
                        "probe_cache": {"hits": 44, "misses": 0},
                    },
                    "incremental_leaf": {
                        "cmake": {"median_ms": 900.0, "peak_rss_mib": 195.0},
                        "rcmake": {"median_ms": 45.0, "p95_ms": 52.0, "peak_rss_mib": 62.0},
                        "speedup": 20.0,
                        "leaf_file": "src/main/CMakeLists.txt",
                    },
                    "build_verify": {
                        "status": "pass",
                        "dry_run": {"step_count_match": True},
                        "compile": {
                            "target": "src/a.cpp.o",
                            "cmake": {"wall_ms": 150.0},
                            "rcmake": {"wall_ms": 148.0},
                            "build_speedup": 1.01,
                        },
                    },
                },
                {
                    "name": "qtbase",
                    "repository": "qt/qtbase",
                    "version": "v6.8.3",
                    "status": "pass",
                    "cold": {
                        "cmake": {"median_ms": 9000.0, "peak_rss_mib": 320.0},
                        "rcmake": {"median_ms": 2250.0, "peak_rss_mib": 140.0},
                        "speedup": 4.0,
                    },
                    "warm_probe": {
                        "cmake": {"median_ms": 9000.0, "peak_rss_mib": 318.0},
                        "rcmake": {"median_ms": 1125.0, "peak_rss_mib": 135.0},
                        "speedup": 8.0,
                        "probe_cache": {"hits": 120, "misses": 0},
                    },
                    "incremental_leaf": {
                        "cmake": {"median_ms": 1500.0, "peak_rss_mib": 300.0},
                        "rcmake": {"median_ms": 75.0, "p95_ms": 88.5, "peak_rss_mib": 90.0},
                        "speedup": 20.0,
                        "leaf_file": "src/corelib/CMakeLists.txt",
                    },
                },
                {
                    "name": "vcpkg",
                    "repository": "local/conformance/ecosystem/vcpkg-manifest",
                    "version": "manifest",
                    "status": "unavailable",
                    "unavailable_reason": "vcpkg executable not found",
                },
            ],
        })
        section = self.build("--giants-bench", path)["tracks"]["benchmarksAndGates"]
        giants = section["giantsBenchmark"]
        self.assertEqual(giants["status"], "partial")
        self.assertEqual(giants["runs"], 3)
        self.assertEqual(giants["reproducer"], reproducer)
        self.assertEqual(giants["summary"], {
            "projectsTotal": 3,
            "projectsPassed": 2,
            "coldSpeedupGeomean": 4.0,
            "warmProbeSpeedupGeomean": 8.0,
            "incrementalLeafSpeedupGeomean": 20.0,
            "incrementalLeafMaxP95Ms": 88.5,
            "rcmakeMaxPeakRssMiB": 140.0,
            "cmakeMaxPeakRssMiB": 320.0,
        })
        self.assertEqual(giants["projects"][0]["warmProbe"]["probeCacheHits"], 44)
        self.assertEqual(giants["projects"][0]["buildVerify"]["target"], "src/a.cpp.o")
        self.assertEqual(giants["projects"][2]["status"], "pending")
        self.assertEqual(giants["projects"][2]["note"], "vcpkg executable not found")
        # Existing top-level benchmark cards remain intact.
        self.assertEqual(section["benchmarks"], self.base["tracks"]["benchmarksAndGates"]["benchmarks"])

    def test_giants_bench_rejects_invalid_reports_without_writing(self):
        self.output.write_text("keep original", encoding="utf-8")
        bad = self.report("bad-giants.json", {
            "schema_version": 1,
            "reproducer": "python3 tools/measure/bench_giants.py --projects duckdb --runs 3",
            "meta": {"runs": 3},
            "projects": [{
                "name": "duckdb",
                "status": "pass",
                "cold": {"cmake": {"median_ms": -5.0}, "rcmake": {"median_ms": 10.0}, "speedup": 1.0},
                "warm_probe": {"cmake": {"median_ms": 10.0}, "rcmake": {"median_ms": 2.0}, "speedup": 5.0},
                "incremental_leaf": {"cmake": {"median_ms": 5.0}, "rcmake": {"median_ms": 1.0}, "speedup": 5.0},
            }],
        })
        with redirect_stderr(io.StringIO()) as err, self.assertRaises(SystemExit) as exit_code:
            builder.main(["--out", str(self.output), "--giants-bench", bad])
        self.assertEqual(exit_code.exception.code, 2)
        self.assertIn("nonnegative", err.getvalue())
        self.assertEqual(self.output.read_text(encoding="utf-8"), "keep original")

    def test_differential_fuzz_ingests_oracle_prop_test_without_touching_crash_fuzz_gate(self):
        report_a = self.report("diff-fuzz/seed-42.json", {
            "seed": 42,
            "total": 4,
            "oracle_bin": "/opt/oracle/bin/cmake",
            "rcmake_bin": "/opt/stage/bin/cmake",
            "ninja_bin": "/usr/bin/ninja",
            "counts": {"pass": 4, "fail": 0, "oracle_error": 0},
            "cases": [
                {"index": 1, "case_seed": 11, "family": "genex.bool_logic", "mode": "configure", "status": "pass"},
                {"index": 2, "case_seed": 12, "family": "string.json", "mode": "script", "status": "pass"},
                {"index": 3, "case_seed": 13, "family": "list.sort", "mode": "script", "status": "pass"},
                {"index": 4, "case_seed": 14, "family": "string.json", "mode": "script", "status": "pass"},
            ],
        })
        result = self.build("--differential-fuzz", report_a)
        section = result["tracks"]["benchmarksAndGates"]
        diff = section["differentialFuzz"]
        self.assertEqual(diff["status"], "pass")
        self.assertEqual((diff["casesTotal"], diff["casesPassed"], diff["mismatches"], diff["oracleErrors"]),
                         (4, 4, 0, 0))
        self.assertEqual(diff["passRate"], 100.0)
        self.assertEqual(diff["seeds"], [42])
        self.assertEqual((diff["scriptCases"], diff["configureCases"], diff["families"]), (3, 1, 3))
        # The existing 14M crash-fuzz gate stays intact and separate.
        crash_gate = self.gate(result, "fuzz")
        self.assertEqual((crash_gate["status"], crash_gate["passed"], crash_gate["total"]),
                         ("pass", 14000000, 14000000))
        serialized = self.output.read_text(encoding="utf-8")
        self.assertNotIn("/opt/oracle", serialized)
        self.assertNotIn("/opt/stage", serialized)

        # Mismatches and oracle errors both fail the differential fuzz gate across directories.
        self.report("diff-fuzz/seed-99.json", {
            "seed": 99,
            "total": 3,
            "counts": {"pass": 1, "fail": 1, "oracle_error": 1},
            "cases": [
                {"index": 1, "case_seed": 21, "family": "if.matches", "mode": "script", "status": "pass"},
                {"index": 2, "case_seed": 22, "family": "file.glob", "mode": "script", "status": "fail",
                 "repro": "/tmp/repros/file.glob-deadbeef"},
                {"index": 3, "case_seed": 23, "family": "genex.list", "mode": "configure", "status": "oracle_error"},
            ],
        })
        self.report("diff-fuzz/seed-99.repros/case/outcomes.json", {"oracle": {}, "rcmake": {}})
        dir_result = self.build("--differential-fuzz", str(self.root / "diff-fuzz"))
        diff_dir = dir_result["tracks"]["benchmarksAndGates"]["differentialFuzz"]
        self.assertEqual(diff_dir["status"], "fail")
        self.assertEqual((diff_dir["casesTotal"], diff_dir["casesPassed"],
                          diff_dir["mismatches"], diff_dir["oracleErrors"]),
                         (7, 5, 1, 1))
        self.assertEqual(diff_dir["seeds"], [42, 99])
        self.assertEqual(diff_dir["reportsCount"], 2)
        self.assertNotIn("/tmp/repros", self.output.read_text(encoding="utf-8"))

        # Oracle error alone (zero mismatches) still fails differential fuzzing.
        oracle_only = self.report("diff-oracle-error.json", {
            "seed": 7,
            "total": 2,
            "counts": {"pass": 1, "fail": 0, "oracle_error": 1},
            "cases": [],
        })
        oracle_diff = self.build("--differential-fuzz", oracle_only)["tracks"]["benchmarksAndGates"]["differentialFuzz"]
        self.assertEqual(oracle_diff["status"], "fail")
        self.assertEqual((oracle_diff["mismatches"], oracle_diff["oracleErrors"]), (0, 1))

    def test_differential_fuzz_rejects_mismatched_counts_and_crash_logs(self):
        bad_counts = self.report("bad-diff-counts.json", {
            "seed": 1,
            "total": 2,
            "counts": {"pass": 2, "fail": 0, "oracle_error": 0},
            "cases": [
                {"index": 1, "family": "if.matches", "mode": "script", "status": "pass"},
                {"index": 2, "family": "if.matches", "mode": "script", "status": "fail"},
            ],
        })
        crash_log = self.report("crash-log.json", {"iterations": 500, "failures": 0})
        for path in (bad_counts, crash_log):
            with self.subTest(path=path):
                with redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as exit_code:
                    builder.main(["--out", str(self.output), "--differential-fuzz", path])
                self.assertEqual(exit_code.exception.code, 2)

    def test_holdout_parses_unseen_eval_json_and_blocks_all_repo_leakage(self):
        secret_dev = "dev-org/dev-secret-repo"
        secret_holdout = "sealed-org/sealed-secret-repo"
        secret_url = f"https://github.com/{secret_holdout}.git"
        path = self.report("unseen-eval.json", {
            "dev": {
                "pass_rates": {"L1": {"passed": 10, "eligible": 10, "rate": 1.0}},
                "failure_categories": {},
                "projects": [{"name": secret_dev, "url": f"https://github.com/{secret_dev}"}],
            },
            "holdout": {
                "pass_rates": {
                    "L1": {"passed": 238, "eligible": 240, "rate": 238 / 240},
                    "L2": {"passed": 235, "eligible": 237, "rate": 235 / 237},
                },
                "failure_categories": {
                    "configure": 2,
                    "generator": 2,
                    "oracle-red": 1,
                    secret_holdout: 1,
                    secret_url: 3,
                },
                "projects": [
                    {"name": secret_holdout, "url": secret_url, "levels": {"L1": "fail"}},
                ],
            },
        })
        track = self.build("--holdout", path)["tracks"]["unseenHoldout"]
        self.assertTrue(track["sealed"])
        # 2 failed at L1 + 2 failed at L2 = 4 failed; 235 passed L2 -> 235 / 239 eligible across L1+L2.
        self.assertEqual((track["reposPassed"], track["reposTotal"], track["status"]),
                         (235, 239, "fail"))
        self.assertEqual(track["passRate"], round(100 * 235 / 239, 2))
        self.assertEqual(track["levelPassRates"], {
            "L1": {"passed": 238, "eligible": 240, "passRate": 99.17},
            "L2": {"passed": 235, "eligible": 237, "passRate": 99.16},
        })
        self.assertEqual(track["failureCategories"], {
            "configure": 2,
            "generator": 2,
            "oracle-red": 1,
            "other": 4,
        })
        serialized = self.output.read_text(encoding="utf-8")
        for forbidden in (secret_dev, secret_holdout, secret_url, "sealed-org", "dev-org"):
            self.assertNotIn(forbidden, serialized)

        # Unwrapped pass_rates + top-level aggregate schema also work cleanly.
        unwrapped = self.report("unseen-holdout-only.json", {
            "pass_rates": {"L1": {"passed": 240, "eligible": 240, "rate": 1.0}},
            "failure_categories": {},
        })
        ok_track = self.build("--holdout", unwrapped)["tracks"]["unseenHoldout"]
        self.assertEqual((ok_track["reposPassed"], ok_track["reposTotal"], ok_track["status"]),
                         (240, 240, "pass"))

        # Dev-only report without a holdout split must be rejected.
        dev_only = self.report("dev-only.json", {
            "dev": {"pass_rates": {"L1": {"passed": 5, "eligible": 5, "rate": 1.0}}, "failure_categories": {}},
        })
        with redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as exit_code:
            builder.main(["--out", str(self.output), "--holdout", dev_only])
        self.assertEqual(exit_code.exception.code, 2)

    def test_large_projects_captures_l1_l2_l3_l4_from_local_and_ecosystem_reports(self):
        # Local runner format with full-mode lanes (build + test + manifest_l4).
        self.report("lp/local/duckdb/result.json", {
            "project": "duckdb",
            "mode": "full",
            "status": "pass",
            "l1": {"cache.json": "equal", "codemodel.json": "equal",
                   "edges.stdout": "equal", "diagnostics.txt": "equal"},
            "l2": "equal",
            "lanes": {
                "oracle": {"path": "pass", "configure": "pass", "identity": "pass",
                           "dry_run": "pass", "build": "pass", "test": "pass", "manifest_l4": "pass"},
                "rcmake": {"path": "pass", "configure": "pass", "identity": "pass",
                           "dry_run": "pass", "build": "pass", "test": "pass", "manifest_l4": "pass"},
            },
        })
        # Local runner format in parity mode (build/test/manifest_l4 not_requested -> pending).
        self.report("lp/local/opencv/result.json", {
            "project": "opencv",
            "mode": "parity",
            "status": "pass",
            "l1": {"cache.json": "equal"},
            "l2": "equal",
            "lanes": {
                "oracle": {"build": "not_requested", "test": "not_requested", "manifest_l4": "not_requested"},
                "rcmake": {"build": "not_requested", "test": "not_requested", "manifest_l4": "not_requested"},
            },
        })
        # Ecosystem CI format with explicit l1/l2/l3/l4 objects.
        self.report("lp/ci/large-projects-vtk/result.json", {
            "project": "vtk",
            "l1": {"diagnostics": "equal", "cache": "equal", "codemodel": "equal", "edges": "equal"},
            "l2": "equal",
            "l3": {"status": "pass", "targets": ["CommonCore", "FiltersCore"]},
            "l4": {"status": "fail", "checks": {"install": "pass", "ctest": "fail"}},
        })
        # Ecosystem CI format using levels dict.
        self.report("lp/ci/large-projects-qtbase/result.json", {
            "name": "qtbase",
            "levels": {"L1": "pass", "L2": "pass", "L3": "pass", "L4": "pass"},
        })
        track = self.build("--large-projects-dir", str(self.root / "lp"))["tracks"]["bossFight"]
        by_name = {row["name"]: row for row in track["projects"]}
        self.assertEqual((by_name["duckdb"]["l1"], by_name["duckdb"]["l2"],
                          by_name["duckdb"]["l3"], by_name["duckdb"]["l4"]),
                         ("pass", "pass", "pass", "pass"))
        self.assertEqual((by_name["opencv"]["l1"], by_name["opencv"]["l2"],
                          by_name["opencv"]["l3"], by_name["opencv"]["l4"]),
                         ("pass", "pass", "pending", "pending"))
        self.assertEqual((by_name["vtk"]["l1"], by_name["vtk"]["l2"],
                          by_name["vtk"]["l3"], by_name["vtk"]["l4"]),
                         ("pass", "pass", "pass", "fail"))
        self.assertEqual((by_name["qtbase"]["l1"], by_name["qtbase"]["l2"],
                          by_name["qtbase"]["l3"], by_name["qtbase"]["l4"]),
                         ("pass", "pass", "pass", "pass"))
        self.assertEqual((track["l1Passed"], track["l2Passed"], track["l3Passed"], track["l4Passed"]),
                         (4, 4, 3, 2))
        self.assertEqual(track["status"], "fail")

        # All 10 projects passing L1-L4 sets bossFight status to pass.
        all_ten = self.root / "lp-all"
        self.report("lp-all/result.json", {
            "projects": [
                {"project": row["name"], "l1": "pass", "l2": "pass", "l3": "pass", "l4": "pass"}
                for row in self.base["tracks"]["bossFight"]["projects"]
            ]
        })
        all_track = self.build("--large-projects-dir", str(all_ten))["tracks"]["bossFight"]
        self.assertEqual(all_track["status"], "pass")
        self.assertEqual((all_track["l1Passed"], all_track["l2Passed"],
                          all_track["l3Passed"], all_track["l4Passed"]),
                         (10, 10, 10, 10))

    def test_same_sha_integrity_guard_resets_stale_tracks_and_validates_digests(self):
        digest_a = "a" * 64
        digest_b = "b" * 64
        # Base has commit=null with pre-populated fuzz=pass and wasi=pass.
        # Updating with --release-sha + --binary-sha256 + --strict-same-sha resets untouched tracks.
        diff_report = self.report("same-sha-diff.json", {
            "commit": "1234567abcdef",
            "binary_sha256": digest_a,
            "seed": 42,
            "total": 10,
            "counts": {"pass": 10, "fail": 0, "oracle_error": 0},
        })
        first = self.build(
            "--release-sha", "1234567abcdef",
            "--binary-sha256", digest_a,
            "--strict-same-sha",
            "--differential-fuzz", diff_report,
        )
        self.assertEqual(first["commit"], "1234567abcdef")
        self.assertEqual(first["releaseSha"], "1234567abcdef")
        self.assertEqual(first["binarySha256"], digest_a)
        self.assertEqual(first["tracks"]["benchmarksAndGates"]["differentialFuzz"]["status"], "pass")
        # Stale fuzz and wasi passes from the commit=null base must be reset to pending.
        self.assertEqual(self.gate(first, "fuzz")["status"], "pending")
        self.assertIsNone(self.gate(first, "fuzz")["passed"])
        self.assertEqual(self.gate(first, "wasi")["status"], "pending")
        self.assertIsNone(self.gate(first, "wasi")["passed"])

        # Subsequent merge on the exact same SHA + binary digest preserves verified tracks.
        first_path = self.report("first-snap.json", first)
        wasm_ok = self.report("same-sha-wasm.json", {
            "commit": "1234567abcdef",
            "binary_sha256": digest_a,
            "status": "pass",
            "checks": [{"status": "pass"}],
        })
        second = self.build(
            "--base", first_path,
            "--release-sha", "1234567abcdef",
            "--binary-sha256", digest_a,
            "--strict-same-sha",
            "--wasm", wasm_ok,
        )
        self.assertEqual(second["tracks"]["benchmarksAndGates"]["differentialFuzz"]["status"], "pass")
        self.assertEqual(self.gate(second, "wasi")["status"], "pass")

        # Updating from a different commit resets untouched tracks unless --allow-stale-base is given.
        second_path = self.report("second-snap.json", second)
        moved = self.build("--base", second_path, "--release-sha", "fedcba7654321")
        self.assertEqual(moved["commit"], "fedcba7654321")
        self.assertNotIn("binarySha256", moved)
        self.assertNotIn("differentialFuzz", moved["tracks"]["benchmarksAndGates"])
        self.assertEqual(self.gate(moved, "wasi")["status"], "pending")

        # Explicit --allow-stale-base keeps untouched tracks when changing commits without --strict-same-sha.
        carried = self.build("--base", second_path, "--release-sha", "fedcba7654321", "--allow-stale-base")
        self.assertEqual(self.gate(carried, "wasi")["status"], "pass")
        self.assertNotIn("binarySha256", carried)

        # Changing binary digest on the same commit also resets stale tracks.
        rebuilt = self.build(
            "--base", second_path,
            "--release-sha", "1234567abcdef",
            "--binary-sha256", digest_b,
            "--strict-same-sha",
        )
        self.assertEqual(rebuilt["binarySha256"], digest_b)
        self.assertEqual(self.gate(rebuilt, "wasi")["status"], "pending")

        # Embedded report SHA or binary digest mismatch must fail without writing.
        wrong_sha = self.report("wrong-sha.json", {
            "commit": "deadbeef1234",
            "status": "pass",
            "checks": [{"status": "pass"}],
        })
        wrong_digest = self.report("wrong-digest.json", {
            "binary_sha256": digest_b,
            "status": "pass",
            "checks": [{"status": "pass"}],
        })
        for bad_args in (
            ["--strict-same-sha"],
            ["--release-sha", "1234567", "--strict-same-sha", "--allow-stale-base"],
            ["--release-sha", "not-a-sha"],
            ["--binary-sha256", "short"],
            ["--commit", "1234567", "--release-sha", "7654321"],
            ["--release-sha", "1234567abcdef", "--strict-same-sha", "--wasm", wrong_sha],
            ["--release-sha", "1234567abcdef", "--binary-sha256", digest_a, "--strict-same-sha", "--wasm", wrong_digest],
        ):
            with self.subTest(bad_args=bad_args):
                with redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as exit_code:
                    builder.main(["--out", str(self.output), *bad_args])
                self.assertEqual(exit_code.exception.code, 2)


if __name__ == "__main__":
    unittest.main()

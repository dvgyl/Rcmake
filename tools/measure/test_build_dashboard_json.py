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
        self.base = builder.read_json(builder.DATA / "latest.json")
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
                         ["pass", "pending", "pending"])
        self.assertEqual(tracks["coreConformance"]["status"], "partial")
        self.assertEqual(tracks["policies"], self.base["tracks"]["policies"])

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
             "rcmake": {"ok": True, "peak_rss_mib": 88.2}},
        ]})
        section = self.build("--bench", configure, "--bench", legacy)["tracks"]["benchmarksAndGates"]
        self.assertEqual({key: (row["value"], row["status"]) for key, row in section["benchmarks"].items()},
                         {"warmProbeSpeedup": (6, "fail"), "incrementalLeafP95Ms": (63, "pass"),
                          "coldConfigureSpeedup": (1.5, "pass"), "peakRssMiB": (88.2, "pass")})
        partial = self.build("--bench", configure)["tracks"]["benchmarksAndGates"]["benchmarks"]
        self.assertEqual(partial["peakRssMiB"]["status"], "pending")

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


if __name__ == "__main__":
    unittest.main()

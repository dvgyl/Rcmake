# SPDX-License-Identifier: MIT OR Apache-2.0
"""Test the release pipeline with temporary receipts and no native builds."""

from contextlib import redirect_stderr, redirect_stdout
from hashlib import sha256
import io
import json
from pathlib import Path
import shutil
import subprocess
import sys
from tempfile import TemporaryDirectory
import unittest
from unittest.mock import patch

from tools.measure import bench_giants, build_dashboard_json, stamp_metrics, verify_release_receipts
from tools.measure.runcmake_eval import SUITE_GROUPS


ROOT = Path(__file__).resolve().parents[2]
SCHEMA = ROOT / "site/data/dashboard.schema.json"
COMMIT = "0123456789abcdef0123456789abcdef01234567"
STALE = "fedcba9876543210fedcba9876543210fedcba98"
BINARY = b"mock frozen rcmake binary"
DIGEST = sha256(BINARY).hexdigest()


def read_json(path):
    return json.loads(path.read_text(encoding="utf-8"))


def write_json(path, payload):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(payload), encoding="utf-8")
    return path


def measure_giant(spec, **_kwargs):
    def phase(cmake_ms, rcmake_ms):
        cmake = bench_giants.CommandMeasurement(cmake_ms, 160 * bench_giants.MIB, 0)
        rcmake = bench_giants.CommandMeasurement(rcmake_ms, 118 * bench_giants.MIB, 0)
        return {
            "cmake": bench_giants.summarize_measurements([cmake]),
            "rcmake": bench_giants.summarize_measurements([rcmake]),
            "speedup": cmake_ms / rcmake_ms,
        }

    warm = phase(1000, 200)
    warm["probe_cache"] = {"hits": 8, "misses": 0}
    leaf = phase(100, 40)
    leaf["rcmake"]["p95_ms"] = 42
    leaf["leaf_file"] = "src/CMakeLists.txt"
    return {
        "name": spec.name, "repository": spec.repository, "version": spec.version,
        "status": "pass", "cold": phase(1200, 600), "warm_probe": warm,
        "incremental_leaf": leaf, "build_verify": None,
    }


class ReleasePipelineTests(unittest.TestCase):
    def setUp(self):
        temporary = TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.work = Path(temporary.name)
        self.receipts = self.work / "receipts"
        self.dashboard = self.work / "latest.json"
        self.findings = self.work / "findings.json"
        self.base = read_json(ROOT / "site/data/latest.json")
        self.entries = {}
        self.build_args = []
        self.builder_members = {}
        self.make_receipts()

    def report(self, key, payload, flag=None):
        path = self.receipts / (key.replace("_", "-") + ".json")
        self.entries[key] = {"path": path.name}
        if flag:
            self.build_args.extend((flag, path))
            self.builder_members[key] = path
        return self.write_receipt(path, payload)

    def write_receipt(self, path, payload):
        return write_json(path, {"release_commit": COMMIT, "binary_sha256": DIGEST, **payload})

    def directory(self, key, flag=None):
        path = self.receipts / key.replace("_", "-")
        self.entries[key] = {"path": path.name}
        if flag:
            self.build_args.extend((flag, path))
        return path

    def make_receipts(self):
        policy_suites = [f"CMP{i:04d}" for i in range(66, 99)] + ["CMP0223"]
        common = list(dict.fromkeys([*SUITE_GROUPS["all"], *policy_suites]))
        for os_name, total in (("macos", 371), ("linux", 362), ("windows", 367)):
            suites = common + [f"MockSuite{i:03d}" for i in range(total - len(common))]
            self.report(f"runcmake_{os_name}", {
                "suites": {name: {"passed": 5, "total": 5} for name in suites},
            }, f"--runcmake-{os_name}")

        self.report("independent_scenarios", {
            "mode": "differential",
            "scenarios": [
                {"name": f"scenario-{i}", "status": "pass",
                 "runs": {"rcmake": {"checks": [{"ok": True}] * (34 if i < 19 else 33)}}}
                for i in range(20)
            ],
        }, "--independent")
        projects = self.directory("large_projects", "--large-projects-dir")
        for project in self.base["tracks"]["bossFight"]["projects"]:
            name = project["name"]
            self.builder_members["large_projects"] = self.write_receipt(projects / name / "result.json", {
                "project": name, "status": "pass", "l1": {"cache.json": "equal"}, "l2": "equal",
                "levels": {"L3": {"status": "pass"}, "L4": {"checks": {"test": "pass", "install": "pass"}}},
            })

        shards = self.directory("vcpkg", "--vcpkg-dir")
        for index in range(64):
            path = self.write_receipt(shards / str(index) / "shard.json", {
                "shard_index": index, "shard_count": 64, "expected_ports": 40, "duration_seconds": 12.5,
            })
            results = (json.dumps({"port": f"port-{index}-{port}", "status": "pass"}) for port in range(40))
            path.with_name("results.jsonl").write_text("\n".join(results) + "\n", encoding="utf-8")
            self.builder_members["vcpkg"] = path

        self.report("conan", {
            "status": "pass", "scenarios": [
                {"name": name, "status": "pass"}
                for name in ("recursive-driver", "conan-consumer", "conan-source-build", "ide-file-api")
            ],
        }, "--conan")
        self.report("unseen_holdout", {
            "holdout": {
                "pass_rates": {"L1": {"eligible": 240, "passed": 239}, "L2": {"eligible": 239, "passed": 238}},
                "failure_categories": {"configure": 1, "generator": 1},
            },
        }, "--unseen-holdout")
        self.make_benchmarks()

        self.report("probe_cache", {
            "mode": "gate", "passed": True,
            "cases": [{"case": f"invalidation-{i}", "status": "passed", "passed": True} for i in range(16)],
        }, "--probe-cache")
        self.report("mutation_replay", {
            "mode": "gate", "passed": True, "replay_verified": True,
            "steps": [{"step": i, "passed": True} for i in range(21)],
        }, "--mutation")
        fuzz = self.directory("fuzz", "--fuzz-dir")
        for i in range(14):
            self.builder_members["fuzz"] = self.write_receipt(fuzz / f"target-{i}.json", {
                "target": f"target-{i}", "iterations": 1_000_000, "failures": [],
            })
        self.report("differential_fuzz", {
            "seed": 42, "total": 1000, "counts": {"pass": 1000, "fail": 0, "oracle_error": 0},
            "cases": [{"status": "pass", "family": "math.expr", "mode": "script"} for _ in range(1000)],
        }, "--differential-fuzz")
        self.report("wasi", {
            "status": "pass", "checks": [{"check": f"wasm-{i}", "status": "pass"} for i in range(6)],
        }, "--wasm")
        self.report("cli_ux", {"status": "pass", "passed": 12, "total": 12})
        self.report("path_swap", {"status": "pass", "differences": []}, "--path-swap")
        self.report("ci_jobs", {
            "event": "push",
            "run_conclusion": "success",
            "workflow_runs": [
                {"name": name, "status": "completed", "conclusion": "success", "head_sha": COMMIT}
                for name in verify_release_receipts.REQUIRED_CI_JOBS
            ],
        }, "--ci-status-json")

        archive = self.receipts / "rcmake-0.1.0-aarch64-apple-darwin.tar.gz"
        archive.write_bytes(b"mock release archive")
        checksum = sha256(archive.read_bytes()).hexdigest()
        checksum_file = archive.with_suffix(".gz.sha256")
        checksum_file.write_text(f"{checksum}  {archive.name}\n", encoding="ascii")
        self.report("dist", {
            "tarball": archive.name, "tarball_path": archive.name, "tarball_sha256": checksum,
            "sha256_path": checksum_file.name, "binary_size_bytes": 15_728_640,
        }, "--dist")
        wasm = self.work / "rcmake_wasm.wasm"
        wasm.write_bytes(b"\x00asm\x01\x00\x00\x00\x01\x04\x01\x60\x00\x00")
        wasm.with_suffix(".js").write_text("export function configure() {}\n", encoding="utf-8")
        self.manifest = write_json(self.receipts / "manifest.json", {
            "schema_version": 1, "release_commit": COMMIT, "binary_sha256": DIGEST,
            "oracle_version": "4.5.1", "dashboard": str(self.dashboard), "wasm_bundle": str(wasm),
            "wasm_sha256": sha256(wasm.read_bytes()).hexdigest(), "receipts": self.entries,
        })

    def make_benchmarks(self):
        giants = self.receipts / "bench-giants.json"
        self.entries["benchmarks_giants"] = {"path": giants.name}
        self.build_args.extend(("--giants-benchmark", giants))
        self.builder_members["benchmarks_giants"] = giants
        rcmake = self.work / "rcmake"
        rcmake.write_bytes(BINARY)
        with (
            patch.object(bench_giants, "find_rcmake", return_value=rcmake),
            patch.object(bench_giants, "find_oracle", return_value=self.work / "cmake"),
            patch.object(bench_giants, "read_tool_version", side_effect=["cmake version 4.5.1", "rcmake 0.1.0"]),
            patch.object(bench_giants, "benchmark_project", side_effect=measure_giant),
            redirect_stdout(io.StringIO()),
            redirect_stderr(io.StringIO()),
        ):
            code = bench_giants.main([
                "--quick", "--json", str(giants), "--rcmake-sha", COMMIT, "--binary-sha256", DIGEST,
            ])
        self.assertEqual(code, 0)
        self.assertEqual(read_json(giants)["binary_sha256"], DIGEST)

        synthetic = self.directory("benchmarks_synthetic")
        cold = self.write_receipt(synthetic / "bench-eval.json", {"results": [{
            "name": "configure 500 targets (cold)", "speedup": 2.15,
            "notes": "new build tree, empty probe cache, includes compiler checks",
            "rcmake": {"ok": True, "peak_rss_mib": 74.5}, "oracle": {"ok": True},
        }]})
        configure = self.write_receipt(synthetic / "bench-configure.json", {"results": [
            {"project": "synthetic", "case": "warm probe-cache configure", "speedup": 5.4,
             "gate": {"status": "pass", "passed": True}, "rcmake": {"failures": 0}},
            {"project": "synthetic", "case": "incremental-leaf", "speedup": 2.2,
             "gate": {"status": "pass", "passed": True}, "rcmake": {"p95_ms": 38.2, "failures": 0}},
        ]})
        self.build_args.extend(("--bench", cold, "--bench", configure))
        self.builder_members.update(bench_eval=cold, bench_configure=configure)

    def cli(self, script, *args):
        return subprocess.run(
            [sys.executable, str(ROOT / "tools/measure" / script), *map(str, args)],
            cwd=self.work, capture_output=True, text=True, timeout=30,
        )

    def build(self):
        return self.cli(
            "build_dashboard_json.py", "--strict-same-sha", "--release-sha", COMMIT,
            "--binary-sha256", DIGEST, "--version", "0.1.0", "--out", self.dashboard,
            "--known-parity-gaps", *self.build_args,
        )

    def verify(self):
        return self.cli(
            "verify_release_receipts.py", "--manifest", self.manifest, "--dashboard", self.dashboard,
            "--expected-commit", COMMIT, "--expected-binary-sha256", DIGEST, "--json-out", self.findings,
        )

    def build_snapshot(self):
        result = self.build()
        self.assertEqual(result.returncode, 0, result.stderr)
        return read_json(self.dashboard)

    def test_receipts_build_verify_and_stamp_all_markers(self):
        snapshot = self.build_snapshot()
        schema = read_json(SCHEMA)
        build_dashboard_json.validate(snapshot, schema, schema)
        self.assertEqual(snapshot["commit"], COMMIT)
        self.assertEqual(snapshot["binarySha256"], DIGEST)

        tracks = snapshot["tracks"]
        self.assertEqual(tracks["coreConformance"]["suitesAt100"], 62)
        self.assertEqual(tracks["policies"]["suitesPassed"], 35)
        self.assertEqual([row["suitesPassed"] for row in tracks["runCmakeOs"]["platforms"]],
                         [371, 362, 367])
        self.assertEqual(tracks["vcpkgMatrix"]["shardsPassed"], 64)
        giants = tracks["benchmarksAndGates"]["giantsBenchmark"]
        self.assertEqual(giants["summary"]["projectsPassed"], 2)
        self.assertEqual(giants["summary"]["warmProbeSpeedupGeomean"], 5)
        self.assertEqual(giants["projects"][0]["incrementalLeaf"]["rcmakeP95Ms"], 42)
        self.assertEqual(tracks["benchmarksAndGates"]["differentialFuzz"]["casesPassed"], 1000)
        self.assertEqual([tracks["bossFight"][f"l{i}Passed"] for i in range(1, 5)], [10] * 4)
        self.assertEqual(tracks["unseenHoldout"]["reposTotal"], 240)
        self.assertEqual(tracks["unseenHoldout"]["status"], "pass")

        result = self.verify()
        self.assertEqual(result.returncode, 0, result.stderr)
        verified = read_json(self.findings)
        self.assertEqual(verified["findings"], [])
        self.assertEqual(set(verified["checked_receipts"]), set(self.entries))

        args = ["--dashboard", self.dashboard]
        files = {}
        for relative, expected_count in (("README.md", 19), ("docs/BUILD_WRITEUP.md", 25)):
            destination = self.work / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(ROOT / relative, destination)
            files[destination] = expected_count
            args.extend(("--file", destination))

        for mode in ("--write", "--check"):
            result = self.cli("stamp_metrics.py", mode, *args)
            self.assertEqual(result.returncode, 0, result.stderr)
        result = self.cli("stamp_metrics.py", "--check", "--receipts", self.manifest, *args)
        self.assertEqual(result.returncode, 0, result.stderr)

        resolved = {}
        for path, expected_count in files.items():
            markers = stamp_metrics.scan_markers(path.read_text(encoding="utf-8"), path.name)
            self.assertEqual(len(markers), expected_count)
            for marker in markers:
                self.assertTrue(marker.value, marker.key)
                self.assertNotRegex(marker.value, stamp_metrics.PENDING_VALUE_RE)
                resolved[marker.key] = marker.value
        self.assertEqual(resolved.keys(), stamp_metrics.EXPECTED_KEYS)
        self.assertEqual(resolved["conan_ecosystem"], "4 / 4 scenarios passed")
        self.assertEqual(resolved["path_swap_eval"], "pass (0 differences)")
        self.assertEqual(resolved["binary_size_macos"], "15.00 MiB (15,728,640 bytes)")
        self.assertEqual(resolved["unseen_holdout"], "99.17% (238 / 240 repositories)")
        self.assertEqual(resolved["independent_scenarios"], "20 / 20 scenarios (679 / 679 checks passed)")
        self.assertEqual(resolved["fuzz_failures"], "14,000,000 inputs, 0 failures")
        self.assertEqual(resolved["wasm_smoke"], "6 of 6 checks passed")

    def test_builder_rejects_stale_commit_on_each_input(self):
        for lane, path in self.builder_members.items():
            with self.subTest(lane=lane):
                original = path.read_bytes()
                try:
                    payload = read_json(path)
                    payload["release_commit"] = STALE
                    write_json(path, payload)
                    result = self.build()
                    self.assertEqual(result.returncode, 2, result.stderr)
                    self.assertIn(f"reports commit {STALE!r}", result.stderr)
                    self.assertFalse(self.dashboard.exists())
                finally:
                    path.write_bytes(original)

    def test_builder_rejects_missing_commit_and_stale_binary(self):
        path = self.builder_members["wasi"]
        payload = read_json(path)
        del payload["release_commit"]
        write_json(path, payload)
        result = self.build()
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertIn("is missing a release commit", result.stderr)

        payload.update(release_commit=COMMIT, binary_sha256="f" * 64)
        write_json(path, payload)
        result = self.build()
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertIn("reports binary SHA-256", result.stderr)
        self.assertFalse(self.dashboard.exists())

        write_json(path, {**payload, "binary_sha256": DIGEST})
        ci_path = self.builder_members["ci_jobs"]
        ci = read_json(ci_path)
        ci["workflow_runs"][0]["head_sha"] = STALE
        write_json(ci_path, ci)
        result = self.build()
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertIn(f"run 0 reports commit {STALE!r}", result.stderr)

    def test_verifier_rejects_stale_member_and_redacts_holdout_names(self):
        self.build_snapshot()
        path = self.builder_members["vcpkg"]
        original = read_json(path)
        write_json(path, {**original, "release_commit": STALE})
        result = self.verify()
        self.assertEqual(result.returncode, 1, result.stderr)
        findings = read_json(self.findings)["findings"]
        self.assertEqual([(row["receipt"], row["kind"]) for row in findings], [("vcpkg:63/shard.json", "stale")])
        readme = self.work / "README.md"
        shutil.copyfile(ROOT / "README.md", readme)
        result = self.cli("stamp_metrics.py", "--write", "--dashboard", self.dashboard,
                          "--receipts", self.manifest, "--file", readme)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("vcpkg:63/shard.json", result.stderr)
        self.assertEqual(readme.read_bytes(), (ROOT / "README.md").read_bytes())
        write_json(path, original)

        secret = "mock-private-org/mock-private-repository"
        path = self.builder_members["unseen_holdout"]
        payload = read_json(path)
        payload["holdout"]["failure_categories"][secret] = 1
        write_json(path, payload)
        self.build_snapshot()
        self.assertNotIn(secret, self.dashboard.read_text(encoding="utf-8"))
        result = self.verify()
        self.assertEqual(result.returncode, 1, result.stderr)
        findings = read_json(self.findings)["findings"]
        self.assertEqual([(row["receipt"], row["kind"]) for row in findings], [("unseen_holdout", "leak")])
        self.assertNotIn(secret, result.stderr + self.findings.read_text(encoding="utf-8"))

    @unittest.skipUnless(shutil.which("node"), "Node.js is not available.")
    def test_site_controllers_render_the_generated_snapshot(self):
        self.build_snapshot()
        site = self.work / "site"
        site.mkdir()
        write_json(site / "package.json", {"type": "module"})
        for filename in ("app.js", "charts.js", "dashboard.js", "index.js", "motion.js", "pipeline.js"):
            shutil.copyfile(ROOT / "site" / filename, site / filename)

        program = r"""
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";

const snapshot = JSON.parse(readFileSync(process.argv[2], "utf8"));
const missing = new Set();
const observed = new Set();
const data = new Proxy(snapshot, {
  get(target, key, receiver) {
    if (typeof key === "string" && key !== "then") {
      observed.add(key);
      if (!Object.prototype.hasOwnProperty.call(target, key)) missing.add(key);
    }
    return Reflect.get(target, key, receiver);
  }
});
const elements = new Map(
  ["report", "report-meta", "report-note", "waffle-panel", "gauge-grid", "speed-panel", "home-load-error"]
    .map(id => [id, { querySelector: () => null, querySelectorAll: () => [] }])
);
globalThis.document = {
  documentElement: { dataset: {} },
  getElementById: id => elements.get(id),
  querySelector: () => null,
  querySelectorAll: () => []
};
globalThis.fetch = async () => ({ ok: true, json: async () => data });
await import("./dashboard.js");
await import("./index.js");
assert.deepEqual([...missing], []);
assert.ok(observed.has("tracks"));
const report = elements.get("report").innerHTML;
const home = elements.get("speed-panel").innerHTML;
assert.match(report, /data-test="dash-giants-bench"/);
assert.match(home, /data-test="home-giants-bench"/);
for (const html of [report, home]) {
  assert.match(html, /duckdb/);
  assert.match(html, /5\.00×/);
  assert.match(html, /42 ms/);
  assert.match(html, /118 \/ 160 MiB/);
}
assert.equal(elements.get("home-load-error").textContent, undefined);
"""
        result = subprocess.run(
            ["node", "--input-type=module", "-", str(self.dashboard)],
            cwd=site, input=program, capture_output=True, text=True, timeout=30,
        )
        self.assertEqual(result.returncode, 0, result.stderr)


if __name__ == "__main__":
    unittest.main()

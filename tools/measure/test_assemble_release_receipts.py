# SPDX-License-Identifier: MIT OR Apache-2.0
"""Unit tests for tools/measure/assemble_release_receipts.py (zero Cargo builds)."""

from __future__ import annotations

from hashlib import sha256
import io
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
from tempfile import TemporaryDirectory
import unittest

from tools.measure import assemble_release_receipts as assembler
from tools.measure import build_dashboard_json as builder
from tools.measure import stamp_metrics as stamper
from tools.measure import verify_release_receipts as verifier
from tools.measure.runcmake_eval import SUITE_GROUPS


ROOT = Path(__file__).resolve().parents[2]
ASSEMBLER_SCRIPT = ROOT / "tools" / "measure" / "assemble_release_receipts.py"
VERIFIER_SCRIPT = ROOT / "tools" / "measure" / "verify_release_receipts.py"
STAMPER_SCRIPT = ROOT / "tools" / "measure" / "stamp_metrics.py"
SCHEMA_PATH = ROOT / "site" / "data" / "dashboard.schema.json"

COMMIT = "0123456789abcdef0123456789abcdef01234567"
STALE = "fedcba9876543210fedcba9876543210fedcba98"
BINARY_BYTES = b"mock frozen rcmake release binary"
DIGEST = sha256(BINARY_BYTES).hexdigest()
STALE_DIGEST = "b" * 64


def read_json(path: Path) -> dict:
    return json.loads(path.read_text(encoding="utf-8"))


def write_json(path: Path, payload: dict) -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
    return path


def make_runcmake_payload(total_suites: int, commit: str = COMMIT, binary: str = DIGEST) -> dict:
    policy_suites = [f"CMP{i:04d}" for i in range(66, 99)] + ["CMP0223"]
    common = list(dict.fromkeys([*SUITE_GROUPS["all"], *policy_suites]))
    suites = common + [f"MockSuite{i:03d}" for i in range(total_suites - len(common))]
    return {
        "release_commit": commit,
        "binary_sha256": binary,
        "suites": {name: {"passed": 5, "total": 5} for name in suites},
    }


def make_giants_payload(commit: str = COMMIT, binary: str = DIGEST) -> dict:
    def phase(cmake_ms: float, rcmake_ms: float) -> dict:
        return {
            "cmake": {"succeeded": 3, "failed": 0, "median_ms": cmake_ms, "peak_rss_mib": 160.0},
            "rcmake": {"succeeded": 3, "failed": 0, "median_ms": rcmake_ms, "peak_rss_mib": 118.0},
            "speedup": round(cmake_ms / rcmake_ms, 2),
        }

    warm = phase(1000.0, 200.0)
    warm["probe_cache"] = {"hits": 8, "misses": 0}
    leaf = phase(100.0, 40.0)
    leaf["rcmake"]["p95_ms"] = 42.0
    leaf["leaf_file"] = "src/CMakeLists.txt"

    return {
        "schema_version": 1,
        "release_commit": commit,
        "binary_sha256": binary,
        "generated_at": "2026-11-26T00:00:00Z",
        "meta": {"runs": 3},
        "reproducer": "python3 tools/measure/bench_giants.py --quick",
        "projects": [
            {
                "name": "duckdb",
                "repository": "duckdb/duckdb",
                "version": "v1.3.2",
                "status": "pass",
                "cold": phase(1200.0, 600.0),
                "warm_probe": warm,
                "incremental_leaf": leaf,
                "build_verify": None,
            },
            {
                "name": "opencv",
                "repository": "opencv/opencv",
                "version": "4.12.0",
                "status": "pass",
                "cold": phase(1400.0, 700.0),
                "warm_probe": warm,
                "incremental_leaf": leaf,
                "build_verify": None,
            },
        ],
    }


def make_synthetic_results() -> tuple[list[dict], list[dict]]:
    cold_rows = [
        {
            "name": "configure 500 targets (cold)",
            "notes": "new build tree, empty probe cache, includes compiler checks",
            "speedup": 2.15,
            "rcmake": {"ok": True, "failures": 0, "peak_rss_mib": 74.5},
            "oracle": {"ok": True, "failures": 0},
        }
    ]
    configure_rows = [
        {
            "project": "synthetic",
            "case": "warm probe-cache configure",
            "speedup": 5.4,
            "gate": {"status": "pass", "passed": True},
            "rcmake": {"ok": True, "failures": 0},
        },
        {
            "project": "synthetic",
            "case": "incremental-leaf",
            "speedup": 2.2,
            "gate": {"status": "pass", "passed": True},
            "rcmake": {"ok": True, "p95_ms": 38.2, "failures": 0},
        },
    ]
    return cold_rows, configure_rows


class AssembleReleaseReceiptsTests(unittest.TestCase):
    def setUp(self) -> None:
        tmp = TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.work = Path(tmp.name)
        self.receipts = self.work / "receipts"
        self.receipts.mkdir()
        self.out_json = self.work / "out" / "latest.json"
        self.manifest_out = self.work / "out" / "manifest.json"
        self.findings_out = self.work / "out" / "findings.json"

        self.readme = self.work / "docs_copy" / "README.md"
        self.writeup = self.work / "docs_copy" / "BUILD_WRITEUP.md"
        self.readme.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(ROOT / "README.md", self.readme)
        shutil.copyfile(ROOT / "docs" / "BUILD_WRITEUP.md", self.writeup)

        self.wasm_bundle = self.work / "wasm" / "rcmake_wasm.wasm"
        self.wasm_bundle.parent.mkdir(parents=True, exist_ok=True)
        self.wasm_bundle.write_bytes(b"\x00asm\x01\x00\x00\x00\x01\x04\x01\x60\x00\x00")
        self.wasm_bundle.with_suffix(".js").write_text("export function configure() {}\n", encoding="utf-8")

        base = read_json(ROOT / "site" / "data" / "latest.json")
        self.boss_project_names = [row["name"] for row in base["tracks"]["bossFight"]["projects"]]

    def run_script(self, script: Path, *args: object) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [sys.executable, str(script), *(str(arg) for arg in args)],
            cwd=self.work,
            capture_output=True,
            text=True,
            timeout=60,
            check=False,
        )

    def populate_tarball(self, target_dir: Path) -> tuple[str, str, str]:
        archive = target_dir / "rcmake-0.1.0-aarch64-apple-darwin.tar.gz"
        with tarfile.open(archive, "w:gz") as tar:
            info = tarfile.TarInfo(name="rcmake-0.1.0-aarch64-apple-darwin/bin/rcmake")
            info.size = len(BINARY_BYTES)
            info.mode = 0o755
            tar.addfile(info, io.BytesIO(BINARY_BYTES))
        tarball_sha = sha256(archive.read_bytes()).hexdigest()
        sha_file = archive.with_suffix(".gz.sha256")
        sha_file.write_text(f"{tarball_sha}  {archive.name}\n", encoding="ascii")
        return archive.name, tarball_sha, sha_file.name

    def populate_complete_directory_layout(self, target_dir: Path, commit: str = COMMIT, binary: str = DIGEST) -> None:
        """Create all 19 required release receipts using canonical names and nested dirs."""
        ident = {"release_commit": commit, "binary_sha256": binary}
        for os_name, count in (("macos", 371), ("linux", 362), ("windows", 367)):
            write_json(target_dir / f"runcmake-{os_name}.json", make_runcmake_payload(count, commit, binary))

        write_json(
            target_dir / "independent-scenarios.json",
            {
                **ident,
                "mode": "differential",
                "scenarios": [
                    {
                        "name": f"scenario-{i}",
                        "status": "pass",
                        "runs": {"rcmake": {"checks": [{"ok": True}] * (34 if i < 19 else 33)}},
                    }
                    for i in range(20)
                ],
            },
        )

        for name in self.boss_project_names:
            write_json(
                target_dir / "large-projects" / name / "result.json",
                {
                    **ident,
                    "project": name,
                    "status": "pass",
                    "l1": {"cache.json": "equal"},
                    "l2": "equal",
                    "levels": {"L3": {"status": "pass"}, "L4": {"checks": {"test": "pass", "install": "pass"}}},
                },
            )

        for index in range(64):
            shard_path = write_json(
                target_dir / "vcpkg" / str(index) / "shard.json",
                {
                    **ident,
                    "shard_index": index,
                    "shard_count": 64,
                    "expected_ports": 40,
                    "duration_seconds": 12.5,
                },
            )
            statuses = ["pass"] * 38 + ["oracle-blocked-before-configure", "no-observed-cmake-configure"]
            rows = (json.dumps({"port": f"port-{index}-{p}", "status": status})
                    for p, status in enumerate(statuses))
            shard_path.with_name("results.jsonl").write_text("\n".join(rows) + "\n", encoding="utf-8")

        write_json(
            target_dir / "conan.json",
            {
                **ident,
                "status": "pass",
                "scenarios": [
                    {"name": name, "status": "pass"}
                    for name in ("recursive-driver", "conan-consumer", "conan-source-build", "ide-file-api")
                ],
            },
        )
        write_json(
            target_dir / "unseen-holdout.json",
            {
                **ident,
                "holdout": {
                    "pass_rates": {
                        "L1": {"eligible": 240, "passed": 239},
                        "L2": {"eligible": 239, "passed": 238},
                    },
                    "failure_categories": {"configure": 1, "generator": 1},
                },
            },
        )
        write_json(target_dir / "bench-giants.json", make_giants_payload(commit, binary))

        cold_rows, configure_rows = make_synthetic_results()
        write_json(target_dir / "benchmarks-synthetic" / "bench-eval.json", {**ident, "results": cold_rows})
        write_json(target_dir / "benchmarks-synthetic" / "bench-configure.json", {**ident, "results": configure_rows})

        write_json(
            target_dir / "probe-cache.json",
            {
                **ident,
                "mode": "gate",
                "passed": True,
                "cases": [{"case": f"invalidation-{i}", "status": "passed", "passed": True} for i in range(16)],
            },
        )
        write_json(
            target_dir / "mutation-replay.json",
            {
                **ident,
                "mode": "gate",
                "passed": True,
                "replay_verified": True,
                "steps": [{"step": i, "passed": True} for i in range(21)],
            },
        )
        for index in range(14):
            write_json(
                target_dir / "fuzz" / f"target-{index}.json",
                {**ident, "target": f"target-{index}", "iterations": 1_000_000, "failures": []},
            )
        write_json(
            target_dir / "differential-fuzz.json",
            {
                **ident,
                "seed": 42,
                "total": 1000,
                "counts": {"pass": 1000, "fail": 0, "oracle_error": 0},
                "cases": [{"status": "pass", "family": "math.expr", "mode": "script"} for _ in range(1000)],
            },
        )
        write_json(
            target_dir / "wasi.json",
            {**ident, "status": "pass", "checks": [{"check": f"wasm-{i}", "status": "pass"} for i in range(6)]},
        )
        write_json(target_dir / "cli-ux.json", {**ident, "status": "pass", "passed": 12, "total": 12})
        write_json(target_dir / "path-swap.json", {**ident, "status": "pass", "differences": []})
        write_json(
            target_dir / "ci-jobs.json",
            {
                **ident,
                "event": "push",
                "run_conclusion": "success",
                "workflow_runs": [
                    {"name": name, "status": "completed", "conclusion": "success", "head_sha": commit}
                    for name in verifier.REQUIRED_CI_JOBS
                ],
            },
        )
        tarball_name, tarball_sha, sha_name = self.populate_tarball(target_dir)
        write_json(
            target_dir / "dist.json",
            {
                **ident,
                "tarball": tarball_name,
                "tarball_path": tarball_name,
                "tarball_sha256": tarball_sha,
                "sha256_path": sha_name,
                "binary_size_bytes": 15_728_640,
            },
        )

    def populate_flat_alias_layout(self, target_dir: Path, commit: str = COMMIT, binary: str = DIGEST) -> None:
        """Create all 19 required receipts + optional determinism & ide using flat short alias names."""
        ident = {"release_commit": commit, "binary_sha256": binary}
        for os_name, count in (("macos", 371), ("linux", 362), ("windows", 367)):
            write_json(target_dir / f"runcmake-{os_name}.json", make_runcmake_payload(count, commit, binary))

        write_json(
            target_dir / "blackbox.json",
            {
                **ident,
                "mode": "differential",
                "scenarios": [
                    {
                        "name": f"scenario-{i}",
                        "status": "pass",
                        "runs": {"rcmake": {"checks": [{"ok": True}] * 34}},
                    }
                    for i in range(20)
                ],
            },
        )
        write_json(
            target_dir / "large-projects.json",
            {
                **ident,
                "projects": [
                    {
                        "project": name,
                        "status": "pass",
                        "l1": {"cache.json": "equal"},
                        "l2": "equal",
                        "levels": {"L3": {"status": "pass"}, "L4": {"status": "pass"}},
                    }
                    for name in self.boss_project_names
                ],
            },
        )
        write_json(
            target_dir / "vcpkg.json",
            {
                **ident,
                "shards_total": 64,
                "shards_completed": 64,
                "shards_passed": 64,
                "ports_total": 2560,
                "ports_passed": 2560,
                "ports_failed": 0,
                "shards": [
                    {
                        "shard": i,
                        "shard_count": 64,
                        "status": "pass",
                        "ports_total": 40,
                        "ports_passed": 40,
                        "ports_failed": 0,
                        "duration_seconds": 10.0,
                    }
                    for i in range(64)
                ],
            },
        )
        write_json(
            target_dir / "conan.json",
            {
                **ident,
                "status": "pass",
                "scenarios": [
                    {"name": "conan-consumer", "status": "pass"},
                    {"name": "conan-source-build", "status": "pass"},
                ],
            },
        )
        write_json(
            target_dir / "holdout.json",
            {
                **ident,
                "sealed": True,
                "status": "pass",
                "aggregate": {"reposTotal": 240, "reposPassed": 238},
                "categories": [{"category": "Core C/C++ Libraries", "reposTotal": 240, "reposPassed": 238}],
            },
        )
        write_json(target_dir / "bench-giants.json", make_giants_payload(commit, binary))

        cold_rows, configure_rows = make_synthetic_results()
        write_json(target_dir / "bench-configure.json", {**ident, "results": configure_rows})
        write_json(target_dir / "bench-eval.json", {**ident, "results": cold_rows})

        write_json(
            target_dir / "probe-cache.json",
            {
                **ident,
                "mode": "gate",
                "passed": True,
                "cases": [{"case": f"c{i}", "status": "passed", "passed": True} for i in range(16)],
            },
        )
        write_json(
            target_dir / "mutation.json",
            {
                **ident,
                "mode": "gate",
                "passed": True,
                "replay_verified": True,
                "steps": [{"step": i, "passed": True} for i in range(21)],
            },
        )
        write_json(
            target_dir / "fuzz.json",
            {**ident, "iterations": 14_000_000, "failures": 0, "targets": [f"t{i}" for i in range(14)]},
        )
        write_json(
            target_dir / "diff-fuzz.json",
            {**ident, "seed": 42, "total": 500, "counts": {"pass": 500, "fail": 0, "oracle_error": 0}},
        )
        write_json(
            target_dir / "wasm.json",
            {**ident, "status": "pass", "checks": [{"check": f"w{i}", "status": "pass"} for i in range(6)]},
        )
        write_json(target_dir / "cli-ux.json", {**ident, "status": "pass", "passed": 10, "total": 10})
        write_json(target_dir / "path-swap.json", {**ident, "status": "pass", "differences": []})
        write_json(
            target_dir / "ci-status.json",
            {
                **ident,
                "event": "workflow_dispatch",
                "run_conclusion": "success",
                "workflow_runs": [
                    {"name": name, "status": "completed", "conclusion": "success", "head_sha": commit}
                    for name in verifier.REQUIRED_CI_JOBS
                ],
            },
        )
        tarball_name, tarball_sha, sha_name = self.populate_tarball(target_dir)
        write_json(
            target_dir / "binary-size.json",
            {
                **ident,
                "tarball": tarball_name,
                "tarball_path": tarball_name,
                "tarball_sha256": tarball_sha,
                "sha256_path": sha_name,
                "binary_size_bytes": 15_728_640,
            },
        )
        write_json(target_dir / "determinism.json", {**ident, "status": "pass"})
        write_json(target_dir / "ide.json", {**ident, "status": "pass", "checks": 4})

    def test_status_on_partial_directory_exits_2_and_lists_missing_lanes(self) -> None:
        ident = {"release_commit": COMMIT, "binary_sha256": DIGEST}
        write_json(self.receipts / "runcmake-macos.json", make_runcmake_payload(371))
        write_json(self.receipts / "path-swap.json", {**ident, "status": "pass", "differences": []})
        write_json(self.receipts / "wasm.json", {**ident, "status": "pass", "checks": []})

        res = self.run_script(
            ASSEMBLER_SCRIPT,
            "--receipts-dir", self.receipts,
            "--commit", COMMIT,
            "--status",
        )
        self.assertEqual(res.returncode, 2, res.stderr)
        self.assertIn("Summary: 3 / 19 required lanes ready (16 missing).", res.stdout)
        self.assertIn("| `runcmake_macos` | `track1-conformance` | `present` |", res.stdout)
        self.assertIn("| `wasi` | `track5-wasm` | `present` | `wasm.json` |", res.stdout)

        expected_missing = [
            lane.key for lane in assembler.REQUIRED_LANES
            if lane.key not in {"runcmake_macos", "path_swap", "wasi"}
        ]
        self.assertEqual(len(expected_missing), 16)
        self.assertIn(f"Missing required lanes: {', '.join(expected_missing)}", res.stdout)
        for key in expected_missing:
            self.assertIn(f"| `{key}` |", res.stdout)
        self.assertFalse(self.out_json.exists())

    def test_status_on_mismatched_sha_flags_exact_offending_filenames(self) -> None:
        self.populate_complete_directory_layout(self.receipts)

        linux_path = self.receipts / "runcmake-linux.json"
        linux_payload = read_json(linux_path)
        linux_payload["release_commit"] = STALE
        write_json(linux_path, linux_payload)

        fuzz_member = self.receipts / "fuzz" / "target-7.json"
        fuzz_payload = read_json(fuzz_member)
        fuzz_payload["release_commit"] = STALE
        write_json(fuzz_member, fuzz_payload)

        res = self.run_script(
            ASSEMBLER_SCRIPT,
            "--receipts-dir", self.receipts,
            "--commit", COMMIT,
            "--status",
        )
        self.assertEqual(res.returncode, 2, res.stderr)
        self.assertIn("| `runcmake_linux` | `track1-conformance` | `sha-mismatch` |", res.stdout)
        self.assertIn("| `fuzz` | `track4-soundness` | `sha-mismatch` |", res.stdout)
        self.assertIn(f"runcmake-linux.json: release_commit {STALE!r} != expected {COMMIT!r}", res.stdout)
        self.assertIn(f"fuzz/target-7.json: release_commit {STALE!r} != expected {COMMIT!r}", res.stdout)

    def test_status_on_complete_19_receipt_directory_exits_0(self) -> None:
        self.populate_complete_directory_layout(self.receipts)

        res = self.run_script(
            ASSEMBLER_SCRIPT,
            "--receipts-dir", self.receipts,
            "--commit", COMMIT,
            "--binary-sha256", DIGEST,
            "--status",
        )
        self.assertEqual(res.returncode, 0, res.stderr)
        self.assertIn("Summary: 19 / 19 required lanes ready.", res.stdout)
        self.assertNotIn("Missing required lanes:", res.stdout)
        self.assertNotIn("Offending files:", res.stdout)

    def test_full_assemble_and_stamp_docs_on_19_receipt_directory(self) -> None:
        self.populate_complete_directory_layout(self.receipts)
        write_json(self.receipts / "binary-size.json", {
            "release_commit": COMMIT, "binary_sha256": DIGEST, "binary_size_bytes": 15_728_640,
        })

        res = self.run_script(
            ASSEMBLER_SCRIPT,
            "--receipts-dir", self.receipts,
            "--commit", COMMIT,
            "--binary-sha256", DIGEST,
            "--version", "0.1.0",
            "--out-json", self.out_json,
            "--manifest-out", self.manifest_out,
            "--verify-json-out", self.findings_out,
            "--wasm-bundle", self.wasm_bundle,
            "--assemble",
            "--stamp-docs",
            "--readme", self.readme,
            "--writeup", self.writeup,
        )
        self.assertEqual(res.returncode, 0, f"stdout:\n{res.stdout}\nstderr:\n{res.stderr}")
        self.assertTrue(self.out_json.is_file())
        self.assertTrue(self.manifest_out.is_file())
        self.assertTrue(self.findings_out.is_file())

        snapshot = read_json(self.out_json)
        schema = read_json(SCHEMA_PATH)
        builder.validate(snapshot, schema, schema)
        self.assertEqual(snapshot["commit"], COMMIT)
        self.assertEqual(snapshot["binarySha256"], DIGEST)
        self.assertEqual(snapshot["tracks"]["coreConformance"]["suitesAt100"], 62)
        policies = snapshot["tracks"]["policies"]
        self.assertEqual((policies["suitesPassed"], policies["suitesTotal"], policies["status"]),
                         (35, 35, "pass"))
        vcpkg = snapshot["tracks"]["vcpkgMatrix"]
        self.assertEqual((vcpkg["shardsPassed"], vcpkg["passRate"], vcpkg["status"]), (64, 100, "pass"))
        self.assertEqual((vcpkg["portsPassed"], vcpkg["portsTotal"], vcpkg["portsInspected"]),
                         (2432, 2432, 2560))
        self.assertIn("2,432 / 2,432 scored CMake ports", self.writeup.read_text(encoding="utf-8"))
        self.assertIn("2,560 ports inspected", self.writeup.read_text(encoding="utf-8"))
        self.assertEqual(read_json(self.manifest_out)["receipts"]["dist"]["path"],
                         str((self.receipts / "dist.json").resolve()))
        self.assertEqual(snapshot["tracks"]["bossFight"]["l1Passed"], 10)
        self.assertEqual(snapshot["tracks"]["unseenHoldout"]["status"], "pass")

        findings_report = read_json(self.findings_out)
        self.assertTrue(findings_report["ok"])
        self.assertEqual(findings_report["findings"], [])
        self.assertEqual(len(findings_report["checked_receipts"]), 19)

        # Independently verify with verify_release_receipts.py and stamp_metrics.py --check.
        verify_res = self.run_script(
            VERIFIER_SCRIPT,
            "--manifest", self.manifest_out,
            "--dashboard", self.out_json,
            "--wasm-bundle", self.wasm_bundle,
            "--expected-commit", COMMIT,
            "--expected-binary-sha256", DIGEST,
        )
        self.assertEqual(verify_res.returncode, 0, verify_res.stderr)

        check_res = self.run_script(
            STAMPER_SCRIPT,
            "--check",
            "--dashboard", self.out_json,
            "--receipts", self.manifest_out,
            "--file", self.readme,
            "--file", self.writeup,
        )
        self.assertEqual(check_res.returncode, 0, check_res.stderr)

        resolved_keys: set[str] = set()
        for path, expected_count in ((self.readme, 19), (self.writeup, 25)):
            markers = stamper.scan_markers(path.read_text(encoding="utf-8"), path.name)
            self.assertEqual(len(markers), expected_count)
            for marker in markers:
                self.assertTrue(marker.value, marker.key)
                self.assertNotRegex(marker.value, stamper.PENDING_VALUE_RE)
                resolved_keys.add(marker.key)
        self.assertEqual(resolved_keys, stamper.EXPECTED_KEYS)

    def test_distribution_rejects_a_disagreeing_size_receipt(self) -> None:
        distribution = {"release_commit": COMMIT, "binary_sha256": DIGEST, "binary_size_bytes": 123}
        write_json(self.receipts / "dist.json", distribution)
        for key, value, state in (
            ("binary_size_bytes", 124, "invalid"),
            ("release_commit", STALE, "sha-mismatch"),
            ("binary_sha256", STALE_DIGEST, "binary-mismatch"),
        ):
            with self.subTest(key=key):
                write_json(self.receipts / "binary-size.json", {**distribution, key: value})
                scan = assembler.scan_receipts_dir(self.receipts, COMMIT)
                dist = next(lane for lane in scan.lanes if lane.spec.key == "dist")
                self.assertEqual(dist.state, state)
                self.assertTrue(any("binary-size.json" in item and key in item for item in dist.offenders))

    def test_full_assemble_with_flat_short_aliases_and_inferred_binary_sha256(self) -> None:
        self.populate_flat_alias_layout(self.receipts)
        summary_path = self.receipts / "vcpkg.json"
        summary = read_json(summary_path)
        summary.update(ports_scored=2559, ports_passed=2559, ports_skipped=1)
        summary["shards"][0].update(ports_scored=39, ports_passed=39, ports_skipped=1)
        write_json(summary_path, summary)

        res = self.run_script(
            ASSEMBLER_SCRIPT,
            "--receipts-dir", self.receipts,
            "--commit", COMMIT,
            "--out-json", self.out_json,
            "--manifest-out", self.manifest_out,
            "--verify-json-out", self.findings_out,
            "--wasm-bundle", self.wasm_bundle,
            "--assemble",
            "--stamp-docs",
            "--readme", self.readme,
            "--writeup", self.writeup,
        )
        self.assertEqual(res.returncode, 0, f"stdout:\n{res.stdout}\nstderr:\n{res.stderr}")

        snapshot = read_json(self.out_json)
        self.assertEqual(snapshot["commit"], COMMIT)
        self.assertEqual(snapshot["binarySha256"], DIGEST)
        vcpkg = snapshot["tracks"]["vcpkgMatrix"]
        self.assertEqual((vcpkg["portsPassed"], vcpkg["portsTotal"], vcpkg["portsInspected"]),
                         (2559, 2559, 2560))
        gate_ids = {row["id"] for row in snapshot["tracks"]["benchmarksAndGates"]["gates"]}
        self.assertIn("determinism", gate_ids)
        self.assertIn("path-swap", gate_ids)

        findings_report = read_json(self.findings_out)
        self.assertTrue(findings_report["ok"])
        self.assertEqual(findings_report["findings"], [])
        self.assertIn("determinism", findings_report["checked_receipts"])
        self.assertIn("ide", findings_report["checked_receipts"])

        verify_res = self.run_script(
            VERIFIER_SCRIPT,
            "--manifest", self.manifest_out,
            "--dashboard", self.out_json,
            "--wasm-bundle", self.wasm_bundle,
            "--expected-commit", COMMIT,
            "--expected-binary-sha256", DIGEST,
        )
        self.assertEqual(verify_res.returncode, 0, verify_res.stderr)

        check_res = self.run_script(
            STAMPER_SCRIPT,
            "--check",
            "--dashboard", self.out_json,
            "--receipts", self.manifest_out,
            "--file", self.readme,
            "--file", self.writeup,
        )
        self.assertEqual(check_res.returncode, 0, check_res.stderr)

    def test_default_commit_falls_back_to_git_head_and_partial_stamp_docs(self) -> None:
        head_sha = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip().lower()
        res = self.run_script(
            ASSEMBLER_SCRIPT,
            "--receipts-dir", self.receipts,
            "--status",
        )
        self.assertEqual(res.returncode, 2)
        self.assertIn(f"Commit: `{head_sha}`", res.stdout)

        ident = {"release_commit": head_sha, "binary_sha256": DIGEST}
        write_json(self.receipts / "runcmake-macos.json", make_runcmake_payload(371, head_sha, DIGEST))
        write_json(self.receipts / "path-swap.json", {**ident, "status": "pass", "differences": []})

        partial_res = self.run_script(
            ASSEMBLER_SCRIPT,
            "--receipts-dir", self.receipts,
            "--out-json", self.out_json,
            "--assemble",
            "--allow-partial",
            "--stamp-docs",
            "--readme", self.readme,
            "--writeup", self.writeup,
        )
        self.assertEqual(partial_res.returncode, 0, partial_res.stderr)
        self.assertIn(f"<!-- METRIC:release_commit -->`{head_sha[:12]}`<!-- /METRIC -->", self.readme.read_text(encoding="utf-8"))
        self.assertIn("<!-- METRIC:runcmake_windows -->pending frozen release run<!-- /METRIC -->", self.writeup.read_text(encoding="utf-8"))

    def test_assemble_refuses_mixed_sha_and_mixed_binary_without_touching_outputs(self) -> None:
        self.populate_complete_directory_layout(self.receipts)
        original_readme = self.readme.read_bytes()
        original_writeup = self.writeup.read_bytes()

        probe_path = self.receipts / "probe-cache.json"
        probe_payload = read_json(probe_path)
        probe_payload["release_commit"] = STALE
        write_json(probe_path, probe_payload)

        ci_path = self.receipts / "ci-jobs.json"
        ci_payload = read_json(ci_path)
        ci_payload["workflow_runs"][0]["head_sha"] = STALE
        write_json(ci_path, ci_payload)

        # Both strict --assemble and --assemble --allow-partial must refuse mixed SHAs.
        for extra in ((), ("--allow-partial",)):
            res = self.run_script(
                ASSEMBLER_SCRIPT,
                "--receipts-dir", self.receipts,
                "--commit", COMMIT,
                "--out-json", self.out_json,
                "--wasm-bundle", self.wasm_bundle,
                "--assemble",
                "--stamp-docs",
                "--readme", self.readme,
                "--writeup", self.writeup,
                *extra,
            )
            self.assertEqual(res.returncode, 2, res.stderr)
            self.assertIn("probe-cache.json", res.stderr)
            self.assertIn("ci-jobs.json", res.stderr)
            self.assertFalse(self.out_json.exists())
            self.assertEqual(self.readme.read_bytes(), original_readme)
            self.assertEqual(self.writeup.read_bytes(), original_writeup)

        # Restore commits and inject a mismatched binary_sha256 on an optional lane.
        probe_payload["release_commit"] = COMMIT
        write_json(probe_path, probe_payload)
        ci_payload["workflow_runs"][0]["head_sha"] = COMMIT
        write_json(ci_path, ci_payload)
        write_json(self.receipts / "ide.json", {"release_commit": COMMIT, "binary_sha256": STALE_DIGEST, "status": "pass"})

        res = self.run_script(
            ASSEMBLER_SCRIPT,
            "--receipts-dir", self.receipts,
            "--commit", COMMIT,
            "--out-json", self.out_json,
            "--wasm-bundle", self.wasm_bundle,
            "--assemble",
        )
        self.assertEqual(res.returncode, 2, res.stderr)
        self.assertIn("ide.json", res.stderr)
        self.assertIn("binary_sha256", res.stderr)
        self.assertFalse(self.out_json.exists())

    def test_assemble_allow_partial_writes_preview_and_reports_missing_lanes(self) -> None:
        ident = {"release_commit": COMMIT, "binary_sha256": DIGEST}
        write_json(self.receipts / "runcmake-macos.json", make_runcmake_payload(371))
        write_json(
            self.receipts / "probe-cache.json",
            {
                **ident,
                "mode": "gate",
                "passed": True,
                "cases": [{"case": f"c{i}", "status": "passed", "passed": True} for i in range(16)],
            },
        )
        write_json(self.receipts / "path-swap.json", {**ident, "status": "pass", "differences": []})

        res = self.run_script(
            ASSEMBLER_SCRIPT,
            "--receipts-dir", self.receipts,
            "--commit", COMMIT,
            "--binary-sha256", DIGEST,
            "--out-json", self.out_json,
            "--assemble",
            "--allow-partial",
        )
        self.assertEqual(res.returncode, 0, res.stderr)
        self.assertIn("Partial preview written", res.stdout)
        self.assertIn("Missing required lanes:", res.stdout)
        self.assertTrue(self.out_json.is_file())

        snapshot = read_json(self.out_json)
        schema = read_json(SCHEMA_PATH)
        builder.validate(snapshot, schema, schema)
        self.assertEqual(snapshot["commit"], COMMIT)
        self.assertEqual(snapshot["binarySha256"], DIGEST)
        platforms = {row["id"]: row for row in snapshot["tracks"]["runCmakeOs"]["platforms"]}
        self.assertEqual(platforms["macos-arm64"]["status"], "pass")
        self.assertEqual(platforms["linux-x86_64"]["status"], "pending")
        self.assertEqual(snapshot["tracks"]["vcpkgMatrix"]["status"], "pending")

    def test_strict_verifier_failure_preserves_existing_out_json_and_docs(self) -> None:
        self.populate_complete_directory_layout(self.receipts)
        probe_path = self.receipts / "probe-cache.json"
        probe_payload = read_json(probe_path)
        probe_payload["mode"] = "smoke"
        write_json(probe_path, probe_payload)

        self.out_json.parent.mkdir(parents=True, exist_ok=True)
        sentinel = b'{"sentinel": true}\n'
        self.out_json.write_bytes(sentinel)
        original_readme = self.readme.read_bytes()
        original_writeup = self.writeup.read_bytes()

        res = self.run_script(
            ASSEMBLER_SCRIPT,
            "--receipts-dir", self.receipts,
            "--commit", COMMIT,
            "--binary-sha256", DIGEST,
            "--out-json", self.out_json,
            "--wasm-bundle", self.wasm_bundle,
            "--assemble",
            "--stamp-docs",
            "--readme", self.readme,
            "--writeup", self.writeup,
        )
        self.assertEqual(res.returncode, 1)
        self.assertIn("probe_cache (failed)", res.stderr)
        self.assertEqual(self.out_json.read_bytes(), sentinel)
        self.assertEqual(self.readme.read_bytes(), original_readme)
        self.assertEqual(self.writeup.read_bytes(), original_writeup)

    def test_ambiguous_alias_files_and_help_layout_documentation(self) -> None:
        ident = {"release_commit": COMMIT, "binary_sha256": DIGEST}
        write_json(self.receipts / "wasi.json", {**ident, "status": "pass", "checks": []})
        write_json(self.receipts / "wasm.json", {**ident, "status": "pass", "checks": []})

        res = self.run_script(
            ASSEMBLER_SCRIPT,
            "--receipts-dir", self.receipts,
            "--commit", COMMIT,
            "--status",
        )
        self.assertEqual(res.returncode, 2)
        self.assertIn("ambiguous", res.stdout)
        self.assertIn("wasi.json", res.stdout)
        self.assertIn("wasm.json", res.stdout)

        help_res = self.run_script(ASSEMBLER_SCRIPT, "--help")
        self.assertEqual(help_res.returncode, 0)
        for name in (
            "runcmake-macos.json",
            "blackbox.json",
            "large-projects.json",
            "holdout.json",
            "vcpkg.json",
            "conan.json",
            "ide.json",
            "path-swap.json",
            "bench-configure.json",
            "bench-giants.json",
            "probe-cache.json",
            "mutation.json",
            "determinism.json",
            "wasm.json",
            "fuzz.json",
            "diff-fuzz.json",
            "binary-size.json",
            "cli-ux.json",
            "ci-jobs.json",
        ):
            self.assertIn(name, help_res.stdout)


if __name__ == "__main__":
    unittest.main()

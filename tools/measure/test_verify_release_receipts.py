# SPDX-License-Identifier: MIT OR Apache-2.0
"""Unit tests for frozen release receipt verification across all 6 Launch Bar tracks."""

from __future__ import annotations

from contextlib import redirect_stderr, redirect_stdout
import hashlib
import io
import json
from pathlib import Path
from tempfile import TemporaryDirectory
import unittest

from tools.measure import build_dashboard_json as builder
from tools.measure import verify_release_receipts as verifier


COMMIT_OK = "0123456789abcdef0123456789abcdef01234567"
COMMIT_STALE = "fedcba9876543210fedcba9876543210fedcba98"
BINARY_OK = "a" * 64
BINARY_STALE = "b" * 64


def make_runcmake_payload(
    suites_total: int,
    suites_passed: int | None = None,
    cases_passed: int = 2000,
    cases_total: int = 2000,
    commit: str = COMMIT_OK,
    binary: str = BINARY_OK,
) -> dict:
    return {
        "release_commit": commit,
        "binary_sha256": binary,
        "suites_total": suites_total,
        "suites_passed": suites_total if suites_passed is None else suites_passed,
        "cases_passed": cases_passed,
        "cases_total": cases_total,
    }


def make_full_dashboard(commit: str = COMMIT_OK, binary: str = BINARY_OK) -> dict:
    base = builder.read_json(builder.DATA / "latest.json")
    base["commit"] = commit
    base["binarySha256"] = binary
    base["generatedAt"] = "2026-11-25T23:00:00Z"
    base["statusNote"] = "CI reports 6 of 6 completed runs as successful. Open runs total 0."
    base["knownParityGaps"] = []
    base["receipt"] = {
        "commit": commit,
        "binarySha256": binary,
        "binarySizeBytes": 15_728_640,
        "oracleVersion": "4.5.1",
        "generatedAt": "2026-11-25T23:00:00Z",
    }

    tracks = base["tracks"]
    tracks["coreConformance"].update(
        status="pass", suitesTotal=62, suitesAt100=62, casesPassed=1240, casesTotal=1240, passRate=100.0
    )
    tracks["policies"].update(
        status="pass", suitesTotal=56, suitesPassed=56, casesPassed=560, casesTotal=560, passRate=100.0
    )
    tracks["runCmakeOs"]["status"] = "pass"
    for plat, count in zip(tracks["runCmakeOs"]["platforms"], (371, 362, 367)):
        plat.update(
            suitesTotal=count, suitesPassed=count, casesPassed=count * 5, casesTotal=count * 5,
            passRate=100.0, status="pass",
        )
    tracks["bossFight"].update(status="pass", projectsTotal=10, l1Passed=10, l2Passed=10)
    for proj in tracks["bossFight"]["projects"]:
        proj.update(l1="pass", l2="pass")
    tracks["unseenHoldout"].update(
        sealed=True, status="pass", reposTotal=240, reposPassed=238, passRate=99.17
    )
    tracks["vcpkgMatrix"].update(
        status="pass", shardsTotal=64, shardsCompleted=64, shardsPassed=64,
        portsTotal=2560, portsPassed=2560, passRate=100.0,
    )
    for shard in tracks["vcpkgMatrix"]["shards"]:
        shard.update(status="pass", portsTotal=40, portsPassed=40, portsFailed=0, durationSeconds=12.5)

    bench = tracks["benchmarksAndGates"]["benchmarks"]
    bench["warmProbeSpeedup"].update(value=5.4, status="pass")
    bench["incrementalLeafP95Ms"].update(value=38.2, status="pass")
    bench["coldConfigureSpeedup"].update(value=2.15, status="pass")
    bench["peakRssMiB"].update(value=74.5, status="pass")

    tracks["benchmarksAndGates"]["realGiants"] = [
        {
            "project": "duckdb",
            "repository": "duckdb/duckdb",
            "version": "v1.3.2",
            "coldConfigureSpeedup": 1.85,
            "warmProbeSpeedup": 4.9,
            "incrementalLeafP95Ms": 42.0,
            "peakRssMiB": 118.4,
            "binarySha256": binary,
            "status": "pass",
        }
    ]
    tracks["benchmarksAndGates"]["differentialFuzz"] = {
        "label": "Seeded Differential Fuzzing",
        "status": "pass",
        "seed": 42,
        "total": 1000,
        "passed": 1000,
        "failed": 0,
        "oracleErrors": 0,
        "binarySha256": binary,
    }
    for gate in tracks["benchmarksAndGates"]["gates"]:
        if gate["id"] == "probe-cache":
            gate.update(status="pass", passed=16, total=16, failures=0, summary="16 / 16 checks passed")
        elif gate["id"] == "mutation-replay":
            gate.update(status="pass", passed=20, total=20, failures=0, summary="20 / 20 checks passed")
        elif gate["id"] == "blackbox":
            gate.update(
                status="pass", passed=679, total=679, failures=0,
                scenariosPassed=20, scenariosTotal=20, summary="20 scenarios / 679 checks passed",
            )
    return base


class VerifyReleaseReceiptsTests(unittest.TestCase):
    def setUp(self):
        tmp = TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        self.receipts_dir = self.root / "receipts"
        self.receipts_dir.mkdir()
        self.wasm_path = self.root / "site" / "wasm" / "rcmake_wasm.wasm"
        self.wasm_path.parent.mkdir(parents=True)
        wasm_bytes = verifier.WASM_V1_MAGIC + b"\x01\x04\x01\x60\x00\x00"
        self.wasm_path.write_bytes(wasm_bytes)
        self.wasm_sha256 = hashlib.sha256(wasm_bytes).hexdigest()
        self.wasm_path.with_suffix(".js").write_text("export function configure() {}\n", encoding="utf-8")

        self.tarball_path = self.receipts_dir / "rcmake-0.1.0-aarch64-apple-darwin.tar.gz"
        tarball_bytes = b"synthetic-release-tarball-payload"
        self.tarball_path.write_bytes(tarball_bytes)
        self.tarball_sha256 = hashlib.sha256(tarball_bytes).hexdigest()
        self.tarball_sha_file = self.receipts_dir / "rcmake-0.1.0-aarch64-apple-darwin.tar.gz.sha256"
        self.tarball_sha_file.write_text(
            f"{self.tarball_sha256}  {self.tarball_path.name}\n",
            encoding="ascii",
        )

        self.dashboard_path = self.root / "site" / "data" / "latest.json"
        self.dashboard_path.parent.mkdir(parents=True)
        self.write_json(self.dashboard_path, make_full_dashboard())

    def write_json(self, path: Path, payload: dict) -> Path:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
        return path

    def build_valid_receipts(self, commit: str = COMMIT_OK, binary: str = BINARY_OK) -> dict[str, dict]:
        ident = {"release_commit": commit, "binary_sha256": binary}
        return {
            "runcmake_macos": make_runcmake_payload(371, 370, 1995, 2000, commit, binary),
            "runcmake_linux": make_runcmake_payload(362, 361, 1990, 2000, commit, binary),
            "runcmake_windows": make_runcmake_payload(367, 365, 1988, 2000, commit, binary),
            "independent_scenarios": {
                **ident,
                "mode": "differential",
                "scenarios": [
                    {"name": f"s{i:02d}", "status": "pass", "runs": {"rcmake": {"checks": [{"ok": True}]}}}
                    for i in range(1, 21)
                ],
            },
            "large_projects": {
                **ident,
                "projects": [
                    {"project": f"proj-{i}", "l1": {"cache.json": "equal"}, "l2": "equal", "status": "pass"}
                    for i in range(10)
                ],
            },
            "vcpkg": {
                **ident,
                "shards_total": 64,
                "shards_completed": 64,
                "shards_passed": 64,
                "ports_total": 2560,
                "ports_passed": 2560,
                "ports_failed": 0,
                "shards": [
                    {"shard": i, "shard_count": 64, "status": "pass", "ports_total": 40, "ports_passed": 40, "ports_failed": 0}
                    for i in range(64)
                ],
            },
            "conan": {
                **ident,
                "status": "pass",
                "scenarios": [
                    {"name": "recursive-driver", "status": "pass"},
                    {"name": "conan-consumer", "status": "pass"},
                    {"name": "conan-source-build", "status": "pass"},
                ],
            },
            "unseen_holdout": {
                **ident,
                "sealed": True,
                "status": "pass",
                "aggregate": {"reposTotal": 240, "reposPassed": 238},
                "categories": [{"category": "Core C/C++ Libraries", "reposTotal": 240, "reposPassed": 238}],
            },
            "benchmarks_giants": {
                **ident,
                "results": [
                    {
                        "project": "duckdb",
                        "case": "warm probe-cache configure",
                        "speedup": 4.9,
                        "gate": {"status": "pass", "passed": True},
                        "rcmake": {"ok": True, "failures": 0, "p95_ms": 42.0},
                        "oracle": {"ok": True, "failures": 0},
                    }
                ],
            },
            "benchmarks_synthetic": {
                **ident,
                "results": [
                    {
                        "project": "synthetic",
                        "name": "configure 500 targets (cold)",
                        "speedup": 2.15,
                        "rcmake": {"ok": True, "failures": 0, "peak_rss_mib": 74.5},
                        "oracle": {"ok": True, "failures": 0},
                    },
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
                        "gate": {"status": "pass", "passed": True},
                        "rcmake": {"ok": True, "failures": 0, "p95_ms": 38.2},
                    },
                ],
            },
            "probe_cache": {
                **ident,
                "mode": "gate",
                "passed": True,
                "cases": [{"case": f"p{i}", "status": "passed", "passed": True} for i in range(16)],
            },
            "mutation_replay": {
                **ident,
                "mode": "gate",
                "passed": True,
                "replay_verified": True,
                "steps": [{"step": i, "passed": True} for i in range(21)],
            },
            "fuzz": {
                **ident,
                "iterations": 14_000_000,
                "failures": 0,
                "targets": [f"t{i}" for i in range(14)],
            },
            "differential_fuzz": {
                **ident,
                "seed": 42,
                "total": 1000,
                "counts": {"pass": 1000, "fail": 0, "oracle_error": 0},
                "cases": [],
            },
            "wasi": {
                **ident,
                "status": "pass",
                "checks": [{"check": f"w{i}", "status": "pass"} for i in range(6)],
            },
            "cli_ux": {
                **ident,
                "status": "pass",
                "passed": 12,
                "total": 12,
                "failures": 0,
            },
            "path_swap": {
                **ident,
                "status": "pass",
                "differences": [],
            },
            "dist": {
                **ident,
                "tarball": self.tarball_path.name,
                "tarball_sha256": self.tarball_sha256,
                "tarball_path": self.tarball_path.name,
                "sha256_path": self.tarball_sha_file.name,
                "binary_size_bytes": 15_728_640,
            },
        }

    def write_manifest(self, receipts_payloads: dict[str, dict], **overrides) -> Path:
        entries = {}
        for key, payload in receipts_payloads.items():
            filename = f"{key.replace('_', '-')}.json"
            self.write_json(self.receipts_dir / filename, payload)
            entries[key] = {"path": filename}
        manifest = {
            "schema_version": 1,
            "release_commit": COMMIT_OK,
            "binary_sha256": BINARY_OK,
            "oracle_version": "4.5.1",
            "generated_at": "2026-11-25T23:00:00Z",
            "dashboard": str(self.dashboard_path),
            "wasm_bundle": str(self.wasm_path),
            "wasm_sha256": self.wasm_sha256,
            "receipts": entries,
        }
        manifest.update(overrides)
        return self.write_json(self.receipts_dir / "manifest.json", manifest)

    def run_cli(self, *args: str) -> tuple[int, str, str]:
        out, err = io.StringIO(), io.StringIO()
        with redirect_stdout(out), redirect_stderr(err):
            code = verifier.main(list(args))
        return code, out.getvalue(), err.getvalue()

    def test_valid_manifest_all_six_tracks_passes(self):
        manifest = self.write_manifest(self.build_valid_receipts())
        json_out = self.root / "report.json"
        code, stdout, stderr = self.run_cli(
            "--manifest", str(manifest),
            "--expected-commit", COMMIT_OK,
            "--expected-binary-sha256", BINARY_OK,
            "--json-out", str(json_out),
        )
        self.assertEqual(code, 0, stderr)
        self.assertIn("PASS: verified 18 release receipts", stdout)
        self.assertEqual(stderr, "")
        report = json.loads(json_out.read_text(encoding="utf-8"))
        self.assertTrue(report["ok"])
        self.assertEqual(len(report["checked_receipts"]), 18)
        self.assertEqual(report["findings"], [])

    def test_auto_discovery_receipts_dir_including_subdirectories(self):
        payloads = self.build_valid_receipts()
        for key, payload in payloads.items():
            if key in {"fuzz", "vcpkg", "large_projects"}:
                continue
            self.write_json(self.receipts_dir / f"{key.replace('_', '-')}.json", payload)

        # Multi-file fuzz directory
        for idx in range(14):
            self.write_json(
                self.receipts_dir / "fuzz" / f"target-{idx}.json",
                {"release_commit": COMMIT_OK, "binary_sha256": BINARY_OK, "iterations": 1_000_000, "failures": 0},
            )
        # Multi-file 64-shard vcpkg directory
        for idx in range(64):
            shard_dir = self.receipts_dir / "vcpkg" / f"shard-{idx:02d}"
            self.write_json(
                shard_dir / "shard.json",
                {
                    "release_commit": COMMIT_OK,
                    "binary_sha256": BINARY_OK,
                    "shard_index": idx,
                    "shard_count": 64,
                    "expected_ports": 2,
                },
            )
            (shard_dir / "results.jsonl").write_text(
                '{"status": "pass", "port": "a"}\n{"status": "pass", "port": "b"}\n',
                encoding="utf-8",
            )
        # Multi-file large-projects directory
        for idx in range(10):
            self.write_json(
                self.receipts_dir / "large-projects" / f"proj-{idx}" / "result.json",
                {
                    "release_commit": COMMIT_OK,
                    "binary_sha256": BINARY_OK,
                    "project": f"proj-{idx}",
                    "l1": {"cache.json": "equal"},
                    "l2": "equal",
                },
            )

        code, stdout, stderr = self.run_cli(
            "--receipts-dir", str(self.receipts_dir),
            "--dashboard", str(self.dashboard_path),
            "--wasm-bundle", str(self.wasm_path),
        )
        self.assertEqual(code, 0, stderr)
        self.assertIn("PASS: verified 18 release receipts", stdout)

    def test_rejects_stale_commit_and_mixed_binary_sha256(self):
        payloads = self.build_valid_receipts()
        payloads["runcmake_linux"]["release_commit"] = COMMIT_STALE
        payloads["probe_cache"]["binary_sha256"] = BINARY_STALE
        manifest = self.write_manifest(payloads)

        code, stdout, stderr = self.run_cli("--manifest", str(manifest))
        self.assertEqual(code, 1)
        self.assertEqual(stdout, "")
        self.assertIn("runcmake_linux (stale)", stderr)
        self.assertIn(COMMIT_STALE, stderr)
        self.assertIn("probe_cache (stale)", stderr)
        self.assertIn(BINARY_STALE, stderr)

    def test_rejects_envelope_vs_file_sha_mismatch(self):
        payloads = self.build_valid_receipts()
        payloads["cli_ux"]["release_commit"] = COMMIT_STALE
        manifest_path = self.write_manifest(payloads)
        manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
        manifest["receipts"]["cli_ux"]["release_commit"] = COMMIT_OK
        self.write_json(manifest_path, manifest)

        code, _, stderr = self.run_cli("--manifest", str(manifest_path))
        self.assertEqual(code, 1)
        self.assertIn("cli_ux (mismatch)", stderr)
        self.assertIn("envelope commit disagrees", stderr)

    def test_rejects_mixed_sha_inside_multi_file_fuzz_directory(self):
        payloads = self.build_valid_receipts()
        for key, payload in payloads.items():
            if key == "fuzz":
                continue
            self.write_json(self.receipts_dir / f"{key.replace('_', '-')}.json", payload)
        for idx in range(14):
            self.write_json(
                self.receipts_dir / "fuzz" / f"target-{idx}.json",
                {
                    "release_commit": COMMIT_OK,
                    "binary_sha256": BINARY_STALE if idx == 7 else BINARY_OK,
                    "iterations": 1_000_000,
                    "failures": 0,
                },
            )
        code, _, stderr = self.run_cli(
            "--receipts-dir", str(self.receipts_dir),
            "--no-dashboard",
            "--wasm-bundle", str(self.wasm_path),
        )
        self.assertEqual(code, 1)
        self.assertIn("fuzz:target-7.json (stale)", stderr)

    def test_rejects_missing_receipt_and_runcmake_below_threshold(self):
        payloads = self.build_valid_receipts()
        del payloads["cli_ux"]
        payloads["runcmake_macos"] = make_runcmake_payload(7, 7, 100, 100)
        payloads["runcmake_windows"] = make_runcmake_payload(367, 360, 1970, 2000)
        manifest = self.write_manifest(payloads)

        code, _, stderr = self.run_cli("--manifest", str(manifest))
        self.assertEqual(code, 1)
        self.assertIn("cli_ux (missing)", stderr)
        self.assertIn("runcmake_macos (failed): expected >= 371 suites; got 7 suites", stderr)
        self.assertIn("runcmake_windows (failed): expected >= 99.0%; got 98.50%", stderr)

    def test_rejects_unseen_holdout_leak_without_echoing_repo_identity(self):
        payloads = self.build_valid_receipts()
        secret_org = "sealed-private-org-xyz"
        secret_repo = "confidential-cmake-target"
        payloads["unseen_holdout"]["projects"] = [f"{secret_org}/{secret_repo}"]
        payloads["unseen_holdout"]["note"] = f"https://github.com/{secret_org}/{secret_repo}.git"
        manifest = self.write_manifest(payloads)
        json_out = self.root / "leak-report.json"

        code, _, stderr = self.run_cli("--manifest", str(manifest), "--json-out", str(json_out))
        self.assertEqual(code, 1)
        self.assertIn("unseen_holdout (leak)", stderr)
        self.assertNotIn(secret_org, stderr)
        self.assertNotIn(secret_repo, stderr)
        serialized = json_out.read_text(encoding="utf-8")
        self.assertNotIn(secret_org, serialized)
        self.assertNotIn(secret_repo, serialized)

    def test_rejects_failing_soundness_ecosystem_benchmark_and_packaging_gates(self):
        payloads = self.build_valid_receipts()
        payloads["probe_cache"]["mode"] = "smoke"
        payloads["mutation_replay"]["steps"] = [{"step": i, "passed": True} for i in range(20)]  # only 19 edits
        payloads["fuzz"]["iterations"] = 13_999_999
        payloads["differential_fuzz"]["counts"] = {"pass": 998, "fail": 1, "oracle_error": 1}
        payloads["wasi"]["checks"] = [{"check": f"w{i}", "status": "pass"} for i in range(5)]
        payloads["conan"]["scenarios"][1]["status"] = "skipped_missing_tool"
        payloads["vcpkg"]["shards_completed"] = 63
        payloads["benchmarks_giants"]["results"] = [
            {"project": "synthetic", "speedup": 3.0, "rcmake": {"ok": True}}
        ]
        manifest = self.write_manifest(payloads)

        code, _, stderr = self.run_cli("--manifest", str(manifest))
        self.assertEqual(code, 1)
        for key in (
            "probe_cache", "mutation_replay", "fuzz", "differential_fuzz",
            "wasi", "conan", "vcpkg", "benchmarks_giants",
        ):
            self.assertIn(f"{key} (failed)", stderr)

    def test_rejects_corrupt_wasm_bundle_and_dist_tarball_checksum_mismatch(self):
        payloads = self.build_valid_receipts()
        manifest = self.write_manifest(payloads)
        # Tamper with both the WASM bundle and the release tarball on disk after manifest generation.
        self.wasm_path.write_bytes(b"not-a-wasm-binary")
        self.tarball_path.write_bytes(b"tampered-release-tarball")

        code, _, stderr = self.run_cli("--manifest", str(manifest))
        self.assertEqual(code, 1)
        self.assertIn("wasm_bundle (invalid)", stderr)
        self.assertIn("dist (mismatch)", stderr)

    def test_rejects_stale_dashboard_commit_and_pending_track(self):
        payloads = self.build_valid_receipts()
        stale_dash = make_full_dashboard(commit=COMMIT_STALE)
        stale_dash["tracks"]["bossFight"]["status"] = "pending"
        self.write_json(self.dashboard_path, stale_dash)
        manifest = self.write_manifest(payloads)

        code, _, stderr = self.run_cli("--manifest", str(manifest))
        self.assertEqual(code, 1)
        self.assertIn("dashboard (stale)", stderr)
        self.assertIn("dashboard.bossFight (stale)", stderr)

    def test_rejects_null_top_level_dashboard_commit_even_when_receipt_block_has_commit(self):
        payloads = self.build_valid_receipts()
        dash = make_full_dashboard(commit=COMMIT_OK)
        dash["commit"] = None
        self.write_json(self.dashboard_path, dash)
        manifest = self.write_manifest(payloads)

        code, _, stderr = self.run_cli("--manifest", str(manifest))
        self.assertEqual(code, 1)
        self.assertIn("dashboard (missing)", stderr)
        self.assertIn("commit is null or missing", stderr)


if __name__ == "__main__":
    unittest.main()

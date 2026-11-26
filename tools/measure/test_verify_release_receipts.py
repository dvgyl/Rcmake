# SPDX-License-Identifier: MIT OR Apache-2.0
"""Unit tests for frozen release receipt verification across all 6 Launch Bar tracks."""

from __future__ import annotations

from contextlib import redirect_stderr, redirect_stdout
import hashlib
import io
import json
from pathlib import Path
import re
import shutil
import subprocess
import tarfile
from tempfile import TemporaryDirectory
import unittest
import zipfile

from tools.measure import build_dashboard_json as builder
from tools.measure import verify_release_receipts as verifier


COMMIT_OK = "0123456789abcdef0123456789abcdef01234567"
COMMIT_STALE = "fedcba9876543210fedcba9876543210fedcba98"
BINARY_BYTES = b"#!/bin/sh\necho rcmake-0.1.0\n"
BINARY_OK = hashlib.sha256(BINARY_BYTES).hexdigest()
BINARY_STALE = "b" * 64


def write_minimal_tarball(
    archive_path: Path,
    binary_bytes: bytes = BINARY_BYTES,
    *,
    top_dir: str = "rcmake-0.1.0-aarch64-apple-darwin",
    member_rel: str | None = "bin/rcmake",
) -> str:
    """Write a real minimal .tar.gz archive containing bin/rcmake and return its SHA-256."""
    with tarfile.open(archive_path, "w:gz") as tar:
        if member_rel is not None:
            info = tarfile.TarInfo(name=f"{top_dir}/{member_rel}")
            info.size = len(binary_bytes)
            info.mode = 0o755
            tar.addfile(info, io.BytesIO(binary_bytes))
    return hashlib.sha256(archive_path.read_bytes()).hexdigest()


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
        status="pass", suitesTotal=35, suitesPassed=35, casesPassed=350, casesTotal=350, passRate=100.0
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
        self.tarball_sha256 = write_minimal_tarball(self.tarball_path, BINARY_BYTES)
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
                        "notes": "new build tree, empty probe cache, includes compiler checks",
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
            "ci_jobs": self.make_ci_jobs_payload(commit=commit, binary=binary),
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
        self.assertIn("PASS: verified 19 release receipts", stdout)
        self.assertEqual(stderr, "")
        report = json.loads(json_out.read_text(encoding="utf-8"))
        self.assertTrue(report["ok"])
        self.assertEqual(len(report["checked_receipts"]), 19)
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
                    "expected_ports": 3,
                },
            )
            (shard_dir / "results.jsonl").write_text(
                '{"status": "pass", "port": "a"}\n'
                '{"status": "oracle-blocked-before-configure", "port": "b"}\n'
                '{"status": "no-observed-cmake-configure", "port": "c"}\n',
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
        self.assertIn("PASS: verified 19 release receipts", stdout)

    def test_vcpkg_summary_uses_explicit_scored_counts(self):
        payloads = self.build_valid_receipts()
        vcpkg = payloads["vcpkg"]
        vcpkg.update(ports_scored=2496, ports_passed=2496, ports_skipped=64)
        for shard in vcpkg["shards"]:
            shard.update(ports_scored=39, ports_passed=39, ports_skipped=1)
        manifest = self.write_manifest(payloads)
        code, _, stderr = self.run_cli("--manifest", str(manifest))
        self.assertEqual(code, 0, stderr)

        vcpkg["shards"][0]["status"] = "fail"
        manifest = self.write_manifest(payloads)
        code, _, stderr = self.run_cli("--manifest", str(manifest))
        self.assertEqual(code, 1)
        self.assertIn("vcpkg (failed)", stderr)

        vcpkg["shards"][0].update(status="pass", ports_scored=0, ports_passed=0, ports_skipped=40)
        manifest = self.write_manifest(payloads)
        code, _, stderr = self.run_cli("--manifest", str(manifest))
        self.assertEqual(code, 1)
        self.assertIn("shard 0: passed=0, scored=0", stderr)

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

    def test_rejects_cold_benchmark_without_empty_probe_cache(self):
        payloads = self.build_valid_receipts()
        cold = payloads["benchmarks_synthetic"]["results"][0]
        cold["notes"] = "new build tree, includes compiler checks"
        manifest = self.write_manifest(payloads)

        code, _, stderr = self.run_cli("--manifest", str(manifest))
        self.assertEqual(code, 1)
        self.assertIn("benchmarks_synthetic (missing)", stderr)
        self.assertIn("cold 500-target configure (empty probe cache)", stderr)

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

    def make_ci_jobs_payload(
        self,
        *,
        commit: str = COMMIT_OK,
        binary: str = BINARY_OK,
        failed_job: str | None = None,
        omit_job: str | None = None,
        event: str = "push",
        run_conclusion: str = "success",
    ) -> dict:
        runs = []
        for name in verifier.REQUIRED_CI_JOBS:
            if name == omit_job:
                continue
            runs.append({
                "name": name,
                "status": "completed",
                "conclusion": "failure" if name == failed_job else "success",
                "head_sha": commit,
            })
        return {
            "release_commit": commit,
            "binary_sha256": binary,
            "workflow": "CI",
            "event": event,
            "run_conclusion": run_conclusion,
            "workflow_runs": runs,
        }

    def test_ci_jobs_requires_head_sha_in_every_job(self):
        for fields in ({}, {"head_sha": ""}, {"head_sha": " "}, {"head_sha": 123},
                       {"headSha": COMMIT_OK}):
            with self.subTest(fields=fields):
                payloads = self.build_valid_receipts()
                ci_jobs = self.make_ci_jobs_payload()
                ci_jobs["workflow_runs"].append({
                    "name": "Extra CI job",
                    "status": "completed",
                    "conclusion": "success",
                    **fields,
                })
                payloads["ci_jobs"] = ci_jobs
                manifest = self.write_manifest(payloads)
                code, _, stderr = self.run_cli("--manifest", str(manifest))
                self.assertEqual(code, 1)
                self.assertIn("Extra CI job (head_sha must be a non-empty string)", stderr)

    def test_ci_jobs_accepts_all_six_distinct_green_and_rejects_five_of_six(self):
        payloads = self.build_valid_receipts()
        manifest = self.write_manifest(payloads)
        code, stdout, stderr = self.run_cli("--manifest", str(manifest))
        self.assertEqual(code, 0, stderr)
        self.assertIn("PASS: verified 19 release receipts", stdout)

        # Missing ci_jobs receipt must fail standalone verification in both manifest and receipts-dir modes.
        no_ci = self.build_valid_receipts()
        del no_ci["ci_jobs"]
        manifest = self.write_manifest(no_ci)
        code, _, stderr = self.run_cli("--manifest", str(manifest))
        self.assertEqual(code, 1)
        self.assertIn("ci_jobs (missing)", stderr)

        (self.receipts_dir / "ci-jobs.json").unlink(missing_ok=True)
        code, _, stderr = self.run_cli(
            "--receipts-dir", str(self.receipts_dir),
            "--dashboard", str(self.dashboard_path),
            "--wasm-bundle", str(self.wasm_path),
        )
        self.assertEqual(code, 1)
        self.assertIn("ci_jobs (missing)", stderr)

        # Missing run event or missing run conclusion must fail even when all 6 job rows say success.
        missing_event = self.make_ci_jobs_payload()
        del missing_event["event"]
        payloads["ci_jobs"] = missing_event
        manifest = self.write_manifest(payloads)
        code, _, stderr = self.run_cli("--manifest", str(manifest))
        self.assertEqual(code, 1)
        self.assertIn("ci_jobs (missing)", stderr)
        self.assertIn("event", stderr)

        missing_conclusion = self.make_ci_jobs_payload()
        del missing_conclusion["run_conclusion"]
        payloads["ci_jobs"] = missing_conclusion
        manifest = self.write_manifest(payloads)
        code, _, stderr = self.run_cli("--manifest", str(manifest))
        self.assertEqual(code, 1)
        self.assertIn("ci_jobs (missing)", stderr)
        self.assertIn("run_conclusion", stderr)

        # 5/6 with one failed job is rejected.
        payloads["ci_jobs"] = self.make_ci_jobs_payload(failed_job="Corpus smoke on macos-15")
        manifest = self.write_manifest(payloads)
        code, _, stderr = self.run_cli("--manifest", str(manifest))
        self.assertEqual(code, 1)
        self.assertIn("ci_jobs (failed)", stderr)
        self.assertIn("Corpus smoke on macos-15 (completed/failure)", stderr)

        # 5/6 with one omitted job is rejected.
        payloads["ci_jobs"] = self.make_ci_jobs_payload(omit_job="Corpus smoke on windows-2025")
        manifest = self.write_manifest(payloads)
        code, _, stderr = self.run_cli("--manifest", str(manifest))
        self.assertEqual(code, 1)
        self.assertIn("Corpus smoke on windows-2025 (missing)", stderr)

        # Duplicate job substituting for a missing required job is rejected.
        dup_payload = self.make_ci_jobs_payload(omit_job="Corpus smoke on windows-2025")
        dup_payload["workflow_runs"].append({
            "name": "CI on ubuntu-24.04",
            "status": "completed",
            "conclusion": "success",
            "head_sha": COMMIT_OK,
        })
        payloads["ci_jobs"] = dup_payload
        manifest = self.write_manifest(payloads)
        code, _, stderr = self.run_cli("--manifest", str(manifest))
        self.assertEqual(code, 1)
        self.assertIn("CI on ubuntu-24.04 (2 entries)", stderr)
        self.assertIn("Corpus smoke on windows-2025 (missing)", stderr)

        # pull_request event and summary-only payloads are rejected.
        payloads["ci_jobs"] = self.make_ci_jobs_payload(event="pull_request")
        manifest = self.write_manifest(payloads)
        code, _, stderr = self.run_cli("--manifest", str(manifest))
        self.assertEqual(code, 1)
        self.assertIn("pull_request", stderr)

        payloads["ci_jobs"] = {"release_commit": COMMIT_OK, "binary_sha256": BINARY_OK, "passed": 6, "total": 6}
        manifest = self.write_manifest(payloads)
        code, _, stderr = self.run_cli("--manifest", str(manifest))
        self.assertEqual(code, 1)
        self.assertIn("ci_jobs (invalid)", stderr)

        # --receipts-dir auto-discovers ci-jobs.json and enforces the same 6-job gate.
        self.write_json(
            self.receipts_dir / "ci-jobs.json",
            self.make_ci_jobs_payload(failed_job="CI on windows-2025"),
        )
        code, _, stderr = self.run_cli(
            "--receipts-dir", str(self.receipts_dir),
            "--dashboard", str(self.dashboard_path),
            "--wasm-bundle", str(self.wasm_path),
        )
        self.assertEqual(code, 1)
        self.assertIn("CI on windows-2025 (completed/failure)", stderr)

    def test_dist_rejects_plain_text_fake_archive_mismatched_basename_and_wrong_binary_digest(self):
        # Plain-text fake archive with matching SHA-256 and sidecar must still be rejected.
        fake_archive = self.receipts_dir / "rcmake-0.1.0-fake.tar.gz"
        fake_archive.write_bytes(b"synthetic-release-tarball-payload")
        fake_sha = hashlib.sha256(fake_archive.read_bytes()).hexdigest()
        fake_sidecar = self.receipts_dir / "rcmake-0.1.0-fake.tar.gz.sha256"
        fake_sidecar.write_text(f"{fake_sha}  {fake_archive.name}\n", encoding="ascii")

        payloads = self.build_valid_receipts()
        payloads["dist"].update(
            tarball=fake_archive.name,
            tarball_path=fake_archive.name,
            tarball_sha256=fake_sha,
            sha256_path=fake_sidecar.name,
        )
        manifest = self.write_manifest(payloads)
        code, _, stderr = self.run_cli("--manifest", str(manifest))
        self.assertEqual(code, 1)
        self.assertIn("dist (invalid)", stderr)

        # Mismatched basename between receipt tarball label and referenced archive path is rejected.
        other_archive = self.receipts_dir / "rcmake-0.1.0-other-target.tar.gz"
        other_sha = write_minimal_tarball(other_archive, BINARY_BYTES)
        other_sidecar = self.receipts_dir / "rcmake-0.1.0-other-target.tar.gz.sha256"
        other_sidecar.write_text(f"{other_sha}  {self.tarball_path.name}\n", encoding="ascii")
        payloads = self.build_valid_receipts()
        payloads["dist"].update(
            tarball=self.tarball_path.name,
            tarball_path=other_archive.name,
            tarball_sha256=other_sha,
            sha256_path=other_sidecar.name,
        )
        manifest = self.write_manifest(payloads)
        code, _, stderr = self.run_cli("--manifest", str(manifest))
        self.assertEqual(code, 1)
        self.assertIn("dist (mismatch)", stderr)
        self.assertIn("basename", stderr)

        # Real tar.gz whose packaged bin/rcmake does not match binary_sha256 is rejected.
        wrong_bin_archive = self.receipts_dir / "rcmake-0.1.0-wrong-bin.tar.gz"
        wrong_bin_sha = write_minimal_tarball(wrong_bin_archive, b"#!/bin/sh\necho tampered\n")
        wrong_bin_sidecar = self.receipts_dir / "rcmake-0.1.0-wrong-bin.tar.gz.sha256"
        wrong_bin_sidecar.write_text(f"{wrong_bin_sha}  {wrong_bin_archive.name}\n", encoding="ascii")
        payloads = self.build_valid_receipts()
        payloads["dist"].update(
            tarball=wrong_bin_archive.name,
            tarball_path=wrong_bin_archive.name,
            tarball_sha256=wrong_bin_sha,
            sha256_path=wrong_bin_sidecar.name,
        )
        manifest = self.write_manifest(payloads)
        code, _, stderr = self.run_cli("--manifest", str(manifest))
        self.assertEqual(code, 1)
        self.assertIn("dist (mismatch)", stderr)
        self.assertIn("Packaged executable", stderr)

        # Real tar.gz missing bin/rcmake is rejected.
        empty_archive = self.receipts_dir / "rcmake-0.1.0-no-bin.tar.gz"
        empty_sha = write_minimal_tarball(empty_archive, BINARY_BYTES, member_rel="README.txt")
        empty_sidecar = self.receipts_dir / "rcmake-0.1.0-no-bin.tar.gz.sha256"
        empty_sidecar.write_text(f"{empty_sha}  {empty_archive.name}\n", encoding="ascii")
        payloads = self.build_valid_receipts()
        payloads["dist"].update(
            tarball=empty_archive.name,
            tarball_path=empty_archive.name,
            tarball_sha256=empty_sha,
            sha256_path=empty_sidecar.name,
        )
        manifest = self.write_manifest(payloads)
        code, _, stderr = self.run_cli("--manifest", str(manifest))
        self.assertEqual(code, 1)
        self.assertIn("dist (invalid)", stderr)
        self.assertIn("does not contain bin/rcmake", stderr)

        # Real .zip release archive containing bin/rcmake.exe passes.
        zip_archive = self.receipts_dir / "rcmake-0.1.0-x86_64-pc-windows-msvc.zip"
        with zipfile.ZipFile(zip_archive, "w", compression=zipfile.ZIP_DEFLATED) as zf:
            zf.writestr("rcmake-0.1.0-x86_64-pc-windows-msvc/bin/rcmake.exe", BINARY_BYTES)
        zip_sha = hashlib.sha256(zip_archive.read_bytes()).hexdigest()
        zip_sidecar = self.receipts_dir / f"{zip_archive.name}.sha256"
        zip_sidecar.write_text(f"{zip_sha}  {zip_archive.name}\n", encoding="ascii")
        payloads = self.build_valid_receipts()
        payloads["dist"].update(
            tarball=zip_archive.name,
            tarball_path=zip_archive.name,
            tarball_sha256=zip_sha,
            sha256_path=zip_sidecar.name,
        )
        manifest = self.write_manifest(payloads)
        code, _, stderr = self.run_cli("--manifest", str(manifest))
        self.assertEqual(code, 0, stderr)

    def test_rejects_missing_or_empty_dist_archive_and_checksum_paths(self):
        # Removing both paths (even when files are absent) must fail instead of returning findings: [].
        payloads = self.build_valid_receipts()
        payloads["dist"].pop("tarball_path")
        payloads["dist"].pop("sha256_path")
        manifest = self.write_manifest(payloads)
        json_out = self.root / "missing-dist-paths.json"
        code, _, stderr = self.run_cli("--manifest", str(manifest), "--json-out", str(json_out))
        self.assertEqual(code, 1)
        report = json.loads(json_out.read_text(encoding="utf-8"))
        dist_findings = [row for row in report["findings"] if row["receipt"] == "dist"]
        self.assertEqual(len(dist_findings), 2, stderr)
        self.assertTrue(all(row["kind"] == "missing" for row in dist_findings))

        # Blank paths are also rejected.
        payloads["dist"]["tarball_path"] = "   "
        payloads["dist"]["sha256_path"] = ""
        manifest = self.write_manifest(payloads)
        code, _, stderr = self.run_cli("--manifest", str(manifest))
        self.assertEqual(code, 1)
        self.assertIn("dist (missing)", stderr)

        # archive_path and checksum_path aliases are accepted when valid.
        payloads = self.build_valid_receipts()
        payloads["dist"]["archive_path"] = payloads["dist"].pop("tarball_path")
        payloads["dist"]["checksum_path"] = payloads["dist"].pop("sha256_path")
        manifest = self.write_manifest(payloads)
        code, _, stderr = self.run_cli("--manifest", str(manifest))
        self.assertEqual(code, 0, stderr)

    def test_pages_workflow_checks_every_js_file_individually_with_node_check(self):
        pages_yml = (verifier.ROOT / ".github" / "workflows" / "pages.yml").read_text(encoding="utf-8")
        # Must not pass multiple JS paths or a glob to one node --check call (Node only validates the first argument).
        self.assertNotRegex(pages_yml, r"node[ \t]+--check[ \t]+(?:--[^\s]+[ \t]+)*(?![-<])\S+[ \t]+(?![-<])\S+")
        self.assertNotRegex(pages_yml, r"node[ \t]+--check[ \t]+[^\n]*\*")
        match = re.search(
            r'for\s+(\w+)\s+in\s+([^;]+);\s*do\s+node\s+--check(?:\s+--input-type=module\s*<\s*|\s+)"?\$\1"?(?:\s*;|\s*\n)\s*done',
            pages_yml,
            re.DOTALL,
        )
        self.assertIsNotNone(match, "pages.yml must loop over JS files and invoke node --check per file.")
        assert match is not None
        loop_targets = match.group(2).split()
        expanded: set[Path] = set()
        for pattern in loop_targets:
            expanded.update(verifier.ROOT.glob(pattern))
        all_site_js = set((verifier.ROOT / "site").rglob("*.js"))
        self.assertEqual(expanded, all_site_js, "pages.yml node --check loop must cover every site/**/*.js file.")

        if shutil.which("node"):
            for js_file in sorted(all_site_js):
                res = subprocess.run(
                    ["node", "--check", "--input-type=module"],
                    input=js_file.read_text(encoding="utf-8"),
                    capture_output=True,
                    text=True,
                    timeout=15,
                    check=False,
                )
                self.assertEqual(res.returncode, 0, f"node --check failed on {js_file}: {res.stderr}")

            # Confirm a syntax error in a second ESM file fails the per-file loop (unlike multi-arg node --check).
            good_js = self.root / "a-good.js"
            bad_js = self.root / "b-bad.js"
            good_js.write_text("export const ok = 1;\n", encoding="utf-8")
            bad_js.write_text("export const broken = (\n", encoding="utf-8")
            loop_res = subprocess.run(
                [
                    "bash", "-euo", "pipefail", "-c",
                    'for f in "$@"; do node --check --input-type=module < "$f"; done',
                    "_", str(good_js), str(bad_js),
                ],
                capture_output=True,
                text=True,
                timeout=15,
                check=False,
            )
            self.assertNotEqual(loop_res.returncode, 0, "Per-file node --check loop must fail on a broken second file.")


if __name__ == "__main__":
    unittest.main()

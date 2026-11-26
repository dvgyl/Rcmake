# SPDX-License-Identifier: MIT OR Apache-2.0
"""Unit tests for tools/measure/fetch_ci_receipts.py with a fake gh CLI (zero Cargo builds)."""

from __future__ import annotations

from contextlib import redirect_stderr, redirect_stdout
import io
import json
from pathlib import Path
from tempfile import TemporaryDirectory
from typing import Any
import unittest

from tools.measure import assemble_release_receipts as assembler
from tools.measure import build_dashboard_json as builder
from tools.measure import fetch_ci_receipts as fetcher
from tools.measure import verify_release_receipts as verifier
from tools.measure.test_assemble_release_receipts import COMMIT, DIGEST, STALE, make_runcmake_payload


ROOT = Path(__file__).resolve().parents[2]
CI_RUN = 101
ECO_RUN = 202
UPSTREAM_PIN = "1111111111111111111111111111111111111111"
BOSS_PROJECTS = [
    row["name"]
    for row in json.loads((ROOT / "site" / "data" / "latest.json").read_text(encoding="utf-8"))["tracks"]["bossFight"]["projects"]
]


def dumps(payload: Any) -> str:
    return json.dumps(payload, indent=2) + "\n"


def ci_jobs() -> list[dict[str, Any]]:
    jobs = [
        {"databaseId": 900 + i, "name": f"CI on {os_name}", "status": "completed", "conclusion": "success",
         "url": f"https://example.invalid/job/{900 + i}"}
        for i, os_name in enumerate(("ubuntu-24.04", "macos-15", "windows-2025"))
    ]
    jobs.append({"databaseId": 950, "name": "Corpus smoke on macos-15", "status": "completed",
                 "conclusion": "failure", "url": "https://example.invalid/job/950"})
    return jobs


def ecosystem_artifacts(commit: str = COMMIT) -> dict[str, dict[str, str]]:
    """Return raw Ecosystem artifacts in the shape that ecosystem.yml uploads."""
    windows = make_runcmake_payload(367)
    windows.pop("release_commit")
    windows.pop("binary_sha256")
    windows.update(profile="windows", rcmake_sha=commit)
    artifacts: dict[str, dict[str, str]] = {"runcmake-windows": {"runcmake-windows.json": dumps(windows)}}
    # linux-extended uploads a partial report as "runcmake-linux". Only "runcmake-linux-full" is the full lane.
    artifacts["runcmake-linux"] = {"runcmake-linux.json": dumps({"rcmake_sha": commit, "suites": {}})}
    for name in BOSS_PROJECTS:
        artifacts[f"large-projects-{name}"] = {
            "result.json": dumps({
                "project": name, "commit": UPSTREAM_PIN, "rcmake_sha": commit,
                "l1_status": "pass", "l2_status": "pass", "l3_status": "not_requested",
                "l4_status": "not_requested", "l1": {"cache.json": "equal"}, "l2": "equal",
                "status": "pass", "checks": {},
            }),
            "cache.json.summary.json": dumps({"oracle_rows": 1}),
        }
    for index in range(64):
        artifacts[f"vcpkg-shard-{index}"] = {
            "shard.json": dumps({
                "shard_index": index, "shard_count": 64, "expected_ports": 3, "worst_case_seconds": 780,
                "budget_seconds": 3000, "install_passed": {"oracle": 3, "rcmake": 3},
                "configure_results": {"pass": 3}, "duration_seconds": 12.5,
            }),
            "results.jsonl": "".join(json.dumps({"port": f"p{index}-{n}", "status": "pass"}) + "\n" for n in range(3)),
            "zlib-oracle.log": "log\n",
        }
    return artifacts


class FakeGh:
    """Stand-in for fetcher.GhCli that serves canned runs and artifacts."""

    def __init__(self, ci_head: str = COMMIT, eco_head: str = COMMIT, artifacts: dict | None = None) -> None:
        self.runs = {
            CI_RUN: {"databaseId": CI_RUN, "headSha": ci_head, "status": "completed", "conclusion": "failure",
                     "url": "https://example.invalid/run/101", "workflowName": "CI", "jobs": ci_jobs()},
            ECO_RUN: {"databaseId": ECO_RUN, "headSha": eco_head, "status": "completed", "conclusion": "success",
                      "url": "https://example.invalid/run/202", "workflowName": "Ecosystem parity", "jobs": []},
        }
        self.listed = {
            "ci.yml": [{"databaseId": CI_RUN, "headSha": ci_head, "status": "completed"}],
            "ecosystem.yml": [{"databaseId": ECO_RUN, "headSha": eco_head, "status": "completed"}],
        }
        self.artifacts = ecosystem_artifacts() if artifacts is None else artifacts
        self.list_calls: list[tuple[str, str]] = []
        self.downloads: list[list[str]] = []

    def list_runs(self, workflow: fetcher.Workflow, commit: str) -> list[dict[str, Any]]:
        self.list_calls.append((workflow.file, commit))
        return self.listed[workflow.file]

    def view_run(self, run_id: int) -> dict[str, Any]:
        return self.runs[run_id]

    def artifact_names(self, run_id: int) -> list[str]:
        assert run_id == ECO_RUN
        return list(self.artifacts)

    def download(self, run_id: int, names: list[str], dest: Path) -> None:
        assert run_id == ECO_RUN
        self.downloads.append(list(names))
        for name in names:
            for rel, content in self.artifacts[name].items():
                path = dest / name / rel
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(content, encoding="utf-8")


class FetchCiReceiptsTests(unittest.TestCase):
    def setUp(self) -> None:
        tmp = TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.work = Path(tmp.name)
        self.receipts = self.work / "receipts"

    def fetch(self, gh: FakeGh, *extra: str, status: bool = False) -> tuple[int, str, str]:
        argv = ["--receipts-dir", str(self.receipts), "--commit", COMMIT, "--repo", "owner/repo", *extra]
        if not status:
            argv.append("--no-status")
        out, err = io.StringIO(), io.StringIO()
        with redirect_stdout(out), redirect_stderr(err):
            code = fetcher.main(argv, gh_factory=lambda repo: gh)
        return code, out.getvalue(), err.getvalue()

    def scan(self, binary: str | None = DIGEST) -> dict[str, assembler.LaneInspection]:
        result = assembler.scan_receipts_dir(self.receipts, COMMIT, binary)
        return {lane.spec.key: lane for lane in result.lanes}

    def files(self) -> list[str]:
        if not self.receipts.exists():
            return []
        return sorted(p.relative_to(self.receipts).as_posix() for p in self.receipts.rglob("*"))

    # Discovery and headSha enforcement.

    def test_auto_discovery_fetches_latest_completed_runs_for_commit(self) -> None:
        gh = FakeGh()
        code, out, err = self.fetch(gh, "--binary-sha256", DIGEST)
        self.assertEqual(code, 0, err)
        self.assertEqual(gh.list_calls, [("ci.yml", COMMIT), ("ecosystem.yml", COMMIT)])
        self.assertIn(f"CI: run {CI_RUN}", out)
        self.assertIn(f"Ecosystem: run {ECO_RUN}", out)
        ci = json.loads((self.receipts / "ci-jobs.json").read_text(encoding="utf-8"))
        self.assertEqual(ci["run_id"], CI_RUN)
        self.assertEqual({row["head_sha"] for row in ci["workflow_runs"]}, {COMMIT})

    def test_discovered_run_with_other_head_sha_is_refused(self) -> None:
        for kwargs in ({"ci_head": STALE}, {"eco_head": STALE}):
            gh = FakeGh(**kwargs)
            code, _, err = self.fetch(gh)
            self.assertEqual(code, 2)
            self.assertIn("Refusing", err)
            self.assertIn(STALE, err)
            self.assertEqual(gh.downloads, [])
            self.assertEqual(self.files(), [])

    def test_explicit_run_id_with_other_head_sha_is_refused(self) -> None:
        gh = FakeGh()
        gh.runs[ECO_RUN]["headSha"] = STALE
        code, _, err = self.fetch(gh, "--ci-run-id", str(CI_RUN), "--ecosystem-run-id", str(ECO_RUN))
        self.assertEqual(code, 2)
        self.assertIn(f"Refusing Ecosystem run {ECO_RUN}", err)
        self.assertEqual(gh.list_calls, [])
        self.assertEqual(self.files(), [])

    def test_run_id_from_other_workflow_is_refused(self) -> None:
        code, _, err = self.fetch(FakeGh(), "--ci-run-id", str(ECO_RUN))
        self.assertEqual(code, 2)
        self.assertIn("'Ecosystem parity' run, not 'CI'", err)

    def test_missing_ecosystem_run_still_fetches_ci_jobs(self) -> None:
        gh = FakeGh()
        gh.listed["ecosystem.yml"] = []
        code, out, err = self.fetch(gh)
        self.assertEqual(code, 0, err)
        self.assertIn("no completed ecosystem.yml run", out)
        self.assertEqual(self.files(), ["ci-jobs.json"])

    def test_artifact_receipt_with_other_commit_writes_nothing(self) -> None:
        artifacts = ecosystem_artifacts()
        payload = json.loads(artifacts["runcmake-windows"]["runcmake-windows.json"])
        payload["rcmake_sha"] = STALE
        artifacts["runcmake-windows"]["runcmake-windows.json"] = dumps(payload)
        code, _, err = self.fetch(FakeGh(artifacts=artifacts))
        self.assertEqual(code, 2)
        self.assertIn("runcmake-windows/runcmake-windows.json: records commit", err)
        self.assertEqual(self.files(), [])

    def test_artifact_receipt_with_other_binary_digest_writes_nothing(self) -> None:
        artifacts = ecosystem_artifacts()
        path = "vcpkg-shard-7"
        shard = json.loads(artifacts[path]["shard.json"])
        shard["binary_sha256"] = "c" * 64
        artifacts[path]["shard.json"] = dumps(shard)
        code, _, err = self.fetch(FakeGh(artifacts=artifacts), "--binary-sha256", DIGEST)
        self.assertEqual(code, 2)
        self.assertIn("vcpkg-shard-7/shard.json: records binary SHA-256", err)
        self.assertEqual(self.files(), [])

    # ci-jobs.json schema.

    def test_ci_jobs_receipt_is_accepted_by_builder_and_verifier(self) -> None:
        code, _, err = self.fetch(FakeGh(), "--binary-sha256", DIGEST)
        self.assertEqual(code, 0, err)
        ci_path = self.receipts / "ci-jobs.json"

        out_json = self.work / "latest.json"
        with redirect_stderr(io.StringIO()) as builder_err:
            builder_code = builder.main([
                "--base", str(ROOT / "site" / "data" / "latest.json"), "--out", str(out_json),
                "--strict-same-sha", "--release-sha", COMMIT, "--binary-sha256", DIGEST,
                "--ci-status-json", str(ci_path),
            ])
        self.assertEqual(builder_code, 0, builder_err.getvalue())
        snapshot = json.loads(out_json.read_text(encoding="utf-8"))
        self.assertIn("CI reports 3 of 4 completed runs as successful", snapshot["statusNote"])

        record = verifier.load_receipt_entry("ci_jobs", "extra", {"path": str(ci_path)}, self.receipts)
        findings: list[verifier.Finding] = []
        verifier.check_identity(record, COMMIT, DIGEST, findings)
        self.assertEqual(findings, [])
        self.assertEqual(self.scan()["ci_jobs"].state, "present")

    # Ecosystem artifact mapping.

    def test_ecosystem_artifacts_map_to_assembler_lanes(self) -> None:
        gh = FakeGh()
        code, out, err = self.fetch(gh, "--binary-sha256", DIGEST, status=True)
        self.assertEqual(code, 0, err)

        lanes = self.scan()
        for key in ("ci_jobs", "runcmake_windows", "large_projects", "vcpkg"):
            self.assertEqual(lanes[key].state, "present", lanes[key].detail)
        self.assertEqual(lanes["vcpkg"].member_count, 64)
        self.assertEqual(lanes["large_projects"].member_count, len(BOSS_PROJECTS))
        self.assertEqual(lanes["runcmake_linux"].state, "missing")
        self.assertNotIn("runcmake-linux", gh.downloads[0])
        self.assertFalse(any(name.endswith(".log") for name in self.files()))

        first = json.loads((self.receipts / "large-projects" / BOSS_PROJECTS[0] / "result.json").read_text())
        self.assertEqual(first["project_commit"], UPSTREAM_PIN)
        self.assertNotIn("commit", first)
        self.assertTrue((self.receipts / "vcpkg" / "63" / "results.jsonl").is_file())

        checks = (
            ("runcmake_windows", "runcmake-windows.json", verifier.check_runcmake),
            ("large_projects", "large-projects", verifier.check_large_projects),
            ("vcpkg", "vcpkg", verifier.check_vcpkg),
        )
        for key, rel, check in checks:
            record = verifier.load_receipt_entry(key, "track", {"path": str(self.receipts / rel)}, self.receipts)
            findings: list[verifier.Finding] = []
            verifier.check_identity(record, COMMIT, DIGEST, findings)
            check(record, findings)
            self.assertEqual(findings, [], key)

        self.assertIn("Release receipt readiness", out)
        for key in ("ci_jobs", "runcmake_windows", "large_projects", "vcpkg"):
            self.assertRegex(out, rf"\| `{key}` \| `[^`]+` \| `present` \|")
        self.assertIn("Missing required lanes:", out)

    def test_flat_runcmake_artifact_name_is_found_in_subdirectory(self) -> None:
        artifacts = ecosystem_artifacts()
        content = artifacts["runcmake-windows"].pop("runcmake-windows.json")
        artifacts["runcmake-windows"]["nested/report.json"] = content
        code, _, err = self.fetch(FakeGh(artifacts=artifacts), "--binary-sha256", DIGEST)
        self.assertEqual(code, 0, err)
        self.assertEqual(self.scan()["runcmake_windows"].state, "present")

    # --dry-run and --overwrite.

    def test_dry_run_prints_plan_and_writes_nothing(self) -> None:
        gh = FakeGh()
        code, out, err = self.fetch(gh, "--dry-run")
        self.assertEqual(code, 0, err)
        self.assertEqual(gh.downloads, [])
        self.assertEqual(self.files(), [])
        self.assertIn("would write  ci-jobs.json", out)
        self.assertIn("would write  vcpkg <- Ecosystem run 202: 64 vcpkg-shard-* artifacts", out)
        self.assertIn("would write  runcmake-windows.json", out)

    def test_local_receipts_are_kept_without_overwrite(self) -> None:
        local = {"release_commit": COMMIT, "binary_sha256": DIGEST, "suites": {}, "local": True}
        self.receipts.mkdir()
        (self.receipts / "runcmake-windows.json").write_text(dumps(local), encoding="utf-8")
        (self.receipts / "vcpkg.json").write_text(dumps(local), encoding="utf-8")
        before = (self.receipts / "runcmake-windows.json").read_bytes()

        gh = FakeGh()
        code, out, err = self.fetch(gh, "--binary-sha256", DIGEST)
        self.assertEqual(code, 0, err)
        self.assertEqual((self.receipts / "runcmake-windows.json").read_bytes(), before)
        self.assertTrue((self.receipts / "vcpkg.json").is_file())
        self.assertFalse((self.receipts / "vcpkg").exists())
        self.assertIn("keep   runcmake-windows.json", out)
        self.assertIn("keep   vcpkg.json", out)
        downloaded = gh.downloads[0]
        self.assertNotIn("runcmake-windows", downloaded)
        self.assertFalse(any(name.startswith("vcpkg-shard-") for name in downloaded))

    def test_overwrite_replaces_every_alias_of_the_lane(self) -> None:
        local = {"release_commit": COMMIT, "binary_sha256": DIGEST, "suites": {}, "local": True}
        self.receipts.mkdir()
        (self.receipts / "runcmake_windows.json").write_text(dumps(local), encoding="utf-8")
        (self.receipts / "vcpkg.json").write_text(dumps(local), encoding="utf-8")

        code, out, err = self.fetch(FakeGh(), "--binary-sha256", DIGEST, "--overwrite")
        self.assertEqual(code, 0, err)
        self.assertIn("(replaces vcpkg.json)", out)
        self.assertFalse((self.receipts / "vcpkg.json").exists())
        self.assertFalse((self.receipts / "runcmake_windows.json").exists())
        lanes = self.scan()
        self.assertEqual(lanes["vcpkg"].state, "present")
        self.assertEqual(lanes["runcmake_windows"].state, "present")
        self.assertNotIn("local", json.loads((self.receipts / "runcmake-windows.json").read_text()))
        self.assertFalse(any(name.startswith(".fetch-ci-receipts-") for name in self.files()))


if __name__ == "__main__":
    unittest.main()
